use std::path::PathBuf;
use serde::Serialize;
use forklift_core::globals;
use forklift_core::util::path_utils::WarehousePath;
use forklift_core::util::scope_utils::{MaterializationScope, ScopeClass};
use forklift_core::util::{
    bay_utils, file_utils, object_utils, pallet_utils, scope_utils, shift_utils, warehouse_utils,
};
use crate::cli::BayAction;
use crate::output::{self, CommandOutput};

/// Handle the bay command (§7.5): parallel working directories bound to one warehouse.
///
/// # Arguments
/// * `action` - The subcommand (`None` lists the bays).
///
/// # Returns
/// * `Ok(())`      - If the command was handled.
/// * `Err(String)` - If a bay could not be created, listed or removed.
pub fn handle_command(action: Option<BayAction>) -> Result<(), String> {
    match action {
        Some(BayAction::Add { name, path, scope }) => add(&name, path, scope),
        Some(BayAction::Remove { name }) => remove(&name),
        None => list(),
    }
}

/// Create a bay: a new pallet `<name>` at the current head, and a working directory bound
/// to this warehouse — sharing its object store and refs, with its own working tree,
/// inventory, current pallet and lock.
///
/// When `scope_paths` are given, the bay is **scoped (sparse, §7.6)**: it materializes and
/// operates on only those subtrees. On a full warehouse the object store still holds everything
/// (only materialization is scoped); on a sparse one the bay scope must stay within the
/// warehouse fetch scope. The scope is recorded bay-locally and drives the materialize, and
/// every later stack copies the out-of-scope siblings forward by hash.
fn add(name: &str, path: Option<String>, scope_paths: Vec<String>) -> Result<(), String> {
    pallet_utils::validate_pallet_name(name)?;

    // A bay is its own line of work: it checks out a new pallet named after it.
    if pallet_utils::does_pallet_exist(name) {
        return Err(format!(
            "A pallet named \"{}\" already exists; a bay checks out a new pallet, so pick \
            another name.", name
        ));
    }
    if bay_utils::does_bay_exist(name) {
        return Err(format!("A bay named \"{}\" already exists.", name));
    }

    // The warehouse root: the CLI entered it, so it is the current directory.
    let warehouse_root = std::env::current_dir()
        .map_err(|e| format!("Error while getting the warehouse root: {}", e))?;

    // A bay branches off the current pallet's head, so there must be one.
    let current = pallet_utils::get_current_pallet_name()?;
    let head = pallet_utils::get_pallet_head(&current)?.ok_or(format!(
        "Pallet \"{}\" has nothing stacked yet; stack something before opening a bay.", current
    ))?;

    let tree = object_utils::load_parcel(&head)?.tree_hash;

    // Normalize and validate the scope prefixes (against the head tree) before anything is
    // created, so a typo'd or non-directory scope refuses up front rather than yielding a
    // silently empty bay. Resolved while still at the warehouse root, before the cwd changes.
    let scope = resolve_scope(&scope_paths, &tree)?;

    let bay_dir = resolve_bay_dir(name, path, &warehouse_root)?;

    if bay_dir.exists() && bay_dir.read_dir().map(|mut d| d.next().is_some()).unwrap_or(false) {
        return Err(format!(
            "\"{}\" already exists and is not empty; choose an empty (or new) directory for the bay.",
            bay_dir.to_string_lossy()
        ));
    }

    // The bay's pallet, at the current head (a shared ref).
    pallet_utils::set_pallet_head(name, &head)?;

    // The bay's working directory + its local state folder + the redirect back here.
    std::fs::create_dir_all(&bay_dir)
        .map_err(|e| format!("Error while creating the bay directory \"{}\": {}", bay_dir.to_string_lossy(), e))?;
    file_utils::create_folder_if_not_exists(&bay_utils::bay_state_dir(name))?;
    bay_utils::write_bay_redirect(&bay_dir, &warehouse_root, name)?;
    bay_utils::write_bay_path(name, &bay_dir)?;

    // Enter the bay to check out its pallet and materialize its working tree. The current
    // warehouse lock stored its path at acquire time, so switching context here does not
    // affect its release.
    globals::set_bay_context(warehouse_root.join(globals::FOLDER_NAME_FORKLIFT_ROOT), name.to_string());
    std::env::set_current_dir(&bay_dir)
        .map_err(|e| format!("Error while entering the bay \"{}\": {}", bay_dir.to_string_lossy(), e))?;

    pallet_utils::set_current_pallet_name(name)?;

    // Record the materialization scope before materializing — the materialize (and every
    // later scope-aware verb in this bay) reads it back through `scope_utils::current_scope`.
    if !scope.is_full() {
        scope_utils::set_bay_scope(&scope)?;
    }

    shift_utils::materialize_tree(None, &tree, "Creating the bay")?;

    output::emit("bay", &BayCreated {
        name: name.to_string(),
        path: bay_dir.to_string_lossy().into_owned(),
        pallet: name.to_string(),
        head,
        scope: scope.prefixes().to_vec(),
    });

    Ok(())
}

/// Normalize the `--scope` prefixes into a [`MaterializationScope`] and verify each names a
/// directory (subtree) in the head tree. An empty list yields the full (unscoped) scope.
///
/// On a sparse warehouse, every requested prefix is checked against the warehouse fetch scope
/// **before** it is ever used to load a tree object: a prefix outside the fetch scope may name
/// an object that was never downloaded (a spine ancestor like `src` when only `src/api` was
/// fetched, or a sealed sibling like `src/web` itself), and `is_directory_in_tree` below would
/// otherwise hard-error on that absent object instead of refusing cleanly. Checking here, before
/// `add` creates any bay state, also keeps a rejected `--scope` from leaving a half-created bay
/// behind — `scope_utils::set_bay_scope` re-checks the same ⊆ invariant later, but only after
/// the pallet ref, directory and redirect already exist.
fn resolve_scope(scope_paths: &[String], head_tree_hash: &str) -> Result<MaterializationScope, String> {
    if scope_paths.is_empty() {
        return Ok(MaterializationScope::full());
    }

    let fetch_scope = scope_utils::read_fetch_scope()?;
    let mut keys: Vec<String> = Vec::new();

    for raw in scope_paths {
        let path = WarehousePath::from_user_input(raw)?;

        if path.is_root() {
            return Err(
                "A scope of the warehouse root is the whole tree — open a plain bay (without \
                --scope) instead.".to_string()
            );
        }

        let key = path.as_key().to_string();

        if !fetch_scope.is_full() && fetch_scope.classify(&key) != ScopeClass::InScope {
            return Err(format!(
                "\"{}\" is outside the warehouse's fetch scope ({}), so it cannot be a bay \
                scope. Fetch it first with \"expand {}\".",
                raw, fetch_scope.prefixes().join(", "), raw
            ));
        }

        if !is_directory_in_tree(head_tree_hash, &key)? {
            return Err(format!(
                "\"{}\" is not a directory in the current head, so it cannot be a bay scope.",
                raw
            ));
        }

        keys.push(key);
    }

    Ok(MaterializationScope::from_prefixes(keys))
}

/// Whether a warehouse path resolves to a subtree (directory) in the given root tree.
fn is_directory_in_tree(root_tree_hash: &str, key: &str) -> Result<bool, String> {
    let mut current = object_utils::load_tree(root_tree_hash)?;

    for component in key.split('/') {
        let subtree = current.get_subtrees()
            .find(|(name, _)| name.as_str() == component)
            .map(|(_, item)| item.hash.clone());

        match subtree {
            Some(hash) => current = object_utils::load_tree(&hash)?,
            None => return Ok(false),
        }
    }

    Ok(true)
}

/// Resolve where the bay's working directory should live: the given path (relative to the
/// directory the user invoked forklift from), or a default sibling of the warehouse.
fn resolve_bay_dir(name: &str, path: Option<String>, warehouse_root: &std::path::Path) -> Result<PathBuf, String> {
    match path {
        Some(path) => {
            let given = PathBuf::from(path);

            if given.is_absolute() {
                Ok(given)
            } else {
                // The CLI changed into the warehouse root; resolve relative to where the
                // user actually was.
                Ok(warehouse_root.join(warehouse_utils::cwd_relative_to_root()).join(given))
            }
        }
        None => {
            let dir_name = warehouse_root.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "warehouse".to_string());
            let parent = warehouse_root.parent().unwrap_or(warehouse_root);

            Ok(parent.join(format!("{}.{}", dir_name, name)))
        }
    }
}

/// List the bays: their names, working directories and current pallets.
fn list() -> Result<(), String> {
    let bays = bay_utils::list_bays()?
        .into_iter()
        .map(|name| {
            let path = bay_utils::read_bay_path(&name)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let pallet = bay_utils::read_bay_current_pallet(&name);

            BayEntry { name, path, pallet }
        })
        .collect();

    output::emit("bay", &BayList { bays });

    Ok(())
}

/// Remove a bay: de-register it (its local state and the redirect in its working tree).
/// The bay's pallet is a normal ref and is kept; the materialized files are left in place.
fn remove(name: &str) -> Result<(), String> {
    if !bay_utils::does_bay_exist(name) {
        return Err(format!("There is no bay named \"{}\".", name));
    }

    // Drop the redirect so the directory is no longer a bay (best effort — the working
    // tree may already be gone).
    if let Ok(bay_dir) = bay_utils::read_bay_path(name) {
        let _ = std::fs::remove_file(bay_dir.join(globals::FOLDER_NAME_FORKLIFT_ROOT));
    }

    bay_utils::remove_bay_state(name)?;

    output::emit("bay", &BayRemoved { name: name.to_string() });

    Ok(())
}

/// A newly created bay.
#[derive(Serialize)]
struct BayCreated {
    name: String,
    path: String,
    pallet: String,
    head: String,

    /// The bay's materialization scope prefixes (empty for a full, unscoped bay).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    scope: Vec<String>,
}

impl CommandOutput for BayCreated {
    fn render_human(&self) {
        println!(
            "Opened bay \"{}\" at \"{}\" on pallet \"{}\" (head {}).",
            self.name, self.path, self.pallet, self.head
        );

        if self.scope.is_empty() {
            println!("cd into it to work there — it shares this warehouse's objects and refs.");
        } else {
            println!(
                "Scoped (sparse) to: {}. cd into it to work there — it materializes only those \
                subtrees and shares this warehouse's objects and refs.",
                self.scope.join(", ")
            );
        }
    }
}

/// The list of bays.
#[derive(Serialize)]
struct BayList {
    bays: Vec<BayEntry>,
}

/// One bay in the list.
#[derive(Serialize)]
struct BayEntry {
    name: String,
    path: String,

    /// The bay's current pallet (`null` when unreadable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pallet: Option<String>,
}

impl CommandOutput for BayList {
    fn render_human(&self) {
        if self.bays.is_empty() {
            println!("No bays. Open one with \"bay add <name>\".");
            return;
        }

        for bay in &self.bays {
            let pallet = bay.pallet.as_deref().unwrap_or("(unborn)");
            println!("{}  on \"{}\"  at {}", bay.name, pallet, bay.path);
        }
    }
}

/// A removed bay.
#[derive(Serialize)]
struct BayRemoved {
    name: String,
}

impl CommandOutput for BayRemoved {
    fn render_human(&self) {
        println!(
            "Removed bay \"{}\" (its pallet is kept; delete the directory to reclaim the space).",
            self.name
        );
    }
}
