//! Deleting what an applied redaction removes.
//!
//! A redaction applies once nothing this warehouse holds names its hash
//! ([`applied_redactions`]): no pallet head and no meta pallet's history. Applying it records the
//! hash; this module removes the copies the store still holds.
//!
//! What may go is decided by the deletion census ([`deletion_census`]), not by the collector's
//! roots. The census is what this warehouse holds a copy *for*: every pallet head's tree, every meta
//! pallet's whole history, and each checkout's own state (its base, its parked parcels, a
//! consolidation or cherry-pick in progress, and staged work). The history the trust anchor adopts
//! is meta history, so it is in it too. Old history is not, and neither are the roots that keep
//! history only for others or for an undo: tombstones, hauls, the git mirror's roots, the other
//! trust pins and a journal's removed heads. A copy only those name is the one
//! the redaction exists to delete, and reads already tolerate its absence.
//!
//! The census names hashes, never the objects above them: no parcel or tree is deleted, only the
//! redacted blob or recipe, and the chunks of a recipe no unredacted object names.

use std::collections::{BTreeMap, HashMap, HashSet};
use crate::enums::dir_entry_type::DirEntryType;
use crate::globals::{forklift_root, FOLDER_NAME_INVENTORY_ROOT};
use crate::enums::object::parsed_object::ParsedObject;
use crate::parser::object::loose_object_parser;
use crate::util::lock_utils::{ServeLock, StoreAccessExclusive, StoreLock};
use crate::util::progress_utils::Progress;
use crate::util::{
    applied_redactions, bay_utils, cache_miss, file_utils, gc_utils, object_utils, office_utils, pack_utils,
    pallet_utils, prune_utils, recovery_utils, redaction_utils,
};

/// A checkout: the warehouse's own working tree, or a named bay.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Checkout {
    /// The warehouse's own working tree.
    Main,

    /// The named bay.
    Bay(String),
}

/// A census root that names a copy of a redacted hash, so the copy is kept.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum CopyHolder {
    /// The named user pallet's head.
    Pallet(String),

    /// The named meta pallet's history (the name without its `@`).
    MetaHistory(String),

    /// The history the trust anchor adopts.
    AdoptedHistory,

    /// A checkout's base.
    Base(Checkout),

    /// A parcel parked in a checkout.
    Parked(Checkout),

    /// A consolidation in progress in a checkout.
    Consolidation(Checkout),

    /// A cherry-pick in progress in a checkout.
    Pick(Checkout),

    /// Work staged in a checkout.
    Staged(Checkout),
}

impl Checkout {
    fn describe(&self) -> String {
        match self {
            Checkout::Main => "the warehouse's own working tree".to_string(),
            Checkout::Bay(name) => format!("bay \"{}\"", name),
        }
    }
}

impl CopyHolder {
    /// The holder as a noun phrase, such as `the base of bay "x"`. Names are unsanitized: a pallet
    /// or bay name is the operator's text, so a caller printing it sanitizes it.
    pub fn describe(&self) -> String {
        match self {
            CopyHolder::Pallet(name) => format!("pallet \"{}\"", name),
            CopyHolder::MetaHistory(name) => format!("the history of @{}", name),
            CopyHolder::AdoptedHistory => "the history the trust anchor adopts".to_string(),
            CopyHolder::Base(checkout) => format!("the base of {}", checkout.describe()),
            CopyHolder::Parked(checkout) => format!("a parcel parked in {}", checkout.describe()),
            CopyHolder::Consolidation(checkout) => format!("the consolidation in progress in {}", checkout.describe()),
            CopyHolder::Pick(checkout) => format!("the cherry-pick in progress in {}", checkout.describe()),
            CopyHolder::Staged(checkout) => format!("the work staged in {}", checkout.describe()),
        }
    }
}

/// Which census roots name each of `targets`: a target no root names is absent from the map, and
/// every listed holder appears once per target, sorted.
///
/// The walk is presence-tolerant, as the collector's is: an absent parcel or tree is skipped, since
/// a copy present beneath an absent tree is named through another, present path or by nothing.
/// A tree's entries are read, never its blobs, so a target is found by the hash a tree entry or a
/// staged shard holds for it.
///
/// # Returns
/// * `Ok(BTreeMap<String, Vec<CopyHolder>>)` - The holders of each named target.
/// * `Err(String)` - If a pallet ref, the trust anchor, a bay's state or a staged shard could not
///                   be read, or a present parcel or tree could not be loaded: the census is then
///                   unknown, and nothing may be deleted on it.
pub fn deletion_census(targets: &HashSet<String>) -> Result<BTreeMap<String, Vec<CopyHolder>>, String> {
    let mut census = Census { targets, under: HashMap::new(), held: BTreeMap::new() };

    if targets.is_empty() {
        return Ok(census.held);
    }

    let mut meta_heads: Vec<(String, CopyHolder)> = Vec::new();

    for (pallet_ref, head) in pallet_utils::all_pallet_refs()? {
        match pallet_ref.namespace {
            pallet_utils::PalletNamespace::User => census.parcel_tree(&head, CopyHolder::Pallet(pallet_ref.name))?,
            pallet_utils::PalletNamespace::Meta => meta_heads.push((head, CopyHolder::MetaHistory(pallet_ref.name))),
        }
    }

    // The trust anchor's `adopts` pin is meta history no meta pallet head need still reach.
    for (pin, policy) in office_utils::collect_trust_pin_roots(office_utils::TrustPinReadPolicy::FailClosed)?.roots {
        if policy == office_utils::PinAbsencePolicy::Loss {
            meta_heads.push((pin, CopyHolder::AdoptedHistory));
        }
    }

    for (head, holder) in meta_heads {
        for parcel in gc_utils::collect_reachable_for_sweep(&[head])? {
            census.parcel_tree(&parcel, holder.clone())?;
        }
    }

    let checkouts = std::iter::once((Checkout::Main, forklift_root()))
        .chain(bay_utils::list_bays()?.into_iter().map(|name| (Checkout::Bay(name.clone()), bay_utils::bay_state_dir(&name))));

    for (checkout, dir) in checkouts {
        let state = bay_utils::read_bay_state_roots_in(&dir)?;

        if let Some(base) = state.base {
            census.parcel_tree(&base, CopyHolder::Base(checkout.clone()))?;
        }
        for parked in state.parked {
            census.parcel_tree(&parked, CopyHolder::Parked(checkout.clone()))?;
        }
        for parcel in state.consolidation {
            census.parcel_tree(&parcel, CopyHolder::Consolidation(checkout.clone()))?;
        }
        for parcel in state.pick {
            census.parcel_tree(&parcel, CopyHolder::Pick(checkout.clone()))?;
        }

        let mut leaves: Vec<(String, DirEntryType)> = Vec::new();
        recovery_utils::walk_shard_files(&dir.join(FOLDER_NAME_INVENTORY_ROOT), &mut leaves)?;

        for (hash, _) in leaves {
            if targets.contains(&hash) {
                census.hold(&hash, CopyHolder::Staged(checkout.clone()));
            }
        }
    }

    Ok(census.held)
}

/// The applied redactions no deletion census root names: the hashes the collector and a repack
/// treat as unreferenced, and the deleter deletes. Empty, with no census taken, when none is
/// applied.
///
/// # Returns
/// * `Ok(HashSet<String>)` - The excluded hashes.
/// * `Err(String)`         - If the applied record could not be read, or the census could not be
///                           taken.
pub(crate) fn unheld_applied() -> Result<HashSet<String>, String> {
    unheld(&applied_redactions::read_applied()?.hashes().cloned().collect())
}

/// Of `applied`, the hashes the store holds as a file's content that no deletion census root names
/// ([`maybe_present`]). A hash the store plainly lacks (one an earlier run deleted) is left out
/// unread, so no census is taken once every applied object is gone; one whose presence or kind
/// cannot be read is left out too, so a collection keeps it rather than take what it cannot tell
/// is a copy.
///
/// # Returns
/// * `Ok(HashSet<String>)` - The excluded hashes.
/// * `Err(String)`         - If the census could not be taken.
pub(crate) fn unheld(applied: &HashSet<String>) -> Result<HashSet<String>, String> {
    let targets = maybe_present(applied);
    let held = deletion_census(&targets)?;

    Ok(targets.into_iter().filter(|hash| !held.contains_key(hash)).collect())
}

/// Of `hashes`, those the store holds as a file's content
/// ([`redaction_utils::holds_as_file_content`]). One it plainly lacks (an earlier run deleted it) is
/// left out unread. So is one it holds as a tree, a parcel or a chunk: a redaction names a file's
/// content, so such an object is no copy of it, and other content may share it (only a cell naming
/// that hash by itself, applied while the store lacked the object, lets one be stored under it).
/// One whose presence or kind cannot be read is left out too, as the deletion must not take what it
/// cannot tell is a copy; a deletion that runs again once the store reads finishes.
fn maybe_present(hashes: &HashSet<String>) -> HashSet<String> {
    hashes.iter().filter(|hash| redaction_utils::holds_as_file_content(hash)).cloned().collect()
}

/// The copies of applied redactions' objects the store may still hold, as the census finds them
/// now, taken without any lock so a caller can tell whether [`delete_applied`] has anything to
/// delete before it takes the locks the deletion needs. The deletion takes its own census under
/// them; this one only decides whether to try.
#[derive(Debug, Default)]
pub struct AppliedCopies {
    /// The copies the census names, each with its holders: kept.
    pub held: BTreeMap<String, Vec<CopyHolder>>,

    /// How many copies no census root names: the deletion's to delete.
    pub unheld: usize,
}

/// Take the census of every applied redaction's object the store may still hold (see
/// [`AppliedCopies`]). Both parts are empty once every applied object is gone, or when none is
/// applied: no bay state is read then.
///
/// # Returns
/// * `Ok(AppliedCopies)` - The held copies and the count of unheld ones.
/// * `Err(String)`       - If the applied record or the census could not be read.
pub fn applied_copies() -> Result<AppliedCopies, String> {
    let applied: HashSet<String> = applied_redactions::read_applied()?.hashes().cloned().collect();
    let present = maybe_present(&applied);

    if present.is_empty() {
        return Ok(AppliedCopies::default());
    }

    let held = deletion_census(&present)?;
    let unheld = present.iter().filter(|hash| !held.contains_key(*hash)).count();

    Ok(AppliedCopies { held, unheld })
}

/// What [`delete_applied`] did.
#[derive(Debug, Default)]
pub struct Deletion {
    /// The applied redactions' objects (blobs and recipes) deleted, sorted.
    pub objects: Vec<String>,

    /// The chunks deleted: those of a deleted recipe that no unredacted object names.
    pub chunks: usize,

    /// The bytes freed: loose files deleted, and what the old packs held beyond the new ones.
    pub bytes_freed: u64,

    /// The applied redactions whose copies are kept, each with the census roots that hold it.
    pub held: BTreeMap<String, Vec<CopyHolder>>,
}

/// Delete every copy of an applied redaction's object that no deletion census root names, and the
/// chunks of a deleted recipe that no unredacted object names. Copies the census names are kept and
/// reported with their holders: once the file is taken out of that state, the next deletion
/// removes them.
///
/// Runs under the locks a full repack takes, in its order, from before the census until the last
/// delete, so no writer can publish a new reference to an object about to go. Nothing is fetched.
///
/// Every deletion follows evict's order: packed copies go by a full repack that keeps everything
/// else, while loose copies of delta bases can still be read, then loose copies. It runs in two
/// passes so that a present recipe never lacks a chunk, a state a walk reads as damage: the first
/// deletes the redacted objects and keeps every chunk, the second deletes the chunks only they
/// named. Each recipe's chunk list is read before anything is deleted. A run cut short at any point
/// leaves either a recipe with all its chunks, which running it again deletes, or chunks nothing
/// names: with their recipe gone a rerun cannot find them, and they are left as garbage, which the
/// next collection or full repack deletes.
///
/// # Arguments
/// * `store_lock`   - The object-store lock, held.
/// * `serve_lock`   - The serve lock, held: no server may answer for an object about to go.
/// * `store_access` - Exclusive store access, held.
/// * `progress`     - Where a repack reports its phases.
///
/// # Returns
/// * `Ok(Deletion)` - What was deleted, and what was kept.
/// * `Err(String)`  - If the applied record, the census, a recipe or the packs could not be read
///                    (nothing is deleted then), a repack or a delete failed, or the first
///                    pass's repack left a copy inside a pack it found damaged (the chunks are
///                    then all kept). Running it again finishes, as for a run cut short.
pub fn delete_applied(store_lock: &StoreLock,
                      serve_lock: &ServeLock,
                      store_access: &StoreAccessExclusive,
                      progress: &Progress) -> Result<Deletion, String> {
    let _unfetched = cache_miss::Unfetched::enter();

    let applied: HashSet<String> = applied_redactions::read_applied()?.hashes().cloned().collect();
    let present = maybe_present(&applied);
    let held = deletion_census(&present)?;
    let targets: HashSet<String> = present.into_iter().filter(|hash| !held.contains_key(hash)).collect();

    // An excluded target is never marked live, and a chunk's hash is never a blob's or a recipe's
    // (an object's hash covers its type), so every target goes.
    let live = gc_utils::collect_live_set_excluding(&targets)?;
    let objects = targets;

    let mut chunks: HashSet<String> = HashSet::new();
    for hash in &objects {
        if !file_utils::is_present_for_sweep(hash)? {
            continue;
        }
        let bytes = file_utils::retrieve_object_by_hash_shared(hash)?;
        if let ParsedObject::Recipe(recipe) = loose_object_parser::parse(&bytes)? {
            chunks.extend(recipe.chunks.into_iter().map(|chunk| chunk.hash).filter(|chunk| !live.contains(chunk)));
        }
    }

    let first = drop_copies(&objects, &chunks, store_lock, serve_lock, store_access, progress)?;

    #[cfg(test)]
    if tests::STOP_BETWEEN_PASSES.with(|stop| stop.get()) {
        return Err("stopped between the passes".to_string());
    }

    let second = drop_copies(&chunks, &HashSet::new(), store_lock, serve_lock, store_access, progress)?;

    let mut deleted = first.deleted;
    deleted.sort();

    Ok(Deletion {
        objects: deleted,
        chunks: second.deleted.len(),
        bytes_freed: first.bytes_freed + second.bytes_freed,
        held,
    })
}

/// What one pass of [`delete_applied`] dropped.
#[derive(Default)]
struct Dropped {
    deleted: Vec<String>,
    bytes_freed: u64,
}

/// Drop every copy of `set`, packed and loose, keeping `kept` through the repack as if it were
/// live.
fn drop_copies(set: &HashSet<String>,
               kept: &HashSet<String>,
               store_lock: &StoreLock,
               serve_lock: &ServeLock,
               store_access: &StoreAccessExclusive,
               progress: &Progress) -> Result<Dropped, String> {
    let mut dropped = Dropped::default();

    if set.is_empty() {
        return Ok(dropped);
    }

    let mut packed: HashSet<String> = HashSet::new();
    let mut loose: Vec<String> = Vec::new();

    for hash in set {
        if pack_utils::is_in_packs(hash)? {
            packed.insert(hash.clone());
        }
        if file_utils::loose_dentry_exists(hash)? {
            loose.push(hash.clone());
        }
    }

    // A packed delta can stand on a loose-only copy; the repack re-encodes it before that copy is
    // freed. A damaged pack refuses the pass, which could not tell what stands on what it deletes.
    let loose_only: HashSet<String> = loose.iter().filter(|hash| !packed.contains(*hash)).cloned().collect();
    let delta_on_loose = pack_utils::any_packed_delta_against(&loose_only)
        .map_err(|error| error.refusal("Deleting a redacted object", "the objects it names were kept"))?;

    if !packed.is_empty() || delta_on_loose {
        dropped.bytes_freed += pack_utils::repack_redacting(set, kept, store_lock, serve_lock, store_access, progress)?
            .pack_bytes_freed;

        // A pack found damaged only during the repack is left as found, holding its copy. Stop
        // here, before a later pass deletes the chunks of a recipe it still holds.
        let mut still_packed = 0;
        for hash in &packed {
            if pack_utils::is_in_packs(hash)? {
                still_packed += 1;
            }
        }
        if still_packed > 0 {
            return Err(format!(
                "{} redacted object(s) are still inside a pack the repack found damaged, so they \
                were kept. Run \"forklift store compact --all\" to salvage it, then \"forklift redact \
                apply\" again.",
                still_packed
            ));
        }
    }

    dropped.bytes_freed += prune_utils::free_objects(&loose, store_access)?.bytes;
    dropped.deleted = packed.iter().chain(loose.iter())
        .cloned()
        .collect::<HashSet<String>>()
        .into_iter()
        .collect();

    Ok(dropped)
}

/// A census in progress: the targets, the targets each tree walked so far reaches, and the holders
/// found.
struct Census<'a> {
    targets: &'a HashSet<String>,
    under: HashMap<String, Vec<String>>,
    held: BTreeMap<String, Vec<CopyHolder>>,
}

impl Census<'_> {
    /// Hold every target the tree of the parcel `parcel` reaches for `holder`.
    fn parcel_tree(&mut self, parcel: &str, holder: CopyHolder) -> Result<(), String> {
        if !file_utils::is_present_for_sweep(parcel)? {
            return Ok(());
        }

        let tree = object_utils::load_parcel(parcel)?.tree_hash;

        for target in self.targets_under(&tree)? {
            self.hold(&target, holder.clone());
        }

        Ok(())
    }

    fn hold(&mut self, target: &str, holder: CopyHolder) {
        let holders = self.held.entry(target.to_string()).or_default();

        if !holders.contains(&holder) {
            holders.push(holder);
            holders.sort();
        }
    }

    /// The targets the tree `tree` reaches, each once. Each tree is read once per census, however
    /// many roots share it.
    fn targets_under(&mut self, tree: &str) -> Result<Vec<String>, String> {
        if let Some(found) = self.under.get(tree) {
            return Ok(found.clone());
        }

        let mut found: Vec<String> = Vec::new();

        if file_utils::is_present_for_sweep(tree)? {
            let loaded = object_utils::load_tree(tree)?;

            for (_, file) in loaded.get_files() {
                if self.targets.contains(&file.hash) && !found.contains(&file.hash) {
                    found.push(file.hash.clone());
                }
            }

            for (_, subtree) in loaded.get_subtrees() {
                for target in self.targets_under(&subtree.hash)? {
                    if !found.contains(&target) {
                        found.push(target);
                    }
                }
            }
        }

        self.under.insert(tree.to_string(), found.clone());
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::object::loose_object_builder::LooseObjectBuilder;
    use crate::globals::StorageRootScope;
    use crate::model::blob::Blob;
    use crate::model::parcel::Parcel;
    use crate::util::gc_utils::tests::{plant_staged_shard, store_blob, store_chunk, store_recipe, store_tree};
    use crate::util::pack_utils;
    use crate::util::progress_utils::Progress;
    use std::path::PathBuf;

    /// A fresh full warehouse root for one test, entered as the active storage-root scope.
    struct Scratch {
        _scope: StorageRootScope,
        root: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let root = std::env::temp_dir().join(format!(
                "forklift-redaction-deleter-test-{}-{}-{}", name, std::process::id(), id
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = StorageRootScope::enter(&root);
            crate::util::warehouse_utils::mark_for_test(crate::util::warehouse_utils::WarehouseMode::Full);
            pallet_utils::create_position_if_not_exists().unwrap();

            Scratch { _scope: scope, root }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn store_parcel(tree_hash: &str, parents: Vec<String>) -> String {
        let parcel = Parcel { tree_hash: tree_hash.to_string(), parents, actions: Vec::new(), description: Some("p".to_string()) };
        let mut object = LooseObjectBuilder::build_parcel(&parcel);
        object.store().unwrap();
        object.hash
    }

    fn set_main(parcel: &str) {
        pallet_utils::update_pallet_head(
            "main", pallet_utils::get_pallet_head("main").unwrap().as_deref(), parcel, &pallet_utils::RefPreconditions::NONE,
        ).unwrap();
    }

    fn set_base(parcel: &str) {
        pallet_utils::write_position(&pallet_utils::BayPosition::new("main", Some(parcel.to_string()))).unwrap();
    }

    fn read_fresh(hash: &str) -> Result<Vec<u8>, String> {
        pack_utils::forget_warehouse();
        file_utils::retrieve_object_by_hash_uncached(hash)
    }

    fn present(hash: &str) -> bool {
        pack_utils::forget_warehouse();
        file_utils::is_present_for_sweep(hash).unwrap()
    }

    const FILE: DirEntryType = DirEntryType::Normal;

    thread_local! {
        /// Makes [`delete_applied`] stop between its two passes, as a run cut short there would.
        pub(super) static STOP_BETWEEN_PASSES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// Run the deleter under the locks it needs, as `forklift redact apply` takes them.
    fn delete() -> Result<Deletion, String> {
        let store_lock = StoreLock::acquire().expect("no other holder in a test root");
        let serve_lock = ServeLock::acquire().expect("no other holder in a test root");
        let store_access = crate::util::lock_utils::StoreAccessLock::try_exclusive().expect("no other holder in a test root");

        delete_applied(&store_lock, &serve_lock, &store_access, &Progress::NONE)
    }

    /// Store a chunk into a pack, as a received pack can hold one (a compaction leaves a loose
    /// chunk loose), and return its hash.
    fn pack_chunk(content: &[u8]) -> String {
        let object = LooseObjectBuilder::build_chunk(&crate::model::chunk::Chunk { content: content.to_vec() });
        let mut ingest = pack_utils::StoreIngest::new().unwrap();
        ingest.store_with_base(&object, None).unwrap();
        ingest.finish().unwrap();

        object.hash
    }

    /// A chunked file's recipe and one of its chunks only it names, both packed and named only by
    /// old history, beside a live recipe sharing its other chunk. Returns (recipe, own chunk,
    /// shared chunk, live recipe).
    fn packed_chunked_leak() -> (String, String, String, String) {
        let (own_chunk, shared_chunk, other_chunk) = (pack_chunk(b"own"), store_chunk(b"shared"), store_chunk(b"other"));
        let recipe = store_recipe(&[(own_chunk.clone(), 3), (shared_chunk.clone(), 6)]);
        let live_recipe = store_recipe(&[(shared_chunk.clone(), 6), (other_chunk, 5)]);

        let old = store_parcel(&store_tree(&[("big.bin", &recipe, DirEntryType::NormalChunked)]), Vec::new());
        set_main(&store_parcel(&store_tree(&[("other.bin", &live_recipe, DirEntryType::NormalChunked)]), vec![old]));
        pack_utils::compact(true, false, false, &Progress::NONE).unwrap();
        assert!(pack_utils::is_in_packs(&recipe).unwrap(), "the fixture packs the recipe");
        assert!(pack_utils::is_in_packs(&own_chunk).unwrap(), "the fixture packs the chunk");
        applied_redactions::apply_for_test(&[&recipe]);

        (recipe, own_chunk, shared_chunk, live_recipe)
    }

    /// The deleter drops a packed and a loose copy of applied redactions' objects that nothing in
    /// the census names, keeps the one a checkout's base names, and reports it with its holder.
    ///
    /// Revert: skip the repack → the packed copy stays; skip freeing the loose copies → the loose
    /// one stays; delete every present applied object, census or not → the held copy goes.
    #[test]
    fn the_deleter_drops_packed_and_loose_copies_and_keeps_a_held_one() {
        let _scratch = Scratch::new("delete");
        let (packed, kept) = (store_blob("packed secret\n"), store_blob("kept\n"));
        let p0 = store_parcel(&store_tree(&[("p.txt", &packed, FILE)]), Vec::new());
        let p1 = store_parcel(&store_tree(&[("k.txt", &kept, FILE)]), vec![p0.clone()]);
        set_main(&p1);
        pack_utils::compact(true, false, false, &Progress::NONE).unwrap();
        assert!(pack_utils::is_in_packs(&packed).unwrap(), "the fixture packs it");

        let (loose, held) = (store_blob("loose secret\n"), store_blob("held\n"));
        let p2 = store_parcel(&store_tree(&[("l.txt", &loose, FILE)]), vec![p1.clone()]);
        let p3 = store_parcel(&store_tree(&[("k.txt", &kept, FILE)]), vec![p2.clone()]);
        set_main(&p3);
        let base = store_parcel(&store_tree(&[("held.txt", &held, FILE)]), Vec::new());
        set_base(&base);
        assert!(!pack_utils::is_in_packs(&loose).unwrap(), "the fixture leaves it loose");
        applied_redactions::apply_for_test(&[&packed, &loose, &held]);

        let deletion = delete().unwrap();

        let mut expected = vec![packed.clone(), loose.clone()];
        expected.sort();
        assert_eq!(deletion.objects, expected);
        assert_eq!(deletion.chunks, 0);
        assert!(deletion.bytes_freed > 0, "{:?}", deletion);
        assert_eq!(deletion.held, [(held.clone(), vec![CopyHolder::Base(Checkout::Main)])].into_iter().collect());
        for hash in [&packed, &loose] {
            assert!(!present(hash), "deleted: {}", hash);
        }
        for hash in [&held, &kept, &p0, &p1, &p2, &p3, &base] {
            assert!(present(hash), "kept: {}", hash);
        }

        // Run again, nothing is left to delete, and the held copy is still reported.
        let again = delete().unwrap();
        assert!(again.objects.is_empty() && again.chunks == 0, "{:?}", again);
        assert_eq!(again.held, deletion.held);
    }

    /// An applied hash this store holds as a chunk is no file's content, so the deleter leaves it,
    /// even when nothing names it: the walks that tolerate an applied absence hold it only where
    /// it stands as a file.
    ///
    /// Revert: count every present applied object as a target → the chunk is deleted.
    #[test]
    fn the_deleter_leaves_an_applied_hash_held_as_a_chunk() {
        let _scratch = Scratch::new("chunk-applied");
        let chunk = store_chunk(b"bytes");
        set_main(&store_parcel(&store_tree(&[]), Vec::new()));
        applied_redactions::apply_for_test(&[&chunk]);

        let deletion = delete().unwrap();

        assert!(deletion.objects.is_empty(), "{:?}", deletion.objects);
        assert!(present(&chunk));
    }

    /// An applied hash whose stored object cannot be read is kept: the deleter cannot tell it is a
    /// copy of a file's content rather than a tree or a chunk other content shares.
    ///
    /// Revert: count an unreadable object as a file's content → it is deleted.
    #[test]
    fn the_deleter_keeps_an_applied_object_it_cannot_read() {
        let _scratch = Scratch::new("unreadable-applied");
        set_main(&store_parcel(&store_tree(&[]), Vec::new()));
        let hash = "e".repeat(64);
        let (folder, file) = file_utils::get_path_for_object(&hash).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(std::path::Path::new(&folder).join(file), b"not an object").unwrap();
        applied_redactions::apply_for_test(&[&hash]);

        let deletion = delete().unwrap();

        assert!(deletion.objects.is_empty(), "{:?}", deletion.objects);
        assert!(present(&hash));
    }


    /// recipe shares, which stays readable.
    ///
    /// Revert: take every chunk of a deleted recipe → the shared chunk goes; skip the second pass →
    /// the recipe's own chunk stays.
    #[test]
    fn the_chunk_pass_keeps_a_chunk_a_live_recipe_shares() {
        let _scratch = Scratch::new("chunks");
        let (recipe, own_chunk, shared_chunk, live_recipe) = packed_chunked_leak();

        let deletion = delete().unwrap();

        assert_eq!(deletion.objects, vec![recipe.clone()]);
        assert_eq!(deletion.chunks, 1);
        assert!(!present(&recipe) && !present(&own_chunk));
        assert!(present(&shared_chunk) && present(&live_recipe));
        assert_eq!(read_fresh(&shared_chunk).unwrap(), file_utils::retrieve_object_by_hash_uncached(&shared_chunk).unwrap());
    }

    /// Cut short between its passes, the deleter leaves no present recipe missing a chunk: the
    /// first pass deletes the recipe and keeps every chunk. The chunk it leaves is garbage, which a
    /// full repack deletes.
    ///
    /// Revert: keep no chunk through the first pass's repack → the recipe's own chunk is gone
    /// after the first pass; drop the kept set from the repack's live set → the same.
    #[test]
    fn a_deletion_cut_short_between_its_passes_leaves_every_chunk() {
        let _scratch = Scratch::new("cut-short");
        let (recipe, own_chunk, shared_chunk, _) = packed_chunked_leak();

        STOP_BETWEEN_PASSES.with(|stop| stop.set(true));
        let stopped = delete();
        STOP_BETWEEN_PASSES.with(|stop| stop.set(false));

        assert!(stopped.is_err(), "{:?}", stopped);
        assert!(!present(&recipe), "the first pass deleted the recipe");
        assert!(present(&own_chunk) && present(&shared_chunk), "and kept its chunks");

        // With the recipe gone, nothing names the chunk: a full repack drops it as garbage.
        pack_utils::compact(true, false, false, &Progress::NONE).unwrap();
        assert!(!present(&own_chunk));
        assert!(present(&shared_chunk));
    }

    /// Each census root names the copies it holds and only those: a pallet's head, not its older
    /// history; a meta pallet's whole history; a checkout's base; another bay's staged work.
    ///
    /// Revert: walk only the meta heads → the meta copy is unnamed; walk each user pallet's history →
    /// the old copy is named; leave the bays' staged shards out → the staged copy is unnamed.
    #[test]
    fn the_census_names_each_root_that_holds_a_copy_and_nothing_else() {
        let _scratch = Scratch::new("census");
        let (old, base, head, meta, staged) =
            (store_blob("old\n"), store_blob("base\n"), store_blob("head\n"), store_blob("meta\n"), store_blob("staged\n"));

        let p0 = store_parcel(&store_tree(&[("old.txt", &old, FILE)]), Vec::new());
        let p1 = store_parcel(&store_tree(&[("base.txt", &base, FILE)]), vec![p0]);
        let p2 = store_parcel(&store_tree(&[("dir", &store_tree(&[("head.txt", &head, FILE)]), DirEntryType::Tree)]), vec![p1.clone()]);
        set_main(&p2);
        set_base(&p1);

        let m0 = store_parcel(&store_tree(&[("record", &meta, FILE)]), Vec::new());
        let m1 = store_parcel(&store_tree(&[("record", &store_blob("now\n"), FILE)]), vec![m0]);
        pallet_utils::update_meta_pallet_head("notes", None, &m1, &pallet_utils::RefPreconditions::NONE).unwrap();
        plant_staged_shard("b", &[("wip.txt", &staged, FILE)]);

        let targets: HashSet<String> = [&old, &base, &head, &meta, &staged].into_iter().cloned().collect();
        let census = deletion_census(&targets).unwrap();

        let expected: BTreeMap<String, Vec<CopyHolder>> = [
            (base, vec![CopyHolder::Base(Checkout::Main)]),
            (head, vec![CopyHolder::Pallet("main".to_string())]),
            (meta, vec![CopyHolder::MetaHistory("notes".to_string())]),
            (staged, vec![CopyHolder::Staged(Checkout::Bay("b".to_string()))]),
        ].into_iter().collect();
        assert_eq!(census, expected, "old history names nothing: {}", old);
    }

    /// The census lists every root that holds a copy, once each, however many trees name it.
    #[test]
    fn the_census_lists_every_holder_once() {
        let _scratch = Scratch::new("census-holders");
        let shared = store_blob("shared\n");
        let tree = store_tree(&[("a.txt", &shared, FILE), ("dir", &store_tree(&[("b.txt", &shared, FILE)]), DirEntryType::Tree)]);
        let parcel = store_parcel(&tree, Vec::new());
        set_main(&parcel);
        set_base(&parcel);

        let census = deletion_census(&[shared.clone()].into_iter().collect()).unwrap();

        assert_eq!(census[&shared], vec![CopyHolder::Pallet("main".to_string()), CopyHolder::Base(Checkout::Main)]);
    }

    /// A collection deletes an applied redaction's object only old history names, and a recipe's
    /// chunks no other file names; it keeps a copy the census names, and a chunk another recipe shares.
    ///
    /// Revert: mark an excluded tree entry live anyway → the blob, the recipe and its own chunk
    /// stay; exclude a census-named hash → the copy the checkout's base names goes.
    #[test]
    fn gc_deletes_an_unheld_applied_object_and_keeps_a_held_one() {
        let _scratch = Scratch::new("gc");
        let (secret, held) = (store_blob("secret\n"), store_blob("held\n"));
        let (own_chunk, shared_chunk, other_chunk) = (store_chunk(b"own"), store_chunk(b"shared"), store_chunk(b"other"));
        let recipe = store_recipe(&[(own_chunk.clone(), 3), (shared_chunk.clone(), 6)]);
        let other_recipe = store_recipe(&[(shared_chunk.clone(), 6), (other_chunk.clone(), 5)]);

        let old_tree = store_tree(&[
            ("secret.txt", &secret, FILE), ("big.bin", &recipe, DirEntryType::NormalChunked), ("held.txt", &held, FILE),
        ]);
        let old = store_parcel(&old_tree, Vec::new());
        let head = store_parcel(&store_tree(&[("other.bin", &other_recipe, DirEntryType::NormalChunked)]), vec![old.clone()]);
        set_main(&head);
        let base = store_parcel(&store_tree(&[("held.txt", &held, FILE)]), Vec::new());
        set_base(&base);
        applied_redactions::apply_for_test(&[&secret, &recipe, &held]);

        let stats = gc_utils::collect_garbage(0).unwrap();

        assert_eq!(stats.deleted, 3);
        for hash in [&secret, &recipe, &own_chunk] {
            assert!(!present(hash), "deleted: {}", hash);
        }
        for hash in [&held, &shared_chunk, &other_chunk, &other_recipe, &old, &old_tree, &head] {
            assert!(present(hash), "kept: {}", hash);
        }
    }

    /// A full repack drops a packed copy of an applied redaction's object nothing in the census
    /// names, and keeps a version stored as a delta on it readable.
    ///
    /// Revert: pass no exclusion to the repack's live set → the packed copy stays; offer the
    /// redacted object as a delta base → (with the exclusion) the kept version no longer reads.
    #[test]
    fn a_full_repack_drops_a_packed_unheld_applied_object() {
        let _scratch = Scratch::new("repack");
        let lines: String = (0..200).map(|line| format!("{}\n", blake3::hash(format!("line {}", line).as_bytes()).to_hex())).collect();
        let blob = |tail: &str| LooseObjectBuilder::build_blob(&Blob { content: format!("{}{}\n", lines, tail).into_bytes() });
        let (mut secret, kept) = (blob("secret"), blob("kept"));
        secret.store().unwrap();

        let mut ingest = pack_utils::StoreIngest::new().unwrap();
        let stored = ingest.store_with_base(&kept, Some(pack_utils::IngestBase { hash: &secret.hash, bytes: &secret.content, depth: 0 })).unwrap();
        assert!(matches!(stored, pack_utils::IngestStored::Delta { .. }), "the fixture's shape");
        ingest.finish().unwrap();

        let old = store_parcel(&store_tree(&[("f", &secret.hash, FILE)]), Vec::new());
        let head = store_parcel(&store_tree(&[("f", &kept.hash, FILE)]), vec![old]);
        set_main(&head);
        pack_utils::compact(true, false, false, &Progress::NONE).unwrap();
        assert!(present(&secret.hash), "the fixture packs the secret");
        applied_redactions::apply_for_test(&[&secret.hash]);

        pack_utils::compact(true, false, false, &Progress::NONE).unwrap();

        assert!(read_fresh(&secret.hash).is_err(), "the packed copy is dropped");
        assert_eq!(read_fresh(&kept.hash).unwrap(), kept.content, "the delta on it is re-encoded");
    }

    /// An incremental compaction leaves an applied redaction's loose object loose, where a
    /// collection deletes it, and packs the rest.
    ///
    /// Revert: pack every loose object → the secret lands in a pack and the collection, which
    /// sweeps loose objects only, leaves it.
    #[test]
    fn an_incremental_compaction_leaves_an_applied_object_loose_for_the_collection() {
        let _scratch = Scratch::new("incremental");
        let (secret, kept) = (store_blob("secret\n"), store_blob("kept\n"));
        let old = store_parcel(&store_tree(&[("f", &secret, FILE)]), Vec::new());
        set_main(&store_parcel(&store_tree(&[("f", &kept, FILE)]), vec![old]));
        applied_redactions::apply_for_test(&[&secret]);

        pack_utils::compact(false, false, false, &Progress::NONE).unwrap();
        assert!(!pack_utils::is_in_packs(&secret).unwrap() && pack_utils::is_in_packs(&kept).unwrap());

        gc_utils::collect_garbage(0).unwrap();
        assert!(!present(&secret), "the collection deletes the loose copy");
        assert!(present(&kept));
    }

    /// No census is taken once every applied object is gone, and one is while one remains: an
    /// unreadable bay state fails a census, so it shows which.
    ///
    /// Revert: take the census over every applied hash → the exclusion fails on the bad bay.
    #[test]
    fn no_census_is_taken_once_every_applied_object_is_gone() {
        let _scratch = Scratch::new("gone");
        let secret = store_blob("secret\n");
        set_main(&store_parcel(&store_tree(&[("f", &store_blob("kept\n"), FILE)]), Vec::new()));
        applied_redactions::apply_for_test(&[&secret]);
        gc_utils::collect_garbage(0).unwrap();
        assert!(!present(&secret));

        let bay = bay_utils::bay_state_dir("broken");
        std::fs::create_dir_all(&bay).unwrap();
        std::fs::write(bay.join(pallet_utils::FILE_NAME_POSITION), b"not a position").unwrap();

        let gone: HashSet<String> = [secret].into_iter().collect();
        assert_eq!(unheld(&gone).expect("no census once the applied object is gone"), HashSet::new());

        let remaining: HashSet<String> = [store_blob("secret again\n")].into_iter().collect();
        assert!(unheld(&remaining).is_err(), "a census over a present object reads every bay");
    }

    /// No applied redaction's object is offered as a delta base, even one the census still names,
    /// so the deleter never meets a kept delta standing on it.
    ///
    /// Revert: leave the applied hashes out of what is never a base → the repack stores the new
    /// version as a delta on the redacted object.
    #[test]
    fn a_repack_never_bases_a_delta_on_an_applied_object() {
        let _scratch = Scratch::new("never-base");
        let lines: String = (0..200).map(|line| format!("{}\n", blake3::hash(format!("line {}", line).as_bytes()).to_hex())).collect();
        let secret = store_blob(&format!("{}secret\n", lines));
        let kept = store_blob(&format!("{}kept\n", lines));

        let old = store_parcel(&store_tree(&[("f", &secret, FILE)]), Vec::new());
        let head = store_parcel(&store_tree(&[("f", &kept, FILE)]), vec![old.clone()]);
        set_main(&head);
        set_base(&old);
        applied_redactions::apply_for_test(&[&secret]);

        pack_utils::compact(true, false, false, &Progress::NONE).unwrap();

        assert!(present(&secret), "the base names the secret, so it stays");
        assert!(
            matches!(pack_utils::any_packed_delta_against(&[secret].into_iter().collect()), Ok(false)),
            "no packed delta stands on the secret: {}", kept
        );
    }
}
