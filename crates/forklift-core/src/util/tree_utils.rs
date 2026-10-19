use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::sync::Arc;
use crate::builder::object::loose_object_builder::LooseObjectBuilder;
use crate::enums::dir_entry_type::DirEntryType;
use crate::enums::inventory_item_state::InventoryItemState;
use crate::model::task::tree_builder::tree_builder_context::TreeBuilderContext;
use crate::model::task::TaskExecutor;
use crate::model::tree_item::TreeItem;
use crate::traits::task_context::TaskContext;
use crate::types::task::Task;
use crate::util::scope_utils::{self, MaterializationScope, ScopeClass};
use crate::util::{file_utils, inventory_utils, object_utils};

/// Build (and store) tree objects from an already-parsed [`inventory_utils::PreparedInventory`]
/// snapshot, bottom-up: one tree object per inventoried directory, staged into `batch` instead of
/// written (and fsynced) immediately.
///
/// * Entries staged for removal (`Deleted`) are excluded — that is how a staged removal
///   becomes an actual removal in the next parcel.
/// * Directories that end up empty (no files, no non-empty subdirectories) are pruned,
///   except the warehouse root.
/// * Ancestor directories that have no shard of their own (e.g. only `src/a` was ever
///   loaded) are synthesized so the chain root → `src` → `a` exists in the tree.
///
/// Used by `stack` (`stack_utils::stack_parcel`) and `park` push (`park::park_changes`,
/// DESIGN.html §5.0 D item 10, finding #3). `stack`'s `prepared` snapshot is the single
/// read+parse pass shared with the conflict check and the post-stack cleanup (§ perf); `park`
/// reads its own fresh snapshot instead (it has no other step to share it with), taken after its
/// working-directory refresh so it reflects the just-rehashed state. Either way, `batch` turns
/// the per-object fsync pairs this build would otherwise pay (one per built directory) into one
/// barrier the caller runs after this returns (see [`file_utils::WriteBatch`]) — for `park`, the
/// same batch its own parcel object joins too, so the whole push is one barrier.
///
/// Also applies the rollup-based skip (DESIGN.html §5.0 D item 8, stage 2) `prepared` was read
/// with (`prepared.skip`): a subtree whose shard's rollup already matches the corresponding head
/// subtree hash is never read, hashed or (re)stored — its parent's tree includes it verbatim by
/// the hash the rollup and the head agree on. See [`inventory_utils::prepare_stack_inventory`]
/// for exactly which keys are eligible.
///
/// The build runs in parallel over the `TaskExecutor` (one task per directory), scheduled
/// bottom-up by dependency: the leaves are enqueued first, and each completing directory
/// enqueues its parent once the parent's last child is built.
///
/// # Arguments
/// * `prepared`            - The already-parsed shard snapshot.
/// * `batch`                - Where every built tree object's write is staged.
/// * `track_tree_hashes`    - Whether the caller will actually consult the returned tree-hash
///                            map — see [`TreeBuilderContext::track_tree_hashes`]. `stack` passes
///                            `true` (it stamps shards' rollups from the map afterward); `park`
///                            passes `false` (DESIGN.html §5.0 D item 10, finding #8) — it
///                            overwrites every shard from head right after this returns
///                            (`inventory_utils::replace_all_inventories`), so the map would be
///                            discarded unread, and every one of the build's per-directory tasks
///                            would otherwise pay a needless `Mutex` acquisition to populate it.
///
/// # Returns
/// * `Ok((Some(TreeItem), BTreeMap<String, String>))` - The root tree, and every freshly built
///   directory's subtree hash by warehouse path key (a non-empty subtree only — see
///   [`build_tree_for_inventory_key`]; empty when `track_tree_hashes` is `false`).
/// * `Ok((None, _))`                                  - If there is no inventory at all.
/// * `Err(String)`                                    - If an object could not be stored.
pub async fn build_tree_from_inventory_deferred(
    prepared: &Arc<inventory_utils::PreparedInventory>,
    batch: &Arc<file_utils::WriteBatch>,
    track_tree_hashes: bool,
) -> Result<(Option<TreeItem>, BTreeMap<String, String>), String> {
    let Some(metadata) = &prepared.metadata else {
        return Ok((None, BTreeMap::new()));
    };

    if metadata.is_empty() {
        return Ok((None, BTreeMap::new()));
    }

    // Collect every inventoried directory key plus all of its ancestors (ancestors may
    // have no shard of their own), then derive the parent → children relation.
    let mut keys: BTreeSet<String> = BTreeSet::new();

    for entry in metadata {
        let mut key = inventory_utils::metadata_entry_to_key(entry);

        loop {
            keys.insert(key.to_string());

            match key.rsplit_once(file_utils::PATH_SEPARATOR_CHAR) {
                Some((parent, _)) => key = parent,
                None => break,
            }
        }

        keys.insert(String::new());
    }

    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for key in &keys {
        if key.is_empty() {
            continue;
        }

        let parent = key.rsplit_once(file_utils::PATH_SEPARATOR_CHAR)
            .map(|(parent, _)| parent)
            .unwrap_or("");

        children.entry(parent.to_string()).or_default().push(key.clone());
    }

    let skip_plan = &prepared.skip;

    // The entire staged tree is provably identical to head (the root's own rollup already
    // named it): `prepare_stack_inventory` read only the root shard, and nothing is hashed or
    // (re)stored.
    if let Some(whole_tree_hash) = &skip_plan.whole_tree_hash {
        return Ok((Some(TreeItem::new(String::new(), whole_tree_hash.clone(), DirEntryType::Tree)), BTreeMap::new()));
    }

    // Every pruned key (a skipped subtree's root, and everything beneath it) is excluded from
    // the task graph entirely — as if it were never inventoried. Its parent instead gets it
    // injected verbatim (by hash) inside `build_tree_for_inventory_key`, from the plan's
    // `injections`. Only worth rebuilding `keys`/`children` when the plan pruned something.
    if !skip_plan.pruned.is_empty() {
        keys = keys.difference(&skip_plan.pruned).cloned().collect();
        children = children.into_iter()
            .filter(|(key, _)| !skip_plan.pruned.contains(key))
            .map(|(key, child_keys)| {
                let child_keys = child_keys.into_iter().filter(|c| !skip_plan.pruned.contains(c)).collect();
                (key, child_keys)
            })
            .collect();
    }

    let children = Arc::new(children);

    // Dependency counters: a directory becomes buildable once all of its (unpruned) children are
    // built. Directories without children (the leaves) are buildable immediately.
    let pending_children: HashMap<String, usize> = keys.iter()
        .map(|key| (key.clone(), children.get(key).map(|c| c.len()).unwrap_or(0)))
        .collect();

    let mut leaves: Vec<String> = keys.iter()
        .filter(|key| children.get(*key).map(|c| c.is_empty()).unwrap_or(true))
        .cloned()
        .collect();

    let context = Arc::new(TreeBuilderContext::new(
        pending_children, Arc::clone(prepared), Arc::clone(batch), track_tree_hashes,
    ));
    let executor = TaskExecutor::new(Arc::clone(&context));

    let first_leaf = leaves.pop()
        .ok_or("The tree build found no leaf directory to start from.".to_string())?;

    for leaf in leaves {
        context.send_task(Box::pin(build_tree_for_inventory_key(
            Arc::clone(&context),
            leaf,
            Arc::clone(&children),
        )))?;
    }

    let root_task: Task<(), String> = Box::pin(build_tree_for_inventory_key(
        Arc::clone(&context),
        first_leaf,
        Arc::clone(&children),
    ));

    executor.execute(root_task).await.map_err(|e|
        e.unwrap_or("An unknown error occurred while building the trees.".to_string())
    )?;

    let root = context.built.lock().await.remove("")
        .ok_or("The tree build finished without producing a root tree.".to_string())?;

    let tree_hashes: BTreeMap<String, String> = context.tree_hashes.lock().await.iter()
        .map(|(key, hash)| (key.clone(), hash.clone()))
        .collect();

    Ok((Some(root), tree_hashes))
}

/// The per-directory task of the tree build: build (and store) the tree object for one
/// inventoried directory, taking the already-built child trees from the shared context,
/// and enqueue the parent's task when this was the parent's last unbuilt child.
///
/// Empty subtrees are pruned (`add_child` is skipped by the parent), but every built
/// tree object is stored, and the root is always kept, even when empty.
///
/// # Arguments
/// * `context`  - The shared build context.
/// * `key`      - The warehouse path key of the directory.
/// * `children` - The parent key → child keys relation over all inventoried directories.
///
/// # Returns
/// * `Ok(())`      - If the directory's tree was built and stored.
/// * `Err(String)` - If a shard could not be read or an object could not be stored.
fn build_tree_for_inventory_key(context: Arc<TreeBuilderContext>,
                                key: String,
                                children: Arc<BTreeMap<String, Vec<String>>>)
                                -> impl Future<Output = Result<(), String>> + Send {
    async move {
        let name = key.rsplit_once(file_utils::PATH_SEPARATOR_CHAR)
            .map(|(_, name)| name)
            .unwrap_or(&key);

        let mut tree = TreeItem::new(name.to_string(), String::new(), DirEntryType::Tree);

        if let Some(inventory) = context.prepared.shards.get(&key) {
            for (_, item) in inventory.get_items() {
                if item.state == InventoryItemState::Deleted {
                    continue;
                }

                tree.add_child(TreeItem::new(item.name.clone(), item.hash.clone(), item.item_type));
            }
        }

        if let Some(child_keys) = children.get(&key) {
            let mut built = context.built.lock().await;

            for child_key in child_keys {
                let child_tree = built.remove(child_key)
                    .ok_or(format!("Subtree \"{}\" was not built before its parent.", child_key))?;

                if !is_tree_empty(&child_tree) {
                    tree.add_child(child_tree);
                }
            }
        }

        // Skip-injected children (DESIGN.html §5.0 D item 8, stage 2): subtrees whose rollup
        // already matched the head, added verbatim by hash — no load, no task, and unlike the
        // `built`-sourced children above, never empty (a matching rollup is only ever recorded
        // for a non-empty subtree — see `build_tree_for_inventory_key`'s own rollup stamping —
        // so no emptiness check applies here).
        if let Some(injected) = context.prepared.skip.injections.get(&key) {
            for (child_name, head_hash) in injected {
                tree.add_child(TreeItem::new(child_name.clone(), head_hash.clone(), DirEntryType::Tree));
            }
        }

        let mut object = LooseObjectBuilder::build_tree(&tree);
        tree.hash = object.hash.clone();

        object.store_deferred(&context.batch)?;

        // Only a non-empty subtree's hash is a meaningful rollup: an empty directory is pruned
        // by its parent (below), so "the tree stack would build for this subtree" is ill-defined
        // for it — omitted here rather than recorded and later filtered out. Skipped entirely
        // (not even the emptiness check) when this build's caller has no use for the map at all
        // — see `TreeBuilderContext::track_tree_hashes`.
        if context.track_tree_hashes && !is_tree_empty(&tree) {
            context.tree_hashes.lock().await.insert(key.clone(), tree.hash.clone());
        }

        context.built.lock().await.insert(key.clone(), tree);

        // The parent becomes buildable once its last child is built.
        if !key.is_empty() {
            let parent = key.rsplit_once(file_utils::PATH_SEPARATOR_CHAR)
                .map(|(parent, _)| parent)
                .unwrap_or("")
                .to_string();

            let is_parent_ready = {
                let mut pending = context.pending_children.lock().await;
                let counter = pending.get_mut(&parent)
                    .ok_or(format!("No pending-children counter for directory \"{}\".", parent))?;

                *counter -= 1;
                *counter == 0
            };

            if is_parent_ready {
                context.send_task(Box::pin(build_tree_for_inventory_key(
                    Arc::clone(&context),
                    parent,
                    Arc::clone(&children),
                )))?;
            }
        }

        Ok(())
    }
}

/// Splice freshly built in-scope subtree(s) over the current head's spine to produce the
/// root tree of a *scoped* (sparse) stack (design §3.2).
///
/// A scoped bay materializes only its in-scope subtree(s); a plain
/// [`build_tree_from_inventory_deferred`] over that dock would emit a root that has *only* the
/// in-scope path, with every out-of-scope sibling silently gone. This overlay instead walks the
/// head's spine, copies each out-of-scope sibling's hash **verbatim** (present, by definition of
/// spine, in the spine tree objects it already holds — no blob load, no descent) and splices in
/// the freshly built in-scope subtree(s). It replicates the tree build's own empty-subtree
/// pruning exactly (see [`build_tree_for_inventory_key`]'s child-emptiness check), bottom-up, so
/// a scoped stack's tree hash is **byte-identical** to what a full workspace stacking the same
/// content would produce — the stage-1 invariant.
///
/// When the stack completes a merge, `overrides` carries the out-of-scope skeleton:
/// out-of-scope siblings theirs changed one-sided, adopted by hash. The overlay applies them on
/// top of the head's verbatim siblings — `Some((hash, type))` sets an entry (a subtree, file or
/// symlink adopted from theirs), `None` deletes it, and an override for a name the head lacks
/// adds it — so the committed merge tree is byte-identical to a full workspace merging the same
/// two heads. For a plain stack the map is empty and every out-of-scope sibling is copied
/// verbatim from the head.
///
/// # Arguments
/// * `head_root_hash` - The current head parcel's root tree hash (`None` for an unborn pallet).
/// * `partial_root`   - The freshly built (sparse) root from [`build_tree_from_inventory_deferred`]:
///                      its in-scope subtree objects are already stored with correct hashes.
/// * `scope`          - The bay's materialization scope (the caller gates on it not being full).
/// * `overrides`      - The out-of-scope skeleton for a completing merge (empty for a plain stack).
/// * `batch`          - Where every new spine tree object built here is staged. `stack`/`park`
///                      pass the same batch their tree build used, so these writes join the same
///                      single durability barrier.
///
/// # Returns
/// * `Ok(TreeItem)` - The spliced root tree (its hash set, every new spine tree object staged
///                    into `batch`).
/// * `Err(String)`  - On a spine-path type flip (`scope_path_type_changed`), or a failed load
///                    or store.
pub fn build_scoped_root_tree(head_root_hash: Option<&str>,
                              partial_root: &TreeItem,
                              scope: &MaterializationScope,
                              overrides: &BTreeMap<String, Option<(String, DirEntryType)>>,
                              batch: &Arc<file_utils::WriteBatch>)
                              -> Result<TreeItem, String> {
    let head_root = match head_root_hash {
        Some(hash) => Some(object_utils::load_tree(hash)?),
        None => None,
    };

    // The root is never pruned, even when empty — matching the tree build, which always keeps
    // (and stores) the root tree object.
    match splice_spine_level(head_root.as_ref(), Some(partial_root), "", scope, overrides, batch)? {
        Some(tree) => Ok(tree),
        None => {
            let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
            store_tree(&mut tree, batch)?;
            Ok(tree)
        }
    }
}

/// Splice one spine directory level: emit a new tree whose entries are the head's
/// out-of-scope siblings copied verbatim and the continuing in-scope / spine children rebuilt
/// from the dock. Returns `None` when the level ends up empty (pruned from its parent), exactly
/// as the tree build's own bottom-up build prunes an empty child (see
/// [`build_tree_for_inventory_key`]).
///
/// The `head` tree is one level deep (its subtree children carry hashes; descending requires a
/// load), while the `dock` tree is the fully in-memory partial root — so out-of-scope siblings
/// are copied by hash off `head` with no load, and in-scope subtrees are taken straight off
/// `dock`.
fn splice_spine_level(head: Option<&TreeItem>,
                      dock: Option<&TreeItem>,
                      key: &str,
                      scope: &MaterializationScope,
                      overrides: &BTreeMap<String, Option<(String, DirEntryType)>>,
                      batch: &Arc<file_utils::WriteBatch>)
                      -> Result<Option<TreeItem>, String> {
    let mut tree = TreeItem::new(last_component(key).to_string(), String::new(), DirEntryType::Tree);

    let head_files: BTreeMap<&String, &TreeItem> = head
        .map(|tree| tree.get_files().collect())
        .unwrap_or_default();
    let head_subtrees: BTreeMap<&String, &TreeItem> = head
        .map(|tree| tree.get_subtrees().collect())
        .unwrap_or_default();
    let dock_files: BTreeMap<&String, &TreeItem> = dock
        .map(|tree| tree.get_files().collect())
        .unwrap_or_default();
    let dock_subtrees: BTreeMap<&String, &TreeItem> = dock
        .map(|tree| tree.get_subtrees().collect())
        .unwrap_or_default();

    // Every entry name the head already holds at this level (file or subtree) — the newly-added
    // merge-skeleton pass below only emits names the head lacks, so it never double-emits one an
    // inline branch already handled.
    let head_names: BTreeSet<&str> = head
        .map(|tree| tree.get_files().map(|(name, _)| name.as_str())
            .chain(tree.get_subtrees().map(|(name, _)| name.as_str()))
            .collect())
        .unwrap_or_default();

    // A spine level's own files are out of scope, and copied verbatim — or resolved by the merge
    // skeleton where it changed them — except the root ignore file, which every scope holds in
    // scope and which the dock pass below takes. A file where the scope expects a directory (a
    // spine ancestor, or an in-scope prefix itself) flipped from a directory at this revision:
    // a spine-path type change.
    for (name, item) in &head_files {
        let child_key = join_key(key, name);

        if scope.spine_file_is_sealed(&child_key)? {
            splice_out_of_scope_entry(&mut tree, name, item, overrides.get(&child_key));
        }
    }

    // The dock's files at this level: an in-scope one is taken as the dock has it, the same way
    // an in-scope subtree is below, so a head file the dock no longer holds is deleted. A file
    // where the scope expects a directory is the type change again, from the working-tree side:
    // without the refusal it would land in no branch below and be silently dropped from the
    // signed tree, so a scoped stack would diverge from a full stack of the same content.
    for (name, item) in &dock_files {
        let child_key = join_key(key, name);

        // An out-of-scope dock file is not producible by the current materialize/load model (the
        // working tree only ever holds in-scope content) — refused defensively rather than
        // silently spliced into a signed tree unsealed.
        if scope.spine_file_is_sealed(&child_key)? {
            // Frontier: reframe the typed refusal for this still-String walker (bridge shim).
            return Err(scope_utils::out_of_scope_refusal(&child_key).into());
        }

        tree.add_child(shallow_entry(item));
    }

    // Head subtrees: out-of-scope ones are copied verbatim (or resolved by the merge skeleton);
    // the in-scope subtree is taken fresh from the dock (pruned when empty or deleted); a deeper
    // spine recurses.
    for (name, item) in &head_subtrees {
        let child_key = join_key(key, name);

        match scope.classify(&child_key) {
            ScopeClass::OutOfScope =>
                splice_out_of_scope_entry(&mut tree, name, item, overrides.get(&child_key)),
            ScopeClass::InScope => {
                if let Some(dock_subtree) = dock_subtrees.get(name) {
                    if !is_tree_empty(dock_subtree) {
                        tree.add_child(shallow_entry(dock_subtree));
                    }
                }
            }
            ScopeClass::Spine => {
                let head_subtree = object_utils::load_tree(&item.hash)?;
                let dock_subtree = dock_subtrees.get(name).copied();

                if let Some(spliced) =
                    splice_spine_level(Some(&head_subtree), dock_subtree, &child_key, scope, overrides, batch)?
                {
                    tree.add_child(spliced);
                }
            }
        }
    }

    // In-scope / spine subtrees the dock has but the head does not — a newly added in-scope
    // directory (or the spine leading to one). A head *file* of the same name was either refused
    // above as a type change or, being in scope, left out for the dock to supply, so nothing
    // here can collide with one.
    for (name, dock_subtree) in &dock_subtrees {
        if head_subtrees.contains_key(*name) {
            continue;
        }

        let child_key = join_key(key, name);

        match scope.classify(&child_key) {
            ScopeClass::InScope => {
                if !is_tree_empty(dock_subtree) {
                    tree.add_child(shallow_entry(dock_subtree));
                }
            }
            ScopeClass::Spine => {
                if let Some(spliced) =
                    splice_spine_level(None, Some(dock_subtree), &child_key, scope, overrides, batch)?
                {
                    tree.add_child(spliced);
                }
            }
            // The dock never holds an out-of-scope subtree; ignore defensively.
            ScopeClass::OutOfScope => {}
        }
    }

    // Merge-skeleton entries theirs *added* out of scope at this level: an out-of-scope subtree,
    // file or symlink the head does not carry (so no inline branch above emitted it). A deletion
    // of a non-existent entry is a no-op, so only `Some` resolutions add anything. By construction
    // every skeleton path's parent is a spine directory the overlay walks, so each is applied at
    // exactly one level.
    for (path, resolution) in overrides {
        let Some((hash, item_type)) = resolution else { continue };

        let (parent, name) = match path.rsplit_once('/') {
            Some((parent, name)) => (parent, name),
            None => ("", path.as_str()),
        };

        if parent != key || head_names.contains(name) {
            continue;
        }

        // Skeleton paths are out-of-scope by construction; guard defensively so a malformed one
        // can never be spliced into a signed tree at an in-scope or spine path.
        if scope.classify(path) == ScopeClass::OutOfScope {
            tree.add_child(TreeItem::new(name.to_string(), hash.clone(), *item_type));
        }
    }

    if is_tree_empty(&tree) {
        return Ok(None);
    }

    store_tree(&mut tree, batch)?;

    Ok(Some(tree))
}

/// Emit one out-of-scope sibling into the spliced spine level: the merge skeleton's resolution
/// wins when present (`Some((hash, type))` sets it to theirs' entry, `None` deletes it — omit),
/// otherwise the head's entry is copied verbatim by hash. Never loads or descends the object.
fn splice_out_of_scope_entry(tree: &mut TreeItem,
                             name: &str,
                             head_entry: &TreeItem,
                             resolution: Option<&Option<(String, DirEntryType)>>) {
    match resolution {
        Some(Some((hash, item_type))) =>
            tree.add_child(TreeItem::new(name.to_string(), hash.clone(), *item_type)),
        Some(None) => {} // theirs deleted this out-of-scope entry — omit it
        None => tree.add_child(shallow_entry(head_entry)),
    }
}

/// A shallow copy of a tree entry — its `(name, hash, item_type)` with no children. The tree
/// object format serializes only that triple per entry (subtrees first, files second, each set
/// name-sorted), so a shallow entry builds a byte-identical parent object while carrying no
/// descendants; the child object it names is already stored.
fn shallow_entry(item: &TreeItem) -> TreeItem {
    TreeItem::new(item.name.clone(), item.hash.clone(), item.item_type)
}

/// Whether a tree has no files and no subtrees — the emptiness test the tree build
/// ([`build_tree_for_inventory_key`]) prunes an empty child on.
fn is_tree_empty(tree: &TreeItem) -> bool {
    tree.get_files().len() == 0 && tree.get_subtrees().len() == 0
}

/// Whether a directory's inventory shard, just rebuilt wholesale from `tree`, may be trusted to
/// carry `tree`'s hash as its rollup. Shared by `restore --staged`'s and `shift`'s tree-to-shard
/// builders (`build_unstaged_shards`, `shift_utils::build_inventories_for_tree`), which each
/// independently rebuild every shard of a subtree straight from a resolved tree and must decide
/// the same way whether the shard they just wrote is a trustworthy rollup — kept as one function
/// so the (subtle) rule can never drift between the two copies.
///
/// A rewrite is only trustworthy when both hold:
/// * it is a *complete* materialization of `tree` — the whole tree (an unscoped bay), or a
///   directory already fully in scope (see [`MaterializationScope::classify`]). A spine
///   directory in a scoped bay only ever rebuilds the in-scope subset of its real content, so
///   stamping it with `tree`'s hash would misrepresent staged state.
/// * `tree` is not empty. An empty tree is only possible for the warehouse root (a stored tree
///   never carries a pruned-empty child entry) — pruning makes "this subtree's hash" ill-defined
///   as a rollup either way.
///
/// # Arguments
/// * `scope` - The active bay's materialization scope.
/// * `key`   - The warehouse path key of the directory whose shard was just rebuilt.
/// * `tree`  - The (loaded) tree the shard was rebuilt from.
pub fn rollup_stampable(scope: &MaterializationScope, key: &str, tree: &TreeItem) -> bool {
    (scope.is_full() || scope.classify(key) == ScopeClass::InScope) && !is_tree_empty(tree)
}

/// Build, store and hash a tree object (setting the item's hash), like the per-directory step
/// of [`build_tree_from_inventory_deferred`]'s own tree build — staged into `batch`, same as
/// every other tree object this module writes.
fn store_tree(tree: &mut TreeItem, batch: &Arc<file_utils::WriteBatch>) -> Result<(), String> {
    let mut object = LooseObjectBuilder::build_tree(tree);
    tree.hash = object.hash.clone();

    object.store_deferred(batch)?;

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

/// The final component (the directory's own name) of a warehouse path key.
fn last_component(key: &str) -> &str {
    key.rsplit_once('/').map(|(_, name)| name).unwrap_or(key)
}

/// Resolve a warehouse path key to its subtree object hash inside a tree, or `None` when the key
/// names nothing (or names a file, not a directory). The empty key resolves to the root itself.
/// The trees on the path must be present to descend — which they are for an in-scope key.
///
/// # Arguments
/// * `root_tree_hash` - The tree to resolve the key in.
/// * `key`            - The warehouse path key (`/`-separated, e.g. `src/api`).
pub fn resolve_subtree_hash(root_tree_hash: &str, key: &str) -> Result<Option<String>, String> {
    if key.is_empty() {
        return Ok(Some(root_tree_hash.to_string()));
    }

    let mut current = root_tree_hash.to_string();

    for component in key.split('/') {
        let tree = object_utils::load_tree(&current)?;

        match tree.get_subtrees().find(|(name, _)| name.as_str() == component) {
            Some((_, item)) => current = item.hash.clone(),
            None => return Ok(None),
        }
    }

    Ok(Some(current))
}

/// The outcome of [`collect_subtree_closure`]: either the walk ran to completion under `cap`, or
/// it was abandoned the moment the closure exceeded `cap` — before loading whatever the rest of
/// the subtree contains. `Overflowed` carries nothing: a caller that only wants to refuse an
/// oversized subtree (the one caller today) has no use for a partial, arbitrarily-truncated list,
/// and not building one is the point — keeping it would reintroduce the unbounded work this type
/// exists to avoid paying for.
pub enum SubtreeClosure {
    /// The full closure, `cap` objects or fewer.
    Complete(Vec<String>),
    /// The closure exceeds `cap`; the walk stopped as soon as that was known.
    Overflowed,
}

/// The object closure of the subtree at a warehouse path key inside a parcel's tree: the subtree
/// root object and every tree and blob beneath it, de-duplicated. `Ok(None)` when the key names
/// nothing (or a file) in this tree.
///
/// The path-addressed subtree fetch endpoint (`docs/format/REMOTE_PROTOCOL.md`) serves exactly
/// this set, so a remote can resolve — and, when file-level path enforcement ships, authorize —
/// a fetch by path rather than by an opaque hash a path-blind object endpoint cannot gate.
///
/// The walk is bounded by `cap`: as soon as the closure built so far exceeds it, the walk stops
/// and returns [`SubtreeClosure::Overflowed`] without loading anything further. That makes an
/// oversized-subtree request cheap to refuse — a caller enforcing a response-size limit does not
/// pay for the rest of the walk just to learn it will discard the answer.
///
/// # Arguments
/// * `root_tree_hash` - The parcel's root tree hash.
/// * `key`            - The warehouse path key of the subtree.
/// * `cap`            - The closure size above which the walk aborts early.
pub fn collect_subtree_closure(
    root_tree_hash: &str, key: &str, cap: usize
) -> Result<Option<SubtreeClosure>, String> {
    let subtree = match resolve_subtree_hash(root_tree_hash, key)? {
        Some(hash) => hash,
        None => return Ok(None),
    };

    let mut closure: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut frontier: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    frontier.push_back(subtree);

    while let Some(tree_hash) = frontier.pop_front() {
        if !seen.insert(tree_hash.clone()) {
            continue;
        }

        closure.push(tree_hash.clone());
        if closure.len() > cap {
            return Ok(Some(SubtreeClosure::Overflowed));
        }

        let tree = object_utils::load_tree(&tree_hash)?;

        for (_, file) in tree.get_files() {
            if seen.insert(file.hash.clone()) {
                closure.push(file.hash.clone());
                if closure.len() > cap {
                    return Ok(Some(SubtreeClosure::Overflowed));
                }
            }
        }

        for (_, subtree) in tree.get_subtrees() {
            frontier.push_back(subtree.hash.clone());
        }
    }

    Ok(Some(SubtreeClosure::Complete(closure)))
}

#[cfg(test)]
mod subtree_closure_tests {
    use super::*;
    use crate::builder::object::loose_object_builder::LooseObjectBuilder;
    use crate::enums::dir_entry_type::DirEntryType;
    use crate::globals::StorageRootScope;
    use crate::model::blob::Blob;
    use crate::model::tree_item::TreeItem;
    use std::path::PathBuf;

    /// A fresh warehouse root for one test, entered as the active storage-root scope for its
    /// lifetime (each test runs on its own thread, so scopes never collide) — mirrors
    /// `prune_utils`'s test `Scratch`.
    struct Scratch {
        _scope: StorageRootScope,
        root: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let root = std::env::temp_dir().join(format!(
                "forklift-subtree-closure-test-{}-{}-{}", name, std::process::id(), id
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = StorageRootScope::enter(&root);

            Scratch { _scope: scope, root }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn store_blob(content: &str) -> String {
        let mut object = LooseObjectBuilder::build_blob(&Blob { content: content.as_bytes().to_vec() });
        object.store().unwrap();
        object.hash
    }

    /// A one-file tree whose content (hence hash) is unique to `marker` — trees are
    /// content-addressed, so two calls with the same `marker` collide on the same hash and two
    /// calls with different markers never do. Plain empty trees would all collide with each other
    /// (an empty tree has no content to vary), which the walk's own de-duplication would then
    /// silently fold into one node — exactly the trap this fixture needs to avoid to keep the
    /// closure size predictable.
    fn store_marked_tree(marker: &str) -> String {
        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        tree.add_child(TreeItem::new("v".to_string(), store_blob(marker), DirEntryType::Normal));
        let mut object = LooseObjectBuilder::build_tree(&tree);
        object.store().unwrap();
        object.hash
    }

    /// Six real, loadable, mutually-distinct trees named `a1..a6` (so `BTreeMap` iteration visits
    /// them in that order) plus one entry, `zzz_poison`, whose hash was never stored — sorted
    /// last, so nothing but a walk that runs past the cap can ever reach it. Returns the root
    /// tree's hash.
    ///
    /// A walk that (bug) only checks the cap after finishing keeps going once the real cap trip
    /// is behind it: it works through the remaining `aN` entries and eventually reaches
    /// `zzz_poison`, calling `load_tree` on a hash that names nothing — which fails. A walk that
    /// aborts as soon as the closure exceeds the cap never gets there. That gives a hard,
    /// non-timing signal that separates "aborted early" from "walked fully then checked":
    /// `Ok(Overflowed)` vs `Err(_)`.
    fn build_fixture_with_poison() -> String {
        let mut root = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        for name in ["a1", "a2", "a3", "a4", "a5", "a6"] {
            root.add_child(TreeItem::new(name.to_string(), store_marked_tree(name), DirEntryType::Tree));
        }
        root.add_child(TreeItem::new("zzz_poison".to_string(), "f".repeat(64), DirEntryType::Tree));

        let mut object = LooseObjectBuilder::build_tree(&root);
        object.store().unwrap();
        object.hash
    }

    #[test]
    fn the_walk_aborts_as_soon_as_the_closure_exceeds_the_cap_rather_than_completing() {
        let _scratch = Scratch::new("aborts-early");
        let root_hash = build_fixture_with_poison();

        let result = collect_subtree_closure(&root_hash, "", 5);

        match result {
            Ok(Some(SubtreeClosure::Overflowed)) => {} // expected: aborted before reaching poison
            Ok(Some(SubtreeClosure::Complete(closure))) => {
                panic!("the walk completed with {} objects instead of aborting", closure.len())
            }
            Ok(None) => panic!("the root resolved to nothing"),
            Err(error) => panic!(
                "the walk ran past the cap and failed loading the poisoned entry \
                (proves it did NOT abort early): {}", error
            ),
        }
    }

    #[test]
    fn under_the_cap_the_full_closure_still_comes_back() {
        let _scratch = Scratch::new("under-cap");

        // A small, poison-free fixture: root + two distinct one-file subtrees, capped well above
        // its size — a cap that never trips must still return the complete closure, not a
        // truncated one. Closure: root, x's tree, x's blob, y's tree, y's blob = 5 objects.
        let mut small = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        small.add_child(TreeItem::new("x".to_string(), store_marked_tree("x"), DirEntryType::Tree));
        small.add_child(TreeItem::new("y".to_string(), store_marked_tree("y"), DirEntryType::Tree));
        let mut object = LooseObjectBuilder::build_tree(&small);
        object.store().unwrap();

        let result = collect_subtree_closure(&object.hash, "", 100).unwrap().unwrap();

        match result {
            SubtreeClosure::Complete(closure) => {
                assert_eq!(closure.len(), 5, "root + (tree, blob) for each of x and y, none deduped away");
            }
            SubtreeClosure::Overflowed => panic!("a closure of 5 must not overflow a cap of 100"),
        }
    }
}
