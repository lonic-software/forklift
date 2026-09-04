//! The warehouse office: users and keys as tracked metadata.
//!
//! Identity records live on a reserved pallet (the "office") as ordinary blobs under a
//! reserved tree namespace (`.forklift/tracked/…`) — a path normal loads can never
//! produce, because the `.forklift` folder itself is never tracked. Being ordinary
//! objects, the records inherit hashing, signing and (later) transport for free, and the
//! office pallet's parcel history *is* the audit trail of every user and key change.

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
use crate::util::{file_utils, object_utils, pallet_utils, sign_utils};

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
const FILE_NAME_TRUST: &str = "trust";

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
    /// own rules — see `verify_office_privileges`).
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
    /// The key id: the Blake3 hex hash of the raw public key bytes.
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
            come from the same \"office keygen\" run.",
            operator
        ));
    }

    let endorsement = sign_utils::sign_message(
        authorized_by,
        &key_endorsement_payload(public_key_hex, operator, authorized_by, issued_at)
    )?;

    Ok(KeyRecord {
        key_id: sign_utils::key_id_for_public_key(
            &sign_utils::from_hex(public_key_hex)
                .map_err(|_| "The public key is not valid hex.".to_string())?
        ),
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

/// Ensure a tree is a normal content tree: office parcel trees carry a top-level
/// `.forklift` entry and must never be materialized into a working directory.
///
/// # Arguments
/// * `root_tree_hash` - The hash of the tree to check.
///
/// # Returns
/// * `Ok(())`      - If the tree carries no tracked-metadata namespace.
/// * `Err(String)` - If it does (or could not be read).
pub fn ensure_not_metadata_tree(root_tree_hash: &str) -> Result<(), String> {
    let root = object_utils::load_tree(root_tree_hash)?;

    let has_namespace = root.get_subtrees().any(|(name, _)| name == TREE_NAME_FORKLIFT)
        || root.get_files().any(|(name, _)| name == TREE_NAME_FORKLIFT);

    if has_namespace {
        return Err(
            "This parcel carries tracked metadata (an office parcel); it cannot be \
            materialized into a working directory.".to_string()
        );
    }

    Ok(())
}

/// Read the trust anchor, if signing has been established.
///
/// # Returns
/// * `Ok(Some(TrustAnchor))` - The anchor.
/// * `Ok(None)`              - If signing has not been established.
/// * `Err(String)`           - If the trust file exists but is invalid.
pub fn read_trust_anchor() -> Result<Option<TrustAnchor>, String> {
    let path = forklift_root().join(FILE_NAME_TRUST);

    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Error while reading the trust file: {}", e)),
    };

    let doc: DocumentMut = content.parse()
        .map_err(|e| format!("The trust file is not valid TOML: {}", e))?;

    let genesis = doc.get("genesis").and_then(|item| item.as_str())
        .ok_or("The trust file has no \"genesis\" entry.".to_string())?;
    let enabled_at = doc.get("enabled_at").and_then(|item| item.as_integer())
        .ok_or("The trust file has no \"enabled_at\" entry.".to_string())?;

    // Required and strict: `write_trust_anchor_file` always writes this key, even when the
    // boundary is empty (an empty array, never an omitted key) — so a trust file missing it, or
    // carrying a non-string entry, is corrupt, never merely "an old/lenient shape" to shrug off.
    // Silently dropping a non-string entry (the previous `filter_map` behavior) would under-count
    // roots and let `gc` collect a pinned head — exactly what this helper's callers rely on this
    // parse never doing (CLAUDE.md: fail loudly on anything no longer understood, never fall back
    // to a permissive default).
    let boundary = read_required_string_array(&doc, "boundary", "trust file")?;

    let field = |name: &str| doc.get(name)
        .and_then(|item| item.as_str())
        .map(|s| s.to_string());

    Ok(Some(TrustAnchor {
        genesis: genesis.to_string(),
        enabled_at,
        boundary,
        prior_genesis: field("prior_genesis"),
        adopts: field("adopts"),
    }))
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
/// pin sources do not share this property (F2, PR #120 round 2: folding `adopts` into the same
/// gap treatment as `boundary`/`distrust_boundary` silently weakened its pre-existing, main-branch
/// semantics — this warehouse's own commands set `adopts`, so its absence was always reported
/// there, and still is under this policy).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PinAbsencePolicy {
    /// This warehouse's own commands set this pin to a head it held at the moment of writing: the
    /// re-genesis `adopts` pin (`office regenesis`, office.rs) names *this warehouse's own* prior
    /// office head — present by construction on the warehouse that performed the re-genesis. Its
    /// absence is genuine local loss and is reported exactly like any ordinary ref.
    ///
    /// The one documented exception is a franchise's verbatim copy of a remote's already-
    /// re-genesis'd anchor (`remote_utils::adopt_remote_trust`'s `write_trust_anchor(&remote_trust
    /// .to_anchor())`): that franchise's own `fetch_history` call only follows the *new* chain's
    /// ancestry, never the adopted, pre-re-genesis one, so it can legitimately never hold that
    /// object. This policy still reports it there — matching `adopts`'s pre-FORK-81 (main-branch)
    /// behavior exactly, not a new regression; recovering that specific case is a distinct piece
    /// of work, not this one — tracked as FORK-114 (PR #120 round 3, F3): on such a franchise,
    /// every `forklift heal` reports the adopted head dangling and refuses to clear, forever.
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
    /// Any other read failure (a present-but-corrupt object, an I/O error) that does not fit
    /// either of the two more specific — and more actionable — kinds above.
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
/// object load — every tree along `path` is checked for presence before it is loaded, so an
/// absent one is [`OfficeReadErrorKind::AbsentObject`] by construction, never guessed from a
/// deeper loader's error text.
fn resolve_subtree_classified(root_tree_hash: &str, path: &[&str]) -> Result<Option<TreeItem>, OfficeReadError> {
    let mut current = load_object_classified(root_tree_hash, object_utils::parse_local_tree_bytes)?;

    for component in path {
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

/// The trust-pin-relevant slice of the office record: every key's own `distrust_boundary` — the
/// only thing [`collect_trust_pin_roots`] consumes from the office chain, never a user record (F5,
/// PR #120 round 2 — see [`collect_trust_pin_roots`]'s own doc comment for why that matters).
/// Walks `.forklift/tracked/keys` directly ([`resolve_subtree_classified`] returns `Ok(None)` for
/// a missing component, per its shared [`resolve_subtree`] counterpart) rather than
/// [`read_office_state`]'s full users-and-keys parse. Classified per [`OfficeReadErrorKind`] so
/// [`collect_trust_pin_roots`]'s `Tolerate` branch can word its remedy correctly (F4, same round).
fn read_trust_pin_keys() -> Result<Vec<KeyRecord>, OfficeReadError> {
    let head = pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME).map_err(OfficeReadError::other)?;
    let Some(head) = head else { return Ok(Vec::new()); };

    let parcel = load_object_classified(&head, object_utils::parse_local_parcel_bytes)?;

    let Some(keys_tree) = resolve_subtree_classified(
        &parcel.tree_hash, &[TREE_NAME_FORKLIFT, TREE_NAME_TRACKED, TREE_NAME_KEYS],
    )? else {
        return Ok(Vec::new());
    };

    let mut keys = Vec::new();

    for (_, file) in keys_tree.get_files() {
        let record = load_object_classified(&file.hash, parse_record_bytes)?;
        keys.push(parse_key_record(&record).map_err(|e| OfficeReadError {
            kind: OfficeReadErrorKind::UnparseableRecord,
            message: e,
        })?);
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
        OfficeReadErrorKind::Other =>
            "resolve the error named below, then re-run \"forklift heal\"",
    };

    format!(
        "the office record could not be read this run ({}); every revoked key's own distrust \
        boundary pin could not be established (the trust anchor's own \"adopts\"/\"boundary\" \
        pins are unaffected). {}.",
        error, remedy
    )
}

/// Write the trust anchor. This is a one-way door: once signing is established it can
/// never be disabled (admin lockout is handled by archive + re-genesis, not by an
/// unsigned escape hatch), so overwriting an existing anchor is refused.
///
/// # Arguments
/// * `anchor` - The anchor to write.
///
/// # Returns
/// * `Ok(())`      - If the anchor was written.
/// * `Err(String)` - If an anchor already exists or the write failed.
pub fn write_trust_anchor(anchor: &TrustAnchor) -> Result<(), String> {
    let path = forklift_root().join(FILE_NAME_TRUST);

    if path.exists() {
        return Err("Trust is already established for this warehouse; it cannot be re-established.".to_string());
    }

    write_trust_anchor_file(anchor)
}

/// Replace the trust anchor with a re-genesis anchor (§8.7) — the one sanctioned way
/// through the one-way door. The new anchor must name the current genesis as its
/// prior and pin an adopted office head; the caller owns the ceremony (this is the
/// mechanical check, not the authorization).
///
/// # Arguments
/// * `anchor` - The re-genesis anchor.
///
/// # Returns
/// * `Ok(())`      - If the anchor was replaced.
/// * `Err(String)` - If no anchor exists, the chain of custody does not match, or the
///                   write failed.
pub fn replace_trust_anchor(anchor: &TrustAnchor) -> Result<(), String> {
    let Some(existing) = read_trust_anchor()? else {
        return Err(
            "This warehouse has no trust anchor to replace; use \"office enroll\".".to_string()
        );
    };

    if anchor.prior_genesis.as_deref() != Some(existing.genesis.as_str()) {
        return Err(format!(
            "The new anchor does not name this warehouse's genesis ({}) as its prior; \
            the chain of custody does not match.",
            existing.genesis
        ));
    }

    if anchor.adopts.is_none() {
        return Err("A re-genesis anchor must pin the office head it adopts.".to_string());
    }

    write_trust_anchor_file(anchor)
}

/// Serialize an anchor to the trust file (no one-way-door check — callers own that).
fn write_trust_anchor_file(anchor: &TrustAnchor) -> Result<(), String> {
    let path = forklift_root().join(FILE_NAME_TRUST);

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

    file_utils::write_file_atomically(&path, doc.to_string().as_bytes())
}

/// Read the full office state from the office pallet's head. An unborn office pallet
/// yields an empty state.
///
/// # Returns
/// * `Ok(OfficeState)` - The state.
/// * `Err(String)`     - If an object or record could not be read.
pub fn read_office_state() -> Result<OfficeState, String> {
    let Some(head) = pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME)? else {
        return Ok(OfficeState { users: Vec::new(), keys: Vec::new() });
    };

    read_office_state_of(&head)
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

    let Some(tracked) = resolve_subtree(&tree_hash, &[TREE_NAME_FORKLIFT, TREE_NAME_TRACKED])? else {
        return Ok(OfficeState { users, keys });
    };

    for (name, item) in tracked.get_subtrees() {
        let subtree = object_utils::load_tree(&item.hash)?;

        match name.as_str() {
            TREE_NAME_USERS => {
                for (_, file) in subtree.get_files() {
                    users.push(parse_user_record(&load_record(&file.hash)?)?);
                }
            }
            TREE_NAME_KEYS => {
                for (_, file) in subtree.get_files() {
                    keys.push(parse_key_record(&load_record(&file.hash)?)?);
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
///
/// # Returns
/// * `Ok(String)`  - The hash of the new office parcel.
/// * `Err(String)` - If an object could not be built, stored or signed.
pub fn stack_office_parcel(state: &OfficeState,
                           actor: &Operator,
                           description: String,
                           signing_key_id: &str) -> Result<String, String> {
    let parents: Vec<String> = pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME)?
        .into_iter()
        .collect();

    stack_office_parcel_with_parents(state, actor, description, signing_key_id, parents)
}

/// Stack a parentless office genesis parcel: the root of a fresh office chain. Used by
/// enrollment (implicitly, via the unborn office pallet) and by re-genesis (§8.7),
/// where the office head exists but the new chain must not descend from it — the old
/// chain is pinned by the anchor's `adopts`, not extended.
pub fn stack_office_genesis(state: &OfficeState,
                            actor: &Operator,
                            description: String,
                            signing_key_id: &str) -> Result<String, String> {
    stack_office_parcel_with_parents(state, actor, description, signing_key_id, Vec::new())
}

/// The shared body: build the record trees, stack the parcel with the given parents,
/// sign it and advance the office head.
fn stack_office_parcel_with_parents(state: &OfficeState,
                                    actor: &Operator,
                                    description: String,
                                    signing_key_id: &str,
                                    parents: Vec<String>) -> Result<String, String> {
    let hash = build_and_sign_office_parcel(state, actor, description, signing_key_id, parents)?;

    pallet_utils::set_meta_pallet_head(OFFICE_PALLET_NAME, &hash)?;

    Ok(hash)
}

/// Build, store and sign an office parcel — everything [`stack_office_parcel_with_parents`]
/// does except advancing the office ref. A caller that must drive the ref move itself
/// (a test exercising `post_ref_update`'s own handling of a lift, rather than landing
/// one out of band) needs the exact tree shape a real office lift produces without
/// this function landing it first.
///
/// # Returns
/// * `Ok(String)`  - The hash of the new (unlinked) office parcel.
/// * `Err(String)` - If an object could not be built, stored or signed.
pub fn build_and_sign_office_parcel(state: &OfficeState,
                                    actor: &Operator,
                                    description: String,
                                    signing_key_id: &str,
                                    parents: Vec<String>) -> Result<String, String> {
    let mut users_tree = TreeItem::new(TREE_NAME_USERS.to_string(), String::new(), DirEntryType::Tree);

    for user in &state.users {
        let file_name = format!("{}{}", blake3::hash(user.identifier.as_bytes()).to_hex(), RECORD_SUFFIX);
        let blob_hash = store_record(&user_record_to_toml(user))?;

        users_tree.add_child(TreeItem::new(file_name, blob_hash, DirEntryType::Normal));
    }

    let mut keys_tree = TreeItem::new(TREE_NAME_KEYS.to_string(), String::new(), DirEntryType::Tree);

    for key in &state.keys {
        let file_name = format!("{}{}", key.key_id, RECORD_SUFFIX);
        let blob_hash = store_record(&key_record_to_toml(key))?;

        keys_tree.add_child(TreeItem::new(file_name, blob_hash, DirEntryType::Normal));
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
/// * `Err(String)`        - If a tree object could not be loaded.
fn resolve_subtree(root_tree_hash: &str, path: &[&str]) -> Result<Option<TreeItem>, String> {
    let mut current = object_utils::load_tree(root_tree_hash)?;

    for component in path {
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

    let pallets = doc.get("pallets")
        .and_then(|item| item.as_array())
        .map(|array| {
            array.iter()
                .filter_map(|entry| entry.as_str())
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();

    let class = match doc.get("class").and_then(|item| item.as_str()) {
        Some(class) => IdentityClass::parse(class)?,
        None => IdentityClass::Human,
    };

    let supervisor = doc.get("supervisor")
        .and_then(|item| item.as_str())
        .map(|s| s.to_string());

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

    doc["key_id"] = value(key.key_id.as_str());
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

    let revocation_reason = match doc.get("revocation_reason").and_then(|item| item.as_str()) {
        Some(reason) => Some(RevocationReason::parse(reason)?),
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

    Ok(KeyRecord {
        key_id: read_string(&doc, "key_id", "key record")?,
        operator: read_string(&doc, "operator", "key record")?,
        public_key: read_string(&doc, "public_key", "key record")?,
        issued_at: read_integer(&doc, "issued_at", "key record")?,
        retired_at: doc.get("retired_at").and_then(|item| item.as_integer()),
        revocation_reason,
        distrust_boundary,
        authorized_by: read_string(&doc, "authorized_by", "key record")?,
        endorsement: read_string(&doc, "endorsement", "key record")?,
        proof_of_possession: read_string(&doc, "proof_of_possession", "key record")?,
    })
}

/// Read a required string field from a TOML document.
fn read_string(doc: &DocumentMut, field: &str, record_kind: &str) -> Result<String, String> {
    doc.get(field)
        .and_then(|item| item.as_str())
        .map(|s| s.to_string())
        .ok_or(format!("A {} has no \"{}\" entry.", record_kind, field))
}

/// Read a required integer field from a TOML document.
fn read_integer(doc: &DocumentMut, field: &str, record_kind: &str) -> Result<i64, String> {
    doc.get(field)
        .and_then(|item| item.as_integer())
        .ok_or(format!("A {} has no \"{}\" entry.", record_kind, field))
}

/// Read a required array-of-hashes field, strictly: the key must be present and every entry must
/// be a string, or this errors naming the record kind, the field and (for a bad entry) its index
/// — never silently drops an entry the way a `filter_map` would. For a field the writer always
/// serializes (even when empty), a missing key is corruption, not an old/lenient shape.
fn read_required_string_array(doc: &DocumentMut, field: &str, record_kind: &str) -> Result<Vec<String>, String> {
    let array = doc.get(field)
        .and_then(|item| item.as_array())
        .ok_or_else(|| format!("A {} has no \"{}\" array entry.", record_kind, field))?;

    strict_string_array(array, field, record_kind)
}

/// Read an optional array-of-hashes field, strictly: an absent key is a valid empty list (the
/// field's normal, expected shape when the writer omits it), but a *present* key must be an
/// array of strings, or this errors the same way [`read_required_string_array`] does — an absent
/// field and a malformed one are never conflated.
fn read_optional_string_array(doc: &DocumentMut, field: &str, record_kind: &str) -> Result<Vec<String>, String> {
    let Some(item) = doc.get(field) else { return Ok(Vec::new()); };

    let array = item.as_array()
        .ok_or_else(|| format!("A {}'s \"{}\" entry is present but is not an array.", record_kind, field))?;

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
                "A {} has a non-string entry in \"{}\" at index {}.", record_kind, field, index
            )))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    /// `filter_map(|e| e.as_str())`, silently dropping a non-string entry instead of erroring —
    /// under-counting roots on a damaged trust file and letting `gc` collect a pinned head, the
    /// opposite of what `collect_trust_pin_roots`'s own doc comment claims. `boundary` is a
    /// required, always-serialized field (`write_trust_anchor_file` writes it even when empty),
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
        let toml = "key_id = \"k1\"\n\
            operator = \"op@x\"\n\
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
        let toml = "key_id = \"k1\"\n\
            operator = \"op@x\"\n\
            public_key = \"ab\"\n\
            issued_at = 1\n\
            authorized_by = \"k1\"\n\
            endorsement = \"ee\"\n\
            proof_of_possession = \"pp\"\n";

        let parsed = parse_key_record(toml)
            .expect("a record with no distrust_boundary key must still parse");
        assert!(parsed.distrust_boundary.is_empty());
    }
}
