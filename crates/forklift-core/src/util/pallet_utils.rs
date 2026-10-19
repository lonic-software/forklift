use std::path::{Path, PathBuf};
use crate::globals::{bay_root, forklift_root};
use crate::util::{file_utils, object_utils};

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

/// Validate a pallet name.
///
/// Names may consist of `/`-separated components of ASCII letters, digits, `.`, `_`
/// and `-`. Components must not be empty, must not be `.` or `..`, and must not start
/// with a `-` (such a name would be indistinguishable from a flag on the command line).
///
/// # Arguments
/// * `name` - The pallet name to validate.
///
/// # Returns
/// * `Ok(())`      - If the name is valid.
/// * `Err(String)` - If the name is not valid.
pub fn validate_pallet_name(name: &str) -> Result<(), String> {
    let error = |reason: &str| Err(format!("\"{}\" is not a valid pallet name: {}", name, reason));

    if name.is_empty() {
        return error("it is empty");
    }

    for component in name.split('/') {
        if component.is_empty() {
            return error("it contains an empty path component");
        }

        if component == "." || component == ".." {
            return error("\".\" and \"..\" are not allowed as path components");
        }

        if component.starts_with('-') {
            return error("path components must not start with \"-\"");
        }

        let is_valid_char = |c: char| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-';

        if !component.chars().all(is_valid_char) {
            return error("only ASCII letters, digits, \".\", \"_\", \"-\" and \"/\" are allowed");
        }
    }

    Ok(())
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

        Err(behind_message(&self.pallet, self.base.as_deref(), head.as_deref(), operation))
    }
}

/// Why a bay standing on `base` may not move `pallet`, whose head is `head`: see
/// [`BayPosition::require_current`].
pub fn behind_message(pallet: &str, base: Option<&str>, head: Option<&str>, operation: &str) -> String {
    format!(
        "This bay is behind pallet \"{}\": its files stand on {}, but the pallet is at {}, moved \
        since by another bay or process. Nothing was {}. Run \"forklift shift {}\" to catch up, \
        then try again.",
        pallet,
        base.map(|base| format!("parcel {}", base)).unwrap_or("no parcel".to_string()),
        head.map(|head| format!("parcel {}", head)).unwrap_or("no parcel".to_string()),
        operation,
        pallet
    )
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

    /// Anything else: an invalid name or value, or the ref could not be read or written.
    Failed(String),
}

impl std::fmt::Display for RefUpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefUpdateError::Moved(moved) => moved.fmt(f),
            RefUpdateError::Failed(message) => f.write_str(message),
        }
    }
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
///
/// # Returns
/// * `Ok(())`                - The head was moved.
/// * `Err(RefUpdateError)`   - The ref moved under the caller, or the write failed.
pub fn update_pallet_head(name: &str, expected: Option<&str>, new: &str) -> Result<(), RefUpdateError> {
    update_pallet_head_in(PalletNamespace::User, name, expected, new)
}

/// Move the head of the given meta pallet from `expected` to `new` (see
/// [`update_pallet_head_in`]).
///
/// # Arguments
/// * `name`     - The name of the meta pallet. Must be a valid pallet name.
/// * `expected` - The head the caller's work was based on (`None`: the pallet is unborn).
/// * `new`      - The hash of the new head parcel.
///
/// # Returns
/// * `Ok(())`                - The head was moved.
/// * `Err(RefUpdateError)`   - The ref moved under the caller, or the write failed.
pub fn update_meta_pallet_head(name: &str, expected: Option<&str>, new: &str) -> Result<(), RefUpdateError> {
    update_pallet_head_in(PalletNamespace::Meta, name, expected, new)
}

/// Move the head of a pallet from `expected` to `new`: a compare-and-set, and the only way a
/// pallet ref is written. Under the warehouse's [`RefLock`](crate::util::lock_utils::RefLock),
/// which every bay shares, the head is read; when it is not `expected` nothing is written and
/// the write is refused with [`RefMoved`]. Otherwise the ref file is replaced atomically, created
/// (with any subfolders a `/`-separated name needs) when the pallet is unborn.
///
/// `expected` is the head the caller's work was based on, read earlier in the same command, so a
/// move by another bay or process anywhere between that read and this write is refused rather
/// than overwritten.
///
/// # Arguments
/// * `namespace` - The pallet namespace.
/// * `name`      - The name of the pallet. Must be a valid pallet name.
/// * `expected`  - The head the ref must hold (`None`: the ref must not exist).
/// * `new`       - The hash of the new head parcel.
///
/// # Returns
/// * `Ok(())`                         - The head was moved.
/// * `Err(RefUpdateError::Moved)`     - The ref did not hold `expected`; nothing was written.
/// * `Err(RefUpdateError::Failed)`    - The name is invalid, `new` is not an object hash, or the
///                                      ref could not be locked, read or written.
pub fn update_pallet_head_in(
    namespace: PalletNamespace,
    name: &str,
    expected: Option<&str>,
    new: &str,
) -> Result<(), RefUpdateError> {
    validate_pallet_name(name)?;

    // The writer holds the reader's rule: a ref `get_pallet_head_in` would refuse is never stored.
    if !object_utils::is_object_hash(new) {
        return Err(RefUpdateError::Failed(format!(
            "Refusing to set the head of pallet \"{}\": the value is not an object hash (64 \
            lowercase hex characters).",
            name
        )));
    }

    let _lock = crate::util::lock_utils::RefLock::acquire()?;

    let found = get_pallet_head_in(namespace, name)?;

    if found.as_deref() != expected {
        return Err(RefUpdateError::Moved(RefMoved {
            reference: PalletRef { namespace, name: name.to_string() },
            expected: expected.map(str::to_string),
            found,
        }));
    }

    let path = get_pallet_ref_path(namespace, name);

    if let Some(parent) = path.parent() {
        file_utils::create_folder_if_not_exists(parent)?;
    }

    let written = file_utils::write_file_atomically(&path, format!("{}\n", new).as_bytes());

    // A failed write can still have landed (the rename went through, the directory sync did not);
    // the log records what the ref now holds, so it is read back rather than inferred.
    if written.is_ok() || get_pallet_head_in(namespace, name).ok().flatten().as_deref() == Some(new) {
        log_write(PalletRef { namespace, name: name.to_string() }.to_wire(), found, new);
    }

    written?;

    Ok(())
}

/// One head write [`update_pallet_head_in`] made: the ref's wire form, the head it replaced
/// (`None` for a ref it created) and the head it wrote.
pub type LoggedWrite = (String, Option<String>, String);

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

fn log_write(reference: String, old: Option<String>, new: &str) {
    if let Some(entries) = write_log().get_mut(&forklift_root()) {
        entries.push((reference, old, new.to_string()));
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
/// * `Err(String)` - If a folder or entry could not be read.
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
        } else {
            names.push(full_name);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::StorageRootScope;

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
        c.to_string().repeat(64)
    }

    fn ref_bytes(name: &str) -> Vec<u8> {
        std::fs::read(get_pallet_ref_path(PalletNamespace::User, name)).unwrap()
    }

    #[test]
    fn the_write_log_records_only_its_own_recording_root() {
        let (root, _scope) = scratch("write-log");

        // Written before recording started: not logged.
        update_pallet_head("main", None, &hash('a')).unwrap();

        start_write_log();
        update_pallet_head("main", Some(&hash('a')), &hash('b')).unwrap();
        update_pallet_head("main", Some(&hash('b')), &hash('c')).unwrap();
        update_meta_pallet_head("office", None, &hash('d')).unwrap();

        // A refused write moved nothing, so it is not logged.
        assert!(update_pallet_head("main", Some(&hash('a')), &hash('e')).is_err());

        {
            // Another warehouse, not recording: its writes go nowhere, and it reads back nothing.
            let (other, _other_scope) = scratch("write-log-other");
            update_pallet_head("main", None, &hash('f')).unwrap();
            assert!(take_write_log().is_empty(), "a root that is not recording must read back nothing");
            let _ = std::fs::remove_dir_all(&other);
        }

        assert_eq!(take_write_log(), vec![
            ("main".to_string(), Some(hash('a')), hash('b')),
            ("main".to_string(), Some(hash('b')), hash('c')),
            ("@office".to_string(), None, hash('d')),
        ]);

        // Taking the log stopped recording.
        update_pallet_head("main", Some(&hash('c')), &hash('a')).unwrap();
        assert!(take_write_log().is_empty(), "taking the log must stop recording");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_head_write_with_a_stale_expectation_is_refused_and_leaves_the_ref_unchanged() {
        let (root, _scope) = scratch("cas-mismatch");

        update_pallet_head("main", None, &hash('a')).unwrap();
        let before = ref_bytes("main");

        match update_pallet_head("main", Some(&hash('b')), &hash('c')) {
            Err(RefUpdateError::Moved(moved)) => {
                assert_eq!(moved.reference, PalletRef::user("main"));
                assert_eq!(moved.expected, Some(hash('b')));
                assert_eq!(moved.found, Some(hash('a')));
            }
            other => panic!("a stale expectation must be refused as moved: {:?}", other.err().map(|e| e.to_string())),
        }

        assert_eq!(ref_bytes("main"), before, "a refused write must leave the ref byte-identical");

        // Expecting no head against a ref that exists is a stale expectation too.
        assert!(matches!(update_pallet_head("main", None, &hash('c')), Err(RefUpdateError::Moved(_))));
        assert_eq!(ref_bytes("main"), before);

        // A matching expectation writes.
        update_pallet_head("main", Some(&hash('a')), &hash('c')).unwrap();
        assert_eq!(get_pallet_head("main").unwrap(), Some(hash('c')));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_head_write_waits_for_the_ref_lock() {
        let (root, _scope) = scratch("cas-lock");

        update_pallet_head("main", None, &hash('a')).unwrap();

        let held = crate::util::lock_utils::RefLock::acquire().unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();

        let writer = {
            let root = root.clone();
            std::thread::spawn(move || {
                let _scope = StorageRootScope::enter(&root);
                let result = update_pallet_head("main", Some(&hash('a')), &hash('b')).map_err(String::from);
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
        for name in ["main", "feature/load-command", "v1.2", "a_b-c", "fix/FORK-17"] {
            assert!(validate_pallet_name(name).is_ok(), "expected valid: {}", name);
        }
    }

    #[test]
    fn invalid_pallet_names_are_rejected() {
        for name in ["", "/", "a/", "/a", "a//b", ".", "..", "a/../b", "-main", "fix/-x", "with space", "emoji📦", "new\nline", "@office"] {
            assert!(validate_pallet_name(name).is_err(), "expected invalid: {}", name);
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

            if file_name.starts_with(rest) {
                matches.push(format!("{}{}", &prefix[0..2], file_name));
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
