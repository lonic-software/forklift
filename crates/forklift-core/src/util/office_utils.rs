//! The warehouse office: users and keys as tracked metadata.
//!
//! Identity records live on a reserved pallet (the "office") as ordinary blobs under a
//! reserved tree namespace (`.forklift/tracked/…`) — a path normal loads can never
//! produce, because the `.forklift` folder itself is never tracked. Being ordinary
//! objects, the records inherit hashing, signing and (later) transport for free, and the
//! office pallet's parcel history *is* the audit trail of every user and key change.

use std::collections::HashSet;

use chrono::Utc;
use toml_edit::{value, DocumentMut};
use crate::builder::object::loose_object_builder::LooseObjectBuilder;
use crate::enums::dir_entry_type::DirEntryType;
use crate::enums::parcel_action_type::ParcelActionType;
use crate::globals::forklift_root;
use crate::model::blob::Blob;
use crate::model::operator::Operator;
use crate::model::parcel::Parcel;
use crate::model::parcel_action::ParcelAction;
use crate::model::tree_item::TreeItem;
use crate::util::{alias_utils, file_utils, object_utils, pallet_utils, sign_utils};
use crate::util::pallet_utils::{AnchorPrecondition, OfficePrecondition, PalletsPrecondition, RefPreconditions, RefUpdateError};

/// The name of the office meta pallet — the tracked-metadata pallet holding users and
/// keys. It lives in the meta namespace (`.forklift/meta/office`, not `.forklift/pallets/`),
/// so it is reached with the `@` qualifier (`@office`) and no user pallet name is reserved
/// by it (DESIGN.html §3.3). Use the `*_meta_*` pallet functions to read/write its ref.
pub const OFFICE_PALLET_NAME: &str = "office";

/// The office pallet's reference string (the wire and revision form): `@office`.
pub fn office_wire_key() -> String {
    pallet_utils::PalletRef::meta(OFFICE_PALLET_NAME).to_wire()
}

/// The tree namespace components of tracked metadata: `.forklift/tracked/…`.
const TREE_NAME_FORKLIFT: &str = ".forklift";
const TREE_NAME_TRACKED: &str = "tracked";
const TREE_NAME_USERS: &str = "users";
const TREE_NAME_KEYS: &str = "keys";

/// The file suffix of tracked metadata records.
const RECORD_SUFFIX: &str = ".toml";

/// The trust anchor file inside the forklift root. Its presence means signing is
/// required — a one-way door (nothing ever removes it).
pub(crate) const FILE_NAME_TRUST: &str = "trust";

/// What an operator may do. Recorded in the user's tracked office record, so
/// privileges are signed, audited metadata like everything else in the office.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// Manage the office (admissions, roles, others' keys) and write any pallet.
    Admin,

    /// Write working pallets (all of them, or the granted list); manage own keys.
    Writer,

    /// Read only; may still manage own keys (rotation is self-service).
    Reader,
}

impl Role {
    /// The TOML value of the role.
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Writer => "writer",
            Role::Reader => "reader",
        }
    }

    /// Parse a role from its TOML value.
    pub fn parse(value: &str) -> Result<Role, String> {
        match value {
            "admin" => Ok(Role::Admin),
            "writer" => Ok(Role::Writer),
            "reader" => Ok(Role::Reader),
            other => Err(format!(
                "\"{}\" is not a role (expected \"admin\", \"writer\" or \"reader\").",
                other
            )),
        }
    }
}

/// What kind of principal an operator is (§7.1). Orthogonal to [`Role`] (which is
/// *authority*): the class is *provenance* — is this a person, or automation, and if
/// automation, what kind. It rides in the signed office record, so "an agent wrote
/// this, supervised by Alice" is forge-proof and offline-verifiable. The class also
/// carries the expectation about keys: a human identity is meant to hold a
/// passphrase-protected key (a per-action human gate), automated ones sign freely
/// under their own passphraseless key — automation signs *as itself*, never as a human.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IdentityClass {
    /// A person.
    Human,

    /// An AI agent, bound to a supervising human.
    Agent,

    /// A non-human automation (a scripted bot: dependency bumps, changelog, mirrors).
    Bot,

    /// A service/CI identity (a build or release pipeline).
    Service,
}

impl IdentityClass {
    /// The TOML value of the class.
    pub fn as_str(&self) -> &'static str {
        match self {
            IdentityClass::Human => "human",
            IdentityClass::Agent => "agent",
            IdentityClass::Bot => "bot",
            IdentityClass::Service => "service",
        }
    }

    /// Parse a class from its TOML value.
    pub fn parse(value: &str) -> Result<IdentityClass, String> {
        match value {
            "human" => Ok(IdentityClass::Human),
            "agent" => Ok(IdentityClass::Agent),
            "bot" => Ok(IdentityClass::Bot),
            "service" => Ok(IdentityClass::Service),
            other => Err(format!(
                "\"{}\" is not an identity class (expected \"human\", \"agent\", \"bot\" or \"service\").",
                other
            )),
        }
    }

    /// Whether this is an automated (non-human) identity.
    pub fn is_automated(&self) -> bool {
        !matches!(self, IdentityClass::Human)
    }
}

/// Why a key was revoked (§8.11). The mechanics are identical — signatures by the key
/// are vouched only within its distrust boundary — but the reason tells humans and
/// tooling how alarmed to be about what sits inside it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RevocationReason {
    /// A routine goodbye (rotation, decommissioned machine): nothing inside the
    /// boundary is suspect.
    Retirement,

    /// The key may be in someone else's hands: everything it signed deserves a look,
    /// and anything outside the boundary is refused outright.
    Compromise,
}

impl RevocationReason {
    /// The TOML value of the reason.
    pub fn as_str(&self) -> &'static str {
        match self {
            RevocationReason::Retirement => "retirement",
            RevocationReason::Compromise => "compromise",
        }
    }

    /// Parse a reason from its TOML value.
    pub fn parse(value: &str) -> Result<RevocationReason, String> {
        match value {
            "retirement" => Ok(RevocationReason::Retirement),
            "compromise" => Ok(RevocationReason::Compromise),
            other => Err(format!(
                "\"{}\" is not a revocation reason (expected \"retirement\" or \"compromise\").",
                other
            )),
        }
    }
}

/// An enrolled user. Records carry only the opaque operator id — no display data ever
/// goes on-chain (a hosting provider resolves ids to names behind its own policy).
///
/// `PartialEq` is derived (not hand-projected) deliberately: `verify_self_service_change`
/// compares whole records so that a field added here is protected by default, rather
/// than needing a matching update to a separate projection (see FORK-76 — a hand-built
/// projection of five of these seven fields let `class` and `supervisor` drift silently
/// unguarded for a release).
#[derive(Clone, PartialEq, Debug)]
pub struct UserRecord {
    /// The operator id the chain sees: an opaque string (a minted UUID by default).
    pub identifier: String,

    pub enrolled_at: i64,

    /// The user's role.
    pub role: Role,

    /// The pallets a writer may move (empty = every working pallet). Ignored for
    /// admins (who may move any) and readers (who may move none).
    pub pallets: Vec<String>,

    /// The id of the user's identity-root key (§8.5 of the design): the office enrolls
    /// the identity root, and every further key must chain to it through sigchain
    /// endorsements (or be authorized by an admin).
    pub identity_root: String,

    /// What kind of principal this is (§7.1). `Human` by default (and when the field is
    /// absent from an older record); set at admission and never changed by `role`.
    pub class: IdentityClass,

    /// For an automated identity, the operator responsible for it (an agent's
    /// supervising human). Absent for humans and for unsupervised bots/services.
    pub supervisor: Option<String>,
}

impl UserRecord {
    /// Whether the user may move the given working pallet (the office pallet has its
    /// own rules — see `audit_utils::verify_office_chain`).
    pub fn may_write_pallet(&self, pallet: &str) -> bool {
        match self.role {
            Role::Admin => true,
            Role::Writer => self.pallets.is_empty() || self.pallets.iter().any(|p| p == pallet),
            Role::Reader => false,
        }
    }
}

/// A tracked (public) key. Retired keys are retained forever: they still verify the
/// parcels that were signed while they were active.
#[derive(Clone)]
pub struct KeyRecord {
    /// The key id: the Blake3 hex hash of the raw public key bytes. Never stored in the
    /// record: [`parse_key_record`] derives it from `public_key`, and the record's file name
    /// is `<key_id>.toml`.
    pub key_id: String,

    /// The identifier of the operator the key belongs to.
    pub operator: String,

    /// The Ed25519 public key (hex).
    pub public_key: String,

    pub issued_at: i64,

    /// When the key was retired; `None` means the key is active. Display metadata —
    /// validity questions are decided by the distrust boundary, never by time.
    pub retired_at: Option<i64>,

    /// Why the key was revoked. Present exactly when `retired_at` is.
    pub revocation_reason: Option<RevocationReason>,

    /// The revocation's distrust boundary (§8.11): the pallet heads the revoker
    /// vouched for at revocation. Signatures by this key are valid only on parcels
    /// reachable from these heads — exact ancestry, like the trust boundary, immune
    /// to forged or shifted clocks. Empty while the key is active.
    pub distrust_boundary: Vec<String>,

    /// The id of the key that authorized this one (§8.5/8.6 of the design): one of the
    /// operator's own keys (a sigchain endorsement — self only for the identity root),
    /// or an admin's key (an admin-authorized key, scoped to this office).
    pub authorized_by: String,

    /// The authorizer's Ed25519 signature (hex) over `key_endorsement_payload`.
    pub endorsement: String,

    /// The new key's own Ed25519 signature (hex) over `key_pop_payload` — the
    /// proof-of-possession that closes the gap where someone enrolls a key the
    /// operator does not control.
    pub proof_of_possession: String,
}

impl KeyRecord {
    /// Check whether the key is active (not retired).
    pub fn is_active(&self) -> bool {
        self.retired_at.is_none()
    }
}

/// The full state of the office: every user and every key ever tracked.
#[derive(Clone)]
pub struct OfficeState {
    pub users: Vec<UserRecord>,
    pub keys: Vec<KeyRecord>,
}

impl OfficeState {
    /// Find a user by identifier.
    pub fn find_user(&self, identifier: &str) -> Option<&UserRecord> {
        self.users.iter().find(|user| user.identifier == identifier)
    }

    /// Find a key by id.
    pub fn find_key(&self, key_id: &str) -> Option<&KeyRecord> {
        self.keys.iter().find(|key| key.key_id == key_id)
    }

    /// The active keys of an operator.
    pub fn active_keys_of(&self, identifier: &str) -> Vec<&KeyRecord> {
        self.keys.iter()
            .filter(|key| key.operator == identifier && key.is_active())
            .collect()
    }

    /// The key an operator can sign with right now: an active key of theirs whose
    /// private half is present on this machine.
    pub fn signing_key_of(&self, identifier: &str) -> Option<&KeyRecord> {
        self.active_keys_of(identifier)
            .into_iter()
            .find(|key| sign_utils::has_private_key(&key.key_id))
    }

}

/// The message a new key's authorizer signs (§8.6 of the design): the key, who it
/// belongs to, who authorized it and when. Recorded in the key record as `endorsement`.
///
/// # Arguments
/// * `public_key_hex` - The endorsed public key (hex).
/// * `operator`       - The operator id the key belongs to.
/// * `authorized_by`  - The id of the authorizing key.
/// * `issued_at`      - When the key was issued (Unix seconds).
///
/// # Returns
/// * `Vec<u8>` - The canonical payload bytes.
pub fn key_endorsement_payload(public_key_hex: &str,
                               operator: &str,
                               authorized_by: &str,
                               issued_at: i64) -> Vec<u8> {
    format!(
        "forklift key endorsement v1\nkey: {}\noperator: {}\nauthorized_by: {}\nissued_at: {}\n",
        public_key_hex, operator, authorized_by, issued_at
    ).into_bytes()
}

/// The message a new key signs about itself: the proof-of-possession (§8.6). It binds
/// the key to the operator id, so an admission cannot re-attribute a consenting key to
/// someone else. Recorded in the key record as `proof_of_possession`.
///
/// # Arguments
/// * `public_key_hex` - The public key (hex).
/// * `operator`       - The operator id the key claims to belong to.
///
/// # Returns
/// * `Vec<u8>` - The canonical payload bytes.
pub fn key_pop_payload(public_key_hex: &str, operator: &str) -> Vec<u8> {
    format!(
        "forklift key proof-of-possession v1\nkey: {}\noperator: {}\n",
        public_key_hex, operator
    ).into_bytes()
}

/// Sign a proof-of-possession for a locally held key.
///
/// # Arguments
/// * `key_id`         - The id of the key (its private half must be local).
/// * `public_key_hex` - The public key (hex).
/// * `operator`       - The operator id the key belongs to.
///
/// # Returns
/// * `Ok(String)`  - The proof-of-possession signature (hex).
/// * `Err(String)` - If the private key is missing or invalid.
pub fn sign_key_pop(key_id: &str, public_key_hex: &str, operator: &str) -> Result<String, String> {
    sign_utils::sign_message(key_id, &key_pop_payload(public_key_hex, operator))
        .map(|signature| sign_utils::to_hex(&signature))
}

/// Build a fully endorsed key record: verify the proof-of-possession, then sign the
/// endorsement with the (locally held) authorizing key. Every path that adds a key —
/// genesis, admission, rotation, device linking — goes through this.
///
/// # Arguments
/// * `public_key_hex` - The new public key (hex, lowercase).
/// * `operator`       - The operator id the key belongs to.
/// * `authorized_by`  - The id of the authorizing key (its private half must be local).
/// * `pop_hex`        - The new key's proof-of-possession signature (hex).
/// * `issued_at`      - When the key is issued (Unix seconds).
///
/// # Returns
/// * `Ok(KeyRecord)`  - The endorsed record (active).
/// * `Err(String)`    - If the proof-of-possession does not verify, or signing failed.
pub fn endorse_key(public_key_hex: &str,
                   operator: &str,
                   authorized_by: &str,
                   pop_hex: &str,
                   issued_at: i64) -> Result<KeyRecord, String> {
    let pop_signature = sign_utils::from_hex(pop_hex)
        .map_err(|_| "The proof-of-possession is not valid hex.".to_string())?;

    let pop_valid = sign_utils::verify_message(
        public_key_hex,
        &key_pop_payload(public_key_hex, operator),
        &pop_signature
    )?;

    if !pop_valid {
        return Err(format!(
            "The proof-of-possession does not verify: the key holder did not sign for \
            operator \"{}\" with this key. Make sure the operator id and the public key \
            come from the same \"forklift office keygen\" run.",
            operator
        ));
    }

    let endorsement = sign_utils::sign_message(
        authorized_by,
        &key_endorsement_payload(public_key_hex, operator, authorized_by, issued_at)
    )?;

    Ok(KeyRecord {
        key_id: key_id_of(public_key_hex)?,
        operator: operator.to_string(),
        public_key: public_key_hex.to_string(),
        issued_at,
        retired_at: None,
        revocation_reason: None,
        distrust_boundary: Vec::new(),
        authorized_by: authorized_by.to_string(),
        endorsement: sign_utils::to_hex(&endorsement),
        proof_of_possession: pop_hex.to_string(),
    })
}

/// The trust anchor: the genesis office parcel that established signing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TrustAnchor {
    /// The hash of the genesis office parcel.
    pub genesis: String,

    /// When signing was established (Unix seconds).
    pub enabled_at: i64,

    /// The heads of every pallet at the moment trust was established. Parcels reachable
    /// from these are the pre-trust (legacy) history and may be unsigned; everything
    /// else must carry a signature. An exact, ancestry-based boundary — timestamps have
    /// second granularity and can be forged, so they never decide.
    pub boundary: Vec<String>,

    /// The genesis of the chain this anchor replaced (§8.7 re-genesis). `None` for an
    /// original enrollment. Old-anchor holders use it to verify the chain of custody
    /// before consciously re-accepting the new anchor.
    pub prior_genesis: Option<String>,

    /// The office head of the replaced chain at the moment of re-genesis: the pin that
    /// freezes the prior history as *attested* — not gone, but the guarantee over it
    /// degrades from verified-by-chain to attested-by-this-anchor.
    pub adopts: Option<String>,
}

/// Ensure a tree is a normal content tree: meta pallet trees (the office and the meta-tree pallets) carry a
/// top-level `.forklift` entry and must never be materialized into a working directory, where
/// that entry would land in the warehouse's own metadata folder.
///
/// # Arguments
/// * `root_tree_hash` - The hash of the tree to check.
///
/// # Returns
/// * `Ok(())`      - If the tree carries no tracked-metadata namespace.
/// * `Err(String)` - If it does (or could not be read).
pub fn ensure_not_metadata_tree(root_tree_hash: &str) -> Result<(), String> {
    ensure_not_metadata_root(&object_utils::load_tree(root_tree_hash)?)
}

/// Whether a working-directory name, at any depth, opens a warehouse's metadata folder on some file
/// system: at the top level the warehouse's own, below it another working tree's. The match is wider than the exact name, after git's `verify_path` (`is_ntfs_dotgit`,
/// `is_hfs_dotgit`):
/// * a file system that ignores case (macOS, Windows) opens `.forklift` for `.ForkLift`. Case is
///   folded by Unicode rules, as those file systems fold it, so `.for\u{212A}lift` (KELVIN SIGN)
///   matches;
/// * Windows drops trailing dots and spaces (`.forklift.`), and reads what follows a `:` as a data
///   stream of the folder itself (`.forklift::$INDEX_ALLOCATION`);
/// * HFS+ skips the code points [`is_hfs_ignorable`] lists, wherever they sit (`.fork\u{200C}lift`);
/// * NTFS gives the folder an 8.3 short name, `FORKLI~1` (`~2` to `~4` when siblings share the
///   prefix). Past `~4` it hashes the long name instead, into [`HASHED_SHORT_NAME_PREFIX`] and an
///   ordinal (`FOCBB9~1`), or a shorter cut of that prefix, matched as git's `is_ntfs_dot_generic`
///   matches `gi7eba~1` for `.gitmodules`.
///
/// The hash is undocumented, so [`opens_metadata_folder`] also asks the volume about a path in the
/// working directory itself.
pub fn is_metadata_folder_name(name: &str) -> bool {
    let name = name.split(':').next().unwrap_or(name);
    let name: String = name.chars().filter(|c| !is_hfs_ignorable(*c)).collect();
    let name = name.trim_end_matches(['.', ' ']).to_lowercase();

    name == TREE_NAME_FORKLIFT
        || name.strip_prefix("forkli~").is_some_and(|ordinal| matches!(ordinal, "1" | "2" | "3" | "4"))
        || is_hashed_short_name(&name)
}

/// The six characters NTFS hashes `.forklift` to when it cannot give it `FORKLI~1` to `~4`, as a
/// Windows volume reported it (`FOCBB9~1`), lowercased.
const HASHED_SHORT_NAME_PREFIX: &str = "focbb9";

/// Whether the lowercased `name` is a hashed 8.3 short name of `.forklift`: a cut of
/// [`HASHED_SHORT_NAME_PREFIX`], a `~`, then an ordinal not starting with `0`, eight characters in
/// all (the ordinal grows into the prefix: `focbb~10`).
fn is_hashed_short_name(name: &str) -> bool {
    let Some((prefix, ordinal)) = name.split_once('~') else {
        return false;
    };

    name.len() == 8
        && !prefix.is_empty()
        && HASHED_SHORT_NAME_PREFIX.starts_with(prefix)
        && ordinal.starts_with(|c: char| ('1'..='9').contains(&c))
        && ordinal.chars().all(|c| c.is_ascii_digit())
}

/// The code points HFS+ ignores in a file name, so `.fork\u{200C}lift` opens `.forklift` there: the
/// list git's `is_hfs_dotgit` skips (CVE-2014-9390).
fn is_hfs_ignorable(c: char) -> bool {
    matches!(c, '\u{200C}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{206A}'..='\u{206F}' | '\u{FEFF}')
}

/// Whether the working-directory entry `name` in the directory `parent` opens a metadata folder
/// there: by [`is_metadata_folder_name`], or, for a name holding a `~` (the mark of every 8.3 short
/// name, hashed ones included), because it resolves to the same entry as `parent/.forklift` on this
/// volume. A `.forklift` in the root is the warehouse's own metadata folder; one in any directory
/// below it is another working tree's, which warehouse discovery from inside that directory stops at.
///
/// `parent` is relative to the current directory, which is the working directory's root (empty for
/// the root itself): every working-directory read and write resolves there, so the volume is asked
/// about the very entry the caller touches. Every check of a name that ends in the working directory
/// (load, stack, every materializer) asks this one, so they agree on which names are the metadata
/// folder.
///
/// # Arguments
/// * `parent` - The directory holding the entry, relative to the working directory's root.
/// * `name`   - The entry's name.
///
/// # Returns
/// * `Ok(bool)`    - Whether `name` opens a metadata folder.
/// * `Err(String)` - If the volume could not say: an entry that cannot be identified is never
///                   taken for a different one.
pub fn opens_metadata_folder(parent: &std::path::Path, name: &str) -> Result<bool, String> {
    use crate::globals::FOLDER_NAME_FORKLIFT_ROOT;

    if is_metadata_folder_name(name) {
        return Ok(true);
    }
    if !name.contains('~') {
        return Ok(false);
    }

    alias_utils::same_entry(&parent.join(name), &parent.join(FOLDER_NAME_FORKLIFT_ROOT))
}

/// The first component of the `/`-separated warehouse path `path` that opens a metadata folder in
/// the directory above it (see [`opens_metadata_folder`]), at any depth: git's `verify_path` rule,
/// which refuses `.git` in every component, not only the first.
///
/// # Arguments
/// * `path` - The warehouse path, relative to the working directory's root.
///
/// # Returns
/// * `Ok(Some((depth, component)))` - The first component that names a metadata folder, and its
///                                    depth: 0 for the top level, where it is this warehouse's own.
/// * `Ok(None)`                     - If none does.
/// * `Err(String)`                  - If the volume could not say for some component.
pub fn metadata_folder_component(path: &str) -> Result<Option<(usize, &str)>, String> {
    let mut parent = std::path::PathBuf::new();

    for (depth, component) in path.split('/').enumerate() {
        if opens_metadata_folder(&parent, component)? {
            return Ok(Some((depth, component)));
        }

        parent.push(component);
    }

    Ok(None)
}

/// [`ensure_not_metadata_tree`] over a root tree already loaded, matching names with
/// [`opens_metadata_folder`], so a short name the volume resolves to the metadata folder is refused
/// by name here rather than left to whatever later check trips over the folder it opens.
///
/// # Arguments
/// * `root` - The root tree to check.
///
/// # Returns
/// * `Ok(())`      - If no top-level entry names the metadata folder.
/// * `Err(String)` - The refusal, naming the entry Debug-quoted.
pub fn ensure_not_metadata_root(root: &TreeItem) -> Result<(), String> {
    let found = root.get_subtrees().chain(root.get_files())
        .map(|(name, _)| name)
        .find_map(|name| match opens_metadata_folder(std::path::Path::new(""), name) {
            Ok(true) => Some(Ok(name)),
            Ok(false) => None,
            Err(error) => Some(Err(error)),
        })
        .transpose()?;

    match found {
        Some(name) => Err(format!(
            "This parcel's tree has a top-level entry {:?}, which is the warehouse's metadata \
            folder (a meta pallet's tree, or a crafted one); it cannot be materialized into a \
            working directory.",
            name
        )),
        None => Ok(()),
    }
}

/// Read the trust anchor, if signing has been established.
///
/// # Returns
/// * `Ok(Some(TrustAnchor))` - The anchor.
/// * `Ok(None)`              - If signing has not been established.
/// * `Err(String)`           - If the trust file exists but is invalid.
pub fn read_trust_anchor() -> Result<Option<TrustAnchor>, String> {
    Ok(read_trust_anchor_with_bytes()?.map(|(anchor, _)| anchor))
}

/// [`read_trust_anchor`], with the trust file's bytes the anchor was decoded from: one read, so
/// a writer conditioned on the bytes ([`AnchorPrecondition::Bytes`]) is conditioned on exactly
/// the anchor it judged.
///
/// # Returns
/// * `Ok(Some((TrustAnchor, Vec<u8>)))` - The anchor, and the file's bytes.
/// * `Ok(None)`                         - If signing has not been established.
/// * `Err(String)`                      - If the trust file exists but is invalid.
pub fn read_trust_anchor_with_bytes() -> Result<Option<(TrustAnchor, Vec<u8>)>, String> {
    let Some(bytes) = read_trust_anchor_bytes()? else {
        return Ok(None);
    };

    let content = std::str::from_utf8(&bytes)
        .map_err(|e| format!("The trust file is not valid UTF-8: {}", e))?;

    Ok(Some((parse_trust_anchor(content)?, bytes)))
}

/// The trust file's bytes, undecoded (`None`: there is no trust file).
pub(crate) fn read_trust_anchor_bytes() -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(forklift_root().join(FILE_NAME_TRUST)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("Error while reading the trust file: {}", e)),
    }
}

fn parse_trust_anchor(content: &str) -> Result<TrustAnchor, String> {
    let doc: DocumentMut = content.parse()
        .map_err(|e| format!("The trust file is not valid TOML: {}", e))?;

    let genesis = read_string(&doc, "genesis", "trust file")?;
    let enabled_at = read_integer(&doc, "enabled_at", "trust file")?;

    // Required and strict: `encode_trust_anchor` always writes this key, even when the
    // boundary is empty (an empty array, never an omitted key) — so a trust file missing it, or
    // carrying a non-string entry, is corrupt, never merely "an old/lenient shape" to shrug off.
    // Silently dropping a non-string entry (the previous `filter_map` behavior) would under-count
    // roots and let `gc` collect a pinned head — exactly what this helper's callers rely on this
    // parse never doing (CLAUDE.md: fail loudly on anything no longer understood, never fall back
    // to a permissive default).
    let boundary = read_required_string_array(&doc, "boundary", "trust file")?;

    // Optional, unlike `boundary`: `encode_trust_anchor` writes these two only for a
    // re-genesis anchor, so an absent key is the normal (original-enrollment) shape and must
    // parse as `None`. A *present* value that is not a string, though, must still fail loudly
    // rather than be silently treated the same as absent — `.and_then(as_str)` would conflate
    // "no re-genesis" with "a corrupt re-genesis pin" and hand `collect_trust_pin_roots` a
    // silently missing `adopts` root (FORK-81 follow-up; same lenience `pallets` had).
    let prior_genesis = read_optional_string(&doc, "prior_genesis", "trust file")?;
    let adopts = read_optional_string(&doc, "adopts", "trust file")?;

    Ok(TrustAnchor {
        genesis,
        enabled_at,
        boundary,
        prior_genesis,
        adopts,
    })
}

/// How [`collect_trust_pin_roots`] treats an unreadable office record (the parcel history that
/// carries every revoked key's own `distrust_boundary` pin) — mirrors
/// [`bay_utils::BayReadPolicy`](crate::util::bay_utils::BayReadPolicy)'s `FailClosed`/`Tolerate`
/// split, and for the same underlying reason: the office chain can be unreadable *exactly* when
/// it is mid-crash — `office admit`/`office rotate`/`office revoke` advance the office pallet ref
/// before its parcel object is durable, the textbook case `forklift heal` exists to recover from
/// (DESIGN.html §3.1.1) — so a caller that always failed closed on that would brick the very
/// command a standing taint tells the operator to run.
///
/// The trust *file* itself (`read_trust_anchor`, and therefore the `adopts`/`boundary` pins) is
/// never covered by this policy: it is a plain local file, not an object-store parcel subject to
/// the ref-before-parcel crash window, so an unreadable trust file stays an unconditional `Err`
/// under both variants.
pub enum TrustPinReadPolicy {
    /// Abort on an unreadable office record. Required wherever the result feeds a destructive
    /// sweep ([`gc_utils::collect_live_set`](crate::util::gc_utils::collect_live_set)) — an
    /// unreadable source cannot be proven not to pin some object, so skipping it to keep a sweep
    /// going would silently under-count roots.
    FailClosed,
    /// Skip an unreadable office record — contributing only the trust anchor's own `adopts` and
    /// `boundary` pins, never a revoked key's `distrust_boundary` (that requires reading the
    /// office record itself) — and report the omission via the returned outcome's `degraded`,
    /// instead of aborting. Sound only for a caller — `forklift heal`
    /// ([`recovery_utils::collect_walk_roots`](crate::util::recovery_utils::collect_walk_roots))
    /// — that treats a `Some` `degraded` the same way it already treats a degraded bay: as "this
    /// run's negative answers are not proof," never as license to clear or delete anything.
    Tolerate,
}

/// Whether a trust-pin root's own absence, if genuine, is real local loss (report it, exactly
/// like an ordinary ref this warehouse's own commands set) or a legitimate gap (a snapshot of a
/// remote's or a revoker's *declared* heads, never pulled first — see each variant's own doc
/// comment for which pin gets which and why). Carried per pin entry in
/// [`TrustPinRootsOutcome::roots`] rather than one policy for the whole call, because the three
/// pin sources do not share this property: `adopts` names history every warehouse holding the
/// anchor holds (see [`PinAbsencePolicy::Loss`]), so folding it into the gap treatment
/// `boundary`/`distrust_boundary` get would silently tolerate real loss.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PinAbsencePolicy {
    /// The pin names history every warehouse holding the anchor holds, by construction: the
    /// re-genesis `adopts` pin. `office regenesis` (office.rs) sets it to this warehouse's own
    /// prior office head; every other way an anchor with `adopts` arrives fetches that history
    /// before writing the anchor (`remote_utils::fetch_adopted_history`, on clone and on
    /// `office accept-regenesis`), and a remote refuses a first anchor whose adopted history it
    /// does not hold. Its absence is therefore genuine local loss, reported exactly like any
    /// ordinary ref.
    Loss,
    /// This pin names a head *declared* by a remote or a revoker at the moment of enrollment or
    /// revocation, never pulled first (`office enroll`/`office revoke`, DESIGN.html §8.7/§8.11):
    /// the trust anchor's `boundary` snapshot and every key's own `distrust_boundary`. Its absence
    /// can be entirely legitimate, so it is never reported as a dangling reference.
    Gap,
}

/// [`collect_trust_pin_roots`]'s result.
pub struct TrustPinRootsOutcome {
    /// Every trust/distrust pin hash this call could establish, paired with its
    /// [`PinAbsencePolicy`], in no particular order and with no deduplication (callers push these
    /// onto a larger root bag alongside other sources, which already tolerates duplicates). Empty
    /// if no trust anchor exists.
    pub roots: Vec<(String, PinAbsencePolicy)>,
    /// A plain-language note if the office record could not be read this run — only ever
    /// populated under [`TrustPinReadPolicy::Tolerate`] (`FailClosed` returns `Err` instead of
    /// populating this). `None` on the common path.
    pub degraded: Option<String>,
}

/// Every durable hash pin the trust anchor and the key registry hold: the re-genesis
/// `adopts` pin, the enrollment `boundary` snapshot, and every key's revocation
/// `distrust_boundary` (§8.11). All three are pallet-head hashes a signed record commits
/// to by value, with no other pointer keeping them alive — a pallet ref that moves away
/// (or is deleted) leaves nothing else naming them, so a walk that only follows refs
/// would collect the very history these pins attest.
///
/// Shared by [`gc_utils::collect_live_set`](crate::util::gc_utils::collect_live_set) and
/// [`recovery_utils::collect_walk_roots`](crate::util::recovery_utils::collect_walk_roots)
/// so the two root lists can never drift apart on this portion — the same reason
/// [`bay_utils::collect_bay_scoped_parcel_roots`](crate::util::bay_utils::collect_bay_scoped_parcel_roots)
/// is shared between them for the bay-scoped portion; see
/// [`recovery_utils::collect_walk_roots`](crate::util::recovery_utils::collect_walk_roots)'s own
/// doc comment for the superset invariant this helper's callers must both keep true — that is the
/// single place the full invariant is actually stated; `bay_utils`'s own doc comment covers only
/// the bay-scoped portion.
///
/// `policy` decides what an unreadable **office record** does — see [`TrustPinReadPolicy`]. The
/// trust file itself (`adopts`/`boundary`) is unconditionally fail-closed regardless of `policy` —
/// see that enum's own doc comment for why. **No anchor means no roots**: signing has not been
/// established, so there is no boundary snapshot and (since a key can only be revoked once the
/// office pallet exists) no revocation to pin either.
///
/// Reads only the office record's **keys**, never its users (F5, PR #120 round 2): only a key's
/// own `distrust_boundary` is a pin this call needs, so it walks the tracked-keys tree directly
/// ([`read_trust_pin_keys`]) instead of [`read_office_state`]'s full user-and-key parse — which
/// would otherwise cost every gc/heal/compact run U extra object loads for user records this call
/// never reads, and (under [`TrustPinReadPolicy::FailClosed`]) can abort a sweep on a malformed
/// user record that pins nothing at all.
///
/// # Returns
/// * `Ok(TrustPinRootsOutcome)` - The pin hashes this call could establish, plus (under
///                                `Tolerate`) a note if the office record itself was unreadable.
/// * `Err(String)`              - If the trust file could not be read, or (under `FailClosed`)
///                                the office record could not be read.
pub fn collect_trust_pin_roots(policy: TrustPinReadPolicy) -> Result<TrustPinRootsOutcome, String> {
    let Some(anchor) = read_trust_anchor()? else {
        return Ok(TrustPinRootsOutcome { roots: Vec::new(), degraded: None });
    };

    let mut roots: Vec<(String, PinAbsencePolicy)> = Vec::new();

    if let Some(adopts) = anchor.adopts {
        roots.push((adopts, PinAbsencePolicy::Loss));
    }

    roots.extend(anchor.boundary.into_iter().map(|hash| (hash, PinAbsencePolicy::Gap)));

    match read_trust_pin_keys() {
        Ok(keys) => {
            for key in &keys {
                roots.extend(
                    key.distrust_boundary.iter().cloned().map(|hash| (hash, PinAbsencePolicy::Gap))
                );
            }

            Ok(TrustPinRootsOutcome { roots, degraded: None })
        }
        Err(office_error) => match policy {
            TrustPinReadPolicy::FailClosed => Err(office_error.message),
            TrustPinReadPolicy::Tolerate => Ok(TrustPinRootsOutcome {
                roots,
                degraded: Some(degraded_office_note(office_error.kind, &office_error.message)),
            }),
        },
    }
}

/// Why an office-record read failed — see [`read_trust_pin_keys`] and
/// [`degraded_office_note`] (F4, PR #120 round 2). Replaces one opaque error string: the previous
/// single "restage" remedy never actually fit either real cause (a never-durable object cannot be
/// restaged — there is nothing present to rewrite — and a strictly-rejected record is rewritten
/// byte for byte, so restaging changes nothing about it either), so the note now says something
/// that can actually be acted on, chosen by which of these a run actually hit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OfficeReadErrorKind {
    /// An object the read needs — the office pallet's own head parcel, a tracked-tree node, or a
    /// key record blob — is not present in this store at all. The textbook shape: `office
    /// admit`/`office rotate`/`office revoke` advance the office ref before its parcel object is
    /// durable, and a crash in that window leaves exactly this (`forklift heal`'s own reason to
    /// exist, DESIGN.html §3.1.1). Remedy: roll `@office` back to a durable head this warehouse
    /// actually holds, or fetch the missing object from a configured remote.
    AbsentObject,
    /// Every object along the path loaded and verified, but a key record's own content does not
    /// parse as its expected shape (bad TOML, a missing/invalid required field, a non-string
    /// `distrust_boundary` entry — see `parse_key_record`). The bytes on disk are exactly what was
    /// durably written; restaging cannot change them. Remedy: fix (or hand-restore a good copy of)
    /// the record itself.
    UnparseableRecord,
    /// Every object loaded, but the office tree holds a file where Forklift only ever writes a
    /// folder (`.forklift`, `tracked` or `keys`). No record is at fault, so there is no record to
    /// fix, and heal cannot change the tree. Remedy: roll `@office` back to a good head.
    MalformedOffice,
    /// Any other read failure (a present-but-corrupt object, an I/O error) that does not fit
    /// any of the three more specific — and more actionable — kinds above.
    Other,
}

/// [`OfficeReadErrorKind`] paired with the underlying plain-language error.
pub struct OfficeReadError {
    pub kind: OfficeReadErrorKind,
    pub message: String,
}

impl OfficeReadError {
    fn other(message: String) -> Self {
        OfficeReadError { kind: OfficeReadErrorKind::Other, message }
    }
}

/// Load an object by hash, classifying an absence explicitly — via a single classified read
/// ([`file_utils::read_object_classified`]) rather than trying to infer "absent" from whatever a
/// deeper loader's error text happens to say. The one distinction [`read_trust_pin_keys`] actually
/// needs in order to word [`degraded_office_note`]'s remedy correctly (F4, PR #120 round 2). A
/// present object that still fails to parse (wrong object type, corrupt content) is
/// [`OfficeReadErrorKind::Other`] — it is not the "restage the missing dentry" case
/// [`OfficeReadErrorKind::AbsentObject`] names, but it is also not a record *parse* failure, since
/// nothing here has reached record content yet.
///
/// **Gate-free, deliberately (F1, PR #120 round 3).** This is reached from
/// [`recovery_utils::collect_walk_roots`](crate::util::recovery_utils::collect_walk_roots) — the
/// durability-taint recovery walk, which necessarily runs while a taint gate may be standing under
/// this very root (that is the walk's whole reason to exist; see that module's own doc comment).
/// [`file_utils::does_object_exist`] would refuse on its very first loose-object call under a
/// standing gate, degrading a perfectly intact office record for no reason — exactly the class of
/// bug [`file_utils::raw_object_present`]'s own doc comment already names as its "one sanctioned
/// exception." [`file_utils::read_object_classified`] is the same core the ordinary read path is
/// itself expressed through and never consults the gate at all, so using it here both fixes that
/// and — as a byproduct — folds the old two-step "check, then separately load" window into one
/// classified read: `Verified` hands back bytes to parse directly, rather than this function's
/// caller re-reading the same hash a second time through `loader`.
fn load_object_classified<T>(
    hash: &str,
    parse: impl FnOnce(&str, &[u8]) -> Result<T, String>,
) -> Result<T, OfficeReadError> {
    match file_utils::read_object_classified(hash) {
        Ok(file_utils::StoreReadOutcome::Verified(bytes)) => parse(hash, &bytes).map_err(OfficeReadError::other),
        Ok(file_utils::StoreReadOutcome::Absent) => Err(OfficeReadError {
            kind: OfficeReadErrorKind::AbsentObject,
            message: format!("object {} is not present", hash),
        }),
        Ok(file_utils::StoreReadOutcome::Unverifiable(reason)) => Err(OfficeReadError::other(reason)),
        Err(e) => Err(OfficeReadError::other(e)),
    }
}

/// [`resolve_subtree`], classified the same way [`load_object_classified`] classifies a single
/// object load — each tree along `path` is read through the one classified read, so an absent one
/// is [`OfficeReadErrorKind::AbsentObject`] by construction (from
/// [`file_utils::StoreReadOutcome::Absent`]), never guessed from a deeper loader's error text.
/// There is deliberately no separate presence check before the load: that check-then-load shape
/// used the taint-gated predicate and had a window between the two, both of which round 3 removed.
fn resolve_subtree_classified(office_parcel_hash: &str, root_tree_hash: &str, path: &[&str])
    -> Result<Option<TreeItem>, OfficeReadError> {
    let mut current = load_object_classified(root_tree_hash, object_utils::parse_local_tree_bytes)?;

    for (depth, component) in path.iter().enumerate() {
        if current.get_files().any(|(name, _)| name == component) {
            return Err(OfficeReadError {
                kind: OfficeReadErrorKind::MalformedOffice,
                message: file_in_place_of_folder(office_parcel_hash, &path[..=depth]),
            });
        }

        let subtree_hash = current.get_subtrees()
            .find(|(name, _)| name == component)
            .map(|(_, item)| item.hash.clone());

        match subtree_hash {
            Some(hash) => current = load_object_classified(&hash, object_utils::parse_local_tree_bytes)?,
            None => return Ok(None),
        }
    }

    Ok(Some(current))
}

/// The refusal for an office tree that holds a file where Forklift only ever writes a folder:
/// read as an absent folder instead, it would drop every record below it.
fn file_in_place_of_folder(office_parcel_hash: &str, path: &[&str]) -> String {
    let path = path.join("/");
    format!(
        "The office parcel {} holds a file {}, but {} is a folder. Forklift never writes a \
        file there, so this office may have been tampered with.",
        office_parcel_hash, path, path
    )
}

/// The trust-pin-relevant slice of the office record: every key's own `distrust_boundary` — the
/// only thing [`collect_trust_pin_roots`] consumes from the office chain, never a user record (F5,
/// PR #120 round 2 — see [`collect_trust_pin_roots`]'s own doc comment for why that matters).
/// Walks `.forklift/tracked/keys` and every folder under it directly, rather than
/// [`read_office_state`]'s full users-and-keys parse: an absent folder on the way holds no pins,
/// while a file in a folder's place is refused ([`resolve_subtree_classified`]). Classified per [`OfficeReadErrorKind`] so
/// [`collect_trust_pin_roots`]'s `Tolerate` branch can word its remedy correctly (F4, same round).
fn read_trust_pin_keys() -> Result<Vec<KeyRecord>, OfficeReadError> {
    let head = pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME).map_err(OfficeReadError::other)?;
    let Some(head) = head else { return Ok(Vec::new()); };

    let parcel = load_object_classified(&head, object_utils::parse_local_parcel_bytes)?;

    let Some(keys_tree) = resolve_subtree_classified(
        &head, &parcel.tree_hash, &[TREE_NAME_FORKLIFT, TREE_NAME_TRACKED, TREE_NAME_KEYS],
    )? else {
        return Ok(Vec::new());
    };

    let mut keys = Vec::new();

    // File names and folder shape are deliberately not checked here, unlike in
    // `read_office_state_of`: this reader only gathers pins that keep history alive, so a
    // misfiled or nested record's pins are kept (the union of every record's) rather than the
    // office refused — judging the office's integrity is the verifying readers' job.
    let mut pending = vec![keys_tree];
    while let Some(tree) = pending.pop() {
        for (_, item) in tree.get_subtrees() {
            pending.push(load_object_classified(&item.hash, object_utils::parse_local_tree_bytes)?);
        }
        for (_, file) in tree.get_files() {
            let record = load_object_classified(&file.hash, parse_record_bytes)?;
            keys.push(parse_key_record(&record).map_err(|e| OfficeReadError {
                kind: OfficeReadErrorKind::UnparseableRecord,
                message: e,
            })?);
        }
    }

    keys.sort_by(|a, b| a.issued_at.cmp(&b.issued_at));

    Ok(keys)
}

/// Plain-language note for an office record [`collect_trust_pin_roots`] could not read under
/// [`TrustPinReadPolicy::Tolerate`] — names the mechanical error and, per `kind`, the remedy that
/// actually applies to it (F4, PR #120 round 2). The previous version's "restage" wording never
/// matched either real cause — a never-durable object cannot be restaged (there is nothing present
/// to rewrite) and a strictly-rejected record is rewritten byte for byte — and its "treated as
/// missing rather than blocking this command" clause was actively misleading: every consumer of a
/// non-empty `degraded_sources` refuses (see `recovery_utils::collect_walk_roots`'s own doc
/// comment), so this note only ever appears *inside* a refusal, never instead of one. Mirrors
/// `bay_utils::degraded_bay_note`'s shape.
fn degraded_office_note(kind: OfficeReadErrorKind, error: &str) -> String {
    let remedy = match kind {
        OfficeReadErrorKind::AbsentObject =>
            "roll \"@office\" back to a durable head this warehouse actually holds, or fetch the \
             missing object from a configured remote, then re-run \"forklift heal\"",
        OfficeReadErrorKind::UnparseableRecord =>
            "fix (or hand-restore a good copy of) the record itself — its bytes are exactly what \
             was durably written, so re-running heal alone will not change them",
        OfficeReadErrorKind::MalformedOffice =>
            "roll \"@office\" back to a head whose office tree is well formed, then re-run \
             \"forklift heal\"",
        OfficeReadErrorKind::Other =>
            "resolve that error, then re-run \"forklift heal\"",
    };

    format!(
        "the office record could not be read this run ({}); every revoked key's own distrust \
        boundary pin could not be established (the trust anchor's own \"adopts\"/\"boundary\" \
        pins are unaffected). {}.",
        error, remedy
    )
}

/// One trust install: a new trust anchor, and the office head it governs, written together
/// (see [`install_trust`]).
pub struct TrustInstall<'a> {
    /// The anchor to install.
    pub anchor: &'a TrustAnchor,

    /// The trust file's bytes as the caller's decision read them (`None`: no anchor was held).
    /// `Some` makes this a re-genesis: `anchor` must name the replaced anchor's genesis as its
    /// prior and pin the office head it adopts.
    pub replaces: Option<&'a [u8]>,

    /// The office head the caller's decision read (`None`: unborn).
    pub office: Option<&'a str>,

    /// The office head the anchor governs (`None`: the office stays at `office`).
    pub new_office: Option<&'a str>,

    /// The boundary the anchor records, as a precondition on the pallet heads
    /// ([`PalletsPrecondition::NotConsumed`] for an anchor adopted from a remote, whose boundary
    /// is the remote's).
    pub pallets: PalletsPrecondition,
}

/// What a successful [`install_trust`] did: a caller announcing a trust change tells its own write
/// from one that was already in place, which another bay or request made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustInstalled {
    /// This call wrote the anchor (and moved the office, if the install named one).
    Written,

    /// The anchor and the office already carried the install; nothing was written.
    AlreadyInPlace,
}

/// Install a trust anchor and move the office to the head it governs, in one hold of the ref
/// lock: the only way an anchor is written. The one-way door: an anchor is replaced only by a
/// re-genesis whose chain of custody matches (`replaces`), never removed.
///
/// Under the hold, the office, the anchor and the boundary are checked against what the caller
/// read, then the anchor is written, then the office moves. So no other bay ever sees the new
/// anchor beside the old office, and a refusal writes nothing. Its parcels are built and signed
/// before, outside the hold. A write can land and still fail, so on any error both files are read
/// back before anything is decided: both new stands but still reports the failure (a write that
/// landed may not be durable), a new anchor beside the old office puts the old anchor back (still
/// inside the hold), and anything else fails naming both.
///
/// An install whose anchor and office are both already in place succeeds without writing: another
/// bay or request installed the same trust (accepted the same re-genesis) first.
///
/// # Returns
/// * `Ok(Written)`         - This call wrote the install; the anchor and the office both carry it.
/// * `Ok(AlreadyInPlace)`  - The anchor and the office already carried the install; nothing was
///                           written and no precondition was checked.
/// * `Err(RefUpdateError)` - `OfficeMoved`, `AnchorMoved` or `PalletsMoved` if what the caller read
///                           moved (nothing was written); `Failed` if the chain of custody does
///                           not match or a write failed (the warehouse is as before, unless the
///                           message says otherwise).
pub fn install_trust(install: &TrustInstall) -> Result<TrustInstalled, RefUpdateError> {
    if let Some(replaced) = install.replaces {
        let replaced = std::str::from_utf8(replaced)
            .map_err(|e| format!("The trust file is not valid UTF-8: {}", e))
            .and_then(parse_trust_anchor)?;

        if install.anchor.prior_genesis.as_deref() != Some(replaced.genesis.as_str()) {
            return Err(RefUpdateError::Failed(format!(
                "The new anchor does not name this warehouse's genesis ({}) as its prior; \
                the chain of custody does not match.",
                replaced.genesis
            )));
        }

        if install.anchor.adopts.is_none() {
            return Err(RefUpdateError::Failed(
                "A re-genesis anchor must pin the office head it adopts.".to_string()
            ));
        }
    }

    let bytes = encode_trust_anchor(install.anchor).into_bytes();
    let office_target = install.new_office.or(install.office);

    let lock = crate::util::lock_utils::RefLock::acquire()?;

    if read_trust_anchor_bytes()?.as_deref() == Some(bytes.as_slice())
        && pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME)?.as_deref() == office_target {
        return Ok(TrustInstalled::AlreadyInPlace);
    }

    pallet_utils::check_preconditions(&lock, &RefPreconditions {
        office: OfficePrecondition::At(install.office.map(str::to_string)),
        anchor: match install.replaces {
            Some(replaced) => AnchorPrecondition::Bytes(replaced.to_vec()),
            None => AnchorPrecondition::Absent,
        },
        pallets: install.pallets.clone(),
    })?;

    let error = match write_trust_install(&lock, install, &bytes) {
        Ok(()) => return Ok(TrustInstalled::Written),
        Err(error) => error,
    };

    // Read back, still inside the hold: what a failed write left decides what to put back.
    let anchor_now = read_trust_anchor_bytes();
    let office_now = pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME);

    let (anchor_now, office_now) = match (anchor_now, office_now) {
        (Ok(anchor_now), Ok(office_now)) => (anchor_now, office_now),
        (anchor_now, office_now) => return Err(RefUpdateError::Failed(format!(
            "{} The trust file and the office could not be read back ({}), so neither was put \
            back; the trust anchor may not match the office.",
            error,
            [anchor_now.err(), office_now.err()].into_iter().flatten().collect::<Vec<_>>().join("; ")
        ))),
    };

    let anchor_is_new = anchor_now.as_deref() == Some(bytes.as_slice());

    if anchor_is_new && office_now.as_deref() == office_target {
        return Err(RefUpdateError::Failed(format!(
            "{} The trust anchor and the office were both written and stand, so nothing was put \
            back, but they may not be on disk durably yet.",
            error
        )));
    }

    if anchor_now.as_deref() == install.replaces && office_now.as_deref() == install.office {
        return Err(error);
    }

    if anchor_is_new && office_now.as_deref() == install.office {
        return match restore_trust_file(install.replaces) {
            Ok(()) => Err(error),
            Err(e) => Err(RefUpdateError::Failed(format!(
                "{} The previous trust anchor could not be put back either ({}): the trust anchor \
                is the new one while the office is not.",
                error, e
            ))),
        };
    }

    Err(RefUpdateError::Failed(format!(
        "{} The trust file and the office were left in a state this install did not write (the \
        trust anchor is {}, the office head is {}); run \"forklift audit\" to see whether they \
        match.",
        error,
        if anchor_is_new { "the new one" } else { "not the new one" },
        office_now.as_deref().unwrap_or("unborn")
    )))
}

/// The writes of [`install_trust`], under its hold: the anchor, then the office.
fn write_trust_install(lock: &crate::util::lock_utils::RefLock,
                       install: &TrustInstall,
                       bytes: &[u8]) -> Result<(), RefUpdateError> {
    file_utils::write_file_atomically(&forklift_root().join(FILE_NAME_TRUST), bytes)?;

    #[cfg(test)]
    tests::BETWEEN_TRUST_WRITES.with(|seam| seam.take().map(|seam| seam()));

    let Some(new_office) = install.new_office.filter(|new| Some(*new) != install.office) else {
        return Ok(());
    };

    #[cfg(test)]
    let fault = tests::OFFICE_WRITE_FAULT.with(|fault| fault.replace(tests::OfficeWriteFault::None));

    #[cfg(test)]
    if fault == tests::OfficeWriteFault::Refuse {
        return Err(RefUpdateError::Failed("injected office write refusal".to_string()));
    }

    let written = pallet_utils::update_pallet_head_locked(
        lock, pallet_utils::PalletNamespace::Meta, OFFICE_PALLET_NAME, install.office, new_office, None,
    );

    #[cfg(test)]
    if written.is_ok() && fault == tests::OfficeWriteFault::AfterLanding {
        return Err(RefUpdateError::Failed("injected office write failure".to_string()));
    }

    written
}

/// Put the trust file back to `previous` (`None`: remove it), under [`install_trust`]'s hold.
fn restore_trust_file(previous: Option<&[u8]>) -> Result<(), String> {
    let path = forklift_root().join(FILE_NAME_TRUST);

    match previous {
        Some(previous) => file_utils::write_file_atomically(&path, previous),
        None => {
            std::fs::remove_file(&path)
                .map_err(|e| format!("Error while removing the trust file: {}", e))?;
            file_utils::sync_dir(&forklift_root())
        }
    }
}

/// Write a trust anchor where none is held, for a test that needs a signed warehouse without an
/// office ceremony.
#[cfg(test)]
pub(crate) fn write_trust_anchor(anchor: &TrustAnchor) -> Result<(), String> {
    let _ref_lock = crate::util::lock_utils::RefLock::acquire()?;
    let path = forklift_root().join(FILE_NAME_TRUST);

    if path.exists() {
        return Err("Trust is already established for this warehouse; it cannot be re-established.".to_string());
    }

    file_utils::write_file_atomically(&path, encode_trust_anchor(anchor).as_bytes())
}

/// Serialize an anchor as the trust file holds it.
pub fn encode_trust_anchor(anchor: &TrustAnchor) -> String {
    let mut doc = DocumentMut::new();
    doc["genesis"] = value(anchor.genesis.as_str());
    doc["enabled_at"] = value(anchor.enabled_at);

    let mut boundary = toml_edit::Array::new();

    for hash in &anchor.boundary {
        boundary.push(hash.as_str());
    }

    doc["boundary"] = value(boundary);

    if let Some(prior_genesis) = &anchor.prior_genesis {
        doc["prior_genesis"] = value(prior_genesis.as_str());
    }

    if let Some(adopts) = &anchor.adopts {
        doc["adopts"] = value(adopts.as_str());
    }

    doc.to_string()
}

/// Read the full office state from the office pallet's head. An unborn office pallet
/// yields an empty state.
///
/// # Returns
/// * `Ok(OfficeState)` - The state.
/// * `Err(String)`     - If an object or record could not be read.
pub fn read_office_state() -> Result<OfficeState, String> {
    Ok(read_office_state_and_head()?.0)
}

/// Read the full office state and the office head it was read from, for a caller that stacks a
/// changed state: the head is the base its office parcel must extend (see
/// [`stack_office_parcel`]). An unborn office pallet yields an empty state and no head.
///
/// # Returns
/// * `Ok((OfficeState, Option<String>))` - The state, and the head it was read from.
/// * `Err(String)`                       - If an object or record could not be read.
pub fn read_office_state_and_head() -> Result<(OfficeState, Option<String>), String> {
    let Some(head) = pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME)? else {
        return Ok((OfficeState { users: Vec::new(), keys: Vec::new() }, None));
    };

    Ok((read_office_state_of(&head)?, Some(head)))
}

/// Read the office state as recorded by a specific office parcel.
///
/// # Arguments
/// * `office_parcel_hash` - The hash of the office parcel.
///
/// # Returns
/// * `Ok(OfficeState)` - The state.
/// * `Err(String)`     - If an object or record could not be read.
pub fn read_office_state_of(office_parcel_hash: &str) -> Result<OfficeState, String> {
    let tree_hash = object_utils::load_parcel(office_parcel_hash)?.tree_hash;

    let mut users: Vec<UserRecord> = Vec::new();
    let mut keys: Vec<KeyRecord> = Vec::new();

    let Some(tracked) = resolve_subtree(
        office_parcel_hash, &tree_hash, &[TREE_NAME_FORKLIFT, TREE_NAME_TRACKED],
    )? else {
        return Ok(OfficeState { users, keys });
    };

    // A `users` or `keys` file instead of a folder would otherwise read as an office with no
    // such records.
    if let Some((name, _)) = tracked.get_files()
        .find(|(name, _)| name.as_str() == TREE_NAME_USERS || name.as_str() == TREE_NAME_KEYS) {
        return Err(file_in_place_of_folder(
            office_parcel_hash, &[TREE_NAME_FORKLIFT, TREE_NAME_TRACKED, name.as_str()],
        ));
    }

    for (name, item) in tracked.get_subtrees() {
        let subtree = object_utils::load_tree(&item.hash)?;

        if name == TREE_NAME_USERS || name == TREE_NAME_KEYS {
            if let Some((nested, _)) = subtree.get_subtrees().next() {
                return Err(format!(
                    "The office parcel {} holds a folder {}/{}, but {}/ holds only record files. \
                    Forklift never writes a folder there, so this office may have been tampered with.",
                    office_parcel_hash, name, nested, name
                ));
            }
        }

        // A record that cannot be loaded or does not parse is named by parcel and file, as a
        // misfiled one is.
        let unreadable = |file_name: &str, error: String| format!(
            "The office parcel {} holds a record {}/{} that cannot be read: {}",
            office_parcel_hash, name, file_name, error
        );
        let unparseable = |file_name: &str, error: String| format!(
            "The office parcel {} holds a record {}/{} that does not parse: {}",
            office_parcel_hash, name, file_name, error
        );

        match name.as_str() {
            TREE_NAME_USERS => {
                for (file_name, file) in subtree.get_files() {
                    let toml = load_record(&file.hash).map_err(|e| unreadable(file_name, e))?;
                    let user = parse_user_record(&toml).map_err(|e| unparseable(file_name, e))?;
                    verify_record_file_name(
                        office_parcel_hash, TREE_NAME_USERS, file_name,
                        &user_record_file_name(&user), &format!("user {}", user.identifier),
                    )?;
                    users.push(user);
                }
            }
            TREE_NAME_KEYS => {
                for (file_name, file) in subtree.get_files() {
                    let toml = load_record(&file.hash).map_err(|e| unreadable(file_name, e))?;
                    let key = parse_key_record(&toml).map_err(|e| unparseable(file_name, e))?;
                    verify_record_file_name(
                        office_parcel_hash, TREE_NAME_KEYS, file_name,
                        &key_record_file_name(&key), &format!("key {}", key.key_id),
                    )?;
                    keys.push(key);
                }
            }
            _ => {}
        }
    }

    users.sort_by(|a, b| a.identifier.cmp(&b.identifier));
    keys.sort_by(|a, b| a.issued_at.cmp(&b.issued_at));

    Ok(OfficeState { users, keys })
}

/// Stack a new office parcel recording the given state, sign it, and advance the office
/// pallet head.
///
/// # Arguments
/// * `state`          - The full office state to record.
/// * `actor`          - The operator performing the change.
/// * `description`    - The parcel description (the audit line).
/// * `signing_key_id` - The key to sign with (its private half must be local).
/// * `base`           - The office head `state` was read from (`None`: the office is unborn). It
///                      is the new parcel's parent, and the office ref moves only while it still
///                      holds it.
/// * `preconditions`  - What the change's decision consumed besides the office: the anchor it
///                      was judged under, and the boundary it records.
///
/// # Returns
/// * `Ok(String)`          - The hash of the new office parcel.
/// * `Err(RefUpdateError)` - `Failed` if an object could not be built, stored or signed, or the
///                           office ref write's refusal or failure.
pub fn stack_office_parcel(state: &OfficeState,
                           actor: &Operator,
                           description: String,
                           signing_key_id: &str,
                           base: Option<&str>,
                           preconditions: &RefPreconditions) -> Result<String, RefUpdateError> {
    let parents: Vec<String> = base.map(str::to_string).into_iter().collect();
    let hash = build_and_sign_office_parcel(state, actor, description, signing_key_id, parents)?;

    pallet_utils::update_meta_pallet_head(OFFICE_PALLET_NAME, base, &hash, preconditions)?;

    Ok(hash)
}

/// Build, store and sign an office parcel — everything [`stack_office_parcel`] does except
/// advancing the office ref. For a caller that moves the ref itself: a trust install (enroll's
/// genesis, a re-genesis's parentless one), which moves it together with the anchor, or a test
/// exercising `post_ref_update`'s own handling of a lift.
///
/// # Returns
/// * `Ok(String)`  - The hash of the new (unlinked) office parcel.
/// * `Err(String)` - If an object could not be built, stored or signed.
pub fn build_and_sign_office_parcel(state: &OfficeState,
                                    actor: &Operator,
                                    description: String,
                                    signing_key_id: &str,
                                    parents: Vec<String>) -> Result<String, String> {
    // Refuse, before anything is stored or signed, a state no reader would accept: a key
    // whose id is not its public key's, or two records for one subject (which would also
    // collapse into one file here, silently dropping the other).
    let mut user_records: Vec<(String, String)> = Vec::new();
    let mut user_files: HashSet<String> = HashSet::new();

    for user in &state.users {
        let file_name = user_record_file_name(user);

        if !user_files.insert(file_name.clone()) {
            return Err(format!("The office state holds two records for user {}.", user.identifier));
        }

        user_records.push((file_name, user_record_to_toml(user)));
    }

    let mut key_records: Vec<(String, String)> = Vec::new();
    let mut key_files: HashSet<String> = HashSet::new();

    for key in &state.keys {
        let actual_id = key_id_of(&key.public_key)?;

        if key.key_id != actual_id {
            return Err(format!(
                "The office state holds key {} with a public key whose id is {}.", key.key_id, actual_id
            ));
        }

        let file_name = key_record_file_name(key);

        if !key_files.insert(file_name.clone()) {
            return Err(format!("The office state holds two records for key {}.", key.key_id));
        }

        key_records.push((file_name, key_record_to_toml(key)));
    }

    let mut users_tree = TreeItem::new(TREE_NAME_USERS.to_string(), String::new(), DirEntryType::Tree);

    for (file_name, toml) in user_records {
        users_tree.add_child(TreeItem::new(file_name, store_record(&toml)?, DirEntryType::Normal));
    }

    let mut keys_tree = TreeItem::new(TREE_NAME_KEYS.to_string(), String::new(), DirEntryType::Tree);

    for (file_name, toml) in key_records {
        keys_tree.add_child(TreeItem::new(file_name, store_record(&toml)?, DirEntryType::Normal));
    }

    store_subtree(&mut users_tree)?;
    store_subtree(&mut keys_tree)?;

    let mut tracked_tree = TreeItem::new(TREE_NAME_TRACKED.to_string(), String::new(), DirEntryType::Tree);
    tracked_tree.add_child(users_tree);
    tracked_tree.add_child(keys_tree);
    store_subtree(&mut tracked_tree)?;

    let mut forklift_tree = TreeItem::new(TREE_NAME_FORKLIFT.to_string(), String::new(), DirEntryType::Tree);
    forklift_tree.add_child(tracked_tree);
    store_subtree(&mut forklift_tree)?;

    let mut root_tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
    root_tree.add_child(forklift_tree);
    let mut root_object = LooseObjectBuilder::build_tree(&root_tree);
    root_object.store()?;

    let timestamp = Utc::now();

    let parcel = Parcel {
        tree_hash: root_object.hash.clone(),
        parents,
        actions: vec![
            ParcelAction {
                operator: actor.clone(),
                action: ParcelActionType::Author,
                description: None,
                timestamp,
            },
            ParcelAction {
                operator: actor.clone(),
                action: ParcelActionType::Stack,
                description: None,
                timestamp,
            },
        ],
        description: Some(description),
    };

    let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
    parcel_object.store()?;

    let signature = sign_utils::sign_parcel_hash(signing_key_id, &parcel_object.hash)?;
    sign_utils::store_parcel_signature(&parcel_object.hash, &signature)?;

    Ok(parcel_object.hash)
}

/// Build and store one subtree's object, recording the hash on the tree item (so it can
/// be added to its parent afterwards).
fn store_subtree(tree: &mut TreeItem) -> Result<(), String> {
    let mut object = LooseObjectBuilder::build_tree(tree);
    tree.hash = object.hash.clone();
    object.store()?;

    Ok(())
}

/// Store one TOML record as a blob.
fn store_record(toml: &str) -> Result<String, String> {
    let blob = Blob { content: toml.as_bytes().to_vec() };
    let mut object = LooseObjectBuilder::build_blob(&blob);
    object.store()?;

    Ok(object.hash)
}

/// Load one record blob as a string.
fn load_record(hash: &str) -> Result<String, String> {
    String::from_utf8(object_utils::load_blob(hash)?.content)
        .map_err(|_| format!("The metadata record {} is not valid UTF-8.", hash))
}

/// [`load_record`]'s bytes-in-hand sibling — parses a record blob's content as a string from an
/// already-in-hand classified read's bytes, rather than triggering a second, independent read of
/// the same hash. See [`object_utils::parse_local_tree_bytes`]'s own doc comment for why
/// [`load_object_classified`] needs this shape.
fn parse_record_bytes(hash: &str, bytes: &[u8]) -> Result<String, String> {
    String::from_utf8(object_utils::parse_local_blob_bytes(hash, bytes)?.content)
        .map_err(|_| format!("The metadata record {} is not valid UTF-8.", hash))
}

/// Resolve a chain of subtree names from a root tree.
///
/// # Returns
/// * `Ok(Some(TreeItem))` - The resolved (loaded) subtree.
/// * `Ok(None)`           - If a component does not exist.
/// * `Err(String)`        - If a tree object could not be loaded, or a component is a file.
fn resolve_subtree(office_parcel_hash: &str, root_tree_hash: &str, path: &[&str])
    -> Result<Option<TreeItem>, String> {
    let mut current = object_utils::load_tree(root_tree_hash)?;

    for (depth, component) in path.iter().enumerate() {
        if current.get_files().any(|(name, _)| name == component) {
            return Err(file_in_place_of_folder(office_parcel_hash, &path[..=depth]));
        }

        let subtree_hash = current.get_subtrees()
            .find(|(name, _)| name == component)
            .map(|(_, item)| item.hash.clone());

        match subtree_hash {
            Some(hash) => current = object_utils::load_tree(&hash)?,
            None => return Ok(None),
        }
    }

    Ok(Some(current))
}

/// Serialize a user record as TOML.
fn user_record_to_toml(user: &UserRecord) -> String {
    let mut doc = DocumentMut::new();

    doc["identifier"] = value(user.identifier.as_str());
    doc["enrolled_at"] = value(user.enrolled_at);
    doc["role"] = value(user.role.as_str());
    doc["identity_root"] = value(user.identity_root.as_str());

    // Human is the default; only automated identities carry the class (and a
    // supervisor), so human records keep their historical shape.
    if user.class != IdentityClass::Human {
        doc["class"] = value(user.class.as_str());
    }

    if let Some(supervisor) = &user.supervisor {
        doc["supervisor"] = value(supervisor.as_str());
    }

    if !user.pallets.is_empty() {
        let mut pallets = toml_edit::Array::new();

        for pallet in &user.pallets {
            pallets.push(pallet.as_str());
        }

        doc["pallets"] = value(pallets);
    }

    doc.to_string()
}

/// Parse a user record from TOML.
fn parse_user_record(toml: &str) -> Result<UserRecord, String> {
    let doc: DocumentMut = toml.parse()
        .map_err(|e| format!("A user record is not valid TOML: {}", e))?;

    let role = Role::parse(&read_string(&doc, "role", "user record")?)?;

    // Optional, strict (FORK-81 follow-up): `user_record_to_toml` omits `pallets` entirely for
    // an unrestricted writer (the empty-list case), so an absent key must keep parsing as an
    // empty list — that is the field's normal, expected shape, not an old/lenient one. But
    // `may_write_pallet` reads an empty list as "may write ANY pallet" (`Role::Writer =>
    // self.pallets.is_empty() || …`), so this is an authorization check: a *present* value that
    // is not an array, or an array with a non-string entry, must never be silently downgraded to
    // that same empty list the way the previous `.and_then(as_array).map(filter_map(as_str))
    // .unwrap_or_default()` did — that would turn a writer meant to be restricted to one pallet
    // into an unrestricted one the moment their grant record is damaged, exactly backwards from
    // CLAUDE.md's fail-loudly rule for an authorization check. Mirrors `distrust_boundary`'s
    // absent-vs-malformed split below (`parse_key_record`) — not `boundary`'s just above, which
    // is required and has the opposite absent-case policy (a missing key is corruption, not a
    // default).
    let pallets = read_optional_string_array(&doc, "pallets", "user record")?;

    // Optional, strict, same shape as `pallets`: `user_record_to_toml` omits `class` entirely
    // for a human (the default), so an absent key legitimately means `Human`. A *present*
    // non-string value must still error rather than silently collapse to that same default —
    // `IdentityClass` gates admission rules (an agent record requires a supervisor, §7.1) and
    // audit/query classification (`author.class`), so quietly misreading a damaged automated
    // identity as human would hide exactly the provenance distinction the field exists to carry.
    let class = match read_optional_string(&doc, "class", "user record")? {
        Some(class) => IdentityClass::parse(&class)?,
        None => IdentityClass::Human,
    };

    // Optional, strict, same shape again: absent means "no supervisor" (every human, and any
    // unsupervised bot/service), which stays the parse of a missing key. A present non-string
    // value must error instead of silently reading as "unsupervised" — that would quietly drop
    // the one piece of accountability an automated identity's record carries.
    let supervisor = read_optional_string(&doc, "supervisor", "user record")?;

    Ok(UserRecord {
        identifier: read_string(&doc, "identifier", "user record")?,
        enrolled_at: read_integer(&doc, "enrolled_at", "user record")?,
        role,
        pallets,
        identity_root: read_string(&doc, "identity_root", "user record")?,
        class,
        supervisor,
    })
}

/// Serialize a key record as TOML.
fn key_record_to_toml(key: &KeyRecord) -> String {
    let mut doc = DocumentMut::new();

    doc["operator"] = value(key.operator.as_str());
    doc["public_key"] = value(key.public_key.as_str());
    doc["issued_at"] = value(key.issued_at);

    if let Some(retired_at) = key.retired_at {
        doc["retired_at"] = value(retired_at);
    }

    if let Some(reason) = key.revocation_reason {
        doc["revocation_reason"] = value(reason.as_str());
    }

    if !key.distrust_boundary.is_empty() {
        let mut boundary = toml_edit::Array::new();

        for hash in &key.distrust_boundary {
            boundary.push(hash.as_str());
        }

        doc["distrust_boundary"] = value(boundary);
    }

    doc["authorized_by"] = value(key.authorized_by.as_str());
    doc["endorsement"] = value(key.endorsement.as_str());
    doc["proof_of_possession"] = value(key.proof_of_possession.as_str());

    doc.to_string()
}

/// Parse a key record from TOML.
fn parse_key_record(toml: &str) -> Result<KeyRecord, String> {
    let doc: DocumentMut = toml.parse()
        .map_err(|e| format!("A key record is not valid TOML: {}", e))?;

    // Optional, strict, same absent-vs-malformed split as `distrust_boundary` below:
    // `key_record_to_toml` omits `revocation_reason` for an active (never-revoked) key, so an
    // absent key must keep parsing as `None`. A present non-string value must still error
    // rather than fall through the same `None` arm — `verify_key_permanence` (audit_utils)
    // treats a revoked key with `revocation_reason.is_none()` as "revoked without a reason" and
    // refuses the parcel, so silently losing a present-but-damaged reason to `None` would turn a
    // legitimately-reasoned revocation into a spurious rejection instead of a loud parse error
    // naming the actual problem.
    let revocation_reason = match read_optional_string(&doc, "revocation_reason", "key record")? {
        Some(reason) => Some(RevocationReason::parse(&reason)?),
        None => None,
    };

    // Optional, unlike the trust anchor's `boundary`: `key_record_to_toml` omits this key
    // entirely for an active (never-revoked) key — the overwhelming majority of records — so a
    // missing key here is the normal, expected shape and must parse as an empty boundary, not an
    // error. What must still fail loudly is a *present* entry that is malformed: a non-array
    // value, or any non-string element, silently dropped by the previous `filter_map` — exactly
    // the same under-counting hazard `boundary`'s strict parse above closes, just for a field
    // that is legitimately absent instead of always-present.
    let distrust_boundary = read_optional_string_array(&doc, "distrust_boundary", "key record")?;

    // A key's id is derived, never stored (see `key_id_of`). A record that still states one is
    // not one this build wrote, and its id claim may disagree with the derived one; refuse it
    // rather than drop the claim without a word.
    if doc.contains_key("key_id") {
        return Err("A key record has a \"key_id\" entry. Key records no longer store their id, \
            which is derived from the public key, so this record was not written by this version \
            of forklift.".to_string());
    }

    let public_key = read_string(&doc, "public_key", "key record")?;

    Ok(KeyRecord {
        key_id: key_id_of(&public_key)?,
        operator: read_string(&doc, "operator", "key record")?,
        public_key,
        issued_at: read_integer(&doc, "issued_at", "key record")?,
        // Optional, strict, same split: absent means the key is active (the common case;
        // `key_record_to_toml` omits it), but a present non-integer value must error rather than
        // silently read as "active". `retired_at` is documented as display metadata — validity
        // is decided by the distrust boundary, never by time — but `is_active`/`active_keys_of`/
        // `signing_key_of` still read it to pick which of an operator's own keys a local sign
        // uses, and `verify_key_permanence`'s append-once check (audit_utils) compares it across
        // parcels the same way it compares `revocation_reason`; silently collapsing a damaged
        // present value to "active" would misinform both instead of naming the actual damage.
        retired_at: read_optional_integer(&doc, "retired_at", "key record")?,
        revocation_reason,
        distrust_boundary,
        authorized_by: read_string(&doc, "authorized_by", "key record")?,
        endorsement: read_string(&doc, "endorsement", "key record")?,
        proof_of_possession: read_string(&doc, "proof_of_possession", "key record")?,
    })
}

/// The id of a hex-encoded public key. A key record does not store its id: every lookup
/// ([`OfficeState::find_key`], a signature sidecar's key id) goes by id, while the
/// endorsement and proof-of-possession sign only the public key, so a stored id could name
/// one key over another's key material (FORK-74).
fn key_id_of(public_key_hex: &str) -> Result<String, String> {
    let key_bytes = sign_utils::from_hex(public_key_hex).map_err(|_| {
        let shown: String = public_key_hex.chars().take(80).collect();
        let more = if shown.len() < public_key_hex.len() { "…" } else { "" };
        format!("The public key {:?}{} is not valid hex.", shown, more)
    })?;

    Ok(sign_utils::key_id_for_public_key(&key_bytes))
}

/// The file a key record is filed under in the office tree: `<key_id>.toml`.
fn key_record_file_name(key: &KeyRecord) -> String {
    format!("{}{}", key.key_id, RECORD_SUFFIX)
}

/// The file a user record is filed under in the office tree: `<blake3(identifier)>.toml`.
fn user_record_file_name(user: &UserRecord) -> String {
    format!("{}{}", blake3::hash(user.identifier.as_bytes()).to_hex(), RECORD_SUFFIX)
}

/// Check that a record sits in the file its own subject names. Tree entry names are unique,
/// so this is also what makes two records for one subject impossible: every reader resolves a
/// subject to its first match, and a second record would be unreachable to some checks and
/// decisive for others (FORK-74).
///
/// # Arguments
/// * `office_parcel_hash` - The office parcel being read, for the message.
/// * `tree_name`          - The record subtree (`users`, `keys`), for the message.
/// * `file_name`          - The file the record was read from.
/// * `expected`           - The file its subject names.
/// * `subject`            - The subject, described for the message ("key <id>").
fn verify_record_file_name(office_parcel_hash: &str,
                           tree_name: &str,
                           file_name: &str,
                           expected: &str,
                           subject: &str) -> Result<(), String> {
    if file_name == expected {
        return Ok(());
    }

    Err(format!(
        "The office parcel {} files the record for {} as {}/{}, but that record belongs in \
        {}/{}. Forklift never writes a record under another name, so this office may have been \
        tampered with.",
        office_parcel_hash, subject, tree_name, file_name, tree_name, expected
    ))
}

/// Read a required string field from a TOML document, strictly: an absent key errors naming
/// the record kind and field ("has no ... entry"), and so does a *present* key that is not a
/// string — but with its own wording ("is present but is not a string"), so a hand-edited
/// `genesis = 7` is never reported as missing. Same two-arm shape as [`read_integer`], for a
/// field this record kind always writes.
fn read_string(doc: &DocumentMut, field: &str, record_kind: &str) -> Result<String, String> {
    let Some(item) = doc.get(field) else {
        return Err(format!("{} has no \"{}\" entry.", record_kind_prefix(record_kind), field));
    };

    item.as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!(
            "{}'s \"{}\" entry is present but is not a string.", record_kind_prefix(record_kind), field
        ))
}

/// Record kinds that exist exactly once per warehouse, so `"a {kind}"` would misleadingly
/// suggest there could be more than one — these take the definite article instead. Every
/// other record kind recurs many times (`"a user record"`, `"a key record"`, ...). Explicit
/// list, not a single hardcoded comparison (PR #122 round 5 F6): a future unique-per-
/// warehouse kind must be added here, or it silently falls through to the indefinite article
/// below.
const UNIQUE_RECORD_KINDS: [&str; 1] = ["trust file"];

/// Record kinds whose first *letter* is a vowel but whose first *sound* is not — a leading
/// "y" glide, as in "user" ("yoo-zer") — so English picks "a", not the "an" a spelling-only
/// check below would otherwise pick. The reverse (a consonant letter with a vowel sound, e.g.
/// "hour") does not occur among today's record kinds; add it here if it ever does.
const CONSONANT_SOUNDING_RECORD_KINDS: [&str; 1] = ["user record"];

/// The record-kind noun phrase for a message like `"{} has no ... entry."`, with its article.
/// Capitalized: every caller uses this at the start of a sentence.
///
/// The indefinite article is picked from the record kind's first letter (PR #122 round 5 F6):
/// hardcoding "A" for everything is correct only by coincidence for today's seven kinds — a
/// future vowel-initial kind (e.g. "office record") would silently read "A office record".
fn record_kind_prefix(record_kind: &str) -> String {
    if UNIQUE_RECORD_KINDS.contains(&record_kind) {
        return format!("The {}", record_kind);
    }

    let starts_with_vowel_sound = record_kind.starts_with(|c: char| "aeiouAEIOU".contains(c))
        && !CONSONANT_SOUNDING_RECORD_KINDS.contains(&record_kind);

    let article = if starts_with_vowel_sound { "An" } else { "A" };
    format!("{} {}", article, record_kind)
}

/// Read a required integer field from a TOML document, strictly: an absent key errors
/// naming the record kind and field ("has no ... entry"), and so does a *present* key
/// that is not an integer — but with its own wording ("is present but is not an
/// integer"), so a hand-edited, quoted `tagged_at = "1700000000"` is never reported as
/// missing. Same two-arm shape as [`read_optional_integer`], for a field this record
/// kind always writes.
pub(crate) fn read_integer(doc: &DocumentMut, field: &str, record_kind: &str) -> Result<i64, String> {
    let Some(item) = doc.get(field) else {
        return Err(format!("{} has no \"{}\" entry.", record_kind_prefix(record_kind), field));
    };

    item.as_integer()
        .ok_or_else(|| format!(
            "{}'s \"{}\" entry is present but is not an integer.", record_kind_prefix(record_kind), field
        ))
}

/// Read an optional string field, strictly: an absent key is `None` — a defined, expected
/// shape for a field the writer genuinely omits — but a *present* key must be a string, or
/// this errors naming the record kind and field. Never conflates "absent" with "present but
/// the wrong type" the way `doc.get(field).and_then(|item| item.as_str())` collapsing both
/// cases to `None` would (FORK-81 follow-up: the same lenience `pallets` had).
fn read_optional_string(doc: &DocumentMut, field: &str, record_kind: &str) -> Result<Option<String>, String> {
    let Some(item) = doc.get(field) else { return Ok(None); };

    item.as_str()
        .map(|s| Some(s.to_string()))
        .ok_or_else(|| format!(
            "{}'s \"{}\" entry is present but is not a string.", record_kind_prefix(record_kind), field
        ))
}

/// [`read_optional_string`]'s integer counterpart: an absent key is `None`, a present
/// non-integer value errors naming the record kind and field.
fn read_optional_integer(doc: &DocumentMut, field: &str, record_kind: &str) -> Result<Option<i64>, String> {
    let Some(item) = doc.get(field) else { return Ok(None); };

    item.as_integer()
        .map(Some)
        .ok_or_else(|| format!(
            "{}'s \"{}\" entry is present but is not an integer.", record_kind_prefix(record_kind), field
        ))
}

/// Read a required array-of-hashes field, strictly: the key must be present and every entry must
/// be a string, or this errors naming the record kind, the field and (for a bad entry) its index
/// — never silently drops an entry the way a `filter_map` would. For a field the writer always
/// serializes (even when empty), a missing key is corruption, not an old/lenient shape.
fn read_required_string_array(doc: &DocumentMut, field: &str, record_kind: &str) -> Result<Vec<String>, String> {
    // Two arms, like every other reader here: collapsing them would report `boundary = 7` as
    // "has no boundary array entry" while the key is plainly there, sending the reader to add a
    // field that already exists. This was the last single-arm reader in the file.
    let Some(item) = doc.get(field) else {
        return Err(format!("{} has no \"{}\" array entry.", record_kind_prefix(record_kind), field));
    };

    let array = item.as_array()
        .ok_or_else(|| format!(
            "{}'s \"{}\" entry is present but is not an array.", record_kind_prefix(record_kind), field
        ))?;

    strict_string_array(array, field, record_kind)
}

/// Read an optional array-of-hashes field, strictly: an absent key is a valid empty list (the
/// field's normal, expected shape when the writer omits it), but a *present* key must be an
/// array of strings, or this errors the same way [`read_required_string_array`] does — an absent
/// field and a malformed one are never conflated.
fn read_optional_string_array(doc: &DocumentMut, field: &str, record_kind: &str) -> Result<Vec<String>, String> {
    let Some(item) = doc.get(field) else { return Ok(Vec::new()); };

    let array = item.as_array()
        .ok_or_else(|| format!(
            "{}'s \"{}\" entry is present but is not an array.", record_kind_prefix(record_kind), field
        ))?;

    strict_string_array(array, field, record_kind)
}

/// Shared body: every entry of `array` must be a string, or this errors naming the record kind,
/// the field and the offending entry's index — the strict counterpart of a `filter_map` that
/// would otherwise silently drop a non-string entry (under-counting whatever this array roots).
fn strict_string_array(array: &toml_edit::Array, field: &str, record_kind: &str) -> Result<Vec<String>, String> {
    array.iter()
        .enumerate()
        .map(|(index, entry)| entry.as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| format!(
                "{} has a non-string entry in \"{}\" at index {}.", record_kind_prefix(record_kind), field, index
            )))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        /// Run between [`install_trust`]'s anchor write and its office write.
        pub(crate) static BETWEEN_TRUST_WRITES: Cell<Option<Box<dyn FnOnce()>>> = const { Cell::new(None) };

        /// Fail [`install_trust`]'s office write once.
        pub(crate) static OFFICE_WRITE_FAULT: Cell<OfficeWriteFault> = const { Cell::new(OfficeWriteFault::None) };
    }

    /// How [`OFFICE_WRITE_FAULT`] fails the office write.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum OfficeWriteFault {
        None,

        /// Fail without writing.
        Refuse,

        /// Write, then report failure.
        AfterLanding,
    }

    fn user(class: IdentityClass, supervisor: Option<&str>) -> UserRecord {
        UserRecord {
            identifier: "op@x".to_string(),
            enrolled_at: 7,
            role: Role::Writer,
            pallets: Vec::new(),
            identity_root: "root".to_string(),
            class,
            supervisor: supervisor.map(str::to_string),
        }
    }

    #[test]
    fn identity_class_and_supervisor_round_trip() {
        let agent = user(IdentityClass::Agent, Some("alice"));
        let parsed = parse_user_record(&user_record_to_toml(&agent)).unwrap();
        assert_eq!(parsed.class, IdentityClass::Agent);
        assert_eq!(parsed.supervisor.as_deref(), Some("alice"));

        // A human record carries no class/supervisor keys (historical shape), and
        // parses back to Human with no supervisor.
        let human = user(IdentityClass::Human, None);
        let toml = user_record_to_toml(&human);
        assert!(!toml.contains("class"));
        assert!(!toml.contains("supervisor"));
        let parsed = parse_user_record(&toml).unwrap();
        assert_eq!(parsed.class, IdentityClass::Human);
        assert!(parsed.supervisor.is_none());
    }

    #[test]
    fn a_record_without_a_class_defaults_to_human() {
        // Any record predating the class field (or a hand-written one) is a human.
        let toml = "identifier = \"op@x\"\nenrolled_at = 1\nrole = \"admin\"\nidentity_root = \"r\"\n";
        let parsed = parse_user_record(toml).unwrap();
        assert_eq!(parsed.class, IdentityClass::Human);
    }

    /// `verify_self_service_change` (in `audit_utils`) protects user records by
    /// comparing whole `UserRecord` values instead of a hand-picked field projection
    /// (FORK-76: the projection it replaced named five of seven fields and silently let
    /// `class` and `supervisor` drift). That guard is only as strong as this derive: if
    /// every field here did not genuinely participate in `==`, a whole-record compare
    /// would be no safer than the projection it replaced.
    ///
    /// Two separate guarantees are in play, and they come from different places:
    ///
    /// - Automatic, no maintenance required: because `#[derive(PartialEq)]` compares
    ///   structurally (there is no way to derive it over only some fields), a field
    ///   added to `UserRecord` tomorrow is swept into `==` — and therefore into
    ///   `verify_self_service_change` — without anyone touching this test.
    /// - What THIS test actually covers: the seven fields that exist today, changed one
    ///   at a time against `base`, proving none of them is silently excluded from `==`.
    ///   A field added in the future is NOT automatically exercised here — nothing
    ///   forces a matching variant to be added below, because there is no Rust
    ///   construct that enforces "one variant per field".
    ///
    /// The one compile-time tripwire that does exist is the exhaustive `base` literal
    /// itself: a new field fails the build there (E0063, missing field) until someone
    /// fills it in. That failure is the cue to add a matching variant above — it does
    /// not add the variant for you.
    #[test]
    fn user_record_equality_distinguishes_every_current_field() {
        // Exhaustive on purpose (no `..Default::default()`): adding a field to
        // `UserRecord` fails the build right here (E0063) rather than silently. When
        // that happens, add a corresponding variant below — the compile error only
        // forces this literal to stay exhaustive, not the variant list.
        let base = UserRecord {
            identifier: "op@x".to_string(),
            enrolled_at: 7,
            role: Role::Writer,
            pallets: vec!["main".to_string()],
            identity_root: "root".to_string(),
            class: IdentityClass::Human,
            supervisor: None,
        };

        assert_eq!(base, base.clone(), "a record must equal an identical copy of itself");

        let variants: Vec<UserRecord> = vec![
            UserRecord { identifier: "op@y".to_string(), ..base.clone() },
            UserRecord { enrolled_at: 8, ..base.clone() },
            UserRecord { role: Role::Admin, ..base.clone() },
            UserRecord { pallets: vec!["other".to_string()], ..base.clone() },
            UserRecord { identity_root: "root2".to_string(), ..base.clone() },
            UserRecord { class: IdentityClass::Agent, ..base.clone() },
            UserRecord { supervisor: Some("alice".to_string()), ..base.clone() },
        ];

        for (index, variant) in variants.iter().enumerate() {
            assert_ne!(
                &base, variant,
                "field at index {} does not affect UserRecord equality", index
            );
        }
    }

    /// Finding 5 (round 1, PR #120): `read_trust_anchor` used to parse `boundary` with
    /// The last single-arm reader in this file: `boundary = 7` used to report "has no
    /// \"boundary\" array entry" while the key was plainly present, sending the reader to add a
    /// field that already exists — the exact misdirection `read_string`'s own doc comment cites as
    /// the reason its arms were split. This pins the present-but-wrong-type arm; the absent arm is
    /// pinned by the parse of a trust file written without the key.
    #[test]
    fn a_present_but_non_array_boundary_is_distinguished_from_an_absent_one() {
        let doc: DocumentMut = "boundary = 7\n".parse().unwrap();

        let error = match read_required_string_array(&doc, "boundary", "trust file") {
            Err(error) => error,
            Ok(value) => panic!("a present, non-array boundary must error, not read as {:?}", value),
        };

        assert!(error.contains("present but is not an array"),
            "the error must say the key is present and wrongly typed, not that it is missing: {}",
            error
        );
        assert!(error.contains("boundary"), "the error must name the field: {}", error);

        // The absent arm still reports absence, and still says so in the old words.
        let empty: DocumentMut = "".parse().unwrap();
        let absent = read_required_string_array(&empty, "boundary", "trust file").unwrap_err();

        assert!(absent.contains("has no"),
            "an absent required array must still report absence: {}", absent);
    }

    /// `filter_map(|e| e.as_str())`, silently dropping a non-string entry instead of erroring —
    /// under-counting roots on a damaged trust file and letting `gc` collect a pinned head, the
    /// opposite of what `collect_trust_pin_roots`'s own doc comment claims. `boundary` is a
    /// required, always-serialized field (`encode_trust_anchor` writes it even when empty),
    /// so a malformed entry must fail loudly rather than be quietly dropped.
    #[test]
    fn a_non_string_boundary_entry_in_the_trust_file_errors_naming_it() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-office-utils-malformed-boundary-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let path = forklift_root().join(FILE_NAME_TRUST);
        std::fs::write(&path,
            "genesis = \"genesis-hash\"\n\
             enabled_at = 1\n\
             boundary = [\"a-real-hash\", 7]\n"
        ).unwrap();

        let error = match read_trust_anchor() {
            Err(e) => e,
            Ok(_) => panic!("a non-string boundary entry must error, not be silently dropped"),
        };
        assert!(error.contains("\"boundary\""), "the error must name the field: {}", error);
        assert!(error.contains("index 1"), "the error must name the offending index: {}", error);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The `distrust_boundary` twin of the test above — same lenience, same fix. Unlike
    /// `boundary`, this field is legitimately *absent* for every active (never-revoked) key
    /// (`key_record_to_toml` omits it), so only a present-but-malformed entry must error — see
    /// the sibling test below for the missing-key case staying valid.
    #[test]
    fn a_non_string_distrust_boundary_entry_in_a_key_record_errors_naming_it() {
        let toml = "operator = \"op@x\"\n\
            public_key = \"ab\"\n\
            issued_at = 1\n\
            distrust_boundary = [\"a-real-hash\", 42]\n\
            authorized_by = \"k1\"\n\
            endorsement = \"ee\"\n\
            proof_of_possession = \"pp\"\n";

        let error = match parse_key_record(toml) {
            Err(e) => e,
            Ok(_) => panic!("a non-string distrust_boundary entry must error, not be silently dropped"),
        };
        assert!(error.contains("\"distrust_boundary\""), "the error must name the field: {}", error);
        assert!(error.contains("index 1"), "the error must name the offending index: {}", error);
    }

    /// The other half of `distrust_boundary`'s strictness: unlike `boundary` (always written),
    /// `key_record_to_toml` omits `distrust_boundary` entirely for an active key — the ordinary,
    /// overwhelmingly common shape — so a record with no such key must still parse, as an empty
    /// boundary, never as an error. Guards against over-correcting the lenience fix above into
    /// breaking every active key's record.
    #[test]
    fn a_key_record_with_no_distrust_boundary_key_parses_as_empty() {
        let toml = "operator = \"op@x\"\n\
            public_key = \"ab\"\n\
            issued_at = 1\n\
            authorized_by = \"k1\"\n\
            endorsement = \"ee\"\n\
            proof_of_possession = \"pp\"\n";

        let parsed = parse_key_record(toml)
            .expect("a record with no distrust_boundary key must still parse");
        assert!(parsed.distrust_boundary.is_empty());
    }

    /// A minimal, otherwise-valid user record TOML string with the given `pallets` clause (or
    /// none, if `pallets_clause` is empty) spliced in — the shared fixture for the tests below.
    fn user_record_toml(pallets_clause: &str) -> String {
        format!(
            "identifier = \"op@x\"\n\
             enrolled_at = 1\n\
             role = \"writer\"\n\
             identity_root = \"r\"\n\
             {}",
            pallets_clause
        )
    }

    /// FORK-81 follow-up (the finding this commit fixes): `may_write_pallet` reads an empty
    /// `pallets` list as "may write ANY pallet" for a `Writer` — the previous parse
    /// (`.and_then(as_array).unwrap_or_default()`) silently turned a `pallets` value that is
    /// present but not an array into that same empty list, fail-OPEN on an authorization check.
    /// A present non-array value must error instead.
    #[test]
    fn a_present_non_array_pallets_entry_in_a_user_record_errors_naming_it() {
        let toml = user_record_toml("pallets = \"main\"\n");

        let error = match parse_user_record(&toml) {
            Err(e) => e,
            Ok(_) => panic!("a non-array pallets value must error, not silently become empty"),
        };
        assert!(error.contains("\"pallets\""), "the error must name the field: {}", error);
    }

    /// The other half of the same fail-open hazard: the previous parse's `filter_map(|e| e.
    /// as_str())` silently dropped a non-string entry rather than erroring, which would have
    /// narrowed (not widened) `pallets` — still a silent misread of what was actually granted,
    /// and inconsistent with every other strict array field in this file.
    #[test]
    fn a_non_string_pallets_entry_in_a_user_record_errors_naming_it() {
        let toml = user_record_toml("pallets = [\"main\", 7]\n");

        let error = match parse_user_record(&toml) {
            Err(e) => e,
            Ok(_) => panic!("a non-string pallets entry must error, not be silently dropped"),
        };
        assert!(error.contains("\"pallets\""), "the error must name the field: {}", error);
        assert!(error.contains("index 1"), "the error must name the offending index: {}", error);
    }

    /// The absent-key case must stay exactly as lenient as before: `user_record_to_toml` omits
    /// `pallets` entirely for an unrestricted writer, so a record with no such key must still
    /// parse as an empty list — and `may_write_pallet` must still read that empty list as
    /// "may write any pallet" (the behaviour the strictness fix above must NOT change).
    #[test]
    fn a_user_record_with_no_pallets_key_parses_as_empty_and_stays_unrestricted() {
        let toml = user_record_toml("");

        let parsed = parse_user_record(&toml)
            .expect("a record with no pallets key must still parse");
        assert!(parsed.pallets.is_empty());
        assert!(
            parsed.may_write_pallet("some-arbitrary-pallet"),
            "an empty pallets list must still mean unrestricted write"
        );
    }

    /// `class` gets the same absent-vs-malformed split as `pallets`: absent legitimately
    /// defaults to `Human` (covered by `a_record_without_a_class_defaults_to_human` above), but
    /// a present non-string value must error rather than silently collapse to that same default
    /// — `IdentityClass` gates admission rules and audit/query classification, so misreading a
    /// damaged automated identity as human would hide the provenance distinction it exists for.
    #[test]
    fn a_present_non_string_class_in_a_user_record_errors_naming_it() {
        let toml = "identifier = \"op@x\"\nenrolled_at = 1\nrole = \"writer\"\nidentity_root = \"r\"\nclass = 7\n";

        let error = match parse_user_record(toml) {
            Err(e) => e,
            Ok(_) => panic!("a non-string class value must error, not silently default to human"),
        };
        assert!(error.contains("\"class\""), "the error must name the field: {}", error);
    }

    /// `supervisor`'s malformed-vs-absent split: absent legitimately means "no supervisor"
    /// (every human, and any unsupervised bot/service), but a present non-string value must
    /// error rather than silently read as unsupervised — losing the one accountability field an
    /// automated identity's record carries.
    #[test]
    fn a_present_non_string_supervisor_in_a_user_record_errors_naming_it() {
        let toml = "identifier = \"op@x\"\nenrolled_at = 1\nrole = \"writer\"\nidentity_root = \"r\"\nsupervisor = 7\n";

        let error = match parse_user_record(toml) {
            Err(e) => e,
            Ok(_) => panic!("a non-string supervisor value must error, not silently become none"),
        };
        assert!(error.contains("\"supervisor\""), "the error must name the field: {}", error);
    }

    /// `revocation_reason`'s malformed-vs-absent split, key-record side: absent legitimately
    /// means "not revoked" (the common, active-key shape), but a present non-string value must
    /// error rather than fall through the same `None` arm — `verify_key_permanence`
    /// (audit_utils) treats a revoked key with no reason as invalid, so silently losing a
    /// present-but-damaged reason would turn a legitimate revocation into a spurious rejection.
    #[test]
    fn a_present_non_string_revocation_reason_in_a_key_record_errors_naming_it() {
        let toml = "operator = \"op@x\"\n\
            public_key = \"ab\"\n\
            issued_at = 1\n\
            revocation_reason = 7\n\
            authorized_by = \"k1\"\n\
            endorsement = \"ee\"\n\
            proof_of_possession = \"pp\"\n";

        let error = match parse_key_record(toml) {
            Err(e) => e,
            Ok(_) => panic!("a non-string revocation_reason value must error, not silently become none"),
        };
        assert!(error.contains("\"revocation_reason\""), "the error must name the field: {}", error);
    }

    /// `retired_at`'s malformed-vs-absent split: absent legitimately means "active" (the common
    /// shape; `key_record_to_toml` omits it), but a present non-integer value must error rather
    /// than silently read as active.
    #[test]
    fn a_present_non_integer_retired_at_in_a_key_record_errors_naming_it() {
        let toml = "operator = \"op@x\"\n\
            public_key = \"ab\"\n\
            issued_at = 1\n\
            retired_at = \"soon\"\n\
            authorized_by = \"k1\"\n\
            endorsement = \"ee\"\n\
            proof_of_possession = \"pp\"\n";

        let error = match parse_key_record(toml) {
            Err(e) => e,
            Ok(_) => panic!("a non-integer retired_at value must error, not silently become active"),
        };
        assert!(error.contains("\"retired_at\""), "the error must name the field: {}", error);
    }

    /// `prior_genesis`/`adopts` get the same split as `pallets`, trust-file side: absent
    /// legitimately means "original enrollment, no re-genesis" (the common shape), but a
    /// present non-string value must error rather than silently collapse to that same `None` —
    /// `collect_trust_pin_roots` treats a missing `adopts` as "nothing to pin", so a damaged
    /// present value must not be misread as the ordinary absent case.
    #[test]
    fn a_present_non_string_adopts_in_the_trust_file_errors_naming_it() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-office-utils-malformed-adopts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let path = forklift_root().join(FILE_NAME_TRUST);
        std::fs::write(&path,
            "genesis = \"genesis-hash\"\n\
             enabled_at = 1\n\
             boundary = []\n\
             adopts = 7\n"
        ).unwrap();

        let error = match read_trust_anchor() {
            Err(e) => e,
            Ok(_) => panic!("a non-string adopts value must error, not silently become none"),
        };
        assert!(error.contains("\"adopts\""), "the error must name the field: {}", error);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// PR #122 round 4 finding F2: `read_string` still said "has no ... entry" for BOTH an
    /// absent key and a present, wrong-typed one — the exact conflation `read_integer` was
    /// given a two-arm shape to fix — so a hand-edited `genesis = 7` reported "has no
    /// \"genesis\" entry" while the key is plainly there, misdirecting the fix. It must
    /// distinguish the two the same way `read_integer` does.
    #[test]
    fn read_string_reports_an_absent_key_as_missing() {
        let doc: DocumentMut = "other = \"x\"\n".parse().unwrap();

        let error = match read_string(&doc, "genesis", "trust file") {
            Err(e) => e,
            Ok(v) => panic!("an absent key must error, not read as {:?}", v),
        };
        assert!(error.contains("has no"), "must report absence, not type: {}", error);
        assert!(error.contains("\"genesis\""), "the error must name the field: {}", error);
    }

    #[test]
    fn read_string_reports_a_present_non_string_as_wrongly_typed_not_missing() {
        let doc: DocumentMut = "genesis = 7\n".parse().unwrap();

        let error = match read_string(&doc, "genesis", "trust file") {
            Err(e) => e,
            Ok(v) => panic!("a present, non-string value must error, not read as {:?}", v),
        };
        assert!(
            !error.contains("has no"),
            "a present-but-wrong-type value must not be reported as absent: {}", error
        );
        assert!(error.contains("is present but is not a string"), "unexpected error: {}", error);
        assert!(error.contains("\"genesis\""), "the error must name the field: {}", error);
    }

    /// `read_integer` used to say "has no ... entry" for BOTH an absent key and a present,
    /// wrong-typed one — so a hand-edited, quoted `tagged_at = "1700000000"` was reported as
    /// missing, misdirecting the fix. It must distinguish the two the same way
    /// `read_optional_integer` already does.
    #[test]
    fn read_integer_reports_an_absent_key_as_missing() {
        let doc: DocumentMut = "other = 1\n".parse().unwrap();

        let error = match read_integer(&doc, "tagged_at", "tag record") {
            Err(e) => e,
            Ok(v) => panic!("an absent key must error, not read as {:?}", v),
        };
        assert!(error.contains("has no"), "must report absence, not type: {}", error);
        assert!(error.contains("\"tagged_at\""), "the error must name the field: {}", error);
    }

    #[test]
    fn read_integer_reports_a_present_non_integer_as_wrongly_typed_not_missing() {
        let doc: DocumentMut = "tagged_at = \"1700000000\"\n".parse().unwrap();

        let error = match read_integer(&doc, "tagged_at", "tag record") {
            Err(e) => e,
            Ok(v) => panic!("a present, non-integer value must error, not read as {:?}", v),
        };
        assert!(
            !error.contains("has no"),
            "a present-but-wrong-type value must not be reported as absent: {}", error
        );
        assert!(error.contains("is present but is not an integer"), "unexpected error: {}", error);
        assert!(error.contains("\"tagged_at\""), "the error must name the field: {}", error);
    }

    /// Finding F7 (round 3): the trust file's `genesis`/`enabled_at` used to be hand-rolled
    /// required reads with the same `and_then(...).ok_or(...)` shape the four retired sites
    /// used, directly under a comment insisting the parse must be strict. They now route
    /// through the shared `read_string`/`read_integer`, and the error text still names the
    /// trust file (not a generic "record").
    #[test]
    fn a_present_non_integer_enabled_at_in_the_trust_file_errors_naming_it() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-office-utils-malformed-enabled-at-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let path = forklift_root().join(FILE_NAME_TRUST);
        std::fs::write(&path,
            "genesis = \"genesis-hash\"\n\
             enabled_at = \"soon\"\n\
             boundary = []\n"
        ).unwrap();

        let error = match read_trust_anchor() {
            Err(e) => e,
            Ok(_) => panic!("a non-integer enabled_at value must error, not silently drop the record"),
        };
        assert!(error.contains("trust file"), "the error must name the record kind: {}", error);
        assert!(error.contains("\"enabled_at\""), "the error must name the field: {}", error);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// PR #122 round 4 finding F5: routing `genesis`/`enabled_at` through the shared reader
    /// changed `The trust file has no "genesis" entry.` to `A trust file has no ...` — two
    /// lines below the surviving, hardcoded `The trust file is not valid TOML`. There is
    /// exactly one trust file per warehouse, so it takes the definite article like every other
    /// message about it; a record kind that recurs across a warehouse (a user record, a key
    /// record, ...) keeps the indefinite one.
    #[test]
    fn trust_file_errors_take_the_definite_article_other_record_kinds_the_indefinite() {
        let doc: DocumentMut = "other = \"x\"\n".parse().unwrap();

        let trust_file_error = read_string(&doc, "genesis", "trust file").unwrap_err();
        assert!(
            trust_file_error.starts_with("The trust file has no"),
            "the trust file is unique per warehouse and must take \"The\": {}", trust_file_error
        );

        let user_record_error = read_string(&doc, "identifier", "user record").unwrap_err();
        assert!(
            user_record_error.starts_with("A user record has no"),
            "a user record recurs across a warehouse and must keep \"A\": {}", user_record_error
        );

        let key_record_error = read_string(&doc, "public_key", "key record").unwrap_err();
        assert!(
            key_record_error.starts_with("A key record has no"),
            "a key record recurs across a warehouse and must keep \"A\": {}", key_record_error
        );
    }

    /// PR #122 round 5 finding F6: hardcoding "A" for everything but the literal "trust
    /// file" was correct only by coincidence for today's seven record kinds. A future
    /// vowel-initial kind must take "An", and a future unique-per-warehouse kind must be
    /// added to the explicit list rather than falling through to the indefinite article.
    #[test]
    fn record_kind_prefix_picks_the_article_from_the_first_letter_and_an_explicit_uniqueness_list() {
        assert_eq!(record_kind_prefix("office record"), "An office record");
        assert_eq!(record_kind_prefix("audit entry"), "An audit entry");

        // "user record" starts with a vowel *letter* but a consonant *sound* ("yoo-zer") —
        // English picks the article by sound, so a bare first-letter check would get this
        // wrong; it must stay "A", not flip to "An".
        assert_eq!(record_kind_prefix("user record"), "A user record");

        // The explicit uniqueness list decides the definite article, not the first letter.
        assert_eq!(record_kind_prefix("trust file"), "The trust file");

        // Ordinary consonant-initial kinds are unaffected.
        assert_eq!(record_kind_prefix("user record"), "A user record");
        assert_eq!(record_kind_prefix("tag record"), "A tag record");
        assert_eq!(record_kind_prefix("key record"), "A key record");
        assert_eq!(record_kind_prefix("manifest entry"), "A manifest entry");
        assert_eq!(record_kind_prefix("delivery entry"), "A delivery entry");
    }

    // -------------------------------------------------------------------------------
    // Key ids derived from public keys; records filed under their subjects (FORK-74)
    // -------------------------------------------------------------------------------

    /// A structurally valid key record for `public_key`, with whatever extra lines `extra`
    /// adds (a stale `key_id` field, a distrust boundary).
    fn key_record_for(public_key: &str, extra: &str) -> String {
        format!(
            "{}operator = \"op@x\"\npublic_key = \"{}\"\nissued_at = 1\n\
             authorized_by = \"k1\"\nendorsement = \"ee\"\nproof_of_possession = \"pp\"\n",
            extra, public_key
        )
    }

    /// A public key (hex) and the id it hashes to.
    fn bound_key(byte: u8) -> (String, String) {
        let bytes = [byte; 32];
        (sign_utils::to_hex(&bytes), sign_utils::key_id_for_public_key(&bytes))
    }

    /// A key record's id is the hash of its public key, never read from the record, so no
    /// record can put one key's material under another key's id.
    #[test]
    fn a_key_records_id_is_derived_from_its_public_key() {
        let (public_key, own_id) = bound_key(7);

        let parsed = parse_key_record(&key_record_for(&public_key, "")).unwrap();
        assert_eq!(parsed.key_id, own_id);
    }

    /// A record that still states a `key_id` (the shape every record had before ids were
    /// derived) is refused naming the entry, whether the id it states is its own or another's:
    /// a claim the reader no longer reads is never dropped silently.
    #[test]
    fn a_key_record_that_states_a_key_id_is_refused_naming_the_entry() {
        let (public_key, own_id) = bound_key(7);
        let (_, other_id) = bound_key(9);

        for stated in [own_id, other_id] {
            let error = match parse_key_record(&key_record_for(&public_key, &format!("key_id = \"{}\"\n", stated))) {
                Err(e) => e,
                Ok(_) => panic!("a key record stating key_id {} must not parse", stated),
            };
            assert!(error.contains("\"key_id\""), "the error must name the entry: {}", error);
        }
    }

    /// A public key that is not hex has no id, so its record does not parse.
    #[test]
    fn a_key_record_whose_public_key_is_not_hex_errors_naming_it() {
        let error = match parse_key_record(&key_record_for("not-hex", "")) {
            Err(e) => e,
            Ok(_) => panic!("a key record with a non-hex public key must not parse"),
        };
        assert!(error.contains("\"not-hex\""), "the error must quote the public key: {}", error);

        let long = "z".repeat(500);
        let error = parse_key_record(&key_record_for(&long, "")).err().unwrap();
        assert!(error.len() < 200 && error.contains('…'), "a long value is cut short: {}", error);
    }

    /// `key_record_to_toml` writes no `key_id`, and what it writes parses back to the same key.
    #[test]
    fn an_emitted_key_record_stores_no_id_and_round_trips() {
        let (public_key, key_id) = bound_key(7);
        let record = KeyRecord {
            key_id: key_id.clone(),
            operator: "op@x".to_string(),
            public_key: public_key.clone(),
            issued_at: 1,
            retired_at: None,
            revocation_reason: None,
            distrust_boundary: Vec::new(),
            authorized_by: key_id.clone(),
            endorsement: "ee".to_string(),
            proof_of_possession: "pp".to_string(),
        };

        let toml = key_record_to_toml(&record);
        assert!(!toml.parse::<DocumentMut>().unwrap().contains_key("key_id"), "no key_id is stored: {}", toml);

        let parsed = parse_key_record(&toml).unwrap();
        assert_eq!(parsed.key_id, key_id);
        assert_eq!(parsed.public_key, public_key);
    }

    /// Each user and key record of `state`, as a `(file name, record)` pair filed under the
    /// id or identifier the record holds in memory — unchecked, unlike
    /// [`build_and_sign_office_parcel`], so a test can file a record where no writer would.
    pub(crate) fn naive_record_entries(state: &OfficeState) -> (Vec<(String, String)>, Vec<(String, String)>) {
        let users = state.users.iter()
            .map(|user| (user_record_file_name(user), user_record_to_toml(user)))
            .collect();
        let keys = state.keys.iter()
            .map(|key| (key_record_file_name(key), key_record_to_toml(key)))
            .collect();

        (users, keys)
    }

    /// Store an office parcel whose `users` and `keys` trees hold exactly the given
    /// `(file name, record)` pairs, sign it with `signer` if given, make it the office head,
    /// and return its hash. File names are free here, unlike in
    /// [`build_and_sign_office_parcel`], which is what lets a test misfile a record.
    pub(crate) fn plant_office(users: &[(String, String)],
                               keys: &[(String, String)],
                               parents: Vec<String>,
                               signer: Option<&str>) -> String {
        let mut users_tree = TreeItem::new(TREE_NAME_USERS.to_string(), String::new(), DirEntryType::Tree);
        for (name, toml) in users {
            users_tree.add_child(TreeItem::new(name.to_string(), store_record(toml).unwrap(), DirEntryType::Normal));
        }

        let mut keys_tree = TreeItem::new(TREE_NAME_KEYS.to_string(), String::new(), DirEntryType::Tree);
        for (name, toml) in keys {
            keys_tree.add_child(TreeItem::new(name.to_string(), store_record(toml).unwrap(), DirEntryType::Normal));
        }

        store_subtree(&mut users_tree).unwrap();
        store_subtree(&mut keys_tree).unwrap();

        let mut tracked_tree = TreeItem::new(TREE_NAME_TRACKED.to_string(), String::new(), DirEntryType::Tree);
        tracked_tree.add_child(users_tree);
        tracked_tree.add_child(keys_tree);

        plant_tracked(tracked_tree, parents, signer)
    }

    /// Plant an office parcel over a hand-built `tracked` tree, for shapes no writer produces.
    fn plant_tracked(mut tracked_tree: TreeItem, parents: Vec<String>, signer: Option<&str>) -> String {
        store_subtree(&mut tracked_tree).unwrap();

        let mut forklift_tree = TreeItem::new(TREE_NAME_FORKLIFT.to_string(), String::new(), DirEntryType::Tree);
        forklift_tree.add_child(tracked_tree);
        store_subtree(&mut forklift_tree).unwrap();

        let mut root_tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        root_tree.add_child(forklift_tree);
        plant_root(root_tree, parents, signer)
    }

    /// Plant an office parcel over a hand-built root tree whose subtrees are already stored.
    fn plant_root(root_tree: TreeItem, parents: Vec<String>, signer: Option<&str>) -> String {
        let mut root_object = LooseObjectBuilder::build_tree(&root_tree);
        root_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: root_object.hash, parents, actions: Vec::new(),
            description: Some("planted office".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();

        if let Some(signer) = signer {
            let signature = sign_utils::sign_parcel_hash(signer, &parcel_object.hash).unwrap();
            sign_utils::store_parcel_signature(&parcel_object.hash, &signature).unwrap();
        }

        let head = pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME).unwrap();
        pallet_utils::update_meta_pallet_head(OFFICE_PALLET_NAME, head.as_deref(), &parcel_object.hash, &pallet_utils::RefPreconditions::NONE).unwrap();

        parcel_object.hash
    }

    fn enter_scratch_warehouse(name: &str) -> (crate::globals::StorageRootScope, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("forklift-office-utils-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let scope = crate::globals::StorageRootScope::enter(&dir);
        crate::util::warehouse_utils::prepare_warehouse(crate::util::warehouse_utils::PrepareMode::Full).unwrap();
        (scope, dir)
    }

    fn entry(name: &str, toml: String) -> (String, String) {
        (name.to_string(), toml)
    }

    /// A key record filed under any name but its own id is refused, naming the parcel, the
    /// file and the file it belongs in. This is also what refuses two records for one key —
    /// tree entry names are unique, so the second must sit under a name not its own — which
    /// matters because a second record for a known key is never re-endorsed, and `find_key`
    /// returns whichever sorts first: such a shadow could present a revoked key as active.
    #[test]
    fn a_key_record_filed_under_another_name_is_refused() {
        let (scope, dir) = enter_scratch_warehouse("misfiled-key");
        let (public_key, key_id) = bound_key(7);
        let (_, other_id) = bound_key(9);
        let own_file = format!("{}.toml", key_id);

        for misfiled in [format!("{}.toml", other_id), "a.toml".to_string()] {
            for keys in [
                vec![entry(&misfiled, key_record_for(&public_key, ""))],
                vec![entry(&own_file, key_record_for(&public_key, "")), entry(&misfiled, key_record_for(&public_key, ""))],
            ] {
                let planted = plant_office(&[], &keys, Vec::new(), None);

                let error = match read_office_state_of(&planted) {
                    Err(e) => e,
                    Ok(_) => panic!("a key record filed as {} must not read", misfiled),
                };
                assert!(error.contains(&planted), "the error must name the parcel: {}", error);
                assert!(error.contains(&format!("keys/{}", misfiled)), "the error must name the file: {}", error);
                assert!(error.contains(&format!("keys/{}", own_file)),
                    "the error must name the file the record belongs in: {}", error);
            }
        }

        // The domain was non-empty: the same record under its own name reads.
        let planted = plant_office(&[], &[entry(&own_file, key_record_for(&public_key, ""))], Vec::new(), None);
        assert_eq!(read_office_state_of(&planted).unwrap().keys[0].key_id, key_id);

        drop(scope);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The same for user records: filed under any name but the hash of the identifier, refused.
    #[test]
    fn a_user_record_filed_under_another_name_is_refused() {
        let (scope, dir) = enter_scratch_warehouse("misfiled-user");
        let record = user(IdentityClass::Human, None);
        let own_file = user_record_file_name(&record);

        let planted = plant_office(&[
            entry(&own_file, user_record_to_toml(&record)), entry("u2.toml", user_record_to_toml(&record)),
        ], &[], Vec::new(), None);
        let error = match read_office_state_of(&planted) {
            Err(e) => e,
            Ok(_) => panic!("a user record filed as u2.toml must not read"),
        };
        assert!(error.contains("users/u2.toml") && error.contains(&format!("users/{}", own_file)),
            "the error must name the file and the file it belongs in: {}", error);
        assert!(error.contains("op@x"), "the error must name the user: {}", error);

        let planted = plant_office(&[entry(&own_file, user_record_to_toml(&record))], &[], Vec::new(), None);
        assert_eq!(read_office_state_of(&planted).unwrap().users.len(), 1);

        drop(scope);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The trust-pin reader, which only gathers pins that keep history alive, keeps a misfiled
    /// record's pins instead of refusing the office: the union of every record's pins is the
    /// safe side for collecting roots.
    #[test]
    fn the_trust_pin_reader_keeps_a_misfiled_records_pins() {
        let (scope, dir) = enter_scratch_warehouse("misfiled-pins");
        let (public_key, _) = bound_key(7);

        plant_office(&[], &[
            entry("a.toml", key_record_for(&public_key, "distrust_boundary = [\"pin-a\"]\n")),
            entry("b.toml", key_record_for(&public_key, "distrust_boundary = [\"pin-b\"]\n")),
        ], Vec::new(), None);

        let keys = read_trust_pin_keys().ok().expect("the trust-pin reader does not judge file names");
        let pins: Vec<&str> = keys.iter().flat_map(|key| key.distrust_boundary.iter().map(String::as_str)).collect();
        assert!(pins.contains(&"pin-a") && pins.contains(&"pin-b"), "both records' pins are kept: {:?}", pins);

        drop(scope);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The writer refuses, before signing, a state no reader would accept: a key whose id is
    /// not its public key's, and two records for one key or one user (which would otherwise
    /// collapse into one file, silently dropping one).
    #[test]
    fn the_office_writer_refuses_a_state_no_reader_would_accept() {
        let (scope, dir) = enter_scratch_warehouse("writer-refuses");
        let actor = crate::model::operator::Operator { identifier: "op@x".to_string() };
        let (public_key, key_id) = bound_key(7);
        let (_, other_id) = bound_key(9);
        let key = parse_key_record(&key_record_for(&public_key, "")).unwrap();

        fn files_under(path: &std::path::Path) -> usize {
            std::fs::read_dir(path).unwrap().map(|entry| {
                let path = entry.unwrap().path();
                if path.is_dir() { files_under(&path) } else { 1 }
            }).sum()
        }
        let stored = || files_under(&dir);
        let before = stored();

        let mut rebound = key.clone();
        rebound.key_id = other_id.clone();
        let error = build_and_sign_office_parcel(
            &OfficeState { users: Vec::new(), keys: vec![rebound] }, &actor, "x".to_string(), &key_id, Vec::new(),
        ).err().expect("a key whose id is not its public key's must not be written");
        assert!(error.contains(&other_id) && error.contains(&key_id), "the error must name both ids: {}", error);

        let error = build_and_sign_office_parcel(
            &OfficeState { users: Vec::new(), keys: vec![key.clone(), key.clone()] }, &actor, "x".to_string(), &key_id, Vec::new(),
        ).err().expect("two records for one key must not be written");
        assert!(error.contains("two records") && error.contains(&key_id), "{}", error);

        let user = user(IdentityClass::Human, None);
        let error = build_and_sign_office_parcel(
            &OfficeState { users: vec![user.clone(), user], keys: vec![key.clone()] }, &actor, "x".to_string(), &key_id, Vec::new(),
        ).err().expect("two records for one user must not be written");
        assert!(error.contains("two records") && error.contains("op@x"), "{}", error);

        // Each state was refused before any record was stored, not only before signing.
        assert_eq!(stored(), before, "a refused state must leave nothing in the store");

        drop(scope);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A folder inside `keys/` or `users/` is refused naming it, rather than skipped: a record
    /// nested there would otherwise silently vanish from the office, which on the client path
    /// amounts to removing a key.
    #[test]
    fn a_folder_nested_in_a_record_tree_is_refused() {
        let (scope, dir) = enter_scratch_warehouse("nested-folder");
        let (public_key, key_id) = bound_key(7);
        let own_file = format!("{}.toml", key_id);

        for tree_name in [TREE_NAME_KEYS, TREE_NAME_USERS] {
            let mut nested = TreeItem::new("deeper".to_string(), String::new(), DirEntryType::Tree);
            nested.add_child(TreeItem::new(
                own_file.clone(), store_record(&key_record_for(&public_key, "")).unwrap(), DirEntryType::Normal,
            ));
            store_subtree(&mut nested).unwrap();

            let mut tracked = TreeItem::new(TREE_NAME_TRACKED.to_string(), String::new(), DirEntryType::Tree);
            for name in [TREE_NAME_KEYS, TREE_NAME_USERS] {
                let mut records = TreeItem::new(name.to_string(), String::new(), DirEntryType::Tree);
                if name == tree_name {
                    records.add_child(nested.clone());
                }
                store_subtree(&mut records).unwrap();
                tracked.add_child(records);
            }
            store_subtree(&mut tracked).unwrap();

            let mut forklift = TreeItem::new(TREE_NAME_FORKLIFT.to_string(), String::new(), DirEntryType::Tree);
            forklift.add_child(tracked);
            store_subtree(&mut forklift).unwrap();
            let mut root = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
            root.add_child(forklift);
            let mut root_object = LooseObjectBuilder::build_tree(&root);
            root_object.store().unwrap();
            let mut parcel = LooseObjectBuilder::build_parcel(&Parcel {
                tree_hash: root_object.hash, parents: Vec::new(), actions: Vec::new(), description: None,
            });
            parcel.store().unwrap();

            let error = match read_office_state_of(&parcel.hash) {
                Err(e) => e,
                Ok(_) => panic!("a folder nested in {}/ must not read", tree_name),
            };
            assert!(error.contains(&format!("{}/deeper", tree_name)), "the error must name the folder: {}", error);
        }

        // The domain was non-empty: the same record directly in keys/ reads.
        let planted = plant_office(&[], &[entry(&own_file, key_record_for(&public_key, ""))], Vec::new(), None);
        assert_eq!(read_office_state_of(&planted).unwrap().keys.len(), 1);

        drop(scope);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A record that does not parse is named by its parcel and file.
    #[test]
    fn an_unreadable_record_is_named_by_parcel_and_file() {
        let (scope, dir) = enter_scratch_warehouse("unreadable-record");

        let planted = plant_office(&[], &[entry("x.toml", "not = [toml".to_string())], Vec::new(), None);
        let error = read_office_state_of(&planted).err().expect("an unparseable record must not read");
        assert!(error.contains(&planted) && error.contains("keys/x.toml") && error.contains("does not parse"),
            "the error must name the parcel and the file: {}", error);

        drop(scope);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A `.forklift`, `tracked`, `keys` or `users` entry stored as a file rather than a folder is
    /// refused, rather than read as an office with no such records — on the client path that
    /// would remove every key. The trust-pin reader refuses a file on its way to `keys` too, since
    /// it would otherwise drop every pin.
    #[test]
    fn a_record_tree_stored_as_a_file_is_refused() {
        let (scope, dir) = enter_scratch_warehouse("record-tree-file");
        let (public_key, key_id) = bound_key(7);
        let record = key_record_for(&public_key, "distrust_boundary = [\"pin-a\"]\n");

        let shapes: [&[&str]; 4] = [
            &[TREE_NAME_FORKLIFT],
            &[TREE_NAME_FORKLIFT, TREE_NAME_TRACKED],
            &[TREE_NAME_FORKLIFT, TREE_NAME_TRACKED, TREE_NAME_KEYS],
            &[TREE_NAME_FORKLIFT, TREE_NAME_TRACKED, TREE_NAME_USERS],
        ];
        for path in shapes {
            // A file at the end of `path`, inside folders for every component before it.
            let (file_name, folders) = path.split_last().unwrap();
            let mut item = TreeItem::new(file_name.to_string(), store_record(&record).unwrap(), DirEntryType::Normal);
            let mut root = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
            for folder in folders.iter().rev() {
                let mut tree = TreeItem::new(folder.to_string(), String::new(), DirEntryType::Tree);
                tree.add_child(item);
                store_subtree(&mut tree).unwrap();
                item = tree;
            }
            root.add_child(item);
            let planted = plant_root(root, Vec::new(), None);
            let shown = format!("a file {},", path.join("/"));

            let error = match read_office_state_of(&planted) {
                Err(e) => e,
                Ok(_) => panic!("a file {} must not read", path.join("/")),
            };
            assert!(error.contains(&planted) && error.contains(&shown),
                "the error must name the parcel and the file: {}", error);

            let pins = read_trust_pin_keys();
            if *file_name != TREE_NAME_USERS {
                let error = pins.err().expect("the trust-pin reader must refuse a file on the way to keys");
                assert!(error.kind == OfficeReadErrorKind::MalformedOffice && error.message.contains(&shown),
                    "the trust-pin reader must name the file: {:?}", error.message);
            } else {
                assert!(pins.map_err(|e| e.message).unwrap().is_empty(), "a users file holds no pins");
            }
        }

        // The domain was non-empty: the same record in a keys folder reads, pins included.
        plant_office(&[], &[entry(&format!("{}.toml", key_id), record)], Vec::new(), None);
        assert_eq!(read_trust_pin_keys().map_err(|e| e.message).unwrap().len(), 1);

        drop(scope);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The trust-pin reader keeps the pins of a record nested in a folder under `keys/`, which
    /// the verifying reader refuses: the union of every record's pins is the safe side.
    #[test]
    fn the_trust_pin_reader_keeps_a_nested_records_pins() {
        let (scope, dir) = enter_scratch_warehouse("nested-pins");
        let (public_key, key_id) = bound_key(7);

        let mut nested = TreeItem::new("deeper".to_string(), String::new(), DirEntryType::Tree);
        nested.add_child(TreeItem::new(
            format!("{}.toml", key_id),
            store_record(&key_record_for(&public_key, "distrust_boundary = [\"pin-nested\"]\n")).unwrap(),
            DirEntryType::Normal,
        ));
        store_subtree(&mut nested).unwrap();
        let mut keys = TreeItem::new(TREE_NAME_KEYS.to_string(), String::new(), DirEntryType::Tree);
        keys.add_child(nested);
        store_subtree(&mut keys).unwrap();
        let mut tracked = TreeItem::new(TREE_NAME_TRACKED.to_string(), String::new(), DirEntryType::Tree);
        tracked.add_child(keys);
        plant_tracked(tracked, Vec::new(), None);

        let keys = read_trust_pin_keys().map_err(|e| e.message).unwrap();
        let pins: Vec<&str> = keys.iter().flat_map(|key| key.distrust_boundary.iter().map(String::as_str)).collect();
        assert_eq!(pins, vec!["pin-nested"], "the nested record's pins are kept");

        drop(scope);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A malformed office tree is worded with the roll-back remedy when heal tolerates it, and
    /// refuses gc, which reads fail-closed: neither heal nor a record fix can change a signed tree.
    #[test]
    fn a_malformed_office_words_the_roll_back_remedy() {
        let (scope, dir) = enter_scratch_warehouse("malformed-remedy");
        write_trust_anchor(&TrustAnchor {
            genesis: "0".repeat(64), enabled_at: 0, boundary: Vec::new(), prior_genesis: None, adopts: None,
        }).unwrap();

        let mut root = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        root.add_child(TreeItem::new(
            TREE_NAME_FORKLIFT.to_string(), store_record("x = 1").unwrap(), DirEntryType::Normal,
        ));
        plant_root(root, Vec::new(), None);

        let note = collect_trust_pin_roots(TrustPinReadPolicy::Tolerate).unwrap().degraded
            .expect("a malformed office must be recorded as degraded");
        assert!(note.contains("well formed") && note.contains("a file .forklift,"),
            "the note must name the file and word the roll-back remedy: {}", note);
        assert!(collect_trust_pin_roots(TrustPinReadPolicy::FailClosed).is_err(),
            "gc must refuse a malformed office");

        drop(scope);
        std::fs::remove_dir_all(&dir).ok();
    }
}
