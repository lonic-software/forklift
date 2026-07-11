//! Scope-prune: reclaiming a sparse warehouse's disk by freeing the content under a
//! narrowed-away fetch-scope path (DESIGN.html §7.6).
//!
//! Distinct from reachability-`gc` on purpose. The objects a prune frees are still reachable
//! from pallet heads — ordinary history, not garbage — so `gc` neither does nor structurally
//! can free them. A prune frees them precisely because the warehouse fetch scope no longer
//! covers their path: afterward each is sealed by the hash its parent spine tree still commits,
//! exactly the state of an object a sparse franchise never fetched. So a prune re-enters the
//! "sealed but unfetched" state rather than leaving a hole an absence check would flag.
//!
//! The reclamation is computed as the closure of the pruned subtree(s) across history, minus
//! everything the post-prune scope still keeps. The subtraction makes it content-addressing-
//! safe: an object the pruned path shares with a still-fetched path (or with any meta pallet)
//! is retained, never freed. Meta pallets (office and the rest) are walked in full and never
//! scoped — the carve-out this feature depends on.
//!
//! Callers hold the warehouse lock and narrow the fetch scope *before* deleting (durable
//! before destructive): once the scope is narrowed, every scope-aware walk reads the pruned
//! path as out-of-scope, so no deletion can ever leave an object that reads as unexpectedly
//! *missing*. A crash mid-deletion therefore leaves the store correct — the deleted objects
//! read as sealed-but-unfetched, and any not-yet-deleted objects are harmless present-but-out-
//! of-scope extras (still reachable history, so `gc` keeps them; a scope-aware repack is the
//! future path that would reclaim them). The reverse order would be unsafe: deleting before
//! narrowing could leave an in-scope object missing, the one state absence must never mean.
//!
//! A crashed or killed prune must also be **resumable**, not just safe: a later call for the
//! same path re-derives the closure and finishes freeing what an earlier run left behind. That
//! requires the target closure to tolerate a partially-deleted plan — a hash absent mid-walk
//! must mean "already freed by an earlier run," never "corrupt, stop" — which only holds if
//! objects are always freed in an order where **every child is deleted before its parent**
//! ([`collect_prune_targets`] computes exactly that order, and `free_objects` deletes a plan
//! front-to-back). Under that discipline, a present parent implies every one of its still-listed
//! children is present too, so an absent node can only mean its whole subtree was already
//! freed — never a corrupt gap a resumed walk could misinterpret as newly-discovered garbage.

use std::collections::{HashSet, VecDeque};
use crate::util::pallet_utils::PalletNamespace;
use crate::util::scope_utils::{MaterializationScope, ScopeClass};
use crate::util::{audit_utils, file_utils, object_utils, pack_utils, pallet_utils, tree_utils};

/// The reclamation a prune would carry out, computed without mutating anything (safe for a
/// dry run).
pub struct PrunePlan {
    /// Present, loose objects to delete: under a pruned path, and needed by no retained scope.
    pub to_free: Vec<String>,

    /// Candidates present only inside a pack. A loose delete cannot reclaim a packed object
    /// (that is a repack concern), so they are counted and reported, never silently dropped.
    pub still_packed: usize,
}

/// What an executed prune deleted.
pub struct PruneStats {
    /// Loose objects actually deleted.
    pub freed: usize,
}

/// Compute a prune plan without mutating anything: the present, loose objects under the pruned
/// path(s) that no post-prune scope still needs. Safe to call for a dry run — and safe to call
/// again over the same path after an earlier, interrupted prune: an object an earlier run
/// already freed is simply absent from the plan, never an error (see the module doc).
///
/// # Arguments
/// * `pruned_prefixes` - The warehouse path keys being pruned (each a current fetch-scope
///   prefix, validated by the caller).
/// * `post_prune`      - The warehouse fetch scope that will remain after the prune.
pub fn plan_prune(
    pruned_prefixes: &[String],
    post_prune: &MaterializationScope,
) -> Result<PrunePlan, String> {
    let (user_parcels, meta_parcels) = reachable_parcels()?;
    let retained = collect_retained(&user_parcels, &meta_parcels, post_prune)?;

    // Ordered children-before-parents (see the module doc): filtering this sequence (never
    // reordering it) preserves that property in `to_free`, which is what makes a partial
    // `free_objects` run — and a later re-`plan_prune` call over the same path — resumable.
    let targets = collect_prune_targets(&user_parcels, pruned_prefixes)?;

    let mut to_free: Vec<String> = Vec::new();
    let mut still_packed = 0usize;

    for hash in &targets {
        // Shared with a retained path by content-addressing (or with a meta pallet): must stay.
        if retained.contains(hash) {
            continue;
        }

        // A candidate present in a pack cannot be reclaimed by a loose delete; count it.
        if pack_utils::is_in_packs(hash)? {
            still_packed += 1;
            continue;
        }

        // Present loose and reclaimable. (An object neither packed nor loose is already gone —
        // freed by an earlier, interrupted prune — and needs no action; `collect_prune_targets`
        // already excluded it, having found it absent during its own walk.)
        if loose_object_exists(hash)? {
            to_free.push(hash.clone());
        }
    }

    Ok(PrunePlan { to_free, still_packed })
}

/// Delete the planned loose objects, reclaiming their disk. Content objects (trees, blobs)
/// carry no signature sidecar — only parcels do, and a plan never contains a parcel — so
/// nothing rides along. Idempotent: an object already gone (a resumed prune) is the desired
/// end state, not an error.
///
/// # Arguments
/// * `to_free` - The loose object hashes from a [`PrunePlan`].
pub fn free_objects(to_free: &[String]) -> Result<PruneStats, String> {
    let mut freed = 0usize;

    for hash in to_free {
        let (folder, file_name) = file_utils::get_path_for_object(hash)?;
        let path = std::path::Path::new(&folder).join(file_name);

        match std::fs::remove_file(&path) {
            Ok(()) => freed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("Error while freeing object {}: {}", hash, e)),
        }
    }

    Ok(PruneStats { freed })
}

/// The reachable user- and meta-pallet parcels, kept apart because the carve-out scopes only
/// user-pallet content: meta pallets (office and the rest) are retained in full, always.
///
/// Only the persistent pallet heads are roots. Transient local states (parked parcels, an
/// in-progress consolidation) are deliberately omitted: a prune only ever frees objects under
/// a pruned path, and any such object a transient state references is out of scope after the
/// prune — sealed by hash, its object not needed — so omitting those roots can never free
/// something a transient state still needs. New in-scope work under the pruned path could only
/// live in a checkout that scopes the path, which the caller's materialization-scope guard
/// refuses before reaching here.
fn reachable_parcels() -> Result<(Vec<String>, Vec<String>), String> {
    let mut user_roots: Vec<String> = Vec::new();
    let mut meta_roots: Vec<String> = Vec::new();

    for (pallet_ref, head) in pallet_utils::all_pallet_refs()? {
        match pallet_ref.namespace {
            PalletNamespace::Meta => meta_roots.push(head),
            PalletNamespace::User => user_roots.push(head),
        }
    }

    let user: Vec<String> = audit_utils::collect_reachable_present(&user_roots)?.into_iter().collect();
    let meta: Vec<String> = audit_utils::collect_reachable_present(&meta_roots)?.into_iter().collect();

    Ok((user, meta))
}

/// Every object that must survive the prune: exactly what a fresh franchise scoped to the
/// post-prune fetch scope would hold.
///
/// User-pallet parcels are walked scoped — the spine and every in-scope subtree in full; at an
/// out-of-scope subtree the walk keeps nothing and stops, so the boundary object itself becomes
/// freeable while its hash stays sealed inside the retained parent spine tree. Meta-pallet
/// parcels are walked in full: the carve-out never scopes them.
fn collect_retained(
    user_parcels: &[String],
    meta_parcels: &[String],
    post_prune: &MaterializationScope,
) -> Result<HashSet<String>, String> {
    let mut retained: HashSet<String> = HashSet::new();

    // Meta pallets and every in-scope subtree feed one full, path-independent closure walk.
    let mut full_frontier: VecDeque<String> = VecDeque::new();

    for parcel in meta_parcels {
        retained.insert(parcel.clone());
        full_frontier.push_back(object_utils::load_parcel(parcel)?.tree_hash);
    }

    // The scoped spine walk. Its frontier dedups on (hash, path) — classification is
    // path-dependent, so one tree object can be spine at one path and in-scope at another, and
    // must be revisited per path to retain the right children each time.
    let mut spine_seen: HashSet<(String, String)> = HashSet::new();
    let mut spine_frontier: VecDeque<(String, String)> = VecDeque::new();

    for parcel in user_parcels {
        retained.insert(parcel.clone());
        let tree = object_utils::load_parcel(parcel)?.tree_hash;
        spine_frontier.push_back((tree, String::new()));
    }

    while let Some((tree_hash, path)) = spine_frontier.pop_front() {
        if !spine_seen.insert((tree_hash.clone(), path.clone())) {
            continue;
        }

        retained.insert(tree_hash.clone());
        let tree = object_utils::load_tree(&tree_hash)?;

        for (name, file) in tree.get_files() {
            // A file is never spine; retain it unless it is out of scope (files carry no
            // children, so an out-of-scope file's blob is simply freeable).
            if post_prune.classify(&join_key(&path, name)) != ScopeClass::OutOfScope {
                retained.insert(file.hash.clone());
            }
        }

        for (name, subtree) in tree.get_subtrees() {
            let child = join_key(&path, name);

            match post_prune.classify(&child) {
                ScopeClass::InScope => full_frontier.push_back(subtree.hash.clone()),
                ScopeClass::Spine => spine_frontier.push_back((subtree.hash.clone(), child)),
                // The boundary object is freeable; its hash stays sealed in this spine tree.
                ScopeClass::OutOfScope => {}
            }
        }
    }

    // The full closure of every in-scope (and meta) subtree, deduped on hash. Its own visited
    // set is independent of the spine walk's: a tree seen as spine at one path must still be
    // descended in full when it also appears in scope at another.
    let mut full_seen: HashSet<String> = HashSet::new();

    while let Some(tree_hash) = full_frontier.pop_front() {
        if !full_seen.insert(tree_hash.clone()) {
            continue;
        }

        retained.insert(tree_hash.clone());
        let tree = object_utils::load_tree(&tree_hash)?;

        for (_, file) in tree.get_files() {
            retained.insert(file.hash.clone());
        }

        for (_, subtree) in tree.get_subtrees() {
            full_frontier.push_back(subtree.hash.clone());
        }
    }

    Ok(retained)
}

/// The full closure of every version of the pruned subtree(s) across the reachable user
/// history — the candidate set a prune may free — **ordered so every child precedes its
/// parent** (a topological, post-order sequence). Only user-pallet parcels are walked: a pruned
/// path is user-pallet content, never meta. A parcel that never held the path (older history,
/// or a revision where the name was a file) contributes nothing.
///
/// Presence-tolerant, and that is what makes a prune resumable (see the module doc): a tree
/// hash the walk cannot find on disk is treated as "already freed by an earlier run" — its
/// descent is simply skipped, never an error — which is sound only because objects are always
/// *freed* in the same child-before-parent order this function produces, so an absent node
/// proves its whole subtree is already gone, not merely that this one object is.
fn collect_prune_targets(
    user_parcels: &[String],
    pruned_prefixes: &[String],
) -> Result<Vec<String>, String> {
    /// One step of the explicit (non-recursive) post-order walk: a tree not yet expanded, or a
    /// tree whose children have all been visited and is ready to be appended to `ordered`.
    enum Step {
        Expand(String),
        Finalize(String),
    }

    let mut ordered: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<Step> = Vec::new();

    for parcel in user_parcels {
        let root = object_utils::load_parcel(parcel)?.tree_hash;

        for prefix in pruned_prefixes {
            if let Some(subtree) = tree_utils::resolve_subtree_hash(&root, prefix)? {
                stack.push(Step::Expand(subtree));
            }
        }
    }

    while let Some(step) = stack.pop() {
        match step {
            // Popped only after every child pushed below it has been fully processed (LIFO), so
            // appending here is exactly the post-order position: after every descendant.
            Step::Finalize(hash) => ordered.push(hash),

            Step::Expand(hash) => {
                if !seen.insert(hash.clone()) {
                    continue;
                }

                if !file_utils::does_object_exist(&hash)? {
                    // Already freed by an earlier run: everything beneath it is gone too (the
                    // child-before-parent deletion order guarantees it), so there is nothing
                    // left to discover past this boundary.
                    continue;
                }

                let tree = object_utils::load_tree(&hash)?;

                // This tree's own position comes after all its children: push its Finalize
                // first so it sits *underneath* them on the stack and pops last.
                stack.push(Step::Finalize(hash.clone()));

                for (_, file) in tree.get_files() {
                    // A blob is a leaf — no descent needed — but it gets the same presence
                    // tolerance: absent means an earlier run already freed it.
                    if seen.insert(file.hash.clone()) && file_utils::does_object_exist(&file.hash)? {
                        ordered.push(file.hash.clone());
                    }
                }

                for (_, subtree) in tree.get_subtrees() {
                    stack.push(Step::Expand(subtree.hash.clone()));
                }
            }
        }
    }

    Ok(ordered)
}

/// Whether a loose object file is present in the store (independent of any packed copy).
fn loose_object_exists(hash: &str) -> Result<bool, String> {
    let (folder, file_name) = file_utils::get_path_for_object(hash)?;
    let path = std::path::Path::new(&folder).join(file_name);

    std::fs::exists(&path).map_err(|e| format!("Error while checking object {}: {}", hash, e))
}

/// Join a warehouse path key with a child name (the root key is the empty string).
fn join_key(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", path, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::object::loose_object_builder::LooseObjectBuilder;
    use crate::enums::dir_entry_type::DirEntryType;
    use crate::globals::StorageRootScope;
    use crate::model::blob::Blob;
    use crate::model::parcel::Parcel;
    use crate::model::tree_item::TreeItem;
    use std::path::PathBuf;

    /// A fresh warehouse root for one test, entered as the active storage-root scope for its
    /// lifetime (each test runs on its own thread, so scopes never collide).
    struct Scratch {
        _scope: StorageRootScope,
        root: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let root = std::env::temp_dir().join(format!(
                "forklift-prune-test-{}-{}-{}", name, std::process::id(), id
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

    fn store_tree(entries: &[(&str, &str, DirEntryType)]) -> String {
        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        for (name, hash, item_type) in entries {
            tree.add_child(TreeItem::new(name.to_string(), hash.to_string(), *item_type));
        }
        let mut object = LooseObjectBuilder::build_tree(&tree);
        object.store().unwrap();
        object.hash
    }

    /// Store a parcel over `tree_hash` with the given parents. Returns its hash.
    fn store_parcel(tree_hash: &str, parents: Vec<String>) -> String {
        let parcel = Parcel {
            tree_hash: tree_hash.to_string(),
            parents,
            actions: Vec::new(),
            description: Some("p".to_string()),
        };
        let mut object = LooseObjectBuilder::build_parcel(&parcel);
        object.store().unwrap();
        object.hash
    }

    fn loose_exists(hash: &str) -> bool {
        loose_object_exists(hash).unwrap()
    }

    /// A one-parcel warehouse: root holds `src` (with in-scope `api` and out-of-scope `web`)
    /// and an out-of-scope root file `README.md`. `main` points at the parcel.
    struct Fixture {
        parcel: String,
        root_tree: String,
        src_tree: String,
        api_tree: String,
        api_blob: String,
        web_tree: String,
        web_blob: String,
        readme_blob: String,
    }

    fn build_fixture() -> Fixture {
        let api_blob = store_blob("api a v1\n");
        let api_tree = store_tree(&[("a.txt", &api_blob, DirEntryType::Normal)]);

        let web_blob = store_blob("web v1\n");
        let web_tree = store_tree(&[("w.txt", &web_blob, DirEntryType::Normal)]);

        let src_tree = store_tree(&[
            ("api", &api_tree, DirEntryType::Tree),
            ("web", &web_tree, DirEntryType::Tree),
        ]);

        let readme_blob = store_blob("readme v1\n");
        let root_tree = store_tree(&[
            ("src", &src_tree, DirEntryType::Tree),
            ("README.md", &readme_blob, DirEntryType::Normal),
        ]);

        let parcel = store_parcel(&root_tree, Vec::new());
        pallet_utils::set_pallet_head("main", &parcel).unwrap();

        Fixture { parcel, root_tree, src_tree, api_tree, api_blob, web_tree, web_blob, readme_blob }
    }

    #[test]
    fn plan_prune_frees_only_the_pruned_subtree_and_never_the_spine() {
        let _scratch = Scratch::new("frees-only-pruned");
        let f = build_fixture();

        // Prune src/web, keeping src/api in scope.
        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune).unwrap();

        // Exactly the pruned subtree's closure is freed.
        assert_eq!(plan.to_free.len(), 2);
        assert!(plan.to_free.contains(&f.web_tree));
        assert!(plan.to_free.contains(&f.web_blob));
        assert_eq!(plan.still_packed, 0);

        // Ordered children-before-parents (the property that makes a prune resumable, see the
        // module doc): the blob precedes the tree that names it.
        let pos = |h: &str| plan.to_free.iter().position(|x| x == h).unwrap();
        assert!(pos(&f.web_blob) < pos(&f.web_tree),
            "a child must precede its parent in the plan: {:?}", plan.to_free);

        // The spine trees, the parcel and the in-scope content are never freed.
        for kept in [&f.parcel, &f.root_tree, &f.src_tree, &f.api_tree, &f.api_blob] {
            assert!(!plan.to_free.contains(kept), "a retained object must not be freed: {}", kept);
        }

        // A different out-of-scope object that was NOT pruned (the root README) is not freed:
        // prune frees the pruned path, not every out-of-scope object.
        assert!(!plan.to_free.contains(&f.readme_blob),
            "prune must free only the pruned path, not all out-of-scope content");
    }

    #[test]
    fn plan_prune_keeps_content_shared_with_a_retained_path() {
        // Content-addressing: when the pruned subtree shares a blob with a still-fetched path,
        // that blob is retained (freeing it would break the retained path). Only the pruned
        // path's *unique* objects are freed.
        let _scratch = Scratch::new("keeps-shared");

        let shared_blob = store_blob("identical bytes\n");
        let api_tree = store_tree(&[("a.txt", &shared_blob, DirEntryType::Normal)]);
        let web_tree = store_tree(&[("w.txt", &shared_blob, DirEntryType::Normal)]);
        let src_tree = store_tree(&[
            ("api", &api_tree, DirEntryType::Tree),
            ("web", &web_tree, DirEntryType::Tree),
        ]);
        let root_tree = store_tree(&[("src", &src_tree, DirEntryType::Tree)]);
        let parcel = store_parcel(&root_tree, Vec::new());
        pallet_utils::set_pallet_head("main", &parcel).unwrap();

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune).unwrap();

        // The shared blob stays (retained via src/api); only web's unique tree object is freed.
        assert!(!plan.to_free.contains(&shared_blob), "a blob shared with a retained path must not be freed");
        assert!(plan.to_free.contains(&web_tree), "the pruned subtree's unique object is freed");
    }

    #[test]
    fn plan_prune_frees_every_historical_version_of_the_pruned_path() {
        // A prune reclaims the pruned path across the whole history, not just at the head.
        let _scratch = Scratch::new("historical");

        let api_blob = store_blob("api v1\n");
        let api_tree = store_tree(&[("a.txt", &api_blob, DirEntryType::Normal)]);

        let web_v1_blob = store_blob("web v1\n");
        let web_v1_tree = store_tree(&[("w.txt", &web_v1_blob, DirEntryType::Normal)]);
        let src_v1 = store_tree(&[
            ("api", &api_tree, DirEntryType::Tree),
            ("web", &web_v1_tree, DirEntryType::Tree),
        ]);
        let root_v1 = store_tree(&[("src", &src_v1, DirEntryType::Tree)]);
        let p1 = store_parcel(&root_v1, Vec::new());

        let web_v2_blob = store_blob("web v2\n");
        let web_v2_tree = store_tree(&[("w.txt", &web_v2_blob, DirEntryType::Normal)]);
        let src_v2 = store_tree(&[
            ("api", &api_tree, DirEntryType::Tree),
            ("web", &web_v2_tree, DirEntryType::Tree),
        ]);
        let root_v2 = store_tree(&[("src", &src_v2, DirEntryType::Tree)]);
        let p2 = store_parcel(&root_v2, vec![p1.clone()]);
        pallet_utils::set_pallet_head("main", &p2).unwrap();

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune).unwrap();

        // Both versions of the pruned subtree — and both their blobs — are freed.
        for freed in [&web_v1_tree, &web_v1_blob, &web_v2_tree, &web_v2_blob] {
            assert!(plan.to_free.contains(freed), "a historical version must be freed: {}", freed);
        }

        // The in-scope subtree, shared across both revisions, survives.
        assert!(!plan.to_free.contains(&api_tree), "the in-scope subtree must survive");
        assert!(!plan.to_free.contains(&api_blob), "the in-scope blob must survive");
    }

    #[test]
    fn free_objects_deletes_the_plan_and_is_idempotent() {
        let _scratch = Scratch::new("free-and-heal");
        let f = build_fixture();

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune).unwrap();

        assert!(loose_exists(&f.web_tree) && loose_exists(&f.web_blob), "the pruned objects start present");

        let stats = free_objects(&plan.to_free).unwrap();
        assert_eq!(stats.freed, 2, "both pruned objects are freed");
        assert!(!loose_exists(&f.web_tree), "the pruned subtree object is gone");
        assert!(!loose_exists(&f.web_blob), "the pruned blob is gone");

        // The spine and in-scope content survive, and the store still walks.
        assert!(loose_exists(&f.root_tree) && loose_exists(&f.src_tree), "the spine survives the prune");
        assert!(loose_exists(&f.api_tree) && loose_exists(&f.api_blob), "in-scope content survives");
        object_utils::load_tree(&f.src_tree).expect("the spine tree still loads: src/web stays sealed by hash");

        // A resumed prune (objects already gone) is not an error.
        let again = free_objects(&plan.to_free).unwrap();
        assert_eq!(again.freed, 0, "a resumed prune deletes nothing more and does not error");
    }

    #[test]
    fn collect_prune_targets_orders_nested_children_before_every_ancestor() {
        // The ordering property must hold transitively, not just one level deep.
        let _scratch = Scratch::new("nested-order");

        let deep_blob = store_blob("deep v1\n");
        let inner_tree = store_tree(&[("deep.txt", &deep_blob, DirEntryType::Normal)]);
        let web_tree = store_tree(&[("inner", &inner_tree, DirEntryType::Tree)]);

        let api_blob = store_blob("api v1\n");
        let api_tree = store_tree(&[("a.txt", &api_blob, DirEntryType::Normal)]);

        let src_tree = store_tree(&[
            ("api", &api_tree, DirEntryType::Tree),
            ("web", &web_tree, DirEntryType::Tree),
        ]);
        let root_tree = store_tree(&[("src", &src_tree, DirEntryType::Tree)]);
        let parcel = store_parcel(&root_tree, Vec::new());
        pallet_utils::set_pallet_head("main", &parcel).unwrap();

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune).unwrap();

        assert_eq!(plan.to_free.len(), 3, "web_tree, inner_tree and deep_blob");
        let pos = |h: &str| plan.to_free.iter().position(|x| x == h).unwrap();

        assert!(pos(&deep_blob) < pos(&inner_tree), "the leaf blob precedes its parent tree");
        assert!(pos(&inner_tree) < pos(&web_tree), "the inner tree precedes its own parent");
    }

    #[test]
    fn plan_prune_is_resumable_after_a_partial_free_deletes_a_prefix_of_the_plan() {
        // The mechanism a crash-then-retry relies on: a partially-executed free_objects run
        // (which always deletes a PREFIX of the ordered plan — front to back) leaves the store
        // in a state a fresh plan_prune call can re-derive without erroring, finishing exactly
        // what the interruption left behind.
        let _scratch = Scratch::new("resumable");
        let f = build_fixture();

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune).unwrap();
        assert_eq!(plan.to_free.len(), 2, "src/web's tree and blob are the whole plan");

        // Simulate an interruption: free only the first item in plan order (a child, by
        // construction — never a parent, since children always precede parents in the plan).
        let (first, rest) = plan.to_free.split_at(1);
        assert_eq!(first, std::slice::from_ref(&f.web_blob), "the child is freed first, by the plan's own order");
        free_objects(first).unwrap();
        assert!(loose_exists(&f.web_tree), "the parent tree was NOT freed by the simulated interruption");

        // Re-planning over the exact same path must not error on the now-absent blob, and must
        // recompute exactly what the interruption left: just the parent tree.
        let resumed_plan = plan_prune(&["src/web".to_string()], &post_prune)
            .expect("re-planning after a partial free must tolerate the absent object");
        assert_eq!(resumed_plan.to_free, rest, "the resumed plan is exactly what the interruption left");

        let stats = free_objects(&resumed_plan.to_free).unwrap();
        assert_eq!(stats.freed, rest.len());

        // Everything is now gone, and the store is still coherent: the spine still loads and the
        // in-scope content is untouched.
        assert!(!loose_exists(&f.web_tree) && !loose_exists(&f.web_blob), "the resumed prune finished the job");
        object_utils::load_tree(&f.src_tree).expect("the spine still loads after a resumed prune");
        assert!(loose_exists(&f.api_tree) && loose_exists(&f.api_blob), "in-scope content is untouched");
    }
}
