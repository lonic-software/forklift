use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use crate::globals::{self, forklift_root, FOLDER_NAME_FORKLIFT_ROOT};
use crate::util::{alias_utils, bay_utils, config_utils, file_utils, pallet_utils};

/// The directory the user invoked forklift from, relative to the warehouse root.
/// Set once by [`enter_warehouse`]; empty when the user is at the root itself.
static CWD_RELATIVE_TO_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Find the root folder of the warehouse by walking up from the current directory
/// until a folder containing the forklift root folder is found.
///
/// # Returns
/// * `Ok(PathBuf)`  - The path of the warehouse root.
/// * `Err(String)`  - If the current directory is not inside a warehouse.
pub fn find_warehouse_root() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir()
        .map_err(|e| format!("Error while getting the current directory: {}", e))?;

    let mut dir = cwd.as_path();

    loop {
        // A warehouse and a bay (§7.5) both mark the root of their working tree with a
        // `.forklift` folder; `enter_warehouse` tells them apart.
        if dir.join(FOLDER_NAME_FORKLIFT_ROOT).exists() {
            return Ok(dir.to_path_buf());
        }

        match dir.parent() {
            Some(parent) => dir = parent,
            None => return Err(format!(
                "Not a forklift warehouse (no \"{}\" found in this directory or any of \
                its parents). Use the \"prepare\" command to create a warehouse.",
                FOLDER_NAME_FORKLIFT_ROOT
            )),
        }
    }
}

/// Whether `dir` is the root of a working tree: it holds a `.forklift` entry (a warehouse's or a
/// bay's folder, or an old-format bay file), which [`find_warehouse_root`] stops at from anywhere inside it. Below
/// the warehouse's own root, such a directory is another working tree, never content of this one.
///
/// Stricter than discovery, which follows a symlink: an entry of any kind counts, so a dangling
/// link is not taken for an ordinary directory either.
///
/// # Arguments
/// * `dir` - The directory, relative to the current directory or absolute.
///
/// # Returns
/// * `Ok(bool)`    - Whether `dir` holds a `.forklift` entry (`false` when `dir` does not exist or
///                   is not a directory).
/// * `Err(String)` - If the entry could not be read.
pub fn is_working_tree_root(dir: &Path) -> Result<bool, String> {
    use std::io::ErrorKind;

    let marker = dir.join(FOLDER_NAME_FORKLIFT_ROOT);

    match std::fs::symlink_metadata(&marker) {
        Ok(_) => Ok(true),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => Ok(false),
        Err(e) => Err(format!("Error while checking \"{}\": {}", marker.to_string_lossy(), e)),
    }
}

/// Discover the warehouse root, remember where the process was invoked from (relative to
/// the root), and switch the working directory to the root, so that all warehouse paths
/// (e.g. the object store) resolve correctly regardless of where forklift was invoked.
///
/// # Returns
/// * `Ok(())`      - If the warehouse was entered successfully.
/// * `Err(String)` - If no warehouse was found or the directory switch failed.
pub fn enter_warehouse() -> Result<(), String> {
    let cwd = std::env::current_dir()
        .map_err(|e| format!("Error while getting the current directory: {}", e))?;
    let root = find_warehouse_root()?;
    let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);

    // Inside a bay, the `.forklift` folder redirects to the shared warehouse: shared storage
    // (objects, refs, trust, config) resolves there, while the working directory, the
    // inventory, the current pallet, the lock and the probes of this bay's volume stay here.
    match bay_utils::classify_forklift(&forklift)? {
        bay_utils::ForkliftKind::Bay => {
            let redirect = bay_utils::read_bay_redirect(&forklift)?;
            globals::set_bay_context(redirect.warehouse_root.join(FOLDER_NAME_FORKLIFT_ROOT), forklift, redirect.name);
        }
        bay_utils::ForkliftKind::OldFormatBay => return Err(bay_utils::old_format_bay_error(&root)),
        bay_utils::ForkliftKind::Warehouse | bay_utils::ForkliftKind::Absent => (),
    }

    let relative = cwd.strip_prefix(&root).unwrap_or(Path::new(""));
    let _ = CWD_RELATIVE_TO_ROOT.set(respell_as_listed(&root, relative)?);

    std::env::set_current_dir(&root)
        .map_err(|e| format!("Error while switching to the working tree root \"{}\": {}", root.to_string_lossy(), e))
}

/// Respell each component of `relative`, a directory below `root`, as its parent lists it.
///
/// A volume can open a directory by a spelling its parent does not list (`DOCS` for `Docs` on a
/// case-insensitive one), and a process started there may see that spelling as its current
/// directory. Joined onto a user path, it would mint a key beside the tracked one. The current
/// directory names one entry, so respelling it chooses nothing on the user's behalf: a component
/// the parent lists exactly is kept, and any other is replaced by the listed entry that is the
/// same entry on disk. When no listed entry can be identified as it (none is, or the volume will
/// not identify the entry), the spelling is refused rather than used: the parent's listing has
/// already shown it is not the listed name, so keeping it would mint the alias key. A parent
/// that cannot be listed (traversable but unreadable) cannot be asked, so its component keeps the
/// spelling the volume opened.
///
/// # Arguments
/// * `root`     - The warehouse root.
/// * `relative` - The directory, relative to `root`.
///
/// # Returns
/// * `Ok(PathBuf)` - `relative`, spelled as listed.
/// * `Err(String)` - If a listed parent lists no entry for a component.
fn respell_as_listed(root: &Path, relative: &Path) -> Result<PathBuf, String> {
    let mut respelled = PathBuf::new();

    for component in relative.components() {
        let name = component.as_os_str();
        let parent = root.join(&respelled);

        let Ok(listing) = std::fs::read_dir(&parent)
            .and_then(|entries| entries.map(|entry| entry.map(|entry| entry.file_name())).collect::<Result<Vec<_>, _>>()) else {
            respelled.push(name);
            continue;
        };

        if listing.iter().any(|listed| listed == name) {
            respelled.push(name);
            continue;
        }

        // A name that is not valid UTF-8 is no warehouse key, so only UTF-8 entries can be it.
        let names: Vec<String> = listing.into_iter().filter_map(|listed| listed.into_string().ok()).collect();
        let listed = name.to_str().and_then(|name| alias_utils::listed_alias(&parent, name, &names));

        let Some(listed) = listed else {
            return Err(format!(
                "The current directory's component \"{}\" is not listed in \"{}\" under that name, and \
                no entry listed there could be identified as it, so Forklift cannot tell which directory it is.",
                name.to_string_lossy(), parent.to_string_lossy()
            ));
        };

        respelled.push(listed);
    }

    Ok(respelled)
}

/// Get the directory the user invoked forklift from, relative to the warehouse root.
/// Returns an empty path if [`enter_warehouse`] has not been called (e.g. in tests
/// that operate from the root directly).
pub fn cwd_relative_to_root() -> PathBuf {
    CWD_RELATIVE_TO_ROOT.get().cloned().unwrap_or_default()
}

/// Prepare a warehouse in the current folder. Every step is idempotent: only the pieces
/// that are missing get created.
///
/// No `.forkliftignore` is written, as `git init` writes no `.gitignore`. The ignore file is
/// tracked content, and a template written here would be an untracked file that franchise and
/// lower then refuse to overwrite with a pallet's own copy.
///
/// # Returns
/// * `Ok(Vec<String>)` - One note per created piece; empty if the warehouse was already
///                       fully prepared.
/// * `Err(String)`     - If the folder is a bay's, or a folder or file could not be created.
pub fn prepare_warehouse() -> Result<Vec<String>, String> {
    let mut created: Vec<String> = Vec::new();

    // A bay's `.forklift` is a folder too: filling it with warehouse pieces would report success
    // while leaving a directory that is neither.
    match bay_utils::classify_forklift(&forklift_root())? {
        bay_utils::ForkliftKind::Bay | bay_utils::ForkliftKind::OldFormatBay => return Err(format!(
            "\"{}\" belongs to a bay, so a warehouse cannot be prepared there. Remove the bay from its \
            warehouse first.", forklift_root().to_string_lossy()
        )),
        bay_utils::ForkliftKind::Warehouse | bay_utils::ForkliftKind::Absent => (),
    }

    if file_utils::create_folder_if_not_exists(&forklift_root())? {
        created.push(format!("Created folder \"{}\".", FOLDER_NAME_FORKLIFT_ROOT));
    };

    let folder_path_objects_root = file_utils::get_path_objects_root();

    if file_utils::create_folder_if_not_exists(Path::new(&folder_path_objects_root))? {
        created.push(format!("Created folder \"{}\".", &folder_path_objects_root));
    }

    let folder_path_inventory_root = file_utils::get_path_inventory_root();

    if file_utils::create_folder_if_not_exists(Path::new(&folder_path_inventory_root))? {
        created.push(format!("Created folder \"{}\".", &folder_path_inventory_root));
    }

    let folder_path_config_root = config_utils::get_warehouse_config_folder();

    if file_utils::create_folder_if_not_exists(&folder_path_config_root)? {
        created.push(format!(
            "Created folder \"{}\".",
            folder_path_config_root.to_string_lossy()
        ));
    }

    if pallet_utils::create_position_if_not_exists()? {
        created.push(format!(
            "Created the pallets folder and the bay position (on pallet \"{}\").",
            pallet_utils::DEFAULT_PALLET_NAME
        ));
    }

    if config_utils::create_warehouse_config_if_not_exists()? {
        created.push("Created warehouse configuration file.".to_string());
    }

    Ok(created)
}

/// Validate a warehouse id, the one rule both server heads apply: a single portable component
/// (see [`pallet_utils::portable_component_problem`]) of at most 100 characters that does not
/// start with `.`. The id names a folder directly under forklift-server's served base and keys
/// the AWS head's ref partition, so it is stricter than a pallet name.
///
/// # Arguments
/// * `id` - The warehouse id.
///
/// # Returns
/// * `Ok(())`      - If the id is valid.
/// * `Err(String)` - The refusal, shaped `"<id>" is not a valid warehouse id: <reason>.`.
pub fn validate_warehouse_id(id: &str) -> Result<(), String> {
    match warehouse_id_problem(id) {
        None => Ok(()),
        Some(reason) => {
            let reason = pallet_utils::with_lowercase_hint(reason, id, |lowered| warehouse_id_problem(lowered).is_none());
            Err(format!("\"{}\" is not a valid warehouse id: {}.", id, reason))
        }
    }
}

/// The reason `id` is not a valid warehouse id, if it is not (see [`validate_warehouse_id`]).
fn warehouse_id_problem(id: &str) -> Option<String> {
    if id.is_empty() {
        return Some("it is empty".to_string());
    }

    if id.len() > 100 {
        return Some("it is longer than 100 characters".to_string());
    }

    if id.starts_with('.') || id.starts_with('-') {
        return Some("it must not start with \".\" or \"-\"".to_string());
    }

    pallet_utils::portable_component_problem(id, "\".\", \"_\" and \"-\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one warehouse-id rule both heads apply: the portable component grammar plus the id's
    /// own limits (one component, at most 100 characters, no leading `.`).
    #[test]
    fn a_warehouse_id_follows_the_portable_grammar_and_its_own_limits() {
        for id in ["tenant-a", "tenant_a.1", "console", "x.con", "com10"] {
            assert!(validate_warehouse_id(id).is_ok(), "expected valid: {}", id);
        }
        assert!(validate_warehouse_id(&"a".repeat(100)).is_ok(), "exactly 100 is fine");

        let cases = [
            ("", "it is empty"),
            (&*"a".repeat(101), "longer than 100 characters"),
            (".hidden", "must not start with"),
            ("-flag", "must not start with"),
            ("tenant/a", "only lowercase ASCII letters"),
            ("tenant a", "only lowercase ASCII letters"),
            ("Acme", "only lowercase ASCII letters"),
            ("acme.", "must not end with \".\""),
            ("aux", "reserved device name"),
            ("nul.tar.gz", "reserved device name"),
        ];

        for (id, reason) in cases {
            let error = validate_warehouse_id(id).expect_err(id);
            assert!(error.starts_with(&format!("\"{}\" is not a valid warehouse id: ", id)) && error.ends_with('.')
                && error.contains(reason), "{:?}: expected the reason {:?}, got {}", id, reason, error);
        }

        assert_eq!(
            validate_warehouse_id("Acme").unwrap_err(),
            "\"Acme\" is not a valid warehouse id: only lowercase ASCII letters, digits, \".\", \"_\" and \"-\" are \
            allowed; the lowercase \"acme\" is valid."
        );
    }

    /// A component the parent lists exactly is kept; one it lists under another spelling of the
    /// same entry takes the listed spelling; one it lists under no spelling is refused.
    #[test]
    fn a_current_directory_is_respelled_as_its_parents_list_it() {
        let root = std::env::temp_dir().join(format!("forklift-warehouse-respell-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("docs/Sub")).unwrap();

        assert_eq!(respell_as_listed(&root, Path::new("docs/Sub")).unwrap(), Path::new("docs/Sub"));
        assert_eq!(respell_as_listed(&root, Path::new("")).unwrap(), Path::new(""));

        let folds = root.join("DOCS").exists();
        if folds {
            assert_eq!(respell_as_listed(&root, Path::new("DOCS/sub")).unwrap(), Path::new("docs/Sub"));
        }

        let error = respell_as_listed(&root, Path::new("docs/missing")).unwrap_err();
        assert!(error.contains("\"missing\" is not listed"), "{}", error);

        std::fs::remove_dir_all(&root).ok();
    }

    /// Below a parent that can be traversed but not listed, a component keeps the spelling it was
    /// opened by, and the components above it are still respelled.
    #[cfg(unix)]
    #[test]
    fn a_current_directory_below_an_unlistable_parent_keeps_its_spelling() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("forklift-warehouse-respell-locked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("locked/inner")).unwrap();
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o100)).unwrap();

        // Root lists through any mode, so there is no unlistable parent to test.
        let unlistable = std::fs::read_dir(root.join("locked")).is_err();
        assert!(unlistable || unsafe { libc::geteuid() } == 0, "the parent was listable");
        if unlistable {
            assert_eq!(respell_as_listed(&root, Path::new("locked/inner")).unwrap(), Path::new("locked/inner"));

            if root.join("LOCKED").exists() {
                assert_eq!(respell_as_listed(&root, Path::new("LOCKED/inner")).unwrap(), Path::new("locked/inner"));
            }
        }

        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&root).ok();
    }
}
