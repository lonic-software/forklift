use std::path::{Path, PathBuf};
use crate::error::{CoreError, RefusalCode};
use crate::globals::{bay_root, forklift_root};
use crate::util::{file_utils, object_utils};
use crate::util::scope_utils::MaterializationScope;

/// The folder inside the forklift root that holds the *user* pallet ref files (branches).
/// Each pallet is a file named after the pallet, containing the hash of its head parcel
/// (followed by a new line). Pallet names may contain `/`, which maps to subfolders.
pub(crate) const FOLDER_NAME_PALLETS_ROOT: &str = "pallets";

/// The folder inside the forklift root that holds the *meta* pallet ref files: real
/// pallets (they hash, sign and transport like any other) that carry tracked metadata
/// rather than working-directory content — the office (users + keys) today, post-metadata
/// and provenance later. Kept in their own namespace so no user pallet
/// name is ever reserved (DESIGN.html §3.3). Mirrors git's `refs/heads/*` vs `refs/notes/*`.
pub(crate) const FOLDER_NAME_META_ROOT: &str = "meta";

/// The character that qualifies a meta pallet in a revision/wire reference (`@office`).
/// A bare name is always a user pallet; user names can never start with `@`
/// (see [`validate_pallet_name`]), so the qualifier is unambiguous.
pub const META_QUALIFIER: char = '@';

/// The bay-local file that holds the bay's position: its current pallet and the parcel its files
/// stand on (see [`BayPosition`]).
pub const FILE_NAME_POSITION: &str = "position";

/// The name of the default pallet (used when a warehouse is prepared).
pub const DEFAULT_PALLET_NAME: &str = "main";

/// The two pallet namespaces: user pallets (branches) and meta pallets (tracked
/// metadata). They share one ref space split into two storage folders; only the
/// namespace decides where a ref lives and how it is addressed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PalletNamespace {
    /// Working-directory branches — the pallets users create and stack on.
    User,
    /// Tracked-metadata pallets (the office, and future meta pallets).
    Meta,
}

impl PalletNamespace {
    /// The storage folder (under the forklift root) this namespace's refs live in.
    fn folder_name(self) -> &'static str {
        match self {
            PalletNamespace::User => FOLDER_NAME_PALLETS_ROOT,
            PalletNamespace::Meta => FOLDER_NAME_META_ROOT,
        }
    }
}

/// A namespaced reference to a pallet: which namespace, and the bare (validated) name.
/// This is the parsed form of a revision/wire string — `main` is `(User, "main")`,
/// `@office` is `(Meta, "office")`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PalletRef {
    pub namespace: PalletNamespace,
    pub name: String,
}

impl PalletRef {
    /// A reference to a user pallet.
    pub fn user(name: impl Into<String>) -> Self {
        PalletRef { namespace: PalletNamespace::User, name: name.into() }
    }

    /// A reference to a meta pallet.
    pub fn meta(name: impl Into<String>) -> Self {
        PalletRef { namespace: PalletNamespace::Meta, name: name.into() }
    }

    /// Parse a reference string: a leading [`META_QUALIFIER`] selects the meta
    /// namespace, anything else is a user pallet. The bare name is validated.
    ///
    /// # Arguments
    /// * `reference` - The reference string (`main`, `feature/x`, `@office`).
    ///
    /// # Returns
    /// * `Ok(PalletRef)` - The parsed, name-validated reference.
    /// * `Err(String)`   - If the bare name is not a valid pallet name.
    pub fn parse(reference: &str) -> Result<Self, String> {
        let (namespace, name) = match reference.strip_prefix(META_QUALIFIER) {
            Some(rest) => (PalletNamespace::Meta, rest),
            None => (PalletNamespace::User, reference),
        };

        validate_pallet_name(name)?;

        Ok(PalletRef { namespace, name: name.to_string() })
    }

    /// The reference string form (the wire and display form): meta pallets carry the
    /// qualifier, user pallets are bare. Round-trips with [`PalletRef::parse`].
    pub fn to_wire(&self) -> String {
        match self.namespace {
            PalletNamespace::User => self.name.clone(),
            PalletNamespace::Meta => format!("{}{}", META_QUALIFIER, self.name),
        }
    }
}

/// Get the path of a namespace's pallets folder.
///
/// # Arguments
/// * `namespace` - The pallet namespace.
///
/// # Returns
/// * `PathBuf` - The path of the folder (relative to the warehouse root).
fn get_namespace_folder(namespace: PalletNamespace) -> PathBuf {
    forklift_root().join(namespace.folder_name())
}

/// Get the path of the (user) pallets folder.
///
/// # Returns
/// * `PathBuf` - The path of the pallets folder (relative to the warehouse root).
pub fn get_pallets_folder() -> PathBuf {
    get_namespace_folder(PalletNamespace::User)
}

/// Get the path of the active bay's position file. The position is bay-local: each bay stands on
/// its own pallet and base.
fn get_position_path() -> PathBuf {
    bay_root().join(FILE_NAME_POSITION)
}

/// Get the path of the ref file of the given pallet, in the given namespace.
/// The name must already be validated (see [`validate_pallet_name`]).
///
/// # Arguments
/// * `namespace` - The pallet namespace.
/// * `name`      - The name of the pallet.
///
/// # Returns
/// * `PathBuf` - The path of the pallet's ref file (which may not exist yet).
pub(crate) fn get_pallet_ref_path(namespace: PalletNamespace, name: &str) -> PathBuf {
    let mut path = get_namespace_folder(namespace);

    for component in name.split('/') {
        path.push(component);
    }

    path
}

/// The Windows device names. A file whose stem (the text before its first `.`) is one of these
/// opens the device on Windows, whatever its extension, so no portable name may use one.
const RESERVED_DEVICE_STEMS: [&str; 22] = [
    "con", "prn", "aux", "nul",
    "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9",
    "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Check one component of a portable name: one path component that means the same file on every
/// host. It holds only lowercase ASCII letters, digits, `.`, `_` and `-`; it is not `.` or `..`;
/// it does not start with `-` or end with `.`; and its stem is not a Windows device name. Case
/// is refused rather than folded, so two names that differ as strings never share a file on a
/// case-folding volume. The caller refuses an empty component.
///
/// # Arguments
/// * `component` - The component (non-empty).
/// * `allowed`   - The character list the refusal names (the caller's separator, if any).
///
/// # Returns
/// * `None`         - If the component is portable.
/// * `Some(String)` - The reason it is not.
pub(crate) fn portable_component_problem(component: &str, allowed: &str) -> Option<String> {
    if component == "." || component == ".." {
        return Some("\".\" and \"..\" are not allowed as path components".to_string());
    }

    if component.starts_with('-') {
        return Some("path components must not start with \"-\"".to_string());
    }

    let is_valid_char = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-';

    if !component.chars().all(is_valid_char) {
        return Some(format!("only lowercase ASCII letters, digits, {} are allowed", allowed));
    }

    if component.ends_with('.') {
        return Some("path components must not end with \".\"".to_string());
    }

    let stem = component.split('.').next().unwrap_or(component);

    if RESERVED_DEVICE_STEMS.contains(&stem) {
        return Some(format!("\"{}\" is a reserved device name on Windows", stem));
    }

    None
}

/// Append the lowercase spelling of `name` to a refusal when lowercasing alone would make it
/// valid. The spelling is only suggested: a name is never folded to it.
pub(crate) fn with_lowercase_hint(reason: String, name: &str, is_valid: impl Fn(&str) -> bool) -> String {
    let lowered = name.to_ascii_lowercase();

    if lowered != name && is_valid(&lowered) {
        format!("{}; the lowercase \"{}\" is valid", reason, lowered)
    } else {
        reason
    }
}

/// Validate a pallet name.
///
/// Names are `/`-separated components, each a portable component (see
/// [`portable_component_problem`]): lowercase ASCII letters, digits, `.`, `_` and `-`, not `.`
/// or `..`, not starting with `-` (such a name would be indistinguishable from a flag on the
/// command line), not ending with `.`, and not a Windows device name. Components must not be
/// empty.
///
/// # Arguments
/// * `name` - The pallet name to validate.
///
/// # Returns
/// * `Ok(())`      - If the name is valid.
/// * `Err(String)` - If the name is not valid.
pub fn validate_pallet_name(name: &str) -> Result<(), String> {
    match pallet_name_problem(name) {
        None => Ok(()),
        Some(reason) => {
            let reason = with_lowercase_hint(reason, name, |lowered| pallet_name_problem(lowered).is_none());
            Err(format!("\"{}\" is not a valid pallet name: {}", name, reason))
        }
    }
}

/// The reason `name` is not a valid pallet name, if it is not (see [`validate_pallet_name`]).
fn pallet_name_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("it is empty".to_string());
    }

    for component in name.split('/') {
        if component.is_empty() {
            return Some("it contains an empty path component".to_string());
        }

        if let Some(reason) = portable_component_problem(component, "\".\", \"_\", \"-\" and \"/\"") {
            return Some(reason);
        }
    }

    None
}

/// Check whether a user pallet exists (i.e. it has a ref file — it was stacked on at least
/// once or explicitly palletized from an existing head).
///
/// # Arguments
/// * `name` - The name of the pallet.
///
/// # Returns
/// * `true`  - If the pallet has a ref file.
/// * `false` - If it does not.
pub fn does_pallet_exist(name: &str) -> bool {
    get_pallet_ref_path(PalletNamespace::User, name).is_file()
}

/// Check whether a meta pallet exists.
///
/// # Arguments
/// * `name` - The name of the meta pallet.
///
/// # Returns
/// * `true`  - If the meta pallet has a ref file.
/// * `false` - If it does not.
pub fn does_meta_pallet_exist(name: &str) -> bool {
    get_pallet_ref_path(PalletNamespace::Meta, name).is_file()
}

/// Where a bay stands: its current pallet, and the base — the parcel its inventory and working
/// directory were last materialized from or stacked as (`None` while the pallet is unborn). The
/// pallet's ref is shared by every bay on it and can move under this bay; the base cannot, so
/// whatever treats "the head" as the tree this bay holds reads the base, and whatever treats it as
/// the pallet's history reads the ref.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BayPosition {
    /// The current pallet (the one new parcels are stacked on — git's HEAD equivalent).
    pub pallet: String,

    /// The parcel the bay's files stand on, or `None` for an unborn pallet.
    pub base: Option<String>,
}

impl BayPosition {
    /// A position on `pallet`, standing on `base`.
    pub fn new(pallet: impl Into<String>, base: Option<String>) -> Self {
        BayPosition { pallet: pallet.into(), base }
    }

    /// The head the pallet's ref holds now, which another bay may have moved past [`Self::base`].
    pub fn head(&self) -> Result<Option<String>, String> {
        get_pallet_head(&self.pallet)
    }

    /// Refuse, unless the bay stands on its pallet's head (an unborn pallet and an unborn base
    /// count as equal). Every command that moves the current pallet asks this first: building on a
    /// base the pallet has moved past would revert what moved it. The compare-and-set from the
    /// base, not this check, is what closes the race; the check gives the message, before any
    /// object is written.
    ///
    /// # Arguments
    /// * `operation` - What is refused, such as "stacked" ("Nothing was <operation>.").
    pub fn require_current(&self, operation: &str) -> Result<(), String> {
        let head = self.head()?;

        if head == self.base {
            return Ok(());
        }

        Err(behind_refusal(&self.pallet, self.base.as_deref(), head.as_deref(), operation).into())
    }
}

/// Why a bay standing on `base` may not move `pallet`, whose head is `head`: see
/// [`BayPosition::require_current`].
pub fn behind_refusal(pallet: &str, base: Option<&str>, head: Option<&str>, operation: &str) -> CoreError {
    CoreError::refusal(
        RefusalCode::Behind,
        format!(
            "This bay is behind pallet \"{}\": its files stand on {}, but the pallet is at {}, moved \
            since by another bay or process. Nothing was {}. Run \"forklift shift {}\" to catch up, \
            then try again.",
            pallet,
            base.map(|base| format!("parcel {}", base)).unwrap_or("no parcel".to_string()),
            head.map(|head| format!("parcel {}", head)).unwrap_or("no parcel".to_string()),
            operation,
            pallet
        ),
        catch_up_step(pallet),
    )
}

/// A [`RefusalCode::RemoteAhead`] refusal saying `message`: `lower` brings the remote's parcels in.
pub fn remote_ahead_refusal(message: String) -> CoreError {
    CoreError::refusal(RefusalCode::RemoteAhead, message, "forklift lower")
}

/// The [`RefusalCode::PalletNotFound`] refusal for `reference`, a pallet this warehouse holds no
/// ref for. A user pallet may exist on the remote, so with a remote configured the next step is
/// `lower <name>`, which brings it in; without one it is `palletize <name>`, which creates it. A
/// meta pallet is never created by hand: a bare `lower` syncs the remote's, and without a remote
/// the next step lists the ones there are.
pub fn pallet_not_found_refusal(reference: &PalletRef) -> CoreError {
    let wire = reference.to_wire();
    let remote = crate::util::remote_utils::RemoteClient::is_configured();

    if reference.namespace == PalletNamespace::Meta && remote {
        return CoreError::refusal(
            RefusalCode::PalletNotFound,
            format!(
                "No meta pallet \"{}\" exists in this warehouse. \"forklift lower\" syncs the remote's \
                meta pallets.",
                wire
            ),
            "forklift lower",
        );
    }

    if reference.namespace == PalletNamespace::Meta {
        return CoreError::refusal(
            RefusalCode::PalletNotFound,
            format!("No meta pallet \"{}\" exists in this warehouse.", wire),
            "forklift palletize --all",
        );
    }

    if remote {
        return CoreError::refusal(
            RefusalCode::PalletNotFound,
            format!(
                "No pallet named \"{p}\" exists in this warehouse (or it has nothing stacked). If the \
                remote has one, \"forklift lower {p}\" brings it in; \"forklift palletize {p}\" \
                creates a new one.",
                p = wire
            ),
            format!("forklift lower {}", wire),
        );
    }

    CoreError::refusal(
        RefusalCode::PalletNotFound,
        format!(
            "No pallet named \"{p}\" exists in this warehouse (or it has nothing stacked). \
            \"forklift palletize {p}\" creates it.",
            p = wire
        ),
        format!("forklift palletize {}", wire),
    )
}

/// The refusal a remote head answers a ref update with when the pallet's head is `current`, not
/// the `expected` the client sent: the remote moved since the client read it.
pub fn pallet_moved_refusal(current: Option<&str>, expected: Option<&str>) -> CoreError {
    remote_ahead_refusal(format!(
        "The pallet moved: its head is {}, not {}. Lower and retry.",
        current.unwrap_or("unborn"),
        expected.unwrap_or("unborn")
    ))
}

/// The next step of a [`RefusalCode::Behind`] refusal with nothing in progress: catch up.
fn catch_up_step(pallet: &str) -> String {
    format!("forklift shift {}", pallet)
}

/// The [`record_utils`] kind and version of a bay's position file: the pallet name, then the base
/// parcel hash (an empty entry for an unborn pallet).
const POSITION_RECORD: &str = "position";
const POSITION_VERSION: u32 = 1;

/// Read the active bay's position.
///
/// # Returns
/// * `Ok(BayPosition)` - Where the bay stands.
/// * `Err(String)`     - If the position file is missing, unreadable or malformed. There is no
///                       default: a bay without one was never prepared, and guessing a pallet
///                       would pair it with no base.
pub fn read_position() -> Result<BayPosition, String> {
    read_position_in(&bay_root())
}

/// Read the position of the bay whose bay-local state dir is `dir` (see [`read_position`]).
pub fn read_position_in(dir: &Path) -> Result<BayPosition, String> {
    let path = dir.join(FILE_NAME_POSITION);

    let bytes = std::fs::read(&path)
        .map_err(|e| format!("Error while reading the bay position file \"{}\": {}", path.to_string_lossy(), e))?;

    let malformed = |reason: String| format!("The bay position file \"{}\" {}", path.to_string_lossy(), reason);

    let entries = crate::util::record_utils::from_bytes(&bytes, POSITION_RECORD, POSITION_VERSION)
        .map_err(|e| malformed(e.describe()))?;

    let [pallet, base] = <[String; 2]>::try_from(entries).map_err(|_| malformed("is malformed.".to_string()))?;

    validate_pallet_name(&pallet).map_err(|e| malformed(format!("names an invalid pallet: {}", e)))?;

    let base = match base.as_str() {
        "" => None,
        hash if object_utils::is_object_hash(hash) => Some(base),
        _ => return Err(malformed("is malformed.".to_string())),
    };

    Ok(BayPosition { pallet, base })
}

/// Write the active bay's position (atomically: the pallet and the base change together).
///
/// # Returns
/// * `Ok(())`      - If the position was written.
/// * `Err(String)` - If the pallet name is invalid or the file could not be written.
pub fn write_position(position: &BayPosition) -> Result<(), String> {
    write_position_in(&bay_root(), position)
}

/// Record that the active bay now stands on `head`, the parcel this command just moved `pallet`
/// to. The ref already holds `head`, so a failure says so (see [`landed_but`]).
pub fn record_landed(pallet: &str, head: &str) -> Result<(), String> {
    write_position(&BayPosition::new(pallet, Some(head.to_string()))).map_err(|e| landed_but(pallet, head, &e))
}

/// The error for a step that failed after this command moved `pallet` to `head`: the move stands,
/// and catching up (`shift <pallet>`) brings the bay to it — recording the position alone when
/// the files already match it.
pub fn landed_but(pallet: &str, head: &str, error: &str) -> String {
    format!(
        "Pallet \"{}\" now holds parcel {}, but a step after that write failed: {} The parcel \
        stands; run \"forklift shift {}\" to bring this bay up to it.",
        pallet, head, error, pallet
    )
}

/// [`write_position`] into the bay-local state dir `dir`, whichever bay it belongs to.
pub fn write_position_in(dir: &Path, position: &BayPosition) -> Result<(), String> {
    validate_pallet_name(&position.pallet)?;

    let base = position.base.as_deref().unwrap_or("");
    let bytes = crate::util::record_utils::to_bytes(POSITION_RECORD, POSITION_VERSION, [position.pallet.as_str(), base]);

    file_utils::write_file_atomically(&dir.join(FILE_NAME_POSITION), &bytes)
}

/// Create the active bay's position (the default pallet, unborn) if it has none, along with the
/// pallets folder. Called by `prepare`.
///
/// # Returns
/// * `Ok(true)`    - If the position file was created.
/// * `Ok(false)`   - If it already existed.
/// * `Err(String)` - If an error occurred.
pub fn create_position_if_not_exists() -> Result<bool, String> {
    file_utils::create_folder_if_not_exists(&get_pallets_folder())?;

    if get_position_path().exists() {
        return Ok(false);
    }

    write_position(&BayPosition::new(DEFAULT_PALLET_NAME, None))?;

    Ok(true)
}

/// Get the head parcel hash of the given pallet.
///
/// # Arguments
/// * `name` - The name of the pallet. Must be a valid pallet name.
///
/// # Returns
/// * `Ok(Some(String))` - The hash of the pallet's head parcel.
/// * `Ok(None)`         - If the pallet is unborn (it has no ref file yet; it will be
///                        born by the first parcel stacked on it).
/// * `Err(String)`      - If the name is invalid, or the ref file could not be read or
///                        does not contain a valid hash.
pub fn get_pallet_head(name: &str) -> Result<Option<String>, String> {
    get_pallet_head_in(PalletNamespace::User, name)
}

/// Get the head parcel hash of the given meta pallet (e.g. the office).
///
/// # Arguments
/// * `name` - The name of the meta pallet. Must be a valid pallet name.
///
/// # Returns
/// * `Ok(Some(String))` - The hash of the pallet's head parcel.
/// * `Ok(None)`         - If the meta pallet is unborn.
/// * `Err(String)`      - If the name is invalid, or the ref could not be read.
pub fn get_meta_pallet_head(name: &str) -> Result<Option<String>, String> {
    get_pallet_head_in(PalletNamespace::Meta, name)
}

/// Get the head parcel hash of a pallet in the given namespace.
///
/// # Arguments
/// * `namespace` - The pallet namespace.
/// * `name`      - The name of the pallet. Must be a valid pallet name.
///
/// # Returns
/// * `Ok(Some(String))` - The hash of the pallet's head parcel.
/// * `Ok(None)`         - If the pallet is unborn (it has no ref file yet).
/// * `Err(String)`      - If the name is invalid, or the ref file could not be read or
///                        does not contain a valid hash.
pub fn get_pallet_head_in(namespace: PalletNamespace, name: &str) -> Result<Option<String>, String> {
    validate_pallet_name(name)?;

    let path = get_pallet_ref_path(namespace, name);

    if !path.is_file() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Error while reading the ref of pallet \"{}\": {}", name, e))?;
    let hash = content.trim_end_matches('\n').to_string();

    let is_valid_hash = object_utils::is_object_hash(&hash);

    if !is_valid_hash {
        return Err(format!(
            "The ref of pallet \"{}\" does not contain a valid parcel hash. The warehouse \
            may be corrupted; fix \"{}\" by hand.",
            name,
            path.to_string_lossy()
        ));
    }

    Ok(Some(hash))
}

/// A ref write refused because the ref no longer held the head the writer's work was based on:
/// another bay or process moved it while this command ran. Nothing was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefMoved {
    /// The ref that moved.
    pub reference: PalletRef,

    /// The head the writer expected (`None`: the ref was expected not to exist).
    pub expected: Option<String>,

    /// The head the ref held instead (`None`: it no longer exists).
    pub found: Option<String>,
}

impl RefMoved {
    /// This move as a [`RefusalCode::Behind`] refusal, for a writer moving the pallet its bay
    /// stands on: the bay is now behind it. Only such a writer may use it; a move of any other ref
    /// (a delivery's target) is retried, not caught up with.
    pub fn behind_refusal(&self) -> CoreError {
        let pallet = self.reference.to_wire();

        CoreError::refusal(
            RefusalCode::Behind,
            format!("{} Run \"forklift shift {}\" to catch up, then try again.", self, pallet),
            catch_up_step(&pallet),
        )
    }
}

impl std::fmt::Display for RefMoved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let describe = |head: &Option<String>| match head {
            Some(head) => head.clone(),
            None => "no head".to_string(),
        };

        write!(
            f,
            "Another bay or process moved pallet \"{}\" while this command ran (it expected {}, \
            found {}); this command did not move it.",
            self.reference.to_wire(), describe(&self.expected), describe(&self.found)
        )
    }
}

/// Why a ref compare-and-set did not write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefUpdateError {
    /// The ref moved under the writer (see [`RefMoved`]).
    Moved(RefMoved),

    /// The office pallet no longer held the head the writer's decision was based on
    /// ([`OfficePrecondition::At`]); nothing was written.
    OfficeMoved {
        /// The head the office held instead (`None`: unborn).
        found: Option<String>,
    },

    /// The trust anchor was no longer what the writer's decision read
    /// ([`AnchorPrecondition`]); nothing was written.
    AnchorMoved,

    /// A pallet head was outside the boundary the writer recorded
    /// ([`PalletsPrecondition::WithinBoundary`]); nothing was written.
    PalletsMoved {
        /// The pallet whose head is not in the boundary.
        pallet: PalletRef,

        /// Its head.
        found: String,
    },

    /// The pallet a delete's decision named as holding the deleted work was no longer at the head
    /// it was read at ([`PalletsPrecondition::Witness`]); nothing was written.
    WitnessMoved {
        /// The witness pallet.
        pallet: String,

        /// The head the decision read.
        expected: String,

        /// The head it holds now (`None`: it no longer exists).
        found: Option<String>,
    },

    /// The new head of a user pallet does not have its history completely present in this
    /// warehouse, within its fetch scope: a parcel, tree, blob or chunk it needs is missing, as a
    /// fetch that stopped part-way leaves it; nothing was written.
    Incomplete {
        /// The pallet whose ref was to move.
        pallet: String,

        /// The refused head.
        head: String,

        /// What is missing, as the closure check reported it.
        missing: String,
    },

    /// A creation of a user pallet that was deleted in this served warehouse, whose tombstone
    /// still stands: re-creating the name must acknowledge that tombstone and descend from its
    /// head (see [`update_pallet_head_in`]); nothing was written.
    Deleted {
        /// The deleted pallet.
        pallet: String,

        /// The head it was deleted at, which its tombstone holds.
        head: String,

        /// Whether the writer acknowledged the tombstone: then the refusal is that the new head
        /// does not descend from its head.
        acknowledged: bool,
    },

    /// Anything else: an invalid name or value, or the ref could not be read or written.
    Failed(String),
}

impl std::fmt::Display for RefUpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefUpdateError::Moved(moved) => moved.fmt(f),
            RefUpdateError::OfficeMoved { found } => write!(
                f,
                "Another bay or process changed the office while this command ran (its head is now \
                {}); this command wrote nothing. Run it again.",
                found.as_deref().unwrap_or("unborn")
            ),
            RefUpdateError::Incomplete { pallet, head, missing } => write!(
                f,
                "Refusing to set pallet \"{}\" to {}: its history is not completely present in this \
                warehouse ({}). Nothing was written. If a fetch of it stopped part-way, run that \
                fetch again before this command.",
                pallet, &head[..head.len().min(12)], missing
            ),
            RefUpdateError::AnchorMoved => f.write_str(
                "Another bay or process changed this warehouse's trust anchor while this command \
                ran; this command wrote nothing. Run it again."
            ),
            RefUpdateError::PalletsMoved { pallet, found } => write!(
                f,
                "Another bay or process moved pallet \"{}\" to {} while this command ran, after it \
                recorded the pallet heads it vouches for; this command wrote nothing. Run it again.",
                pallet.to_wire(), found
            ),
            RefUpdateError::WitnessMoved { pallet, expected, found } => write!(
                f,
                "Another bay or process moved pallet \"{}\", which holds this work, while this command \
                ran (it was at {}, and is now at {}); this command wrote nothing. Run it again.",
                pallet, expected, found.as_deref().unwrap_or("no head")
            ),
            RefUpdateError::Deleted { pallet, head, acknowledged } => {
                match acknowledged {
                    false => write!(
                        f,
                        "Pallet \"{}\" was deleted at {}, and this warehouse has been served, so the \
                        name stays reserved for that history. Nothing was written.",
                        pallet, head
                    )?,
                    true => write!(
                        f,
                        "Refusing to re-create deleted pallet \"{}\": the new head does not descend \
                        from {}, the head it was deleted at, and this warehouse has been served, so \
                        the name stays reserved for that history. Nothing was written.",
                        pallet, head
                    )?,
                }

                write!(
                    f,
                    " Run \"forklift palletize --restore {}\" to bring it back at that head; or, \
                    where the warehouse is served from, \"forklift-server tombstone drop --root <warehouse> {}\" frees \
                    the name for unrelated history (and lets the deleted history be collected).",
                    pallet, pallet
                )
            }
            RefUpdateError::Failed(message) => f.write_str(message),
        }
    }
}

/// What a ref write's decision consumed besides the ref itself, re-checked under the
/// [`RefLock`](crate::util::lock_utils::RefLock) hold of the write: a decision judged against an
/// office, an anchor or a set of pallet heads is only written while they still hold. Read once,
/// earlier in the same command, from the same storage root as the target.
///
/// There is no default: every writer states what it consumed, and [`RefPreconditions::NONE`] is
/// the explicit spelling for a decision that consumed none of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefPreconditions {
    pub office: OfficePrecondition,
    pub anchor: AnchorPrecondition,
    pub pallets: PalletsPrecondition,
}

impl RefPreconditions {
    /// A decision that consumed neither the office, the anchor nor any other pallet head.
    pub const NONE: RefPreconditions = RefPreconditions {
        office: OfficePrecondition::NotConsumed,
        anchor: AnchorPrecondition::NotConsumed,
        pallets: PalletsPrecondition::NotConsumed,
    };
}

/// The office head a write's decision was based on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OfficePrecondition {
    /// The decision did not read the office.
    NotConsumed,

    /// The office must still be at this head (`None`: unborn).
    At(Option<String>),
}

/// The trust anchor a write's decision was based on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnchorPrecondition {
    /// The decision did not read the anchor.
    NotConsumed,

    /// The decision read no anchor: there must still be none.
    Absent,

    /// The anchor file must still hold exactly these bytes. Compared as bytes, not as a decoded
    /// anchor: any rewrite refuses.
    Bytes(Vec<u8>),
}

/// The pallet heads a trust write recorded as its boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PalletsPrecondition {
    /// The decision recorded no boundary.
    NotConsumed,

    /// Every head of the scope's pallets must be a member of `boundary`. Membership, not a
    /// snapshot: a boundary head no pallet holds (a remote's, or a deleted pallet's) never
    /// refuses, while a moved head or a new pallet always does.
    WithinBoundary {
        boundary: Vec<String>,
        scope: BoundaryScope,
    },

    /// User pallet `pallet` must still be at `head`: the pallet a decision read as holding some
    /// work (a delete's containment judgement).
    Witness {
        pallet: String,
        head: String,
    },
}

/// The pallets a boundary was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundaryScope {
    /// The user pallets.
    UserPallets,

    /// The user pallets and every meta pallet except the office.
    AllButOffice,
}

/// Check `preconditions` against the warehouse as it is now. Lock-free: the caller holds the
/// [`RefLock`](crate::util::lock_utils::RefLock) the write that follows takes place under.
///
/// # Returns
/// * `Ok(())`              - Every precondition holds.
/// * `Err(RefUpdateError)` - `OfficeMoved`, `AnchorMoved`, then `PalletsMoved` or `WitnessMoved`
///                           for the first one that does not hold (checked in that order), or
///                           `Failed` if one could not be read.
pub(crate) fn check_preconditions(_lock: &crate::util::lock_utils::RefLock,
                                  preconditions: &RefPreconditions) -> Result<(), RefUpdateError> {
    if let OfficePrecondition::At(expected) = &preconditions.office {
        let found = get_pallet_head_in(PalletNamespace::Meta, crate::util::office_utils::OFFICE_PALLET_NAME)?;

        if &found != expected {
            return Err(RefUpdateError::OfficeMoved { found });
        }
    }

    let anchor = match &preconditions.anchor {
        AnchorPrecondition::NotConsumed => None,
        AnchorPrecondition::Absent => Some(None),
        AnchorPrecondition::Bytes(bytes) => Some(Some(bytes.as_slice())),
    };

    if let Some(expected) = anchor {
        if crate::util::office_utils::read_trust_anchor_bytes()?.as_deref() != expected {
            return Err(RefUpdateError::AnchorMoved);
        }
    }

    match &preconditions.pallets {
        PalletsPrecondition::NotConsumed => {}
        PalletsPrecondition::WithinBoundary { boundary, scope } => check_within_boundary(boundary, *scope)?,
        PalletsPrecondition::Witness { pallet, head } => {
            let found = get_pallet_head_in(PalletNamespace::User, pallet)?;

            if found.as_deref() != Some(head.as_str()) {
                return Err(RefUpdateError::WitnessMoved { pallet: pallet.clone(), expected: head.clone(), found });
            }
        }
    }

    Ok(())
}

/// [`PalletsPrecondition::WithinBoundary`]: every head of `scope`'s pallets is in `boundary`.
fn check_within_boundary(boundary: &[String], scope: BoundaryScope) -> Result<(), RefUpdateError> {
    let boundary: std::collections::HashSet<&str> = boundary.iter().map(String::as_str).collect();

    let namespaces: &[PalletNamespace] = match scope {
        BoundaryScope::UserPallets => &[PalletNamespace::User],
        BoundaryScope::AllButOffice => &[PalletNamespace::User, PalletNamespace::Meta],
    };

    for &namespace in namespaces {
        for name in list_pallets_in(namespace)? {
            if namespace == PalletNamespace::Meta && name == crate::util::office_utils::OFFICE_PALLET_NAME {
                continue;
            }

            if let Some(found) = get_pallet_head_in(namespace, &name)? {
                if !boundary.contains(found.as_str()) {
                    return Err(RefUpdateError::PalletsMoved { pallet: PalletRef { namespace, name }, found });
                }
            }
        }
    }

    Ok(())
}

impl From<String> for RefUpdateError {
    fn from(message: String) -> Self {
        RefUpdateError::Failed(message)
    }
}

impl From<RefUpdateError> for String {
    fn from(error: RefUpdateError) -> Self {
        error.to_string()
    }
}

/// Move the head of the given pallet from `expected` to `new` (see [`update_pallet_head_in`]).
///
/// # Arguments
/// * `name`     - The name of the pallet. Must be a valid pallet name.
/// * `expected` - The head the caller's work was based on (`None`: the pallet is unborn).
/// * `new`      - The hash of the new head parcel.
/// * `preconditions` - What the caller's decision consumed besides the ref.
///
/// # Returns
/// * `Ok(())`                - The head was moved.
/// * `Err(RefUpdateError)`   - The ref or a precondition moved under the caller, or the write
///                             failed.
pub fn update_pallet_head(name: &str,
                          expected: Option<&str>,
                          new: &str,
                          preconditions: &RefPreconditions) -> Result<(), RefUpdateError> {
    update_pallet_head_in(PalletNamespace::User, name, expected, new, preconditions)
}

/// Move the head of the given meta pallet from `expected` to `new` (see
/// [`update_pallet_head_in`]).
///
/// # Arguments
/// * `name`     - The name of the meta pallet. Must be a valid pallet name.
/// * `expected` - The head the caller's work was based on (`None`: the pallet is unborn).
/// * `new`      - The hash of the new head parcel.
/// * `preconditions` - What the caller's decision consumed besides the ref.
///
/// # Returns
/// * `Ok(())`                - The head was moved.
/// * `Err(RefUpdateError)`   - The ref or a precondition moved under the caller, or the write
///                             failed.
pub fn update_meta_pallet_head(name: &str,
                               expected: Option<&str>,
                               new: &str,
                               preconditions: &RefPreconditions) -> Result<(), RefUpdateError> {
    update_pallet_head_in(PalletNamespace::Meta, name, expected, new, preconditions)
}

/// Move the head of a pallet from `expected` to `new`: a compare-and-set, and the only way a
/// pallet ref is written. Under the warehouse's [`RefLock`](crate::util::lock_utils::RefLock),
/// which every bay shares, the head is read and then the `preconditions` are checked; when either
/// does not hold nothing is written and the write is refused (a moved head is reported first). Otherwise the ref file is replaced
/// atomically, created (with any subfolders a `/`-separated name needs) when the pallet is unborn.
///
/// `expected` is the head the caller's work was based on, and `preconditions` the office, anchor
/// and boundary its decision consumed, all read earlier in the same command, so a move by another
/// bay or process anywhere between those reads and this write is refused rather than overwritten.
/// A target that is the office itself may carry an [`OfficePrecondition::At`] too; both must hold.
///
/// In a served warehouse (see [`tombstone_utils`](crate::util::tombstone_utils)), creating a user
/// pallet whose name has a tombstone is refused with [`RefUpdateError::Deleted`]: only a creation
/// that acknowledges that tombstone and whose new head descends from (or is) the tombstone's head
/// may take the name, through [`recreate_pallet_head`] or a remote's acknowledged update, and it
/// clears the tombstone in the same hold, since the new ref then reaches everything it held.
///
/// # Arguments
/// * `namespace`     - The pallet namespace.
/// * `name`          - The name of the pallet. Must be a valid pallet name.
/// * `expected`      - The head the ref must hold (`None`: the ref must not exist).
/// * `new`           - The hash of the new head parcel.
/// * `preconditions` - What the caller's decision consumed besides the ref.
///
/// # Returns
/// * `Ok(())`                         - The head was moved.
/// * `Err(RefUpdateError::Moved)`     - The ref did not hold `expected`; nothing was written.
/// * `Err(RefUpdateError::OfficeMoved | AnchorMoved | PalletsMoved | WitnessMoved)`
///                                    - A precondition did not hold; nothing was written.
/// * `Err(RefUpdateError::Incomplete)` - A user pallet's new head does not have its history
///                                      completely present; nothing was written.
/// * `Err(RefUpdateError::Deleted)`   - The pallet was deleted in this served warehouse; nothing
///                                      was written.
/// * `Err(RefUpdateError::Failed)`    - The name is invalid, `new` is not an object hash, or the
///                                      ref could not be locked, read or written (the write may
///                                      still have landed: read the ref back to tell).
pub fn update_pallet_head_in(
    namespace: PalletNamespace,
    name: &str,
    expected: Option<&str>,
    new: &str,
    preconditions: &RefPreconditions,
) -> Result<(), RefUpdateError> {
    update_pallet_head_checked(namespace, name, expected, new, preconditions, None, None)
}

/// Create user pallet `name` at `new`, acknowledging the tombstone a delete in this served
/// warehouse left at `tombstone` (see [`update_pallet_head_in`]): the creation is refused unless
/// that is the name's tombstone and `new` descends from it, and clears it when it lands. A name
/// with no tombstone is created as [`update_pallet_head_in`] creates it.
pub fn recreate_pallet_head(name: &str,
                            new: &str,
                            preconditions: &RefPreconditions,
                            tombstone: &str) -> Result<(), RefUpdateError> {
    update_pallet_head_checked(PalletNamespace::User, name, None, new, preconditions, None, Some(tombstone))
}

/// [`update_pallet_head_in`] for a caller that has just verified `new`'s closure itself (a
/// remote's ref update, after its own closure check): the write skips its own verification when
/// `verified` is for `new` at the current fetch scope, and verifies as usual otherwise. A creation
/// may acknowledge the name's tombstone, as [`recreate_pallet_head`]'s does.
pub fn update_pallet_head_verified(
    namespace: PalletNamespace,
    name: &str,
    expected: Option<&str>,
    new: &str,
    preconditions: &RefPreconditions,
    verified: &VerifiedClosure,
    tombstone: Option<&str>,
) -> Result<(), RefUpdateError> {
    update_pallet_head_checked(namespace, name, expected, new, preconditions, Some(verified), tombstone)
}

/// That a head's closure was verified, and at which fetch scope. Minted only by the verifier
/// ([`audit_utils::verify_parcel_closure_for_write`](crate::util::audit_utils::verify_parcel_closure_for_write)),
/// so holding one means the check ran.
#[derive(Clone, Debug)]
pub struct VerifiedClosure {
    head: String,
    scope: MaterializationScope,
}

impl VerifiedClosure {
    pub(crate) fn new(head: &str, scope: MaterializationScope) -> VerifiedClosure {
        VerifiedClosure { head: head.to_string(), scope }
    }
}

/// The compare-and-set behind [`update_pallet_head_in`] and [`update_pallet_head_verified`].
///
/// A user-pallet ref moving to a new parcel first has that parcel's closure verified (see
/// [`verify_new_head`]), before the lock, since the walk can be long and the hold does no walking.
/// Objects cannot vanish between the walk and the write: every command that writes a ref holds
/// the store-access lock shared, which every deleter needs exclusively. The fetch scope can change
/// in between, though (an `expand` in another bay commits a widened one under the lock), so the
/// hold re-reads it, and when it is no longer the scope verified against, the hold is released and
/// the closure verified again at the new one.
///
/// A creation's tombstone check works the same way: whether `new` descends from the tombstone's
/// head is walked before the lock, and the hold re-reads the tombstone, starting over when it
/// changed meanwhile.
fn update_pallet_head_checked(
    namespace: PalletNamespace,
    name: &str,
    expected: Option<&str>,
    new: &str,
    preconditions: &RefPreconditions,
    verified: Option<&VerifiedClosure>,
    acknowledged: Option<&str>,
) -> Result<(), RefUpdateError> {
    // Checked before the lock too, so an invalid write never waits for it.
    validate_head_write(name, new)?;

    let mut verified_at = match closure_to_verify(namespace, expected, new) {
        false => None,
        true => {
            let scope = crate::util::scope_utils::read_fetch_scope()?;

            if !verified.is_some_and(|verified| verified.head == new && verified.scope == scope) {
                verify_new_head(name, expected, new, &scope)?;
            }

            Some(scope)
        }
    };

    // Worked out before the lock, since it can walk history: the compare-and-set pins the ref to
    // `expected`, so what it says about `expected` holds inside the hold too. After the closure
    // check, so a head whose ancestry is missing is refused as incomplete rather than failing
    // this walk first.
    let left = left_unreached(namespace, expected, new)?;

    let mut tombstone = recreated_tombstone(namespace, name, expected, new, acknowledged)?;

    loop {
        #[cfg(test)]
        tests::before_hold_checkpoint();

        let lock = crate::util::lock_utils::RefLock::acquire()?;

        // The ref's own compare goes first: a ref another writer moved, perhaps to this very
        // parcel (which callers read as landed), reports that move, never a precondition it
        // would mask.
        require_head(namespace, name, expected)?;
        check_preconditions(&lock, preconditions)?;

        if tombstone_of(namespace, name, expected)? != tombstone.as_ref().map(|(head, _)| head.clone()) {
            drop(lock);
            tombstone = recreated_tombstone(namespace, name, expected, new, acknowledged)?;
            continue;
        }

        if let Some((head, descends)) = &tombstone {
            let acknowledged = acknowledged == Some(head.as_str());

            if !acknowledged || !descends {
                return Err(RefUpdateError::Deleted { pallet: name.to_string(), head: head.clone(), acknowledged });
            }
        }

        if let Some(scope) = &verified_at {
            let current = crate::util::scope_utils::read_fetch_scope()?;

            if &current != scope {
                drop(lock);
                verify_new_head(name, expected, new, &current)?;
                verified_at = Some(current);
                continue;
            }
        }

        let written = update_pallet_head_locked(&lock, namespace, name, expected, new, left);

        // A write can land and still report a failure, so the ref is read back before the
        // tombstone is kept. Once it has landed, a tombstone that cannot be cleared fails nothing:
        // beside a live ref it only over-roots, and the next removal of the name replaces it.
        if tombstone.is_some() && (written.is_ok() || get_pallet_head_in(namespace, name).ok().flatten().as_deref() == Some(new)) {
            let _ = clear_tombstone(&lock, name);
        }

        return written;
    }
}

/// The head of the tombstone a write of `new` over `expected` would re-create, and whether the
/// write may: `acknowledged` names that head and `new` descends from (or is) it. `None` unless the
/// write creates a user pallet whose name has a tombstone (see [`tombstone_of`]).
fn recreated_tombstone(namespace: PalletNamespace,
                       name: &str,
                       expected: Option<&str>,
                       new: &str,
                       acknowledged: Option<&str>) -> Result<Option<(String, bool)>, String> {
    let Some(head) = tombstone_of(namespace, name, expected)? else {
        return Ok(None);
    };

    // A creation that does not acknowledge the tombstone is refused whatever it descends from, so
    // its history is not walked.
    let descends = acknowledged == Some(head.as_str())
        && crate::util::merge_utils::is_ancestor(&head, new)?;

    Ok(Some((head, descends)))
}

/// The head of the tombstone of user pallet `name`, when a write over `expected` creates it: only
/// a creation is bound by a tombstone, and only a served warehouse has any. The mark is read
/// fail-closed.
fn tombstone_of(namespace: PalletNamespace,
                name: &str,
                expected: Option<&str>) -> Result<Option<String>, String> {
    use crate::util::tombstone_utils;

    if expected.is_some() || namespace != PalletNamespace::User || !tombstone_utils::is_served()? {
        return Ok(None);
    }

    Ok(tombstone_utils::read_tombstones()?.remove(name))
}

/// Remove `name`'s tombstone from the list, under the held ref lock.
fn clear_tombstone(lock: &crate::util::lock_utils::RefLock, name: &str) -> Result<(), String> {
    use crate::util::tombstone_utils;

    let mut tombstones = tombstone_utils::read_tombstones()?;

    if tombstones.remove(name).is_some() {
        tombstone_utils::write_tombstones(lock, &tombstones)?;
    }

    Ok(())
}

/// Whether a write of `new` over `expected` must verify `new`'s closure: a user-pallet ref moving
/// to a parcel it does not already hold. A meta pallet's writers verify their own history.
fn closure_to_verify(namespace: PalletNamespace, expected: Option<&str>, new: &str) -> bool {
    namespace == PalletNamespace::User && expected != Some(new)
}

/// Verify that `new`'s history is completely present at the fetch scope `scope`, back to what
/// some user-pallet ref already reaches ([`audit_utils::verify_ref_head_closure`]), refusing with
/// [`RefUpdateError::Incomplete`] otherwise. `expected`, the ref's current head, is one of those.
///
/// [`audit_utils::verify_ref_head_closure`]: crate::util::audit_utils::verify_ref_head_closure
fn verify_new_head(name: &str,
                   expected: Option<&str>,
                   new: &str,
                   scope: &MaterializationScope) -> Result<(), RefUpdateError> {
    let heads: Vec<String> = all_pallet_refs()?.into_iter()
        .filter(|(pallet, _)| pallet.namespace == PalletNamespace::User)
        .map(|(_, head)| head)
        .collect();

    crate::util::audit_utils::verify_ref_head_closure(new, &heads, expected, scope)
        .map_err(|missing| RefUpdateError::Incomplete {
            pallet: name.to_string(),
            head: new.to_string(),
            missing,
        })
}

/// The head a write of `new` over `expected` leaves no longer reached from its ref, when that
/// matters: a user-pallet ref in a sparse warehouse moving to a parcel that does not descend from
/// its old head. Such a head goes into the fetched-heads ledger with the write, so an `expand`
/// walks it before an `undo` can write it back.
fn left_unreached<'a>(namespace: PalletNamespace,
                      expected: Option<&'a str>,
                      new: &str) -> Result<Option<&'a str>, String> {
    let old = match expected {
        Some(old) if namespace == PalletNamespace::User && old != new => old,
        _ => return Ok(None),
    };

    if crate::util::scope_utils::read_fetch_scope()?.is_full()
        || crate::util::merge_utils::is_ancestor(old, new)?
    {
        return Ok(None);
    }

    Ok(Some(old))
}

/// Refuse with [`RefUpdateError::Moved`] unless the ref holds `expected`; the head it holds.
fn require_head(namespace: PalletNamespace,
                name: &str,
                expected: Option<&str>) -> Result<Option<String>, RefUpdateError> {
    let found = get_pallet_head_in(namespace, name)?;

    if found.as_deref() != expected {
        return Err(RefUpdateError::Moved(RefMoved {
            reference: PalletRef { namespace, name: name.to_string() },
            expected: expected.map(str::to_string),
            found,
        }));
    }

    Ok(found)
}

/// The refusals of a head write that need no lock: an invalid name, or a value the reader would
/// refuse.
fn validate_head_write(name: &str, new: &str) -> Result<(), RefUpdateError> {
    validate_pallet_name(name)?;

    // The writer holds the reader's rule: a ref `get_pallet_head_in` would refuse is never stored.
    if !object_utils::is_object_hash(new) {
        return Err(RefUpdateError::Failed(format!(
            "Refusing to set the head of pallet \"{}\": the value is not an object hash (64 \
            lowercase hex characters).",
            name
        )));
    }

    Ok(())
}

/// [`update_pallet_head_in`]'s compare-and-set without its lock or its preconditions, for a writer
/// that already holds the [`RefLock`](crate::util::lock_utils::RefLock) and checked what it
/// consumed under it (a trust install, which writes the anchor and the office in one hold). Keeps
/// the readback and the write log.
///
/// It is where `left`, the head the write leaves unreached ([`left_unreached`], worked out before
/// the hold), is recorded in the fetched-heads ledger, in the same hold as the write. It verifies
/// no closure, so a user-pallet head write must reach it through [`update_pallet_head_checked`],
/// which verifies the new head or takes a [`VerifiedClosure`] for exactly that head and scope. Its
/// one other caller, the trust install, writes the meta office ref, whose writer verifies its own
/// history.
pub(crate) fn update_pallet_head_locked(
    lock: &crate::util::lock_utils::RefLock,
    namespace: PalletNamespace,
    name: &str,
    expected: Option<&str>,
    new: &str,
    left: Option<&str>,
) -> Result<(), RefUpdateError> {
    validate_head_write(name, new)?;

    let found = require_head(namespace, name, expected)?;

    if let Some(old) = left {
        crate::util::fetch_ledger::record_fetch(lock, old, name)?;
    }

    let path = get_pallet_ref_path(namespace, name);

    // A folder where the ref file must go (left by a nested name whose emptied folders could not
    // be removed) reads as an unborn pallet; writing over it is refused by name, never attempted.
    if path.is_dir() {
        return Err(RefUpdateError::Failed(format!(
            "Refusing to write the ref of pallet \"{}\": a folder stands where its ref file must go \
            (\"{}\"). Move that folder out of the pallets folder, and run the command again.",
            name, std::path::absolute(&path).unwrap_or_else(|_| path.clone()).to_string_lossy()
        )));
    }

    if let Some(parent) = path.parent() {
        file_utils::create_folder_if_not_exists(parent)?;
    }

    let written = file_utils::write_file_atomically(&path, format!("{}\n", new).as_bytes());

    // A failed write can still have landed (the rename went through, the directory sync did not);
    // the log records what the ref now holds, so it is read back rather than inferred.
    if written.is_ok() || get_pallet_head_in(namespace, name).ok().flatten().as_deref() == Some(new) {
        log_write(PalletRef { namespace, name: name.to_string() }.to_wire(), found, Some(new.to_string()));
    }

    written?;

    Ok(())
}

/// Remove the ref of user pallet `name`, which must hold `expected`: the compare-and-set core's
/// one removing write. Objects are untouched. What keeps the removed head stored depends on the
/// warehouse: in a served one (see [`tombstone_utils`](crate::util::tombstone_utils)) the removal
/// leaves a tombstone, which gc, compaction and prune root until it is cleared or dropped; in any
/// other, `journal`, the delete's undo entry, whose live entries they root while the delete can be
/// undone (see [`journal_utils::removed_heads`](crate::util::journal_utils::removed_heads)). A
/// removal with no undo entry (a remote's delete, which no bay made) is refused outside a served
/// warehouse, since nothing would root its head.
///
/// Under the [`RefLock`](crate::util::lock_utils::RefLock), in this order: the ref's own compare,
/// the `preconditions`, the fetched-heads ledger (a sparse warehouse records the removed head,
/// which this ref no longer reaches, so `expand` walks it), the served mark, read fail-closed,
/// then the tombstone (in a served warehouse, replacing any the name had) and `journal` appended
/// to the bay's undo journal, each durably, before the ref file is unlinked. The unlink and every
/// folder a nested name leaves empty are synced through the durability taint, so what is visible
/// is durable.
///
/// A failed unlink or sync can still have landed, so the ref is read back inside the hold. Still
/// at `expected`: nothing was removed, and the tombstone list and the journal are written back
/// exactly as they were read before (not the appended journal with its last entry popped: an
/// append to a full journal drops the oldest entry, which can itself root a removed head). Gone:
/// the removal is logged and the failure reported; the caller reads the ref back to tell.
/// Unreadable: both records stay. A tombstone beside a live ref of its name only keeps its head,
/// and an undo of the entry finds the ref either absent or at `expected`, both of which it handles.
///
/// # Arguments
/// * `namespace`     - The pallet namespace; a meta pallet is refused.
/// * `name`          - The name of the pallet. Must be a valid pallet name.
/// * `expected`      - The head the ref must hold.
/// * `preconditions` - What the caller's decision consumed besides the ref.
/// * `journal`       - The undo entry to append in the hold, for a delete a bay makes. It must
///                     record exactly this removal (`name` at `expected`): outside a served
///                     warehouse the entry is what keeps the head rooted while the delete can be
///                     undone, so a removal it does not record is refused.
///
/// # Returns
/// * `Ok(())`                       - The ref was removed.
/// * `Err(RefUpdateError::Moved)`   - The ref did not hold `expected`; nothing was written.
/// * `Err(RefUpdateError::OfficeMoved | AnchorMoved | PalletsMoved | WitnessMoved)`
///                                  - A precondition did not hold; nothing was written.
/// * `Err(RefUpdateError::Failed)`  - The pallet is a meta pallet, the name is invalid, `journal`
///                                    does not record this removal, there is no `journal` and the
///                                    warehouse is not served, a bulk store session is open, or a
///                                    read or write failed (the removal may still have landed:
///                                    read the ref back to tell).
pub fn remove_pallet_head_in(
    namespace: PalletNamespace,
    name: &str,
    expected: &str,
    preconditions: &RefPreconditions,
    journal: Option<&crate::util::journal_utils::JournalEntry>,
) -> Result<(), RefUpdateError> {
    if namespace == PalletNamespace::Meta {
        return Err(RefUpdateError::Failed(format!(
            "Refusing to remove meta pallet \"{}\": meta pallets hold the office, hauls and tags, \
            and are never removed.",
            PalletRef { namespace, name: name.to_string() }.to_wire()
        )));
    }

    validate_pallet_name(name)?;

    let wire = PalletRef { namespace, name: name.to_string() }.to_wire();

    if let Some(journal) = journal {
        if crate::util::journal_utils::removed_by(journal) != vec![(wire.clone(), expected.to_string())] {
            return Err(RefUpdateError::Failed(format!(
                "Refusing to remove pallet \"{}\": the undo entry written with the removal does not \
                record removing it at {}.",
                wire, expected
            )));
        }

        // Before anything is written, the ledger record included: an entry the journal would refuse.
        crate::util::journal_utils::validate_entry(journal).map_err(|e| RefUpdateError::Failed(format!(
            "Refusing to remove pallet \"{}\": its undo entry {}.", wire, e
        )))?;
    }

    // The records must be durable before the unlink; a bulk session would only stage them.
    if file_utils::is_bulk_session_open() {
        return Err(RefUpdateError::Failed(
            "A pallet ref cannot be removed while a bulk store session is open.".to_string()
        ));
    }

    // A removal has no new head, so its old head is always left unreached by this ref: in a
    // sparse warehouse it goes into the fetched-heads ledger, as a non-descendant move's does.
    let sparse = !crate::util::scope_utils::read_fetch_scope()?.is_full();

    let lock = crate::util::lock_utils::RefLock::acquire()?;

    require_head(namespace, name, Some(expected))?;
    check_preconditions(&lock, preconditions)?;

    if sparse {
        crate::util::fetch_ledger::record_fetch(&lock, expected, name)?;
    }

    let tombstones_before = match crate::util::tombstone_utils::is_served()? {
        true => {
            let read = crate::util::tombstone_utils::read_tombstones()?;
            let mut tombstones = read.clone();
            tombstones.insert(wire.clone(), expected.to_string());
            crate::util::tombstone_utils::write_tombstones(&lock, &tombstones)?;

            Some(read)
        }
        false if journal.is_none() => return Err(RefUpdateError::Failed(format!(
            "Refusing to remove pallet \"{}\": this warehouse has never been served, so it keeps no \
            tombstones, and a removal no bay can undo would leave its head held by nothing.",
            wire
        ))),
        false => None,
    };

    let journal_before = match journal {
        Some(journal) => match crate::util::journal_utils::append_in_hold(&lock, journal) {
            Ok(read) => Some(read),
            Err(e) => return Err(RefUpdateError::Failed(match &tombstones_before {
                Some(read) => match crate::util::tombstone_utils::write_tombstones(&lock, read) {
                    Ok(()) => e,
                    Err(put_back) => format!("{} The tombstones could not be put back either: {}", e, put_back),
                },
                None => e,
            })),
        },
        None => None,
    };

    let path = get_pallet_ref_path(namespace, name);
    let unlinked = unlink_ref(namespace, &path);

    match get_pallet_head_in(namespace, name) {
        Ok(None) => {
            log_write(wire, Some(expected.to_string()), None);
            unlinked.map_err(RefUpdateError::Failed)
        }
        Ok(Some(_)) => {
            let mut error = unlinked.err().unwrap_or_else(|| format!(
                "The ref of pallet \"{}\" was still there after it was removed.", name
            ));

            // Not removed: both records go back to what they held before.
            if let Some(read) = &journal_before {
                if let Err(e) = crate::util::journal_utils::write_back_in_hold(&lock, read) {
                    error = format!("{} The undo journal could not be put back either: {}", error, e);
                }
            }

            if let Some(read) = &tombstones_before {
                if let Err(e) = crate::util::tombstone_utils::write_tombstones(&lock, read) {
                    error = format!("{} The tombstones could not be put back either: {}", error, e);
                }
            }

            Err(RefUpdateError::Failed(error))
        }
        Err(read) => Err(RefUpdateError::Failed(match unlinked {
            Ok(()) => format!("The ref of pallet \"{}\" could not be read back after it was removed: {}", name, read),
            Err(e) => format!("{} The ref could not be read back either: {}", e, read),
        })),
    }
}

/// Unlink the ref file at `path` and every folder of a nested name it leaves empty, up to the
/// namespace folder, syncing each removal's parent through the durability taint. Folder removal
/// stops at the first that does not go (it holds another ref, or anything else).
fn unlink_ref(namespace: PalletNamespace, path: &Path) -> Result<(), String> {
    std::fs::remove_file(path).map_err(|e| format!(
        "Error while removing the ref file \"{}\": {}", path.to_string_lossy(), e
    ))?;

    let top = get_namespace_folder(namespace);
    let mut removed: &Path = path;

    while let Some(parent) = removed.parent() {
        file_utils::sync_dir_or_taint(parent, &[path])?;

        if parent == top || !parent.starts_with(&top) || std::fs::remove_dir(parent).is_err() {
            break;
        }

        removed = parent;
    }

    Ok(())
}

/// One head write the compare-and-set core made: the ref's wire form, the head it replaced
/// (`None` for a ref it created) and the head it wrote (`None` for a ref it removed).
pub type LoggedWrite = (String, Option<String>, Option<String>);

/// The write log: the head writes made under each recording warehouse, keyed by its
/// `forklift_root`. This is how a journaled command learns which refs it moved itself, rather than
/// diffing snapshots that would also catch what other bays or processes moved meanwhile. Keyed by
/// root, not per thread, because a command's writes can run on any runtime thread, and so
/// parallel tests in separate scratch warehouses neither see each other's writes nor switch each
/// other's recording off.
static WRITE_LOG: std::sync::Mutex<std::collections::BTreeMap<PathBuf, Vec<LoggedWrite>>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

fn write_log() -> std::sync::MutexGuard<'static, std::collections::BTreeMap<PathBuf, Vec<LoggedWrite>>> {
    WRITE_LOG.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Start recording this warehouse's head writes, discarding any recorded before.
pub fn start_write_log() {
    write_log().insert(forklift_root(), Vec::new());
}

/// Stop recording this warehouse's head writes and return them, oldest first (empty if it was
/// not recording).
pub fn take_write_log() -> Vec<LoggedWrite> {
    write_log().remove(&forklift_root()).unwrap_or_default()
}

fn log_write(reference: String, old: Option<String>, new: Option<String>) {
    if let Some(entries) = write_log().get_mut(&forklift_root()) {
        entries.push((reference, old, new));
    }
}

/// List the names of all user pallets that have a ref file, sorted alphabetically.
///
/// # Returns
/// * `Ok(Vec<String>)` - The pallet names.
/// * `Err(String)`     - If the pallets folder could not be read.
pub fn list_pallets() -> Result<Vec<String>, String> {
    list_pallets_in(PalletNamespace::User)
}

/// List the names of all meta pallets that have a ref file, sorted alphabetically.
///
/// # Returns
/// * `Ok(Vec<String>)` - The meta pallet names.
/// * `Err(String)`     - If the meta folder could not be read.
pub fn list_meta_pallets() -> Result<Vec<String>, String> {
    list_pallets_in(PalletNamespace::Meta)
}

/// List the names of all pallets with a ref file in the given namespace, sorted.
///
/// # Arguments
/// * `namespace` - The pallet namespace.
///
/// # Returns
/// * `Ok(Vec<String>)` - The pallet names.
/// * `Err(String)`     - If the folder could not be read.
pub fn list_pallets_in(namespace: PalletNamespace) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = Vec::new();
    let folder = get_namespace_folder(namespace);

    if folder.is_dir() {
        collect_pallet_names(&folder, "", &mut names)?;
    }

    names.sort();

    Ok(names)
}

/// Every born pallet across *both* namespaces, as `(ref, head)` pairs. This is the full
/// reachable-ref set: use it wherever the whole warehouse matters and meta pallets must
/// not be forgotten — GC roots, bundle contents, the re-genesis boundary, the handshake.
///
/// # Returns
/// * `Ok(Vec<(PalletRef, String)>)` - Every pallet with a head, user then meta.
/// * `Err(String)`                  - If a ref folder or file could not be read.
pub fn all_pallet_refs() -> Result<Vec<(PalletRef, String)>, String> {
    let mut refs: Vec<(PalletRef, String)> = Vec::new();

    for namespace in [PalletNamespace::User, PalletNamespace::Meta] {
        for name in list_pallets_in(namespace)? {
            if let Some(head) = get_pallet_head_in(namespace, &name)? {
                refs.push((PalletRef { namespace, name }, head));
            }
        }
    }

    Ok(refs)
}

/// Recursively collect pallet names from the pallets folder.
///
/// # Arguments
/// * `folder` - The folder to collect from.
/// * `prefix` - The name prefix accumulated so far (`""` at the top level).
/// * `names`  - The collected names.
///
/// # Returns
/// * `Ok(())`      - If the folder was processed.
/// * `Err(String)` - If a folder or entry could not be read, or a ref file's name is not a
///                   valid pallet name.
/// Files an operating system leaves in any folder a file browser opens. None is a valid pallet
/// name, so none can be a ref file, and skipping them hides no pallet.
const OS_LITTER_FILE_NAMES: [&str; 2] = [".DS_Store", "Thumbs.db"];

fn collect_pallet_names(folder: &Path, prefix: &str, names: &mut Vec<String>) -> Result<(), String> {
    for entry_result in file_utils::read_directory(&folder.to_path_buf())? {
        let entry = entry_result.map_err(|e| format!("Error while reading directory entry: {}", e))?;
        let entry_name = file_utils::get_name_for_file_or_directory(&entry)?;

        let full_name = if prefix.is_empty() {
            entry_name
        } else {
            format!("{}/{}", prefix, entry_name)
        };

        let metadata = file_utils::get_symlink_metadata_for_path(&entry.path())?;

        if metadata.is_dir() {
            collect_pallet_names(&entry.path(), &full_name, names)?;
        } else if OS_LITTER_FILE_NAMES.iter().any(|litter| full_name.rsplit('/').next() == Some(*litter)) {
            continue;
        } else if file_utils::staged_temp_base(&full_name)
            .is_some_and(|ref_name| validate_pallet_name(ref_name).is_ok()) {
            // An atomic ref write's temp beside a valid ref name: a write in flight, or litter from
            // one that was killed before its rename. Its spelling is never a valid pallet name,
            // and this listing holds no ref lock, so it is neither listed nor refused.
            continue;
        } else {
            // A ref file is a pallet only under a valid name. One that is not was written by
            // hand or by an older grammar, and listing it would hand every reader a name it
            // refuses (or, on a case-folding volume, an alias of another pallet).
            validate_pallet_name(&full_name).map_err(|e| format!(
                "The ref file \"{}\" does not name a valid pallet: {}. Rename it to a valid pallet \
                name, or move it out of the pallets folder, and run the command again.",
                std::path::absolute(entry.path()).unwrap_or_else(|_| entry.path()).to_string_lossy(), e
            ))?;
            names.push(full_name);
        }
    }

    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::globals::StorageRootScope;

    thread_local! {
        /// Run once by the next head write on this thread, between its closure check and its hold.
        static BEFORE_HOLD: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    }

    /// The seam [`update_pallet_head_checked`] calls before each hold.
    pub(super) fn before_hold_checkpoint() {
        if let Some(run) = BEFORE_HOLD.with(|slot| slot.borrow_mut().take()) {
            run();
        }
    }

    /// A fresh warehouse root (its `.forklift` folder exists) entered for this thread.
    fn scratch(name: &str) -> (PathBuf, StorageRootScope) {
        let root = std::env::temp_dir()
            .join(format!("forklift-pallet-utils-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let scope = StorageRootScope::enter(&root);

        (root, scope)
    }

    fn hash(c: char) -> String {
        stub_parcel(&c.to_string())
    }

    /// Point a user pallet at `new` without verifying `new`'s closure: a warehouse whose history
    /// went missing after a ref write that found it complete, for a test of what reads and
    /// repairs do then.
    pub(crate) fn update_pallet_head_unverified(name: &str,
                                                expected: Option<&str>,
                                                new: &str,
                                                preconditions: &RefPreconditions) -> Result<(), RefUpdateError> {
        update_pallet_head_in_unverified(PalletNamespace::User, name, expected, new, preconditions)
    }

    /// [`update_pallet_head_unverified`] in `namespace`.
    pub(crate) fn update_pallet_head_in_unverified(namespace: PalletNamespace,
                                                   name: &str,
                                                   expected: Option<&str>,
                                                   new: &str,
                                                   preconditions: &RefPreconditions) -> Result<(), RefUpdateError> {
        let verified = VerifiedClosure::new(new, crate::util::scope_utils::read_fetch_scope()?);
        update_pallet_head_verified(namespace, name, expected, new, preconditions, &verified, None)
    }

    /// A parcel over an empty tree, stored with its tree and distinct per `tag`: a head whose
    /// closure is complete, for a test that needs only some pallet head. Deterministic, so a test
    /// may compare a ref against a second call.
    pub(crate) fn stub_parcel(tag: &str) -> String {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::model::tree_item::TreeItem;

        let mut tree = LooseObjectBuilder::build_tree(
            &TreeItem::new(String::new(), String::new(), crate::enums::dir_entry_type::DirEntryType::Tree)
        );
        tree.store().unwrap();

        let mut parcel = LooseObjectBuilder::build_parcel(&crate::model::parcel::Parcel {
            tree_hash: tree.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some(tag.to_string()),
        });
        parcel.store().unwrap();
        parcel.hash
    }

    /// The closure check at a user-pallet head write: a head whose history is not completely
    /// present at the fetch scope is refused, back to what some ref already reaches.
    mod closure_at_write {
        use super::*;
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::enums::dir_entry_type::DirEntryType;
        use crate::model::blob::Blob;
        use crate::model::tree_item::TreeItem;

        fn blob(content: &str) -> String {
            let mut object = LooseObjectBuilder::build_blob(&Blob { content: content.as_bytes().to_vec() });
            object.store().unwrap();
            object.hash
        }

        fn tree(entries: &[(&str, &str, DirEntryType)]) -> String {
            let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
            for (name, hash, item_type) in entries {
                tree.add_child(TreeItem::new(name.to_string(), hash.to_string(), *item_type));
            }
            let mut object = LooseObjectBuilder::build_tree(&tree);
            object.store().unwrap();
            object.hash
        }

        /// A tree with `src/a` and `web/b`, holding `src` and `web` as their contents.
        fn two_dirs(src: &str, web: &str) -> String {
            let src = tree(&[("a", &blob(src), DirEntryType::Normal)]);
            let web = tree(&[("b", &blob(web), DirEntryType::Normal)]);
            tree(&[("src", &src, DirEntryType::Tree), ("web", &web, DirEntryType::Tree)])
        }

        fn parcel(tree_hash: &str, parents: &[&str]) -> String {
            let mut object = LooseObjectBuilder::build_parcel(&crate::model::parcel::Parcel {
                tree_hash: tree_hash.to_string(),
                parents: parents.iter().map(|parent| parent.to_string()).collect(),
                actions: Vec::new(),
                description: None,
            });
            object.store().unwrap();
            object.hash
        }

        fn remove(hash: &str) {
            let (folder, file) = file_utils::get_path_for_object(hash).unwrap();
            std::fs::remove_file(std::path::Path::new(&folder).join(file)).unwrap();
        }

        fn write(name: &str, expected: Option<&str>, new: &str) -> Result<(), RefUpdateError> {
            update_pallet_head(name, expected, new, &RefPreconditions::NONE)
        }

        fn assert_incomplete(result: Result<(), RefUpdateError>, name: &str, expected: Option<&str>) {
            assert!(matches!(result, Err(RefUpdateError::Incomplete { .. })), "{:?}", result);
            assert_eq!(get_pallet_head(name).unwrap().as_deref(), expected, "nothing was written");
        }

        // Revert: skip the closure check → both writes land over the missing blob; or refuse
        // every new head → the head stays refused once the blob arrives.
        #[test]
        fn a_head_and_an_intermediate_parcel_with_a_missing_blob_are_refused_until_it_arrives() {
            let (_root, _scope) = scratch("closure-full");
            let base = parcel(&two_dirs("a0", "b0"), &[]);
            write("main", None, &base).unwrap();

            let mid = parcel(&two_dirs("a1", "b1"), &[&base]);
            let head = parcel(&two_dirs("a2", "b1"), &[&mid]);
            let missing = blob("b1");
            remove(&missing);

            assert_incomplete(write("main", Some(&base), &head), "main", Some(&base));
            assert_incomplete(write("main", Some(&base), &mid), "main", Some(&base));
            assert_incomplete(write("side", None, &head), "side", None);

            // The fetch completes.
            blob("b1");
            write("main", Some(&base), &head).unwrap();
        }

        // Revert: drop the error's name for the missing object → the message names nothing.
        #[test]
        fn the_refusal_names_the_head_and_the_missing_object() {
            let (_root, _scope) = scratch("closure-message");
            let missing = blob("lost");
            let head = parcel(&tree(&[("f", &missing, DirEntryType::Normal)]), &[]);
            remove(&missing);

            let message = write("main", None, &head).unwrap_err().to_string();
            assert!(message.contains(&head[..12]) && message.contains(&missing), "{}", message);
            assert!(message.contains("Nothing was written"), "{}", message);
        }

        // Revert: check only the head parcel itself → the write lands over a deleted ancestor.
        #[test]
        fn a_head_whose_ancestor_parcel_is_gone_is_refused() {
            let (_root, _scope) = scratch("closure-ancestor");
            let base = parcel(&two_dirs("a0", "b0"), &[]);
            write("main", None, &base).unwrap();
            let mid = parcel(&two_dirs("a1", "b0"), &[&base]);
            let head = parcel(&two_dirs("a2", "b0"), &[&mid]);
            remove(&mid);

            assert_incomplete(write("main", Some(&base), &head), "main", Some(&base));
        }

        // Revert: work out the head the write leaves unreached before the closure check → its
        // ancestry walk meets the gone parcel first and the write fails, not refused as incomplete.
        #[test]
        fn a_sparse_move_off_a_non_ancestor_onto_a_gone_ancestry_is_refused_as_incomplete() {
            let (_root, _scope) = scratch("closure-sparse-unreached");
            crate::util::scope_utils::set_fetch_scope(&MaterializationScope::from_prefixes(["src"])).unwrap();
            let other = parcel(&two_dirs("x0", "y0"), &[]);
            write("main", None, &other).unwrap();
            let base = parcel(&two_dirs("a0", "b0"), &[]);
            let mid = parcel(&two_dirs("a1", "b0"), &[&base]);
            let head = parcel(&two_dirs("a2", "b0"), &[&mid]);
            remove(&mid);

            assert_incomplete(write("main", Some(&other), &head), "main", Some(&other));
        }

        // Revert: check only the first parent's side → the merge lands over the part-way side.
        #[test]
        fn a_merge_onto_a_part_way_parcel_is_refused() {
            let (_root, _scope) = scratch("closure-merge");
            let base = parcel(&two_dirs("a0", "b0"), &[]);
            write("main", None, &base).unwrap();

            // A lower that stopped part-way: the parcel arrived, a blob of its tree did not.
            let lowered = parcel(&two_dirs("a0", "b-lowered"), &[&base]);
            remove(&blob("b-lowered"));
            let merge = parcel(&two_dirs("a-merged", "b0"), &[&base, &lowered]);

            assert_incomplete(write("main", Some(&base), &merge), "main", Some(&base));
        }

        // Revert: refuse a head no ref reaches → a complete stray parcel is refused.
        #[test]
        fn a_complete_parcel_no_ref_reaches_is_accepted() {
            let (_root, _scope) = scratch("closure-stray");
            let base = parcel(&two_dirs("a0", "b0"), &[]);
            let head = parcel(&two_dirs("a1", "b1"), &[&base]);

            write("feature", None, &head).unwrap();
            assert_eq!(get_pallet_head("feature").unwrap(), Some(head));
        }

        // Revert: bound the walk by the written ref's own head only → the walk enters `other`'s
        // head, whose damage is not this write's, and refuses.
        #[test]
        fn the_walk_stops_at_every_ref_head() {
            let (_root, _scope) = scratch("closure-bounds");
            let theirs = parcel(&two_dirs("a0", "b-theirs"), &[]);
            write("other", None, &theirs).unwrap();
            let first = parcel(&two_dirs("a0", "b0"), &[&theirs]);
            let head = parcel(&two_dirs("a1", "b0"), &[&first]);
            remove(&blob("b-theirs"));
            remove(&blob("b0"));

            // `first` is walked and refused; once it is whole, `theirs` bounds the walk.
            assert_incomplete(write("feature", None, &head), "feature", None);
            blob("b0");
            write("feature", None, &head).unwrap();
        }

        // Revert: drop the base-tree prune → `web`, carried unchanged from the current head, is
        // walked and its damaged blob refuses a write that changed only `src`.
        #[test]
        fn a_subtree_the_current_head_carries_is_not_walked_again() {
            let (_root, _scope) = scratch("closure-prune");
            let base = parcel(&two_dirs("a0", "b0"), &[]);
            write("main", None, &base).unwrap();
            let head = parcel(&two_dirs("a1", "b0"), &[&base]);
            remove(&blob("b0"));

            write("main", Some(&base), &head).unwrap();
        }

        // Revert: verify at the full scope in a sparse warehouse → an out-of-scope blob that was
        // never fetched refuses the write; or skip the scoped walk → the in-scope one does not.
        #[test]
        fn a_sparse_warehouse_checks_only_what_its_scope_holds() {
            let (_root, _scope) = scratch("closure-sparse");
            crate::util::scope_utils::set_fetch_scope(&MaterializationScope::from_prefixes(["src"])).unwrap();
            let base = parcel(&two_dirs("a0", "b0"), &[]);
            write("main", None, &base).unwrap();

            let sealed = parcel(&two_dirs("a1", "b-sealed"), &[&base]);
            let part_way = parcel(&two_dirs("a-lost", "b-sealed"), &[&sealed]);
            remove(&blob("b-sealed"));
            write("main", Some(&base), &sealed).unwrap();

            remove(&blob("a-lost"));
            assert_incomplete(write("main", Some(&sealed), &part_way), "main", Some(&sealed));
        }

        // Revert: pass no bases to the in-scope walk below a spine tree → `src`, carried unchanged
        // from the current head, is walked and its damaged blob refuses a write that changed only
        // `web`, which is out of scope.
        #[test]
        fn a_sparse_warehouse_keeps_the_base_tree_prune() {
            let (_root, _scope) = scratch("closure-sparse-prune");
            crate::util::scope_utils::set_fetch_scope(&MaterializationScope::from_prefixes(["src"])).unwrap();
            let base = parcel(&two_dirs("a0", "b0"), &[]);
            write("main", None, &base).unwrap();
            let head = parcel(&two_dirs("a0", "b1"), &[&base]);
            remove(&blob("a0"));

            write("main", Some(&base), &head).unwrap();
        }

        // Revert: walk the tree recursively → the write overflows the small stack below and aborts.
        #[test]
        fn a_deeply_nested_tree_is_checked_without_overflowing_the_stack() {
            let (root, _scope) = scratch("closure-deep");
            let mut current = tree(&[("f", &blob("leaf"), DirEntryType::Normal)]);

            // Written without the store's durability steps, which would dominate the test.
            for _ in 0..2_000 {
                let mut level = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
                level.add_child(TreeItem::new("d".to_string(), current, DirEntryType::Tree));
                let mut object = LooseObjectBuilder::build_tree(&level);
                let (folder, file) = file_utils::get_path_for_object(&object.hash).unwrap();
                std::fs::create_dir_all(&folder).unwrap();
                std::fs::write(std::path::Path::new(&folder).join(file), object.compress().unwrap()).unwrap();
                current = object.hash;
            }
            let head = parcel(&current, &[]);

            let written = std::thread::Builder::new()
                .stack_size(256 * 1024)
                .spawn(move || {
                    let _scope = StorageRootScope::enter(&root);
                    write("main", None, &head)
                })
                .unwrap()
                .join()
                .expect("the write thread must not panic");
            written.unwrap();
        }

        // Revert: never re-read the scope inside the hold → the write lands over `web`, which
        // the widened scope holds and this warehouse lacks.
        #[test]
        fn a_scope_widened_between_the_check_and_the_hold_is_checked_again() {
            let (_root, _scope) = scratch("closure-widened");
            crate::util::scope_utils::set_fetch_scope(&MaterializationScope::from_prefixes(["src"])).unwrap();
            let base = parcel(&two_dirs("a0", "b0"), &[]);
            write("main", None, &base).unwrap();
            let head = parcel(&two_dirs("a1", "b-sealed"), &[&base]);
            remove(&blob("b-sealed"));

            BEFORE_HOLD.with(|slot| *slot.borrow_mut() = Some(Box::new(|| {
                crate::util::scope_utils::set_fetch_scope(&MaterializationScope::from_prefixes(["src", "web"])).unwrap();
            })));

            assert_incomplete(write("main", Some(&base), &head), "main", Some(&base));
        }

        // Revert: re-verify a head the caller verified → the write refuses although it was told
        // `head` was verified; or skip on any token → a token for another head or scope skips.
        #[test]
        fn a_verification_token_skips_the_check_only_for_its_own_head_and_scope() {
            let (_root, _scope) = scratch("closure-token");
            let missing = blob("lost");
            let head = parcel(&tree(&[("f", &missing, DirEntryType::Normal)]), &[]);
            let other = stub_parcel("other");
            remove(&missing);
            let full = MaterializationScope::full();

            let wrong_head = VerifiedClosure::new(&other, full.clone());
            assert_incomplete(update_pallet_head_verified(
                PalletNamespace::User, "main", None, &head, &RefPreconditions::NONE, &wrong_head, None
            ), "main", None);

            let wrong_scope = VerifiedClosure::new(&head, MaterializationScope::from_prefixes(["src"]));
            assert_incomplete(update_pallet_head_verified(
                PalletNamespace::User, "main", None, &head, &RefPreconditions::NONE, &wrong_scope, None
            ), "main", None);

            // The token stands for a verification its caller made: the write takes its word.
            let token = VerifiedClosure::new(&head, full);
            update_pallet_head_verified(PalletNamespace::User, "main", None, &head, &RefPreconditions::NONE, &token, None).unwrap();
        }

        // Revert: verify meta-pallet writes too → the office write walks a parcel it never needed.
        #[test]
        fn a_meta_pallet_write_is_not_checked_here() {
            let (_root, _scope) = scratch("closure-meta");
            update_meta_pallet_head(OFFICE, None, &"e".repeat(64), &RefPreconditions::NONE).unwrap();
        }
    }

    fn ref_bytes(name: &str) -> Vec<u8> {
        std::fs::read(get_pallet_ref_path(PalletNamespace::User, name)).unwrap()
    }

    const OFFICE: &str = crate::util::office_utils::OFFICE_PALLET_NAME;

    /// Plant a trust anchor; returns the trust file's bytes.
    fn plant_anchor(genesis: char) -> Vec<u8> {
        crate::util::office_utils::write_trust_anchor(&crate::util::office_utils::TrustAnchor {
            genesis: hash(genesis), enabled_at: 1, boundary: Vec::new(), prior_genesis: None, adopts: None,
        }).unwrap();

        crate::util::office_utils::read_trust_anchor_bytes().unwrap().unwrap()
    }

    fn office_at(head: &str) -> RefPreconditions {
        RefPreconditions { office: OfficePrecondition::At(Some(head.to_string())), ..RefPreconditions::NONE }
    }

    fn anchor_is(anchor: AnchorPrecondition) -> RefPreconditions {
        RefPreconditions { anchor, ..RefPreconditions::NONE }
    }

    fn within(boundary: &[&str], scope: BoundaryScope) -> RefPreconditions {
        RefPreconditions {
            pallets: PalletsPrecondition::WithinBoundary { boundary: boundary.iter().map(|h| h.to_string()).collect(), scope },
            ..RefPreconditions::NONE
        }
    }

    // Revert: drop the office comparison in `check_preconditions` → the write lands.
    #[test]
    fn a_write_judged_against_an_office_that_moved_is_refused_and_writes_nothing() {
        let (_root, _scope) = scratch("office-moved");
        update_meta_pallet_head(OFFICE, None, &hash('a'), &RefPreconditions::NONE).unwrap();
        update_pallet_head("main", None, &hash('1'), &RefPreconditions::NONE).unwrap();
        update_meta_pallet_head(OFFICE, Some(&hash('a')), &hash('b'), &RefPreconditions::NONE).unwrap();

        let refused = update_pallet_head("main", Some(&hash('1')), &hash('2'), &office_at(&hash('a')));

        assert_eq!(get_pallet_head("main").unwrap(), Some(hash('1')), "the target must be unchanged");
        assert_eq!(refused, Err(RefUpdateError::OfficeMoved { found: Some(hash('b')) }));
    }

    // Revert: skip the office check when the target is the office itself → the write lands.
    #[test]
    fn an_office_write_whose_office_precondition_disagrees_is_refused() {
        let (_root, _scope) = scratch("office-on-itself");
        update_meta_pallet_head(OFFICE, None, &hash('a'), &RefPreconditions::NONE).unwrap();

        let refused = update_meta_pallet_head(OFFICE, Some(&hash('a')), &hash('c'), &office_at(&hash('b')));

        assert_eq!(get_meta_pallet_head(OFFICE).unwrap(), Some(hash('a')), "the office must be unchanged");
        assert_eq!(refused, Err(RefUpdateError::OfficeMoved { found: Some(hash('a')) }));
    }

    // Revert: drop the anchor comparison in `check_preconditions` → the write lands.
    #[test]
    fn a_write_judged_against_an_anchor_that_was_replaced_is_refused() {
        let (root, _scope) = scratch("anchor-replaced");
        let read = plant_anchor('0');
        update_pallet_head("main", None, &hash('1'), &RefPreconditions::NONE).unwrap();
        std::fs::write(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT).join("trust"),
            crate::util::office_utils::encode_trust_anchor(&crate::util::office_utils::TrustAnchor {
                genesis: hash('9'), enabled_at: 2, boundary: Vec::new(), prior_genesis: Some(hash('0')), adopts: None,
            })).unwrap();

        let refused = update_pallet_head("main", Some(&hash('1')), &hash('2'), &anchor_is(AnchorPrecondition::Bytes(read)));

        assert_eq!(get_pallet_head("main").unwrap(), Some(hash('1')), "the target must be unchanged");
        assert_eq!(refused, Err(RefUpdateError::AnchorMoved));
    }

    // Revert: compare the decoded anchors rather than the bytes → the write lands.
    #[test]
    fn a_write_judged_against_an_anchor_respelled_in_other_bytes_is_refused() {
        let (root, _scope) = scratch("anchor-respelled");
        let read = plant_anchor('0');
        update_pallet_head("main", None, &hash('1'), &RefPreconditions::NONE).unwrap();
        let trust = root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT).join("trust");
        let mut respelled = read.clone();
        respelled.extend_from_slice(b"# respelled\n");
        std::fs::write(&trust, &respelled).unwrap();
        assert_eq!(crate::util::office_utils::read_trust_anchor().unwrap().map(|anchor| anchor.genesis), Some(hash('0')),
            "the respelled file must decode to the same anchor");

        let refused = update_pallet_head("main", Some(&hash('1')), &hash('2'), &anchor_is(AnchorPrecondition::Bytes(read)));

        assert_eq!(get_pallet_head("main").unwrap(), Some(hash('1')), "the target must be unchanged");
        assert_eq!(refused, Err(RefUpdateError::AnchorMoved));
    }

    // Revert: treat `Absent` as `NotConsumed` → an unsigned write lands under an anchor.
    #[test]
    fn a_write_judged_without_an_anchor_is_refused_once_one_exists() {
        let (_root, _scope) = scratch("anchor-appeared");
        update_pallet_head("main", None, &hash('1'), &RefPreconditions::NONE).unwrap();
        plant_anchor('0');

        let refused = update_pallet_head("main", Some(&hash('1')), &hash('2'), &anchor_is(AnchorPrecondition::Absent));

        assert_eq!(get_pallet_head("main").unwrap(), Some(hash('1')), "the target must be unchanged");
        assert_eq!(refused, Err(RefUpdateError::AnchorMoved));
    }

    // Guards against an always-refusing check.
    #[test]
    fn a_write_whose_preconditions_all_hold_lands() {
        let (_root, _scope) = scratch("preconditions-hold");
        let read = plant_anchor('0');
        update_meta_pallet_head(OFFICE, None, &hash('a'), &RefPreconditions::NONE).unwrap();
        update_pallet_head("main", None, &hash('1'), &RefPreconditions::NONE).unwrap();

        update_pallet_head("main", Some(&hash('1')), &hash('2'), &RefPreconditions {
            office: OfficePrecondition::At(Some(hash('a'))),
            anchor: AnchorPrecondition::Bytes(read),
            pallets: PalletsPrecondition::WithinBoundary { boundary: vec![hash('1')], scope: BoundaryScope::AllButOffice },
        }).unwrap();

        assert_eq!(get_pallet_head("main").unwrap(), Some(hash('2')));
    }

    // Revert: check the preconditions before the ref's own compare → the moved office is reported,
    // hiding that another bay already landed this very parcel.
    #[test]
    fn a_ref_already_moved_reports_the_move_before_a_precondition() {
        let (_root, _scope) = scratch("moved-before-precondition");
        update_meta_pallet_head(OFFICE, None, &hash('a'), &RefPreconditions::NONE).unwrap();
        update_pallet_head("main", None, &hash('1'), &RefPreconditions::NONE).unwrap();

        // Another bay lands the same parcel, then the office moves.
        update_pallet_head("main", Some(&hash('1')), &hash('2'), &RefPreconditions::NONE).unwrap();
        update_meta_pallet_head(OFFICE, Some(&hash('a')), &hash('b'), &RefPreconditions::NONE).unwrap();

        assert_eq!(update_pallet_head("main", Some(&hash('1')), &hash('2'), &office_at(&hash('a'))), Err(RefUpdateError::Moved(RefMoved {
            reference: PalletRef { namespace: PalletNamespace::User, name: "main".to_string() },
            expected: Some(hash('1')),
            found: Some(hash('2')),
        })));
    }

    // Revert: drop the boundary check → the first two land. Compare the boundary as a snapshot of
    // the heads rather than by membership → the last two are refused.
    #[test]
    fn a_boundary_refuses_a_moved_or_new_pallet_and_tolerates_a_deleted_one_or_an_extra_head() {
        let (_root, _scope) = scratch("boundary");
        update_pallet_head("main", None, &hash('1'), &RefPreconditions::NONE).unwrap();
        update_pallet_head("side", None, &hash('2'), &RefPreconditions::NONE).unwrap();
        update_meta_pallet_head(OFFICE, None, &hash('a'), &RefPreconditions::NONE).unwrap();
        let boundary = [hash('1'), hash('2')];
        let boundary: Vec<&str> = boundary.iter().map(String::as_str).collect();
        let write = |preconditions: &RefPreconditions| update_meta_pallet_head(OFFICE, Some(&hash('a')), &hash('b'), preconditions);

        update_pallet_head("main", Some(&hash('1')), &hash('3'), &RefPreconditions::NONE).unwrap();
        assert_eq!(write(&within(&boundary, BoundaryScope::UserPallets)), Err(RefUpdateError::PalletsMoved {
            pallet: PalletRef { namespace: PalletNamespace::User, name: "main".to_string() }, found: hash('3'),
        }), "a moved head");
        update_pallet_head("main", Some(&hash('3')), &hash('1'), &RefPreconditions::NONE).unwrap();

        update_pallet_head("new", None, &hash('4'), &RefPreconditions::NONE).unwrap();
        assert!(matches!(write(&within(&boundary, BoundaryScope::UserPallets)), Err(RefUpdateError::PalletsMoved { .. })),
            "a new pallet");
        assert_eq!(get_meta_pallet_head(OFFICE).unwrap(), Some(hash('a')), "a refusal writes nothing");
        std::fs::remove_file(get_pallet_ref_path(PalletNamespace::User, "new")).unwrap();

        std::fs::remove_file(get_pallet_ref_path(PalletNamespace::User, "side")).unwrap();
        let remote_only = hash('e');
        let extra = [boundary.as_slice(), &[remote_only.as_str()]].concat();
        assert_eq!(write(&within(&extra, BoundaryScope::UserPallets)), Ok(()),
            "a deleted pallet and a head only the remote holds stand inside the boundary");
    }

    // Revert: let `AllButOffice` read the user namespace only → a moved meta pallet passes.
    #[test]
    fn an_all_but_office_boundary_holds_meta_pallets_but_not_the_office() {
        let (_root, _scope) = scratch("boundary-meta");
        update_meta_pallet_head(OFFICE, None, &hash('a'), &RefPreconditions::NONE).unwrap();
        update_meta_pallet_head("kv", None, &hash('c'), &RefPreconditions::NONE).unwrap();
        let boundary = [hash('c')];
        let boundary: Vec<&str> = boundary.iter().map(String::as_str).collect();

        update_meta_pallet_head("kv", Some(&hash('c')), &hash('d'), &RefPreconditions::NONE).unwrap();
        let refused = update_pallet_head("main", None, &hash('1'), &within(&boundary, BoundaryScope::AllButOffice));

        assert!(matches!(&refused, Err(RefUpdateError::PalletsMoved { pallet, .. }) if pallet.name == "kv"), "{:?}", refused);
        assert_eq!(get_pallet_head("main").unwrap(), None);
    }

    /// A loose file whose name is respelled in uppercase resolves, by prefix, to its lowercase hash
    /// where reads open it under that hash, and is not a match where they do not. Matching the
    /// listed name, it was missed on a case-insensitive file system (`CD…` never starts with `cd`),
    /// and a digit-only prefix would have returned the listed spelling.
    ///
    /// Revert: match the listed name (skip `file_utils::canonical_loose_entry`) → red on macOS
    /// and Windows.
    #[test]
    fn a_prefix_resolves_a_respelled_loose_file_to_its_lowercase_hash_where_reads_reach_it() {
        let (root, _scope) = scratch("respelled-prefix");
        let hash = "ab".to_string() + &"cd".repeat(31);
        let (folder, file_name) = file_utils::get_path_for_object(&hash).unwrap();
        file_utils::write_object_to_file(Path::new(&folder), &file_name, b"bytes".to_vec()).unwrap();
        let listed = Path::new(&folder).join(&file_name);
        file_utils::rename(&listed, listed.with_file_name(file_name.to_ascii_uppercase())).unwrap();
        let folds = listed.exists();

        let resolved = resolve_object_hash_prefix("abcd");
        if folds {
            assert_eq!(resolved, Ok(hash.clone()));
        } else {
            assert!(resolved.is_err(), "no read opens it, so it is no match: {:?}", resolved);
        }

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_position_that_cannot_be_recorded_after_the_ref_moved_says_the_parcel_stands() {
        let (root, _scope) = scratch("record-landed");
        update_pallet_head("main", None, &hash('a'), &RefPreconditions::NONE).unwrap();

        // A non-empty directory where the position goes: the atomic rename onto it fails.
        std::fs::create_dir_all(get_position_path().join("blocker")).unwrap();

        let error = record_landed("main", &hash('a')).unwrap_err();
        assert!(error.contains(&format!("now holds parcel {}", hash('a'))) && error.contains("The parcel stands")
            && error.contains("forklift shift main"), "{}", error);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_write_log_records_only_its_own_recording_root() {
        let (root, _scope) = scratch("write-log");

        // Written before recording started: not logged.
        update_pallet_head("main", None, &hash('a'), &RefPreconditions::NONE).unwrap();

        start_write_log();
        update_pallet_head("main", Some(&hash('a')), &hash('b'), &RefPreconditions::NONE).unwrap();
        update_pallet_head("main", Some(&hash('b')), &hash('c'), &RefPreconditions::NONE).unwrap();
        update_meta_pallet_head("office", None, &hash('d'), &RefPreconditions::NONE).unwrap();

        // A refused write moved nothing, so it is not logged.
        assert!(update_pallet_head("main", Some(&hash('a')), &hash('e'), &RefPreconditions::NONE).is_err());

        {
            // Another warehouse, not recording: its writes go nowhere, and it reads back nothing.
            let (other, _other_scope) = scratch("write-log-other");
            update_pallet_head("main", None, &hash('f'), &RefPreconditions::NONE).unwrap();
            assert!(take_write_log().is_empty(), "a root that is not recording must read back nothing");
            let _ = std::fs::remove_dir_all(&other);
        }

        assert_eq!(take_write_log(), vec![
            ("main".to_string(), Some(hash('a')), Some(hash('b'))),
            ("main".to_string(), Some(hash('b')), Some(hash('c'))),
            ("@office".to_string(), None, Some(hash('d'))),
        ]);

        // Taking the log stopped recording.
        update_pallet_head("main", Some(&hash('c')), &hash('a'), &RefPreconditions::NONE).unwrap();
        assert!(take_write_log().is_empty(), "taking the log must stop recording");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_head_write_with_a_stale_expectation_is_refused_and_leaves_the_ref_unchanged() {
        let (root, _scope) = scratch("cas-mismatch");

        update_pallet_head("main", None, &hash('a'), &RefPreconditions::NONE).unwrap();
        let before = ref_bytes("main");

        match update_pallet_head("main", Some(&hash('b')), &hash('c'), &RefPreconditions::NONE) {
            Err(RefUpdateError::Moved(moved)) => {
                assert_eq!(moved.reference, PalletRef::user("main"));
                assert_eq!(moved.expected, Some(hash('b')));
                assert_eq!(moved.found, Some(hash('a')));
            }
            other => panic!("a stale expectation must be refused as moved: {:?}", other.err().map(|e| e.to_string())),
        }

        assert_eq!(ref_bytes("main"), before, "a refused write must leave the ref byte-identical");

        // Expecting no head against a ref that exists is a stale expectation too.
        assert!(matches!(update_pallet_head("main", None, &hash('c'), &RefPreconditions::NONE), Err(RefUpdateError::Moved(_))));
        assert_eq!(ref_bytes("main"), before);

        // A matching expectation writes.
        update_pallet_head("main", Some(&hash('a')), &hash('c'), &RefPreconditions::NONE).unwrap();
        assert_eq!(get_pallet_head("main").unwrap(), Some(hash('c')));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_head_write_waits_for_the_ref_lock() {
        let (root, _scope) = scratch("cas-lock");

        update_pallet_head("main", None, &hash('a'), &RefPreconditions::NONE).unwrap();

        let held = crate::util::lock_utils::RefLock::acquire().unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();

        let writer = {
            let root = root.clone();
            std::thread::spawn(move || {
                let _scope = StorageRootScope::enter(&root);
                let result = update_pallet_head("main", Some(&hash('a')), &hash('b'), &RefPreconditions::NONE).map_err(String::from);
                done_tx.send(()).unwrap();
                result
            })
        };

        // A bounded wait can only err toward passing: a writer that is not blocked finishes well
        // inside it.
        assert!(done_rx.recv_timeout(std::time::Duration::from_millis(500)).is_err(),
            "the write must wait while another holder has the ref lock");
        assert_eq!(get_pallet_head("main").unwrap(), Some(hash('a')));

        drop(held);
        writer.join().unwrap().unwrap();
        assert_eq!(get_pallet_head("main").unwrap(), Some(hash('b')));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn valid_pallet_names_are_accepted() {
        for name in ["main", "mainline", "feature/load-command", "v1.2", "a_b-c", "fix/fork-17", ".hidden", "a.b.c"] {
            assert!(validate_pallet_name(name).is_ok(), "expected valid: {}", name);
        }
    }

    #[test]
    fn invalid_pallet_names_are_rejected() {
        for name in ["", "/", "a/", "/a", "a//b", ".", "..", "a/../b", "-main", "fix/-x", "with space", "emoji📦", "new\nline", "@office"] {
            assert!(validate_pallet_name(name).is_err(), "expected invalid: {}", name);
        }
    }

    /// The portable grammar: no uppercase, no trailing `.`, no Windows device stem, in any
    /// component. Each refusal is pinned to its own reason, so a rule that stops firing cannot
    /// hide behind another one.
    #[test]
    fn names_that_are_not_portable_are_refused_for_their_own_reason() {
        let cases = [
            ("Main", "only lowercase ASCII letters"),
            ("feature/X", "only lowercase ASCII letters"),
            ("LPT1", "only lowercase ASCII letters"),
            ("main.", "must not end with \".\""),
            ("a./b", "must not end with \".\""),
            ("nul", "\"nul\" is a reserved device name"),
            ("con.txt", "\"con\" is a reserved device name"),
            ("com1.x", "\"com1\" is a reserved device name"),
            ("a/lpt9", "\"lpt9\" is a reserved device name"),
        ];

        for (name, reason) in cases {
            let error = validate_pallet_name(name).expect_err(name);
            assert!(error.contains(reason), "{}: expected the reason {:?}, got {}", name, reason, error);
        }
    }

    /// The other side of the device-name rule: only the exact stem before the first `.` is
    /// reserved, so longer or numbered-out names and a device name as an extension stay valid.
    #[test]
    fn names_that_only_resemble_a_device_name_are_accepted() {
        for name in ["console", "null", "com10", "com0", "lpt0", "aux_x", "x.con", "nul-x", "prnt", "a/con-b"] {
            assert!(validate_pallet_name(name).is_ok(), "expected valid: {}", name);
        }
    }

    /// The refusal suggests the lowercase spelling exactly when lowercasing alone makes the name
    /// valid; it never applies it.
    #[test]
    fn a_refusal_suggests_the_lowercase_spelling_only_when_that_alone_is_valid() {
        let error = validate_pallet_name("Feature/X").unwrap_err();
        assert!(error.contains("the lowercase \"feature/x\" is valid"), "{}", error);

        for name in ["MAIN.", "CON", "With Space"] {
            let error = validate_pallet_name(name).unwrap_err();
            assert!(!error.contains("the lowercase"), "{}: lowercasing does not make it valid: {}", name, error);
        }

        let error = validate_pallet_name("main.").unwrap_err();
        assert!(!error.contains("the lowercase"), "an already lowercase name gets no suggestion: {}", error);
    }

    #[test]
    fn every_built_in_pallet_name_is_valid_and_round_trips() {
        let built_in = [
            DEFAULT_PALLET_NAME,
            crate::util::office_utils::OFFICE_PALLET_NAME,
            crate::util::haul_utils::HAUL_PALLET_NAME,
            crate::util::kv_utils::KV_PALLET_NAME,
            crate::util::manifest_utils::MANIFEST_PALLET_NAME,
            crate::util::tag_utils::TAGS_PALLET_NAME,
        ];

        for name in built_in {
            assert!(validate_pallet_name(name).is_ok(), "built-in name refused: {}", name);

            for wire in [name.to_string(), format!("{}{}", META_QUALIFIER, name)] {
                assert_eq!(PalletRef::parse(&wire).unwrap().to_wire(), wire);
            }
        }

        for wire in ["feature/x", "v1.2", "@x.con"] {
            assert_eq!(PalletRef::parse(wire).unwrap().to_wire(), wire);
        }
    }

    /// A ref file under a name the grammar refuses is never listed as a pallet: the listing fails
    /// naming it. Valid neighbours that share a prefix list as before.
    #[test]
    fn a_ref_file_with_an_invalid_name_fails_the_listing_naming_it() {
        let (root, _scope) = scratch("invalid-ref-entry");
        update_pallet_head("main", None, &hash('a'), &RefPreconditions::NONE).unwrap();
        update_pallet_head("mainline", None, &hash('b'), &RefPreconditions::NONE).unwrap();
        assert_eq!(list_pallets_in(PalletNamespace::User).unwrap(), vec!["main", "mainline"]);

        let folder = get_pallets_folder().join("feature");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("X"), format!("{}\n", hash('c'))).unwrap();

        let error = list_pallets_in(PalletNamespace::User).unwrap_err();
        assert!(error.contains("\"feature/X\" is not a valid pallet name") && error.contains("does not name a valid pallet"),
            "{}", error);
        assert!(all_pallet_refs().is_err(), "the reachable-ref set must not skip the entry either");
        assert!(error.contains("Rename it to a valid pallet name"), "the way out is named: {}", error);

        // The named way out: renamed to a valid name, the entry lists as that pallet.
        file_utils::rename(folder.join("X"), folder.join("x2")).unwrap();
        assert_eq!(list_pallets_in(PalletNamespace::User).unwrap(), vec!["feature/x2", "main", "mainline"]);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The files a file browser leaves in any folder it opens are not ref files and never valid
    /// pallet names, so the listing passes over them at every level instead of failing on them.
    #[test]
    fn os_litter_in_the_pallets_folder_is_not_a_pallet() {
        let (root, _scope) = scratch("pallet-os-litter");
        update_pallet_head("feature/x", None, &hash('a'), &RefPreconditions::NONE).unwrap();

        let folder = get_pallets_folder();
        for litter in [folder.join(".DS_Store"), folder.join("feature").join(".DS_Store"), folder.join("Thumbs.db")] {
            std::fs::write(litter, b"\0\0\0\x01Bud1").unwrap();
        }

        assert_eq!(list_pallets_in(PalletNamespace::User).unwrap(), vec!["feature/x"]);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The temp an atomic ref write stages beside the ref is never a pallet name, in either
    /// namespace or under a nested name, and the recognizer the listing skips by accepts exactly
    /// that spelling.
    #[test]
    fn a_ref_write_temp_is_never_a_pallet_name() {
        let (root, _scope) = scratch("temp-spelling");
        for (namespace, name) in [(PalletNamespace::User, "main"), (PalletNamespace::User, "a-b/x"),
                                  (PalletNamespace::Meta, "office")] {
            let temp = file_utils::temp_path_for(&get_pallet_ref_path(namespace, name)).unwrap();
            let temp_name = temp.file_name().unwrap().to_str().unwrap().to_string();
            let sibling = match name.rsplit_once('/') {
                Some((parent, _)) => format!("{}/{}", parent, temp_name),
                None => temp_name.clone(),
            };
            assert!(validate_pallet_name(&sibling).is_err(), "a temp must never be a pallet name: {}", sibling);
            assert_eq!(file_utils::staged_temp_base(&temp_name), name.rsplit('/').next(),
                "the recognizer must accept {} as a temp of its ref", temp_name);
        }

        for not_a_temp in ["main", "main.tmp1-2", "main.tmp~", "main.tmp~1-", "main.tmp~-2", ".tmp~1-2",
                           "main.tmp~1-2x", "main.tmp~1"] {
            assert_eq!(file_utils::staged_temp_base(not_a_temp), None, "the recognizer must refuse {}", not_a_temp);
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// What a ref write killed between its temp write and its rename leaves beside the ref, at the
    /// top level and under a nested name, is neither listed as a pallet nor counted as a ref.
    #[test]
    fn a_killed_ref_write_leaves_no_pallet_behind() {
        let (root, _scope) = scratch("temp-litter");
        update_pallet_head("main", None, &hash('a'), &RefPreconditions::NONE).unwrap();
        update_pallet_head("a-b/x", None, &hash('b'), &RefPreconditions::NONE).unwrap();

        for name in ["main", "a-b/x"] {
            let temp = file_utils::temp_path_for(&get_pallet_ref_path(PalletNamespace::User, name)).unwrap();
            std::fs::write(&temp, format!("{}\n", hash('c'))).unwrap();
        }

        assert_eq!(list_pallets_in(PalletNamespace::User).unwrap(), vec!["a-b/x", "main"]);
        let refs = all_pallet_refs().unwrap();
        assert!(refs.iter().all(|(_, head)| *head != hash('c')), "a temp's head must not be a ref: {:?}", refs);
        assert_eq!(refs.len(), 2, "{:?}", refs);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A pallet whose name is a temp name under the old `.tmp<pid>-<n>` spelling survives a write
    /// to its sibling: the write's temp can no longer be that pallet's ref file. Other tests in this
    /// process draw from the same counter, so the victims cover a window of values, and an attempt
    /// counts only if the counter read after the write shows the write drew inside that window.
    #[test]
    fn a_ref_write_never_consumes_a_neighbouring_pallet() {
        let (root, _scope) = scratch("temp-clobber");
        update_pallet_head("x", None, &hash('a'), &RefPreconditions::NONE).unwrap();
        let next_write_id = || {
            let probe = file_utils::temp_path_for(&get_pallet_ref_path(PalletNamespace::User, "x")).unwrap();
            probe.file_name().unwrap().to_str().unwrap().rsplit_once('-').unwrap().1.parse::<u64>().unwrap() + 1
        };

        let mut certified = false;
        for attempt in 0..8u64 {
            let next = next_write_id();
            let victims: Vec<String> = (next..next + 64)
                .map(|n| format!("x.tmp{}-{}", std::process::id(), n)).collect();
            for victim in &victims {
                validate_pallet_name(victim).unwrap();
                std::fs::write(get_pallet_ref_path(PalletNamespace::User, victim), format!("{}\n", hash('c'))).unwrap();
            }

            let (old, new) = if attempt % 2 == 0 { (hash('a'), hash('b')) } else { (hash('b'), hash('a')) };
            update_pallet_head("x", Some(&old), &new, &RefPreconditions::NONE).unwrap();
            let in_window = next_write_id() <= next + 64;

            let listed = list_pallets_in(PalletNamespace::User).unwrap();
            for victim in &victims {
                assert!(listed.contains(victim), "the pallet {} must survive a write to x", victim);
                assert_eq!(get_pallet_head(victim).unwrap().as_deref(), Some(hash('c').as_str()));
                std::fs::remove_file(get_pallet_ref_path(PalletNamespace::User, victim)).unwrap();
            }
            assert_eq!(get_pallet_head("x").unwrap().as_deref(), Some(new.as_str()));

            if in_window {
                certified = true;
                break;
            }
        }
        assert!(certified, "the write never drew a counter value inside the victims' window");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The listing skips only the exact temp spelling of a valid ref name: a ref file whose name
    /// merely contains `~`, looks like a temp without being one, or is a temp of a name the
    /// grammar refuses, still fails the listing naming it.
    #[test]
    fn a_name_that_only_resembles_a_temp_still_fails_the_listing() {
        for impostor in ["x~", "main.tmp~1-", "Main.tmp~1-2", "-x.tmp~1-1", "bad name.tmp~3-4"] {
            let (root, _scope) = scratch("temp-impostor");
            update_pallet_head("main", None, &hash('a'), &RefPreconditions::NONE).unwrap();
            std::fs::write(get_pallets_folder().join(impostor), format!("{}\n", hash('c'))).unwrap();

            let error = list_pallets_in(PalletNamespace::User).unwrap_err();
            assert!(error.contains(&format!("\"{}\" is not a valid pallet name", impostor))
                && error.contains("does not name a valid pallet"), "{}", error);

            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn pallet_ref_parses_and_round_trips() {
        // Bare names are user pallets; the `@` qualifier selects the meta namespace.
        let user = PalletRef::parse("main").unwrap();
        assert_eq!(user, PalletRef::user("main"));
        assert_eq!(user.namespace, PalletNamespace::User);
        assert_eq!(user.to_wire(), "main");

        let meta = PalletRef::parse("@office").unwrap();
        assert_eq!(meta, PalletRef::meta("office"));
        assert_eq!(meta.namespace, PalletNamespace::Meta);
        assert_eq!(meta.to_wire(), "@office");

        // A subfoldered user name keeps its slashes and stays in the user namespace.
        let nested = PalletRef::parse("feature/x").unwrap();
        assert_eq!(nested.namespace, PalletNamespace::User);
        assert_eq!(nested.to_wire(), "feature/x");
    }

    #[test]
    fn pallet_ref_rejects_invalid_bare_names() {
        // The qualifier alone, or an otherwise invalid bare name, is not a valid ref.
        for reference in ["@", "@ ", "with space", "@a//b"] {
            assert!(PalletRef::parse(reference).is_err(), "expected invalid: {}", reference);
        }
    }

    /// The undo entry of deleting `name` at `head`.
    fn deleting(name: &str, head: &str) -> crate::util::journal_utils::JournalEntry {
        crate::util::journal_utils::delete_entry_for_test(name, head)
    }

    fn witness(pallet: &str, head: &str) -> RefPreconditions {
        RefPreconditions {
            office: OfficePrecondition::NotConsumed,
            anchor: AnchorPrecondition::NotConsumed,
            pallets: PalletsPrecondition::Witness { pallet: pallet.to_string(), head: head.to_string() },
        }
    }

    /// A parcel on `parent`, so a head on it descends from `parent`.
    fn child_of(parent: &str, tag: &str) -> String {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::model::tree_item::TreeItem;

        let mut tree = LooseObjectBuilder::build_tree(
            &TreeItem::new(String::new(), String::new(), crate::enums::dir_entry_type::DirEntryType::Tree)
        );
        tree.store().unwrap();

        let mut parcel = LooseObjectBuilder::build_parcel(&crate::model::parcel::Parcel {
            tree_hash: tree.hash.clone(),
            parents: vec![parent.to_string()],
            actions: Vec::new(),
            description: Some(tag.to_string()),
        });
        parcel.store().unwrap();
        parcel.hash
    }

    /// The tombstones a removal in a served root leaves, and the rule they put on re-creating the
    /// name.
    mod tombstones {
        use super::*;
        use std::collections::BTreeMap;
        use crate::util::tombstone_utils;

        #[test]
        fn a_removal_in_a_served_root_leaves_a_tombstone_and_one_in_an_unserved_root_none() {
            let (root, _scope) = scratch("tombstone-left");
            update_pallet_head("kept", None, &hash('a'), &RefPreconditions::NONE).unwrap();
            update_pallet_head("gone", None, &hash('b'), &RefPreconditions::NONE).unwrap();

            remove_pallet_head_in(PalletNamespace::User, "kept", &hash('a'), &RefPreconditions::NONE, Some(&deleting("kept", &hash('a')))).unwrap();
            assert!(tombstone_utils::read_tombstones().unwrap().is_empty(), "a root never served keeps no tombstones");

            assert!(tombstone_utils::mark_served().unwrap());
            remove_pallet_head_in(PalletNamespace::User, "gone", &hash('b'), &RefPreconditions::NONE, None).unwrap();

            assert_eq!(get_pallet_head("gone").unwrap(), None);
            assert_eq!(tombstone_utils::read_tombstones().unwrap(), BTreeMap::from([("gone".to_string(), hash('b'))]));
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn a_removal_with_no_undo_entry_is_refused_in_a_root_never_served() {
            let (root, _scope) = scratch("tombstone-unserved");
            update_pallet_head("gone", None, &hash('a'), &RefPreconditions::NONE).unwrap();

            let refused = remove_pallet_head_in(PalletNamespace::User, "gone", &hash('a'), &RefPreconditions::NONE, None).unwrap_err();

            assert!(refused.to_string().contains("never been served"), "{}", refused);
            assert_eq!(get_pallet_head("gone").unwrap(), Some(hash('a')), "nothing was removed");
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn a_refused_removal_leaves_the_tombstones_as_they_were() {
            let (root, _scope) = scratch("tombstone-refused");
            update_pallet_head("gone", None, &hash('a'), &RefPreconditions::NONE).unwrap();
            assert!(tombstone_utils::mark_served().unwrap());

            let refused = remove_pallet_head_in(PalletNamespace::User, "gone", &hash('b'), &RefPreconditions::NONE, None);

            assert!(matches!(refused, Err(RefUpdateError::Moved(_))), "{:?}", refused);
            assert!(tombstone_utils::read_tombstones().unwrap().is_empty());
            let _ = std::fs::remove_dir_all(&root);
        }

        /// A served root with pallet "gone" deleted at `hash('a')`.
        fn deleted_in_a_served_root(name: &str) -> (PathBuf, StorageRootScope) {
            let (root, scope) = scratch(name);
            update_pallet_head("gone", None, &hash('a'), &RefPreconditions::NONE).unwrap();
            assert!(tombstone_utils::mark_served().unwrap());
            remove_pallet_head_in(PalletNamespace::User, "gone", &hash('a'), &RefPreconditions::NONE, None).unwrap();

            (root, scope)
        }

        #[test]
        fn a_re_creation_whose_write_did_not_land_keeps_the_tombstone() {
            let (root, _scope) = deleted_in_a_served_root("tombstone-unlanded");
            let on_it = child_of(&hash('a'), "on it");
            std::fs::create_dir_all(get_pallet_ref_path(PalletNamespace::User, "gone")).unwrap();

            let refused = recreate_pallet_head("gone", &on_it, &RefPreconditions::NONE, &hash('a'));

            assert!(matches!(&refused, Err(RefUpdateError::Failed(_))), "{:?}", refused);
            assert_eq!(tombstone_utils::tombstone_heads().unwrap(), vec![hash('a')], "the tombstone stands");
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn creating_a_tombstoned_name_without_acknowledging_it_is_refused() {
            let (root, _scope) = deleted_in_a_served_root("tombstone-unacknowledged");
            let on_it = child_of(&hash('a'), "on it");

            let refused = update_pallet_head("gone", None, &on_it, &RefPreconditions::NONE).unwrap_err();

            assert!(matches!(&refused, RefUpdateError::Deleted { acknowledged: false, .. }), "{:?}", refused);
            assert!(refused.to_string().contains("forklift palletize --restore gone"), "{}", refused);
            assert_eq!(get_pallet_head("gone").unwrap(), None);
            assert_eq!(tombstone_utils::tombstone_heads().unwrap(), vec![hash('a')], "the tombstone stands");
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn an_acknowledged_creation_that_does_not_descend_from_the_tombstone_is_refused() {
            let (root, _scope) = deleted_in_a_served_root("tombstone-unrelated");

            let refused = recreate_pallet_head("gone", &hash('z'), &RefPreconditions::NONE, &hash('a')).unwrap_err();
            assert!(matches!(&refused, RefUpdateError::Deleted { acknowledged: true, .. }), "{:?}", refused);

            let refused = recreate_pallet_head("gone", &hash('a'), &RefPreconditions::NONE, &hash('z')).unwrap_err();
            assert!(matches!(&refused, RefUpdateError::Deleted { acknowledged: false, .. }), "another head acknowledges nothing: {:?}", refused);

            assert_eq!(get_pallet_head("gone").unwrap(), None);
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn an_acknowledged_creation_on_the_tombstone_lands_and_clears_it() {
            let (root, _scope) = deleted_in_a_served_root("tombstone-cleared");
            let on_it = child_of(&hash('a'), "on it");

            recreate_pallet_head("gone", &on_it, &RefPreconditions::NONE, &hash('a')).unwrap();

            assert_eq!(get_pallet_head("gone").unwrap(), Some(on_it));
            assert!(tombstone_utils::read_tombstones().unwrap().is_empty(), "the re-creation clears the tombstone");
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn a_name_with_no_tombstone_is_created_as_ever_in_a_served_root() {
            let (root, _scope) = deleted_in_a_served_root("tombstone-other-name");

            update_pallet_head("fresh", None, &hash('z'), &RefPreconditions::NONE).unwrap();

            assert_eq!(get_pallet_head("fresh").unwrap(), Some(hash('z')));
            assert_eq!(tombstone_utils::tombstone_heads().unwrap(), vec![hash('a')]);
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn a_restore_brings_the_pallet_back_at_its_tombstone_and_clears_it() {
            let (root, _scope) = deleted_in_a_served_root("tombstone-restore");
            write_position(&BayPosition::new("gone", Some(hash('z')))).unwrap();

            let refused = tombstone_utils::restore_pallet("gone").unwrap_err();
            assert!(refused.contains("out of step") && refused.contains("\"forklift palletize --restore gone\" again"), "{}", refused);
            assert_eq!(get_pallet_head("gone").unwrap(), None, "nothing was restored");

            // A bay at exactly the tombstone's head is put back in step by the restore.
            write_position(&BayPosition::new("gone", Some(hash('a')))).unwrap();
            assert_eq!(tombstone_utils::restore_pallet("gone").unwrap(), hash('a'));

            assert_eq!(get_pallet_head("gone").unwrap(), Some(hash('a')));
            assert!(tombstone_utils::read_tombstones().unwrap().is_empty());

            let refused = tombstone_utils::restore_pallet("gone").unwrap_err();
            assert!(refused.contains("No deleted pallet \"gone\""), "{}", refused);
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn removing_a_ref_unlinks_it_and_each_folder_its_nested_name_leaves_empty() {
        let (root, _scope) = scratch("remove-nested");
        update_pallet_head("team/web/x", None, &hash('a'), &RefPreconditions::NONE).unwrap();
        update_pallet_head("team/y", None, &hash('b'), &RefPreconditions::NONE).unwrap();
        let pallets = get_pallet_ref_path(PalletNamespace::User, "team").parent().unwrap().to_path_buf();

        remove_pallet_head_in(PalletNamespace::User, "team/web/x", &hash('a'), &RefPreconditions::NONE, Some(&deleting("team/web/x", &hash('a')))).unwrap();

        assert!(!pallets.join("team").join("web").exists(), "the emptied folder of a nested name goes with it");
        assert!(pallets.join("team").join("y").is_file(), "a folder still holding a ref stays");
        assert_eq!(get_pallet_head("team/web/x").unwrap(), None);

        remove_pallet_head_in(PalletNamespace::User, "team/y", &hash('b'), &RefPreconditions::NONE, Some(&deleting("team/y", &hash('b')))).unwrap();
        assert!(!pallets.join("team").exists());
        assert!(pallets.is_dir(), "the pallets folder itself is never removed");
        assert!(all_pallet_refs().unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_removal_is_logged_with_no_new_head() {
        let (root, _scope) = scratch("remove-log");
        update_pallet_head("gone", None, &hash('a'), &RefPreconditions::NONE).unwrap();
        start_write_log();

        remove_pallet_head_in(PalletNamespace::User, "gone", &hash('a'), &RefPreconditions::NONE, Some(&deleting("gone", &hash('a')))).unwrap();

        assert_eq!(take_write_log(), vec![("gone".to_string(), Some(hash('a')), None)]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_removal_refuses_a_meta_pallet_a_moved_ref_and_a_missing_one() {
        let (root, _scope) = scratch("remove-refusals");
        update_pallet_head("main", None, &hash('a'), &RefPreconditions::NONE).unwrap();
        update_meta_pallet_head("tags", None, &hash('b'), &RefPreconditions::NONE).unwrap();

        let meta = remove_pallet_head_in(PalletNamespace::Meta, "tags", &hash('b'), &RefPreconditions::NONE, Some(&deleting("tags", &hash('b'))));
        assert!(matches!(&meta, Err(RefUpdateError::Failed(message)) if message.contains("never removed")), "{:?}", meta);
        assert_eq!(get_meta_pallet_head("tags").unwrap(), Some(hash('b')));

        let moved = remove_pallet_head_in(PalletNamespace::User, "main", &hash('c'), &RefPreconditions::NONE, Some(&deleting("main", &hash('c'))));
        assert!(matches!(&moved, Err(RefUpdateError::Moved(m)) if m.found == Some(hash('a'))), "{:?}", moved);
        assert_eq!(get_pallet_head("main").unwrap(), Some(hash('a')));

        let missing = remove_pallet_head_in(PalletNamespace::User, "absent", &hash('a'), &RefPreconditions::NONE, Some(&deleting("absent", &hash('a'))));
        assert!(matches!(&missing, Err(RefUpdateError::Moved(m)) if m.found.is_none()), "{:?}", missing);

        // The entry is what roots the removed head, so one recording another removal is refused.
        for entry in [deleting("other", &hash('a')), deleting("main", &hash('c'))] {
            let unrecorded = remove_pallet_head_in(PalletNamespace::User, "main", &hash('a'), &RefPreconditions::NONE, Some(&entry));
            assert!(matches!(&unrecorded, Err(RefUpdateError::Failed(message)) if message.contains("does not record")),
                    "{:?}", unrecorded);
        }
        assert_eq!(get_pallet_head("main").unwrap(), Some(hash('a')));
        assert_eq!(crate::util::journal_utils::peek().unwrap(), None, "nothing is journaled");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_removal_refuses_inside_a_bulk_store_session() {
        let (root, _scope) = scratch("remove-bulk");
        update_pallet_head("main", None, &hash('a'), &RefPreconditions::NONE).unwrap();

        let session = file_utils::BulkStoreSession::open().unwrap();
        let refused = remove_pallet_head_in(PalletNamespace::User, "main", &hash('a'), &RefPreconditions::NONE, Some(&deleting("main", &hash('a'))));
        drop(session);

        assert!(matches!(&refused, Err(RefUpdateError::Failed(message)) if message.contains("bulk store session")), "{:?}", refused);
        assert_eq!(get_pallet_head("main").unwrap(), Some(hash('a')));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_witness_precondition_refuses_once_its_pallet_moved_and_lands_while_it_holds() {
        let (root, _scope) = scratch("remove-witness");
        update_pallet_head("victim", None, &hash('a'), &RefPreconditions::NONE).unwrap();
        update_pallet_head("holder", None, &hash('a'), &RefPreconditions::NONE).unwrap();
        update_pallet_head("holder", Some(&hash('a')), &hash('c'), &RefPreconditions::NONE).unwrap();

        let refused = remove_pallet_head_in(PalletNamespace::User, "victim", &hash('a'), &witness("holder", &hash('a')), Some(&deleting("victim", &hash('a'))));
        assert_eq!(refused, Err(RefUpdateError::WitnessMoved {
            pallet: "holder".to_string(), expected: hash('a'), found: Some(hash('c')),
        }));
        assert_eq!(get_pallet_head("victim").unwrap(), Some(hash('a')), "a refused removal writes nothing");

        // An absent witness is a moved one too.
        let gone = remove_pallet_head_in(PalletNamespace::User, "victim", &hash('a'), &witness("nobody", &hash('a')), Some(&deleting("victim", &hash('a'))));
        assert!(matches!(gone, Err(RefUpdateError::WitnessMoved { found: None, .. })));

        // The same precondition guards an ordinary write.
        let write = update_pallet_head("victim", Some(&hash('a')), &hash('d'), &witness("holder", &hash('a')));
        assert!(matches!(write, Err(RefUpdateError::WitnessMoved { .. })));

        remove_pallet_head_in(PalletNamespace::User, "victim", &hash('a'), &witness("holder", &hash('c')), Some(&deleting("victim", &hash('a')))).unwrap();
        assert_eq!(get_pallet_head("victim").unwrap(), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_sparse_warehouse_records_the_removed_head_in_the_fetched_heads_ledger() {
        let (root, _scope) = scratch("remove-ledger");
        update_pallet_head("full", None, &hash('a'), &RefPreconditions::NONE).unwrap();
        update_pallet_head("sparse", None, &hash('b'), &RefPreconditions::NONE).unwrap();

        remove_pallet_head_in(PalletNamespace::User, "full", &hash('a'), &RefPreconditions::NONE, Some(&deleting("full", &hash('a')))).unwrap();
        assert!(crate::util::fetch_ledger::read_ledger().unwrap().is_empty(), "a full warehouse keeps no ledger");

        crate::util::scope_utils::set_fetch_scope(&crate::util::scope_utils::MaterializationScope::from_prefixes(["src"])).unwrap();
        remove_pallet_head_in(PalletNamespace::User, "sparse", &hash('b'), &RefPreconditions::NONE, Some(&deleting("sparse", &hash('b')))).unwrap();

        let ledger = crate::util::fetch_ledger::read_ledger().unwrap();
        assert!(ledger.iter().any(|entry| entry.head == hash('b') && entry.pallet == "sparse"), "{:?}", ledger);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_folder_where_a_ref_must_go_refuses_the_create_and_never_reads_as_a_pallet() {
        let (root, _scope) = scratch("folder-in-the-way");
        update_pallet_head("team/x", None, &hash('a'), &RefPreconditions::NONE).unwrap();

        // "team" is a folder holding another ref: it reads as no pallet, never as an unborn one
        // that shadows the folder, and creating it refuses naming the folder.
        assert!(!does_pallet_exist("team"));
        assert_eq!(all_pallet_refs().unwrap(), vec![(PalletRef::user("team/x"), hash('a'))]);

        let refused = update_pallet_head("team", None, &hash('b'), &RefPreconditions::NONE);
        assert!(matches!(&refused, Err(RefUpdateError::Failed(message)) if message.contains("a folder stands")), "{:?}", refused);
        assert!(get_pallet_ref_path(PalletNamespace::User, "team/x").is_file(), "the folder's ref is untouched");

        // So does an empty one, left by a removal that could not take its folder.
        remove_pallet_head_in(PalletNamespace::User, "team/x", &hash('a'), &RefPreconditions::NONE, Some(&deleting("team/x", &hash('a')))).unwrap();
        std::fs::create_dir_all(get_pallet_ref_path(PalletNamespace::User, "team")).unwrap();
        assert!(all_pallet_refs().unwrap().is_empty());
        assert!(matches!(update_pallet_head("team", None, &hash('b'), &RefPreconditions::NONE),
                         Err(RefUpdateError::Failed(message)) if message.contains("a folder stands")));

        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Resolve a revision argument to a parcel hash: the name of an existing pallet (its
/// head), a full parcel hash, or a unique parcel-hash prefix of at least 4 characters.
/// A pallet name always wins over a hash-looking string.
///
/// # Arguments
/// * `arg` - The revision argument.
///
/// # Returns
/// * `Ok(String)`  - The resolved parcel hash.
/// * `Err(String)` - If the argument matches nothing (or matches ambiguously), the
///                   pallet is unborn, or the resolved object is not a parcel.
pub fn resolve_revision(arg: &str) -> Result<String, String> {
    // A `@`-qualified reference names a meta pallet (e.g. `@office`) — never a hash.
    if let Some(name) = arg.strip_prefix(META_QUALIFIER) {
        return get_meta_pallet_head(name)?
            .ok_or(format!("Meta pallet \"{}\" has nothing stacked yet.", arg));
    }

    if does_pallet_exist(arg) {
        return get_pallet_head(arg)?
            .ok_or(format!("Pallet \"{}\" has nothing stacked yet.", arg));
    }

    let looks_like_hash = arg.len() >= 4 && arg.bytes().all(|b| b.is_ascii_hexdigit());

    if !looks_like_hash {
        return Err(format!(
            "\"{}\" is neither a pallet nor a parcel hash (hash prefixes need at least \
            4 characters).",
            arg
        ));
    }

    let hash = resolve_object_hash_prefix(arg)?;

    // The resolved object must actually be a parcel; `load_parcel` reports the actual
    // type when it is not.
    crate::util::object_utils::load_parcel(&hash)?;

    Ok(hash)
}

/// Resolve a (possibly partial) object hash to the full hash of a stored object.
///
/// # Arguments
/// * `prefix` - The full hash or a prefix of at least 4 hex characters.
///
/// # Returns
/// * `Ok(String)`  - The full hash of the single matching object.
/// * `Err(String)` - If no object matches, or more than one does.
fn resolve_object_hash_prefix(prefix: &str) -> Result<String, String> {
    // Hashes are stored and compared in lowercase. On a case-insensitive filesystem an uppercase
    // prefix still finds the fan-out folder, and the user's casing would end up in the result.
    let prefix = prefix.to_ascii_lowercase();
    let prefix = prefix.as_str();
    let mut matches: Vec<String> = Vec::new();

    // Loose objects: scan the fan-out folder for matching file names.
    let folder = Path::new(&file_utils::get_path_objects_root())
        .join(&prefix[0..2]);
    let rest = &prefix[2..];

    if let Ok(entries) = std::fs::read_dir(&folder) {
        for entry in entries.flatten() {
            let Ok(file_name) = entry.file_name().into_string() else {
                continue;
            };

            // Sidecar files (parcel signatures) carry a dotted suffix and are not objects.
            if file_name.contains('.') {
                continue;
            }

            // A name spelled other than in lowercase is matched under the spelling a read opens
            // it by, or not at all, so a match is always a hash the store answers for.
            let Some((folder_name, file_name)) = file_utils::canonical_loose_entry(&folder, &file_name) else {
                continue;
            };

            if file_name.starts_with(rest) {
                matches.push(format!("{}{}", folder_name, file_name));
            }
        }
    }

    // Packed objects: an object referenced by hash must still resolve after `compact` moved
    // it out of the loose store.
    matches.extend(crate::util::pack_utils::find_hashes_with_prefix(prefix)?);

    // An object can momentarily be both loose and packed; count it once.
    matches.sort();
    matches.dedup();

    match matches.as_slice() {
        [] => Err(format!("No object with hash \"{}\" exists.", prefix)),
        [hash] => Ok(hash.clone()),
        _ => Err(format!(
            "The hash prefix \"{}\" is ambiguous ({} objects match); use more characters.",
            prefix,
            matches.len()
        )),
    }
}
