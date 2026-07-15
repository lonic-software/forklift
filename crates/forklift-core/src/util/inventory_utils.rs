use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use file_id::FileId;
use regex::Regex;
use crate::builder::inventory::InventoryBuilder;
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
use crate::util::object_utils::IngestMode;
use crate::util::path_utils::WarehousePath;

/// The metadata entry used for the warehouse root (its key is the empty string,
/// which would be confusing as a line in the metadata file).
const METADATA_ENTRY_ROOT: &str = "./";

/// Serializes the read-modify-write step of rollup maintenance (clearing an ancestor's rollup,
/// or writing a mutated shard) against every other such step in this process.
///
/// This is needed because `load`'s per-directory walker (`build_inventory`, below) is genuinely
/// concurrent: a directory's task fires off its subdirectories' tasks and then writes its own
/// shard without waiting for them (see the task's own doc comment). So a directory's shard can
/// be mid-write on one task while a descendant's task is independently walking up to invalidate
/// that same directory's rollup as one of its ancestors. Without serializing the two, a
/// read-modify-write ancestor clear could read stale content, race a sibling task's fresh
/// rewrite of the same file, and lose that rewrite's real content changes — not just its rollup.
///
/// The lock is held only across the (cheap) parse-decide-rewrite of a shard file, never across
/// the (expensive) filesystem walk, hashing or object I/O that produces the content to write —
/// so it costs real parallelism only for that narrow step, and is uncontended (near-free)
/// outside `load`, where every other writer touches shards one at a time to begin with.
static SHARD_MUTATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The warehouse path keys of every strict ancestor of `key`, from the root (`""`) down to
/// (but not including) `key` itself.
fn ancestor_keys_root_first(key: &str) -> Vec<String> {
    let mut chain: Vec<String> = Vec::new();
    let mut current = key;

    while !current.is_empty() {
        current = current.rsplit_once('/').map(|(parent, _)| parent).unwrap_or("");
        chain.push(current.to_string());
    }

    chain.reverse();
    chain
}

/// Clear a single shard's rollup hash on disk, if it exists and currently has one. A missing
/// shard, or a shard whose rollup is already `None`, is left untouched — not even rewritten.
/// Callers must hold [`SHARD_MUTATION_LOCK`].
fn clear_single_shard_rollup_locked(key: &str) -> Result<(), String> {
    let (shard_path, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

    let Some(bytes) = bytes_opt else {
        return Ok(());
    };

    let mut inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
        .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?;

    if inventory.get_rollup_hash().is_none() {
        return Ok(());
    }

    inventory.set_rollup_hash(None);
    save_inventory(&inventory, &shard_path)
}

/// Clear the rollup hash of every existing ancestor shard of `key`, from the root down to (but
/// not including) `key` itself. Must run to completion before a caller writes new content at
/// `key` — see [`write_shard_mutation`], the funnel that does both in the correct order. Public
/// on its own for a caller that removes (rather than rewrites) the shard at `key` — e.g.
/// `remove_inventories_under` — which still needs its ancestors invalidated but has no new
/// content of its own to write there.
///
/// # Arguments
/// * `key` - The warehouse path key whose ancestors should be invalidated.
///
/// # Returns
/// * `Ok(())`      - Every existing ancestor shard's rollup is now cleared on disk.
/// * `Err(String)` - If an ancestor shard could not be read, parsed or written.
pub fn clear_ancestor_rollups(key: &str) -> Result<(), String> {
    let _guard = SHARD_MUTATION_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    for ancestor_key in ancestor_keys_root_first(key) {
        clear_single_shard_rollup_locked(&ancestor_key)?;
    }

    Ok(())
}

/// Write a shard whose effective content (any entry's name/type/hash/state) just changed: the
/// rollup hash of every ancestor shard is cleared first, top-down (root first), then this
/// shard's own rollup is cleared and its new content written.
///
/// Every writer that changes a shard's effective content must go through this instead of
/// writing the shard directly — a direct write would leave a stale-but-still-matching rollup on
/// an ancestor above the change, silently hiding it from a future rollup-based skip
/// (DESIGN.html §5.0 D item 8). A writer that only refreshes stat data (mtime/ctime/inode) for
/// an otherwise-identical entry should *not* go through this — it may write the shard directly
/// and carry its existing rollup forward unchanged.
///
/// Ordering matters for crash safety: nothing above the mutated shard is ever left stale once
/// this shard's write is durable, because every ancestor is cleared first; a crash before this
/// shard's write only costs a few lost skips (ancestors cleared for a mutation that, from disk's
/// perspective, never actually happened yet), never a wrong one.
///
/// # Arguments
/// * `key`       - The warehouse path key of the shard being written.
/// * `inventory` - The new content. Its rollup hash is overwritten with `None` here — callers
///                 never need to (and must not) set it themselves.
///
/// # Returns
/// * `Ok(())`      - Every ancestor was invalidated and the shard was written.
/// * `Err(String)` - If an ancestor or this shard could not be read, parsed or written.
pub fn write_shard_mutation(key: &str, inventory: &mut Inventory) -> Result<(), String> {
    let _guard = SHARD_MUTATION_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    for ancestor_key in ancestor_keys_root_first(key) {
        clear_single_shard_rollup_locked(&ancestor_key)?;
    }

    inventory.set_rollup_hash(None);
    save_inventory(inventory, &file_utils::get_inventory_data_path_for_key(key))
}

/// Whether two inventories carry the same *effective* content — the same set of entries, each
/// with the same name, type, hash and state. Stat fields (timestamps, device/inode, size,
/// ownership) are deliberately excluded: they can differ (e.g. after a stat-only refresh)
/// without the tree `stack` would build from either inventory differing at all. Used to decide
/// whether a shard rewrite may carry its existing rollup forward instead of invalidating it.
fn inventory_content_matches(a: &Inventory, b: &Inventory) -> bool {
    a.get_items_count() == b.get_items_count()
        && a.get_items().all(|(name, item)| {
            b.get_item_by_name(name).is_some_and(|other| {
                other.hash == item.hash && other.item_type == item.item_type && other.state == item.state
            })
        })
}

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
    let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

    let Some(bytes) = bytes_opt else {
        return Ok(false);
    };

    let mut inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
        .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?;

    if inventory.mark_all_items_deleted() {
        write_shard_mutation(key, &mut inventory)?;
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
                            &item, &metadata, item_type, &entry.path(), &name, shard_mtime,
                            IngestMode::Store,
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
                            // store comes back on the next re-load. A chunked file's objects
                            // were already stored during ingest (`object` is `None`).
                            FileVerdict::UnchangedByHash(fresh, object)
                                | FileVerdict::Modified(fresh, object) => {
                                if let Some(mut object) = object {
                                    object.store()?;
                                }
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

        // A pure stat-cache refresh (every entry's name/type/hash/state is exactly what the
        // previous shard already had) never changes the tree `stack` would build here: carry
        // the previous rollup forward instead of invalidating it and every ancestor above it.
        // Anything else (a real change, or a brand new shard) goes through the funnel.
        match existing_inventory.as_ref() {
            Some((old_inventory, _)) if inventory_content_matches(&inventory, old_inventory) => {
                inventory.set_rollup_hash(old_inventory.get_rollup_hash().cloned());
                let inventory_data_path = file_utils::get_inventory_data_path_for_key(path.as_key());
                save_inventory(&inventory, &inventory_data_path)?;
            }
            _ => {
                write_shard_mutation(path.as_key(), &mut inventory)?;
            }
        }

        Ok(())
    }
}

/// Build an inventory item for a file whose blob is already stored (its hash is known,
/// e.g. from a tree object): only the file's metadata is gathered, nothing is read or
/// hashed. Used when repopulating the inventory after materializing a tree.
///
/// # Arguments
/// * `path`      - The path of the file.
/// * `name`      - The name of the file.
/// * `hash`      - The (already known) blob or recipe hash of the file's content.
/// * `item_type` - The entry type from the authoritative source (the tree / merge action). It is
///   **not** re-derived from `stat`: a `stat` cannot tell a chunked file from a plain one
///   (chunking is a storage choice), so the tree's `NormalChunked`/`ExecutableChunked` must be
///   carried through here or the next stack would emit a wrong (plain) tree entry over a recipe
///   hash. For a plain file this equals what `stat` reports; for a symlink it is `SymbolicLink`.
///
/// # Returns
/// * `Ok(InventoryItem)` - The inventory item.
/// * `Err(String)`       - If the file's metadata could not be gathered.
pub fn build_inventory_item_from_stat(path: &Path,
                                      name: &str,
                                      hash: String,
                                      item_type: DirEntryType) -> Result<InventoryItem, String> {
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

/// Stage a fresh inventory entry (with current stat data) for a file whose blob or recipe is
/// already stored (e.g. one just written from a tree or merge).
///
/// # Arguments
/// * `path`      - The warehouse path of the file.
/// * `hash`      - The blob or recipe hash of the file's content.
/// * `item_type` - The authoritative entry type (from the tree / merge action), carried through
///   so a chunked entry keeps its `*Chunked` type in the inventory rather than being demoted to
///   a plain type a `stat` would report.
///
/// # Returns
/// * `Ok(())`      - If the entry was staged.
/// * `Err(String)` - If the file's metadata could not be gathered or the shard written.
pub fn stage_file_entry_from_stat(path: &str, hash: String, item_type: DirEntryType) -> Result<(), String> {
    let (parent_key, name) = match path.rsplit_once(file_utils::PATH_SEPARATOR_CHAR) {
        Some((parent, name)) => (parent, name),
        None => ("", path),
    };

    let entry = build_inventory_item_from_stat(Path::new(path), name, hash, item_type)?;

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
        // Whether any entry's *effective* content (hash/type/state) actually changed, as
        // opposed to only its stat fields going stale (e.g. a re-save with identical bytes).
        // Only a real content change invalidates the rollup — see `write_shard_mutation`.
        let mut content_changed = false;

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
                content_changed = true;
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

            if rebuilt.hash != item.hash || rebuilt.item_type != item.item_type || rebuilt.state != item.state {
                content_changed = true;
            }

            inventory.add_item(rebuilt);
        }

        if content_changed {
            write_shard_mutation(key, &mut inventory)?;
        } else if changed {
            // Only stat data went stale: the tree `stack` would build from this shard is
            // unchanged, so its (and its ancestors') rollup stays valid as-is.
            save_inventory(&inventory, &shard_path)?;
        }
    }

    Ok(())
}

/// A single read-and-parse pass over every registered inventory shard.
///
/// Built once per `stack` (`prepare_stack_inventory`) and shared by three steps that used to
/// each read+parse the whole shard set independently — `has_conflict_entries`,
/// `build_tree_from_inventory` and `cleanup_after_stack` (§ perf: on a large tree this was three
/// full O(shard count) passes over the same on-disk state per stacked parcel). See
/// `stack_utils::stack_parcel`: it builds this once, checks conflicts on it (still strictly
/// before any warehouse mutation — parse-then-check-then-write is preserved), threads it into
/// the tree build, and reuses it again for the post-stack cleanup's rewrite decision. Every
/// other caller of the three originals (`park`, in particular) is unaffected: it still reads
/// fresh, exactly as before.
///
/// Held only for the duration of one `stack_parcel` call — the same transient window
/// `build_tree_from_inventory`'s own parallel read pass already held the parsed shards for, so
/// peak memory retention is unchanged (this does not hold anything longer than the code already
/// did; it just stops re-reading and re-parsing the same bytes two more times).
pub struct PreparedInventory {
    /// `None` when there is no inventory metadata file at all (nothing was ever loaded).
    /// `Some` (possibly empty) mirrors the metadata file's own registered directory keys —
    /// kept distinct from an empty set purely so callers can reproduce the exact behavior of
    /// the original functions where "no file" and "empty file" diverge (e.g.
    /// `cleanup_after_stack_with`'s early return when there was never a file to rewrite).
    pub metadata: Option<BTreeSet<String>>,

    /// Every registered directory's parsed inventory, keyed by its warehouse path key. A key
    /// present in `metadata` but absent here means its shard file could not be found on disk
    /// (a stale metadata entry) — every original caller silently skips such an entry, and so
    /// does everything built on this snapshot.
    ///
    /// Incomplete (missing entries for keys past the one that tripped it) when `has_conflict` is
    /// `true` — see [`prepare_stack_inventory`]'s doc comment. Harmless: `stack_parcel` aborts on
    /// a conflict before this is ever consulted again.
    pub shards: BTreeMap<String, Inventory>,

    /// Whether a conflict entry (an unresolved consolidation) was found while parsing. Set — and
    /// the scan stopped — at the *first* shard (in sorted key order) found to have one; see
    /// [`prepare_stack_inventory`]'s doc comment for why.
    pub has_conflict: bool,
}

/// Read the inventory metadata file and parse every registered shard exactly once. See
/// [`PreparedInventory`] for why (`stack` used to pay this read+parse cost three times per
/// parcel).
///
/// Checks each shard for a conflict entry immediately after parsing it, and stops at the first
/// one found, rather than finishing the full parse pass first and checking afterwards. This
/// matters because it is not just an optimization: the old, still-current `has_conflict_entries`
/// short-circuits the same way (check-then-parse-then-check, one shard at a time, in sorted
/// order), so a real, actionable conflict in an earlier shard is always what a user sees first.
/// A single pass that parsed every shard *before* checking any of them would let an unrelated
/// corrupt shard later in the (sorted) set mask that conflict behind a parse error instead —
/// stopping at the first conflict preserves the original prioritization exactly (and the
/// unparsed remainder is never needed: `stack_parcel` aborts on a conflict before the tree build
/// or cleanup ever consult the rest of this snapshot).
///
/// # Returns
/// * `Ok(PreparedInventory)` - The snapshot (empty when there is nothing staged; incomplete, with
///                             `has_conflict` set, when a conflict was found and the scan
///                             stopped there).
/// * `Err(String)`           - If the metadata file, or a shard scanned before any conflict was
///                             found, could not be read or parsed.
pub fn prepare_stack_inventory() -> Result<PreparedInventory, String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    let Some(metadata) = metadata_opt else {
        return Ok(PreparedInventory { metadata: None, shards: BTreeMap::new(), has_conflict: false });
    };

    let mut shards: BTreeMap<String, Inventory> = BTreeMap::new();

    for entry in &metadata {
        let key = metadata_entry_to_key(entry);
        let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

        let Some(bytes) = bytes_opt else { continue; };

        let inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
            .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?;

        let has_conflict = inventory.get_items().any(|(_, item)| matches!(
            item.state,
            InventoryItemState::FirstParentConflict
                | InventoryItemState::SecondParentConflict
                | InventoryItemState::ThirdParentConflict
        ));

        shards.insert(key.to_string(), inventory);

        if has_conflict {
            return Ok(PreparedInventory { metadata: Some(metadata), shards, has_conflict: true });
        }
    }

    Ok(PreparedInventory { metadata: Some(metadata), shards, has_conflict: false })
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

/// Like [`has_conflict_entries`], but against an already-parsed [`PreparedInventory`] snapshot
/// instead of reading and parsing the shards again. Used by `stack` (`stack_utils::stack_parcel`),
/// which must still abort before any warehouse mutation when this is true.
///
/// A thin accessor: [`prepare_stack_inventory`] already determines this — and stops scanning as
/// soon as it does, so a real conflict is never masked by a later shard's parse error — so there
/// is nothing left to scan here.
///
/// # Returns
/// * `true`  - At least one entry is in conflict.
/// * `false` - No entry is in conflict.
pub fn has_conflict_entries_in(prepared: &PreparedInventory) -> bool {
    prepared.has_conflict
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
    let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

    let mut inventory = match bytes_opt {
        Some(bytes) => parser::inventory::inventory_parser::parse_inventory(&bytes)
            .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?,
        None => Inventory::new(),
    };

    change(&mut inventory)?;

    write_shard_mutation(key, &mut inventory)?;

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
    // The subtree at `key` is about to be replaced wholesale — a content change from any
    // ancestor's point of view — so their rollups (if any) must be cleared before the new
    // content lands, exactly as `write_shard_mutation` orders it. `shards` itself carries
    // correctly-stamped rollups already (the caller builds them from a known tree), so nothing
    // further is needed for `key` and below.
    clear_ancestor_rollups(key)?;

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
/// Reuses an already-parsed [`PreparedInventory`] snapshot — the same one `stack` used for its
/// conflict check and tree build — instead of reading and parsing every shard a third time (§
/// perf; see [`PreparedInventory`]). The snapshot was taken before `stack` wrote anything, but
/// nothing between then and this call mutates a shard's *content* on disk (tree/parcel objects,
/// the pallet ref and the signature sidecar are the only writes stacking does before this point,
/// none of them touch an inventory shard), so reusing it here produces exactly the same result
/// as a fresh read would. The directory-existence checks below are unaffected — they always
/// read the working directory fresh.
///
/// After a stack, staged state *is* the new head — so every surviving shard's rollup is stamped
/// with its just-built subtree hash from `stamp_hashes` (warehouse path key → hash, from the
/// same tree build `stack` just ran; a key absent from the map — an out-of-scope/spine shard in
/// a scoped bay, or a genuinely empty subtree — is stamped `None` instead of guessed).
///
/// # Arguments
/// * `prepared`     - The snapshot from [`prepare_stack_inventory`].
/// * `stamp_hashes` - Warehouse path key → the subtree hash `stack` just built there. Trusted
///                    for every key it names (see `stack_utils::stack_parcel`, which omits any
///                    key a scoped bay's spine splice could have changed).
///
/// # Returns
/// * `Ok(())`      - If the cleanup completed.
/// * `Err(String)` - If a shard could not be written, or a folder could not be removed.
pub fn cleanup_after_stack_with(prepared: &PreparedInventory,
                                stamp_hashes: &BTreeMap<String, String>) -> Result<(), String> {
    let Some(metadata) = &prepared.metadata else {
        return Ok(());
    };

    let mut removed_keys: BTreeSet<String> = BTreeSet::new();

    for entry in metadata {
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

        let Some(inventory) = prepared.shards.get(key) else {
            continue;
        };

        let has_staged_removals = inventory.get_items()
            .any(|(_, item)| item.state == InventoryItemState::Deleted);

        let target_rollup = stamp_hashes.get(key).cloned();

        // A rewrite is needed either to drop the now-consumed `Deleted` entries (unchanged
        // behavior — dropping them never changes the tree `stack` already built, since the tree
        // build itself excludes `Deleted` entries) or, even with nothing to drop, purely to
        // stamp a rollup this shard did not already carry.
        if has_staged_removals || inventory.get_rollup_hash() != target_rollup.as_ref() {
            let mut rebuilt = Inventory::new();

            for (_, item) in inventory.get_items() {
                if item.state != InventoryItemState::Deleted {
                    rebuilt.add_item((**item).clone());
                }
            }

            rebuilt.set_rollup_hash(target_rollup);
            save_inventory(&rebuilt, &file_utils::get_inventory_data_path_for_key(key))?;
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

    // Conservative: the removed shards themselves have no content left to invalidate (their
    // files are about to be deleted below), but every ancestor above `prefix` may have had a
    // rollup describing a subtree that included them — clear those before anything is removed.
    clear_ancestor_rollups(prefix)?;

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
    // Compare the on-disk kind, not the chunked storage decision: `item_type` is derived from a
    // fresh `stat` and can only ever be `Normal`/`Executable`/`SymbolicLink`, while the inventory
    // entry may hold a `*Chunked` variant for the same file. Chunking is a storage choice a stat
    // cannot see, so an unchanged giant (stat says `Normal`, inventory says `NormalChunked`) must
    // still hit this fast path and never be re-chunked. A genuine normal↔executable flip is still
    // caught (their on-disk kinds differ).
    if existing.item_type.on_disk_kind() != item_type.on_disk_kind() {
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
    // A first-time `load` of a file: persist its objects. A small file returns its blob for us to
    // store; a chunked file already stored its chunks and recipe during ingest (`None`).
    let (item, object) = build_item_and_object_for_file(path, name, item_type, IngestMode::Store)?;

    if let Some(mut object) = object {
        object.store()?;
    }

    Ok(item)
}

/// Create an inventory item for a file, together with the built-but-unstored blob object for a
/// small file. A file at or above the chunk threshold is ingested as a recipe plus chunks
/// instead (its entry type becomes a `*Chunked` variant, its hash is the recipe hash) — those
/// objects are handled per `mode`, so the returned object is `None` for a chunked file.
///
/// The read-only stocktake/diff caller passes `IngestMode::ComputeOnly` (nothing is written to
/// the store, not even a chunked giant's chunks); `load`/`park` pass `IngestMode::Store`. Either
/// way the returned blob (small files only) is unstored — writers store it, read-only callers
/// drop it, exactly as before.
///
/// # Arguments
/// * `path`      - The path of the file.
/// * `name`      - The name of the file.
/// * `item_type` - The type of the directory entry (a `*Chunked` upgrade is decided here).
/// * `mode`      - Whether to persist a chunked file's objects or only compute their hashes.
///
/// # Returns
/// * `Ok((InventoryItem, Option<LooseObject>))` - The item, and the unstored blob for a small
///   file (`None` for a chunked file, whose objects were handled per `mode`).
/// * `Err(String)`                              - If the file could not be read or stat'ed.
fn build_item_and_object_for_file(path: &Path,
                                  name: &str,
                                  item_type: DirEntryType,
                                  mode: IngestMode)
                                  -> Result<(InventoryItem, Option<LooseObject>), String> {
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
    let ingested = object_utils::ingest_file(name, path, item_type, mode)?;

    let item = InventoryItem {
        metadata_change_timestamp: ctime,
        content_change_timestamp: mtime,
        device: device_id,
        inode,
        item_type: ingested.item_type,
        user_id,
        group_id,
        file_size: ingested.file_size,
        hash: ingested.hash,
        file_name_length: name.len() as u64,
        state: InventoryItemState::Normal,
        name: String::from(name),
    };

    Ok((item, ingested.deferred))
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
    /// unchanged. Carries the rebuilt item (same hash, fresh stat data) and, for a small
    /// file, the unstored blob object — writers store it anyway (a cheap no-op when present),
    /// which is what makes a re-load heal a blob that went missing from the object store. A
    /// chunked file carries `None` (its chunks/recipe were handled per the ingest mode).
    UnchangedByHash(InventoryItem, Option<LooseObject>),

    /// The content changed. Carries the rebuilt item (new hash, fresh stat data) and, for a
    /// small file, the unstored blob object, so a writer can store it without reading the file
    /// again — and a read-only caller simply drops it. A chunked file carries `None`.
    Modified(InventoryItem, Option<LooseObject>),
}

/// Classify one on-disk file against its existing inventory entry: the stat-cache fast
/// path first (see `is_entry_unchanged`, including the racily-clean protection), then a
/// read-and-hash comparison. Nothing is written to the object store or the inventory.
///
/// # Arguments
/// * `existing`    - The inventory entry the file is compared against.
/// * `metadata`    - The current (symlink) metadata of the file.
/// * `item_type`   - The current type of the directory entry (the filesystem-visible kind).
/// * `path`        - The path of the file.
/// * `name`        - The name of the file.
/// * `shard_mtime` - The modification timestamp of the shard the entry came from.
/// * `mode`        - Whether a re-chunked giant's objects are stored (`load`) or only hashed
///   (read-only stocktake/diff).
///
/// # Returns
/// * `Ok(FileVerdict)` - The verdict.
/// * `Err(String)`     - If the file could not be read or stat'ed.
pub fn classify_file_against_entry(existing: &InventoryItem,
                                   metadata: &std::fs::Metadata,
                                   item_type: DirEntryType,
                                   path: &Path,
                                   name: &str,
                                   shard_mtime: u64,
                                   mode: IngestMode) -> Result<FileVerdict, String> {
    if is_entry_unchanged(existing, metadata, item_type, path, shard_mtime) {
        return Ok(FileVerdict::UnchangedByStat);
    }

    let (item, object) = build_item_and_object_for_file(path, name, item_type, mode)?;

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

    let (_, mut inventory) = retrieve_inventory_or_empty(&parent)?;

    let fs_path = path.to_fs_path();
    let file_metadata = file_utils::get_symlink_metadata_for_path(&fs_path)?;

    let item = build_inventory_item_from_file(
        &fs_path,
        &file_name,
        file_utils::get_type_of_dir_entry(&file_metadata)
    )?;

    inventory.add_item(item);

    write_shard_mutation(parent.as_key(), &mut inventory)?;

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

    let (_, inventory_bytes) = file_utils::retrieve_inventory_or_none_by_key(parent.as_key())?;
    let mut inventory = match inventory_bytes {
        Some(bytes) => parser::inventory::inventory_parser::parse_inventory(&bytes)?,
        None => return Err(format!("\"{}\" is not in the inventory.", path.as_key())),
    };

    if !inventory.mark_item_deleted(&file_name) {
        return Err(format!("\"{}\" is not in the inventory.", path.as_key()));
    }

    write_shard_mutation(parent.as_key(), &mut inventory)?;

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

    // Atomic (temp file, fsync, rename, directory fsync) — the store-wide "durable before
    // destructive" contract. This matters far more now than it used to: post-stack rollup
    // stamping (`cleanup_after_stack_with`) can rewrite every registered shard on a single
    // `stack`, not just the ones with consumed staged removals, so a shard write is on the hot
    // path of a crash-safety-sensitive operation far more often than before.
    file_utils::write_file_atomically(inventory_path, &bytes)
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
        old_inventory.add_item(item("removed.txt", 1, InventoryItemState::Deleted));

        let mut new_inventory = Inventory::new();

        carry_over_missing_entries_as_deleted(&old_inventory, &mut new_inventory);

        assert!(new_inventory.get_item_by_name("removed.txt").unwrap().state == InventoryItemState::Deleted);
    }

    /// A fresh warehouse root for one test — mirrors `journal_utils::tests::Scratch`.
    struct Scratch {
        root: std::path::PathBuf,
        _scope: crate::globals::StorageRootScope,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let root = std::env::temp_dir().join(format!(
                "forklift-inventory-test-{}-{}-{}", name, std::process::id(), id
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = crate::globals::StorageRootScope::enter(&root);

            Scratch { root, _scope: scope }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn prepare_stack_inventory_reports_a_conflict_found_before_a_later_corrupt_shard() {
        // Regression: a single up-front parse pass must not let an unrelated corrupt shard mask
        // a real, actionable conflict discovered earlier in the (sorted) scan. The old
        // `has_conflict_entries` short-circuits — check, then parse, then check, one shard at a
        // time — so a conflict in an earlier-sorting shard is always what the user sees, even
        // when a later shard happens to be corrupt. `prepare_stack_inventory` must reproduce
        // that prioritization, not just accidentally match it when there is nothing corrupt.
        let _scratch = Scratch::new("conflict-before-corrupt");

        // "aaa" sorts before "zzz" and has a genuine unresolved conflict.
        let mut conflicted = Inventory::new();
        conflicted.add_item(item("file.txt", 1, InventoryItemState::FirstParentConflict));
        save_inventory(&conflicted, &file_utils::get_inventory_data_path_for_key("aaa")).unwrap();

        // "zzz" sorts after "aaa" and is not a valid inventory shard at all.
        let corrupt_path = file_utils::get_inventory_data_path_for_key("zzz");
        std::fs::create_dir_all(corrupt_path.parent().unwrap()).unwrap();
        std::fs::write(&corrupt_path, b"not a valid inventory shard").unwrap();

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        metadata.insert("aaa".to_string());
        metadata.insert("zzz".to_string());
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        let prepared = prepare_stack_inventory()
            .expect("the conflict in \"aaa\" must be reported, not \"zzz\"'s parse error");

        assert!(prepared.has_conflict, "the conflict found while scanning must be recorded");
        assert!(has_conflict_entries_in(&prepared));

        // The scan stopped at "aaa": "zzz" was never reached, so it is absent from the snapshot
        // rather than having failed to parse into it.
        assert!(prepared.shards.contains_key("aaa"));
        assert!(!prepared.shards.contains_key("zzz"));
    }

    #[test]
    fn prepare_stack_inventory_still_surfaces_a_corrupt_shard_when_nothing_is_in_conflict() {
        // The other half of the same fix: when there is no conflict anywhere, a corrupt shard
        // must still fail loudly (never silently skipped) — the short-circuit only changes
        // *priority* between two real problems, it must not hide either one on its own.
        let _scratch = Scratch::new("corrupt-with-no-conflict");

        let mut clean = Inventory::new();
        clean.add_item(item("file.txt", 1, InventoryItemState::Normal));
        save_inventory(&clean, &file_utils::get_inventory_data_path_for_key("aaa")).unwrap();

        let corrupt_path = file_utils::get_inventory_data_path_for_key("zzz");
        std::fs::create_dir_all(corrupt_path.parent().unwrap()).unwrap();
        std::fs::write(&corrupt_path, b"not a valid inventory shard").unwrap();

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        metadata.insert("aaa".to_string());
        metadata.insert("zzz".to_string());
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        let error = match prepare_stack_inventory() {
            Ok(_) => panic!("a corrupt shard must still fail the scan"),
            Err(message) => message,
        };
        assert!(error.contains("zzz"), "the error should name the offending shard, got: {error}");
    }

    /// Write a shard directly (bypassing the funnel) with the given rollup hash already
    /// stamped — simulates a shard as it would sit right after a `stack` (see
    /// `cleanup_after_stack_with`), without needing a real tree build.
    fn write_stamped_shard(key: &str, rollup_hash: Option<&str>) {
        let mut inventory = Inventory::new();
        inventory.add_item(item("file.txt", 1, InventoryItemState::Normal));
        inventory.set_rollup_hash(rollup_hash.map(|h| h.to_string()));
        save_inventory(&inventory, &file_utils::get_inventory_data_path_for_key(key)).unwrap();
    }

    fn read_rollup(key: &str) -> Option<String> {
        let (_, bytes) = file_utils::retrieve_inventory_or_none_by_key(key).unwrap();
        let inventory = parser::inventory::inventory_parser::parse_inventory(&bytes.unwrap()).unwrap();
        inventory.get_rollup_hash().cloned()
    }

    #[test]
    fn write_shard_mutation_clears_every_ancestor_top_down_but_spares_an_unrelated_sibling() {
        let _scratch = Scratch::new("mutation-clears-ancestors");

        // A chain of previously-stamped shards from root down to a depth-3 directory, plus an
        // unrelated sibling subtree stamped the same way.
        for key in ["", "a", "a/b", "a/b/c", "x"] {
            write_stamped_shard(key, Some("stale-rollup"));
        }

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        for key in ["", "a", "a/b", "a/b/c", "x"] {
            metadata.insert(key_to_metadata_entry(key));
        }
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        // A content-changing mutation three levels deep.
        update_shard("a/b/c", |inventory| {
            inventory.add_item(item("new.txt", 99, InventoryItemState::Normal));
            Ok(())
        }).unwrap();

        // Every ancestor, root first, is cleared — and the mutated shard itself never keeps a
        // stale rollup (the funnel always writes `None` for the shard it mutates).
        assert_eq!(read_rollup(""), None, "the root's stale rollup must be cleared");
        assert_eq!(read_rollup("a"), None, "an intermediate ancestor's stale rollup must be cleared");
        assert_eq!(read_rollup("a/b"), None, "the immediate parent's stale rollup must be cleared");
        assert_eq!(read_rollup("a/b/c"), None, "the mutated shard itself must not keep a rollup");

        // An unrelated sibling subtree is untouched.
        assert_eq!(read_rollup("x").as_deref(), Some("stale-rollup"),
            "a sibling subtree outside the mutated chain must keep its rollup");
    }

    #[test]
    fn write_shard_mutation_is_a_no_op_on_an_already_clear_ancestor() {
        // A shard whose rollup is already `None` must not be rewritten by ancestor invalidation
        // — a light regression guard for the "no-op, don't rewrite" contract, checked via the
        // shard file's absence of change (it was never given a rollup to lose either way, but
        // this at least exercises the skip path without a real tree build).
        let _scratch = Scratch::new("mutation-noop-ancestor");

        write_stamped_shard("", None);
        write_stamped_shard("a", None);

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        metadata.insert(key_to_metadata_entry(""));
        metadata.insert(key_to_metadata_entry("a"));
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        update_shard("a", |inventory| {
            inventory.add_item(item("new.txt", 1, InventoryItemState::Normal));
            Ok(())
        }).unwrap();

        assert_eq!(read_rollup(""), None);
        assert_eq!(read_rollup("a"), None);
    }

    #[test]
    fn remove_inventories_under_clears_ancestor_rollups_conservatively() {
        let _scratch = Scratch::new("narrow-clears-ancestors");

        for key in ["", "a", "a/b"] {
            write_stamped_shard(key, Some("stale-rollup"));
        }

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        for key in ["", "a", "a/b"] {
            metadata.insert(key_to_metadata_entry(key));
        }
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        remove_inventories_under("a/b").unwrap();

        assert_eq!(read_rollup(""), None, "narrow must conservatively clear the root's rollup");
        assert_eq!(read_rollup("a"), None, "narrow must conservatively clear the parent's rollup");
        assert!(!file_utils::get_inventory_data_path_for_key("a/b").exists());
    }

    #[test]
    fn build_inventory_preserves_rollup_across_a_pure_stat_refresh() {
        // The comparison helper that decides whether a rebuilt shard's content is truly
        // unchanged (so its rollup may be carried forward) versus genuinely different (so it
        // must go through the funnel).
        let mut a = Inventory::new();
        a.add_item(item("file.txt", 1, InventoryItemState::Normal));

        let mut b = Inventory::new();
        b.add_item(item("file.txt", 2, InventoryItemState::Normal)); // different inode only

        assert!(inventory_content_matches(&a, &b),
            "a stat-only difference (inode) must not count as a content change");

        let mut c = Inventory::new();
        let mut changed = item("file.txt", 1, InventoryItemState::Normal);
        changed.hash = "different-hash".to_string();
        c.add_item(changed);

        assert!(!inventory_content_matches(&a, &c), "a real hash change must count as a content change");
    }
}
