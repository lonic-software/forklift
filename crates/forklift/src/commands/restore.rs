use std::collections::BTreeMap;
use forklift_core::enums::dir_entry_type::DirEntryType;
use forklift_core::enums::inventory_item_state::InventoryItemState;
use forklift_core::model::inventory::Inventory;
use forklift_core::model::tree_item::TreeItem;
use forklift_core::util::path_utils::WarehousePath;
use forklift_core::util::scope_utils::{self, MaterializationScope, ScopeClass};
use forklift_core::util::{file_utils, inventory_utils, object_utils, pallet_utils, shift_utils, tree_utils};
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
/// * `restore <path>`          - Rewrite the file (or every tracked file of the directory)
///                               in the working directory from the inventory, discarding
///                               unstaged changes.
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
        return handle_unstage(target, "restore");
    }

    let path = WarehousePath::from_user_input(target)?;

    // An out-of-scope path is sealed by hash in a scoped bay and was never materialized;
    // restoring it would have nothing to restore from. Refuse cleanly rather than let the
    // walk below silently do the wrong thing.
    crate::commands::scope::ensure_path_in_scope(path.as_key())?;

    restore_worktree(&path)
}

/// Unstage a file or directory: reset its inventory entries to the pallet head, leaving
/// the working directory untouched. Shared by `restore --staged` and `unload`; `command`
/// labels the output envelope with the verb the user actually ran.
///
/// # Arguments
/// * `target`  - The path of the file or directory to unstage.
/// * `command` - The invoked command's name, for the output envelope.
///
/// # Returns
/// * `Ok(())`      - If the unstage completed successfully.
/// * `Err(String)` - If there was an error while handling the command.
pub fn handle_unstage(target: &str, command: &str) -> Result<(), String> {
    let path = WarehousePath::from_user_input(target)?;

    // An out-of-scope path is sealed by hash in a scoped bay and was never materialized;
    // unstaging it would smuggle out-of-scope content into the inventory. Refuse cleanly
    // rather than let the walk below silently do the wrong thing.
    crate::commands::scope::ensure_path_in_scope(path.as_key())?;

    restore_staged(&path, command)
}

/// Restore the working directory from the inventory: rewrite the file (or, for a
/// directory, every tracked, non-`Deleted` file below it) from its staged blob.
///
/// # Arguments
/// * `path` - The path to restore.
///
/// # Returns
/// * `Ok(())`      - If the restore completed.
/// * `Err(String)` - If the path is not in the inventory or a write failed.
fn restore_worktree(path: &WarehousePath) -> Result<(), String> {
    let has_shard = file_utils::get_inventory_data_path_for_key(path.as_key()).exists();

    if path.is_root() || has_shard {
        return restore_worktree_directory(path.as_key());
    }

    let (parent, file_name) = path.split_parent()?;

    let (_, shard_bytes) = file_utils::retrieve_inventory_or_none_by_key(parent.as_key())?;
    let inventory = match shard_bytes {
        Some(bytes) => forklift_core::parser::inventory::inventory_parser::parse_inventory(&bytes)?,
        None => return Err(format!("\"{}\" is not in the inventory.", path.as_key())),
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

    restore_file_and_refresh_entry(parent.as_key(), &item.name, &item.hash, item.item_type)?;

    output::message("restore", format!("Restored \"{}\" from the inventory.", path.as_key()));

    Ok(())
}

/// How many shards' decisions [`restore_worktree_directory`] accumulates in one
/// [`inventory_utils::ShardMutationBatch`] before publishing and starting a fresh one
/// (DESIGN.html §5.0 D item 10, PR B review finding #4). Bounds `restore .`'s peak memory to
/// roughly this many parsed `Inventory` shards regardless of how large the repository is — see
/// the function's own doc comment for why an unbounded batch gives up the sharded inventory's
/// whole RAM-scaling rationale for this one caller. Chosen generously (a real barrier is not
/// free, and typical `restore <dir>` calls touch far fewer shards than this): high enough that
/// almost every real invocation still pays exactly the same one-or-two-barrier cost the
/// unbounded design had, low enough that `restore .` on a repository with hundreds of thousands
/// of directories never holds more than a small, constant slice of them in memory at once.
const RESTORE_SHARD_GROUP_SIZE_DEFAULT: usize = 256;

/// The group size [`restore_worktree_directory`] actually flushes at — see
/// [`RESTORE_SHARD_GROUP_SIZE_DEFAULT`]. Overridable via `FORKLIFT_RESTORE_SHARD_GROUP_SIZE` so a
/// test can exercise the periodic-flush boundary with a handful of directories instead of needing
/// hundreds to cross the production threshold (same undocumented, test-only-override shape as
/// `inventory_utils::rollup_skip_enabled`'s `FORKLIFT_DISABLE_ROLLUP_SKIP`). An unset, empty, or
/// unparseable value falls back to the default; not a supported setting.
fn restore_shard_group_size() -> usize {
    std::env::var("FORKLIFT_RESTORE_SHARD_GROUP_SIZE")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|&size: &usize| size > 0)
        .unwrap_or(RESTORE_SHARD_GROUP_SIZE_DEFAULT)
}

/// Restore every tracked file of a directory (and its subdirectories) from the inventory.
///
/// Every file's working-directory write happens immediately (unfsynced, same as always), but its
/// refreshed inventory entry is only *decided* here — collected into one shared
/// [`inventory_utils::ShardMutationBatch`] instead of paying `update_shard`'s full two-barrier
/// *write* funnel per file (DESIGN.html §5.0 D item 10, finding #4). Several files in the same
/// shard collapse into one read-modify-*write* of it, published once per group of shards (see
/// [`restore_shard_group_size`]) — this loop still reads and parses each shard once itself (to
/// enumerate the files it needs to restore) and the batch's own first touch of that same key
/// reads and parses it again; harmless (nothing mutates the shard on disk between the two reads,
/// so both reads see identical bytes) but not collapsed, unlike the write side.
///
/// **Bounded memory, not one unbounded batch for the whole call** (DESIGN.html §5.0 D item 10, PR
/// B review finding #4): a `ShardMutationBatch` keeps every shard it has ever touched parsed in
/// memory until it is published, so a single batch spanning this whole function would hold every
/// directory's `Inventory` resident at once for `restore .` — the old per-file `update_shard`
/// funnel this replaced loaded one shard, wrote it, and dropped it, bounding peak memory to the
/// largest single directory *regardless of repository size*; giving that up for one caller's
/// convenience is exactly the tradeoff the sharded inventory design exists to avoid. `consolidate`/
/// `cherry-pick` and `park pop` stay a single batch (unaffected by this): their working set is
/// bounded by the merge/parked diff, never by the repository as a whole, so they keep their full
/// constant-barrier win. `restore <dir>` (worst case `restore .`) is the one caller whose working
/// set is unbounded, so — and *only* here — the batch is flushed (published, then replaced with a
/// fresh one) every [`restore_shard_group_size`] shards: barrier count becomes
/// `O(shards / group size)` instead of one for the whole call, still far below the
/// old per-file funnel's `O(files)`, while peak memory stays bounded by the group size instead of
/// the repository size.
///
/// If a file's restore fails partway through (an unreadable or missing blob), every file decided
/// before the failure — including every already-published group before this one — is still
/// published (or already durable). The same keep-whatever-was-decided resilience
/// `refresh_tracked_entries` gives a per-shard failure (see its own doc comment): under the old
/// per-file immediate-write loop this replaced, every prior file was already durably applied by
/// the time a later one failed, so this preserves that same guarantee under batching.
///
/// A `batch.publish()` failure (as opposed to a per-file decision failure above) is a wider case
/// than the old code had: every file's working-directory write already happened, unconditionally,
/// before its group's batch is published, so a publish failure unrelated to any specific file (a
/// corrupt ancestor shard, a mid-barrier I/O error) can leave more files' inventory entries stale
/// than the old per-file immediate funnel ever could (at most one, there) — though bounded to at
/// most one group's worth, not the whole call, by the flushing above. Not data loss — every
/// file's on-disk content already matches the pallet head (that is what a restore materializes),
/// so the only thing a subsequent read could get wrong is stat-cache staleness, self-healing on
/// the next `load`/`restore`/`stocktake` — but the caller's error message below says so
/// explicitly rather than leaving the operator to guess.
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

    let prefix = if key.is_empty() { String::new() } else { format!("{}/", key) };
    let mut restored_any = false;
    let mut restored_count = 0usize;
    let mut batch = inventory_utils::ShardMutationBatch::new();
    let mut shards_in_batch = 0usize;

    // `result` accumulates the *first* failure — see the function's own doc comment for why this,
    // rather than propagating immediately, matters here.
    let mut result: Result<(), String> = Ok(());

    'shards: for entry in &metadata {
        let shard_key = inventory_utils::metadata_entry_to_key(entry);

        let is_in_subtree = key.is_empty()
            || shard_key == key
            || shard_key.starts_with(&prefix);

        if !is_in_subtree {
            continue;
        }

        restored_any = true;

        let (_, shard_bytes) = match file_utils::retrieve_inventory_or_none_by_key(shard_key) {
            Ok(bytes_opt) => bytes_opt,
            Err(e) => { result = Err(e); break 'shards; }
        };

        let Some(bytes) = shard_bytes else {
            continue;
        };

        let inventory = match forklift_core::parser::inventory::inventory_parser::parse_inventory(&bytes) {
            Ok(inventory) => inventory,
            Err(e) => {
                result = Err(format!("Error while parsing the inventory of folder \"{}\": {}", shard_key, e));
                break 'shards;
            }
        };

        let files: Vec<(String, String, DirEntryType)> = inventory.get_items()
            .filter(|(_, item)| item.state != InventoryItemState::Deleted)
            .map(|(name, item)| (name.to_string(), item.hash.clone(), item.item_type))
            .collect();

        if files.is_empty() {
            continue;
        }

        match restore_shard_files_into(&mut batch, shard_key, &files) {
            Ok(count) => restored_count += count,
            Err(e) => { result = Err(e); break 'shards; }
        }

        shards_in_batch += 1;

        if shards_in_batch >= restore_shard_group_size() {
            // `std::mem::take` swaps in a fresh, empty (`Default`) batch and hands this function
            // ownership of the full one to publish — `batch` itself is therefore *never* left in
            // a moved-out state, on either the success or the failure path below, which is what
            // lets the trailing `publish()` after the loop always be valid to call, even after a
            // failure here (it would just publish the fresh, empty replacement — a documented
            // no-op, see `ShardMutationBatch::publish`'s own doc comment).
            let full_batch = std::mem::take(&mut batch);

            if let Err(e) = full_batch.publish() {
                result = Err(e);
                break 'shards;
            }

            shards_in_batch = 0;
        }
    }

    // Every file decided above (in the batch's current, not-yet-flushed group) becomes durable
    // now, through the shared join point. Attempted even after a mid-loop failure, exactly like
    // `refresh_tracked_entries`'s identical resilience contract, for the same reason — a no-op if
    // the loop's own periodic flush already published everything and left only a fresh, empty
    // batch behind.
    if let Err(e) = batch.publish() {
        if result.is_ok() { result = Err(e); }
    }

    if let Err(e) = result {
        return Err(format!(
            "{}\nThe restore did not complete: some files may already have been rewritten from \
            the inventory without their stat data being refreshed. Re-run \"restore {}\" (or \
            \"load .\") once the problem is fixed to reconcile.",
            e,
            if key.is_empty() { "." } else { key }
        ));
    }

    if !restored_any {
        return Err(format!(
            "No inventory found for folder \"{}\".",
            if key.is_empty() { "./" } else { key }
        ));
    }

    output::message("restore", format!("Restored {} file(s) from the inventory.", restored_count));

    Ok(())
}

/// Restore every one of a single shard's (non-deleted) files from its staged blob, and refresh
/// all of their inventory entries in one `batch.update` call for the whole shard — collapsing
/// every file in the directory into one read-modify-write, as before, but *also* fixing the stat
/// cache regression batching introduced (DESIGN.html §5.0 D item 10, PR B review finding #5):
/// every file in `files` is written to disk *first* (pass 1, this function's own loop), and only
/// *then* is `batch.update` called — so the whole batch's first-touch anchor for this shard
/// (`ShardMutationBatch`'s `first_touched_at`, captured immediately before the closure below
/// runs) is captured strictly *after* every one of this shard's real writes, and every one of
/// this shard's files is stat'd (pass 2, inside the closure) strictly *after* that anchor was
/// captured.
///
/// This is what makes the anchor both sound and useful for every file in the shard, not just the
/// first: sound, because `ShardOutcome`'s own invariant — the published mtime must be no later
/// than any verification (stat) actually performed for the shard — holds for every file here,
/// since every stat in pass 2 runs strictly after the anchor; useful, because the anchor is also
/// strictly *after* every file's real on-disk write, satisfying `is_entry_unchanged`'s `mtime <
/// shard_mtime` stat-cache guard for every file, not only whichever file happened to be the
/// batch's first touch of this key. Calling `batch.update` once per *file* instead (staging each
/// file's already-computed stat outside the closure, as the single-file, unbatched helper this
/// replaced did) would anchor the shard to the *first* file's touch — every later file's own real
/// write would then postdate the anchor, permanently defeating the stat cache for it until the
/// next `load` rewrites the shard from scratch. See `is_entry_unchanged`'s and `ShardOutcome`'s
/// own doc comments for the full racily-clean reasoning this preserves.
///
/// # Arguments
/// * `batch`     - The batch to stage this shard's mutation into.
/// * `shard_key` - The warehouse path key of the directory these files belong to.
/// * `files`     - Every file to restore in this shard: name, target blob/recipe hash, and type.
///
/// # Returns
/// * `Ok(usize)`   - The number of files restored (for the caller's summary count).
/// * `Err(String)` - If a file's content could not be written, or its fresh stat could not be
///                   gathered.
fn restore_shard_files_into(batch: &mut inventory_utils::ShardMutationBatch,
                            shard_key: &str,
                            files: &[(String, String, DirEntryType)]) -> Result<usize, String> {
    // Pass 1: every file's content lands on disk first (unfsynced, immediate, as always) — see
    // the function's own doc comment for why this must fully precede pass 2 below.
    for (name, hash, item_type) in files {
        materialize_restored_file(shard_key, name, hash, *item_type)?;
    }

    let file_count = files.len();
    let owned_files = files.to_vec();

    // Pass 2, inside the batch's closure — so it runs after this shard's first-touch anchor is
    // captured (see the function's own doc comment). `move` so the closure owns its copy of
    // `files` (`ShardMutationBatch::update` takes `FnOnce`, called synchronously within this call,
    // so no lifetime beyond it is needed).
    batch.update(shard_key, move |inventory| {
        for (name, hash, item_type) in &owned_files {
            let file_path = if shard_key.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", shard_key, name)
            };

            let refreshed = inventory_utils::build_inventory_item_from_stat(
                std::path::Path::new(&file_path), name, hash.clone(), *item_type,
            )?;

            inventory.add_item(refreshed);
        }

        Ok(())
    })?;

    Ok(file_count)
}

/// Write one tracked file from its blob and refresh its inventory entry with the new
/// stat data (so the warehouse reports clean afterwards without rehashing).
///
/// # Arguments
/// * `parent_key` - The warehouse path key of the file's directory.
/// * `name`       - The name of the file.
/// * `hash`       - The blob hash of the staged content.
/// * `item_type`  - The type of the entry.
///
/// # Returns
/// * `Ok(())`      - If the file was restored.
/// * `Err(String)` - If a write failed.
fn restore_file_and_refresh_entry(parent_key: &str,
                                  name: &str,
                                  hash: &str,
                                  item_type: DirEntryType) -> Result<(), String> {
    let file_path = materialize_restored_file(parent_key, name, hash, item_type)?;

    let refreshed = inventory_utils::build_inventory_item_from_stat(
        std::path::Path::new(&file_path),
        name,
        hash.to_string(),
        item_type,
    )?;

    inventory_utils::update_shard(parent_key, |inventory| {
        inventory.add_item(refreshed);
        Ok(())
    })
}

/// Write one tracked file's content from its blob into the working directory (unfsynced, same as
/// every other working-directory write) and return its warehouse path.
fn materialize_restored_file(parent_key: &str,
                             name: &str,
                             hash: &str,
                             item_type: DirEntryType) -> Result<String, String> {
    let file_path = if parent_key.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", parent_key, name)
    };

    shift_utils::write_tracked_file(&file_path, hash, item_type)?;

    Ok(file_path)
}

/// Reset the inventory entries of the given path to the pallet head (unstage). The
/// working directory is not touched; the reset entries carry zeroed stat data, so the
/// next comparison against the working directory rehashes them.
///
/// # Arguments
/// * `path`    - The path to unstage.
/// * `command` - The invoked command's name, for the output envelope.
///
/// # Returns
/// * `Ok(())`      - If the unstage completed.
/// * `Err(String)` - If the path exists neither in the inventory nor in the head.
fn restore_staged(path: &WarehousePath, command: &str) -> Result<(), String> {
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

    let has_shard = file_utils::get_inventory_data_path_for_key(path.as_key()).exists();
    let treat_as_directory = path.is_root()
        || has_shard
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

        output::message(command, format!(
            "Unstaged \"{}\" (inventory reset to the pallet head).",
            if path.is_root() { "./" } else { path.as_key() }
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

    output::message(command, format!("Unstaged \"{}\".", path.as_key()));

    Ok(())
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

            match scope.classify(&child_key) {
                ScopeClass::OutOfScope => continue,
                // A file where the scope expects a directory (a spine ancestor, or the
                // in-scope prefix itself) is the §3.1 type change — refuse rather than guess.
                // Frontier: reframe the typed refusal for this still-String walker (bridge shim).
                ScopeClass::InScope | ScopeClass::Spine =>
                    return Err(scope_utils::type_changed_refusal(&child_key).into()),
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
