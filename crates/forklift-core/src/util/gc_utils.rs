//! Garbage collection of unreferenced objects (DESIGN.html §4.5).
//!
//! A failed or abandoned lift leaves verified objects with no ref pointing at them.
//! The collector marks everything reachable from the GC roots — every pallet head,
//! plus every bay's parked parcels and in-progress consolidation, when those local
//! states exist — and sweeps the rest, with an mtime grace period protecting the
//! objects of in-flight lifts.
//!
//! The mark walk is presence-tolerant: a store can legitimately hold only some of a
//! parcel's paths — an out-of-scope subtree is sealed by a hash the signed parcel commits
//! but may never have been fetched — so the walk marks an absent subtree's hash live and
//! skips the descent it cannot make, rather than erroring. This is a tolerance, not a new
//! collection policy: an object still reachable from a head is live and kept, always,
//! including one a bay merely narrowed its materialization scope away from (that object is
//! still reachable history). Freeing objects that a store narrowed away but that remain
//! reachable is a separate, deliberate, destructive operation — never something this
//! reachability sweep does.

use std::collections::{HashSet, VecDeque};
use std::time::SystemTime;
use crate::util::{audit_utils, bay_utils, file_utils, object_utils};
// Test-only: `collect_live_set`'s own pallet-head root gathering moved into the shared
// `recovery_utils::gc_root_sources` (F8, PR #120 round 2), so this module's non-test code no
// longer calls `pallet_utils` directly — only its own test fixtures still build pallet refs by
// hand.
#[cfg(test)]
use crate::util::pallet_utils;

/// What a collection did.
pub struct GcStats {
    /// Objects examined.
    pub scanned: usize,

    /// Unreferenced objects deleted (their signature sidecars ride along).
    pub deleted: usize,

    /// Unreferenced objects kept because they are younger than the grace period
    /// (an in-flight lift may still be uploading their reachers).
    pub kept_recent: usize,
}

/// Collect the garbage of the active warehouse: delete every object no GC root
/// reaches, unless it was modified within the last `grace_seconds`.
///
/// # Arguments
/// * `grace_seconds` - The grace period; unreferenced objects younger than this stay.
///
/// # Returns
/// * `Ok(GcStats)` - What happened.
/// * `Err(String)` - If the live set could not be computed (nothing is deleted then)
///                   or a deletion failed.
pub fn collect_garbage(grace_seconds: u64) -> Result<GcStats, String> {
    let live = collect_live_set()?;

    let objects_root = std::path::PathBuf::from(file_utils::get_path_objects_root());
    let now = SystemTime::now();

    let mut stats = GcStats { scanned: 0, deleted: 0, kept_recent: 0 };

    let folders = std::fs::read_dir(&objects_root)
        .map_err(|e| format!("Error while reading the objects folder: {}", e))?;

    for folder in folders {
        let folder = folder.map_err(|e| format!("Error while listing the objects folder: {}", e))?;

        if !folder.path().is_dir() {
            continue;
        }

        let prefix = folder.file_name().to_string_lossy().to_string();

        // The pack folder holds packed objects, not loose ones; it is not a hash fan-out
        // folder, so skip it — its `.pack`/`.idx` files are not garbage. (Collecting inside
        // packs is a repack concern, not this loose sweep.)
        if prefix.len() != file_utils::OBJECT_HASH_FOLDER_PATH_CHARACTERS
            || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }

        let files = std::fs::read_dir(folder.path())
            .map_err(|e| format!("Error while reading an objects folder: {}", e))?;

        for file in files {
            let file = file.map_err(|e| format!("Error while listing an objects folder: {}", e))?;
            let name = file.file_name().to_string_lossy().to_string();

            // Sidecars are swept with their object, never on their own.
            if name.ends_with(".sig") {
                continue;
            }

            stats.scanned += 1;

            let hash = format!("{}{}", prefix, name);

            if live.contains(&hash) {
                continue;
            }

            let age_is_protected = file.metadata()
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|modified| now.duration_since(modified).ok())
                .map(|age| age.as_secs() < grace_seconds)
                // An unreadable mtime protects the object: never delete on doubt.
                .unwrap_or(true);

            if age_is_protected {
                stats.kept_recent += 1;
                continue;
            }

            std::fs::remove_file(file.path())
                .map_err(|e| format!("Error while deleting object {}: {}", hash, e))?;

            let sidecar = file.path().with_file_name(format!("{}.sig", name));

            if sidecar.exists() {
                std::fs::remove_file(&sidecar)
                    .map_err(|e| format!("Error while deleting the sidecar of {}: {}", hash, e))?;
            }

            stats.deleted += 1;
        }
    }

    Ok(stats)
}

/// Compute the live set: every parcel, tree and blob reachable from the GC roots.
/// Shared with `pack_utils::compact` (a repack keeps exactly the live set).
///
/// The walk is presence-tolerant: a subtree (or blob) can be absent — sealed by a hash a signed
/// parcel commits, but never fetched into this warehouse (a store can hold only the paths a
/// workspace materializes) — and the walk marks its hash live, skips the descent it cannot make,
/// and finishes (see the mark loop below). This keeps `gc`/repack working on a store that holds
/// only some of its paths. It changes nothing about *what* is collected: an object still
/// reachable from a head is live and kept, always. In particular, an object a bay narrowed its
/// scope away from is still ordinary reachable history — reachable from a pallet head, therefore
/// live, therefore never freed here. Reclaiming disk for narrowed-away content is a separate,
/// deliberate, destructive operation; it is never something this reachability sweep does.
///
/// **The office chain is the one root set that must be *loadable*, not merely tolerated-if-
/// absent.** The tolerance above is about descending into an already-collected root's tree/blob
/// closure — an ordinary pallet head that itself cannot be read is simply skipped by the
/// reachability walk below it (`audit_utils::collect_reachable_present` treats an absent head as
/// a gap, contributing nothing, never an error). Computing the *root set itself* is different:
/// every revoked key's `distrust_boundary` pin can only be found by reading the office pallet's
/// own head parcel, tracked-keys tree and key-record blobs (`office_utils::
/// collect_trust_pin_roots`), and this call passes it `TrustPinReadPolicy::FailClosed` — an
/// unreadable office record aborts `collect_live_set` outright rather than silently returning a
/// pin list gc cannot prove complete, because under-counting here means sweeping a hash a signed
/// revocation still names. `forklift heal`'s own root collection
/// (`recovery_utils::collect_walk_roots`) needs the opposite call for the same read — see that
/// function's own doc comment for why a sweep and a non-deleting recovery walk are allowed to
/// differ here the way they already differ on an unreadable bay (`bay_utils::BayReadPolicy`).
pub(crate) fn collect_live_set() -> Result<HashSet<String>, String> {
    // Pallet heads (both namespaces), bay-scoped parcel roots, and trust-pin roots — the three
    // sources this function shares, hash for hash, with `recovery_utils::collect_walk_roots`
    // (F8, PR #120 round 2: before this, both callers open-coded the same three-source assembly
    // by hand — see `recovery_utils::gc_root_sources`'s own doc comment for why that duplication
    // was worth closing, and for the superset invariant this list must stay a subset of). GC does
    // not need the pin/ordinary distinction `collect_walk_roots` cares about (an absent root, pin
    // or ordinary, is just a gap to `audit_utils::collect_reachable_present` below — see this
    // function's own doc comment), so both lists are flattened into one plain `roots` bag.
    //
    // `FailClosed` for both policies, unconditionally — this feeds a sweep that deletes objects,
    // so an incompletely known live set must abort rather than under-count. See
    // `bay_utils::BayReadPolicy`'s and `office_utils::TrustPinReadPolicy`'s own doc comments for
    // the full reasoning, and why `forklift heal`'s own call site is allowed to differ.
    let bay_dirs = bay_utils::all_bay_state_dirs()?;
    let sources = crate::util::recovery_utils::gc_root_sources(
        &bay_dirs,
        bay_utils::BayReadPolicy::FailClosed,
        crate::util::office_utils::TrustPinReadPolicy::FailClosed,
    )?;

    let mut roots: Vec<String> = sources.parcels;
    roots.extend(sources.pin_parcels.into_iter().map(|(hash, _policy)| hash));

    let parcels = audit_utils::collect_reachable_present(&roots)?;

    let mut live: HashSet<String> = HashSet::new();
    let mut tree_queue: VecDeque<String> = VecDeque::new();

    for parcel_hash in &parcels {
        live.insert(parcel_hash.clone());
        tree_queue.push_back(object_utils::load_parcel(parcel_hash)?.tree_hash);
    }

    while let Some(tree_hash) = tree_queue.pop_front() {
        if !live.insert(tree_hash.clone()) {
            continue;
        }

        // Presence-tolerant descent. A subtree object can be legitimately absent — sealed by a
        // hash committed in a signed parcel's spine tree, but never fetched into this warehouse.
        // Its hash was inserted into `live` on the line above, *before* this check, so the seal
        // is never collected; we simply cannot descend into bytes we do not hold. The tolerance
        // is presence-based only: gc cannot tell a deliberately-unfetched object apart from one
        // genuinely lost to corruption — both read as absent here — and does not try to. `audit`
        // is what re-proves integrity (it re-hashes the trees it holds and flags an object that
        // should be present but is not); gc's job is to free provably-unreachable garbage without
        // ever touching a live hash, and it stays correct whichever kind of absence this is.
        //
        // The store invariant that makes skipping the descent safe: if a subtree object is
        // absent, nothing beneath it is present here either (a warehouse never holds a child
        // without its parent tree). So this marks the boundary hash live and stops — and there is
        // nothing below it on disk for the sweep to see, let alone wrongly collect. Durable-
        // before-destructive holds: the sealed boundary hash stays live, and nothing beneath it
        // is ever left unmarked in a way that matters, because nothing beneath it exists locally
        // to mark. A present-but-corrupt object is the other case: it *is* present, so the load
        // below runs and fails, and `collect_garbage` deletes nothing on that error (safe, loud).
        // This is the same presence tolerance `collect_reachable_present` already applies to
        // parcels above, extended one level down to the tree closure.
        //
        // This invariant is steady-state, not instantaneous: an in-flight or interrupted fetch
        // can transiently violate it, since objects can arrive out of order and a child (a blob)
        // can land before its parent tree. That leaves a *present* orphan this walk never marks
        // live — but `collect_garbage`'s mtime grace period is what makes that safe: a young
        // orphan is kept while the fetch might still resume and complete it, and only one
        // abandoned past the grace period ages into legitimate, re-fetchable garbage.
        if !file_utils::does_object_exist(&tree_hash)? {
            continue;
        }

        let tree = object_utils::load_tree(&tree_hash)?;

        for (_, file) in tree.get_files() {
            // A blob reference gets the same treatment for free: its hash is recorded live here
            // and its bytes are never loaded by this walk, so an absent (sealed-but-unfetched)
            // blob is tolerated with no check at all — marked, and skipped by never being read.
            live.insert(file.hash.clone());

            // A chunked file's hash names a recipe; its chunks are reachable **only** through the
            // recipe (they are never referenced by a tree directly), so a walk that stopped at the
            // recipe hash would leave every chunk unmarked — and a later loose-object sweep would
            // collect them all, silently making the file unmaterializable (the B1 data-loss bug).
            // The `*Chunked` tree entry type is what lets this walk decide to descend here with no
            // speculative load on a plain entry: dispatch on the type, then mark the chunks live.
            if file.item_type.is_chunked() {
                mark_recipe_chunks_live(&file.hash, &mut live)?;
            }
        }

        for (_, subtree) in tree.get_subtrees() {
            tree_queue.push_back(subtree.hash.clone());
        }
    }

    Ok(live)
}

/// Mark every chunk of a chunked file's recipe live, presence-tolerantly.
///
/// The recipe hash itself is already marked live by the caller; this descends it to reach the
/// chunk hashes. Tolerance mirrors the subtree descent exactly, one level deeper: an **absent**
/// recipe (out of scope in a sparse warehouse, never fetched — and by the store invariant its
/// chunks are absent too) is skipped, since we cannot descend bytes we do not hold, and its
/// already-live hash is never collected. A **present** recipe is loaded (which re-hashes it on the
/// content-addressed read, so a corrupt one fails here and `collect_garbage` then deletes nothing
/// — safe and loud) and each chunk hash is marked live, tolerating an absent chunk with no read.
///
/// # Arguments
/// * `recipe_hash` - The hash of the recipe (a chunked file's tree-entry hash).
/// * `live`        - The live set to mark chunk hashes into.
///
/// # Returns
/// * `Ok(())`      - The chunks were marked (or the recipe was absent and tolerated).
/// * `Err(String)` - If a present recipe could not be loaded (corrupt/unreadable).
fn mark_recipe_chunks_live(recipe_hash: &str, live: &mut HashSet<String>) -> Result<(), String> {
    if !file_utils::does_object_exist(recipe_hash)? {
        return Ok(());
    }

    let recipe = object_utils::load_recipe(recipe_hash)?;

    for chunk in &recipe.chunks {
        live.insert(chunk.hash.clone());
    }

    Ok(())
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
    use std::path::{Path, PathBuf};

    /// A fresh warehouse root for one test, entered as the active storage-root scope for its
    /// lifetime. Each test gets its own directory (and its own thread, `cargo test`'s default),
    /// so parallel tests never see each other's objects.
    struct Scratch {
        _scope: StorageRootScope,
        root: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let root = std::env::temp_dir().join(format!(
                "forklift-gc-test-{}-{}-{}", name, std::process::id(), id
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

    /// Store a blob and return its hash.
    fn store_blob(content: &str) -> String {
        let mut object = LooseObjectBuilder::build_blob(&Blob { content: content.as_bytes().to_vec() });
        object.store().unwrap();
        object.hash
    }

    /// Store a chunk object and return its hash.
    fn store_chunk(content: &[u8]) -> String {
        use crate::model::chunk::Chunk;
        let mut object = LooseObjectBuilder::build_chunk(&Chunk { content: content.to_vec() });
        object.store().unwrap();
        object.hash
    }

    /// Store a recipe over the given chunks (its `total_size` is the sum of the sizes, so it
    /// passes the structural check at load) and return its hash.
    fn store_recipe(chunks: &[(String, u64)]) -> String {
        use crate::model::recipe::{Recipe, RecipeChunk};
        let total_size = chunks.iter().map(|(_, size)| *size).sum();
        let recipe = Recipe {
            // gc never verifies `content_hash`; any valid 64-hex value is fine here.
            content_hash: "0".repeat(64),
            total_size,
            chunks: chunks.iter().map(|(hash, size)| RecipeChunk { hash: hash.clone(), size: *size }).collect(),
        };
        let mut object = LooseObjectBuilder::build_recipe(&recipe);
        object.store().unwrap();
        object.hash
    }

    /// Build a one-level tree from `(name, hash, type)` entries, store it, return its hash.
    fn store_tree(entries: &[(&str, &str, DirEntryType)]) -> String {
        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        for (name, hash, item_type) in entries {
            tree.add_child(TreeItem::new(name.to_string(), hash.to_string(), *item_type));
        }
        let mut object = LooseObjectBuilder::build_tree(&tree);
        object.store().unwrap();
        object.hash
    }

    /// Store a parentless parcel over `tree_hash` and point `main` at it. Returns its hash.
    fn store_root_parcel(tree_hash: &str) -> String {
        let parcel = Parcel {
            tree_hash: tree_hash.to_string(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("base".to_string()),
        };
        let mut object = LooseObjectBuilder::build_parcel(&parcel);
        object.store().unwrap();
        pallet_utils::set_pallet_head("main", &object.hash).unwrap();
        object.hash
    }

    /// The loose-object path of `hash` (the on-disk fan-out the store uses).
    fn loose_path(hash: &str) -> PathBuf {
        let (folder, file_name) = file_utils::get_path_for_object(hash).unwrap();
        Path::new(&folder).join(file_name)
    }

    /// Delete a loose object from the store, simulating an out-of-scope object that a sparse fetch
    /// would never have downloaded. Panics if it was not there (keeps the fixture honest).
    fn delete_object(hash: &str) {
        std::fs::remove_file(loose_path(hash)).expect("the object to simulate-absent existed");
    }

    /// A warehouse whose `main` head commits an in-scope subtree (`src/api`), a sibling subtree
    /// (`src/web`) and a root file (`README.md`). Returns the object hashes, so a test can delete
    /// the out-of-scope ones and assert on the boundary. `src` is the spine that both an in-scope
    /// and the out-of-scope subtree hang off.
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

        let parcel = store_root_parcel(&root_tree);

        Fixture {
            parcel, root_tree, src_tree, api_tree, api_blob,
            web_tree, web_blob, readme_blob,
        }
    }

    #[test]
    fn live_set_seals_an_absent_subtree_and_never_errors() {
        let _scratch = Scratch::new("live-set-seal");
        let f = build_fixture();

        // Make the out-of-scope content unreadable, the way a sparsely-fetched store would hold it:
        // the sibling subtree object, the blob beneath it, and the out-of-scope root file's blob.
        delete_object(&f.web_tree);
        delete_object(&f.web_blob);
        delete_object(&f.readme_blob);

        // The mark walk completes rather than erroring on the absent subtree (today's code errors).
        let live = collect_live_set().expect("the live-set walk must tolerate the absent subtree");

        // Everything present and reachable is live.
        for hash in [&f.parcel, &f.root_tree, &f.src_tree, &f.api_tree, &f.api_blob] {
            assert!(live.contains(hash), "a present, reachable object must be live: {}", hash);
        }

        // The store-invariant edge: the parent spine tree (`src`) claims `src/web` by hash, but
        // the object is absent — gc *marks* that boundary hash live (the seal, so it can never be
        // collected) and *skips* the descent it cannot make.
        assert!(live.contains(&f.web_tree), "the sealed boundary subtree hash must stay live");

        // An absent blob referenced by a *present* tree (the root's out-of-scope `README.md`) is
        // marked live without ever being loaded.
        assert!(live.contains(&f.readme_blob), "an absent blob under a present tree is still marked live");

        // The blob *beneath* the sealed boundary is never reached (the boundary was not descended)
        // — correct, and by the store invariant it is absent locally anyway.
        assert!(!live.contains(&f.web_blob), "nothing beneath a sealed boundary is individually marked");
    }

    #[test]
    fn gc_collects_garbage_and_never_touches_the_sealed_spine() {
        let _scratch = Scratch::new("gc-tolerates-absence");
        let f = build_fixture();

        // A real piece of garbage: a loose blob no ref reaches.
        let garbage = store_blob("unreferenced garbage\n");
        assert!(loose_path(&garbage).exists());

        // Simulate the sparse store: the out-of-scope objects are absent.
        delete_object(&f.web_tree);
        delete_object(&f.web_blob);
        delete_object(&f.readme_blob);

        // gc completes (no error on the absent, still-reachable subtree) and collects the garbage.
        let stats = collect_garbage(0).expect("gc must tolerate the absent subtree and still sweep");
        assert_eq!(stats.deleted, 1, "exactly the one unreferenced object is collected");
        assert!(!loose_path(&garbage).exists(), "the garbage object must be gone");

        // The sealed spine and every present in-scope object survive untouched.
        for hash in [&f.parcel, &f.root_tree, &f.src_tree, &f.api_tree, &f.api_blob] {
            assert!(loose_path(hash).exists(), "a live object must survive gc: {}", hash);
        }

        // gc did not resurrect the deliberately-absent objects, and the store still reads back: a
        // subsequent live-set walk (what stack/lift build on) still succeeds and the spine loads.
        assert!(!loose_path(&f.web_tree).exists(), "gc must not recreate an absent object");
        let live = collect_live_set().expect("the store is still walkable after gc");
        assert!(live.contains(&f.root_tree));
        object_utils::load_tree(&f.root_tree).expect("the root spine tree still loads after gc");
        object_utils::load_tree(&f.api_tree).expect("the in-scope subtree still loads after gc");
    }

    #[test]
    fn gc_keeps_live_chunks_and_collects_orphan_chunks() {
        // The B1 fix: a chunk-aware gc descends a live recipe and marks every chunk live, so a
        // live chunked file's chunks survive; a chunk reachable through no recipe is ordinary
        // garbage and is collected.
        let _scratch = Scratch::new("gc-chunks");

        let chunk_a = store_chunk(b"chunk a content");
        let chunk_b = store_chunk(b"chunk b content");
        let recipe = store_recipe(&[(chunk_a.clone(), 15), (chunk_b.clone(), 15)]);

        // A tree entry of the chunked type points at the recipe; a parcel commits it on `main`.
        let root_tree = store_tree(&[("big.bin", &recipe, DirEntryType::NormalChunked)]);
        let parcel = store_root_parcel(&root_tree);

        // An orphan chunk: a valid chunk object no recipe references.
        let orphan = store_chunk(b"orphan chunk no recipe reaches me");

        let stats = collect_garbage(0).expect("gc runs");

        // The orphan chunk (and nothing live) is collected.
        assert_eq!(stats.deleted, 1, "exactly the orphan chunk is collected");
        assert!(!loose_path(&orphan).exists(), "the orphan chunk must be gone");

        // Every object reachable through the recipe survives.
        for hash in [&parcel, &root_tree, &recipe, &chunk_a, &chunk_b] {
            assert!(loose_path(hash).exists(), "a live object must survive gc: {}", hash);
        }
    }

    #[test]
    fn gc_tolerates_an_absent_recipe_the_way_it_tolerates_an_absent_subtree() {
        // Presence tolerance one level deeper: an out-of-scope (sparse) recipe is absent, and by
        // the store invariant its chunks are absent too. The walk marks the recipe hash live and
        // stops, never erroring — exactly like the sealed-subtree tolerance.
        let _scratch = Scratch::new("gc-absent-recipe");

        let chunk_a = store_chunk(b"a");
        let chunk_b = store_chunk(b"bb");
        let recipe = store_recipe(&[(chunk_a.clone(), 1), (chunk_b.clone(), 2)]);
        let root_tree = store_tree(&[("big.bin", &recipe, DirEntryType::NormalChunked)]);
        let _parcel = store_root_parcel(&root_tree);

        // Simulate the sparse store: the recipe and its chunks were never fetched.
        delete_object(&recipe);
        delete_object(&chunk_a);
        delete_object(&chunk_b);

        // The walk completes (no error) and the sealed recipe hash stays live.
        let live = collect_live_set().expect("an absent recipe must be tolerated, not error");
        assert!(live.contains(&recipe), "the sealed recipe hash must stay live");
        assert!(!live.contains(&chunk_a), "nothing beneath an absent recipe is individually marked");
    }

    #[test]
    fn gc_errors_and_deletes_nothing_when_a_present_object_is_corrupt() {
        // The other side of presence-based tolerance: a *present* but corrupt tree is not an
        // absence — the load fails, and gc deletes nothing (durable-before-destructive).
        let _scratch = Scratch::new("gc-corrupt-present");
        let f = build_fixture();
        let garbage = store_blob("garbage\n");

        // Corrupt a present, reachable tree object in place (wrong bytes for its hash), leaving it
        // present so `does_object_exist` still reports it — distinguishing corruption from absence.
        std::fs::write(loose_path(&f.api_tree), zstd::encode_all(&b"not a tree"[..], 0).unwrap()).unwrap();

        let result = collect_garbage(0);
        assert!(result.is_err(), "a present-but-corrupt object must surface as an error, not be tolerated");
        assert!(loose_path(&garbage).exists(), "gc must delete nothing when the live set cannot be computed");
    }

    /// (v) The gc half of the bay-scoping bug: `collect_live_set` used to read only the active
    /// bay's `parked`/`consolidation` state (`bay_root()`, which with no active bay resolves to
    /// `forklift_root()` itself — never a *named* bay's state dir) — a parcel parked in a
    /// different bay was invisible to gc, and `collect_garbage`/`compact --all` run from the
    /// main tree deleted its otherwise-unreferenced objects. Present-tense data loss (worse than
    /// the recovery walk's under-count: nothing has to crash first), not merely a stale taint
    /// record. Pins the per-bay loop over `bay_utils::all_bay_state_dirs` in `collect_live_set`.
    /// Red without it: the blob below is reachable from **no** pallet head — only bay "b"'s
    /// parked file (planted directly, by path, mirroring how `collect_live_set` itself only ever
    /// reads paths) — so a pre-fix `collect_garbage(0)` run from the main scope deletes it.
    #[test]
    fn gc_keeps_an_object_referenced_only_by_another_bays_parked_parcel() {
        let _scratch = Scratch::new("gc-bay-scoped-parked");

        // A real parcel over a real tree/blob, stored but reachable from no pallet head at all —
        // its only reference is bay "b"'s parked file.
        let blob = store_blob("only bay b's parked parcel reaches me\n");
        let tree = store_tree(&[("f.txt", &blob, DirEntryType::Normal)]);
        let parcel = {
            let parcel = Parcel { tree_hash: tree.clone(), parents: Vec::new(), actions: Vec::new(), description: None };
            let mut object = LooseObjectBuilder::build_parcel(&parcel);
            object.store().unwrap();
            object.hash
        };

        let bay_b_dir = bay_utils::bay_state_dir("b");
        std::fs::create_dir_all(&bay_b_dir).unwrap();
        std::fs::write(bay_b_dir.join("parked"), format!("{}\n", parcel)).unwrap();

        // Run from the MAIN scope (no active bay) — gc must still see bay "b"'s parked parcel.
        let stats = collect_garbage(0).expect("gc must tolerate another bay's parked parcel");

        assert_eq!(stats.deleted, 0, "nothing reachable only through bay b's parked parcel may be deleted");
        assert!(loose_path(&blob).exists(), "a blob referenced only by another bay's parked parcel must survive gc");
        assert!(loose_path(&tree).exists(), "a tree referenced only by another bay's parked parcel must survive gc");
        assert!(loose_path(&parcel).exists(), "a parcel parked only in another bay must survive gc");
    }

    /// Regression lock for the fail-closed contract on `bay_utils::collect_bay_scoped_parcel_roots`
    /// (see its doc comment): a malformed `parked` file in a *non-active* bay must make
    /// `collect_garbage`/`collect_live_set` fail outright and delete nothing — never silently
    /// skip the unreadable bay and reclaim garbage anyway, which would re-open the exact
    /// under-counting bug the bay-scope fix closed (a bay's still-live object swept because its
    /// ref source could not be proven to exclude it). Red if `read_parked_in`'s `?` in
    /// `collect_bay_scoped_parcel_roots` (crates/forklift-core/src/util/bay_utils.rs) were ever
    /// changed to skip-and-continue on an `Err` instead of propagating it.
    #[test]
    fn gc_fails_closed_and_deletes_nothing_on_an_unreadable_bay_parked_file() {
        let _scratch = Scratch::new("gc-bay-unreadable-parked");

        // A real piece of garbage that a skip-and-continue bug would wrongly let gc collect.
        let garbage = store_blob("would be wrongly collected if the bad bay were skipped\n");
        assert!(loose_path(&garbage).exists());

        // Bay "b"'s `parked` file is malformed (not 64 hex chars) — `read_parked_in` errors on it.
        let bay_b_dir = bay_utils::bay_state_dir("b");
        std::fs::create_dir_all(&bay_b_dir).unwrap();
        std::fs::write(bay_b_dir.join("parked"), b"not-a-valid-hash\n").unwrap();

        let live_set_result = collect_live_set();
        assert!(live_set_result.is_err(),
            "an unreadable/malformed bay ref source must fail the live-set computation, not be skipped");

        let gc_result = collect_garbage(0);
        assert!(gc_result.is_err(), "gc must refuse rather than reclaim when a bay's ref source is unreadable");
        assert!(loose_path(&garbage).exists(),
            "gc must delete nothing when the live set could not be computed, even unrelated garbage");
    }

    /// Finding 6 (round 1, PR #120): `collect_live_set` must ERROR, not silently narrow the
    /// trust-pin roots, when the office record backing every revoked key's `distrust_boundary`
    /// pin cannot be read — see this module's own doc comment (above `collect_live_set`) on why
    /// the office chain is the one root set this sweep requires to be loadable, unlike an
    /// ordinary pallet head. Fixture: a trust anchor exists (so `collect_trust_pin_roots`
    /// proceeds past the "no anchor, no roots" early return) but the office pallet ref names a
    /// parcel hash never actually stored — the same ref-advanced-before-parcel-durable crash
    /// window `forklift heal` exists to recover from (Finding 1's own fixture, on the tolerant
    /// side of this exact split).
    #[test]
    fn collect_live_set_errors_when_the_office_head_object_is_absent() {
        let _scratch = Scratch::new("gc-office-head-absent");

        crate::util::office_utils::write_trust_anchor(&crate::util::office_utils::TrustAnchor {
            genesis: "0".repeat(64),
            enabled_at: 0,
            boundary: Vec::new(),
            prior_genesis: None,
            adopts: None,
        }).unwrap();

        let never_stored_office_head = "f".repeat(64);
        pallet_utils::set_meta_pallet_head(
            crate::util::office_utils::OFFICE_PALLET_NAME, &never_stored_office_head
        ).unwrap();

        let result = collect_live_set();
        assert!(result.is_err(),
            "an unreadable office record must fail collect_live_set closed, not silently narrow \
             the trust-pin roots");
    }

    /// Finding 5 (round 2, PR #120): `office_utils::collect_trust_pin_roots` only ever consumes a
    /// key's own `distrust_boundary` — never a user record — so it must read the office record's
    /// **keys** subtree alone, not the full `read_office_state` (users and keys both). Before the
    /// fix, a malformed user record (bad role/identifier/enrolled_at/class — any of the strict
    /// field errors `parse_user_record` can raise) aborted `read_office_state` outright, which
    /// under `TrustPinReadPolicy::FailClosed` aborted `collect_live_set` too, even though that
    /// same record pins nothing this sweep needs at all.
    ///
    /// Fixture: one office parcel with a user record missing its required `role` field (malformed)
    /// alongside a well-formed key record naming a real `distrust_boundary` — `collect_live_set`
    /// must still succeed (and still find the distrust-boundary pin), never trip on the sibling
    /// user record it never needed to read.
    #[test]
    fn collect_live_set_succeeds_past_a_malformed_user_record_when_only_keys_are_readable() {
        let _scratch = Scratch::new("gc-malformed-user-readable-keys");

        // A parcel this sweep's own trust-pin walk must find live: reachable ONLY through the
        // valid key's `distrust_boundary`, unreferenced by any pallet ref.
        let pinned_tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        let mut pinned_tree_object = LooseObjectBuilder::build_tree(&pinned_tree);
        pinned_tree_object.store().unwrap();
        let mut pinned_object = LooseObjectBuilder::build_parcel(&Parcel {
            tree_hash: pinned_tree_object.hash, parents: Vec::new(), actions: Vec::new(), description: None,
        });
        pinned_object.store().unwrap();
        let pinned_hash = pinned_object.hash;

        let malformed_user_toml =
            "identifier = \"op@broken\"\nenrolled_at = 1\nidentity_root = \"r\"\n"; // no "role"
        let valid_key_toml = format!(
            "key_id = \"test-key-1\"\n\
             operator = \"op@x\"\n\
             public_key = \"deadbeef\"\n\
             issued_at = 1\n\
             distrust_boundary = [\"{}\"]\n\
             authorized_by = \"test-key-1\"\n\
             endorsement = \"ee\"\n\
             proof_of_possession = \"pp\"\n",
            pinned_hash
        );

        let user_blob = store_blob(&malformed_user_toml);
        let key_blob = store_blob(&valid_key_toml);

        let mut users_tree = TreeItem::new("users".to_string(), String::new(), DirEntryType::Tree);
        users_tree.add_child(TreeItem::new("broken.toml".to_string(), user_blob, DirEntryType::Normal));
        let mut users_object = LooseObjectBuilder::build_tree(&users_tree);
        users_object.store().unwrap();
        users_tree.hash = users_object.hash;

        let mut keys_tree = TreeItem::new("keys".to_string(), String::new(), DirEntryType::Tree);
        keys_tree.add_child(TreeItem::new("key1.toml".to_string(), key_blob, DirEntryType::Normal));
        let mut keys_object = LooseObjectBuilder::build_tree(&keys_tree);
        keys_object.store().unwrap();
        keys_tree.hash = keys_object.hash;

        let mut tracked_tree = TreeItem::new("tracked".to_string(), String::new(), DirEntryType::Tree);
        tracked_tree.add_child(users_tree);
        tracked_tree.add_child(keys_tree);
        let mut tracked_object = LooseObjectBuilder::build_tree(&tracked_tree);
        tracked_object.store().unwrap();
        tracked_tree.hash = tracked_object.hash;

        let mut forklift_tree = TreeItem::new(".forklift".to_string(), String::new(), DirEntryType::Tree);
        forklift_tree.add_child(tracked_tree);
        let mut forklift_object = LooseObjectBuilder::build_tree(&forklift_tree);
        forklift_object.store().unwrap();
        forklift_tree.hash = forklift_object.hash;

        let mut root_tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        root_tree.add_child(forklift_tree);
        let mut root_object = LooseObjectBuilder::build_tree(&root_tree);
        root_object.store().unwrap();

        let office_parcel = Parcel {
            tree_hash: root_object.hash, parents: Vec::new(), actions: Vec::new(),
            description: Some("office with a malformed user record".to_string()),
        };
        let mut office_parcel_object = LooseObjectBuilder::build_parcel(&office_parcel);
        office_parcel_object.store().unwrap();

        crate::util::office_utils::write_trust_anchor(&crate::util::office_utils::TrustAnchor {
            genesis: "0".repeat(64),
            enabled_at: 0,
            boundary: Vec::new(),
            prior_genesis: None,
            adopts: None,
        }).unwrap();
        pallet_utils::set_meta_pallet_head(
            crate::util::office_utils::OFFICE_PALLET_NAME, &office_parcel_object.hash
        ).unwrap();

        // Sanity: the malformed user record really would abort the FULL office-state reader.
        assert!(crate::util::office_utils::read_office_state().is_err(),
            "sanity: the fixture's user record must actually be malformed");

        let live = collect_live_set().expect(
            "a malformed user record must never abort collect_live_set — the trust-pin walk only \
             needs the office record's keys, never its users"
        );
        assert!(live.contains(&pinned_hash),
            "the valid key's own distrust_boundary pin must still be found live: {:?}", live);
    }

    /// DESIGN.html §5.0 D item 10, finding #3: a `WriteBatch`-staged write that never reaches
    /// `finish()` (the exact shape a SIGKILL mid-`load` leaves behind, now that a whole walk's
    /// blobs share one batch instead of each paying its own immediate barrier) leaves a stray
    /// `<hash>.tmp<pid>-<n>` temp in an object fan-out folder. `compact` never touches it
    /// (`pack_utils::enumerate_loose_objects` explicitly skips any name containing `.tmp`) — this
    /// pins that `gc`'s ordinary reachability sweep does the reclaiming instead, once the temp
    /// ages past the grace period: `collect_garbage` does not pattern-match on `.tmp` at all, it
    /// just treats any non-`.sig`, unreferenced file sitting in an object folder as garbage, so a
    /// stranded batch temp is swept exactly like any other abandoned loose write, no special
    /// casing required.
    #[test]
    fn gc_sweeps_a_stranded_write_batch_temp_past_the_grace_period() {
        let _scratch = Scratch::new("gc-stranded-temp");

        let hash = "a".repeat(64);
        let (folder, file_name) = file_utils::get_path_for_object(&hash).unwrap();
        std::fs::create_dir_all(&folder).unwrap();

        // Mirrors `file_utils::temp_path_for`'s naming exactly (`<final-name>.tmp<pid>-<n>`)
        // without going through `WriteBatch` itself — a `WriteBatch` still owned by a live
        // process cleans up after itself on `Drop`; this simulates what a hard kill leaves
        // behind instead, when nothing ever runs `Drop` at all.
        let stray_temp = Path::new(&folder).join(format!("{}.tmp{}-0", file_name, std::process::id()));
        std::fs::write(&stray_temp, b"partial content, never finished").unwrap();
        assert!(stray_temp.exists(), "the stray temp must exist before gc runs");

        let stats = collect_garbage(0).expect("gc must tolerate an unrecognized loose file");
        assert!(!stray_temp.exists(),
            "gc's ordinary reachability sweep must remove a stranded WriteBatch temp past the grace period");
        assert_eq!(stats.deleted, 1);
    }

    /// The other half of the finding #3 claim above: a temp stranded by a *very recent* kill is
    /// exactly as protected as any other young, unreferenced object — `gc` cannot (and does not
    /// try to) tell "an in-flight write's temp, still needed" apart from "a stranded batch temp,
    /// safe to reclaim" any better than it can for an ordinary loose object, and applies the same
    /// mtime grace period to both.
    #[test]
    fn gc_protects_a_stranded_write_batch_temp_within_the_grace_period() {
        let _scratch = Scratch::new("gc-stranded-temp-protected");

        let hash = "b".repeat(64);
        let (folder, file_name) = file_utils::get_path_for_object(&hash).unwrap();
        std::fs::create_dir_all(&folder).unwrap();

        let stray_temp = Path::new(&folder).join(format!("{}.tmp{}-0", file_name, std::process::id()));
        std::fs::write(&stray_temp, b"partial content, never finished").unwrap();

        let stats = collect_garbage(3600).expect("gc must tolerate an unrecognized loose file");
        assert!(stray_temp.exists(), "a recently stranded temp must survive gc within the grace period");
        assert_eq!(stats.kept_recent, 1);
    }
}
