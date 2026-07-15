use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use crate::enums::dir_entry_type::DirEntryType;
use crate::model::inventory::Inventory;
use crate::model::tree_item::TreeItem;
use crate::util::scope_utils::{self, MaterializationScope, ScopeClass};
use crate::util::{file_utils, inventory_utils, object_utils, tree_utils};

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
        /// not overwrite anything — an existing (untracked) file at the path is a conflict.
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

    let mut ops: Vec<FileOp> = Vec::new();
    let mut removed_dirs: Vec<String> = Vec::new();

    // In a scoped (sparse) bay this restricts materialization to the in-scope subtree(s): the
    // walk copies nothing out of scope, so no out-of-scope blob is ever written and no absent
    // object is touched. The scope is full (no restriction) in a plain bay or the main tree, so
    // this is a no-op there and the behavior is byte-for-byte what it always was.
    let scope = scope_utils::current_scope()?;

    diff_directory(from_tree.as_ref(), Some(&to_tree), "", &mut ops, &mut removed_dirs, &scope)?;

    // Deepest directories first, so empty-directory cleanup can proceed bottom-up.
    removed_dirs.sort_by_key(|dir| std::cmp::Reverse(dir.matches('/').count()));

    Ok((ops, removed_dirs))
}

/// Diff one directory level of two trees (recursively).
///
/// # Arguments
/// * `from`         - This directory in the source tree (if it exists there).
/// * `to`           - This directory in the target tree (if it exists there).
/// * `key`          - The warehouse path key of the directory.
/// * `ops`          - The collected file operations.
/// * `removed_dirs` - The collected source-only directories.
///
/// # Returns
/// * `Ok(())`      - If the directory was diffed.
/// * `Err(String)` - If a subtree object could not be loaded.
fn diff_directory(from: Option<&TreeItem>,
                  to: Option<&TreeItem>,
                  key: &str,
                  ops: &mut Vec<FileOp>,
                  removed_dirs: &mut Vec<String>,
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

    for (name, to_item) in &to_files {
        let child_key = join_key(key, name);

        if !scope_is_full {
            // A file where the scope expects a directory (a spine ancestor, or the in-scope
            // prefix itself) is the §3.1 type change — refuse rather than guess. Out-of-scope
            // files are sealed and never materialized; only genuinely in-scope files are written.
            if scope.requires_directory(&child_key) {
                return Err(scope_utils::type_changed_refusal(&child_key).into());
            }
            if scope.classify(&child_key) == ScopeClass::OutOfScope {
                continue;
            }
        }

        let from_item = from_files.get(*name);

        let is_unchanged = from_item
            .map(|item| item.hash == to_item.hash && item.item_type == to_item.item_type)
            .unwrap_or(false);

        if !is_unchanged {
            ops.push(FileOp::Write {
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
            ops.push(FileOp::Remove { path: child_key });
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
            ops,
            removed_dirs,
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

            diff_directory(Some(&from_loaded), None, &subtree_key, ops, removed_dirs, scope)?;
            removed_dirs.push(subtree_key);
        }
    }

    Ok(())
}

/// Find the new-file writes in `ops` that would overwrite something already on disk: used
/// up front by `shift` and consolidate's fast-forward path, before anything is touched.
///
/// A path that exists on disk is not a collision when it is a tracked, fully clean
/// directory (every entry beneath it tracked) — that is a tracked directory the diff is
/// legitimately replacing with a file (a dir→file flip). Applying the diff must then delete
/// the directory's contents before writing the file (see `apply_ops`); a directory with any
/// untracked content beneath it is a genuine collision and still refuses.
///
/// # Arguments
/// * `ops` - The file operations a diff produced.
///
/// # Returns
/// * `Ok(Vec<String>)` - The colliding paths (empty when there are none).
/// * `Err(String)`      - If a directory or a shard could not be read or parsed.
pub fn collect_untracked_collisions(ops: &[FileOp]) -> Result<Vec<String>, String> {
    let mut collisions = Vec::new();

    for op in ops {
        let FileOp::Write { path, is_new: true, .. } = op else { continue };

        let fs_path = Path::new(path);

        if !fs_path.exists() {
            continue;
        }

        if fs_path.is_dir() && inventory_utils::directory_is_safe_to_replace(path)? {
            continue;
        }

        collisions.push(path.clone());
    }

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
    for op in ops {
        if matches!(op, FileOp::Remove { .. }) {
            apply_file_op(op)?;
        }
    }

    remove_empty_directories(removed_dirs);

    for op in ops {
        if matches!(op, FileOp::Write { .. }) {
            apply_file_op(op)?;
        }
    }

    Ok(())
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
        FileOp::Write { path, hash, item_type, .. } => write_tracked_file(path, hash, *item_type),
        FileOp::Remove { path } => {
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
/// # Arguments
/// * `path`      - The warehouse path of the file.
/// * `hash`      - The blob hash of the content.
/// * `item_type` - The type of the entry.
///
/// # Returns
/// * `Ok(())`      - If the file was written.
/// * `Err(String)` - If the blob could not be loaded or a file system operation failed.
pub fn write_tracked_file(path: &str, hash: &str, item_type: DirEntryType) -> Result<(), String> {
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
        return write_chunked_file(path, fs_path, hash, item_type);
    }

    let blob = object_utils::load_blob(hash)?;

    if item_type == DirEntryType::SymbolicLink {
        let target = String::from_utf8(blob.content)
            .map_err(|_| format!("The symlink target stored for \"{}\" is not valid UTF-8.", path))?;

        // A symlink cannot be overwritten in place; remove whatever is at the path first.
        match std::fs::remove_file(fs_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("Error while replacing \"{}\": {}", path, e)),
        }

        #[cfg(unix)]
        return std::os::unix::fs::symlink(&target, fs_path)
            .map_err(|e| format!("Error while creating symlink \"{}\": {}", path, e));

        #[cfg(windows)]
        return Err(format!(
            "Cannot materialize symlink \"{}\" (pointing at \"{}\") on Windows.",
            path, target
        ));
    }

    std::fs::write(fs_path, &blob.content)
        .map_err(|e| format!("Error while writing \"{}\": {}", path, e))?;

    set_file_mode(fs_path, path, item_type)?;

    Ok(())
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
                      item_type: DirEntryType) -> Result<(), String> {
    // Assembly can fail its integrity check partway; assemble into a temp file and only then
    // rename it into place (durable-before-destructive). The original at `fs_path` — an existing
    // file or symlink — survives untouched until the temp is proven good: a failed materialization
    // (a corrupt/missing chunk) never destroys the working copy or leaves a half-written file. The
    // atomic rename replaces a regular file or a symlink in one step (it does not follow the
    // symlink); replacing a directory is the shift command's deletes-before-writes concern, not
    // this one.
    let temp_path = fs_path.with_file_name(format!(
        ".{}.forklift-assemble.tmp",
        fs_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    ));

    let assemble = || -> Result<(), String> {
        let file = std::fs::File::create(&temp_path)
            .map_err(|e| format!("Error while creating \"{}\": {}", path, e))?;
        let mut writer = std::io::BufWriter::new(file);

        object_utils::assemble_chunked_file(recipe_hash, &mut writer)?;

        std::io::Write::flush(&mut writer)
            .map_err(|e| format!("Error while flushing \"{}\": {}", path, e))?;
        Ok(())
    };

    if let Err(e) = assemble() {
        let _ = std::fs::remove_file(&temp_path);
        return Err(e);
    }

    set_file_mode(&temp_path, path, item_type)?;

    std::fs::rename(&temp_path, fs_path)
        .map_err(|e| format!("Error while finalizing \"{}\": {}", path, e))?;

    Ok(())
}

/// Set a materialized file's mode: executable (`0o755`) for an executable (chunked or plain)
/// entry, otherwise `0o644`. A no-op on non-Unix.
fn set_file_mode(fs_path: &Path, path: &str, item_type: DirEntryType) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

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
        // A failure means the directory is not empty (untracked content) or already gone;
        // both are fine.
        let _ = std::fs::remove_dir(dir);
    }
}

/// Build the inventory shards for the given tree: one shard per directory, entries built
/// from the (just materialized) files' metadata — nothing is read or hashed.
///
/// # Arguments
/// * `root_tree_hash` - The hash of the root tree to build shards for.
///
/// # Returns
/// * `Ok(BTreeMap<String, Inventory>)` - Warehouse path key → inventory.
/// * `Err(String)`                     - If a tree object could not be loaded or a file's
///                                       metadata could not be gathered.
pub fn build_inventories_for_tree(root_tree_hash: &str) -> Result<BTreeMap<String, Inventory>, String> {
    let mut shards: BTreeMap<String, Inventory> = BTreeMap::new();
    let root_tree = object_utils::load_tree(root_tree_hash)?;

    // In a scoped bay only the in-scope subtree(s) are inventoried; the spine is descended but
    // its out-of-scope files and siblings are never loaded or shard-ed. Full scope (a plain bay
    // or the main tree) inventories the whole tree exactly as before.
    let scope = scope_utils::current_scope()?;

    build_inventory_for_tree_directory(&root_tree, "", root_tree_hash, &mut shards, &scope)?;

    Ok(shards)
}

/// Build the inventory shard for one directory of a tree (recursively), stamping each shard's
/// rollup with the tree hash it was built from.
///
/// # Arguments
/// * `tree`      - The (loaded) tree of the directory.
/// * `key`       - The warehouse path key of the directory.
/// * `tree_hash` - The hash of `tree` itself (its parent's entry for it — `tree` as loaded
///                 carries no hash of its own; the root passes `root_tree_hash` directly).
/// * `shards`    - The collected shards.
///
/// # Returns
/// * `Ok(())`      - If the shard was built.
/// * `Err(String)` - If a subtree could not be loaded or a file's metadata gathered.
fn build_inventory_for_tree_directory(tree: &TreeItem,
                                      key: &str,
                                      tree_hash: &str,
                                      shards: &mut BTreeMap<String, Inventory>,
                                      scope: &MaterializationScope) -> Result<(), String> {
    // Hoisted once per directory (not per entry): a full (unscoped) scope always classifies
    // everything in scope, so on the hot, common full-bay path the classify calls below are
    // pure overhead — short-circuit them away entirely (`diff_directory` does the same).
    let scope_is_full = scope.is_full();

    let mut inventory = Inventory::new();

    for (name, item) in tree.get_files() {
        let file_key = join_key(key, name);

        // A spine directory's own files are out of scope and were never materialized, so they
        // are not inventoried; only genuinely in-scope files land in a shard.
        if !scope_is_full && scope.classify(&file_key) != ScopeClass::InScope {
            continue;
        }

        let file_path = PathBuf::from(file_key);

        inventory.add_item(inventory_utils::build_inventory_item_from_stat(
            &file_path,
            name,
            item.hash.clone(),
            item.item_type,
        )?);
    }

    // `tree_hash` is a byte-for-byte-trustworthy rollup for this shard only when its content
    // here is a complete materialization of `tree` — see `tree_utils::rollup_stampable`'s doc
    // comment (shared with `restore --staged`'s own tree-to-shard builder, which must decide
    // this exactly the same way).
    if tree_utils::rollup_stampable(scope, key, tree) {
        inventory.set_rollup_hash(Some(tree_hash.to_string()));
    }

    shards.insert(key.to_string(), inventory);

    for (name, subtree) in tree.get_subtrees() {
        let subtree_key = join_key(key, name);

        // Out-of-scope subtrees are sealed by hash — never loaded, never inventoried.
        if !scope_is_full && scope.classify(&subtree_key) == ScopeClass::OutOfScope {
            continue;
        }

        let subtree_loaded = object_utils::load_tree(&subtree.hash)?;
        build_inventory_for_tree_directory(&subtree_loaded, &subtree_key, &subtree.hash, shards, scope)?;
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

/// Materialize a target tree over the working directory and rebuild the whole staging
/// area from it — the shared engine of every head-moving operation that arrives from a
/// remote (`lower`, `franchise`). Refuses up front (before anything is touched) when a
/// new file would overwrite untracked content.
///
/// # Arguments
/// * `from_tree`  - The tree the working directory currently matches (`None` for an
///                  empty/unborn state).
/// * `to_tree`    - The tree to materialize.
/// * `operation`  - What is running (for the collision error message).
///
/// # Returns
/// * `Ok(())`      - If the working directory and the inventory now match `to_tree`.
/// * `Err(String)` - On untracked collisions, or a failed write.
pub fn materialize_tree(from_tree: Option<&str>,
                        to_tree: &str,
                        operation: &str) -> Result<(), String> {
    let (ops, removed_dirs) = diff_trees(from_tree, to_tree)?;

    let conflicts: Vec<&String> = ops.iter()
        .filter_map(|op| match op {
            FileOp::Write { path, is_new: true, .. }
                if std::path::Path::new(path).exists() => Some(path),
            _ => None,
        })
        .collect();

    if !conflicts.is_empty() {
        return Err(format!(
            "{} would overwrite these untracked files:\n  {}\n\
            Move them out of the way (or load and stack them) first.",
            operation,
            conflicts.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n  ")
        ));
    }

    for op in &ops {
        apply_file_op(op)?;
    }

    remove_empty_directories(&removed_dirs);

    let shards = build_inventories_for_tree(to_tree)?;

    crate::util::inventory_utils::replace_all_inventories(&shards)?;

    Ok(())
}
