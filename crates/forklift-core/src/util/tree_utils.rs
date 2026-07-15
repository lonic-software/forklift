use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::ops::Add;
use std::path::PathBuf;
use std::sync::Arc;
use regex::Regex;
use crate::builder::object::loose_object_builder::LooseObjectBuilder;
use crate::enums::dir_entry_type::DirEntryType;
use crate::enums::inventory_item_state::InventoryItemState;
use crate::model::task::tree_builder::tree_builder_context::{ShardSource, TreeBuilderContext};
pub use crate::model::task::tree_builder::tree_builder_context::ObjectSink;
use crate::model::inventory::Inventory;
use crate::model::task::TaskExecutor;
use crate::model::tree_item::TreeItem;
use crate::parser;
use crate::traits::task_context::TaskContext;
use crate::types::task::Task;
use crate::util::scope_utils::{self, MaterializationScope, ScopeClass};
use crate::util::{file_utils, inventory_utils, object_utils};

const FILENAME_METADATA_SUFFIX: &str = ".metadata";

/// Build (and store) tree objects from the inventory, bottom-up: one tree object per
/// inventoried directory. This is the first half of stacking a parcel.
///
/// * Entries staged for removal (`Deleted`) are excluded — that is how a staged removal
///   becomes an actual removal in the next parcel.
/// * Directories that end up empty (no files, no non-empty subdirectories) are pruned,
///   except the warehouse root.
/// * Ancestor directories that have no shard of their own (e.g. only `src/a` was ever
///   loaded) are synthesized so the chain root → `src` → `a` exists in the tree.
///
/// # Returns
/// * `Ok(Some(TreeItem))` - The root tree (its hash set, all tree objects stored).
/// * `Ok(None)`           - If there is no inventory at all (nothing was ever loaded).
/// * `Err(String)`        - If a shard could not be read or an object could not be stored.
///
/// The build runs in parallel over the `TaskExecutor` (one task per directory), scheduled
/// bottom-up by dependency: the leaves are enqueued first, and each completing directory
/// enqueues its parent once the parent's last child is built.
pub async fn build_tree_from_inventory() -> Result<Option<TreeItem>, String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    let Some(metadata) = metadata_opt else {
        return Ok(None);
    };

    // No head to compare rollups against here (this caller — `park` — never needs the skip; see
    // `build_tree_from_inventory_deferred` for `stack`'s optimized path), so no skip is
    // attempted: every directory is read and rebuilt exactly as before stage 2 existed.
    let (root, _tree_hashes, _untouched) =
        build_tree_from_inventory_core(&metadata, ShardSource::Disk, ObjectSink::Immediate, None).await?;

    Ok(root)
}

/// Like [`build_tree_from_inventory`], but reads shard content from an already-parsed
/// [`inventory_utils::PreparedInventory`] snapshot instead of the disk, and stages every built
/// tree object's write into `batch` instead of writing (and fsyncing) it immediately.
///
/// Used by `stack` (`stack_utils::stack_parcel`): the snapshot is the single read+parse pass
/// shared with the conflict check and the post-stack cleanup (§ perf), and the batch turns the
/// per-object fsync pairs this build would otherwise pay (one per built directory) into one
/// barrier the caller runs after this returns (see [`file_utils::WriteBatch`]).
///
/// Also applies the rollup-based skip (DESIGN.html §5.0 D item 8, stage 2) when `head_root_tree_hash`
/// is given: a directory whose shard's rollup already matches the corresponding head subtree
/// hash is never read past its header, never hashed, never (re)stored — its parent's tree
/// includes it verbatim by the hash the rollup and the head agree on. See
/// [`compute_rollup_skip_plan`] for exactly which keys are eligible.
///
/// # Arguments
/// * `prepared`           - The already-parsed shard snapshot.
/// * `batch`               - Where every built tree object's write is staged.
/// * `head_root_tree_hash` - The current pallet head's root tree hash, if any (`None` for an
///                           unborn pallet — no skip is possible with nothing to compare
///                           against). The skip is also disabled by the
///                           `FORKLIFT_DISABLE_ROLLUP_SKIP` kill switch (see
///                           [`inventory_utils::rollup_skip_enabled`]).
/// * `scope`               - The active bay's materialization scope — a skip is only ever
///                           attempted where it cannot interact with a scoped bay's spine
///                           splice (see [`compute_rollup_skip_plan`]).
///
/// # Returns
/// * `Ok((Some(TreeItem), BTreeMap<String, String>, BTreeSet<String>))` - The root tree, every
///   freshly built directory's subtree hash by warehouse path key (a non-empty subtree only —
///   see [`build_tree_for_inventory_key`]), and every directory key the rollup skip proved
///   unchanged and never read (`stack` leaves these shards' rollups untouched rather than
///   stamping or clearing them).
/// * `Ok((None, _, _))`                                                  - If there is no
///   inventory at all.
/// * `Err(String)`                                                       - If a shard could not
///   be read or an object could not be stored.
pub async fn build_tree_from_inventory_deferred(
    prepared: &Arc<inventory_utils::PreparedInventory>,
    batch: &Arc<file_utils::WriteBatch>,
    head_root_tree_hash: Option<&str>,
    scope: &MaterializationScope,
) -> Result<(Option<TreeItem>, BTreeMap<String, String>, BTreeSet<String>), String> {
    let Some(metadata) = &prepared.metadata else {
        return Ok((None, BTreeMap::new(), BTreeSet::new()));
    };

    build_tree_from_inventory_core(
        metadata,
        ShardSource::Prepared(Arc::clone(prepared)),
        ObjectSink::Deferred(Arc::clone(batch)),
        head_root_tree_hash.map(|hash| (hash, scope, &prepared.shards)),
    ).await
}

/// The shared bottom-up parallel tree build, parameterized over where shard content is read
/// from and where built objects are written — see [`build_tree_from_inventory`] (the original,
/// disk-reading, immediately-writing behavior) and [`build_tree_from_inventory_deferred`]
/// (`stack`'s optimized path). Kept as one implementation so the dependency-scheduling logic
/// (leaves-first, bottom-up, parent enqueued once its last child completes) can never drift
/// between two copies.
///
/// # Arguments
/// * `skip_context` - `Some((head_root_tree_hash, scope, shards))` to attempt the rollup-based
///                    skip (stage 2) against that head; `None` to build in full, exactly as
///                    before stage 2 existed (every caller except `stack`'s optimized path).
///                    `shards` is the same already-parsed shard snapshot `stack` built
///                    `PreparedInventory` from — the skip plan reads rollups out of it directly
///                    instead of re-reading shard headers off disk a second time (nothing
///                    mutates a shard between `prepare_stack_inventory` and here — see
///                    `PreparedInventory`'s own doc comment).
///
/// # Returns
/// * `Ok((Some(TreeItem), BTreeMap<String, String>, BTreeSet<String>))` - The root tree (its
///   hash set, all tree objects stored or staged), every built directory's non-empty subtree
///   hash by warehouse path key, and every directory key the rollup skip proved unchanged and
///   never read.
/// * `Ok((None, _, _))`                                                 - If `metadata` is
///   empty (nothing was ever loaded).
/// * `Err(String)`                                                      - If a shard could not
///   be read or an object could not be stored.
async fn build_tree_from_inventory_core(metadata: &BTreeSet<String>,
                                        shard_source: ShardSource,
                                        object_sink: ObjectSink,
                                        skip_context: Option<(&str, &MaterializationScope, &BTreeMap<String, Inventory>)>)
                                        -> Result<(Option<TreeItem>, BTreeMap<String, String>, BTreeSet<String>), String> {
    if metadata.is_empty() {
        return Ok((None, BTreeMap::new(), BTreeSet::new()));
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

    let skip_plan = match skip_context {
        Some((head_root_tree_hash, scope, shards)) => compute_rollup_skip_plan(&children, head_root_tree_hash, scope, shards)?,
        None => RollupSkipPlan::default(),
    };

    // The entire staged tree is provably identical to head (the root's own rollup already
    // named it): nothing is read past the shards `prepare_stack_inventory` already parsed for
    // the conflict check, nothing is hashed, nothing is (re)stored.
    if let Some(whole_tree_hash) = skip_plan.whole_tree_hash {
        return Ok((
            Some(TreeItem::new(String::new(), whole_tree_hash, DirEntryType::Tree)),
            BTreeMap::new(),
            skip_plan.pruned,
        ));
    }

    // Every pruned key (a skipped subtree's root, and everything beneath it) is excluded from
    // the task graph entirely — as if it were never inventoried. Its parent instead gets it
    // injected verbatim (by hash) inside `build_tree_for_inventory_key`, from `context.injections`.
    let keys: BTreeSet<String> = keys.difference(&skip_plan.pruned).cloned().collect();
    let children: BTreeMap<String, Vec<String>> = children.into_iter()
        .filter(|(key, _)| !skip_plan.pruned.contains(key))
        .map(|(key, child_keys)| {
            let child_keys = child_keys.into_iter().filter(|c| !skip_plan.pruned.contains(c)).collect();
            (key, child_keys)
        })
        .collect();

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
        pending_children, shard_source, object_sink, Arc::new(skip_plan.injections),
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

    Ok((Some(root), tree_hashes, skip_plan.pruned))
}

/// What [`compute_rollup_skip_plan`] decided for one tree build: which subtrees the rollup skip
/// lets it never read, hash or store, and what to splice into each surviving parent instead.
#[derive(Default)]
struct RollupSkipPlan {
    /// Set only when the *entire* staged tree is provably unchanged from head (the warehouse
    /// root's own rollup matches the head root tree hash): every registered key ends up in
    /// `pruned` and the caller returns a hash-only root immediately, without building anything.
    whole_tree_hash: Option<String>,

    /// Directory keys entirely excluded from the task graph — a skip root or one of its
    /// descendants. Also `cleanup_after_stack_with`'s "leave untouched" set: none of these were
    /// read this stack, so their on-disk rollups (already equal to what a fresh build would
    /// produce — that is *why* they were skipped) are left exactly as they sit.
    pruned: BTreeSet<String>,

    /// For each *unpruned* directory key, the `(name, head_hash)` pairs of its immediate
    /// children that a matching rollup let the build skip — added directly into that
    /// directory's tree with no load, no task (mirrors `build_scoped_root_tree`'s
    /// `splice_out_of_scope_entry` by-hash pattern).
    injections: BTreeMap<String, Vec<(String, String)>>,
}

/// Resolve, top-down, which inventoried subtrees can be skipped entirely because their shard's
/// rollup already matches the corresponding head subtree hash — no staged change is possible
/// anywhere in one (Stage 1's maintenance guarantees the rollup would have been cleared the
/// moment anything below it actually changed). A match stops the descent right there: everything
/// beneath a skip root is skipped too, without ever comparing its own rollup (redundant — the
/// parent's hash match already proves the whole subtree, content-addressing being what it is).
///
/// **Scoped-bay caveat.** A skip is only attempted where a key's *parent* is already fully in
/// scope (`scope.classify(parent) == InScope`, or the bay is unscoped). The outermost in-scope
/// boundary directory (whose parent is `Spine`) is deliberately never a skip candidate: a scoped
/// stack's spine splice (`build_scoped_root_tree`) later re-visits exactly that directory as an
/// opaque `TreeItem` and prunes it if `is_tree_empty` — which a hash-only, no-children stand-in
/// would wrongly report, silently dropping a real (non-empty) subtree from the signed tree. A
/// full workspace's root has no such boundary crossing to worry about; a scoped bay's own root is
/// always `Spine` itself, so it is never a skip candidate either way (see the `is_full()` gate
/// on the whole-tree fast path below).
///
/// # Arguments
/// * `children`             - The parent → children relation over every inventoried directory
///                            key (pre-pruning).
/// * `head_root_tree_hash`  - The pallet head's root tree hash.
/// * `scope`                - The active bay's materialization scope.
/// * `shards`               - The already-parsed shard snapshot (`stack_utils::stack_parcel`'s
///                            `PreparedInventory`) to read every candidate's rollup out of — an
///                            in-memory lookup, not a disk re-read (nothing mutates a shard
///                            between that snapshot and this call).
///
/// # Returns
/// * `Ok(RollupSkipPlan)` - The plan (empty when the kill switch is set, or nothing matched).
/// * `Err(String)`        - If a head tree object could not be read.
fn compute_rollup_skip_plan(children: &BTreeMap<String, Vec<String>>,
                            head_root_tree_hash: &str,
                            scope: &MaterializationScope,
                            shards: &BTreeMap<String, Inventory>) -> Result<RollupSkipPlan, String> {
    let mut plan = RollupSkipPlan::default();

    if !inventory_utils::rollup_skip_enabled() {
        return Ok(plan);
    }

    let rollup_of = |key: &str| shards.get(key).and_then(|inventory| inventory.get_rollup_hash().cloned());

    let scope_is_full = scope.is_full();
    let root_head_tree = object_utils::load_tree_shared(head_root_tree_hash)?;

    if scope_is_full {
        if let Some(rollup) = rollup_of("") {
            if rollup == head_root_tree_hash {
                prune_subtree("", children, &mut plan.pruned);
                plan.whole_tree_hash = Some(rollup);
                inventory_utils::record_rollup_skip();
                return Ok(plan);
            }
        }
    }

    // (key, the head subtree at key, whether `key` itself is fully in scope — gates whether
    // `key`'s *children* are eligible for a skip; see the caveat above).
    let mut stack: Vec<(String, Arc<TreeItem>, bool)> = vec![(String::new(), root_head_tree, scope_is_full)];

    while let Some((key, head_tree, key_in_scope)) = stack.pop() {
        let Some(child_keys) = children.get(&key) else { continue };

        let head_subtrees: BTreeMap<&String, &TreeItem> = head_tree.get_subtrees().collect();

        for child_key in child_keys {
            let child_name = last_component(child_key);
            let Some(head_child) = head_subtrees.get(&child_name.to_string()) else { continue };

            // A rollup lookup is now a plain in-memory map read (`shards` is the same snapshot
            // already parsed for the conflict check), so there is nothing left to parallelize
            // here — unlike a disk re-read per candidate, which used to cost more wall clock
            // than the parallel rebuild it replaced (DESIGN.html §5.0 D item 8 stage 2b finding).
            if key_in_scope {
                if let Some(rollup) = rollup_of(child_key) {
                    if rollup == head_child.hash {
                        plan.injections.entry(key.clone()).or_default()
                            .push((child_name.to_string(), rollup));
                        prune_subtree(child_key, children, &mut plan.pruned);
                        inventory_utils::record_rollup_skip();
                        continue;
                    }
                }
            }

            let child_in_scope = scope_is_full || scope.classify(child_key) == ScopeClass::InScope;
            let head_child_tree = object_utils::load_tree_shared(&head_child.hash)?;
            stack.push((child_key.clone(), head_child_tree, child_in_scope));
        }
    }

    Ok(plan)
}

/// Add `key` and every one of its descendants (found transitively through `children`) to
/// `pruned` — the whole subtree a matched rollup lets the build skip entirely.
fn prune_subtree(key: &str, children: &BTreeMap<String, Vec<String>>, pruned: &mut BTreeSet<String>) {
    pruned.insert(key.to_string());

    if let Some(child_keys) = children.get(key) {
        for child_key in child_keys {
            prune_subtree(child_key, children, pruned);
        }
    }
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

        match &context.shard_source {
            ShardSource::Disk => {
                let (_, shard_bytes) = file_utils::retrieve_inventory_or_none_by_key(&key)?;

                if let Some(bytes) = shard_bytes {
                    let inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
                        .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?;

                    for (_, item) in inventory.get_items() {
                        if item.state == InventoryItemState::Deleted {
                            continue;
                        }

                        tree.add_child(TreeItem::new(item.name.clone(), item.hash.clone(), item.item_type));
                    }
                }
            }
            ShardSource::Prepared(prepared) => {
                if let Some(inventory) = prepared.shards.get(&key) {
                    for (_, item) in inventory.get_items() {
                        if item.state == InventoryItemState::Deleted {
                            continue;
                        }

                        tree.add_child(TreeItem::new(item.name.clone(), item.hash.clone(), item.item_type));
                    }
                }
            }
        }

        if let Some(child_keys) = children.get(&key) {
            let mut built = context.built.lock().await;

            for child_key in child_keys {
                let child_tree = built.remove(child_key)
                    .ok_or(format!("Subtree \"{}\" was not built before its parent.", child_key))?;

                let is_empty = child_tree.get_files().len() == 0
                    && child_tree.get_subtrees().len() == 0;

                if !is_empty {
                    tree.add_child(child_tree);
                }
            }
        }

        // Skip-injected children (DESIGN.html §5.0 D item 8, stage 2): subtrees whose rollup
        // already matched the head, added verbatim by hash — no load, no task, and unlike the
        // `built`-sourced children above, never empty (a matching rollup is only ever recorded
        // for a non-empty subtree — see `build_tree_for_inventory_key`'s own rollup stamping —
        // so no emptiness check applies here).
        if let Some(injected) = context.injections.get(&key) {
            for (child_name, head_hash) in injected {
                tree.add_child(TreeItem::new(child_name.clone(), head_hash.clone(), DirEntryType::Tree));
            }
        }

        let mut object = LooseObjectBuilder::build_tree(&tree);
        tree.hash = object.hash.clone();

        match &context.object_sink {
            ObjectSink::Immediate => { object.store()?; }
            ObjectSink::Deferred(batch) => { object.store_deferred(batch)?; }
        }

        // Only a non-empty subtree's hash is a meaningful rollup: an empty directory is pruned
        // by its parent (below), so "the tree stack would build for this subtree" is ill-defined
        // for it — omitted here rather than recorded and later filtered out.
        let is_empty = tree.get_files().len() == 0 && tree.get_subtrees().len() == 0;

        if !is_empty {
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
/// A scoped bay materializes only its in-scope subtree(s); a plain [`build_tree_from_inventory`]
/// over that dock would emit a root that has *only* the in-scope path, with every out-of-scope
/// sibling silently gone. This overlay instead walks the head's spine, copies each out-of-scope
/// sibling's hash **verbatim** (present, by definition of spine, in the spine tree objects it
/// already holds — no blob load, no descent) and splices in the freshly built in-scope
/// subtree(s). It replicates [`build_tree_from_inventory`]'s empty-subtree pruning exactly
/// (`:173-178`), bottom-up, so a scoped stack's tree hash is **byte-identical** to what a full
/// workspace stacking the same content would produce — the stage-1 invariant.
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
/// * `partial_root`   - The freshly built (sparse) root from [`build_tree_from_inventory`]: its
///                      in-scope subtree objects are already stored with correct hashes.
/// * `scope`          - The bay's materialization scope (the caller gates on it not being full).
/// * `overrides`      - The out-of-scope skeleton for a completing merge (empty for a plain stack).
/// * `sink`           - Where every new spine tree object built here is written — see
///                      [`ObjectSink`]. `stack` passes the same batch its tree build used, so
///                      these writes join the same single durability barrier.
///
/// # Returns
/// * `Ok(TreeItem)` - The spliced root tree (its hash set, every new spine tree object stored or
///                    staged, per `sink`).
/// * `Err(String)`  - On a spine-path type flip (`scope_path_type_changed`), or a failed load
///                    or store.
pub fn build_scoped_root_tree(head_root_hash: Option<&str>,
                              partial_root: &TreeItem,
                              scope: &MaterializationScope,
                              overrides: &BTreeMap<String, Option<(String, DirEntryType)>>,
                              sink: &ObjectSink)
                              -> Result<TreeItem, String> {
    let head_root = match head_root_hash {
        Some(hash) => Some(object_utils::load_tree(hash)?),
        None => None,
    };

    // The root is never pruned, even when empty — matching build_tree_from_inventory, which
    // always keeps (and stores) the root tree object.
    match splice_spine_level(head_root.as_ref(), Some(partial_root), "", scope, overrides, sink)? {
        Some(tree) => Ok(tree),
        None => {
            let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
            store_tree(&mut tree, sink)?;
            Ok(tree)
        }
    }
}

/// Splice one spine directory level: emit a new tree whose entries are the head's
/// out-of-scope siblings copied verbatim and the continuing in-scope / spine children rebuilt
/// from the dock. Returns `None` when the level ends up empty (pruned from its parent), exactly
/// as [`build_tree_from_inventory`]'s bottom-up build prunes an empty child.
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
                      sink: &ObjectSink)
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

    // A spine level's own files are all out-of-scope (an in-scope path is a directory prefix),
    // so they are copied verbatim — or resolved by the merge skeleton where it changed
    // them — unless the path is where an in-scope directory is expected, i.e. the entry flipped
    // from a directory to a file at this revision (a spine-path type change).
    for (name, item) in &head_files {
        let child_key = join_key(key, name);

        match scope.classify(&child_key) {
            ScopeClass::OutOfScope =>
                splice_out_of_scope_entry(&mut tree, name, item, overrides.get(&child_key)),
            ScopeClass::InScope | ScopeClass::Spine =>
                return Err(scope_utils::type_changed_refusal(&child_key).into()),
        }
    }

    // The symmetric check from the *dock* (working-tree) side: a file here means the working
    // tree has replaced what the scope expects to be a directory (the in-scope prefix itself,
    // or a spine ancestor of one) with a plain file. Without this check such a file lands in no
    // other branch below (`dock_subtrees.get` finds nothing under its name) and is silently
    // dropped from the signed tree — a scoped stack would then diverge from a full stack of the
    // same content. Refuse instead, exactly mirroring the head-files case above — on the first
    // dock file found, since any one of them is already a refusal (a `BTreeMap`, so this is the
    // alphabetically-first name, deterministic either way).
    if let Some((name, _)) = dock_files.iter().next() {
        let child_key = join_key(key, name);

        // Frontier: reframe the typed refusal for this still-String walker (bridge shim).
        return Err(match scope.classify(&child_key) {
            ScopeClass::InScope | ScopeClass::Spine => scope_utils::type_changed_refusal(&child_key),
            // Not producible by the current materialize/load model (the working tree only ever
            // holds content on the path to an in-scope prefix) — refused defensively rather
            // than silently spliced into a signed tree unsealed.
            ScopeClass::OutOfScope => scope_utils::out_of_scope_refusal(&child_key),
        }.into());
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
                    splice_spine_level(Some(&head_subtree), dock_subtree, &child_key, scope, overrides, sink)?
                {
                    tree.add_child(spliced);
                }
            }
        }
    }

    // In-scope / spine subtrees the dock has but the head does not — a newly added in-scope
    // directory (or the spine leading to one). A head *file* of the same name would already
    // have been refused above as a type change, so nothing here can collide with one.
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
                    splice_spine_level(None, Some(dock_subtree), &child_key, scope, overrides, sink)?
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

    store_tree(&mut tree, sink)?;

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

/// Whether a tree has no files and no subtrees — the emptiness test
/// [`build_tree_from_inventory`] prunes on (`:173-178`).
fn is_tree_empty(tree: &TreeItem) -> bool {
    tree.get_files().len() == 0 && tree.get_subtrees().len() == 0
}

/// Build, store and hash a tree object (setting the item's hash), like the per-directory step
/// of [`build_tree_from_inventory`]. `sink` decides whether the write happens immediately or is
/// staged into a batch — see [`ObjectSink`].
fn store_tree(tree: &mut TreeItem, sink: &ObjectSink) -> Result<(), String> {
    let mut object = LooseObjectBuilder::build_tree(tree);
    tree.hash = object.hash.clone();

    match sink {
        ObjectSink::Immediate => { object.store()?; }
        ObjectSink::Deferred(batch) => { object.store_deferred(batch)?; }
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

/// The final component (the directory's own name) of a warehouse path key.
fn last_component(key: &str) -> &str {
    key.rsplit_once('/').map(|(_, name)| name).unwrap_or(key)
}

/// Create tree objects for the given directory and all of its subdirectories.
/// The tree objects are stored in the object store.
/// A metadata file is also created, which contains the mapping of directory paths to tree hashes.
///
/// # Arguments
/// * `path` - The path to the directory.
///
/// # Returns
/// * `Ok(TreeItem)` - if the tree was built successfully.
/// * `Err(String)`  - if an error occurred while building the tree.
pub fn create_tree_for_directory(path: &PathBuf) -> Result<Option<TreeItem>, String> {
    let mut tree_hashes: BTreeMap<String, String> = BTreeMap::new();
    let ignored_paths = file_utils::get_ignored_paths()?;
    let result = build_tree(path, &mut tree_hashes, &ignored_paths)?;

    if let Some(tree_item) = &result {
        build_tree_metadata(&tree_item.hash, &tree_hashes)?;
    }

    Ok(result)
}

/// Build a tree item from a directory.
/// Created tree and blob objects are stored.
///
/// # Arguments
/// * `path`        - The path to the directory.
/// * `tree_hashes` - The mapping of directory paths to tree hashes.
/// The new tree objects will be added to this map.
///
/// # Returns
/// * `Ok(TreeItem)` - if the tree was built successfully.
/// * `Err(String)`  - if an error occurred while building the tree.
fn build_tree(path: &PathBuf,
              tree_hashes: &mut BTreeMap<String, String>,
              ignored_paths: &Vec<Regex>) -> Result<Option<TreeItem>, String> {
    let path_string = file_utils::path_to_string(path)?;

    if ignored_paths.iter().any(|r| r.is_match(&path_string)) {
        return Ok(None)
    }

    let directory = file_utils::read_directory(path)?;
    let name = file_utils::get_filename_from_path(path)?.unwrap_or(String::new());

    let mut tree = TreeItem::new(name, String::from(""), DirEntryType::Tree);

    for entry_result in directory {
        let entry = entry_result
            .map_err(|e| format!("Error while reading directory entry: {}", e))?;
        let entry_path = file_utils::path_to_string(&entry.path())?;

        if ignored_paths.iter().any(|r| r.is_match(&entry_path)) {
            continue;
        }

        let name = file_utils::get_name_for_file_or_directory(&entry)?;
        let metadata = file_utils::get_symlink_metadata_for_path(&entry.path())?;
        let item_type = file_utils::get_type_of_dir_entry(&metadata);

        if item_type.is_file() {
            let tree_item = build_tree_item_from_file(&entry, name, item_type)?;
            tree.add_child(tree_item);
        } else {
            let tree_item = build_tree(&entry.path(), tree_hashes, ignored_paths)?;

            if let Some(item) = tree_item {
                tree.add_child(item);
            }
        }
    }

    let mut object = LooseObjectBuilder::build_tree(&tree);
    tree.hash = object.hash.clone();
    object.store()?;

    let path_string = file_utils::path_to_string(path)?;
    tree_hashes.insert(path_string, object.hash.clone());

    Ok(Some(tree))
}

/// Build a tree item from a file.
/// Created blob objects are stored.
///
/// # Arguments
/// * `entry`     - The directory entry to build the tree item from (should be a file).
/// * `name`      - The name of the file.
/// * `item_type` - The type of the tree item.
///
/// # Returns
/// * `Ok(TreeItem)` - if the tree item was built successfully.
/// * `Err(String)`  - if an error occurred while building the tree item.
fn build_tree_item_from_file(entry: &std::fs::DirEntry,
                             name: String,
                             item_type: DirEntryType) -> Result<TreeItem, String> {
    // Chunk-aware ingest: a giant becomes a recipe + chunks (its entry type upgraded to a
    // `*Chunked` variant), a small file an ordinary blob. Store mode persists whatever it built.
    let ingested = object_utils::ingest_file(&name, &entry.path(), item_type,
                                             object_utils::IngestMode::Store)?;

    if let Some(mut object) = ingested.deferred {
        object.store()?;
    }

    Ok(TreeItem::new(name, ingested.hash, ingested.item_type))
}

/// Create (and save) a tree metadata file.
/// The metadata file contains the mapping of directory paths to tree hashes.
///
/// # Arguments
/// * `root_hash`   - The hash of the root tree object.
/// * `tree_hashes` - The mapping of directory paths to tree hashes.
/// The key should be the path, and the value should be the hash.
///
/// # Returns
/// * `Ok(())`      - if the metadata was successfully created.
/// * `Err(String)` - if an error occurred while creating the metadata.
fn build_tree_metadata(root_hash: &str, tree_hashes: &BTreeMap<String, String>) -> Result<(), String> {
    let mut metadata: Vec<u8>  = Vec::new();

    for (path, hash) in tree_hashes {
        metadata.extend(path.as_bytes());
        object_utils::push_end_of_text(&mut metadata);
        metadata.extend_from_slice(hash.as_bytes());
        object_utils::push_new_line(&mut metadata);
    }

    let (folder_path, tree_filename) = file_utils::get_path_for_object(root_hash)?;
    let metadata_path = String::from(folder_path)
        .add(file_utils::PATH_SEPARATOR)
        .add(&tree_filename)
        .add(FILENAME_METADATA_SUFFIX);

    std::fs::write(&metadata_path, metadata).map_err(|e|
        format!("Error while writing tree metadata to file \"{}\": {}", metadata_path, e)
    )?;

    Ok(())
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

/// The object closure of the subtree at a warehouse path key inside a parcel's tree: the subtree
/// root object and every tree and blob beneath it, de-duplicated. `Ok(None)` when the key names
/// nothing (or a file) in this tree.
///
/// The path-addressed subtree fetch endpoint (`docs/format/REMOTE_PROTOCOL.md`) serves exactly
/// this set, so a remote can resolve — and, when file-level path enforcement ships, authorize —
/// a fetch by path rather than by an opaque hash a path-blind object endpoint cannot gate.
///
/// # Arguments
/// * `root_tree_hash` - The parcel's root tree hash.
/// * `key`            - The warehouse path key of the subtree.
pub fn collect_subtree_closure(root_tree_hash: &str, key: &str) -> Result<Option<Vec<String>>, String> {
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
        let tree = object_utils::load_tree(&tree_hash)?;

        for (_, file) in tree.get_files() {
            if seen.insert(file.hash.clone()) {
                closure.push(file.hash.clone());
            }
        }

        for (_, subtree) in tree.get_subtrees() {
            frontier.push_back(subtree.hash.clone());
        }
    }

    Ok(Some(closure))
}
