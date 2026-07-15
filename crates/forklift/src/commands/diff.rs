use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use serde::Serialize;
use forklift_core::enums::diff_type::DiffType;
use forklift_core::enums::dir_entry_type::DirEntryType;
use forklift_core::model::diff::Diff;
use forklift_core::model::inventory::Inventory;
use forklift_core::model::tree_item::TreeItem;
use forklift_core::util::path_utils::WarehousePath;
use forklift_core::util::scope_utils::{self, MaterializationScope, ScopeClass};
use forklift_core::util::stocktake_utils::ChangeKind;
use forklift_core::util::{
    chunk_utils, diff, fanout_utils, file_utils, merge_utils, object_utils, pallet_utils,
    stocktake_utils,
};
use crate::output::{self, CommandOutput};

/// One side of a file diff: text bytes to line-diff, or an opaque binary (a chunked large file,
/// or a worktree file too big to read whole). A binary side is never assembled or read into
/// memory — it is reported as "binary contents" without a line-by-line diff.
enum DiffSide {
    Text(Vec<u8>),
    Binary,
}

impl DiffSide {
    /// An empty text side (a file absent on this side of the change).
    fn empty() -> DiffSide {
        DiffSide::Text(Vec::new())
    }

    /// Whether this side must be reported as binary (a chunked file, or non-text bytes).
    fn is_binary(&self) -> bool {
        match self {
            DiffSide::Binary => true,
            DiffSide::Text(bytes) => !merge_utils::is_mergeable_text(bytes),
        }
    }

    /// The text bytes of this side (empty for a binary side, which is never line-diffed).
    fn bytes(&self) -> &[u8] {
        match self {
            DiffSide::Text(bytes) => bytes,
            DiffSide::Binary => &[],
        }
    }
}

/// Handle the diff command: show the changed files line by line.
/// * `diff [path]`          - Working directory vs inventory (what a `load` would stage).
/// * `diff --staged [path]` - Inventory vs pallet head (what the next `stack` records).
/// * `diff <revision-a> <revision-b> [path]` - The tree of one revision vs the tree of
///   another (a pallet name, a parcel hash, or the reserved [`EMPTY_TREE_TOKEN`] for "nothing").
///
/// The optional path limits the report to a file or directory. With the global
/// `--verbose` flag, unchanged lines are printed too.
///
/// # Arguments
/// * `staged`  - Whether to compare the inventory against the pallet head instead of the
///               working directory against the inventory.
/// * `targets` - The positional arguments: zero or one path, or two revisions plus an
///               optional path.
/// * `verbose` - Whether to print unchanged lines too.
///
/// # Returns
/// * `Ok(())`      - If the diff completed successfully.
/// * `Err(String)` - If there was an error while handling the command.
pub async fn handle_command(staged: bool, targets: &[String], verbose: bool) -> Result<(), String> {
    match targets {
        [] | [_] => {
            let filter = match targets.first() {
                Some(target) => Some(WarehousePath::from_user_input(target)?),
                None => None,
            };

            // An out-of-scope path filter would report nothing in a scoped bay (the subtree is
            // sealed and unmaterialized) — refuse it clearly instead (§7.6).
            if let Some(filter) = &filter {
                crate::commands::scope::ensure_path_in_scope(filter.as_key())?;
            }

            if staged {
                diff_staged(filter.as_ref(), verbose).await
            } else {
                diff_worktree(filter.as_ref(), verbose).await
            }
        }
        [from_revision, to_revision, rest @ ..] if rest.len() <= 1 => {
            if staged {
                return Err(
                    "--staged cannot be combined with a revision comparison: the staged \
                    changes are always relative to the current pallet's head.".to_string()
                );
            }

            let filter = match rest.first() {
                Some(target) => Some(WarehousePath::from_user_input(target)?),
                None => None,
            };

            if let Some(filter) = &filter {
                crate::commands::scope::ensure_path_in_scope(filter.as_key())?;
            }

            diff_pallets(from_revision, to_revision, filter.as_ref(), verbose)
        }
        _ => Err(
            "Too many arguments. Usage: \"diff [path]\", \"diff --staged [path]\" or \
            \"diff <revision-a> <revision-b> [path]\".".to_string()
        ),
    }
}

/// Diff the working directory against the inventory. Untracked files are not diffed
/// (they have no inventory side); `stocktake` reports them.
///
/// # Arguments
/// * `filter`  - An optional path that limits the report.
/// * `verbose` - Whether to print unchanged lines too.
///
/// # Returns
/// * `Ok(())`      - If the diff completed.
/// * `Err(String)` - If a shard, blob or working file could not be read.
async fn diff_worktree(filter: Option<&WarehousePath>, verbose: bool) -> Result<(), String> {
    // The line-by-line diff is a human display; a program gets the changed-file set
    // (path + kind) and reads content by hash when it needs it (§7.4 keeps agent
    // output token-cheap by default). The `--json` report never reads a shard's content, so
    // collecting the walk's shard memo for it would be pure waste — skip it and take the plain
    // (unmemoized) walk instead.
    if output::is_json() {
        let changes = stocktake_utils::collect_unstaged_changes().await?;

        let files = changes.iter()
            .filter(|change| change.kind != ChangeKind::Untracked && is_within(&change.path, filter))
            .map(DiffFileSummary::from_change)
            .collect();

        output::emit("diff", &DiffReport { mode: "worktree", files });

        return Ok(());
    }

    let (changes, shards) = stocktake_utils::collect_unstaged_changes_with_shards().await?;

    let mut printed_any = false;

    for change in &changes {
        if !is_within(&change.path, filter) {
            continue;
        }

        let (old_content, new_content) = match (change.kind, &change.moved_from) {
            (ChangeKind::Untracked, _) => continue,
            (ChangeKind::Removed, _) => (inventory_content(&change.path, &shards)?, DiffSide::empty()),
            (ChangeKind::Moved, Some(from)) =>
                (inventory_content(from, &shards)?, worktree_content(&change.path)?),
            _ => (inventory_content(&change.path, &shards)?, worktree_content(&change.path)?),
        };

        print_file_diff(change.kind, &change_label(change), &old_content, &new_content,
                        &mut printed_any, verbose);
    }

    if !printed_any {
        println!("The working directory matches the inventory.");
    }

    Ok(())
}

/// Diff the inventory against the pallet head. An unborn head reports every inventoried
/// file as added; conflict entries are listed but not diffed (the working file carries
/// the conflict markers).
///
/// # Arguments
/// * `filter`  - An optional path that limits the report.
/// * `verbose` - Whether to print unchanged lines too.
///
/// # Returns
/// * `Ok(())`      - If the diff completed.
/// * `Err(String)` - If a shard, blob or tree object could not be read.
async fn diff_staged(filter: Option<&WarehousePath>, verbose: bool) -> Result<(), String> {
    let pallet = pallet_utils::get_current_pallet_name()?;
    let head = pallet_utils::get_pallet_head(&pallet)?;

    let head_tree_hash = match &head {
        Some(hash) => Some(object_utils::load_parcel(hash)?.tree_hash),
        None => None,
    };

    // As in `diff_worktree`: the `--json` report only lists the changed-file set and never
    // reads a shard's content, so skip collecting the walk's shard memo for it.
    if output::is_json() {
        let changes = stocktake_utils::collect_staged_changes(head_tree_hash.as_deref()).await?;

        let files = changes.iter()
            .filter(|change| is_within(&change.path, filter))
            .map(DiffFileSummary::from_change)
            .collect();

        output::emit("diff", &DiffReport { mode: "staged", files });

        return Ok(());
    }

    let (changes, shards) =
        stocktake_utils::collect_staged_changes_with_shards(head_tree_hash.as_deref()).await?;

    let mut printed_any = false;

    for change in &changes {
        if !is_within(&change.path, filter) {
            continue;
        }

        if change.kind == ChangeKind::Conflict {
            if printed_any {
                println!();
            }

            printed_any = true;
            println!("\x1b[1mconflict: {}\x1b[0m", change.path);
            println!("  (unresolved — resolve the file and \"load\" it)");
            continue;
        }

        let old_content = match (change.kind, &change.moved_from) {
            (ChangeKind::Added, _) => DiffSide::empty(),
            (ChangeKind::Moved, Some(from)) => head_content(head_tree_hash.as_deref(), from)?,
            _ => head_content(head_tree_hash.as_deref(), &change.path)?,
        };

        let new_content = match change.kind {
            ChangeKind::Removed => DiffSide::empty(),
            _ => inventory_content(&change.path, &shards)?,
        };

        print_file_diff(change.kind, &change_label(change), &old_content, &new_content,
                        &mut printed_any, verbose);
    }

    if !printed_any {
        println!("The inventory matches the pallet head; nothing is staged.");
    }

    Ok(())
}

/// Diff the trees of two revisions: `from` is the old side, `to` the new side (the
/// report reads "what changed going from `from` to `to`"). Identical subtree hashes are
/// skipped entirely. Either side may be [`EMPTY_TREE_TOKEN`] instead of a real revision,
/// meaning "nothing" — the tree with no entries — so a root parcel (or any revision) can be
/// diffed against a clean slate instead of refusing for want of a second side.
///
/// # Arguments
/// * `from_revision` - The old-side revision: a pallet name, a parcel hash (prefix), or
///                     [`EMPTY_TREE_TOKEN`].
/// * `to_revision`   - The new-side revision, same grammar.
/// * `filter`        - An optional path that limits the report.
/// * `verbose`       - Whether to print unchanged lines too.
///
/// # Returns
/// * `Ok(())`      - If the diff completed.
/// * `Err(String)` - If a revision does not resolve (unknown pallet, unborn head, or
///                   unresolvable hash), or an object could not be read.
fn diff_pallets(from_revision: &str,
                to_revision: &str,
                filter: Option<&WarehousePath>,
                verbose: bool) -> Result<(), String> {
    let from_tree_hash = pallet_head_tree(from_revision)?;
    let to_tree_hash = pallet_head_tree(to_revision)?;

    if from_tree_hash == to_tree_hash {
        if output::is_json() {
            output::emit("diff", &DiffReport { mode: "pallets", files: Vec::new() });
        } else {
            println!("Revisions \"{}\" and \"{}\" have identical trees.", from_revision, to_revision);
        }

        return Ok(());
    }

    // Only ever borrowed by `collect_tree_changes` below — the shared form avoids cloning every
    // entry of what can be a large root tree (the whole warehouse's top level).
    let from_tree = object_utils::load_tree_shared(&from_tree_hash)?;
    let to_tree = object_utils::load_tree_shared(&to_tree_hash)?;

    // In a scoped bay the walk is pruned to the in-scope subtree(s): out-of-scope subtrees are
    // sealed by hash, so a scoped diff reports only what the bay actually works on (and never
    // loads an out-of-scope object). Full scope walks the whole tree, unchanged.
    let scope = scope_utils::current_scope()?;

    let mut changes: Vec<TreeChange> = Vec::new();
    collect_tree_changes(Some(from_tree.as_ref()), Some(to_tree.as_ref()), "", &mut changes, &scope)?;

    detect_tree_moves(&mut changes);

    changes.sort_by(|a, b| a.path.cmp(&b.path));

    if output::is_json() {
        let files = changes.iter()
            .filter(|change| is_within(&change.path, filter))
            .map(|change| DiffFileSummary {
                kind: change.kind,
                path: change.path.clone(),
                moved_from: change.moved_from.clone(),
            })
            .collect();

        output::emit("diff", &DiffReport { mode: "pallets", files });

        return Ok(());
    }

    let selected: Vec<&TreeChange> = changes.iter()
        .filter(|change| is_within(&change.path, filter))
        .collect();

    if selected.is_empty() {
        println!("The tracked files of \"{}\" and \"{}\" match.", from_revision, to_revision);
        return Ok(());
    }

    // Each file's diff is independent (its own two blobs, its own histogram diff), and the
    // diff is real CPU, so the files fan out across the cores. The output must stay in path
    // order, so the blocks are computed into strings and printed afterwards rather than
    // inline.
    let blocks = format_changed_files(&selected, verbose)?;

    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            println!();
        }
        print!("{}", block);
    }

    Ok(())
}

/// Format each changed file's diff block, fanning the work across the cores once there are
/// enough files to be worth it. Returns the blocks positionally aligned with `changes`, so
/// the caller prints them in the (path-sorted) order they arrived.
///
/// Loading each file's two blobs shares the object caches, so — like `audit` — the speed-up
/// is real but sub-linear; the histogram diff itself is pure CPU and parallelizes freely.
///
/// # Arguments
/// * `changes` - The changed files to diff (already path-sorted and filtered).
/// * `verbose` - Whether to include unchanged lines too.
///
/// # Returns
/// * `Ok(Vec<String>)` - One formatted diff block per change, in order.
/// * `Err(String)`     - The first (lowest-index) file whose blob could not be read.
fn format_changed_files(changes: &[&TreeChange], verbose: bool) -> Result<Vec<String>, String> {
    // Below this many files the threads cost more than the diffs they would share.
    const PARALLEL_THRESHOLD: usize = 8;

    if changes.len() < PARALLEL_THRESHOLD {
        return changes.iter().map(|change| diff_block(change, verbose)).collect();
    }

    // See `forklift_core::util::fanout_utils::fanout_map` for the fan-out idiom (chunking,
    // worker count, and the storage-scope re-entry every worker needs). It never
    // short-circuits, so the first-path error a serial `.collect()` would report is
    // recovered by collecting the (order-preserved) results the same way here.
    fanout_utils::fanout_map(changes, |change| diff_block(change, verbose))
        .into_iter()
        .collect()
}

/// Load one changed file's two sides and format its diff block (the body of the parallel
/// phase). It reads blobs through the shared, already-thread-safe object caches and formats
/// with no shared state, so it is safe to run on many threads at once.
fn diff_block(change: &TreeChange, verbose: bool) -> Result<String, String> {
    // A chunked file is reported as binary without loading either side (its hash is a recipe, and
    // assembling multi-GB content to line-diff it would defeat the point). Known from the tree
    // entry types recorded when the change was collected — no object load to discover it.
    let (old_side, new_side) = if change.is_binary {
        (DiffSide::Binary, DiffSide::Binary)
    } else {
        let old_side = match &change.old_hash {
            Some(hash) => DiffSide::Text(object_utils::load_blob(hash)?.content),
            None => DiffSide::empty(),
        };
        let new_side = match &change.new_hash {
            Some(hash) => DiffSide::Text(object_utils::load_blob(hash)?.content),
            None => DiffSide::empty(),
        };
        (old_side, new_side)
    };

    let label = match &change.moved_from {
        Some(from) => format!("{} -> {}", from, change.path),
        None => change.path.clone(),
    };

    Ok(format_file_diff(change.kind, &label, &old_side, &new_side, verbose))
}

/// The reserved token for "the empty tree" as either side of a two-revision `diff`: a root
/// parcel has no real "before", so this lets it be compared against a clean slate — every
/// file it introduces then lists as `Added` — instead of `diff` refusing for want of a second
/// revision. Chosen so it can never collide with a real revision: a pallet or meta-pallet name
/// is validated to ASCII letters/digits/`.`/`_`/`-` (`/`-separated; see
/// [`pallet_utils::validate_pallet_name`]) and a hash prefix is 4+ hex digits — neither
/// grammar can ever contain `:`, so this token is unambiguous at a glance and by construction.
pub const EMPTY_TREE_TOKEN: &str = ":empty";

/// Get the root tree hash of a revision: a pallet name (its head), a parcel hash (prefix),
/// or [`EMPTY_TREE_TOKEN`] for the empty tree.
///
/// # Arguments
/// * `revision` - The revision argument.
///
/// # Returns
/// * `Ok(String)`  - The revision's tree hash.
/// * `Err(String)` - If the revision could not be resolved or the parcel could not be
///                   read.
fn pallet_head_tree(revision: &str) -> Result<String, String> {
    if revision == EMPTY_TREE_TOKEN {
        return object_utils::empty_tree_hash();
    }

    let parcel_hash = pallet_utils::resolve_revision(revision)?;

    Ok(object_utils::load_parcel(&parcel_hash)?.tree_hash)
}

/// One changed file of a tree-vs-tree comparison.
struct TreeChange {
    kind: ChangeKind,

    /// The warehouse path of the file (the new path, for moved files).
    path: String,

    /// The blob hash on the old side (absent for added files).
    old_hash: Option<String>,

    /// The blob hash on the new side (absent for removed files).
    new_hash: Option<String>,

    /// The old path of a moved file; `None` for every other kind.
    moved_from: Option<String>,

    /// Whether either side is a chunked (large binary) file: it is reported as "binary
    /// contents", never assembled or line-diffed (its hash is a recipe, not a blob).
    is_binary: bool,
}

/// The §3.2.1 move-detection post-pass over a tree-vs-tree comparison: a removed and an
/// added file with the same blob hash are one move. Only unambiguous 1:1 pairs are
/// converted.
fn detect_tree_moves(changes: &mut Vec<TreeChange>) {
    let mut removed_by_hash: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut added_by_hash: BTreeMap<String, Vec<usize>> = BTreeMap::new();

    for (index, change) in changes.iter().enumerate() {
        match change.kind {
            ChangeKind::Removed => {
                if let Some(hash) = &change.old_hash {
                    removed_by_hash.entry(hash.clone()).or_default().push(index);
                }
            }
            ChangeKind::Added => {
                if let Some(hash) = &change.new_hash {
                    added_by_hash.entry(hash.clone()).or_default().push(index);
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

        let removed_index = removed_indices[0];
        let added_index = added_indices[0];

        changes[added_index].kind = ChangeKind::Moved;
        changes[added_index].moved_from = Some(changes[removed_index].path.clone());
        changes[added_index].old_hash = changes[removed_index].old_hash.clone();
        consumed[removed_index] = true;
    }

    let mut index = 0;

    changes.retain(|_| {
        let keep = !consumed[index];
        index += 1;
        keep
    });
}

/// Compare one directory level of two trees (recursively), collecting the changed files
/// with their blob hashes on both sides. Subtrees with identical hashes are skipped.
///
/// # Arguments
/// * `from`    - This directory in the old tree (if it exists there).
/// * `to`      - This directory in the new tree (if it exists there).
/// * `key`     - The warehouse path key of the directory.
/// * `changes` - The collected changes.
///
/// # Returns
/// * `Ok(())`      - If the directory was compared.
/// * `Err(String)` - If a subtree object could not be loaded.
fn collect_tree_changes(from: Option<&TreeItem>,
                        to: Option<&TreeItem>,
                        key: &str,
                        changes: &mut Vec<TreeChange>,
                        scope: &MaterializationScope) -> Result<(), String> {
    // Hoisted once per directory level (not per entry): a full (unscoped) scope always
    // classifies everything in scope, so the classify (and its `join_key` allocation) is
    // pure overhead on the hot, common full-bay path — short-circuit it away entirely there,
    // the same way `stack_parcel` gates the whole overlay on `is_full()`.
    let scope_is_full = scope.is_full();

    let from_files: BTreeMap<&String, &TreeItem> = from
        .map(|tree| tree.get_files().collect())
        .unwrap_or_default();
    let to_files: BTreeMap<&String, &TreeItem> = to
        .map(|tree| tree.get_files().collect())
        .unwrap_or_default();

    for (name, to_item) in &to_files {
        // Out-of-scope files are sealed by hash in a scoped bay; never report them.
        if !scope_is_full && scope.classify(&join_key(key, name)) == ScopeClass::OutOfScope {
            continue;
        }

        match from_files.get(*name) {
            None => changes.push(TreeChange {
                kind: ChangeKind::Added,
                path: join_key(key, name),
                old_hash: None,
                new_hash: Some(to_item.hash.clone()),
                moved_from: None,
                is_binary: to_item.item_type.is_chunked(),
            }),
            Some(from_item)
                if from_item.hash != to_item.hash
                    || from_item.item_type != to_item.item_type => {
                changes.push(TreeChange {
                    kind: ChangeKind::Modified,
                    path: join_key(key, name),
                    old_hash: Some(from_item.hash.clone()),
                    new_hash: Some(to_item.hash.clone()),
                    moved_from: None,
                    is_binary: from_item.item_type.is_chunked() || to_item.item_type.is_chunked(),
                });
            }
            Some(_) => {}
        }
    }

    for (name, from_item) in &from_files {
        if !scope_is_full && scope.classify(&join_key(key, name)) == ScopeClass::OutOfScope {
            continue;
        }

        if !to_files.contains_key(*name) {
            changes.push(TreeChange {
                kind: ChangeKind::Removed,
                path: join_key(key, name),
                old_hash: Some(from_item.hash.clone()),
                new_hash: None,
                moved_from: None,
                is_binary: from_item.item_type.is_chunked(),
            });
        }
    }

    let from_subtrees: BTreeMap<&String, &TreeItem> = from
        .map(|tree| tree.get_subtrees().collect())
        .unwrap_or_default();
    let to_subtrees: BTreeMap<&String, &TreeItem> = to
        .map(|tree| tree.get_subtrees().collect())
        .unwrap_or_default();

    for (name, to_subtree) in &to_subtrees {
        // An out-of-scope subtree is sealed by hash — never load or descend into it.
        if !scope_is_full && scope.classify(&join_key(key, name)) == ScopeClass::OutOfScope {
            continue;
        }

        let from_subtree = from_subtrees.get(*name);

        // Identical subtree hashes mean identical content all the way down.
        if from_subtree.map(|subtree| subtree.hash == to_subtree.hash).unwrap_or(false) {
            continue;
        }

        // Every load below is only ever borrowed by this recursive walk (subtree/file lookups),
        // and a two-revision diff commonly re-enters the same shared ancestor across many
        // subtrees — the shared form turns a re-entry into a pointer clone instead of a full
        // clone of the tree's entries.
        let from_loaded = match from_subtree {
            Some(subtree) => Some(object_utils::load_tree_shared(&subtree.hash)?),
            None => None,
        };
        let to_loaded = object_utils::load_tree_shared(&to_subtree.hash)?;

        collect_tree_changes(
            from_loaded.as_deref(),
            Some(to_loaded.as_ref()),
            &join_key(key, name),
            changes,
            scope,
        )?;
    }

    for (name, from_subtree) in &from_subtrees {
        if !scope_is_full && scope.classify(&join_key(key, name)) == ScopeClass::OutOfScope {
            continue;
        }

        if !to_subtrees.contains_key(*name) {
            let from_loaded = object_utils::load_tree_shared(&from_subtree.hash)?;

            collect_tree_changes(Some(from_loaded.as_ref()), None, &join_key(key, name), changes, scope)?;
        }
    }

    Ok(())
}

/// The header label of a change: its path, or "old -> new" for moved files.
fn change_label(change: &stocktake_utils::Change) -> String {
    match &change.moved_from {
        Some(from) => format!("{} -> {}", from, change.path),
        None => change.path.clone(),
    }
}

/// Check whether a reported path falls under the filter path (or there is no filter).
fn is_within(path: &str, filter: Option<&WarehousePath>) -> bool {
    let Some(filter) = filter else {
        return true;
    };

    if filter.is_root() {
        return true;
    }

    let key = filter.as_key();

    path == key || (path.starts_with(key)
        && path[key.len()..].starts_with(file_utils::PATH_SEPARATOR_CHAR))
}

/// Get the staged content of a tracked file: its inventory entry's blob.
///
/// # Arguments
/// * `path`   - The warehouse path of the file.
/// * `shards` - The per-directory inventories the preceding stocktake walk already parsed
///              (see `stocktake_utils::collect_staged_changes_with_shards`/
///              `collect_unstaged_changes_with_shards`). A directory's shard is read fresh
///              only when it is missing from this invocation-scoped memo.
///
/// # Returns
/// * `Ok(Vec<u8>)` - The blob content.
/// * `Err(String)` - If the file has no inventory entry or the blob could not be read.
fn inventory_content(path: &str, shards: &HashMap<String, Arc<Inventory>>) -> Result<DiffSide, String> {
    let (parent_key, name) = split_parent(path);

    let inventory = match shards.get(parent_key) {
        Some(inventory) => Arc::clone(inventory),
        None => Arc::new(stocktake_utils::load_shard_or_empty(parent_key)?),
    };

    let Some(item) = inventory.get_item_by_name(name) else {
        return Err(format!("\"{}\" is not in the inventory.", path));
    };

    // A chunked file's inventory hash is a recipe — never load it as a blob; report binary.
    if item.item_type.is_chunked() {
        return Ok(DiffSide::Binary);
    }

    Ok(DiffSide::Text(object_utils::load_blob(&item.hash)?.content))
}

/// Get the working-directory content of a tracked file (a symlink's content is its
/// target path, matching how symlinks are stored).
///
/// # Arguments
/// * `path` - The warehouse path of the file.
///
/// # Returns
/// * `Ok(Vec<u8>)` - The file content.
/// * `Err(String)` - If the file could not be read.
fn worktree_content(path: &str) -> Result<DiffSide, String> {
    let fs_path = Path::new(path);
    let metadata = file_utils::get_symlink_metadata_for_path(fs_path)?;
    let item_type = file_utils::get_type_of_dir_entry(&metadata);
    let name = split_parent(path).1;

    // A worktree file at or above the chunk threshold would be tracked chunked (and is too big to
    // read whole into memory just to diff): report it as binary rather than loading it. Symlinks
    // and directories are never large — only a regular file can trip this.
    if item_type.is_file()
        && item_type != DirEntryType::SymbolicLink
        && metadata.len() >= chunk_utils::CHUNK_THRESHOLD_BYTES as u64 {
        return Ok(DiffSide::Binary);
    }

    Ok(DiffSide::Text(object_utils::get_blob_for_file(name, fs_path, &item_type)?.content))
}

/// Get the pallet-head content of a file: its blob in the head parcel's tree.
///
/// # Arguments
/// * `head_tree_hash` - The hash of the head parcel's root tree.
/// * `path`           - The warehouse path of the file.
///
/// # Returns
/// * `Ok(Vec<u8>)` - The blob content.
/// * `Err(String)` - If the file is not in the head tree or the blob could not be read.
fn head_content(head_tree_hash: Option<&str>, path: &str) -> Result<DiffSide, String> {
    let file = match head_tree_hash {
        Some(tree_hash) => object_utils::resolve_tree_file(tree_hash, path)?,
        None => None,
    };

    match file {
        // A chunked head entry's hash is a recipe — report binary without loading it.
        Some((_, item_type)) if item_type.is_chunked() => Ok(DiffSide::Binary),
        Some((hash, _)) => Ok(DiffSide::Text(object_utils::load_blob(&hash)?.content)),
        None => Err(format!("\"{}\" is not in the pallet head.", path)),
    }
}

/// Print one file's diff: a header with the change kind, then the changed lines (or a
/// note for binary content). Files are separated by a blank line.
///
/// # Arguments
/// * `kind`        - The kind of the change (the header label).
/// * `path`        - The warehouse path of the file.
/// * `old`         - The old content.
/// * `new`         - The new content.
/// * `printed_any` - Whether a previous file was already printed (set by this function).
/// * `verbose`     - Whether to print unchanged lines too.
fn print_file_diff(kind: ChangeKind,
                   path: &str,
                   old: &DiffSide,
                   new: &DiffSide,
                   printed_any: &mut bool,
                   verbose: bool) {
    if *printed_any {
        println!();
    }

    *printed_any = true;

    print!("{}", format_file_diff(kind, path, old, new, verbose));
}

/// Format one file's diff into a block of text — the header line, then the changed lines
/// (or a note for binary content), each line terminated with a newline. The pure sibling
/// of [`print_file_diff`]: it touches no shared state and does not print, so a batch of
/// them can be computed in parallel and printed in order afterwards.
///
/// # Arguments
/// * `kind`    - The kind of the change (the header label).
/// * `path`    - The warehouse path of the file (already a "old -> new" label if moved).
/// * `old`     - The old content.
/// * `new`     - The new content.
/// * `verbose` - Whether to include unchanged lines too.
fn format_file_diff(kind: ChangeKind, path: &str, old: &DiffSide, new: &DiffSide, verbose: bool) -> String {
    use std::fmt::Write;

    let mut out = String::new();

    let _ = writeln!(out, "\x1b[1m{}: {}\x1b[0m", kind, path);

    // A chunked file (or non-text bytes on either side) is reported as binary, never line-diffed.
    if old.is_binary() || new.is_binary() {
        let _ = writeln!(out, "  (binary contents; not shown line by line)");
        return out;
    }

    format_diff_lines(&mut out, &diff::lines(old.bytes(), new.bytes(), verbose));

    out
}

/// Append diff lines with aligned old/new line-number columns to `out`. Additions are
/// green, removals red; whitespace-only changes are rendered faint so meaningful changes
/// stand out (the change is still real — the content hash changes).
///
/// # Arguments
/// * `out`   - The buffer to append to.
/// * `diffs` - The diff lines, in file order.
fn format_diff_lines(out: &mut String, diffs: &[Diff]) {
    use std::fmt::Write;

    let old_width = diffs.iter()
        .filter_map(|d| d.line_number_old)
        .max()
        .map(|n| n.to_string().len())
        .unwrap_or(0);

    let new_width = diffs.iter()
        .filter_map(|d| d.line_number_new)
        .max()
        .map(|n| n.to_string().len())
        .unwrap_or(0);

    for diff in diffs {
        let (color, sign) = match diff.diff_type {
            DiffType::Add => ("\x1b[32m", '+'),
            DiffType::NoOp => ("", ' '),
            DiffType::Remove => ("\x1b[31m", '-'),
        };

        let faint = if diff.is_whitespace_only { "\x1b[2m" } else { "" };

        let old_number = diff.line_number_old.map(|n| n.to_string()).unwrap_or_default();
        let new_number = diff.line_number_new.map(|n| n.to_string()).unwrap_or_default();

        let _ = writeln!(
            out,
            "  {}{}{:>old_width$} {:>new_width$} {} {}\x1b[0m",
            color, faint, old_number, new_number, sign,
            String::from_utf8_lossy(&diff.line)
        );
    }
}

/// Split a warehouse path into its parent directory key and its file name.
fn split_parent(path: &str) -> (&str, &str) {
    path.rsplit_once(file_utils::PATH_SEPARATOR_CHAR).unwrap_or(("", path))
}

/// Join a directory key and an entry name into the entry's warehouse path.
fn join_key(key: &str, name: &str) -> String {
    if key.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", key, name)
    }
}

/// The `--json` diff: the changed-file set. The line-by-line hunks stay a human
/// display (a program reads content by hash when it needs it, and stays token-cheap).
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct DiffReport {
    /// What was compared: `worktree`, `staged` or `pallets`.
    mode: &'static str,

    files: Vec<DiffFileSummary>,
}

/// One changed file in a `--json` diff.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct DiffFileSummary {
    kind: ChangeKind,
    path: String,

    /// The old path, for a moved file.
    #[serde(skip_serializing_if = "Option::is_none")]
    moved_from: Option<String>,
}

impl DiffFileSummary {
    fn from_change(change: &stocktake_utils::Change) -> DiffFileSummary {
        DiffFileSummary {
            kind: change.kind,
            path: change.path.clone(),
            moved_from: change.moved_from.clone(),
        }
    }
}

impl CommandOutput for DiffReport {
    // Only reached under `--json`; the human diff renders inline in each mode above.
    fn render_human(&self) {}
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("DiffReport", schemars::schema_for!(DiffReport)),
    ]
}
