//! Garbage collection of unreferenced objects (DESIGN.html §4.5).
//!
//! A failed or abandoned lift leaves verified objects with no ref pointing at them.
//! The collector marks everything reachable from the GC roots — every pallet head,
//! plus the parked parcels and an in-progress consolidation, when those local states
//! exist — and sweeps the rest, with an mtime grace period protecting the objects of
//! in-flight lifts.
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
use crate::util::{audit_utils, file_utils, merge_utils, object_utils, pallet_utils, park_utils};

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
pub(crate) fn collect_live_set() -> Result<HashSet<String>, String> {
    let mut roots: Vec<String> = Vec::new();

    // Every pallet head across both namespaces — user *and* meta (the office chain is a
    // GC root, or its keys would be collected as unreachable).
    for (_, head) in pallet_utils::all_pallet_refs()? {
        roots.push(head);
    }

    roots.extend(park_utils::read_parked()?);

    if let Some(consolidation) = merge_utils::read_consolidation_state()? {
        roots.push(consolidation.their_head);
    }

    // A re-genesis anchor (§8.7) pins the replaced office chain as attested history;
    // the pin is a GC root, or the attested chain would be collected as unreachable.
    if let Some(anchor) = crate::util::office_utils::read_trust_anchor()? {
        if let Some(adopts) = anchor.adopts {
            roots.push(adopts);
        }
    }

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
}
