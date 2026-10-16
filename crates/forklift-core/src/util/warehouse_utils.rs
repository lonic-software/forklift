use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use crate::globals::{self, forklift_root, FOLDER_NAME_FORKLIFT_ROOT};
use crate::util::{bay_utils, config_utils, file_utils, pallet_utils};

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
        // A `.forklift` folder is a warehouse; a `.forklift` file is a bay redirect
        // (§7.5) — both mark the root of a working tree.
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

    // Inside a bay, the `.forklift` is a redirect to the shared warehouse: shared storage
    // (objects, refs, trust, config) resolves there, while the working directory, the
    // inventory, the current pallet and the lock stay in this bay.
    if bay_utils::is_bay_redirect(&forklift) {
        let redirect = bay_utils::read_bay_redirect(&forklift)?;
        globals::set_bay_context(redirect.warehouse_root.join(FOLDER_NAME_FORKLIFT_ROOT), redirect.name);
    }

    let relative = cwd.strip_prefix(&root).unwrap_or(Path::new("")).to_path_buf();
    let _ = CWD_RELATIVE_TO_ROOT.set(relative);

    std::env::set_current_dir(&root)
        .map_err(|e| format!("Error while switching to the working tree root \"{}\": {}", root.to_string_lossy(), e))
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
/// * `Err(String)`     - If a folder or file could not be created.
pub fn prepare_warehouse() -> Result<Vec<String>, String> {
    let mut created: Vec<String> = Vec::new();

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

    if pallet_utils::create_current_pallet_file_if_not_exists()? {
        created.push(format!(
            "Created the pallets folder and the current pallet file (\"{}\").",
            pallet_utils::DEFAULT_PALLET_NAME
        ));
    }

    if config_utils::create_warehouse_config_if_not_exists()? {
        created.push("Created warehouse configuration file.".to_string());
    }

    Ok(created)
}
