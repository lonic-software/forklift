use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Display;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use file_id::FileId;
use regex::Regex;
use crate::enums::inventory_item_state::InventoryItemState;
use crate::model::inventory::Inventory;
use crate::model::task::change_walk::change_walk_context::ChangeWalkContext;
use crate::model::task::TaskExecutor;
use crate::model::tree_item::TreeItem;
use crate::parser;
use crate::traits::task_context::TaskContext;
use crate::types::task::Task;
use crate::util::scope_utils::{self, MaterializationScope, ScopeClass};
use crate::util::{file_utils, inventory_utils, object_utils};

/// The kind of a change reported by a stocktake.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    /// The item exists in the newer state but not in the older one.
    Added,

    /// The item exists in both states with different content.
    Modified,

    /// The item was moved: it disappeared from one path and reappeared at another with
    /// the same content (detected by a move-detection post-pass; the formats stay move-agnostic).
    Moved,

    /// The item exists in the older state but not in the newer one.
    Removed,

    /// The item exists in the working directory but is not tracked by the inventory.
    Untracked,

    /// The item is in a conflict state (an unresolved consolidation).
    Conflict,
}

impl Display for ChangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let kind_str = match self {
            ChangeKind::Added     => "added",
            ChangeKind::Modified  => "modified",
            ChangeKind::Moved     => "moved",
            ChangeKind::Removed   => "removed",
            ChangeKind::Untracked => "untracked",
            ChangeKind::Conflict  => "conflict",
        };

        write!(f, "{}", kind_str)
    }
}

/// A single change reported by a stocktake.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(serde::Serialize)]
pub struct Change {
    pub kind: ChangeKind,

    /// The warehouse path of the changed item (`/`-separated, relative to the root).
    /// Untracked directories are reported with a trailing `/` and are not descended into.
    /// For `Moved` items this is the new path.
    pub path: String,

    /// The old path of a `Moved` item; `None` for every other kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved_from: Option<String>,
}

impl Change {
    /// Create a (non-move) change.
    fn new(kind: ChangeKind, path: String) -> Change {
        Change { kind, path, moved_from: None }
    }
}

/// Collect the staged changes: the difference between the inventory and the head tree of
/// the current pallet — what the next `stack` would record. An unborn head reports every
/// inventoried file as added. The per-directory comparisons run in parallel over the
/// `TaskExecutor` (one task per directory).
///
/// # Arguments
/// * `head_tree_hash` - The hash of the head parcel's tree, or `None` for an unborn pallet.
///
/// # Returns
/// * `Ok(Vec<Change>)` - The staged changes, sorted by path.
/// * `Err(String)`     - If a shard or tree object could not be read.
pub async fn collect_staged_changes(head_tree_hash: Option<&str>) -> Result<Vec<Change>, String> {
    collect_staged_changes_impl(head_tree_hash, false).await.map(|(changes, _)| changes)
}

/// Like [`collect_staged_changes`], but also returns the per-directory inventories the walk
/// parsed along the way, keyed by warehouse path key.
///
/// `diff --staged` uses this to render each changed file's staged content without re-reading and
/// re-parsing a shard the walk already read moments earlier — see `diff.rs`'s
/// `inventory_content`. The map is invocation-scoped: it belongs to this one walk, never a
/// process-wide cache, because a shard is a mutable file (unlike the content-addressed object
/// caches in `file_utils`/`object_utils`, which are safe to share across calls forever).
///
/// # Arguments
/// * `head_tree_hash` - The hash of the head parcel's tree, or `None` for an unborn pallet.
///
/// # Returns
/// * `Ok((Vec<Change>, HashMap<String, Arc<Inventory>>))` - The staged changes (sorted by path)
///   and the shards the walk parsed.
/// * `Err(String)` - If a shard or tree object could not be read.
pub async fn collect_staged_changes_with_shards(head_tree_hash: Option<&str>)
    -> Result<(Vec<Change>, HashMap<String, Arc<Inventory>>), String> {
    collect_staged_changes_impl(head_tree_hash, true).await
}

/// The shared body of [`collect_staged_changes`] and [`collect_staged_changes_with_shards`].
/// `collect_shards` gates whether the walk pays for populating the shard memo — see
/// [`ChangeWalkContext::collect_shards`].
async fn collect_staged_changes_impl(head_tree_hash: Option<&str>, collect_shards: bool)
    -> Result<(Vec<Change>, HashMap<String, Arc<Inventory>>), String> {
    // The rollup-based skip (DESIGN.html §5.0 D item 8, stage 2): the root shard's rollup
    // already names the whole staged tree, so if it matches the head root tree hash exactly, no
    // staged change is possible anywhere — return without reading the metadata file, loading a
    // single head tree object, or spawning a single walk task.
    if let Some(head_hash) = head_tree_hash {
        if rollup_skip_matches("", head_hash)? {
            inventory_utils::record_rollup_skip();
            return Ok((Vec::new(), HashMap::new()));
        }
    }

    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;
    let metadata = metadata_opt.unwrap_or_default();

    let keys: BTreeSet<String> = metadata.iter()
        .map(|entry| inventory_utils::metadata_entry_to_key(entry).to_string())
        .collect();
    let children = Arc::new(directory_children(&keys));

    // Only ever borrowed by the walk below (name/hash lookups over its files and subtrees, one
    // level at a time) — the shared form avoids cloning every entry of a root tree that can be
    // large (the whole warehouse's top level).
    let head_tree = match head_tree_hash {
        Some(hash) => Some(object_utils::load_tree_shared(hash)?),
        None => None,
    };

    // In a scoped bay the head carries out-of-scope subtrees the dock never materialized: those
    // must be classified as sealed (skipped), not force-loaded and reported as `Removed`. Full
    // scope (a plain bay or the main tree) classifies everything in scope, so the walk is
    // unchanged there.
    let scope = Arc::new(scope_utils::current_scope()?);

    let context = Arc::new(ChangeWalkContext::new(collect_shards));
    let executor = TaskExecutor::new(Arc::clone(&context));

    let root_task: Task<(), String> = Box::pin(walk_directory_staged(
        Arc::clone(&context),
        String::new(),
        head_tree,
        Arc::clone(&children),
        Arc::clone(&scope),
    ));

    executor.execute(root_task).await.map_err(|e|
        e.unwrap_or("An unknown error occurred while collecting the staged changes.".to_string())
    )?;

    let mut changes = std::mem::take(&mut *context.changes.lock().await);

    detect_staged_moves(&mut changes, head_tree_hash)?;

    changes.sort_by(|a, b| a.path.cmp(&b.path));

    let shards = std::mem::take(&mut *context.shards.lock().await);

    Ok((changes, shards))
}

/// Whether the subtree at `key` can be skipped entirely during the staged-changes walk: its
/// rollup exactly matches `head_subtree_hash`, so — by Stage 1's maintenance guarantee that a
/// rollup is cleared the moment anything at or below it actually changes — no staged change is
/// possible anywhere in it. Gated by the kill switch (`FORKLIFT_DISABLE_ROLLUP_SKIP`) so the
/// equivalence tests can force a full walk and diff it against the optimized one.
///
/// # Arguments
/// * `key`               - The warehouse path key of the directory.
/// * `head_subtree_hash` - The head tree's subtree hash at that same key.
fn rollup_skip_matches(key: &str, head_subtree_hash: &str) -> Result<bool, String> {
    if !inventory_utils::rollup_skip_enabled() {
        return Ok(false);
    }

    Ok(inventory_utils::peek_shard_rollup(key)?.as_deref() == Some(head_subtree_hash))
}

/// The per-directory task of the staged walk: compare one directory's shard against the
/// corresponding head tree and enqueue one task per child directory — the union of the
/// inventoried subdirectories and the head subtrees (a head-only subtree sees an empty
/// shard, so everything below it is reported as removed).
///
/// # Arguments
/// * `context`   - The shared walk context.
/// * `key`       - The warehouse path key of the directory.
/// * `head_tree` - The head tree of this directory, if it exists in the head parcel.
/// * `children`  - The parent → children relation over the inventoried directory keys.
///
/// # Returns
/// * `Ok(())`      - If the directory was compared.
/// * `Err(String)` - If a shard or tree object could not be read.
fn walk_directory_staged(context: Arc<ChangeWalkContext>,
                         key: String,
                         head_tree: Option<Arc<TreeItem>>,
                         children: Arc<BTreeMap<String, Vec<String>>>,
                         scope: Arc<MaterializationScope>)
                         -> impl Future<Output = Result<(), String>> + Send {
    async move {
        // Hoisted once per directory (not per entry): a full (unscoped) scope always
        // classifies everything in scope, so on the hot, common full-bay path the classify
        // calls below (and their `join_key` allocations) are pure overhead — short-circuit
        // them away entirely, the same way `stack_parcel` gates the whole overlay on `is_full()`.
        let scope_is_full = scope.is_full();

        let inventory = Arc::new(load_shard_or_empty(&key)?);

        let mut found: Vec<Change> = Vec::new();

        let head_files: BTreeMap<&String, &TreeItem> = head_tree.as_ref()
            .map(|tree| tree.get_files().collect())
            .unwrap_or_default();

        for (name, item) in inventory.get_items() {
            // Conflict states outrank the regular comparison: the entry needs resolving no
            // matter what the head says.
            let is_conflict = matches!(
                item.state,
                InventoryItemState::FirstParentConflict
                    | InventoryItemState::SecondParentConflict
                    | InventoryItemState::ThirdParentConflict
            );

            if is_conflict {
                found.push(Change::new(ChangeKind::Conflict, join_key(&key, name)));
                continue;
            }

            let head_entry = head_files.get(name);
            let is_staged_removal = item.state == InventoryItemState::Deleted;

            let change = match (head_entry, is_staged_removal) {
                (Some(_), true)  => Some(ChangeKind::Removed),
                (Some(head), false) if head.hash != item.hash => Some(ChangeKind::Modified),
                (Some(_), false) => None,
                (None, true)  => None,
                (None, false) => Some(ChangeKind::Added),
            };

            if let Some(kind) = change {
                found.push(Change::new(kind, join_key(&key, name)));
            }
        }

        // Files in the head that have no inventory entry at all count as staged removals
        // (they will not be part of the next parcel) — but only for in-scope files. An
        // out-of-scope head file at a spine level was never materialized: it is sealed by
        // hash, not removed, so it must not be reported as `Removed`.
        for (name, _) in head_files.iter() {
            if inventory.get_item_by_name(name).is_none()
                && (scope_is_full || scope.classify(&join_key(&key, name)) == ScopeClass::InScope)
            {
                found.push(Change::new(ChangeKind::Removed, join_key(&key, name)));
            }
        }

        // Fan out over the union of inventoried subdirectories and head subtrees.
        let head_subtrees: BTreeMap<&String, &TreeItem> = head_tree.as_ref()
            .map(|tree| tree.get_subtrees().collect())
            .unwrap_or_default();

        let empty = Vec::new();
        let child_keys = children.get(&key).unwrap_or(&empty);

        for child_key in child_keys {
            let child_name = last_component(child_key);
            let head_subtree = head_subtrees.get(&child_name.to_string());

            // The rollup-based skip (DESIGN.html §5.0 D item 8, stage 2): a matching rollup
            // means no staged change is possible anywhere at or below this child — skip the
            // head tree load and the entire recursion (that subtree's shard is never even
            // read). A missing rollup, missing shard, or missing head subtree all fall through
            // to the ordinary walk, unchanged.
            if let Some(subtree) = head_subtree {
                if rollup_skip_matches(child_key, &subtree.hash)? {
                    inventory_utils::record_rollup_skip();
                    continue;
                }
            }

            let child_head_tree = match head_subtree {
                Some(subtree) => Some(object_utils::load_tree_shared(&subtree.hash)?),
                None => None,
            };

            context.send_task(Box::pin(walk_directory_staged(
                Arc::clone(&context),
                child_key.clone(),
                child_head_tree,
                Arc::clone(&children),
                Arc::clone(&scope),
            )))?;
        }

        let child_names: BTreeSet<&str> = child_keys.iter().map(|k| last_component(k)).collect();

        for (name, subtree) in head_subtrees.iter() {
            if !child_names.contains(name.as_str()) {
                // A head subtree with no inventory child is a whole-subtree staged removal —
                // but only when it is in scope. An out-of-scope head subtree at a spine level
                // is sealed by hash and was never materialized: skip it entirely (do not load
                // the object, do not report its files as `Removed`).
                if !scope_is_full && scope.classify(&join_key(&key, name)) != ScopeClass::InScope {
                    continue;
                }

                let subtree_tree = object_utils::load_tree_shared(&subtree.hash)?;

                context.send_task(Box::pin(walk_directory_staged(
                    Arc::clone(&context),
                    join_key(&key, name),
                    Some(subtree_tree),
                    Arc::clone(&children),
                    Arc::clone(&scope),
                )))?;
            }
        }

        // Shared with the caller so a content-render pass over this walk's changes (`diff
        // --staged`) can reuse this parse instead of re-reading and re-parsing the same shard.
        // Gated twice over: `collect_shards` (a caller that only wants the change list never
        // reads this map, so it must not pay for the extra lock) and, per directory, whether
        // this directory actually produced a change (the render loop only ever looks up a
        // *changed* file's parent shard — retaining every walked directory's inventory here
        // would be O(whole inventory), defeating the point of sharding it by directory in the
        // first place; O(changed directories) is what the memo is for).
        if context.collect_shards && !found.is_empty() {
            context.shards.lock().await.insert(key.clone(), inventory);
        }

        context.changes.lock().await.extend(found);

        Ok(())
    }
}

/// Collect the unstaged changes: the difference between the working directory and the
/// inventory — what a `load` would stage. Untracked directories are reported once (with a
/// trailing `/`) and not descended into. The per-directory reconciliations (including the
/// hashing of stat-cache misses) run in parallel over the `TaskExecutor` (one task per
/// directory).
///
/// # Returns
/// * `Ok(Vec<Change>)` - The unstaged changes, sorted by path.
/// * `Err(String)`     - If the working directory or a shard could not be read.
pub async fn collect_unstaged_changes() -> Result<Vec<Change>, String> {
    collect_unstaged_changes_impl(false).await.map(|(changes, _)| changes)
}

/// Like [`collect_unstaged_changes`], but also returns the per-directory inventories the walk
/// parsed along the way, keyed by warehouse path key — see
/// [`collect_staged_changes_with_shards`] for why this exists and why it is invocation-scoped.
///
/// # Returns
/// * `Ok((Vec<Change>, HashMap<String, Arc<Inventory>>))` - The unstaged changes (sorted by
///   path) and the shards the walk parsed.
/// * `Err(String)` - If the working directory or a shard could not be read.
pub async fn collect_unstaged_changes_with_shards()
    -> Result<(Vec<Change>, HashMap<String, Arc<Inventory>>), String> {
    collect_unstaged_changes_impl(true).await
}

/// The shared body of [`collect_unstaged_changes`] and [`collect_unstaged_changes_with_shards`].
/// `collect_shards` gates whether the walk pays for populating the shard memo — see
/// [`ChangeWalkContext::collect_shards`].
async fn collect_unstaged_changes_impl(collect_shards: bool)
    -> Result<(Vec<Change>, HashMap<String, Arc<Inventory>>), String> {
    let ignored_paths = Arc::new(file_utils::get_ignored_paths()?);

    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;
    let metadata = metadata_opt.unwrap_or_default();

    let keys: Arc<BTreeSet<String>> = Arc::new(metadata.iter()
        .map(|entry| inventory_utils::metadata_entry_to_key(entry).to_string())
        .collect());

    let context = Arc::new(ChangeWalkContext::new(collect_shards));
    let executor = TaskExecutor::new(Arc::clone(&context));

    let root_task: Task<(), String> = Box::pin(walk_directory_unstaged(
        Arc::clone(&context),
        String::new(),
        Arc::clone(&keys),
        Arc::clone(&ignored_paths),
    ));

    executor.execute(root_task).await.map_err(|e|
        e.unwrap_or("An unknown error occurred while collecting the unstaged changes.".to_string())
    )?;

    let mut changes = std::mem::take(&mut *context.changes.lock().await);

    detect_unstaged_moves(&mut changes)?;

    changes.sort_by(|a, b| a.path.cmp(&b.path));

    let shards = std::mem::take(&mut *context.shards.lock().await);

    Ok((changes, shards))
}

/// The per-directory task of the unstaged walk: reconcile one tracked directory's
/// working-directory state against its shard and enqueue one task per tracked
/// subdirectory found on disk.
///
/// # Arguments
/// * `context`       - The shared walk context.
/// * `key`           - The warehouse path key of the directory.
/// * `keys`          - All inventoried directory keys.
/// * `ignored_paths` - The ignore patterns.
///
/// # Returns
/// * `Ok(())`      - If the directory was compared.
/// * `Err(String)` - If the directory or a shard could not be read.
fn walk_directory_unstaged(context: Arc<ChangeWalkContext>,
                           key: String,
                           keys: Arc<BTreeSet<String>>,
                           ignored_paths: Arc<Vec<Regex>>)
                           -> impl Future<Output = Result<(), String>> + Send {
    async move {
        let fs_path = if key.is_empty() {
            Path::new(".").to_path_buf()
        } else {
            std::path::PathBuf::from(&key)
        };

        // The shard's own modification time is needed for the stat-cache comparison
        // (see `inventory_utils::is_entry_unchanged`).
        let (shard_path, shard_bytes) = file_utils::retrieve_inventory_or_none_by_key(&key)?;
        let shard_mtime = file_utils::get_symlink_metadata_for_path(&shard_path).ok()
            .and_then(|m| file_utils::get_content_modification_timestamp_for_file(&m).ok())
            .unwrap_or(0);

        let inventory = match shard_bytes {
            Some(bytes) => parser::inventory::inventory_parser::parse_inventory(&bytes)
                .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?,
            None => Inventory::new(),
        };
        let inventory = Arc::new(inventory);

        let mut found: Vec<Change> = Vec::new();
        let mut seen_files: BTreeSet<String> = BTreeSet::new();
        let mut seen_dirs: BTreeSet<String> = BTreeSet::new();
        // Names ignored at this level. An ignored entry is present on disk — we just do not
        // descend into it or diff it — so the reconciliation below must not mistake it for a
        // removal. For a tracked directory that was ignored *after* it was tracked (e.g. a
        // build dir added to `.forkliftignore`), reporting it "removed" would fan out into a
        // single-threaded walk of its whole subtree — the cause of a multi-minute stocktake
        // hang on a store that had accumulated many such tracked-then-ignored files.
        let mut ignored_names: BTreeSet<String> = BTreeSet::new();

        for entry_result in file_utils::read_directory(&fs_path)? {
            let entry = entry_result.map_err(|e| format!("Error while reading directory entry: {}", e))?;
            let name = file_utils::get_name_for_file_or_directory(&entry)?;
            let entry_key = join_key(&key, &name);

            if file_utils::is_path_ignored(&entry_key, &ignored_paths) {
                ignored_names.insert(name);
                continue;
            }

            let metadata = file_utils::get_symlink_metadata_for_path(&entry.path())?;
            let item_type = file_utils::get_type_of_dir_entry(&metadata);

            if item_type.is_file() {
                seen_files.insert(name.clone());

                match inventory.get_item_by_name(&name) {
                    None => found.push(Change::new(ChangeKind::Untracked, entry_key)),
                    Some(item) if item.state == InventoryItemState::Deleted => {
                        // The removal is staged but the file is still on disk: from the
                        // inventory's point of view the file is untracked again.
                        found.push(Change::new(ChangeKind::Untracked, entry_key));
                    }
                    Some(item) => {
                        // The shared classifier `load` also uses: stat cache first, hash
                        // on a miss. `ComputeOnly` keeps this read-only — nothing is written
                        // to the object store, not even a re-chunked giant's chunks (the blob
                        // object of a modified small file is likewise dropped here).
                        let verdict = inventory_utils::classify_file_against_entry(
                            &item, &metadata, item_type, &entry.path(), &name, shard_mtime,
                            object_utils::IngestMode::ComputeOnly,
                        )?;

                        if let inventory_utils::FileVerdict::Modified(..) = verdict {
                            found.push(Change::new(ChangeKind::Modified, entry_key));
                        }
                    }
                }
            } else {
                seen_dirs.insert(name.clone());

                if keys.contains(&entry_key) {
                    context.send_task(Box::pin(walk_directory_unstaged(
                        Arc::clone(&context),
                        entry_key,
                        Arc::clone(&keys),
                        Arc::clone(&ignored_paths),
                    )))?;
                } else {
                    found.push(Change::new(ChangeKind::Untracked, format!("{}/", entry_key)));
                }
            }
        }

        // Inventory entries whose file is gone from disk are unstaged removals (unless the
        // removal is already staged).
        for (name, item) in inventory.get_items() {
            if !seen_files.contains(name) && !ignored_names.contains(name)
                && item.state != InventoryItemState::Deleted {
                found.push(Change::new(ChangeKind::Removed, join_key(&key, name)));
            }
        }

        // Inventoried subdirectories that are gone from disk: their whole subtree is removed.
        let subtree_prefix = if key.is_empty() { String::new() } else { format!("{}/", key) };

        for tracked_key in keys.range(subtree_prefix.clone()..) {
            if !tracked_key.starts_with(&subtree_prefix) && !key.is_empty() {
                break;
            }

            if *tracked_key == key || tracked_key.is_empty() {
                continue;
            }

            let relative = &tracked_key[subtree_prefix.len()..];

            // Only direct children of this directory are handled here; deeper descendants
            // are handled by the child tasks (or by their gone parent below).
            if relative.contains(file_utils::PATH_SEPARATOR_CHAR) {
                continue;
            }

            if !seen_dirs.contains(relative) && !ignored_names.contains(relative) {
                report_missing_directory_unstaged(tracked_key, &keys, &mut found)?;
            }
        }

        // Shared with the caller so a content-render pass over this walk's changes (`diff`) can
        // reuse this parse instead of re-reading and re-parsing the same shard. Gated — see the
        // matching comment in `walk_directory_staged`.
        if context.collect_shards && !found.is_empty() {
            context.shards.lock().await.insert(key.clone(), inventory);
        }

        context.changes.lock().await.extend(found);

        Ok(())
    }
}

/// Report every non-`Deleted` entry of a tracked directory that is gone from the working
/// directory (recursively) as removed.
///
/// # Arguments
/// * `key`     - The warehouse path key of the missing directory.
/// * `keys`    - All inventoried directory keys.
/// * `changes` - The collected changes.
///
/// # Returns
/// * `Ok(())`      - If the directory was reported.
/// * `Err(String)` - If a shard could not be read.
fn report_missing_directory_unstaged(key: &str,
                                     keys: &BTreeSet<String>,
                                     changes: &mut Vec<Change>) -> Result<(), String> {
    let inventory = load_shard_or_empty(key)?;

    for (name, item) in inventory.get_items() {
        if item.state != InventoryItemState::Deleted {
            changes.push(Change::new(ChangeKind::Removed, join_key(key, name)));
        }
    }

    let prefix = format!("{}/", key);

    for child_key in keys.range(prefix.clone()..) {
        if !child_key.starts_with(&prefix) {
            break;
        }

        let relative = &child_key[prefix.len()..];

        if !relative.contains(file_utils::PATH_SEPARATOR_CHAR) {
            report_missing_directory_unstaged(child_key, keys, changes)?;
        }
    }

    Ok(())
}

/// The §3.2.1 move-detection post-pass over the staged changes: a file that disappeared
/// from one path and reappeared at another with the same blob hash is a move. Only
/// unambiguous 1:1 pairs are converted — the walk itself (and the formats) stay
/// move-agnostic.
///
/// # Arguments
/// * `changes`        - The collected staged changes.
/// * `head_tree_hash` - The hash of the head parcel's tree, or `None` for an unborn pallet.
///
/// # Returns
/// * `Ok(())`      - If the pass completed.
/// * `Err(String)` - If a shard or tree object could not be read.
fn detect_staged_moves(changes: &mut Vec<Change>,
                       head_tree_hash: Option<&str>) -> Result<(), String> {
    // Without a head nothing can be removed, so nothing can have moved.
    let Some(head_tree_hash) = head_tree_hash else {
        return Ok(());
    };

    let mut removed_by_hash: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut added_by_hash: BTreeMap<String, Vec<usize>> = BTreeMap::new();

    for (index, change) in changes.iter().enumerate() {
        match change.kind {
            ChangeKind::Removed => {
                if let Some((hash, _)) =
                    object_utils::resolve_tree_file(head_tree_hash, &change.path)? {
                    removed_by_hash.entry(hash).or_default().push(index);
                }
            }
            ChangeKind::Added => {
                let (parent_key, name) = split_key(&change.path);
                let inventory = load_shard_or_empty(parent_key)?;

                if let Some(item) = inventory.get_item_by_name(name) {
                    added_by_hash.entry(item.hash.clone()).or_default().push(index);
                }
            }
            _ => {}
        }
    }

    let mut consumed: Vec<bool> = vec![false; changes.len()];

    for (hash, removed_indices) in &removed_by_hash {
        let Some(added_indices) = added_by_hash.get(hash) else {
            continue;
        };

        if removed_indices.len() != 1 || added_indices.len() != 1 {
            continue;
        }

        changes[added_indices[0]].kind = ChangeKind::Moved;
        changes[added_indices[0]].moved_from = Some(changes[removed_indices[0]].path.clone());
        consumed[removed_indices[0]] = true;
    }

    remove_consumed(changes, &consumed);

    Ok(())
}

/// The §3.2.1 move-detection post-pass over the unstaged changes: an untracked file whose
/// inode matches a removed inventory entry is a move candidate; the content hash confirms
/// it (inodes get reused, so the inode alone never decides). Only unambiguous 1:1 pairs
/// are converted.
///
/// # Arguments
/// * `changes` - The collected unstaged changes.
///
/// # Returns
/// * `Ok(())`      - If the pass completed.
/// * `Err(String)` - If a shard could not be read.
fn detect_unstaged_moves(changes: &mut Vec<Change>) -> Result<(), String> {
    let mut removed_by_id: BTreeMap<(u64, u64), Vec<usize>> = BTreeMap::new();
    let mut untracked_by_id: BTreeMap<(u64, u64), Vec<usize>> = BTreeMap::new();
    let mut removed_hashes: HashMap<usize, String> = HashMap::new();

    for (index, change) in changes.iter().enumerate() {
        match change.kind {
            ChangeKind::Removed => {
                let (parent_key, name) = split_key(&change.path);
                let inventory = load_shard_or_empty(parent_key)?;

                if let Some(item) = inventory.get_item_by_name(name) {
                    // Entries with zeroed stat data (fresh from a shift or an unstage)
                    // have no meaningful inode to match on.
                    if item.inode != 0 {
                        removed_by_id.entry((item.device, item.inode)).or_default().push(index);
                        removed_hashes.insert(index, item.hash.clone());
                    }
                }
            }
            // Untracked directories (trailing "/") are not files and cannot pair.
            ChangeKind::Untracked if !change.path.ends_with(file_utils::PATH_SEPARATOR_CHAR) => {
                if let Some(id) = file_id_for_path(&change.path) {
                    untracked_by_id.entry(id).or_default().push(index);
                }
            }
            _ => {}
        }
    }

    let mut consumed: Vec<bool> = vec![false; changes.len()];

    for (id, removed_indices) in &removed_by_id {
        let Some(untracked_indices) = untracked_by_id.get(id) else {
            continue;
        };

        if removed_indices.len() != 1 || untracked_indices.len() != 1 {
            continue;
        }

        let removed_index = removed_indices[0];
        let untracked_index = untracked_indices[0];

        // Confirm the candidate by content: a reused inode with different content is a
        // coincidence, not a move.
        let is_same_content = hash_worktree_file(&changes[untracked_index].path).ok()
            .map(|hash| hash == removed_hashes[&removed_index])
            .unwrap_or(false);

        if !is_same_content {
            continue;
        }

        changes[untracked_index].kind = ChangeKind::Moved;
        changes[untracked_index].moved_from = Some(changes[removed_index].path.clone());
        consumed[removed_index] = true;
    }

    remove_consumed(changes, &consumed);

    Ok(())
}

/// Drop the changes marked as consumed by a move pairing.
fn remove_consumed(changes: &mut Vec<Change>, consumed: &[bool]) {
    let mut index = 0;

    changes.retain(|_| {
        let keep = !consumed[index];
        index += 1;
        keep
    });
}

/// Get the (device, inode) pair of a file in the working directory, if it can be read.
fn file_id_for_path(path: &str) -> Option<(u64, u64)> {
    match file_utils::get_file_id_for_file(Path::new(path)).ok()? {
        FileId::Inode { device_id, inode_number } => Some((device_id, inode_number)),
        FileId::LowRes { volume_serial_number, file_index } =>
            Some((volume_serial_number as u64, file_index)),
        FileId::HighRes { .. } => None,
    }
}

/// Hash a file in the working directory (without storing anything), exactly as `load`
/// would.
///
/// # Arguments
/// * `path` - The warehouse path of the file.
///
/// # Returns
/// * `Ok(String)`  - The blob hash of the file's content.
/// * `Err(String)` - If the file could not be read.
fn hash_worktree_file(path: &str) -> Result<String, String> {
    let fs_path = Path::new(path);
    let metadata = file_utils::get_symlink_metadata_for_path(fs_path)?;
    let item_type = file_utils::get_type_of_dir_entry(&metadata);
    let name = last_component(path);

    // Compute the identity `load` would record — the recipe hash for a giant, the blob hash for a
    // small file — without storing anything (`ComputeOnly`). This keeps move detection comparing
    // like with like: the inventory holds a recipe hash for a chunked file, so the untracked
    // candidate must be hashed the same way (and never read whole into memory).
    Ok(object_utils::ingest_file(name, fs_path, item_type, object_utils::IngestMode::ComputeOnly)?.hash)
}

/// Split a warehouse path into its parent directory key and its entry name.
fn split_key(path: &str) -> (&str, &str) {
    path.rsplit_once(file_utils::PATH_SEPARATOR_CHAR).unwrap_or(("", path))
}

/// Load and parse the inventory shard for the given key, or return an empty inventory if
/// the shard does not exist.
///
/// # Arguments
/// * `key` - The warehouse path key of the directory.
///
/// # Returns
/// * `Ok(Inventory)` - The parsed (or empty) inventory.
/// * `Err(String)`   - If the shard exists but could not be read or parsed.
pub fn load_shard_or_empty(key: &str) -> Result<Inventory, String> {
    let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

    match bytes_opt {
        Some(bytes) => parser::inventory::inventory_parser::parse_inventory(&bytes)
            .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e)),
        None => Ok(Inventory::new()),
    }
}

/// Derive the parent → children relation over the given inventoried directory keys,
/// synthesizing ancestors that have no shard of their own. The root key (`""`) is always
/// present as a parent.
///
/// # Arguments
/// * `keys` - The inventoried directory keys.
///
/// # Returns
/// * `BTreeMap<String, Vec<String>>` - Parent key → child keys (children sorted).
pub fn directory_children(keys: &BTreeSet<String>) -> BTreeMap<String, Vec<String>> {
    let mut all_keys: BTreeSet<String> = BTreeSet::new();

    for key in keys {
        let mut current = key.as_str();

        loop {
            all_keys.insert(current.to_string());

            match current.rsplit_once(file_utils::PATH_SEPARATOR_CHAR) {
                Some((parent, _)) => current = parent,
                None => break,
            }
        }

        all_keys.insert(String::new());
    }

    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for key in &all_keys {
        if key.is_empty() {
            continue;
        }

        let parent = key.rsplit_once(file_utils::PATH_SEPARATOR_CHAR)
            .map(|(parent, _)| parent)
            .unwrap_or("");

        children.entry(parent.to_string()).or_default().push(key.clone());
    }

    children
}

/// Join a directory key and an entry name into the entry's warehouse path.
fn join_key(key: &str, name: &str) -> String {
    if key.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", key, name)
    }
}

/// Get the last path component of a warehouse path key.
fn last_component(key: &str) -> &str {
    key.rsplit_once(file_utils::PATH_SEPARATOR_CHAR)
        .map(|(_, name)| name)
        .unwrap_or(key)
}
