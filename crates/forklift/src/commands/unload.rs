use std::collections::BTreeMap;
use forklift_core::enums::dir_entry_type::DirEntryType;
use forklift_core::enums::inventory_item_state::InventoryItemState;
use forklift_core::model::inventory::{Inventory, InventoryItem};
use forklift_core::model::tree_item::TreeItem;
use forklift_core::util::path_utils::{self, WarehousePath};
use forklift_core::util::scope_utils::{self, PathScope, ScopeClass};
use forklift_core::util::{
    file_utils, inventory_utils, load_guard_utils, object_utils, pallet_utils, tree_utils,
};
use crate::commands::restore::{matching_content, tracked_path, Restored};
use crate::output;

/// A resolved entry of the pallet head's tree.
enum HeadEntry {
    /// A file (normal, executable or symlink) with its blob hash.
    File { hash: String, item_type: DirEntryType },

    /// A directory, loaded, together with its own tree hash (the parent's entry for it —
    /// `tree` as loaded carries no hash of its own; the root resolution uses the root tree
    /// hash directly). Needed to stamp the rebuilt shard's rollup in `build_unstaged_shards`.
    Tree { tree: TreeItem, hash: String },
}

/// Handle the unload command: reset the inventory entries of a file or directory to the pallet
/// head, leaving the working directory untouched — the inverse of `load`. Staging a removal is
/// `remove`, deliberately a different verb, so undoing a `load` can never turn into a staged
/// deletion.
///
/// # Arguments
/// * `target` - The path of the file or directory to unstage.
///
/// # Returns
/// * `Ok(())`      - If the unstage completed successfully.
/// * `Err(String)` - If there was an error while handling the command.
pub fn handle_command(target: &str) -> Result<(), String> {
    let path = WarehousePath::from_user_input(target)?;

    // An out-of-scope path is sealed by hash in a scoped bay and was never materialized;
    // unstaging it would smuggle out-of-scope content into the inventory. Refuse cleanly
    // rather than let the walk below silently do the wrong thing.
    crate::commands::scope::ensure_path_in_scope(path.as_key())?;

    unstage(&path)
}



/// Reset the inventory entries of the given path to the pallet head (unstage). The
/// working directory is not touched; a reset entry records the working file's stat data only
/// where the file provably holds the head's content (see `unstaged_entry`), and zeroed stat data
/// otherwise, so the next comparison against the working directory rehashes it.
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
fn unstage(path: &WarehousePath) -> Result<(), String> {
    // The tree this bay holds: its base, not the pallet's ref, which another bay can move.
    let position = pallet_utils::read_position()?;
    let head = position.base.clone();

    let head_tree_hash = match &head {
        Some(hash) => Some(object_utils::load_parcel(hash)?.tree_hash),
        None => None,
    };

    let head_entry = match &head_tree_hash {
        Some(tree_hash) => resolve_head_entry(tree_hash, path.as_key())?,
        None => None,
    };

    // Read once: nothing below writes the metadata before its one write — `update_shard` or
    // `replace_subtree` through this ledger, or the batch's publish, which reads its own.
    let mut ledger = inventory_utils::ShardLedger::read()?;

    let treat_as_directory = path.is_root()
        || ledger.holds(path.as_key())
        || matches!(head_entry, Some(HeadEntry::Tree { .. }));

    if treat_as_directory {
        // Rebuild the whole subtree of the staging area from the head: directories that
        // exist only in the inventory disappear, directories that exist only in the head
        // come back (with zeroed stat data where nothing on disk matches them).
        let mut shards: BTreeMap<String, Inventory> = BTreeMap::new();

        if let Some(HeadEntry::Tree { tree, hash }) = &head_entry {
            // In a scoped bay, only in-scope directories were ever materialized — the walk
            // must not resurrect out-of-scope shards for content that was never actually
            // written to this bay's working directory.
            let scope = scope_utils::current_scope()?;

            build_unstaged_shards(&ledger, tree, path.as_key(), hash, &mut shards, &scope)?;
        }

        ledger.replace_subtree(path.as_key(), &shards)?;

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
            // As in `build_unstaged_shards`, an unreadable shard only costs the shortcut.
            let current = ledger.inventory(parent.as_key()).unwrap_or(None);
            let entry = unstaged_entry(parent.as_key(), &file_name, hash, item_type, current.as_ref())?;

            // An entry that already matched the head changes only its stat data, which keeps the
            // rollups (see `ShardMutationBatch`).
            let mut batch = inventory_utils::ShardMutationBatch::over(&ledger);
            batch.update(parent.as_key(), |inventory| inventory.add_item(entry))?;
            batch.publish()?;
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
            // the very same function `unload`'s ordinary directory-reset branch above
            // already relies on is deliberate: it is the one place this invariant is enforced,
            // not a rule this call site has to uphold on its own. `nothing_staged_under` is worth
            // paying even when no marker turns out to be involved (`healed` then comes back empty
            // and this falls through to the unchanged ordinary path below) — it is a metadata
            // scan plus, at most, a few small shard reads, not proportional to warehouse size.
            if nothing_staged_under(&ledger, path)? {
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

            ledger.update_shard(parent.as_key(), |inventory| {
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
/// # Arguments
/// * `ledger` - The registered set.
/// * `path`   - The path to check.
///
/// # Returns
/// * `Ok(true)`    - Nothing is staged under `path`, in either sense.
/// * `Ok(false)`   - A parent-shard file entry, or at least one entry in some covered shard,
///                   exists.
/// * `Err(String)` - A shard could not be read or parsed.
fn nothing_staged_under(ledger: &inventory_utils::ShardLedger, path: &WarehousePath) -> Result<bool, String> {
    if !path.is_root() {
        let (parent, file_name) = path.split_parent()?;

        if let Some(inventory) = ledger.inventory(parent.as_key())? {
            if inventory.get_item_by_name(&file_name).is_some() {
                return Ok(false);
            }
        }
    }

    for shard_key in ledger.keys() {
        if !path_utils::covers(path.as_key(), shard_key) {
            continue;
        }

        let Some(inventory) = ledger.inventory(shard_key)? else { continue };

        if inventory.get_items_count() > 0 {
            return Ok(false);
        }
    }

    Ok(true)
}

/// Build inventory shards for a head subtree, each entry as `unstaged_entry` decides it.
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
/// * `scope`     - The active bay's checkout scope.
///
/// Once a level is itself fully in scope (`ScopeClass::InScope`), everything below it is
/// included without further per-entry classification — the classifier's own "nothing below
/// needs re-classifying" contract; only a `ScopeClass::Spine` level needs the per-entry checks.
///
/// # Returns
/// * `Ok(())`      - If the shards were built.
/// * `Err(String)` - If a subtree object could not be loaded, or a spine path's type changed.
fn build_unstaged_shards(ledger: &inventory_utils::ShardLedger,
                         tree: &TreeItem,
                         key: &str,
                         tree_hash: &str,
                         shards: &mut BTreeMap<String, Inventory>,
                         scope: &PathScope) -> Result<(), String> {
    // Hoisted once per directory (not per entry): a full (unscoped) scope, or a level already
    // fully in scope, never needs the per-entry classify calls below — short-circuit them away
    // entirely on that hot, common path.
    let fully_in_scope = scope.is_full() || scope.classify(key) == ScopeClass::InScope;

    let mut inventory = Inventory::new();
    // The current shard only saves work, so one that cannot be read is no reason to refuse the
    // reset that rebuilds it.
    let current = ledger.inventory(key).unwrap_or(None);

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

        inventory.add_item(unstaged_entry(key, name, item.hash.clone(), item.item_type, current.as_ref())?);
    }

    // `tree_hash` is a byte-for-byte-trustworthy rollup for this shard only when the reset here
    // is a complete materialization of `tree` — see `tree_utils::rollup_stamp`'s doc comment
    // (shared with `shift`'s own tree-to-shard builder, which must decide this exactly the same
    // way).
    if let Some(stamp) = tree_utils::rollup_stamp(scope, key, tree, tree_hash) {
        inventory.stamp_rollup(stamp);
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
        build_unstaged_shards(ledger, &subtree_loaded, &child_key, &subtree.hash, shards, scope)?;
    }

    Ok(())
}

/// The entry `unload` resets the file `name` in the directory `parent_key` to: the
/// head's content, with the stat data of the working file when it provably holds that content,
/// and zeroed stat data (see `build_stale_inventory_item`) otherwise, so the next comparison
/// rehashes it. A file the current entry already proves by stat keeps that entry; any other is
/// compared by content, never written.
///
/// # Arguments
/// * `parent_key` - The warehouse path key of the file's directory.
/// * `name`       - The name of the file.
/// * `hash`       - The head's blob hash for the file.
/// * `item_type`  - The head's type for the file.
/// * `current`    - The directory's shard before the reset, if it has one.
///
/// # Returns
/// * `Ok(InventoryItem)` - The entry to record.
/// * `Err(String)`       - If the clock probe could not be stamped.
fn unstaged_entry(parent_key: &str,
                  name: &str,
                  hash: String,
                  item_type: DirEntryType,
                  current: Option<&Inventory>) -> Result<InventoryItem, String> {
    let stale = inventory_utils::build_stale_inventory_item(name, hash, item_type);

    let file_path = tracked_path(parent_key, name);
    let current_entry = current
        .and_then(|inventory| inventory.get_item_by_name(name))
        .filter(|item| item.state == InventoryItemState::Normal);

    // Before the stats below, so a restatted entry proves itself against it.
    let anchor = file_utils::take_stat_anchor()?;

    // The current entry is a cheaper witness than the content: for the head's own content and
    // type it can prove a match, and for any other content it can prove a difference.
    let witness = match current_entry {
        Some(item) if item.hash == stale.hash && item.item_type == stale.item_type => item.as_ref().clone(),
        Some(item) if item.hash != stale.hash && proven_by_stat(&file_path, &item) => return Ok(stale),
        _ => stale.clone(),
    };

    Ok(match matching_content(&file_path, &witness, anchor) {
        Some(Restored::Restatted(refreshed)) => refreshed,
        Some(Restored::Untouched) => witness,
        _ => stale,
    })
}

/// Whether the stat cache proves the file at `file_path` still holds `item`'s content.
fn proven_by_stat(file_path: &str, item: &InventoryItem) -> bool {
    let path = std::path::Path::new(file_path);

    let Ok(metadata) = file_utils::get_symlink_metadata_for_path(path) else {
        return false;
    };

    let Ok(item_type) = file_utils::get_type_of_dir_entry(&metadata) else {
        return false;
    };

    !metadata.is_dir() && inventory_utils::is_entry_unchanged(item, &metadata, item_type, path)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self, Warehouse};

    /// How many times `run` reads the ledger on this thread.
    fn ledger_reads(run: impl FnOnce()) -> u64 {
        let before = inventory_utils::ledger_read_count();
        run();
        inventory_utils::ledger_read_count() - before
    }

    #[test]
    fn an_unload_reads_the_ledger_once_whatever_the_path_names() {
        let warehouse = Warehouse::with_working_tree("unload-ledger-reads");
        forklift_core::util::pallet_utils::write_position(&forklift_core::util::pallet_utils::BayPosition::new("main", None)).unwrap();
        std::fs::create_dir_all(warehouse.root.join("dir/sub")).unwrap();
        std::fs::write(warehouse.root.join("dir/a.txt"), b"a\n").unwrap();
        std::fs::write(warehouse.root.join("dir/sub/b.txt"), b"b\n").unwrap();
        test_support::block_on(crate::commands::load::handle_command(".")).unwrap();

        // One read, which the unstage's own metadata write goes through.
        for target in ["dir/sub/b.txt", "dir"] {
            assert_eq!(ledger_reads(|| handle_command(target).unwrap()), 1, "unload {}", target);
        }

    }
}
