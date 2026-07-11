use std::collections::BTreeSet;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use file_id::FileId;
use regex::Regex;
use crate::builder::inventory::InventoryBuilder;
use crate::builder::object::loose_object_builder::LooseObjectBuilder;
use crate::enums::inventory_item_state::InventoryItemState;
use crate::enums::dir_entry_type::DirEntryType;
use crate::model::inventory::{Inventory, InventoryItem};
use crate::model::object::loose_object::LooseObject;
use crate::model::task::inventory_builder::inventory_builder_context::InventoryBuilderContext;
use crate::model::task::inventory_builder::inventory_builder_task::InventoryBuilderTask;
use crate::model::task::TaskExecutor;
use crate::parser;
use crate::traits::task_context::TaskContext;
use crate::util::{file_utils, object_utils};
use crate::util::path_utils::WarehousePath;

/// The metadata entry used for the warehouse root (its key is the empty string,
/// which would be confusing as a line in the metadata file).
const METADATA_ENTRY_ROOT: &str = "./";

/// Add a file or directory to its corresponding inventory.
/// If no inventory exists for the given directory, a new inventory file will be created.
///
/// # Arguments
/// * `path` - The path of the file or directory to add to the inventory.
///
/// # Returns
/// * `Ok(())`      - If the operation was successful.
/// * `Err(String)` - If there was an error.
pub async fn add_changes_to_inventory(path: &WarehousePath) -> Result<(), String> {
    let is_directory = file_utils::is_directory(&path.to_fs_path())?;

    if is_directory {
        create_inventory_for_directory(path).await?;
    } else {
        add_file_to_inventory(path)?;
    }

    Ok(())
}

/// Stage a file or directory for removal: its inventory entries are marked as
/// `Deleted` instead of being erased, so the staged removal is remembered until the next
/// parcel is stacked (and can be reported by status-like commands). The working directory
/// is never touched.
///
/// # Arguments
/// * `path` - The path of the file or directory to stage for removal.
///
/// # Returns
/// * `Ok(())`      - If the operation was successful.
/// * `Err(String)` - If there was an error.
pub fn stage_removal(path: &WarehousePath) -> Result<(), String> {
    // A directory is recognized by its inventory shard, not by the file system state:
    // the subject may already be gone from the working directory, and staging its removal
    // must still work in that case.
    let has_shard = file_utils::get_inventory_data_path_for_key(path.as_key()).exists();

    if path.is_root() || has_shard {
        return stage_removal_for_directory(path);
    }

    let fs_path = path.to_fs_path();

    if fs_path.exists() && file_utils::is_directory(&fs_path)? {
        return Err(format!("No inventory found for folder \"{}\".", path.as_key()));
    }

    stage_removal_for_file(path)
}

/// Create an inventory for the specified directory (and all subdirectories).
///
/// If the build fails halfway, the inventories that were already written are kept (and
/// registered in the metadata file), so previously loaded, unrelated inventories are never
/// destroyed. Re-running the load after fixing the problem completes the inventory.
///
/// # Arguments
/// * `path` - The path to the directory.
///
/// # Returns
/// * `Ok(())`      - If the inventory was successfully created.
/// * `Err(String)` - If an error occurred while creating the inventory.
pub async fn create_inventory_for_directory(path: &WarehousePath) -> Result<(), String> {
    let context = Arc::new(InventoryBuilderContext::new());
    let executor = TaskExecutor::new(Arc::clone(&context));
    let ignored_paths = file_utils::get_ignored_paths()?;

    // Every previously inventoried directory inside the loaded subtree starts out "dirty";
    // the walk removes each directory it visits. Whatever is left afterwards no longer
    // exists in the working directory (or is ignored now), so its entries are staged
    // as removals.
    populate_dirty_inventory_paths(&context, path).await?;

    let root_task: InventoryBuilderTask = Box::pin(build_inventory(
        Arc::clone(&context),
        Arc::new(path.clone()),
        Arc::new(ignored_paths)
    ));

    let result = executor.execute(root_task).await;

    if let Err(e) = result {
        // Register every inventory that was written, even on failure, so the metadata file
        // stays consistent with the inventory folders that exist on disk. Dirty inventories
        // are deliberately *not* removed on failure: the walk may not have reached them.
        update_inventory_metadata(&*context.new_inventory_paths.lock().await, &BTreeSet::new())?;

        let message = e.unwrap_or("An unknown error occurred while building the inventory.".to_string());

        return Err(format!(
            "{}\nThe load did not complete; entries loaded so far were kept. \
            Re-run the load once the problem is fixed.",
            message
        ));
    }

    let dirty_paths = context.dirty_inventory_paths.lock().await;
    let mut stale_keys: BTreeSet<String> = BTreeSet::new();

    // Directories that are gone from the working directory (deleted, or ignored now) keep
    // their inventory shard, with every entry marked as a staged removal — stacking the next
    // parcel is what consumes and cleans up the staged state. Only shards that no longer
    // exist on disk are dropped from the metadata file.
    for dirty_key in dirty_paths.iter() {
        if !mark_shard_entries_deleted(dirty_key)? {
            stale_keys.insert(dirty_key.clone());
        }
    }

    update_inventory_metadata(&*context.new_inventory_paths.lock().await, &stale_keys)?;

    Ok(())
}

/// Mark every previously inventoried directory inside the given subtree as dirty.
/// Directories visited by the inventory build remove themselves from this set, so the
/// directories remaining after the walk are the ones deleted from the working directory.
///
/// # Arguments
/// * `context` - The inventory builder context.
/// * `path`    - The root of the subtree being loaded.
///
/// # Returns
/// * `Ok(())`      - If the dirty set was populated successfully.
/// * `Err(String)` - If the inventory metadata could not be read.
async fn populate_dirty_inventory_paths(context: &InventoryBuilderContext,
                                        path: &WarehousePath) -> Result<(), String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    let Some(metadata) = metadata_opt else {
        return Ok(());
    };

    let subtree_prefix = format!("{}/", path.as_key());
    let mut dirty = context.dirty_inventory_paths.lock().await;

    for entry in metadata {
        let key = metadata_entry_to_key(&entry);

        let is_in_subtree = path.is_root()
            || key == path.as_key()
            || key.starts_with(&subtree_prefix);

        if is_in_subtree {
            dirty.insert(key.to_string());
        }
    }

    Ok(())
}

/// Convert an inventory metadata file entry to a warehouse path key.
/// The warehouse root is stored as `./` in the metadata file, but its key is the empty string.
pub fn metadata_entry_to_key(entry: &str) -> &str {
    if entry == METADATA_ENTRY_ROOT { "" } else { entry }
}

/// Convert a warehouse path key to its inventory metadata file entry.
fn key_to_metadata_entry(key: &str) -> String {
    if key.is_empty() { String::from(METADATA_ENTRY_ROOT) } else { key.to_string() }
}

/// Stage the removal of a directory: mark every entry in its inventory (and in the
/// inventories of all of its subdirectories) as `Deleted`. The inventory shards and their
/// metadata entries are kept — they are the record of the staged removals.
///
/// # Arguments
/// * `path` - The path of the folder in the working dir.
///
/// # Returns
/// * `Ok(())`      - If the removals were staged successfully.
/// * `Err(String)` - If no inventory exists for the folder, or there was an error.
fn stage_removal_for_directory(path: &WarehousePath) -> Result<(), String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    // The directory's own key is processed even when the metadata file is missing or
    // inconsistent; the metadata supplies the subdirectory shards.
    let mut keys: BTreeSet<String> = BTreeSet::new();
    keys.insert(path.as_key().to_string());

    if let Some(metadata) = metadata_opt {
        let subtree_prefix = format!("{}/", path.as_key());

        for entry in &metadata {
            let key = metadata_entry_to_key(entry);

            if path.is_root() || key.starts_with(&subtree_prefix) {
                keys.insert(key.to_string());
            }
        }
    }

    let mut found_any_shard = false;

    for key in &keys {
        if mark_shard_entries_deleted(key)? {
            found_any_shard = true;
        }
    }

    if !found_any_shard {
        return Err(format!(
            "No inventory found for folder \"{}\".",
            if path.is_root() { METADATA_ENTRY_ROOT } else { path.as_key() }
        ));
    }

    Ok(())
}

/// Mark every entry of the inventory shard with the given key as `Deleted`.
/// The shard is only rewritten when an entry actually changed state.
///
/// # Arguments
/// * `key` - The warehouse path key of the directory whose shard should be marked.
///
/// # Returns
/// * `Ok(true)`    - If the shard exists (whether or not entries changed state).
/// * `Ok(false)`   - If no shard exists for the given key.
/// * `Err(String)` - If the shard could not be read, parsed or written.
fn mark_shard_entries_deleted(key: &str) -> Result<bool, String> {
    let (shard_path, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

    let Some(bytes) = bytes_opt else {
        return Ok(false);
    };

    let mut inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
        .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?;

    if inventory.mark_all_items_deleted() {
        save_inventory(&inventory, &shard_path)?;
    }

    Ok(true)
}

/// A task for building an inventory file for a given directory.
/// When encountering a subdirectory, a new task is created to build the inventory for that directory.
///
/// # Arguments
/// * `context`         - The task context.
/// * `path`            - The warehouse path of the directory.
/// * `paths_to_ignore` - Paths of files and directories that should be ignored. The patterns are
/// matched against warehouse path keys (see `WarehousePath::as_key`).
///
/// # Returns
/// * `Ok(())`      - If the inventory file was build successfully.
/// * `Err(String)` - If there was an error during the operation.
fn build_inventory(context: Arc<InventoryBuilderContext>,
                   path: Arc<WarehousePath>,
                   paths_to_ignore: Arc<Vec<Regex>>) -> impl Future<Output = Result<(), String>> + Send {
    async move {
        let directory = file_utils::read_directory(&path.to_fs_path())?;

        if !path.is_root() && file_utils::is_path_ignored(path.as_key(), &paths_to_ignore) {
            return Ok(());
        }

        // The existing inventory of this directory (if any) is the stat cache: entries whose
        // file metadata is unchanged are reused without reading or hashing the file.
        // An unreadable or unparsable shard simply means a full rebuild of this directory.
        // The shard's own modification time is needed to reject "racily clean" entries
        // (see `is_entry_unchanged`).
        let existing_inventory = match file_utils::retrieve_inventory_or_none_by_key(path.as_key()) {
            Ok((shard_path, Some(bytes))) => {
                let shard_mtime = file_utils::get_symlink_metadata_for_path(&shard_path).ok()
                    .and_then(|m| file_utils::get_content_modification_timestamp_for_file(&m).ok());

                parser::inventory::inventory_parser::parse_inventory(&bytes).ok().zip(shard_mtime)
            }
            _ => None,
        };

        let mut inventory = Inventory::new();

        {
            let key = path.as_key().to_string();
            context.new_inventory_paths.lock().await.insert(key.clone());
            context.dirty_inventory_paths.lock().await.remove(&key);
        }

        for entry_result in directory {
            let entry = entry_result.map_err(|e|
                format!("Error while reading directory entry: {}", e)
            )?;

            let name = file_utils::get_name_for_file_or_directory(&entry)?;
            let metadata = file_utils::get_symlink_metadata_for_path(&entry.path())?;
            let item_type = file_utils::get_type_of_dir_entry(&metadata);
            let entry_path = path.child(&name);

            if file_utils::is_path_ignored(entry_path.as_key(), &paths_to_ignore) {
                continue;
            }

            if item_type.is_file() {
                let existing_entry = existing_inventory.as_ref()
                    .and_then(|(inv, shard_mtime)| {
                        inv.get_item_by_name(&name).map(|item| (item, *shard_mtime))
                    });

                let index_item = match existing_entry {
                    Some((item, shard_mtime)) => {
                        let verdict = classify_file_against_entry(
                            &item, &metadata, item_type, &entry.path(), &name, shard_mtime
                        )?;

                        match verdict {
                            FileVerdict::UnchangedByStat => {
                                // Loading stages the *current* state: a file that is present
                                // on disk is staged as Normal even if it was staged for
                                // removal before (the same way "git add" re-stages a file
                                // after "git rm --cached").
                                let mut item = (*item).clone();
                                item.state = InventoryItemState::Normal;
                                item
                            }
                            // Storing on the unchanged-by-hash path too keeps load
                            // self-healing: a blob that went missing from the object
                            // store comes back on the next re-load.
                            FileVerdict::UnchangedByHash(fresh, mut object)
                                | FileVerdict::Modified(fresh, mut object) => {
                                object.store()?;
                                fresh
                            }
                        }
                    }
                    None => build_inventory_item_from_file(
                        &entry.path(),
                        name.as_str(),
                        item_type
                    )?,
                };

                inventory.add_item(index_item);
            } else {
                let new_task = Box::pin(build_inventory(
                    context.clone(),
                    Arc::new(entry_path),
                    Arc::clone(&paths_to_ignore)
                ));

                context.send_task(new_task)?;
            }
        }

        // Entries of the old inventory whose file is no longer in the directory (deleted,
        // renamed, newly ignored, or replaced by a directory) are carried over as staged
        // removals — this is the "present only in the shard → Deleted" half of the
        // per-directory merge-join.
        if let Some((old_inventory, _)) = existing_inventory.as_ref() {
            carry_over_missing_entries_as_deleted(old_inventory, &mut inventory);
        }

        let inventory_data_path = file_utils::get_inventory_data_path_for_key(path.as_key());
        save_inventory(&inventory, &inventory_data_path)?;

        Ok(())
    }
}

/// Build an inventory item for a file whose blob is already stored (its hash is known,
/// e.g. from a tree object): only the file's metadata is gathered, nothing is read or
/// hashed. Used when repopulating the inventory after materializing a tree.
///
/// # Arguments
/// * `path` - The path of the file.
/// * `name` - The name of the file.
/// * `hash` - The (already known) blob hash of the file's content.
///
/// # Returns
/// * `Ok(InventoryItem)` - The inventory item.
/// * `Err(String)`       - If the file's metadata could not be gathered.
pub fn build_inventory_item_from_stat(path: &Path,
                                      name: &str,
                                      hash: String) -> Result<InventoryItem, String> {
    let metadata = file_utils::get_symlink_metadata_for_path(path)?;
    let item_type = file_utils::get_type_of_dir_entry(&metadata);

    let mtime = file_utils::get_content_modification_timestamp_for_file(&metadata)?;
    let ctime = file_utils::get_metadata_modification_timestamp_for_file(&metadata);

    let file_id = file_utils::get_file_id_for_file(path)?;

    let (device_id, inode) = match file_id {
        FileId::Inode { device_id, inode_number } => Ok((device_id, inode_number)),
        FileId::LowRes { volume_serial_number, file_index } => Ok((volume_serial_number as u64, file_index)),
        FileId::HighRes { .. } => Err("High resolution file IDs are not supported.".to_string()),
    }?;

    let (user_id, group_id) = file_utils::get_owners_for_file(&metadata);

    Ok(
        InventoryItem {
            metadata_change_timestamp: ctime,
            content_change_timestamp: mtime,
            device: device_id,
            inode,
            item_type,
            user_id,
            group_id,
            file_size: metadata.len(),
            hash,
            file_name_length: name.len() as u64,
            state: InventoryItemState::Normal,
            name: String::from(name),
        }
    )
}

/// Stage a fresh inventory entry (with current stat data) for a file whose blob is
/// already stored (e.g. one just written from a tree or merge).
///
/// # Arguments
/// * `path` - The warehouse path of the file.
/// * `hash` - The blob hash of the file's content.
///
/// # Returns
/// * `Ok(())`      - If the entry was staged.
/// * `Err(String)` - If the file's metadata could not be gathered or the shard written.
pub fn stage_file_entry_from_stat(path: &str, hash: String) -> Result<(), String> {
    let (parent_key, name) = match path.rsplit_once(file_utils::PATH_SEPARATOR_CHAR) {
        Some((parent, name)) => (parent, name),
        None => ("", path),
    };

    let entry = build_inventory_item_from_stat(Path::new(path), name, hash)?;

    update_shard(parent_key, |inventory| {
        inventory.add_item(entry);
        Ok(())
    })
}

/// Whether the directory at `path` may be safely replaced (by a file, or cleared to make
/// way for one) without losing data: it is tracked — represented by its own inventory
/// shard, the sharded-inventory way a directory is recognized (see `stage_removal`) — and
/// every entry beneath it, recursively, is tracked too. Called for a path a merge or shift
/// wants to write a new file to that already exists as a directory on disk (a tracked
/// dir→file flip); the caller still refuses when this returns `false`, exactly as it does
/// for a plain untracked file at the path.
///
/// # Arguments
/// * `path` - The warehouse path of the directory (assumed to exist on disk).
///
/// # Returns
/// * `Ok(true)`    - The directory is tracked and has no untracked content beneath it.
/// * `Ok(false)`   - The directory is untracked, or has untracked content beneath it.
/// * `Err(String)` - If a directory entry or a shard could not be read or parsed.
pub fn directory_is_safe_to_replace(path: &str) -> Result<bool, String> {
    if !file_utils::get_inventory_data_path_for_key(path).exists() {
        return Ok(false);
    }

    let ignored_paths = Arc::new(file_utils::get_ignored_paths()?);

    directory_has_no_untracked_content(path, ignored_paths)
}

/// Recursively check a tracked directory for untracked content (the body of
/// `directory_is_safe_to_replace`). Ignored entries are skipped, matching the rest of the
/// inventory machinery (`walk_directory_unstaged` in `stocktake_utils`): they are invisible
/// to tracking, not a collision.
///
/// # Arguments
/// * `key`           - The warehouse path key of the directory.
/// * `ignored_paths` - The ignore patterns, computed once by the caller and threaded through
///                     the recursion instead of being reloaded and recompiled at every level.
fn directory_has_no_untracked_content(key: &str, ignored_paths: Arc<Vec<Regex>>) -> Result<bool, String> {
    let fs_path = if key.is_empty() { std::path::PathBuf::from(".") } else { std::path::PathBuf::from(key) };

    let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;
    let inventory = match bytes_opt {
        Some(bytes) => parser::inventory::inventory_parser::parse_inventory(&bytes)
            .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?,
        None => Inventory::new(),
    };

    for entry_result in file_utils::read_directory(&fs_path)? {
        let entry = entry_result.map_err(|e| format!("Error while reading directory entry: {}", e))?;
        let name = file_utils::get_name_for_file_or_directory(&entry)?;
        let entry_key = if key.is_empty() { name.clone() } else { format!("{}/{}", key, name) };

        if file_utils::is_path_ignored(&entry_key, &ignored_paths) {
            continue;
        }

        let metadata = file_utils::get_symlink_metadata_for_path(&entry.path())?;
        let item_type = file_utils::get_type_of_dir_entry(&metadata);

        let is_tracked = if item_type.is_file() {
            matches!(
                inventory.get_item_by_name(&name),
                Some(item) if item.state != InventoryItemState::Deleted
            )
        } else {
            file_utils::get_inventory_data_path_for_key(&entry_key).exists()
                && directory_has_no_untracked_content(&entry_key, Arc::clone(&ignored_paths))?
        };

        if !is_tracked {
            return Ok(false);
        }
    }

    Ok(true)
}

/// Refresh every *tracked* entry of the inventory from the working directory: modified
/// files are re-hashed (their blobs stored) and re-staged, files gone from disk become
/// staged removals. Untracked files are deliberately left alone — this is `park`'s way of
/// staging the whole work in progress without swallowing untracked content.
///
/// # Returns
/// * `Ok(())`      - If the refresh completed.
/// * `Err(String)` - If a shard or file could not be processed.
pub fn refresh_tracked_entries() -> Result<(), String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    let Some(metadata) = metadata_opt else {
        return Ok(());
    };

    for entry in &metadata {
        let key = metadata_entry_to_key(entry);

        let (shard_path, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

        let Some(bytes) = bytes_opt else {
            continue;
        };

        let shard_mtime = file_utils::get_symlink_metadata_for_path(&shard_path).ok()
            .and_then(|m| file_utils::get_content_modification_timestamp_for_file(&m).ok())
            .unwrap_or(0);

        let mut inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
            .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?;

        let names: Vec<String> = inventory.get_items().map(|(name, _)| name.clone()).collect();
        let mut changed = false;

        for name in names {
            let item = inventory.get_item_by_name(&name).unwrap();

            if item.state == InventoryItemState::Deleted {
                continue;
            }

            let file_path = if key.is_empty() {
                std::path::PathBuf::from(&name)
            } else {
                std::path::PathBuf::from(format!("{}/{}", key, name))
            };

            let Ok(metadata) = file_utils::get_symlink_metadata_for_path(&file_path) else {
                // The file is gone from disk: its removal becomes staged.
                inventory.mark_item_deleted(&name);
                changed = true;
                continue;
            };

            let item_type = file_utils::get_type_of_dir_entry(&metadata);

            if is_entry_unchanged(&item, &metadata, item_type, &file_path, shard_mtime) {
                continue;
            }

            // The entry is rebuilt even when only the stat data went stale (same
            // content), so the refreshed shard keeps the fast path warm.
            let rebuilt = build_inventory_item_from_file(&file_path, &name, item_type)?;
            changed = true;

            inventory.add_item(rebuilt);
        }

        if changed {
            save_inventory(&inventory, &shard_path)?;
        }
    }

    Ok(())
}

/// Check whether any inventory entry is in a conflict state (an unresolved consolidation).
///
/// # Returns
/// * `Ok(bool)`    - Whether at least one entry is in conflict.
/// * `Err(String)` - If a shard could not be read or parsed.
pub fn has_conflict_entries() -> Result<bool, String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    let Some(metadata) = metadata_opt else {
        return Ok(false);
    };

    for entry in &metadata {
        let key = metadata_entry_to_key(entry);
        let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

        let Some(bytes) = bytes_opt else {
            continue;
        };

        let inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
            .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?;

        let has_conflict = inventory.get_items().any(|(_, item)| matches!(
            item.state,
            InventoryItemState::FirstParentConflict
                | InventoryItemState::SecondParentConflict
                | InventoryItemState::ThirdParentConflict
        ));

        if has_conflict {
            return Ok(true);
        }
    }

    Ok(false)
}

/// List the warehouse paths of every inventory entry in a conflict state (an
/// unresolved consolidation), sorted. The counterpart of [`has_conflict_entries`] for
/// callers that need the paths themselves (the `conflicts` command).
///
/// # Returns
/// * `Ok(Vec<String>)` - The conflicted paths (empty when there are none).
/// * `Err(String)`     - If a shard could not be read or parsed.
pub fn list_conflict_paths() -> Result<Vec<String>, String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    let Some(metadata) = metadata_opt else {
        return Ok(Vec::new());
    };

    let mut paths = Vec::new();

    for entry in &metadata {
        let key = metadata_entry_to_key(entry);
        let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

        let Some(bytes) = bytes_opt else {
            continue;
        };

        let inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
            .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?;

        for (name, item) in inventory.get_items() {
            let is_conflict = matches!(
                item.state,
                InventoryItemState::FirstParentConflict
                    | InventoryItemState::SecondParentConflict
                    | InventoryItemState::ThirdParentConflict
            );

            if is_conflict {
                paths.push(if key.is_empty() { name.clone() } else { format!("{}/{}", key, name) });
            }
        }
    }

    paths.sort();

    Ok(paths)
}

/// Build a "stale" inventory item: the hash and type are known (e.g. from a head tree),
/// but the stat fields are zeroed on purpose, so the stat cache can never trust the entry
/// — the next comparison against the working directory always rehashes the file. Used by
/// `restore --staged`, where the file on disk may not match the recorded hash.
///
/// # Arguments
/// * `name`      - The name of the file.
/// * `hash`      - The blob hash of the entry's content.
/// * `item_type` - The type of the entry.
///
/// # Returns
/// * `InventoryItem` - The stale inventory item.
pub fn build_stale_inventory_item(name: &str, hash: String, item_type: DirEntryType) -> InventoryItem {
    InventoryItem {
        metadata_change_timestamp: 0,
        content_change_timestamp: 0,
        device: 0,
        inode: 0,
        item_type,
        user_id: 0,
        group_id: 0,
        file_size: 0,
        hash,
        file_name_length: name.len() as u64,
        state: InventoryItemState::Normal,
        name: String::from(name),
    }
}

/// Load the inventory shard for the given key (or an empty one), apply the given change,
/// and save it back.
///
/// # Arguments
/// * `key`    - The warehouse path key of the directory.
/// * `change` - The change to apply to the inventory.
///
/// # Returns
/// * `Ok(())`      - If the shard was updated.
/// * `Err(String)` - If the shard could not be read, parsed or written.
pub fn update_shard(key: &str,
                    change: impl FnOnce(&mut Inventory) -> Result<(), String>) -> Result<(), String> {
    let (shard_path, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

    let mut inventory = match bytes_opt {
        Some(bytes) => parser::inventory::inventory_parser::parse_inventory(&bytes)
            .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?,
        None => Inventory::new(),
    };

    change(&mut inventory)?;

    save_inventory(&inventory, &shard_path)?;

    let mut new_keys: BTreeSet<String> = BTreeSet::new();
    new_keys.insert(key.to_string());

    update_inventory_metadata(&new_keys, &BTreeSet::new())
}

/// Replace the staging area below the given directory with the given shards: the existing
/// inventory folders under the key are removed, the given shards are written, and the
/// metadata file is updated accordingly. Used by `restore --staged` to reset a subtree of
/// the inventory to the pallet head.
///
/// # Arguments
/// * `key`    - The warehouse path key of the subtree to replace (`""` for everything).
/// * `shards` - Warehouse path key → inventory for the new state of the subtree.
///
/// # Returns
/// * `Ok(())`      - If the subtree was replaced.
/// * `Err(String)` - If a folder or file operation failed.
pub fn replace_subtree_inventories(key: &str,
                                   shards: &std::collections::BTreeMap<String, Inventory>) -> Result<(), String> {
    let folder = file_utils::get_inventory_folder_for_key(key);

    if folder.exists() {
        std::fs::remove_dir_all(&folder).map_err(|e|
            format!("Error while clearing the staging area of \"{}\": {}", key, e)
        )?;
    }

    let (metadata_path, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;
    let mut metadata = metadata_opt.unwrap_or_default();

    if key.is_empty() {
        metadata.clear();
    } else {
        let prefix = format!("{}/", key);
        metadata.retain(|entry| {
            let entry_key = metadata_entry_to_key(entry);
            entry_key != key && !entry_key.starts_with(&prefix)
        });
    }

    for (shard_key, inventory) in shards {
        save_inventory(inventory, &file_utils::get_inventory_data_path_for_key(shard_key))?;
        metadata.insert(key_to_metadata_entry(shard_key));
    }

    write_metadata_to_file(&metadata_path, &metadata)
}

/// Replace the whole staging area with the given shards: the existing inventory folders
/// are removed, the given shards are written, and the metadata file is rewritten to list
/// exactly their directories. Used when `shift` repopulates the inventory from the target
/// pallet's tree.
///
/// # Arguments
/// * `shards` - Warehouse path key → inventory, for every tracked directory.
///
/// # Returns
/// * `Ok(())`      - If the staging area was replaced.
/// * `Err(String)` - If a folder or file operation failed.
pub fn replace_all_inventories(shards: &std::collections::BTreeMap<String, Inventory>) -> Result<(), String> {
    let root_folder = file_utils::get_inventory_folder_for_key("");

    if root_folder.exists() {
        std::fs::remove_dir_all(&root_folder).map_err(|e|
            format!("Error while clearing the staging area: {}", e)
        )?;
    }

    let mut metadata: BTreeSet<String> = BTreeSet::new();

    for (key, inventory) in shards {
        save_inventory(inventory, &file_utils::get_inventory_data_path_for_key(key))?;
        metadata.insert(key_to_metadata_entry(key));
    }

    let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none()?;

    write_metadata_to_file(&metadata_path, &metadata)
}

/// Clean up the staged state after a parcel was stacked: the parcel consumed every staged
/// removal, so `Deleted` entries are dropped from their shards, and the shards of
/// directories that no longer exist in the working directory are removed entirely
/// (together with their metadata entries).
///
/// # Returns
/// * `Ok(())`      - If the cleanup completed.
/// * `Err(String)` - If a shard could not be read, parsed or written.
pub fn cleanup_after_stack() -> Result<(), String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    let Some(metadata) = metadata_opt else {
        return Ok(());
    };

    let mut removed_keys: BTreeSet<String> = BTreeSet::new();

    for entry in &metadata {
        let key = metadata_entry_to_key(entry);

        let dir_path = if key.is_empty() {
            Path::new(".").to_path_buf()
        } else {
            std::path::PathBuf::from(key)
        };

        if !dir_path.is_dir() {
            // The directory is gone from the working tree, and the parcel that was just
            // stacked recorded its removal; its shard has served its purpose.
            let folder = file_utils::get_inventory_folder_for_key(key);

            // A parent directory earlier in the (sorted) set may have removed this folder.
            if folder.exists() {
                std::fs::remove_dir_all(&folder).map_err(|e|
                    format!("Error while removing the inventory of folder \"{}\": {}", key, e)
                )?;
            }

            removed_keys.insert(key.to_string());
            continue;
        }

        let (shard_path, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

        let Some(bytes) = bytes_opt else {
            continue;
        };

        let mut inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
            .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?;

        let staged_removals: Vec<String> = inventory.get_items()
            .filter(|(_, item)| item.state == InventoryItemState::Deleted)
            .map(|(name, _)| name.clone())
            .collect();

        if !staged_removals.is_empty() {
            for name in &staged_removals {
                inventory.remove_item_by_name(name);
            }

            save_inventory(&inventory, &shard_path)?;
        }
    }

    update_inventory_metadata(&BTreeSet::new(), &removed_keys)
}

/// Remove every inventory shard at or under a warehouse path prefix, dropping those keys from
/// the metadata too. Unlike [`stage_removal_for_directory`], this leaves no `Deleted` record —
/// the entries vanish, as if the subtree had never been inventoried. Used by `narrow` when a
/// subtree leaves the checkout's materialization scope: it should stop being reported entirely,
/// not appear as a staged removal to be committed.
///
/// # Arguments
/// * `prefix` - The warehouse path key of the subtree leaving scope (never the root).
///
/// # Returns
/// * `Ok(())`      - If the shards under the prefix were removed.
/// * `Err(String)` - If a shard folder or the metadata could not be updated.
pub fn remove_inventories_under(prefix: &str) -> Result<(), String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    let Some(metadata) = metadata_opt else {
        return Ok(());
    };

    let mut removed_keys: BTreeSet<String> = BTreeSet::new();

    for entry in &metadata {
        let key = metadata_entry_to_key(entry);

        // The prefix directory itself, or a directory strictly under it.
        let under = key == prefix
            || (key.len() > prefix.len()
                && key.as_bytes()[prefix.len()] == b'/'
                && key.starts_with(prefix));

        if !under {
            continue;
        }

        let folder = file_utils::get_inventory_folder_for_key(key);

        // A parent shard folder earlier in the (sorted) set may have removed this one already.
        if folder.exists() {
            std::fs::remove_dir_all(&folder).map_err(|e|
                format!("Error while removing the inventory of folder \"{}\": {}", key, e)
            )?;
        }

        removed_keys.insert(key.to_string());
    }

    update_inventory_metadata(&BTreeSet::new(), &removed_keys)
}

/// Carry over the entries of the previous inventory that were not re-added by the directory
/// walk (their file was deleted, renamed, newly ignored, or replaced by a directory),
/// marking them as staged removals.
///
/// # Arguments
/// * `old_inventory` - The inventory of the previous load.
/// * `new_inventory` - The inventory being rebuilt from the working directory.
fn carry_over_missing_entries_as_deleted(old_inventory: &Inventory, new_inventory: &mut Inventory) {
    let missing_items: Vec<InventoryItem> = old_inventory.get_items()
        .filter(|(name, _)| new_inventory.get_item_by_name(name).is_none())
        .map(|(_, item)| (**item).clone())
        .collect();

    for mut item in missing_items {
        item.state = InventoryItemState::Deleted;
        new_inventory.add_item(item);
    }
}

/// Update the inventory metadata file (a text file that contains the paths of all
/// inventoried directories, sorted alphabetically) in a single write.
///
/// # Arguments
/// * `keys_to_add`    - Warehouse path keys of directories to register.
/// * `keys_to_remove` - Warehouse path keys of directories to remove.
///
/// # Returns
/// * `Ok(())`      - If the metadata was successfully updated.
/// * `Err(String)` - If an error occurred while updating the metadata.
fn update_inventory_metadata(keys_to_add: &BTreeSet<String>,
                             keys_to_remove: &BTreeSet<String>) -> Result<(), String> {
    let (metadata_path, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;
    let mut metadata = metadata_opt.unwrap_or(BTreeSet::new());

    for key in keys_to_add {
        metadata.insert(key_to_metadata_entry(key));
    }

    for key in keys_to_remove {
        metadata.remove(&key_to_metadata_entry(key));
    }

    write_metadata_to_file(&metadata_path, &metadata)
}

/// Save the inventory metadata to file.
///
/// # Arguments
/// * `path`     - The path of the file where the metadata should be saved.
/// * `metadata` - Inventory metadata. A `BTreeSet` consisting of paths of directories where
/// inventories are stored.
///
/// # Returns
/// * `Ok(())`      - If the metadata file was saved successfully.
/// * `Err(String)` - If there was an error while saving the metadata file.
fn write_metadata_to_file(path: &Path, metadata: &BTreeSet<String>) -> Result<(), String> {
    let mut metadata_bytes: Vec<u8> = Vec::new();

    for inv_path in metadata {
        metadata_bytes.extend(inv_path.as_bytes());
        object_utils::push_new_line(&mut metadata_bytes);
    }

    std::fs::write(path, metadata_bytes).map_err(|e|
        format!("Error while writing inventory metadata to file \"{}\": {}", path.to_string_lossy(), e)
    )
}

/// Check whether a file is unchanged compared to its existing inventory entry, based purely
/// on file metadata (no file content is read). This is the stat-cache fast path: matching
/// ctime, mtime, device, inode, type (and size, for non-symlinks) means the stored hash is
/// still valid, exactly like git's index stat cache.
///
/// Any error while gathering metadata simply reports "changed", falling back to the full
/// read-and-hash path.
///
/// Note that editors which save via write-new-then-rename replace the inode on every save;
/// an inode mismatch therefore just means "changed" (rehash), never "different file".
///
/// "Racily clean" protection: timestamps have second granularity, so a file modified in the
/// same second the shard was written could keep identical mtime/ctime/size and slip past the
/// stat check. Entries are therefore only trusted when their mtime is *strictly older* than
/// the shard itself — anything as new as the shard (or newer, e.g. clock skew) is rehashed.
///
/// # Arguments
/// * `existing`    - The inventory entry from the previous load.
/// * `metadata`    - The current (symlink) metadata of the file.
/// * `item_type`   - The current type of the directory entry.
/// * `path`        - The path of the file.
/// * `shard_mtime` - The modification timestamp of the inventory shard the entry came from.
///
/// # Returns
/// * `true`  - If the file is unchanged and the existing entry can be reused.
/// * `false` - If the file changed (or freshness could not be determined).
pub fn is_entry_unchanged(existing: &InventoryItem,
                      metadata: &std::fs::Metadata,
                      item_type: DirEntryType,
                      path: &Path,
                      shard_mtime: u64) -> bool {
    if existing.item_type != item_type {
        return false;
    }

    let Ok(mtime) = file_utils::get_content_modification_timestamp_for_file(metadata) else {
        return false;
    };
    let ctime = file_utils::get_metadata_modification_timestamp_for_file(metadata);

    let Ok(file_id) = file_utils::get_file_id_for_file(path) else {
        return false;
    };
    let (device, inode) = match file_id {
        FileId::Inode { device_id, inode_number } => (device_id, inode_number),
        FileId::LowRes { volume_serial_number, file_index } => (volume_serial_number as u64, file_index),
        FileId::HighRes { .. } => return false,
    };

    // For symlinks the stored size is the length of the target path, which is not comparable
    // to the metadata size on every platform; the other fields are sufficient for them.
    let size_matches = item_type == DirEntryType::SymbolicLink
        || existing.file_size == metadata.len();

    mtime < shard_mtime
        && existing.content_change_timestamp == mtime
        && existing.metadata_change_timestamp == ctime
        && existing.device == device
        && existing.inode == inode
        && size_matches
}

/// Create an inventory item for a file.
/// If the given file does not exist the object store, a new blob is created and stored.
///
/// # Arguments
/// * `path`      - The path of the file.
/// * `name`      - The name of the file.
/// * `item_type` - The type of the directory entry.
///
/// # Returns
/// * `Ok(InventoryItem)` - The inventory item for the file.
/// * `Err(String)`       - The error message if the inventory item could not be created.
pub fn build_inventory_item_from_file(path: &Path,
                                      name: &str,
                                      item_type: DirEntryType) -> Result<InventoryItem, String> {
    let (item, mut object) = build_item_and_object_for_file(path, name, item_type)?;

    object.store()?;

    Ok(item)
}

/// Create an inventory item for a file, together with its blob object — read and hashed,
/// but **not** stored. Read-only callers (the stocktake walk) drop the object; writers
/// (`load`) store it.
///
/// # Arguments
/// * `path`      - The path of the file.
/// * `name`      - The name of the file.
/// * `item_type` - The type of the directory entry.
///
/// # Returns
/// * `Ok((InventoryItem, LooseObject))` - The inventory item and the unstored blob object.
/// * `Err(String)`                      - If the file could not be read or stat'ed.
fn build_item_and_object_for_file(path: &Path,
                                  name: &str,
                                  item_type: DirEntryType)
                                  -> Result<(InventoryItem, LooseObject), String> {
    let metadata = file_utils::get_symlink_metadata_for_path(path)?;

    let mtime = file_utils::get_content_modification_timestamp_for_file(&metadata)?;
    let ctime = file_utils::get_metadata_modification_timestamp_for_file(&metadata);

    let file_id = file_utils::get_file_id_for_file(path)?;

    let (device_id, inode) = match file_id {
        FileId::Inode { device_id, inode_number } => Ok((device_id, inode_number)),
        FileId::LowRes { volume_serial_number, file_index } => Ok((volume_serial_number as u64, file_index)),
        FileId::HighRes { .. } => Err("High resolution file IDs are not supported.".to_string()),
    }?;

    let (user_id, group_id) = file_utils::get_owners_for_file(&metadata);
    let blob = object_utils::get_blob_for_file(name, path, &item_type)?;
    let file_size = blob.content.len() as u64;

    let object = LooseObjectBuilder::build_blob(&blob);

    let item = InventoryItem {
        metadata_change_timestamp: ctime,
        content_change_timestamp: mtime,
        device: device_id,
        inode,
        item_type,
        user_id,
        group_id,
        file_size,
        hash: object.hash.clone(),
        file_name_length: name.len() as u64,
        state: InventoryItemState::Normal,
        name: String::from(name),
    };

    Ok((item, object))
}

/// The verdict of classifying one on-disk file against its existing inventory entry.
/// This is the shared per-file core of the per-directory merge-join (§3.2.1): `load` and
/// the unstaged stocktake walk both classify with it, so their verdicts can never drift
/// apart. The verdict carries facts, not policy — what an untracked file or a staged
/// removal *means* stays with the caller.
pub enum FileVerdict {
    /// The stat cache proves the entry still matches the file — nothing was read or hashed.
    UnchangedByStat,

    /// The stat cache missed, but the content hash matches the entry: the file is
    /// unchanged. Carries the rebuilt item (same hash, fresh stat data) and the unstored
    /// blob object — writers store it anyway (a cheap no-op when present), which is what
    /// makes a re-load heal a blob that went missing from the object store.
    UnchangedByHash(InventoryItem, LooseObject),

    /// The content changed. Carries the rebuilt item (new hash, fresh stat data) and the
    /// unstored blob object, so a writer can store it without reading the file again —
    /// and a read-only caller simply drops it.
    Modified(InventoryItem, LooseObject),
}

/// Classify one on-disk file against its existing inventory entry: the stat-cache fast
/// path first (see `is_entry_unchanged`, including the racily-clean protection), then a
/// read-and-hash comparison. Nothing is written to the object store or the inventory.
///
/// # Arguments
/// * `existing`    - The inventory entry the file is compared against.
/// * `metadata`    - The current (symlink) metadata of the file.
/// * `item_type`   - The current type of the directory entry.
/// * `path`        - The path of the file.
/// * `name`        - The name of the file.
/// * `shard_mtime` - The modification timestamp of the shard the entry came from.
///
/// # Returns
/// * `Ok(FileVerdict)` - The verdict.
/// * `Err(String)`     - If the file could not be read or stat'ed.
pub fn classify_file_against_entry(existing: &InventoryItem,
                                   metadata: &std::fs::Metadata,
                                   item_type: DirEntryType,
                                   path: &Path,
                                   name: &str,
                                   shard_mtime: u64) -> Result<FileVerdict, String> {
    if is_entry_unchanged(existing, metadata, item_type, path, shard_mtime) {
        return Ok(FileVerdict::UnchangedByStat);
    }

    let (item, object) = build_item_and_object_for_file(path, name, item_type)?;

    if item.hash == existing.hash {
        Ok(FileVerdict::UnchangedByHash(item, object))
    } else {
        Ok(FileVerdict::Modified(item, object))
    }
}

/// Add a single file to its corresponding inventory file.
/// If the file is already in the inventory, its entry is updated.
///
/// # Arguments
/// * `path` - The path of the file.
///
/// # Returns
/// * `Ok(())`      - If the file was successfully added to the inventory.
/// * `Err(String)` - If there was an error during the operation.
fn add_file_to_inventory(path: &WarehousePath) -> Result<(), String> {
    let (parent, file_name) = path.split_parent()?;

    let (inventory_path, mut inventory) = retrieve_inventory_or_empty(&parent)?;

    let fs_path = path.to_fs_path();
    let file_metadata = file_utils::get_symlink_metadata_for_path(&fs_path)?;

    let item = build_inventory_item_from_file(
        &fs_path,
        &file_name,
        file_utils::get_type_of_dir_entry(&file_metadata)
    )?;

    inventory.add_item(item);

    save_inventory(&inventory, &inventory_path)?;

    let mut new_items: BTreeSet<String> = BTreeSet::new();
    new_items.insert(parent.as_key().to_string());

    update_inventory_metadata(&new_items, &BTreeSet::new())?;

    Ok(())
}

/// Stage the removal of a single file: mark its entry in its parent's inventory as `Deleted`.
/// Staging the removal of a file that is already staged for removal is a no-op that
/// still succeeds.
///
/// # Arguments
/// * `path` - The path of the file whose removal should be staged.
///
/// # Returns
/// * `Ok(())`      - If the removal was staged successfully.
/// * `Err(String)` - If the file is not in the inventory, or there was an error.
fn stage_removal_for_file(path: &WarehousePath) -> Result<(), String> {
    let (parent, file_name) = path.split_parent()?;

    let (inventory_path, inventory_bytes) = file_utils::retrieve_inventory_or_none_by_key(parent.as_key())?;
    let mut inventory = match inventory_bytes {
        Some(bytes) => parser::inventory::inventory_parser::parse_inventory(&bytes)?,
        None => return Err(format!("\"{}\" is not in the inventory.", path.as_key())),
    };

    if !inventory.mark_item_deleted(&file_name) {
        return Err(format!("\"{}\" is not in the inventory.", path.as_key()));
    }

    save_inventory(&inventory, &inventory_path)?;

    Ok(())
}

/// Retrieve the associated inventory for the given directory
/// (or an empty inventory, if it does not have one yet).
///
/// # Arguments
/// * `parent` - The warehouse path of the directory.
///
/// # Returns
/// * `Ok((PathBuf, Inventory))`:
///    * `PathBuf`   - The path to the inventory file (if the inventory file was not found, this is
///                    the path where it should have been).
///    * `Inventory` - The inventory found (or an empty inventory otherwise).
/// * `Err(String)` - If there was an error.
fn retrieve_inventory_or_empty(parent: &WarehousePath) -> Result<(std::path::PathBuf, Inventory), String> {
    let (inventory_path, inventory_bytes) = file_utils::retrieve_inventory_or_none_by_key(parent.as_key())?;

    let inventory = match inventory_bytes {
        Some(bytes) => parser::inventory::inventory_parser::parse_inventory(&bytes)?,
        None => Inventory::new(),
    };

    Ok((inventory_path, inventory))
}

/// Save the given inventory to the given path.
///
/// # Arguments
/// * `inventory`      - The inventory data that should be written to the file.
/// * `inventory_path` - The file path of the inventory file (including file name).
///
/// # Returns
/// * `Ok(())`      - If the inventory was saved successfully.
/// * `Err(String)` - If there was an error.
fn save_inventory(inventory: &Inventory, inventory_path: &Path) -> Result<(), String> {
    let bytes = InventoryBuilder::build(inventory);
    let mut parent_path = std::path::PathBuf::from(inventory_path);
    parent_path.pop();

    file_utils::create_folder_if_not_exists(&parent_path)?;

    std::fs::write(inventory_path, bytes).map_err(|e|
        format!("Error while writing inventory to file {}", e)
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, inode: u64, state: InventoryItemState) -> InventoryItem {
        InventoryItem {
            metadata_change_timestamp: 0,
            content_change_timestamp: 0,
            device: 1,
            inode,
            item_type: DirEntryType::Normal,
            user_id: 0,
            group_id: 0,
            file_size: 0,
            hash: "hash".to_string(),
            file_name_length: name.len() as u64,
            state,
            name: name.to_string(),
        }
    }

    #[test]
    fn carry_over_marks_missing_entries_as_deleted() {
        let mut old_inventory = Inventory::new();
        old_inventory.add_item(item("kept.txt", 1, InventoryItemState::Normal));
        old_inventory.add_item(item("gone.txt", 2, InventoryItemState::Normal));

        // The rebuilt inventory only found "kept.txt" on disk.
        let mut new_inventory = Inventory::new();
        new_inventory.add_item(item("kept.txt", 1, InventoryItemState::Normal));

        carry_over_missing_entries_as_deleted(&old_inventory, &mut new_inventory);

        assert_eq!(new_inventory.get_items_count(), 2);
        assert!(new_inventory.get_item_by_name("kept.txt").unwrap().state == InventoryItemState::Normal);
        assert!(new_inventory.get_item_by_name("gone.txt").unwrap().state == InventoryItemState::Deleted);
    }

    #[test]
    fn carry_over_keeps_already_staged_removals() {
        let mut old_inventory = Inventory::new();
        old_inventory.add_item(item("unloaded.txt", 1, InventoryItemState::Deleted));

        let mut new_inventory = Inventory::new();

        carry_over_missing_entries_as_deleted(&old_inventory, &mut new_inventory);

        assert!(new_inventory.get_item_by_name("unloaded.txt").unwrap().state == InventoryItemState::Deleted);
    }
}
