use std::path::{Path, PathBuf};
use crate::globals::{bay_root, forklift_root};
use crate::util::file_utils;

/// The folder inside the forklift root that holds the *user* pallet ref files (branches).
/// Each pallet is a file named after the pallet, containing the hash of its head parcel
/// (followed by a new line). Pallet names may contain `/`, which maps to subfolders.
const FOLDER_NAME_PALLETS_ROOT: &str = "pallets";

/// The folder inside the forklift root that holds the *meta* pallet ref files: real
/// pallets (they hash, sign and transport like any other) that carry tracked metadata
/// rather than working-directory content — the office (users + keys) today, post-metadata
/// and provenance later. Kept in their own namespace so no user pallet
/// name is ever reserved (DESIGN.html §3.3). Mirrors git's `refs/heads/*` vs `refs/notes/*`.
const FOLDER_NAME_META_ROOT: &str = "meta";

/// The character that qualifies a meta pallet in a revision/wire reference (`@office`).
/// A bare name is always a user pallet; user names can never start with `@`
/// (see [`validate_pallet_name`]), so the qualifier is unambiguous.
pub const META_QUALIFIER: char = '@';

/// The file inside the forklift root that holds the name of the current pallet
/// (the pallet new parcels are stacked on — git's HEAD equivalent).
const FILE_NAME_CURRENT_PALLET: &str = "pallet";

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

/// Get the path of the current-pallet file.
///
/// # Returns
/// * `PathBuf` - The path of the current-pallet file (relative to the warehouse root).
fn get_current_pallet_path() -> PathBuf {
    // The current pallet is bay-local: each bay is checked out to its own pallet.
    bay_root().join(FILE_NAME_CURRENT_PALLET)
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
fn get_pallet_ref_path(namespace: PalletNamespace, name: &str) -> PathBuf {
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

/// Get the name of the current pallet. Warehouses prepared before pallets existed have no
/// current-pallet file; they default to the default pallet name.
///
/// # Returns
/// * `Ok(String)`  - The name of the current pallet.
/// * `Err(String)` - If the current-pallet file exists but could not be read (or holds an
///                   invalid name).
pub fn get_current_pallet_name() -> Result<String, String> {
    let path = get_current_pallet_path();

    if !path.exists() {
        return Ok(DEFAULT_PALLET_NAME.to_string());
    }

    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Error while reading the current pallet file \"{}\": {}", path.to_string_lossy(), e))?;
    let name = content.trim_end_matches('\n').to_string();

    validate_pallet_name(&name)?;

    Ok(name)
}

/// Set the name of the current pallet (atomically).
///
/// # Arguments
/// * `name` - The name of the pallet to make current. Must be a valid pallet name.
///
/// # Returns
/// * `Ok(())`      - If the current pallet was set.
/// * `Err(String)` - If the name is invalid or the file could not be written.
pub fn set_current_pallet_name(name: &str) -> Result<(), String> {
    validate_pallet_name(name)?;

    file_utils::write_file_atomically(&get_current_pallet_path(), format!("{}\n", name).as_bytes())
}

/// Create the current-pallet file (pointing at the default pallet) if it does not exist,
/// along with the pallets folder. Called by `prepare`.
///
/// # Returns
/// * `Ok(true)`    - If the current-pallet file was created.
/// * `Ok(false)`   - If it already existed.
/// * `Err(String)` - If an error occurred.
pub fn create_current_pallet_file_if_not_exists() -> Result<bool, String> {
    file_utils::create_folder_if_not_exists(&get_pallets_folder())?;

    if get_current_pallet_path().exists() {
        return Ok(false);
    }

    set_current_pallet_name(DEFAULT_PALLET_NAME)?;

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

    let is_valid_hash = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());

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

/// Set the head parcel hash of the given pallet (atomically). Creates the ref file (and
/// any subfolders a `/`-separated name needs) when the pallet is unborn.
///
/// # Arguments
/// * `name` - The name of the pallet. Must be a valid pallet name.
/// * `hash` - The hash of the new head parcel.
///
/// # Returns
/// * `Ok(())`      - If the head was updated.
/// * `Err(String)` - If the name is invalid or the ref file could not be written.
pub fn set_pallet_head(name: &str, hash: &str) -> Result<(), String> {
    set_pallet_head_in(PalletNamespace::User, name, hash)
}

/// Set the head parcel hash of the given meta pallet (atomically).
///
/// # Arguments
/// * `name` - The name of the meta pallet. Must be a valid pallet name.
/// * `hash` - The hash of the new head parcel.
///
/// # Returns
/// * `Ok(())`      - If the head was updated.
/// * `Err(String)` - If the name is invalid or the ref file could not be written.
pub fn set_meta_pallet_head(name: &str, hash: &str) -> Result<(), String> {
    set_pallet_head_in(PalletNamespace::Meta, name, hash)
}

/// Set the head parcel hash of a pallet in the given namespace (atomically). Creates the
/// ref file (and any subfolders a `/`-separated name needs) when the pallet is unborn.
///
/// # Arguments
/// * `namespace` - The pallet namespace.
/// * `name`      - The name of the pallet. Must be a valid pallet name.
/// * `hash`      - The hash of the new head parcel.
///
/// # Returns
/// * `Ok(())`      - If the head was updated.
/// * `Err(String)` - If the name is invalid or the ref file could not be written.
pub fn set_pallet_head_in(namespace: PalletNamespace, name: &str, hash: &str) -> Result<(), String> {
    validate_pallet_name(name)?;

    let path = get_pallet_ref_path(namespace, name);

    if let Some(parent) = path.parent() {
        file_utils::create_folder_if_not_exists(parent)?;
    }

    file_utils::write_file_atomically(&path, format!("{}\n", hash).as_bytes())
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

            // Sidecar files (tree metadata, parcel signatures) carry a dotted suffix and are
            // not objects.
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
