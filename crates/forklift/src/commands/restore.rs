use std::collections::BTreeMap;
use forklift_core::enums::dir_entry_type::DirEntryType;
use forklift_core::enums::inventory_item_state::InventoryItemState;
use forklift_core::model::inventory::{Inventory, InventoryItem};
use forklift_core::model::tree_item::TreeItem;
use forklift_core::util::path_utils::{self, WarehousePath};
use forklift_core::util::inventory_utils::TrackedKind;
use forklift_core::util::scope_utils::{self, MaterializationScope, ScopeClass};
use forklift_core::util::{
    alias_utils, file_utils, inventory_utils, load_guard_utils, object_utils, pallet_utils, shift_utils, tree_utils,
};
use crate::output;

/// A resolved entry of the pallet head's tree.
enum HeadEntry {
    /// A file (normal, executable or symlink) with its blob hash.
    File { hash: String, item_type: DirEntryType },

    /// A directory, loaded, together with its own tree hash (the parent's entry for it —
    /// `tree` as loaded carries no hash of its own; the root resolution uses the root tree
    /// hash directly). Needed to stamp the rebuilt shard's rollup in `build_stale_shards`.
    Tree { tree: TreeItem, hash: String },
}

/// Handle the restore command.
/// * `restore <path>`          - Rewrite the file (or every tracked file of the directory
///                               that differs from its entry) in the working directory from
///                               the inventory, discarding unstaged changes.
/// * `restore --staged <path>` - Reset the inventory entries of the path to the pallet
///                               head (unstage), leaving the working directory untouched.
///
/// # Arguments
/// * `staged` - Whether to reset the inventory entries to the pallet head (unstage)
///              instead of restoring the working directory from the inventory.
/// * `target` - The path of the file or directory to restore.
///
/// # Returns
/// * `Ok(())`      - If the restore completed successfully.
/// * `Err(String)` - If there was an error while handling the command.
pub fn handle_command(staged: bool, target: &str) -> Result<(), String> {
    if staged {
        return handle_unstage(target);
    }

    let path = WarehousePath::from_user_input(target)?;

    // An out-of-scope path is sealed by hash in a scoped bay and was never materialized;
    // restoring it would have nothing to restore from. Refuse cleanly rather than let the
    // walk below silently do the wrong thing.
    crate::commands::scope::ensure_path_in_scope(path.as_key())?;

    restore_worktree(&path)
}

/// Unstage a file or directory: reset its inventory entries to the pallet head, leaving
/// the working directory untouched. Shared by `restore --staged` and `unload`.
///
/// # Arguments
/// * `target` - The path of the file or directory to unstage.
///
/// # Returns
/// * `Ok(())`      - If the unstage completed successfully.
/// * `Err(String)` - If there was an error while handling the command.
pub fn handle_unstage(target: &str) -> Result<(), String> {
    let path = WarehousePath::from_user_input(target)?;

    // An out-of-scope path is sealed by hash in a scoped bay and was never materialized;
    // unstaging it would smuggle out-of-scope content into the inventory. Refuse cleanly
    // rather than let the walk below silently do the wrong thing.
    crate::commands::scope::ensure_path_in_scope(path.as_key())?;

    restore_staged(&path)
}

/// Restore the working directory from the inventory: rewrite the file (or, for a
/// directory, every tracked, non-`Deleted` file below it) from its staged blob, leaving a file
/// that already holds that content untouched (see `restore_entry`).
///
/// # Arguments
/// * `path` - The path to restore.
///
/// # Returns
/// * `Ok(())`      - If the restore completed.
/// * `Err(String)` - If the path is not in the inventory or a write failed.
fn restore_worktree(path: &WarehousePath) -> Result<(), String> {
    if path.is_root() {
        return restore_worktree_directory(path.as_key());
    }

    match inventory_utils::tracked_kind(path)? {
        TrackedKind::Directory => return restore_worktree_directory(path.as_key()),
        TrackedKind::UntrackedDirectory => return Err(format!("No inventory found for folder \"{}\".", path.as_key())),
        TrackedKind::File => {}
    }

    // A tracked file replaced by a folder: writing the file back would have to delete the
    // folder and whatever untracked content it holds, so refuse instead.
    if std::fs::symlink_metadata(path.to_fs_path()).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(format!(
            "Cannot restore \"{}\": the tracked file was replaced by a folder. Move or delete the folder first.",
            path.as_key()
        ));
    }

    let (parent, file_name) = path.split_parent()?;

    let Some((inventory, shard_mtime)) = read_shard(parent.as_key())? else {
        return Err(format!("\"{}\" is not in the inventory.", path.as_key()));
    };

    let Some(item) = inventory.get_item_by_name(&file_name) else {
        return Err(format!("\"{}\" is not in the inventory.", path.as_key()));
    };

    if item.state == InventoryItemState::Deleted {
        return Err(format!(
            "The removal of \"{}\" is staged; use \"unload {}\" to unstage it first.",
            path.as_key(),
            path.as_key()
        ));
    }

    let restored = restore_entry(parent.as_key(), &item, shard_mtime)?;
    let rewritten = matches!(restored, Restored::Rewritten(_));

    if let Restored::Restatted(refreshed) | Restored::Rewritten(refreshed) = restored {
        inventory_utils::update_shard(parent.as_key(), |inventory| {
            inventory.add_item(refreshed);
            Ok(())
        })?;
    }

    if rewritten {
        output::message(format!("Restored \"{}\" from the inventory.", path.as_key()));
    } else {
        output::message(format!("\"{}\" already matches the inventory.", path.as_key()));
    }

    Ok(())
}

/// Restore every tracked file of a directory (and its subdirectories) from the inventory.
///
/// # Arguments
/// * `key` - The warehouse path key of the directory.
///
/// # Returns
/// * `Ok(())`      - If the restore completed.
/// * `Err(String)` - If the directory has no inventory or a write failed.
fn restore_worktree_directory(key: &str) -> Result<(), String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;
    let metadata = metadata_opt.unwrap_or_default();

    let mut restored_any = false;
    let mut restored_count = 0usize;
    let mut matched_count = 0usize;
    let mut batch = inventory_utils::ShardMutationBatch::new();
    let mut result: Result<(), String> = Ok(());

    for entry in &metadata {
        let shard_key = inventory_utils::metadata_entry_to_key(entry);

        if !path_utils::covers(key, shard_key) {
            continue;
        }

        restored_any = true;

        let (inventory, shard_mtime) = match read_shard(shard_key) {
            Ok(Some(shard)) => shard,
            Ok(None) => continue,
            Err(e) => { result = Err(e); break; }
        };

        for (_, item) in inventory.get_items() {
            if item.state == InventoryItemState::Deleted {
                continue;
            }

            // Recorded right after `restore_entry` statted the file, as one `update_shard` per
            // file did: a file statted earlier would record an editor's save in between against
            // the staged hash.
            let recorded = restore_entry(shard_key, item, shard_mtime).and_then(|restored| match restored {
                Restored::Untouched => {
                    matched_count += 1;
                    Ok(())
                }
                Restored::Restatted(refreshed) => {
                    matched_count += 1;
                    batch.update(shard_key, |shard| shard.add_item(refreshed))
                }
                Restored::Rewritten(refreshed) => {
                    restored_count += 1;
                    batch.update(shard_key, |shard| shard.add_item(refreshed))
                }
            });

            if let Err(e) = recorded {
                result = Err(e);
                break;
            }
        }

        if result.is_err() {
            break;
        }
    }

    // What was decided before a failure is still published, unless the failure was a publish
    // itself — the failure is reported after, together with the publish's own failure if that
    // failed too. A failed publish may have recorded some, all or none of the batch's entries,
    // though every file restored before it is already on disk.
    match (result, batch.publish()) {
        (Err(first), Err(publish)) => return Err(format!("{}\n{}", first, publish)),
        (Err(e), Ok(())) | (Ok(()), Err(e)) => return Err(e),
        (Ok(()), Ok(())) => {}
    }

    if !restored_any {
        return Err(format!(
            "No inventory found for folder \"{}\".",
            load_guard_utils::display_path(key)
        ));
    }

    if matched_count == 0 {
        output::message(format!("Restored {} file(s) from the inventory.", restored_count));
    } else {
        output::message(format!(
            "Restored {} file(s) from the inventory; {} already matched it.",
            restored_count, matched_count
        ));
    }

    Ok(())
}

/// The parsed shard at `key` with the shard file's modification timestamp (what
/// `is_entry_unchanged` trusts its entries against), or `None` when it has no shard file.
fn read_shard(key: &str) -> Result<Option<(Inventory, u64)>, String> {
    let (shard_path, shard_bytes) = file_utils::retrieve_inventory_or_none_by_key(key)?;

    let Some(bytes) = shard_bytes else {
        return Ok(None);
    };

    // An unreadable timestamp trusts no entry, so every file is compared by content.
    let shard_mtime = file_utils::get_symlink_metadata_for_path(&shard_path).ok()
        .and_then(|metadata| file_utils::get_content_modification_timestamp_for_file(&metadata).ok())
        .unwrap_or(0);

    forklift_core::parser::inventory::inventory_parser::parse_inventory(&bytes)
        .map(|inventory| Some((inventory, shard_mtime)))
        .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))
}

/// What restoring one tracked entry did.
enum Restored {
    /// The file already held the staged content and the stat cache proved it: nothing was
    /// written, and the entry needs no change.
    Untouched,

    /// The file already held the staged content, but only its hash proved it: nothing was
    /// written, and the entry is to be recorded with the file's fresh stat data, so the next
    /// comparison trusts the stat cache again.
    Restatted(InventoryItem),

    /// The file was written from its blob; the entry is to be recorded with its new stat data.
    Rewritten(InventoryItem),
}

/// Restore one tracked, non-`Deleted` entry: write its file from the staged blob unless the
/// file already holds that content, so a file that matches keeps its mtime and inode (build
/// tools that watch mtimes see nothing changed). The caller records the returned entry.
///
/// Only a `Normal` entry is ever left unwritten: restoring a conflict entry writes it and
/// records it as `Normal`, which is what resolves the conflict. A file of another on-disk kind
/// (a symlink for a file, an executable for a plain file) is written even when its bytes hash
/// the same.
///
/// # Arguments
/// * `parent_key`  - The warehouse path key of the entry's directory.
/// * `item`        - The inventory entry.
/// * `shard_mtime` - The modification timestamp of the shard the entry came from.
///
/// # Returns
/// * `Ok(Restored)` - What was done.
/// * `Err(String)`  - If the path was refused or a write failed.
fn restore_entry(parent_key: &str, item: &InventoryItem, shard_mtime: u64) -> Result<Restored, String> {
    let file_path = tracked_path(parent_key, &item.name);

    ensure_restorable(&file_path)?;

    if item.state == InventoryItemState::Normal {
        if let Some(restored) = matching_content(&file_path, item, shard_mtime) {
            return Ok(restored);
        }
    }

    shift_utils::write_tracked_file(&file_path, &item.hash, item.item_type, false)?;

    // Stat right after the write: a file statted later would record an editor's save in
    // between against the staged hash.
    let refreshed = inventory_utils::build_inventory_item_from_stat(
        std::path::Path::new(&file_path),
        &item.name,
        item.hash.clone(),
        item.item_type,
    )?;

    Ok(Restored::Rewritten(refreshed))
}

/// `Untouched` or `Restatted` when the file at `file_path` already holds `item`'s content as a
/// file of the same kind, `None` when it does not or cannot be compared (it is then written).
/// The stat data a `Restatted` entry carries was read before the file's content, so an edit
/// racing the comparison is never recorded against the staged hash.
fn matching_content(file_path: &str, item: &InventoryItem, shard_mtime: u64) -> Option<Restored> {
    let path = std::path::Path::new(file_path);
    let metadata = file_utils::get_symlink_metadata_for_path(path).ok()?;

    if metadata.is_dir() {
        return None;
    }

    let item_type = file_utils::get_type_of_dir_entry(&metadata);

    if item_type.on_disk_kind() != item.item_type.on_disk_kind() {
        return None;
    }

    let verdict = inventory_utils::classify_file_against_entry(
        item, &metadata, item_type, path, &item.name, shard_mtime, object_utils::IngestMode::ComputeOnly
    ).ok()?;

    match verdict {
        inventory_utils::FileVerdict::UnchangedByStat => Some(Restored::Untouched),
        // Only the stat data may change here, never the entry's type.
        inventory_utils::FileVerdict::UnchangedByHash(refreshed, _) if refreshed.item_type == item.item_type => {
            Some(Restored::Restatted(refreshed))
        }
        _ => None,
    }
}

/// The warehouse path of the file `name` in the directory `parent_key`.
fn tracked_path(parent_key: &str, name: &str) -> String {
    if parent_key.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", parent_key, name)
    }
}

/// Refuse a path whose write would overwrite another file through an alias, or that needs a
/// directory where a file is.
///
/// # Arguments
/// * `file_path` - The warehouse path of the tracked file.
///
/// # Returns
/// * `Ok(())`      - If the path may be written.
/// * `Err(String)` - If the path was refused.
fn ensure_restorable(file_path: &str) -> Result<(), String> {
    // The inventory can hold two spellings of one file on a case-insensitive volume (a
    // case-only directory rename staged under the new name while the old one stays): writing
    // the one that is not on disk would overwrite the other through the alias. A file where the
    // path needs a directory is refused too, rather than failing inside the write.
    match alias_utils::AliasCheck::new().occupancy(file_path)? {
        alias_utils::Occupancy::Alias { occupant, .. } | alias_utils::Occupancy::Unidentified { component: occupant } => {
            Err(format!(
                "Cannot restore \"{}\": on this file system it names the same file as \"{}\", which \
                restoring would overwrite. Stage the one you mean to keep (load or unload the other) \
                first.",
                file_path, occupant
            ))
        }
        alias_utils::Occupancy::NonDirectory { occupant } => {
            Err(format!(
                "Cannot restore \"{}\": \"{}\" is not a directory. Move it aside first.",
                file_path, occupant
            ))
        }
        alias_utils::Occupancy::Vacant | alias_utils::Occupancy::Listed => Ok(()),
    }
}

/// Reset the inventory entries of the given path to the pallet head (unstage). The
/// working directory is not touched; the reset entries carry zeroed stat data, so the
/// next comparison against the working directory rehashes them.
///
/// A path that is neither in the inventory nor in the pallet head is ordinarily an error — but
/// when it also has zero staged entries anywhere beneath it, there is nothing to reset (nothing
/// is there) and nothing to abandon (nothing landed) — only whatever recorded incomplete-load
/// root `path` itself covers, if any, to clear. See `load_guard_utils::clear_recorded_under`'s
/// doc comment for the soundness argument this vacuous-heal case relies on. The governing rule is
/// the same one every other heal in this codebase follows: **this may only clear a recorded root
/// `path` itself covers — never a broader one that merely happens to cover `path`.** `path`
/// having nothing staged proves nothing about a broader recorded region beyond it, which this
/// check never looked at. A path with no marker involvement, or with real staged content
/// somewhere beneath it, keeps the ordinary error.
///
/// # Arguments
/// * `path` - The path to unstage.
///
/// # Returns
/// * `Ok(())`      - If the unstage completed (including the vacuous-heal no-op case above).
/// * `Err(String)` - If the path exists neither in the inventory nor in the head, and is not a
///                   vacuous-heal case.
fn restore_staged(path: &WarehousePath) -> Result<(), String> {
    let pallet = pallet_utils::get_current_pallet_name()?;
    let head = pallet_utils::get_pallet_head(&pallet)?;

    let head_tree_hash = match &head {
        Some(hash) => Some(object_utils::load_parcel(hash)?.tree_hash),
        None => None,
    };

    let head_entry = match &head_tree_hash {
        Some(tree_hash) => resolve_head_entry(tree_hash, path.as_key())?,
        None => None,
    };

    let treat_as_directory = path.is_root()
        || inventory_utils::is_tracked_directory(path.as_key())?
        || matches!(head_entry, Some(HeadEntry::Tree { .. }));

    if treat_as_directory {
        // Rebuild the whole subtree of the staging area from the head: directories that
        // exist only in the inventory disappear, directories that exist only in the head
        // come back (with stale stat data).
        let mut shards: BTreeMap<String, Inventory> = BTreeMap::new();

        if let Some(HeadEntry::Tree { tree, hash }) = &head_entry {
            // In a scoped bay, only in-scope directories were ever materialized — the walk
            // must not resurrect out-of-scope shards for content that was never actually
            // written to this bay's working directory.
            let scope = scope_utils::current_scope()?;

            build_stale_shards(tree, path.as_key(), hash, &mut shards, &scope)?;
        }

        inventory_utils::replace_subtree_inventories(path.as_key(), &shards)?;

        output::message(format!(
            "Unstaged \"{}\" (inventory reset to the pallet head).",
            load_guard_utils::display_path(path.as_key())
        ));

        return Ok(());
    }

    // A single file: reset its entry from the head, or drop it if the head lacks it.
    let (parent, file_name) = path.split_parent()?;

    match head_entry {
        Some(HeadEntry::File { hash, item_type }) => {
            inventory_utils::update_shard(parent.as_key(), |inventory| {
                inventory.add_item(inventory_utils::build_stale_inventory_item(
                    &file_name, hash, item_type
                ));
                Ok(())
            })?;
        }
        Some(HeadEntry::Tree { .. }) => unreachable!("directories are handled above"),
        None => {
            // Before treating this as the ordinary "neither in inventory nor head" refusal:
            // if literally nothing is staged anywhere under `path` (as either a lone file entry
            // in its parent's shard, or a directory subtree), abandoning it is a no-op — the
            // only real effect left to have is clearing whatever recorded incomplete-load root
            // `path` *itself covers* (never a broader one merely entangled with `path` — this
            // check only ever validated `path`'s own subtree, so that is the only region it may
            // ever clear; see `load_guard_utils::clear_recorded_under`'s doc comment). Calling
            // the very same function `restore --staged`'s ordinary directory-reset branch above
            // already relies on is deliberate: it is the one place this invariant is enforced,
            // not a rule this call site has to uphold on its own. `nothing_staged_under` is worth
            // paying even when no marker turns out to be involved (`healed` then comes back empty
            // and this falls through to the unchanged ordinary path below) — it is a metadata
            // scan plus, at most, a few small shard reads, not proportional to warehouse size.
            if nothing_staged_under(path)? {
                let healed = load_guard_utils::clear_recorded_under(path.as_key());

                if !healed.is_empty() {
                    let roots = healed.iter()
                        .map(|key| format!("\"{}\"", load_guard_utils::display_path(key)))
                        .collect::<Vec<_>>()
                        .join(", ");

                    output::message(format!(
                        "Nothing was staged under \"{}\"; cleared the incomplete-load record for {}.",
                        path.as_key(), roots
                    ));

                    return Ok(());
                }
            }

            let mut removed = false;

            inventory_utils::update_shard(parent.as_key(), |inventory| {
                removed = inventory.remove_item_by_name(&file_name);
                Ok(())
            })?;

            if !removed {
                return Err(format!(
                    "\"{}\" is neither in the inventory nor in the pallet head.",
                    path.as_key()
                ));
            }
        }
    }

    output::message(format!("Unstaged \"{}\".", path.as_key()));

    Ok(())
}

/// Whether literally nothing is staged under `path`, checked both ways `path` could name
/// something: as a plain file entry inside its parent's shard, or as a directory whose subtree
/// (its own shard, or any nested one) carries at least one entry (of *any* state, including
/// `Deleted` — a staged removal is still real staged content, not nothing).
///
/// # Returns
/// * `Ok(true)`    - Nothing is staged under `path`, in either sense.
/// * `Ok(false)`   - A parent-shard file entry, or at least one entry in some covered shard,
///                   exists.
/// * `Err(String)` - A shard could not be read or parsed.
fn nothing_staged_under(path: &WarehousePath) -> Result<bool, String> {
    if !path.is_root() {
        let (parent, file_name) = path.split_parent()?;
        let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(parent.as_key())?;

        if let Some(bytes) = bytes_opt {
            let inventory = forklift_core::parser::inventory::inventory_parser::parse_inventory(&bytes)?;

            if inventory.get_item_by_name(&file_name).is_some() {
                return Ok(false);
            }
        }
    }

    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;
    let Some(metadata) = metadata_opt else { return Ok(true) };

    for entry in &metadata {
        let shard_key = inventory_utils::metadata_entry_to_key(entry);

        if !path_utils::covers(path.as_key(), shard_key) {
            continue;
        }

        let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(shard_key)?;

        let Some(bytes) = bytes_opt else { continue };
        let inventory = forklift_core::parser::inventory::inventory_parser::parse_inventory(&bytes)?;

        if inventory.get_items_count() > 0 {
            return Ok(false);
        }
    }

    Ok(true)
}

/// Build stale-stat inventory shards for a head subtree (see `build_stale_inventory_item`).
///
/// Scope-aware: only in-scope content is ever written to a scoped bay's working
/// directory, so restoring "to head" must not resurrect shards for out-of-scope files or
/// subtrees — those are sealed by hash and were never materialized here. A head file where
/// the scope expects a directory (a spine ancestor, or an in-scope prefix itself) is the
/// §3.1 type-change: refuse rather than guess, exactly like the stack overlay does.
///
/// # Arguments
/// * `tree`      - The (loaded) head tree of the directory.
/// * `key`       - The warehouse path key of the directory.
/// * `tree_hash` - The hash of `tree` itself (its parent's entry for it — `tree` as loaded
///                 carries no hash of its own; the caller passes the resolved head subtree
///                 hash for the root of the walk).
/// * `shards`    - The collected shards.
/// * `scope`     - The active bay's materialization scope.
///
/// Once a level is itself fully in scope (`ScopeClass::InScope`), everything below it is
/// included without further per-entry classification — the classifier's own "nothing below
/// needs re-classifying" contract; only a `ScopeClass::Spine` level needs the per-entry checks.
///
/// # Returns
/// * `Ok(())`      - If the shards were built.
/// * `Err(String)` - If a subtree object could not be loaded, or a spine path's type changed.
fn build_stale_shards(tree: &TreeItem,
                      key: &str,
                      tree_hash: &str,
                      shards: &mut BTreeMap<String, Inventory>,
                      scope: &MaterializationScope) -> Result<(), String> {
    // Hoisted once per directory (not per entry): a full (unscoped) scope, or a level already
    // fully in scope, never needs the per-entry classify calls below — short-circuit them away
    // entirely on that hot, common path.
    let fully_in_scope = scope.is_full() || scope.classify(key) == ScopeClass::InScope;

    let mut inventory = Inventory::new();

    for (name, item) in tree.get_files() {
        if !fully_in_scope {
            let child_key = join_key(key, name);

            // A file where the scope expects a directory (a spine ancestor, or the in-scope
            // prefix itself) is the §3.1 type change — refused rather than guessed.
            // Frontier: reframe the typed refusal for this still-String walker (bridge shim).
            if scope.spine_file_is_sealed(&child_key)? {
                continue;
            }
        }

        inventory.add_item(inventory_utils::build_stale_inventory_item(
            name,
            item.hash.clone(),
            item.item_type
        ));
    }

    // `tree_hash` is a byte-for-byte-trustworthy rollup for this shard only when the reset here
    // is a complete materialization of `tree` — see `tree_utils::rollup_stampable`'s doc comment
    // (shared with `shift`'s own tree-to-shard builder, which must decide this exactly the same
    // way).
    if tree_utils::rollup_stampable(scope, key, tree) {
        inventory.set_rollup_hash(Some(tree_hash.to_string()));
    }

    shards.insert(key.to_string(), inventory);

    for (name, subtree) in tree.get_subtrees() {
        let child_key = join_key(key, name);

        // Out-of-scope subtrees are sealed by hash and were never materialized — restoring
        // "to head" must not smuggle them into the scoped bay's staging area.
        if !fully_in_scope && scope.classify(&child_key) == ScopeClass::OutOfScope {
            continue;
        }

        let subtree_loaded = object_utils::load_tree(&subtree.hash)?;
        build_stale_shards(&subtree_loaded, &child_key, &subtree.hash, shards, scope)?;
    }

    Ok(())
}

/// Join a directory key and an entry name into the entry's warehouse path.
fn join_key(key: &str, name: &str) -> String {
    if key.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", key, name)
    }
}

/// Resolve a warehouse path inside the head tree.
///
/// # Arguments
/// * `root_tree_hash` - The hash of the head parcel's root tree.
/// * `key`            - The warehouse path key to resolve (`""` resolves to the root tree).
///
/// # Returns
/// * `Ok(Some(HeadEntry))` - The resolved entry (a file or a loaded directory).
/// * `Ok(None)`            - If the path does not exist in the head tree.
/// * `Err(String)`         - If a tree object could not be loaded.
fn resolve_head_entry(root_tree_hash: &str, key: &str) -> Result<Option<HeadEntry>, String> {
    let mut current = object_utils::load_tree(root_tree_hash)?;
    let mut current_hash = root_tree_hash.to_string();

    if key.is_empty() {
        return Ok(Some(HeadEntry::Tree { tree: current, hash: current_hash }));
    }

    let components: Vec<&str> = key.split(file_utils::PATH_SEPARATOR_CHAR).collect();

    for (index, component) in components.iter().enumerate() {
        let is_last = index == components.len() - 1;

        if is_last {
            if let Some((_, item)) = current.get_files().find(|(name, _)| name == component) {
                return Ok(Some(HeadEntry::File {
                    hash: item.hash.clone(),
                    item_type: item.item_type,
                }));
            }
        }

        let subtree = current.get_subtrees()
            .find(|(name, _)| name == component)
            .map(|(_, item)| item.hash.clone());

        match subtree {
            Some(subtree_hash) => {
                current = object_utils::load_tree(&subtree_hash)?;
                current_hash = subtree_hash;
            }
            None => return Ok(None),
        }
    }

    Ok(Some(HeadEntry::Tree { tree: current, hash: current_hash }))
}
