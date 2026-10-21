use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use crate::enums::dir_entry_type::DirEntryType;
use crate::model::inventory::{Inventory, InventoryItem};
use crate::model::tree_item::TreeItem;
use crate::util::progress_utils::Progress;
use crate::util::scope_utils::{self, MaterializationScope, ScopeClass};
use crate::util::{alias_utils, fanout_utils, file_utils, inventory_utils, object_utils, office_utils, tree_utils, warehouse_utils};

/// A single file operation needed to turn one tree into another in the working directory.
pub enum FileOp {
    /// Write the blob with the given hash to the given path.
    Write {
        /// The warehouse path of the file.
        path: String,
        /// The blob hash of the content.
        hash: String,
        /// The type of the entry (normal / executable / symlink).
        item_type: DirEntryType,
        /// Whether the file is new (it does not exist in the source tree). New files must
        /// not overwrite anything: whatever the path resolves to on disk — an untracked file,
        /// or on a case-insensitive file system another entry spelled differently — is a
        /// conflict (see [`collect_collisions`]), and the write creates the file exclusively.
        is_new: bool,
    },

    /// Remove the file at the given path.
    Remove {
        /// The warehouse path of the file.
        path: String,
    },
}

/// Compute the file operations that turn the `from` tree into the `to` tree, and the
/// directories that exist only in the `from` tree (candidates for removal, deepest first).
///
/// Subtrees with identical hashes are skipped entirely — unchanged parts of the warehouse
/// are never visited.
///
/// # Arguments
/// * `from` - The hash of the source root tree (`None` when shifting from an unborn pallet).
/// * `to`   - The hash of the target root tree.
///
/// # Returns
/// * `Ok((Vec<FileOp>, Vec<String>))` - The file operations and the removed directories
///                                      (deepest first).
/// * `Err(String)`                    - If a tree object could not be loaded.
pub fn diff_trees(from: Option<&str>, to: &str) -> Result<(Vec<FileOp>, Vec<String>), String> {
    let from_tree = match from {
        Some(hash) => Some(object_utils::load_tree(hash)?),
        None => None,
    };
    let to_tree = object_utils::load_tree(to)?;

    // Every materializer comes through here, so the metadata-folder refusal is enforced once for
    // all of them rather than by each caller remembering it.
    office_utils::ensure_not_metadata_root(&to_tree)?;

    let mut ops: Vec<FileOp> = Vec::new();
    let mut removed_dirs: Vec<String> = Vec::new();

    // In a scoped (sparse) bay this restricts materialization to the in-scope subtree(s): the
    // walk copies nothing out of scope, so no out-of-scope blob is ever written and no absent
    // object is touched. The scope is full (no restriction) in a plain bay or the main tree, so
    // this is a no-op there and the behavior is byte-for-byte what it always was.
    let scope = scope_utils::current_scope()?;

    let mut aliased_removes = Vec::new();
    diff_directory(from_tree.as_ref(), Some(&to_tree), "", &mut Plan {
        ops: &mut ops, removed_dirs: &mut removed_dirs, aliased_removes: &mut aliased_removes,
    }, false, &scope)?;

    keep_aliased_files(&to_tree, &aliased_removes, &mut ops, &scope)?;

    // Deepest directories first, so empty-directory cleanup can proceed bottom-up.
    removed_dirs.sort_by_key(|dir| std::cmp::Reverse(dir.matches('/').count()));

    Ok((ops, removed_dirs))
}

/// What [`diff_directory`] collects.
struct Plan<'a> {
    /// The file operations.
    ops: &'a mut Vec<FileOp>,
    /// The source-only directories.
    removed_dirs: &'a mut Vec<String>,
    /// Each removed path some component of which shares a [`alias_utils::fold_key`] with a
    /// sibling in the source tree: the only removals that can reach a kept file through an alias
    /// (see [`keep_aliased_files`]).
    aliased_removes: &'a mut Vec<String>,
}

/// Diff one directory level of two trees (recursively).
///
/// # Arguments
/// * `from`    - This directory in the source tree (if it exists there).
/// * `to`      - This directory in the target tree (if it exists there).
/// * `key`     - The warehouse path key of the directory.
/// * `plan`    - Where the operations, removed directories and aliased removals are collected.
/// * `aliased` - Whether some component of `key` shares a fold key with a source sibling.
///
/// # Returns
/// * `Ok(())`      - If the directory was diffed.
/// * `Err(String)` - If a subtree object could not be loaded.
fn diff_directory(from: Option<&TreeItem>,
                  to: Option<&TreeItem>,
                  key: &str,
                  plan: &mut Plan,
                  aliased: bool,
                  scope: &MaterializationScope) -> Result<(), String> {
    // Hoisted once per directory (not per entry): a full (unscoped) scope always classifies
    // everything in scope, so on the hot, common full-bay path the scope checks below are
    // pure overhead — short-circuit them away entirely, the same way `stack_parcel` gates the
    // whole overlay on `is_full()`.
    let scope_is_full = scope.is_full();

    let from_files: BTreeMap<&String, &TreeItem> = from
        .map(|tree| tree.get_files().collect())
        .unwrap_or_default();
    let to_files: BTreeMap<&String, &TreeItem> = to
        .map(|tree| tree.get_files().collect())
        .unwrap_or_default();

    // The source names here that share a fold key with another: a removal beneath one may reach,
    // on a folding volume, the file its sibling keeps.
    let paired = from.map(fold_paired_names).unwrap_or_default();

    for (name, to_item) in &to_files {
        let child_key = join_key(key, name);

        if !scope_is_full {
            // A file where the scope expects a directory (a spine ancestor, or the in-scope
            // prefix itself) is the §3.1 type change — refused rather than guessed. Out-of-scope
            // files are sealed and never materialized; only in-scope files are written.
            if scope.spine_file_is_sealed(&child_key)? {
                continue;
            }
        }

        let from_item = from_files.get(*name);

        let is_unchanged = from_item
            .map(|item| item.hash == to_item.hash && item.item_type == to_item.item_type)
            .unwrap_or(false);

        if !is_unchanged {
            plan.ops.push(FileOp::Write {
                path: child_key,
                hash: to_item.hash.clone(),
                item_type: to_item.item_type,
                is_new: from_item.is_none(),
            });
        }
    }

    for (name, _) in &from_files {
        let child_key = join_key(key, name);

        // Out-of-scope files were never materialized, so there is nothing to remove there.
        if !scope_is_full && scope.classify(&child_key) == ScopeClass::OutOfScope {
            continue;
        }

        if !to_files.contains_key(*name) {
            if aliased || paired.contains(name.as_str()) {
                plan.aliased_removes.push(child_key.clone());
            }
            plan.ops.push(FileOp::Remove { path: child_key });
        }
    }

    let from_subtrees: BTreeMap<&String, &TreeItem> = from
        .map(|tree| tree.get_subtrees().collect())
        .unwrap_or_default();
    let to_subtrees: BTreeMap<&String, &TreeItem> = to
        .map(|tree| tree.get_subtrees().collect())
        .unwrap_or_default();

    for (name, to_subtree) in &to_subtrees {
        let child_key = join_key(key, name);

        // An out-of-scope subtree is sealed by hash: never load it, never materialize it. A
        // shift whose out-of-scope siblings changed simply re-materializes in scope; the new
        // hashes flow into the next stack through the overlay (§3.2/§3.5).
        if !scope_is_full && scope.classify(&child_key) == ScopeClass::OutOfScope {
            continue;
        }

        let from_subtree = from_subtrees.get(*name);

        // Identical subtree hashes mean identical content all the way down.
        if from_subtree.map(|subtree| subtree.hash == to_subtree.hash).unwrap_or(false) {
            continue;
        }

        let from_loaded = match from_subtree {
            Some(subtree) => Some(object_utils::load_tree(&subtree.hash)?),
            None => None,
        };
        let to_loaded = object_utils::load_tree(&to_subtree.hash)?;

        diff_directory(
            from_loaded.as_ref(),
            Some(&to_loaded),
            &child_key,
            plan,
            aliased || paired.contains(name.as_str()),
            scope,
        )?;
    }

    for (name, from_subtree) in &from_subtrees {
        let subtree_key = join_key(key, name);

        if !scope_is_full && scope.classify(&subtree_key) == ScopeClass::OutOfScope {
            continue;
        }

        if !to_subtrees.contains_key(*name) {
            let from_loaded = object_utils::load_tree(&from_subtree.hash)?;

            diff_directory(Some(&from_loaded), None, &subtree_key, plan, aliased || paired.contains(name.as_str()), scope)?;
            plan.removed_dirs.push(subtree_key);
        }
    }

    Ok(())
}

/// The names in `tree` (files and subtrees) that share an [`alias_utils::fold_key`] with another.
fn fold_paired_names(tree: &TreeItem) -> HashSet<&str> {
    let mut classes: HashMap<String, Vec<&str>> = HashMap::new();

    for (name, _) in tree.get_files().chain(tree.get_subtrees()) {
        classes.entry(alias_utils::fold_key(name)).or_default().push(name.as_str());
    }

    classes.into_values().filter(|names| names.len() > 1).flatten().collect()
}

/// Keep every file the target holds that a planned removal would unlink through an alias (see
/// [`kept_through_aliases`]), as a write of it from the target. Removals run before writes (see
/// [`apply_ops_each`]), so it is written back under its own spelling once the removal is done.
/// The added write is not new: what occupies the kept path is the removed one, which the plan
/// removes first.
///
/// # Arguments
/// * `to`              - The target root tree.
/// * `aliased_removes` - The removals to check (see [`Plan::aliased_removes`]).
/// * `ops`             - The plan's operations, extended in place.
/// * `scope`           - The materialization scope.
///
/// # Returns
/// * `Ok(())`      - If every kept file was judged.
/// * `Err(String)` - If a tree could not be loaded, or the disk could not be read.
fn keep_aliased_files(to: &TreeItem,
                      aliased_removes: &[String],
                      ops: &mut Vec<FileOp>,
                      scope: &MaterializationScope) -> Result<(), String> {
    if aliased_removes.is_empty() {
        return Ok(());
    }

    let planned: HashSet<&str> = ops.iter().map(op_path).collect();
    let kept = kept_through_aliases(to, aliased_removes, &planned, scope)?;

    ops.extend(kept.into_iter().map(|(path, item)| FileOp::Write {
        path,
        hash: item.hash,
        item_type: item.item_type,
        is_new: false,
    }));

    Ok(())
}

/// The files `tree` keeps that removing `removed` would unlink through an alias, each with its
/// entry. On a folding volume a tree may hold `README` beside `readme` where the disk holds one
/// file: removing `readme` unlinks `README` too, and an unchanged `README` has no operation of its
/// own to bring it back. So a file `k` of `tree` is kept when it shares a removed path's fold key
/// component by component, has no operation of its own (`planned`), is in scope, and resolves on
/// disk now to the one listed entry the removed path resolves to. Two paths each listed under its
/// own name (hard links) are not one entry, so removing one never reaches the other. Every
/// materializer that removes files plans its kept files here: the tree diff behind `shift` and
/// `park`, and the merge behind `consolidate` and `cherry-pick`.
///
/// # Arguments
/// * `tree`    - The tree the materialization leaves in place.
/// * `removed` - The paths it removes.
/// * `planned` - Every path it already writes or removes.
/// * `scope`   - The materialization scope.
///
/// # Returns
/// * `Ok(Vec<(String, TreeItem)>)` - The kept files, each once.
/// * `Err(String)`                 - If a tree could not be loaded, or the disk could not be read.
pub fn kept_through_aliases(tree: &TreeItem,
                            removed: &[String],
                            planned: &HashSet<&str>,
                            scope: &MaterializationScope) -> Result<Vec<(String, TreeItem)>, String> {
    let mut index = FoldIndex::new(tree);
    let mut check = alias_utils::AliasCheck::new();
    let mut seen = HashSet::new();
    let mut kept = Vec::new();

    for removed in removed {
        // The disk is read only for a removal with a fold-equal candidate, and only along paths
        // the planned-path check accepts: a path it refuses is left to that refusal, which runs
        // before anything is applied.
        let candidates: Vec<(String, TreeItem)> = index.fold_equal_files(removed)?.into_iter()
            .filter(|(path, _)| path != removed
                && !planned.contains(path.as_str())
                && !seen.contains(path)
                && (scope.is_full() || scope.classify(path) != ScopeClass::OutOfScope)
                && check_working_path(path).is_ok())
            .collect();

        if candidates.is_empty() || check_working_path(removed).is_err() {
            continue;
        }

        let Some(removed_listed) = check.resolve(removed)? else {
            continue;
        };

        for (path, item) in candidates {
            if check.resolve(&path)?.as_ref() != Some(&removed_listed) {
                continue;
            }

            seen.insert(path.clone());
            kept.push((path, item));
        }
    }

    Ok(kept)
}

/// A tree's directories and files grouped by fold key, each directory loaded and grouped once
/// however many removals reach it.
struct FoldIndex {
    /// Each directory of the source spelling, as a removed path names it, to the target
    /// directories that share its fold key component by component.
    directories: HashMap<String, Vec<String>>,
    /// Each target directory loaded so far, by key, its entries grouped by fold key.
    groups: HashMap<String, FoldGroups>,
}

/// One directory's entries by fold key.
struct FoldGroups {
    files: HashMap<String, Vec<(String, TreeItem)>>,
    subtrees: HashMap<String, Vec<(String, String)>>,
}

impl FoldGroups {
    fn of(tree: &TreeItem) -> FoldGroups {
        let mut groups = FoldGroups { files: HashMap::new(), subtrees: HashMap::new() };

        for (name, item) in tree.get_files() {
            groups.files.entry(alias_utils::fold_key(name)).or_default().push((name.clone(), item.clone()));
        }

        for (name, item) in tree.get_subtrees() {
            groups.subtrees.entry(alias_utils::fold_key(name)).or_default().push((name.clone(), item.hash.clone()));
        }

        groups
    }
}

impl FoldIndex {
    fn new(root: &TreeItem) -> FoldIndex {
        FoldIndex {
            directories: HashMap::from([(String::new(), vec![String::new()])]),
            groups: HashMap::from([(String::new(), FoldGroups::of(root))]),
        }
    }

    /// The target directories sharing `directory`'s fold key component by component.
    fn fold_equal_directories(&mut self, directory: &str) -> Result<Vec<String>, String> {
        if let Some(found) = self.directories.get(directory) {
            return Ok(found.clone());
        }

        let (parent, name) = directory.rsplit_once('/').unwrap_or(("", directory));
        let fold = alias_utils::fold_key(name);
        let mut found = Vec::new();

        for parent_key in self.fold_equal_directories(parent)? {
            let subtrees = self.groups[&parent_key].subtrees.get(&fold).cloned().unwrap_or_default();

            for (subtree_name, hash) in subtrees {
                let key = join_key(&parent_key, &subtree_name);

                if !self.groups.contains_key(&key) {
                    let groups = FoldGroups::of(&object_utils::load_tree(&hash)?);
                    self.groups.insert(key.clone(), groups);
                }

                found.push(key);
            }
        }

        self.directories.insert(directory.to_string(), found.clone());
        Ok(found)
    }

    /// The files whose path shares `path`'s fold key component by component (`path` itself
    /// included, if the tree holds it), each with its entry.
    fn fold_equal_files(&mut self, path: &str) -> Result<Vec<(String, TreeItem)>, String> {
        let (directory, name) = path.rsplit_once('/').unwrap_or(("", path));
        let fold = alias_utils::fold_key(name);

        Ok(self.fold_equal_directories(directory)?.iter()
            .flat_map(|key| self.groups[key].files.get(&fold).into_iter().flatten()
                .map(|(file_name, item)| (join_key(key, file_name), item.clone())))
            .collect())
    }
}

/// What a diff removes before it writes: every `Remove` path, and every source-only directory
/// (`remove_empty_directories` tries exactly those). A case-only rename's old name is one of
/// these, so its occupancy of the new name is no collision (see
/// [`alias_utils::AliasCheck::judge`]).
pub struct OpRemovals {
    files: HashSet<String>,
    dirs: HashSet<String>,
}

impl OpRemovals {
    /// The removals of one diff.
    pub fn of(ops: &[FileOp], removed_dirs: &[String]) -> OpRemovals {
        OpRemovals {
            files: ops.iter()
                .filter_map(|op| match op {
                    FileOp::Remove { path } => Some(path.clone()),
                    FileOp::Write { .. } => None,
                })
                .collect(),
            dirs: removed_dirs.iter().cloned().collect(),
        }
    }
}

impl alias_utils::Removals for OpRemovals {
    fn removes_file(&self, key: &str) -> bool {
        self.files.contains(key)
    }

    fn removes_directory(&self, key: &str) -> bool {
        self.dirs.contains(key)
    }
}

/// Find the new-file writes in `ops` that would overwrite something already on disk: used up
/// front, before anything is touched, by every diff-driven materializer (`shift`, consolidate's
/// fast-forward, [`materialize_tree`], `park` and `unpark`).
///
/// A new path is judged by what the file system resolves it to, not by `exists()` (see
/// [`alias_utils`]): a path that only resolves through a case alias of an entry this diff
/// removes is free (a case-only rename), one that aliases a tracked entry that stays is refused
/// naming both, and two new paths the volume would land on one file are refused naming both.
///
/// A path that exists on disk under its own name is not a collision when it is a tracked, fully
/// clean directory (every entry beneath it tracked) — that is a tracked directory the diff is
/// legitimately replacing with a file (a dir→file flip). Applying the diff must then delete the
/// directory's contents before writing the file (see `apply_ops`); a directory with any
/// untracked content beneath it, ignored or not, is a genuine collision and still refuses.
///
/// Every operation's path, writes and removals alike, first goes through [`check_planned_paths`],
/// so a path that names a metadata folder, or lies inside another working tree, refuses the whole
/// diff before its first file changes. [`check_working_path`] inside each write and removal is the
/// backstop, and the only place a symlink parent is judged, once earlier operations have run.
///
/// # Arguments
/// * `ops`          - The file operations a diff produced.
/// * `removed_dirs` - The directories the same diff found source-only.
///
/// # Returns
/// * `Ok(Vec<Collision>)` - The collisions (empty when there are none).
/// * `Err(String)`        - If some operation's path is refused (see [`check_planned_paths`]), or a
///                          directory or a shard could not be read or parsed.
pub fn collect_collisions(ops: &[FileOp], removed_dirs: &[String]) -> Result<Vec<alias_utils::Collision>, String> {
    check_planned_paths(ops.iter().map(|op| {
        let (FileOp::Write { path, .. } | FileOp::Remove { path }) = op;
        path.as_str()
    }))?;

    let removals = OpRemovals::of(ops, removed_dirs);
    let mut check = alias_utils::AliasCheck::new();
    let mut collisions = Vec::new();
    let mut created = Vec::new();

    for op in ops {
        let FileOp::Write { path, is_new: true, .. } = op else { continue };

        created.push(path.as_str());

        match check.judge(path, &removals)? {
            alias_utils::Verdict::Proceed => {}
            alias_utils::Verdict::Refuse(collision) => collisions.push(collision),
            alias_utils::Verdict::Exists => {
                let fs_path = Path::new(path);

                if fs_path.is_dir() && inventory_utils::directory_is_safe_to_replace(check.ledger()?, path)? {
                    continue;
                }

                collisions.push(alias_utils::Collision::Untracked(path.clone()));
            }
        }
    }

    collisions.extend(check.created_pairs(created, &removals)?);

    Ok(collisions)
}

/// Apply a diff's file operations to the working directory: every removal first (so a
/// directory a new file is about to replace is emptied), then `remove_empty_directories`
/// clears the now-empty source-only directories (deepest first) — the removals themselves
/// only unlink files and never touch a directory — then every write lands. Writing before a
/// same-path removal would otherwise fail (`EISDIR`/`EEXIST`) for a type flip in either
/// direction.
///
/// # Arguments
/// * `ops`          - The file operations to apply.
/// * `removed_dirs` - The directories a diff found source-only (deepest first).
///
/// # Returns
/// * `Ok(())`      - If every operation applied.
/// * `Err(String)` - If a file system operation failed.
pub fn apply_ops(ops: &[FileOp], removed_dirs: &[String]) -> Result<(), String> {
    apply_ops_each(ops, removed_dirs, |_| Ok(()))
}

/// [`apply_ops`], calling `after` with each operation once it has applied (for progress, or to
/// stage it). The one place the removals-first order lives: every materializer applies through
/// here.
///
/// # Arguments
/// * `ops`          - The file operations to apply.
/// * `removed_dirs` - The directories a diff found source-only (deepest first).
/// * `after`        - Called after each operation applies; an error stops the run.
///
/// # Returns
/// * `Ok(())`      - If every operation applied.
/// * `Err(String)` - If a file system operation, or `after`, failed.
pub fn apply_ops_each(ops: &[FileOp],
                      removed_dirs: &[String],
                      mut after: impl FnMut(&FileOp) -> Result<(), String>) -> Result<(), String> {
    for op in ops.iter().filter(|op| matches!(op, FileOp::Remove { .. })) {
        apply_file_op(op)?;
        after(op)?;
    }

    remove_empty_directories(removed_dirs);

    let writes: Vec<&FileOp> = ops.iter().filter(|op| matches!(op, FileOp::Write { .. })).collect();

    write_in_order(&writes, |op| match op {
        FileOp::Write { hash, item_type, .. } => Some((hash.as_str(), *item_type)),
        FileOp::Remove { .. } => None,
    }, |op, content| {
        if let FileOp::Write { path, hash, item_type, is_new } = op {
            write_tracked_file_from(path, hash, *item_type, *is_new, content)?;
        }

        after(op)
    })
}

/// Below this many blobs to load, a materializer loads each one itself: the prefetch's threads
/// cost more than they save on a handful of files.
const PREFETCH_THRESHOLD: usize = 8;

/// How many blobs the prefetch may load ahead of the write that needs the next one.
const PREFETCH_WINDOW: usize = 256;

/// The bytes of loaded, unwritten blobs at which the prefetch stops loading ahead. Loads already
/// under way finish, so the most held is this plus one blob per worker, and a prefetched blob is
/// under the chunk threshold (larger ones stream at write time).
const PREFETCH_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// The blob a write loads ahead of time, if any: a chunked entry is streamed at write time
/// instead, so nothing is loaded for it.
fn prefetched<'a>(blob: Option<(&'a str, DirEntryType)>) -> Option<&'a str> {
    blob.filter(|(_, item_type)| !item_type.is_chunked()).map(|(hash, _)| hash)
}

/// Run a materializer's writes in order on this thread while their blobs load ahead on worker
/// threads (see [`fanout_utils::prefetch_in_order`]): reconstructing a blob from its delta chain
/// is CPU work that overlaps, while the file system serializes the writes themselves, so they
/// stay on one thread. Below [`PREFETCH_THRESHOLD`] blobs to load, each write loads its own.
///
/// # Arguments
/// * `items` - The writes, in the order they must land.
/// * `blob`  - The blob one item writes (its hash and type), or `None` if it writes no stored blob.
/// * `write` - Applies one item with its prefetched content (`None` if nothing was loaded for it,
///             e.g. a chunked entry, which streams); the first error stops the run.
///
/// # Returns
/// * `Ok(())`      - If every write applied.
/// * `Err(String)` - The first write's error, after every write before it has landed.
pub fn write_in_order<T: Sync>(items: &[T],
                               blob: impl Fn(&T) -> Option<(&str, DirEntryType)> + Sync,
                               mut write: impl FnMut(&T, Option<Result<Vec<u8>, String>>) -> Result<(), String>)
                               -> Result<(), String> {
    if items.iter().filter(|item| prefetched(blob(item)).is_some()).take(PREFETCH_THRESHOLD).count() < PREFETCH_THRESHOLD {
        return items.iter().try_for_each(|item| write(item, None));
    }

    fanout_utils::prefetch_in_order(
        items,
        PREFETCH_WINDOW,
        PREFETCH_BUDGET_BYTES,
        |item| prefetched(blob(item)).map(|hash| object_utils::load_blob(hash).map(|loaded| loaded.content)),
        |content| match content {
            Some(Ok(bytes)) => bytes.len(),
            _ => 0,
        },
        write,
    )
}

/// The warehouse path a file operation acts on.
pub fn op_path(op: &FileOp) -> &str {
    match op {
        FileOp::Write { path, .. } | FileOp::Remove { path } => path,
    }
}

/// Apply a file operation to the working directory.
///
/// # Arguments
/// * `op` - The file operation to apply.
///
/// # Returns
/// * `Ok(())`      - If the operation was applied.
/// * `Err(String)` - If a file system operation failed.
pub fn apply_file_op(op: &FileOp) -> Result<(), String> {
    match op {
        FileOp::Write { path, hash, item_type, is_new } => write_tracked_file(path, hash, *item_type, *is_new),
        FileOp::Remove { path } => {
            check_working_path(path)?;

            match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                // The file being gone already is the desired outcome.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!("Error while removing \"{}\": {}", path, e)),
            }
        }
    }
}

/// Write a tracked file (normal, executable or symlink) from its blob to the working
/// directory, creating parent directories as needed.
///
/// A `create` write is of a path the source tree did not hold: it removes nothing first and
/// creates the file exclusively, so whatever already resolves at the path — on a case-folding
/// volume, another entry under a different spelling — fails the write loudly instead of being
/// truncated, replaced or deleted. It is the backstop under the up-front checks
/// ([`collect_collisions`] and the merge's), for any alias they cannot see.
///
/// # Arguments
/// * `path`      - The warehouse path of the file.
/// * `hash`      - The blob hash of the content.
/// * `item_type` - The type of the entry.
/// * `create`    - Whether the path is new (nothing of ours is at it): create, never replace.
///
/// # Returns
/// * `Ok(())`      - If the file was written.
/// * `Err(String)` - If the blob could not be loaded, something already exists at a `create`
///                   path, or a file system operation failed.
pub fn write_tracked_file(path: &str, hash: &str, item_type: DirEntryType, create: bool) -> Result<(), String> {
    write_tracked_file_from(path, hash, item_type, create, None)
}

/// [`write_tracked_file`], with the blob's content already loaded when `content` holds it (see
/// [`write_in_order`]). A load's error surfaces only after the path checks, where the write's own
/// load would have failed. `None` loads the blob here.
pub fn write_tracked_file_from(path: &str,
                               hash: &str,
                               item_type: DirEntryType,
                               create: bool,
                               content: Option<Result<Vec<u8>, String>>) -> Result<(), String> {
    check_working_path(path)?;

    let fs_path = Path::new(path);

    if let Some(parent) = fs_path.parent() {
        if !parent.as_os_str().is_empty() {
            file_utils::create_folder_if_not_exists(parent)?;
        }
    }

    // A chunked file's hash names a recipe, not a blob: stream its chunks to the target file in
    // order, verifying `Blake3(assembled) == recipe.content_hash` during assembly (bounded to one
    // chunk in memory at a time, never the whole file). Dispatched on the tree entry type with no
    // extra object load to discover the path.
    if item_type.is_chunked() {
        return write_chunked_file(path, fs_path, hash, item_type, create);
    }

    let content = match content {
        Some(loaded) => loaded?,
        None => object_utils::load_blob(hash)?.content,
    };

    if item_type == DirEntryType::SymbolicLink {
        let target = String::from_utf8(content)
            .map_err(|_| format!("The symlink target stored for \"{}\" is not valid UTF-8.", path))?;

        // A symlink cannot be overwritten in place; remove whatever is at the path first —
        // unless the path is new, where what resolves there is not ours to remove and
        // `symlink` refuses it on its own.
        if !create {
            match std::fs::remove_file(fs_path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("Error while replacing \"{}\": {}", path, e)),
            }
        }

        #[cfg(unix)]
        return std::os::unix::fs::symlink(&target, fs_path)
            .map_err(|e| creation_error(path, "creating symlink", e));

        #[cfg(windows)]
        return Err(format!(
            "Cannot materialize symlink \"{}\" (pointing at \"{}\") on Windows.",
            path, target
        ));
    }

    if create {
        return create_regular_file(fs_path, path, &content, item_type);
    }

    write_regular_file(fs_path, path, &content, item_type)
}

/// The error for a failed write of a new path: an occupant names the backstop that caught it.
fn creation_error(path: &str, what: &str, e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::AlreadyExists {
        format!(
            "Error while {} \"{}\": something already exists at that path on disk (on a \
            case-insensitive file system, possibly another entry spelled differently); it was \
            left untouched.",
            what, path
        )
    } else {
        format!("Error while {} \"{}\": {}", what, path, e)
    }
}

/// Create a regular file at a new path exclusively (see [`write_tracked_file`]'s `create`).
fn create_regular_file(fs_path: &Path, path: &str, content: &[u8], item_type: DirEntryType) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(fs_path)
        .map_err(|e| creation_error(path, "creating", e))?;

    std::io::Write::write_all(&mut file, content)
        .map_err(|e| format!("Error while writing \"{}\": {}", path, e))?;
    drop(file);

    set_file_mode(fs_path, path, item_type)
}

/// Write `content` to the working directory at `path` as a regular file, creating parent
/// directories as needed, after [`check_working_path`] — for content that comes from no blob
/// (a merge result).
///
/// # Arguments
/// * `path`      - The warehouse path of the file.
/// * `content`   - The bytes to write.
/// * `item_type` - The entry type, for the executable bit.
/// * `create`    - Whether the path is new to the working directory: then the file is created
///                 exclusively, as [`write_tracked_file`] does, and anything already there is an
///                 error rather than overwritten.
///
/// # Returns
/// * `Ok(())`      - If the file was written.
/// * `Err(String)` - If the path is refused or a file system operation failed.
pub fn write_working_file(path: &str, content: &[u8], item_type: DirEntryType, create: bool) -> Result<(), String> {
    check_working_path(path)?;

    let fs_path = Path::new(path);

    if let Some(parent) = fs_path.parent() {
        if !parent.as_os_str().is_empty() {
            file_utils::create_folder_if_not_exists(parent)?;
        }
    }

    if create {
        return create_regular_file(fs_path, path, content, item_type);
    }

    write_regular_file(fs_path, path, content, item_type)
}

/// Write a regular file in place. A symlink already at the path (one this tree turns into a
/// regular file) is removed first: writing through it would land in whatever it points at.
fn write_regular_file(fs_path: &Path, path: &str, content: &[u8], item_type: DirEntryType) -> Result<(), String> {
    if std::fs::symlink_metadata(fs_path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        std::fs::remove_file(fs_path)
            .map_err(|e| format!("Error while replacing the symlink \"{}\": {}", path, e))?;
    }

    std::fs::write(fs_path, content)
        .map_err(|e| format!("Error while writing \"{}\": {}", path, e))?;

    set_file_mode(fs_path, path, item_type)
}

/// Refuse a warehouse path that would put a working-directory write or removal anywhere but the
/// place the path names inside the working directory. Every write and removal of tracked content
/// goes through here, because the path comes from a tree that may have arrived from a remote.
///
/// Refused:
/// * a component, at any depth, that opens a metadata folder: by any spelling some file system
///   takes for it, or by resolving to it on this volume (see
///   [`office_utils::metadata_folder_component`]). The root's is this warehouse's own; one below
///   it would plant another working tree, which every later command run inside it would use;
/// * a parent directory, at any depth, that is the root of another working tree (see
///   [`warehouse_utils::is_working_tree_root`]): its content is not this warehouse's to write;
/// * a parent directory, at any depth, that is a symlink: following it would write or remove
///   wherever it points. A file system that ignores case also reaches a symlink `lib` through a
///   tree directory `LIB`;
/// * on Windows, a component the file system would not take as one plain file name (see
///   [`windows_component_problem`]).
///
/// The tree parser has already refused empty, `.`, `..`, `/` and NUL names.
///
/// # Arguments
/// * `path` - The warehouse path, relative to the working directory, `/`-separated.
///
/// # Returns
/// * `Ok(())`      - If the path is safe to write or remove.
/// * `Err(String)` - The refusal, naming the path Debug-quoted.
pub fn check_working_path(path: &str) -> Result<(), String> {
    let components: Vec<&str> = path.split('/').collect();

    if let Some((depth, component)) = office_utils::metadata_folder_component(path)? {
        return Err(metadata_component_refusal(path, depth, component));
    }

    #[cfg(windows)]
    for component in &components {
        if let Some(problem) = windows_component_problem(component) {
            return Err(format!("Refusing to touch {:?}: the name {:?} {}.", path, component, problem));
        }
    }

    check_ancestors(path, &components, true, None)
}

/// The part of [`check_working_path`] that holds before an operation applies, for the pass that
/// runs over every operation's path before the first one does: refuse a path with a component
/// that opens a metadata folder, or one below the root of another working tree as the working
/// directory stands now. An ancestor that is not a real directory yet (missing, a file or a
/// symlink) ends the walk, since an earlier operation may still replace it;
/// [`check_working_path`] judges it again inside the write.
///
/// Nothing is written while the pass runs, so what it finds for each ancestor directory is
/// remembered for the paths after it: a checkout pays two `lstat`s per directory, not per file.
///
/// # Arguments
/// * `paths` - The warehouse paths, relative to the working directory, `/`-separated.
///
/// # Returns
/// * `Ok(())`      - If nothing about any path refuses it before anything applies.
/// * `Err(String)` - The first refusal, naming its path Debug-quoted.
pub fn check_planned_paths<'a>(paths: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
    let mut walked: HashMap<PathBuf, bool> = HashMap::new();

    for path in paths {
        if let Some((depth, component)) = office_utils::metadata_folder_component(path)? {
            return Err(metadata_component_refusal(path, depth, component));
        }

        let components: Vec<&str> = path.split('/').collect();
        check_ancestors(path, &components, false, Some(&mut walked))?;
    }

    Ok(())
}

/// Walk the existing ancestors of `path` from the root, refusing one that is another working
/// tree's root, and, if `refuse_symlinks`, one that is a symlink. Without it a symlink ends the
/// walk like a missing ancestor. `walked`, when given, holds what earlier walks of the same
/// unchanged working directory found: `true` for a real directory to walk on through, `false` for
/// one that ends the walk.
fn check_ancestors(path: &str,
                   components: &[&str],
                   refuse_symlinks: bool,
                   mut walked: Option<&mut HashMap<PathBuf, bool>>) -> Result<(), String> {
    let mut ancestor = PathBuf::new();

    for component in &components[..components.len().saturating_sub(1)] {
        ancestor.push(component);

        if let Some(through) = walked.as_deref().and_then(|walked| walked.get(&ancestor)) {
            if *through {
                continue;
            }
            break;
        }

        let through = match std::fs::symlink_metadata(&ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if refuse_symlinks {
                    return Err(format!(
                        "Refusing to touch {:?}: its parent {:?} is a symlink, and following it could \
                        leave the working directory.",
                        path,
                        ancestor.to_string_lossy()
                    ));
                }
                false
            }
            // A file where a directory will be: an earlier operation replaces it.
            Ok(metadata) if !metadata.is_dir() => false,
            Ok(_) => {
                if warehouse_utils::is_working_tree_root(&ancestor)? {
                    return Err(nested_working_tree_refusal(path, &ancestor.to_string_lossy()));
                }
                true
            }
            // Nothing further down exists yet (or, mid-apply, a file stands above it), so nothing
            // further down can be a symlink or another working tree.
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => false,
            Err(e) => return Err(format!("Error while checking \"{}\": {}", ancestor.to_string_lossy(), e)),
        };

        if let Some(walked) = walked.as_deref_mut() {
            walked.insert(ancestor.clone(), through);
        }
        if !through {
            break;
        }
    }

    Ok(())
}

/// The refusal for a path whose component at `depth` names a metadata folder.
fn metadata_component_refusal(path: &str, depth: usize, component: &str) -> String {
    if depth == 0 {
        format!("Refusing to touch {:?}: it is inside the warehouse's metadata folder.", path)
    } else {
        format!(
            "Refusing to touch {:?}: its component {:?} is, on this or some other file system, a \
            warehouse's metadata folder, and writing it would plant another working tree inside \
            this one.",
            path, component
        )
    }
}

/// The refusal for a path below `root`, the root of another working tree inside this one.
fn nested_working_tree_refusal(path: &str, root: &str) -> String {
    format!(
        "Refusing to touch {:?}: {:?} holds a \".forklift\", so it is another working tree, not \
        part of this one. Move that working tree out of the way first.",
        path, root
    )
}

/// Why Windows would not take `name` as one plain file name, if it would not: a `\` (a path
/// separator there), a `:` (a drive prefix or an alternate data stream), a trailing dot or space
/// (dropped, so `.. ` opens `..`), or a reserved device name (`CON`, `CONIN$`, `NUL`, `COM1`,
/// `COM¹`, … with or without an extension, as git's `is_valid_win32_path` lists them). Compiled
/// everywhere so it is tested everywhere; enforced on Windows.
pub fn windows_component_problem(name: &str) -> Option<&'static str> {
    const RESERVED: [&str; 6] = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];

    if name.contains('\\') {
        return Some("contains a backslash, a path separator on Windows");
    }
    if name.contains(':') {
        return Some("contains a colon, which Windows reads as a drive or a data stream");
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Some("ends with a dot or a space, which Windows drops");
    }

    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    // On characters: Windows reserves the superscript ports too (`COM¹`, `LPT³`), and those
    // digits are two bytes each.
    let is_port = |prefix: &str| {
        let mut chars = stem.chars();
        let head: String = chars.by_ref().take(3).collect();

        head.eq_ignore_ascii_case(prefix)
            && matches!((chars.next(), chars.next()), (Some('1'..='9' | '¹' | '²' | '³'), None))
    };

    if RESERVED.iter().any(|reserved| stem.eq_ignore_ascii_case(reserved)) || is_port("COM") || is_port("LPT") {
        return Some("is a reserved device name on Windows");
    }

    None
}

/// Materialize a chunked file by streaming its recipe's chunks to `fs_path` in order, verifying
/// the assembled content hash as it goes. The file is created/truncated first (bounded memory:
/// one chunk at a time), then the executable bit is set for an `ExecutableChunked` entry.
///
/// # Arguments
/// * `path`        - The warehouse path (for error messages).
/// * `fs_path`     - The filesystem path to write to.
/// * `recipe_hash` - The recipe hash from the tree entry.
/// * `item_type`   - The chunked entry type (`NormalChunked` or `ExecutableChunked`).
///
/// # Returns
/// * `Ok(())`      - If the file was assembled and verified.
/// * `Err(String)` - If a chunk is missing/corrupt, the assembled hash mismatches, or a write fails.
fn write_chunked_file(path: &str,
                      fs_path: &Path,
                      recipe_hash: &str,
                      item_type: DirEntryType,
                      create: bool) -> Result<(), String> {
    // Assembly can fail its integrity check partway; assemble into a temp file and only then
    // rename it into place (durable-before-destructive). The original at `fs_path` — an existing
    // file or symlink — survives untouched until the temp is proven good: a failed materialization
    // (a corrupt/missing chunk) never destroys the working copy or leaves a half-written file. The
    // atomic rename replaces a regular file or a symlink in one step (it does not follow the
    // symlink); replacing a directory is the shift command's deletes-before-writes concern, not
    // this one.
    let (temp_path, file) = create_assembly_temp(path)?;

    let assemble = || -> Result<(), String> {
        let mut writer = std::io::BufWriter::new(file);

        object_utils::assemble_chunked_file(recipe_hash, &mut writer)?;

        std::io::Write::flush(&mut writer)
            .map_err(|e| format!("Error while flushing \"{}\": {}", path, e))?;
        Ok(())
    };

    let placed = assemble().and_then(|()| {
        set_file_mode(&temp_path, path, item_type)?;

        // A new path is claimed exclusively before the rename lands on it: the rename replaces
        // whatever is at the target, so without the claim an alias would be replaced silently.
        // On a failure below, the claimed (empty) placeholder is removed again.
        if create {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(fs_path)
                .map_err(|e| creation_error(path, "creating", e))?;
        }

        match std::fs::rename(&temp_path, fs_path) {
            Ok(()) => Ok(()),
            // A bay may live on another file system than the metadata folder.
            Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
                copy_into_place(&temp_path, fs_path, path, item_type)
            }
            Err(e) => Err(format!("Error while finalizing \"{}\": {}", path, e)),
        }
        .inspect_err(|_| if create { let _ = std::fs::remove_file(fs_path); })
    });

    let _ = std::fs::remove_file(&temp_path);
    placed
}

/// The bay-local folder chunked files are assembled in. It is in the metadata folder, not beside
/// the file: the working directory is the user's namespace, so a temp file there could collide
/// with a tracked name, push a long name past the file system's limit, or be left behind for a
/// later `load` to stack.
const FOLDER_NAME_ASSEMBLY: &str = "assembly";

/// Create a fresh temp file to assemble a chunked file into, in [`FOLDER_NAME_ASSEMBLY`]. Its name
/// is unique to this process and attempt and it is created new, so nothing already there is ever
/// opened (a symlink would be followed). Leftovers of interrupted processes are not cleared here
/// but by [`clear_assembly_leftovers`], when the bay's lock is taken.
fn create_assembly_temp(path: &str) -> Result<(PathBuf, std::fs::File), String> {
    static ATTEMPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    const MAX_ATTEMPTS: usize = 64;

    let dir = crate::globals::bay_root().join(FOLDER_NAME_ASSEMBLY);
    file_utils::create_folder_if_not_exists(&dir)?;

    let pid = std::process::id();
    let candidates = (0..MAX_ATTEMPTS).map(|_| {
        let attempt = ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        dir.join(format!("{}-{}.tmp", pid, attempt))
    });

    create_first_new(path, candidates)
}

/// Remove every entry of the active bay's assembly folder. Called by
/// [`WarehouseLock::acquire`](crate::util::lock_utils::WarehouseLock::acquire) once the bay's
/// lock is held. Every process that assembles into a bay holds that bay's lock (`bay add` and
/// `franchise` take the lock of the one they create), so anything in the folder then was left by
/// an interrupted process and is never read. Best effort: a leftover that cannot be removed only
/// costs space.
pub(crate) fn clear_assembly_leftovers() {
    let dir = crate::globals::bay_root().join(FOLDER_NAME_ASSEMBLY);
    let Ok(entries) = std::fs::read_dir(&dir) else { return };

    for entry in entries.flatten() {
        let _ = std::fs::remove_file(entry.path());
    }
}

/// Place an assembled file across file systems: copy it into a fresh temp file beside `fs_path`
/// and rename that into place, so the swap is still atomic. The beside-the-file name is short
/// and created new, as in [`create_assembly_temp`].
fn copy_into_place(assembled: &Path, fs_path: &Path, path: &str, item_type: DirEntryType) -> Result<(), String> {
    static ATTEMPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let pid = std::process::id();
    let candidates = (0..64).map(|_| {
        let attempt = ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        fs_path.with_file_name(format!(".forklift-place-{}-{}.tmp", pid, attempt))
    });
    let (beside, mut file) = create_first_new(path, candidates)?;

    let placed = (|| -> Result<(), String> {
        let mut source = std::fs::File::open(assembled)
            .map_err(|e| format!("Error while reading the assembly of \"{}\": {}", path, e))?;
        std::io::copy(&mut source, &mut file)
            .map_err(|e| format!("Error while copying \"{}\" into place: {}", path, e))?;
        drop(file);

        set_file_mode(&beside, path, item_type)?;

        std::fs::rename(&beside, fs_path)
            .map_err(|e| format!("Error while finalizing \"{}\": {}", path, e))
    })();

    if placed.is_err() {
        let _ = std::fs::remove_file(&beside);
    }
    placed
}

/// Create the first of `candidates` that does not exist yet (see
/// [`file_utils::create_first_new_file`]), for the file at `path`.
fn create_first_new(path: &str, candidates: impl Iterator<Item = PathBuf>) -> Result<(PathBuf, std::fs::File), String> {
    file_utils::create_first_new_file(candidates)
        .map_err(|e| format!("Error while creating a temp file for \"{}\": {}", path, e))
}

/// Set a materialized file's mode: executable (`0o755`) for an executable (chunked or plain)
/// entry, otherwise `0o644`. A no-op on non-Unix, and on a volume that keeps no executable bit
/// (see [`file_utils::executable_bit_is_observable`]), where a `chmod` may be refused.
fn set_file_mode(fs_path: &Path, path: &str, item_type: DirEntryType) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let metadata = std::fs::symlink_metadata(fs_path)
            .map_err(|e| format!("Error while reading the permissions of \"{}\": {}", path, e))?;
        if !file_utils::executable_bit_is_observable(&metadata)? {
            return Ok(());
        }

        let is_executable = matches!(
            item_type.on_disk_kind(),
            DirEntryType::Executable
        );
        let mode = if is_executable { 0o755 } else { 0o644 };

        std::fs::set_permissions(fs_path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("Error while setting the permissions of \"{}\": {}", path, e))?;
    }

    #[cfg(not(unix))]
    {
        let _ = (fs_path, path, item_type);
    }

    Ok(())
}

/// Try to remove the given directories (deepest first). Directories that still contain
/// anything (e.g. untracked files) are silently kept.
///
/// # Arguments
/// * `removed_dirs` - The directories to remove, deepest first.
pub fn remove_empty_directories(removed_dirs: &[String]) {
    for dir in removed_dirs {
        // A refused path is left alone. A failure means the directory is not empty (untracked
        // content) or already gone; both are fine.
        if check_working_path(dir).is_ok() {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

/// Where the working files a tree's shards are built over came from — what makes each tree
/// entry's hash true of its file, which an entry recorded from a stat presumes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TreeProvenance {
    /// The caller found the working tree clean against its old tree and then wrote every file
    /// the two trees differ in, so the new tree's hashes are true of the files: only their
    /// metadata is gathered, nothing is read or hashed. The anchor must be taken before that
    /// cleanliness check (or before the writes, where nothing was checked): a file edited after
    /// it — during the check, between the writes, or before the rebuild's stat — is then
    /// recorded unproven rather than trusted with a hash it no longer holds.
    Written(file_utils::StatAnchor),

    /// The files were never written or hashed by this command (`import-git` over an existing
    /// checkout): each is re-proved against its tree entry, in that entry's representation. A file
    /// that does not hold its tree entry's content (an uncommitted edit, a missing file) is
    /// recorded with the tree's hash and an unproven stat, so it reads as an unstaged change.
    Verify,
}

/// A tree's inventory shards, and the files among them that did not hold their tree entries.
pub struct TreeInventories {
    /// Warehouse path key → inventory.
    pub shards: BTreeMap<String, Inventory>,

    /// Under [`TreeProvenance::Verify`], the keys of the files that did not hold their tree
    /// entry's content (an edit, a missing file, a file git converted on checkout), each recorded
    /// with the tree's hash and an unproven stat, in tree order. Empty under `Written`, which
    /// presumes every file holds its entry.
    pub unheld: Vec<String>,
}

/// Build the inventory shards for the given tree: one shard per directory, its entries built
/// from the files' metadata, under the given provenance.
///
/// The tree is descended first, loading every in-scope subtree a level at a time; the files are
/// then recorded in parallel (see [`fanout_utils::prefetch_in_order`]), each independent of every other, and
/// gathered into their directories' shards on this thread.
///
/// # Arguments
/// * `root_tree_hash` - The hash of the root tree to build shards for.
/// * `provenance`     - What makes the tree's hashes true of the working files.
///
/// # Returns
/// * `Ok(TreeInventories)` - The shards, and the files that did not hold their entries.
/// * `Err(String)`         - If a tree object could not be loaded, the clock probe stamped, or a
///                           file read or its metadata gathered.
pub fn build_inventories_for_tree(root_tree_hash: &str,
                                  provenance: TreeProvenance) -> Result<TreeInventories, String> {
    let root_tree = object_utils::load_tree(root_tree_hash)?;

    // In a scoped bay only the in-scope subtree(s) are inventoried; the spine is descended but
    // its out-of-scope files and siblings are never loaded or shard-ed. Full scope (a plain bay
    // or the main tree) inventories the whole tree exactly as before.
    let scope = scope_utils::current_scope()?;

    let mut shards: Vec<(String, Inventory)> = Vec::new();
    let mut files: Vec<TreeFile> = Vec::new();

    collect_tree_directories(root_tree, root_tree_hash, &scope, &mut shards, &mut files)?;

    // Written files share the caller's anchor, taken before its cleanliness check. Verified files
    // share one taken after every tree is loaded, before any file's stat.
    let anchor = match provenance {
        TreeProvenance::Written(anchor) => anchor,
        TreeProvenance::Verify => file_utils::take_stat_anchor()?,
    };

    // Each record says whether its file held the tree entry.
    let record = |file: &TreeFile| -> Result<(InventoryItem, bool), String> {
        let path = Path::new(&file.key);

        match provenance {
            TreeProvenance::Written(_) => inventory_utils::build_inventory_item_from_stat(
                path,
                file.name(),
                file.hash.clone(),
                file.item_type,
                anchor,
            ).map(|item| (item, true)),
            TreeProvenance::Verify => verified_tree_entry(
                path,
                file.name(),
                &inventory_utils::build_stale_inventory_item(file.name(), file.hash.clone(), file.item_type),
                anchor,
            ),
        }
    };

    let mut unheld: Vec<String> = Vec::new();

    let gather = |file: &TreeFile, entry: Result<(InventoryItem, bool), String>| -> Result<(), String> {
        let (item, held) = entry?;
        if !held {
            unheld.push(file.key.clone());
        }
        shards[file.shard].1.add_item(item);
        Ok(())
    };

    let threshold = match provenance {
        TreeProvenance::Written(_) => WRITTEN_FANOUT_THRESHOLD,
        TreeProvenance::Verify => VERIFY_FANOUT_THRESHOLD,
    };

    if files.len() < threshold {
        let mut gather = gather;
        files.iter().try_for_each(|file| gather(file, record(file)))?;
    } else {
        // Unbounded window and budget: the records are small and every one is kept, so nothing
        // needs holding back. Prefetch over `fanout_map` for its dynamic claiming: a verified
        // file's cost is its size, and contiguous chunks leave a worker holding the large ones
        // (measured on git.git: 58 ms against 64 ms).
        fanout_utils::prefetch_in_order(&files, usize::MAX, usize::MAX, record, |_| 1, gather)?;
    }

    Ok(TreeInventories { shards: shards.into_iter().collect(), unheld })
}

/// Below this many files, a tree's written files are recorded on the calling thread. Each is a
/// single stat of a few microseconds, so the fan-out's threads only pay for themselves over a
/// few hundred.
pub const WRITTEN_FANOUT_THRESHOLD: usize = 256;

/// Below this many files, a tree's verified files are recorded on the calling thread. Each is
/// read and hashed, so the fan-out pays for itself sooner.
pub const VERIFY_FANOUT_THRESHOLD: usize = 64;

/// One in-scope file of a tree, waiting to be recorded into its directory's shard.
struct TreeFile {
    /// The index of its directory's shard in the collected list.
    shard: usize,
    /// Its warehouse path key (also its working path, relative to the working directory).
    key: String,
    /// The tree entry's hash.
    hash: String,
    /// The tree entry's kind.
    item_type: DirEntryType,
}

impl TreeFile {
    /// Its name within the directory: the key's last component.
    fn name(&self) -> &str {
        self.key.rsplit_once('/').map_or(self.key.as_str(), |(_, name)| name)
    }
}

/// Record one tree file the command did not write: re-proved against its tree entry under an
/// anchor taken before its stat, or else recorded with the tree's hash and an unproven stat.
/// Returns the entry, and whether the file held the tree entry's content.
fn verified_tree_entry(file_path: &Path,
                       name: &str,
                       tree_entry: &InventoryItem,
                       anchor: file_utils::StatAnchor) -> Result<(InventoryItem, bool), String> {
    let Ok(metadata) = file_utils::get_symlink_metadata_for_path(file_path) else {
        return Ok((tree_entry.clone(), false));
    };
    let item_type = file_utils::get_type_of_dir_entry(&metadata)?;

    if !item_type.is_file() {
        return Ok((tree_entry.clone(), false));
    }

    // A match keeps the tree's kind, not the file's: the working tree is the imported tree, so an
    // executable bit the tree does not record (a checkout under `core.fileMode=false`, or one on
    // a file system without the bit) is not this command's change to stage.
    match inventory_utils::reprove_file(tree_entry, item_type, file_path, name, anchor)? {
        Some((item, _)) => Ok((InventoryItem { item_type: tree_entry.item_type, ..item }, true)),
        None => Ok((tree_entry.clone(), false)),
    }
}

/// Collect a tree's directories, one level at a time: an empty shard for each, stamped with the
/// tree hash it is built from where that rollup holds, and its in-scope files to record. Each
/// level's subtrees are loaded in parallel, independent of each other.
///
/// # Arguments
/// * `root_tree`      - The (loaded) root tree.
/// * `root_tree_hash` - The hash of `root_tree` (a loaded tree carries no hash of its own).
/// * `scope`          - The bay's materialization scope.
/// * `shards`         - The collected shards, by directory key.
/// * `files`          - The collected files, each naming its shard's index.
///
/// # Returns
/// * `Ok(())`      - If every in-scope directory was collected.
/// * `Err(String)` - If a subtree could not be loaded.
fn collect_tree_directories(root_tree: TreeItem,
                            root_tree_hash: &str,
                            scope: &MaterializationScope,
                            shards: &mut Vec<(String, Inventory)>,
                            files: &mut Vec<TreeFile>) -> Result<(), String> {
    // Hoisted once (not per entry): a full (unscoped) scope always classifies everything in
    // scope, so on the hot, common full-bay path the classify calls below are pure overhead —
    // short-circuit them away entirely (`diff_directory` does the same).
    let scope_is_full = scope.is_full();

    // Each directory of the level: its key, its tree's hash, and the loaded tree.
    let mut level: Vec<(String, String, TreeItem)> = vec![(String::new(), root_tree_hash.to_string(), root_tree)];

    while !level.is_empty() {
        let mut subtrees: Vec<(String, String)> = Vec::new();

        for (key, tree_hash, tree) in &level {
            let shard = shards.len();

            for (name, item) in tree.get_files() {
                let file_key = join_key(key, name);

                // A spine directory's own files are out of scope and were never materialized, so
                // they are not inventoried — except the root ignore file, which every scope holds
                // in scope. Only in-scope files land in a shard.
                if !scope_is_full && scope.classify(&file_key) != ScopeClass::InScope {
                    continue;
                }

                files.push(TreeFile {
                    shard,
                    key: file_key,
                    hash: item.hash.clone(),
                    item_type: item.item_type,
                });
            }

            let mut inventory = Inventory::new();

            // `tree_hash` is a byte-for-byte-trustworthy rollup for this shard only when its
            // content here is a complete materialization of `tree` — see
            // `tree_utils::rollup_stampable`'s doc comment (shared with `restore --staged`'s own
            // tree-to-shard builder, which must decide this exactly the same way).
            if tree_utils::rollup_stampable(scope, key, tree) {
                inventory.set_rollup_hash(Some(tree_hash.clone()));
            }

            shards.push((key.clone(), inventory));

            for (name, subtree) in tree.get_subtrees() {
                let subtree_key = join_key(key, name);

                // Out-of-scope subtrees are sealed by hash — never loaded, never inventoried.
                if !scope_is_full && scope.classify(&subtree_key) == ScopeClass::OutOfScope {
                    continue;
                }

                subtrees.push((subtree_key, subtree.hash.clone()));
            }
        }

        // One-shot loads, kept out of the parsed-tree cache (see `object_utils::load_tree`).
        let load = |(_, hash): &(String, String)| object_utils::load_tree(hash);

        let loaded: Vec<Result<TreeItem, String>> = if subtrees.len() < TREE_LOAD_FANOUT_THRESHOLD {
            subtrees.iter().map(load).collect()
        } else {
            fanout_utils::fanout_map(&subtrees, load)
        };

        level = subtrees.into_iter()
            .zip(loaded)
            .map(|((key, hash), tree)| tree.map(|tree| (key, hash, tree)))
            .collect::<Result<_, _>>()?;
    }

    Ok(())
}

/// Below this many subtrees in a level, they are loaded on the calling thread: the fan-out's
/// threads cost more than they save on a handful of tree loads.
pub const TREE_LOAD_FANOUT_THRESHOLD: usize = 16;

/// Join a directory key and an entry name into the entry's warehouse path.
fn join_key(key: &str, name: &str) -> String {
    if key.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", key, name)
    }
}

/// Materialize a target tree over the working directory and rebuild the whole staging
/// area from it — the shared engine of every head-moving operation that arrives from a
/// remote (`lower`, `franchise`). Refuses up front (before anything is touched) when a
/// new file would overwrite untracked content or land on another entry through a case alias
/// (see [`collect_collisions`]).
///
/// # Arguments
/// * `from_tree`  - The tree the working directory currently matches (`None` for an
///                  empty/unborn state).
/// * `to_tree`    - The tree to materialize.
/// * `operation`  - What is running (for the collision error message).
/// * `anchor`     - Taken before the caller's cleanliness check against `from_tree` (or before
///                  this call, where nothing was checked) — see [`TreeProvenance::Written`].
/// * `progress`   - Where to report the file writes and the inventory rebuild.
///
/// # Returns
/// * `Ok(())`      - If the working directory and the inventory now match `to_tree`.
/// * `Err(String)` - On untracked collisions, or a failed write.
pub fn materialize_tree(from_tree: Option<&str>,
                        to_tree: &str,
                        operation: &str,
                        anchor: file_utils::StatAnchor,
                        progress: &Progress) -> Result<(), String> {
    let (ops, removed_dirs) = diff_trees(from_tree, to_tree)?;

    alias_utils::refuse_collisions(operation, &collect_collisions(&ops, &removed_dirs)?)?;

    progress.status("Writing files");

    // In `apply_ops`'s order — every removal, then the emptied directories, then every write —
    // never the diff's own (writes first, per directory): across a case-only rename the write
    // would land on the old name through the alias and the removal after it would delete it.
    let mut done = 0;

    apply_ops_each(&ops, &removed_dirs, |op| {
        done += 1;
        progress.fraction(done, ops.len(), || op_path(op).to_string());
        Ok(())
    })?;

    progress.status("Rebuilding the inventory");

    let shards = build_inventories_for_tree(to_tree, TreeProvenance::Written(anchor))?.shards;

    crate::util::inventory_utils::replace_all_inventories(&shards)?;

    Ok(())
}

/// Move `pallet` from `expected` (the bay's base) to `new` after `written_tree` (the tree of `new`)
/// was already materialized, recording `new` as the bay's base once it stands, and putting the working tree and inventory back at `start_tree` (the tree of
/// `expected`, or the empty tree for an unborn pallet) if the pallet did not end up at `new` (see
/// [`settle_head_write`], which decides that).
pub fn write_head_or_roll_back(pallet: &str,
                               expected: Option<&str>,
                               new: &str,
                               written_tree: &str,
                               start_tree: Option<&str>,
                               anchor: file_utils::StatAnchor) -> Result<(), String> {
    let Err(error) = crate::util::pallet_utils::update_pallet_head(pallet, expected, new) else {
        return crate::util::pallet_utils::record_landed(pallet, new);
    };

    match settle_head_write(pallet, new, error, written_tree, start_tree, anchor)? {
        HeadWrite::Stands => Ok(()),
        HeadWrite::Kept(error) | HeadWrite::RolledBack(error) => Err(error),
    }
}

/// Where a head write that did not go through cleanly left the bay (see [`settle_head_write`]).
/// Every variant leaves the working tree and inventory matching the pallet's head.
#[derive(Debug, PartialEq)]
pub enum HeadWrite {
    /// Another bay already moved the pallet to the same head: the move stands, nothing failed.
    Stands,

    /// The write failed after landing: the pallet is at the new head, though not known durable,
    /// and the tree (which matches it) is kept. The error to report.
    Kept(String),

    /// The pallet did not move: the working tree and inventory are back at the start tree. The
    /// error to report.
    RolledBack(String),
}

/// Settle a head write of `pallet` to `new` that returned `error`, after `written_tree` (the tree
/// of `new`) was already materialized. A failed write can still have landed, so the outcome is
/// decided by reading the pallet back, never inferred from the error: at `new`, the tree is kept
/// and the bay's position records `new` as its base; anywhere else, the working tree and inventory are put back at `start_tree` (the empty tree for
/// `None`). Runs under the bay lock, so nothing else touched the bay since `written_tree` was
/// materialized.
///
/// # Returns
/// * `Ok(HeadWrite)` - The bay matches the pallet's head again; see the variant for what happened.
/// * `Err(String)`   - The pallet could not be read back (the tree is left alone rather than
///                     guessed at), or the rollback failed; the error says the working tree may
///                     not match the pallet's head.
pub fn settle_head_write(pallet: &str,
                         new: &str,
                         error: crate::util::pallet_utils::RefUpdateError,
                         written_tree: &str,
                         start_tree: Option<&str>,
                         anchor: file_utils::StatAnchor) -> Result<HeadWrite, String> {
    use crate::util::pallet_utils::{self, RefUpdateError};

    // Wherever the tree is kept at `new`, the bay now stands on it.
    let stands_on_new = || pallet_utils::record_landed(pallet, new);

    let error = match error {
        RefUpdateError::Moved(moved) if moved.found.as_deref() == Some(new) => {
            stands_on_new()?;
            return Ok(HeadWrite::Stands);
        }
        error => error.to_string(),
    };

    match pallet_utils::get_pallet_head(pallet) {
        Ok(head) if head.as_deref() == Some(new) => {
            stands_on_new().map_err(|e| format!("{} {}", error, e))?;
            return Ok(HeadWrite::Kept(error));
        }
        Ok(_) => {}
        Err(e) => return Err(format!(
            "{} The pallet could not be read back ({}), so the working tree is left at tree {}; it \
            may not match the pallet's head — shift or restore to bring it back in line.",
            error, e, written_tree
        )),
    }

    let start = match start_tree {
        Some(tree) => Ok(tree.to_string()),
        None => crate::util::object_utils::empty_tree_hash(),
    };

    let rolled_back = start.and_then(|start| {
        materialize_tree(Some(written_tree), &start, "Rolling back", anchor, &Progress::NONE)
    });

    match rolled_back {
        Ok(()) => Ok(HeadWrite::RolledBack(error)),
        Err(err) => Err(format!(
            "{} The working tree could not be put back ({}): it holds tree {}, not the pallet's \
            head; shift or restore to bring it back in line.",
            error, err, written_tree
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_head_write_another_bay_already_made_stands_and_touches_no_tree() {
        let root = std::env::temp_dir().join(format!("forklift-shift-same-move-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let scope = crate::globals::StorageRootScope::enter(&root);

        let (start, new) = ("a".repeat(64), "b".repeat(64));
        crate::util::pallet_utils::update_pallet_head("main", None, &start).unwrap();

        // Another bay makes the very same move first.
        crate::util::pallet_utils::update_pallet_head("main", Some(&start), &new).unwrap();

        // The trees name no stored objects: a rollback would fail loudly, so Ok proves none ran.
        write_head_or_roll_back("main", Some(&start), &new, &"c".repeat(64), Some(&"d".repeat(64)),
                                file_utils::StatAnchor::Unproven)
            .expect("a move another bay already made must stand");
        assert_eq!(crate::util::pallet_utils::get_pallet_head("main").unwrap(), Some(new));

        drop(scope);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn windows_refuses_names_that_are_not_one_plain_file_name() {
        for name in ["a\\b", "C:", "file.txt:stream", "trailing.", "trailing ", "..  ", "CON", "con.txt",
            "Nul", "aux.tar.gz", "COM1", "lpt9.log", "PRN ", "COM\u{B9}", "lpt\u{B2}.txt", "Com\u{B3}",
            "CONIN$", "conout$.txt"] {
            assert!(windows_component_problem(name).is_some(), "{:?} must be refused", name);
        }
    }

    #[test]
    fn windows_accepts_names_that_only_resemble_reserved_ones() {
        for name in ["CONSOLE", "con_", "COM0", "COM10", "LPT", "auxiliary.txt", ".con", "a.CON", "café",
            "...a", "file.txt", "abé.txt", "a€", "€", "COM\u{2074}", "LPT\u{B9}0", "CO\u{B9}", "COM\u{2070}",
            "CONIN", "conout$x", "CONERR$"] {
            assert_eq!(windows_component_problem(name), None, "{:?} must be accepted", name);
        }
    }

    #[test]
    fn a_path_inside_the_metadata_folder_is_refused_in_any_case() {
        for path in [".forklift/config", ".ForkLift/objects/x", ".forklift./x", ".FORKLIFT /x", ".forklift",
            ".for\u{212A}lift/config"] {
            let error = check_working_path(path).expect_err(path);
            assert!(error.contains(&format!("{:?}", path)), "{}", error);
        }
    }

    #[test]
    fn a_metadata_name_below_the_top_level_is_refused_before_and_during_a_write() {
        // These name paths that do not exist here, so no ancestor can be a symlink or another
        // working tree: only the name decides.
        for path in ["docs/.forklift/x", "no-such-dir/.ForkLift", "a/b/FORKLI~1"] {
            for check in [|path| check_planned_paths([path]), check_working_path] {
                let refused = check(path).expect_err(path);
                assert!(refused.contains("plant another working tree"), "{:?}: {}", path, refused);
            }
        }
        for path in ["x.forklift", "docs/forklift/x", "docs/.forklifts"] {
            assert_eq!(check_planned_paths([path]), Ok(()), "{:?} must be accepted", path);
            assert_eq!(check_working_path(path), Ok(()), "{:?} must be accepted", path);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_temp_candidate_that_exists_is_skipped_not_opened() {
        let dir = std::env::temp_dir().join(format!("forklift-assembly-temp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let outside = dir.join("outside.txt");
        std::fs::write(&outside, "untouched\n").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("taken")).unwrap();

        let (temp_path, file) = create_first_new("big.bin", [dir.join("taken"), dir.join("fresh")].into_iter()).unwrap();
        std::io::Write::write_all(&mut &file, b"assembled").unwrap();

        assert_eq!(temp_path, dir.join("fresh"));
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "untouched\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn copying_into_place_replaces_a_symlink_and_leaves_no_temp() {
        // The cross-device fallback: the rename from the metadata folder failed, so the assembly
        // is copied beside the file and renamed over it, replacing (not following) a symlink.
        let dir = std::env::temp_dir().join(format!("forklift-copy-into-place-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("work")).unwrap();
        std::fs::create_dir_all(dir.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let _scope = crate::globals::StorageRootScope::enter(&dir);
        let outside = dir.join("outside.txt");
        std::fs::write(&outside, "untouched\n").unwrap();
        let assembled = dir.join("assembled.tmp");
        std::fs::write(&assembled, "assembled\n").unwrap();
        let target = dir.join("work").join("big.bin");
        std::os::unix::fs::symlink(&outside, &target).unwrap();

        copy_into_place(&assembled, &target, "big.bin", DirEntryType::NormalChunked).unwrap();

        assert!(!std::fs::symlink_metadata(&target).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "assembled\n");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "untouched\n");
        assert_eq!(std::fs::read_dir(dir.join("work")).unwrap().count(), 1, "a temp file was left beside it");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
