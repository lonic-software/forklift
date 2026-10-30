//! Eviction: shrinking a cache warehouse (`franchise --cache`) back down by freeing content its
//! origin holds and nothing local needs (DESIGN.html §7.6.1).
//!
//! A cache holds every parcel and signature and fetches content when it is read. Eviction is the
//! way back: it frees trees, blobs, recipes and chunks, never parcels or signatures, and records
//! each object it frees in the sealed-by-hash record first, so that a later read refills it from
//! the origin instead of reading it as lost.
//!
//! What may go is decided by the origin's heads, not by asking it about each object. The caller
//! passes the heads the origin holds (its live pallet heads, its tombstone heads, and the recorded
//! heads it confirmed it still holds); the origin keeps the closure of exactly those, so an object
//! they reach stays fetchable until a ref changes there. The evictable set is the present content
//! of that closure, minus what this warehouse must keep:
//!
//! - every meta pallet, in full: trust and metadata reads must never need the network;
//! - each checkout's base parcel tree, in the main tree and in every bay: the next `stocktake`
//!   reads it, so freeing it only buys a refetch (the base itself, not its ancestry);
//! - the parcels a parked, consolidating or cherry-picking bay names, and the trust pins: their
//!   own trees, not their ancestry;
//! - every staged inventory leaf, and a chunked one's chunks: staged work must never depend on the
//!   network, even when it shares a blob with history the origin holds.
//!
//! History only this warehouse holds (unlifted work, a haul, a deleted pallet's head) is never in
//! the origin's closure, so it is never evicted, with no special case: the subtraction does it.
//! An object such history shares with the origin's is evictable, since a refill fetches it by hash
//! from history the origin holds.
//!
//! Every evicted object is recorded, not only the topmost: objects are freed children before
//! parents, and a present recorded parent seals nothing, so a frontier-only record would leave an
//! absent unrecorded child under a present parent between two deletes, which reads as damage. With
//! every one recorded first, each state a crash can leave is one the record tolerates, and running
//! `evict` again finishes the job.
//!
//! Objects a pack holds go by a full repack that keeps everything else
//! ([`pack_utils::repack_evicting`]), and loose copies are deleted after it: the repack re-encodes
//! every kept delta whose base is evicted, so it must still be able to read that base. Each one is
//! recorded by then, so the order the old packs and loose files go in does not matter.
//!
//! Last, the record is bounded ([`sealed_utils::bound`]): an evicted object whose every parent is
//! evicted too is never met by a walk, so its record is dropped, and a whole evicted history leaves
//! only the records of its root trees and of what a kept tree still names.

use std::collections::HashSet;
use crate::util::lock_utils::{ServeLock, StoreAccessExclusive, StoreLock};
use crate::util::pallet_utils::PalletNamespace;
use crate::util::progress_utils::Progress;
use crate::util::sealed_utils::{self, SealOp, SealedSet};
use crate::util::{
    bay_utils, cache_miss, cherry_pick_utils, file_utils, gc_utils, merge_utils, object_utils, office_utils,
    pack_utils, pallet_utils, park_utils, prune_utils, recovery_utils,
};

/// What an eviction would do, computed without changing anything (safe for a dry run).
pub struct EvictPlan {
    /// Every object to evict, each before every parent it has in the list.
    pub to_evict: Vec<String>,

    /// The objects of `to_evict` with a loose copy, freed in that order, so a run killed partway
    /// leaves only absences a recorded parent covers or a present parent still lists.
    pub to_free: Vec<String>,

    /// The objects of `to_evict` a pack holds, dropped by a repack. One both loose and packed is
    /// in both lists.
    pub to_drop: HashSet<String>,

    /// Whether a repack runs: `to_drop` is not empty, or a packed delta's base is an object only a
    /// loose copy holds, which the repack must re-encode before that copy is freed.
    pub repack: bool,

    /// The objects of `to_evict` the record does not hold yet, recorded before anything is freed.
    pub to_record: Vec<String>,

    /// Objects the origin holds that stay because a checkout's base tree needs them.
    pub kept_for_checkouts: usize,

    /// Objects the origin holds that stay for a meta pallet, a parked, consolidating or
    /// cherry-picking bay, a trust pin, or staged work.
    pub kept_local: usize,

    /// Present content only local history reaches, which the origin is not known to hold.
    pub not_held: usize,

    /// Origin heads this warehouse does not hold yet: the history behind each frees nothing until
    /// a `lower` brings it in.
    pub heads_not_lowered: usize,

    /// Kept objects the record still holds although they are present (an interrupted refill or
    /// eviction left them so), unsealed once the eviction is done.
    pub to_unseal: Vec<String>,

    /// The heads the origin holds, which root the record's bound beside the local roots: the
    /// parcels they reach name the trees evicted under them.
    pub held_heads: Vec<String>,
}

/// What a carried-out eviction did.
pub struct EvictStats {
    /// Objects of the plan no longer present: freed, dropped from the packs, or already gone.
    pub evicted: usize,

    /// Objects of the plan still inside a pack: one found damaged only during the repack (the plan
    /// refuses a pack already damaged), which the repack neither copies from nor deletes.
    pub still_packed: usize,

    /// The bytes freed: the loose files deleted, and what the old packs held beyond the new ones.
    pub bytes_freed: u64,

    /// Kept objects whose records were removed.
    pub unsealed: usize,

    /// Records dropped by the bound: absent objects nothing present references any more.
    pub record_dropped: usize,

    /// Why the bound dropped nothing: it met an absent object the record does not hold. Or why it
    /// could not finish, in which case `record_dropped` is `0` though the record may have shrunk.
    pub record_bound_skipped: Option<String>,
}

/// Plan an eviction under the exclusive store access the caller keeps until [`carry_out`] returns:
/// a writer must not dedup against a planned object and publish a reference to it between the plan
/// and the delete.
///
/// Nothing is read through the cache-miss hook: an absent recorded object is a boundary the walks
/// stop at, never a fetch (the hook could not take its shared hold under this exclusive one anyway).
///
/// # Arguments
/// * `held_heads`    - The heads the origin holds, from a trusted handshake: its live pallet and
///                     tombstone heads and the recorded heads it confirmed.
/// * `_store_access` - Exclusive store access, held until [`carry_out`] returns.
///
/// # Returns
/// * `Ok(EvictPlan)` - What would be freed, kept and recorded.
/// * `Err(String)`   - If an object the origin's history reaches is absent and the record does not
///                     hold it (damage: `heal` repairs the record first), or a local root, the
///                     record or the store cannot be read. Nothing is changed.
pub fn plan_evict(held_heads: &[String], _store_access: &StoreAccessExclusive) -> Result<EvictPlan, String> {
    let _unfetched = cache_miss::Unfetched::enter();
    let sealed = sealed_utils::read_sealed()?;

    let mut heads_not_lowered = 0;
    let mut distinct: HashSet<&String> = HashSet::new();
    for head in held_heads {
        if distinct.insert(head) && !file_utils::is_present_for_sweep(head)? {
            heads_not_lowered += 1;
        }
    }

    let held_parcels = sorted(gc_utils::collect_reachable_for_sweep(held_heads)?);
    let held = walk_held(&held_parcels, &sealed)?;

    let bay_dirs = bay_utils::all_bay_state_dirs()?;
    let sources = recovery_utils::gc_root_sources(
        &bay_dirs, bay_utils::BayReadPolicy::FailClosed, office_utils::TrustPinReadPolicy::FailClosed,
    )?;

    let local_parcels = sorted(gc_utils::collect_reachable_for_sweep(&sources.heads())?);
    let local = walk_local(&local_parcels, &held.seen)?;

    let mut checkout_roots: Vec<String> = Vec::new();
    let mut local_roots: Vec<String> = Vec::new();
    for dir in &bay_dirs {
        checkout_roots.extend(pallet_utils::read_position_in(dir)?.base);
        local_roots.extend(park_utils::read_parked_in(dir)?);

        if let Some(consolidation) = merge_utils::read_consolidation_state_in(dir)? {
            local_roots.push(consolidation.their_head);
            local_roots.push(consolidation.our_head);
        }

        if let Some(pick) = cherry_pick_utils::read_state_in(dir)? {
            local_roots.push(pick.source);
            local_roots.push(pick.our_head);
        }
    }
    local_roots.extend(sources.pin_parcels.into_iter().map(|(hash, _)| hash));
    local_roots.extend(meta_parcels()?);

    let mut checkout_keep: HashSet<String> = HashSet::new();
    keep_closure(root_trees(&checkout_roots)?, &mut checkout_keep)?;

    let mut local_keep: HashSet<String> = HashSet::new();
    let mut local_trees = root_trees(&local_roots)?;
    local_trees.extend(held.meta_roots);
    local_trees.extend(local.meta_roots);
    keep_closure(local_trees, &mut local_keep)?;

    for (hash, item_type) in &sources.shard_leaves {
        if local_keep.insert(hash.clone()) && item_type.is_chunked() {
            keep_present_recipe_chunks(hash, &mut local_keep)?;
        }
    }

    let mut plan = EvictPlan {
        to_evict: Vec::new(),
        to_free: Vec::new(),
        to_drop: HashSet::new(),
        repack: false,
        to_record: Vec::new(),
        kept_for_checkouts: 0,
        kept_local: 0,
        not_held: local.present,
        heads_not_lowered,
        to_unseal: Vec::new(),
        held_heads: held_heads.to_vec(),
    };

    for hash in held.present {
        if checkout_keep.contains(&hash) {
            plan.kept_for_checkouts += 1;
        } else if local_keep.contains(&hash) {
            plan.kept_local += 1;
        } else {
            // Present (the walk said so), so an object no pack holds is loose. One a pack holds
            // may have a loose copy too, which nothing else would delete.
            let packed = pack_utils::is_in_packs(&hash)?;
            if !packed || file_utils::loose_dentry_exists(&hash)? {
                plan.to_free.push(hash.clone());
            }
            if packed {
                plan.to_drop.insert(hash.clone());
            }
            plan.to_evict.push(hash);
        }
    }

    plan.to_record = plan.to_evict.iter().filter(|hash| !sealed.contains(hash)).cloned().collect();

    // A packed delta can stand on a loose-only object: one a read fetched again after a
    // compaction, which a later compaction then used as a path base. A damaged pack refuses the
    // whole run: its deltas cannot be read to say what they stand on, and the repack, which runs
    // without salvage, would leave it as found.
    let loose_only: HashSet<String> = plan.to_free.iter().filter(|hash| !plan.to_drop.contains(*hash)).cloned().collect();
    let delta_on_loose = if plan.to_evict.is_empty() {
        false
    } else {
        pack_utils::any_packed_delta_against(&loose_only).map_err(|error| match error {
            pack_utils::PackScanError::Damaged(e) => format!(
                "\"evict\" cannot run while a pack in this warehouse is damaged, since it cannot tell \
                which packed content stands on what it would free; nothing was changed. Run \
                \"compact --all\" to salvage the pack, then \"evict\" again. {}", e
            ),
            pack_utils::PackScanError::Registry(e) => e,
        })?
    };
    plan.repack = !plan.to_drop.is_empty() || delta_on_loose;
    plan.to_unseal = sealed_utils::present_recorded(&sealed)?
        .into_iter()
        .filter(|hash| checkout_keep.contains(hash) || local_keep.contains(hash))
        .collect();

    Ok(plan)
}

/// Carry out `plan`: record every object it evicts, repack without them, free the loose copies in
/// the plan's order, unseal the kept objects the record still held, then bound the record. Records
/// first: a crash after them leaves recorded objects still present, which seal nothing and read as
/// they did, and the next eviction frees them. The repack before the frees: it re-encodes every
/// kept delta whose base is evicted, and reads that base from whichever copy holds it, loose or
/// packed. The bound last, once every object it judges absent is gone; it only shrinks the record,
/// so a bound that fails is reported as skipped rather than failing a run that already freed.
///
/// Holds the locks a full repack takes, which the plan was computed under too: a repack keeps only
/// what is live when it runs, so nothing may change what is live between the plan and the delete.
///
/// # Arguments
/// * `plan`         - From [`plan_evict`], under the same hold.
/// * `store_lock`   - The object-store lock, held since before the plan.
/// * `serve_lock`   - The serve lock, held since before the plan.
/// * `store_access` - The exclusive store access the plan was computed under, still held.
/// * `progress`     - Where the repack reports its phases.
///
/// # Returns
/// * `Ok(EvictStats)` - What was evicted, freed, unsealed and dropped from the record.
/// * `Err(String)`    - If the record could not be written (nothing was freed), or a free, the
///                      repack or the unseal failed. Either way, running `evict` again finishes.
pub fn carry_out(plan: &EvictPlan,
                 store_lock: &StoreLock,
                 serve_lock: &ServeLock,
                 store_access: &StoreAccessExclusive,
                 progress: &Progress) -> Result<EvictStats, String> {
    let _unfetched = cache_miss::Unfetched::enter();

    record(plan)?;

    let mut bytes_freed = 0;
    let mut still_packed = 0;
    if plan.repack {
        // Every evicted object, loose ones too: a loose one is present while the repack runs, so
        // it must not become a target or a path base either.
        let evicted: HashSet<String> = plan.to_evict.iter().cloned().collect();
        bytes_freed += pack_utils::repack_evicting(&evicted, store_lock, serve_lock, store_access, progress)?
            .pack_bytes_freed;

        for hash in &plan.to_drop {
            if pack_utils::is_in_packs(hash)? {
                still_packed += 1;
            }
        }
    }

    bytes_freed += prune_utils::free_objects(&plan.to_free, store_access)?.bytes;

    let unsealed = if plan.to_unseal.is_empty() {
        0
    } else {
        sealed_utils::unseal(store_access, &plan.to_unseal)?.removed.len()
    };

    let bounded = sealed_utils::bound(store_access, &plan.held_heads)
        .unwrap_or_else(|error| sealed_utils::Bounded { dropped: Vec::new(), skipped: Some(error) });

    Ok(EvictStats {
        evicted: plan.to_evict.len() - still_packed,
        still_packed,
        bytes_freed,
        unsealed,
        record_dropped: bounded.dropped.len(),
        record_bound_skipped: bounded.skipped,
    })
}

/// Record every object `plan` frees that the record does not hold yet, synced before this returns.
fn record(plan: &EvictPlan) -> Result<(), String> {
    if plan.to_record.is_empty() {
        return Ok(());
    }

    let ops: Vec<(SealOp, &str)> = plan.to_record.iter().map(|hash| (SealOp::Record, hash.as_str())).collect();
    sealed_utils::append(&ops).map(|_| ())
}

/// What the walk of the origin's closure found.
struct HeldClosure {
    /// The present content objects, each before every parent it has in the list.
    present: Vec<String>,
    /// Every object met, present or not: the whole closure the origin holds.
    seen: HashSet<String>,
    /// The root trees of held parcels that are meta-shaped, kept in full.
    meta_roots: Vec<String>,
}

/// Walk the content closure of `parcels`, the parcels the origin's heads reach, children before
/// parents. An absent object the record holds is a boundary: it was evicted or never fetched, and
/// nothing beneath it is a target. An absent object the record does not hold fails the walk: the
/// record is meant to cover every absence in a cache, so this one is damage, and recording it now
/// would claim the origin holds what may simply be lost.
fn walk_held(parcels: &[String], sealed: &SealedSet) -> Result<HeldClosure, String> {
    /// One step of the explicit post-order walk: a tree to expand (with the object that named it,
    /// for the error, and whether it is a parcel's root), or one whose children are all done.
    enum Step {
        Expand { hash: String, named_by: String, root: bool },
        Finalize(String),
    }

    let mut closure = HeldClosure { present: Vec::new(), seen: HashSet::new(), meta_roots: Vec::new() };
    let mut stack: Vec<Step> = Vec::new();

    for parcel in parcels.iter().rev() {
        let tree_hash = object_utils::load_parcel(parcel)?.tree_hash;
        stack.push(Step::Expand { hash: tree_hash, named_by: parcel.clone(), root: true });
    }

    while let Some(step) = stack.pop() {
        let (hash, named_by, root) = match step {
            // Popped only after every child pushed above it is done (LIFO): the post-order spot.
            Step::Finalize(hash) => {
                closure.present.push(hash);
                continue;
            }
            Step::Expand { hash, named_by, root } => (hash, named_by, root),
        };

        if !closure.seen.insert(hash.clone()) {
            // A root met first as a subtree of another parcel was walked without the root check.
            if root {
                note_meta_root(&hash, &mut closure.meta_roots)?;
            }
            continue;
        }

        if !held_present(&hash, &named_by, sealed)? {
            continue;
        }

        let tree = object_utils::load_tree(&hash)?;
        if root && recovery_utils::walks_in_full(&tree, "") {
            closure.meta_roots.push(hash.clone());
        }

        stack.push(Step::Finalize(hash.clone()));

        for (_, file) in tree.get_files() {
            if !closure.seen.insert(file.hash.clone()) || !held_present(&file.hash, &hash, sealed)? {
                continue;
            }

            // A chunked entry names a recipe whose chunks only it reaches: they go before it, read
            // while it is still present, so a killed run that freed some leaves the rest listed.
            if file.item_type.is_chunked() {
                for chunk in object_utils::recipe_chunk_hashes(&file.hash)? {
                    if closure.seen.insert(chunk.clone()) && held_present(&chunk, &file.hash, sealed)? {
                        closure.present.push(chunk);
                    }
                }
            }

            closure.present.push(file.hash.clone());
        }

        for (_, subtree) in tree.get_subtrees() {
            stack.push(Step::Expand { hash: subtree.hash.clone(), named_by: hash.clone(), root: false });
        }
    }

    Ok(closure)
}

/// Whether `hash`, which `named_by` references in the origin's closure, is present: `false` when
/// it is absent and recorded, an error when it is absent and not.
fn held_present(hash: &str, named_by: &str, sealed: &SealedSet) -> Result<bool, String> {
    if file_utils::is_present_for_sweep(hash)? {
        return Ok(true);
    }

    if sealed.contains(hash) {
        return Ok(false);
    }

    Err(format!(
        "Object {} is missing from this cache warehouse, and its record does not say the origin holds \
        it (object {} references it). Run \"forklift heal\", then \"forklift evict\" again. Nothing was \
        changed.",
        hash, named_by
    ))
}

/// What the walk of local history outside the origin's closure found.
struct LocalClosure {
    /// How many present content objects only local history reaches.
    present: usize,
    /// The root trees of local parcels that are meta-shaped, kept in full.
    meta_roots: Vec<String>,
}

/// Add `hash` to `roots` when it is a present parcel root that walks in full (a meta pallet's shape),
/// for a root a walk reached first as an ordinary subtree, where it skipped the root check.
fn note_meta_root(hash: &str, roots: &mut Vec<String>) -> Result<(), String> {
    if !roots.iter().any(|root| root == hash)
        && file_utils::is_present_for_sweep(hash)?
        && recovery_utils::walks_in_full(&object_utils::load_tree(hash)?, "")
    {
        roots.push(hash.to_string());
    }

    Ok(())
}

/// Count the present content `parcels` reach outside `held`, the origin's closure (whatever an
/// object in it reaches is in it too, so the walk stops there). Tolerant of any absence: this walk
/// only counts, and nothing it meets is freed.
fn walk_local(parcels: &[String], held: &HashSet<String>) -> Result<LocalClosure, String> {
    let mut closure = LocalClosure { present: 0, meta_roots: Vec::new() };
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue: Vec<(String, bool)> = Vec::new();

    for parcel in parcels {
        queue.push((object_utils::load_parcel(parcel)?.tree_hash, true));
    }

    while let Some((hash, root)) = queue.pop() {
        if !seen.insert(hash.clone()) {
            continue;
        }

        if held.contains(&hash) {
            // A root the held walk reached as a subtree was not checked as one.
            if root {
                note_meta_root(&hash, &mut closure.meta_roots)?;
            }
            continue;
        }

        if !file_utils::is_present_for_sweep(&hash)? {
            continue;
        }

        closure.present += 1;
        let tree = object_utils::load_tree(&hash)?;
        if root && recovery_utils::walks_in_full(&tree, "") {
            closure.meta_roots.push(hash.clone());
        }

        for (_, file) in tree.get_files() {
            if held.contains(&file.hash) || !seen.insert(file.hash.clone()) || !file_utils::is_present_for_sweep(&file.hash)? {
                continue;
            }

            closure.present += 1;
            if file.item_type.is_chunked() {
                for chunk in object_utils::recipe_chunk_hashes(&file.hash)? {
                    if !held.contains(&chunk) && seen.insert(chunk.clone()) && file_utils::is_present_for_sweep(&chunk)? {
                        closure.present += 1;
                    }
                }
            }
        }

        for (_, subtree) in tree.get_subtrees() {
            queue.push((subtree.hash.clone(), false));
        }
    }

    Ok(closure)
}

/// The parcels every meta pallet head reaches, kept in full.
fn meta_parcels() -> Result<Vec<String>, String> {
    let heads: Vec<String> = pallet_utils::all_pallet_refs()?
        .into_iter()
        .filter(|(pallet_ref, _)| pallet_ref.namespace == PalletNamespace::Meta)
        .map(|(_, head)| head)
        .collect();

    Ok(sorted(gc_utils::collect_reachable_for_sweep(&heads)?))
}

/// The root trees of the present `parcels`. A pin may name a parcel this warehouse never had.
fn root_trees(parcels: &[String]) -> Result<Vec<String>, String> {
    let mut trees = Vec::new();

    for parcel in parcels {
        if file_utils::is_present_for_sweep(parcel)? {
            trees.push(object_utils::load_parcel(parcel)?.tree_hash);
        }
    }

    Ok(trees)
}

/// Add every present object the trees `roots` reach to `keep`, descending only present trees and
/// recipes: an absent one is already out of the store, with nothing present beneath it to keep.
fn keep_closure(roots: Vec<String>, keep: &mut HashSet<String>) -> Result<(), String> {
    let mut queue = roots;

    while let Some(hash) = queue.pop() {
        if keep.contains(&hash) || !file_utils::is_present_for_sweep(&hash)? {
            continue;
        }

        keep.insert(hash.clone());
        let tree = object_utils::load_tree(&hash)?;

        for (_, file) in tree.get_files() {
            if keep.insert(file.hash.clone()) && file.item_type.is_chunked() {
                keep_present_recipe_chunks(&file.hash, keep)?;
            }
        }

        for (_, subtree) in tree.get_subtrees() {
            queue.push(subtree.hash.clone());
        }
    }

    Ok(())
}

/// Keep every chunk of a recipe that is present; an absent recipe has nothing beneath it to keep.
fn keep_present_recipe_chunks(recipe_hash: &str, keep: &mut HashSet<String>) -> Result<(), String> {
    if !file_utils::is_present_for_sweep(recipe_hash)? {
        return Ok(());
    }

    keep.extend(object_utils::recipe_chunk_hashes(recipe_hash)?);
    Ok(())
}

fn sorted(set: HashSet<String>) -> Vec<String> {
    let mut list: Vec<String> = set.into_iter().collect();
    list.sort();
    list
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
    use crate::util::pallet_utils::BayPosition;
    use std::path::PathBuf;

    fn store_access() -> StoreAccessExclusive {
        crate::util::lock_utils::StoreAccessLock::try_exclusive().expect("no other holder in a test root")
    }

    /// [`carry_out`] under the store and serve locks `evict` takes, acquired here.
    fn carry(plan: &EvictPlan, hold: &StoreAccessExclusive) -> Result<EvictStats, String> {
        let store_lock = StoreLock::acquire().expect("no other holder in a test root");
        let serve_lock = ServeLock::acquire().expect("no other holder in a test root");
        carry_out(plan, &store_lock, &serve_lock, hold, &Progress::NONE)
    }

    /// A fresh cache warehouse root for one test, entered as the active storage-root scope.
    struct Scratch {
        _scope: StorageRootScope,
        root: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let root = std::env::temp_dir().join(format!(
                "forklift-evict-test-{}-{}-{}", name, std::process::id(), id
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = StorageRootScope::enter(&root);
            crate::util::warehouse_utils::mark_for_test(crate::util::warehouse_utils::WarehouseMode::Cache);
            pallet_utils::create_position_if_not_exists().unwrap();

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

    fn set_main(parcel: &str) {
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            parcel,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();
    }

    fn present(hash: &str) -> bool {
        file_utils::does_object_exist(hash).unwrap()
    }

    fn position(base: Option<&str>) {
        pallet_utils::write_position(&BayPosition::new("main", base.map(str::to_string))).unwrap();
    }

    /// Two parcels: `held` (lifted, `a.txt` + `dir/shared.txt`) and its child `local` (stacked
    /// here only, adding `b.txt` and keeping `dir`). The checkout stands on nothing.
    struct History {
        held: String,
        local: String,
        held_root: String,
        a: String,
        dir: String,
        shared: String,
        local_root: String,
        b: String,
    }

    fn history() -> History {
        let a = store_blob("a\n");
        let shared = store_blob("shared\n");
        let dir = store_tree(&[("shared.txt", &shared, DirEntryType::Normal)]);
        let held_root = store_tree(&[("a.txt", &a, DirEntryType::Normal), ("dir", &dir, DirEntryType::Tree)]);
        let held = store_parcel(&held_root, Vec::new());

        let b = store_blob("b\n");
        let local_root = store_tree(&[("b.txt", &b, DirEntryType::Normal), ("dir", &dir, DirEntryType::Tree)]);
        let local = store_parcel(&local_root, vec![held.clone()]);
        set_main(&local);
        position(None);

        History { held, local, held_root, a, dir, shared, local_root, b }
    }

    #[test]
    fn plan_frees_the_held_closure_children_first_and_keeps_local_only_content() {
        let _scratch = Scratch::new("held-closure");
        let h = history();
        let hold = store_access();

        let plan = plan_evict(&[h.held.clone()], &hold).unwrap();

        let freed: HashSet<&String> = plan.to_free.iter().collect();
        assert_eq!(freed, [&h.held_root, &h.a, &h.dir, &h.shared].into_iter().collect());
        let at = |hash: &String| plan.to_free.iter().position(|x| x == hash).unwrap();
        assert!(at(&h.shared) < at(&h.dir) && at(&h.dir) < at(&h.held_root) && at(&h.a) < at(&h.held_root));
        assert_eq!(plan.to_record, plan.to_free, "nothing was recorded before");
        assert_eq!(plan.not_held, 2, "the local root tree and b.txt only local history reaches");
        assert_eq!((plan.kept_for_checkouts, plan.kept_local, plan.to_drop.len(), plan.heads_not_lowered), (0, 0, 0, 0));

        let stats = carry(&plan, &hold).unwrap();
        assert_eq!(stats.evicted, 4);
        for hash in [&h.held_root, &h.a, &h.dir, &h.shared] {
            assert!(!present(hash), "{} is evicted", hash);
        }
        assert!(present(&h.local_root) && present(&h.b) && present(&h.held) && present(&h.local));

        // A second run finds nothing more to do: the evicted tree is a recorded boundary.
        let again = plan_evict(&[h.held.clone()], &hold).unwrap();
        assert!(again.to_free.is_empty() && again.to_record.is_empty());
    }

    #[test]
    fn plan_keeps_the_checkout_base_and_unseals_what_it_kept() {
        let _scratch = Scratch::new("checkout-base");
        let h = history();
        position(Some(h.local.as_str()));
        sealed_utils::append(&[(SealOp::Record, h.dir.as_str())]).unwrap();
        let hold = store_access();

        let plan = plan_evict(&[h.held.clone()], &hold).unwrap();

        assert_eq!(plan.to_free.iter().collect::<HashSet<_>>(), [&h.held_root, &h.a].into_iter().collect());
        assert_eq!(plan.kept_for_checkouts, 2, "dir and shared.txt, which the base tree holds");
        assert_eq!(plan.to_unseal, vec![h.dir.clone()]);

        let stats = carry(&plan, &hold).unwrap();
        assert_eq!(stats.unsealed, 1);
        assert!(!sealed_utils::read_sealed().unwrap().contains(&h.dir));
        assert!(present(&h.dir) && present(&h.shared));
    }

    #[test]
    fn plan_keeps_a_meta_shaped_tree_in_full() {
        let _scratch = Scratch::new("meta-shaped");
        let inner = store_blob("office\n");
        let meta_dir = store_tree(&[("record", &inner, DirEntryType::Normal)]);
        let root = store_tree(&[(crate::globals::FOLDER_NAME_FORKLIFT_ROOT, &meta_dir, DirEntryType::Tree)]);
        let parcel = store_parcel(&root, Vec::new());
        let hold = store_access();

        let plan = plan_evict(&[parcel], &hold).unwrap();

        assert!(plan.to_free.is_empty());
        assert_eq!(plan.kept_local, 3);
    }

    /// A meta-shaped root that another held parcel also holds as an ordinary subtree is kept in
    /// full whichever parcel the walk meets first.
    #[test]
    fn plan_keeps_a_meta_shaped_root_another_parcel_holds_as_a_subtree() {
        let _scratch = Scratch::new("meta-shared");
        let inner = store_blob("office\n");
        let meta_dir = store_tree(&[("record", &inner, DirEntryType::Normal)]);
        let meta_root = store_tree(&[(crate::globals::FOLDER_NAME_FORKLIFT_ROOT, &meta_dir, DirEntryType::Tree)]);
        let meta = store_parcel(&meta_root, Vec::new());
        let user_root = store_tree(&[("nested", &meta_root, DirEntryType::Tree)]);
        let user = store_parcel(&user_root, Vec::new());
        let hold = store_access();

        for heads in [[user.clone(), meta.clone()], [meta.clone(), user.clone()]] {
            let plan = plan_evict(&heads, &hold).unwrap();
            assert_eq!(plan.to_free, vec![user_root.clone()], "only the user root goes");
            assert_eq!(plan.kept_local, 3);
        }

        // Held only through the user parcel, while local history (a deleted meta pallet's head, say)
        // holds it as a root: the local walk stops at the held closure, yet still names the root.
        let held: HashSet<String> = [user_root.clone(), meta_root.clone(), meta_dir.clone(), inner.clone()].into();
        assert_eq!(walk_local(&[meta.clone()], &held).unwrap().meta_roots, vec![meta_root.clone()]);
    }

    #[test]
    fn plan_refuses_an_unrecorded_absence_and_passes_a_recorded_one() {
        let _scratch = Scratch::new("absence");
        let h = history();
        let (folder, file) = file_utils::get_path_for_object(&h.shared).unwrap();
        std::fs::remove_file(PathBuf::from(folder).join(file)).unwrap();
        let hold = store_access();

        let error = plan_evict(&[h.held.clone()], &hold).err().unwrap();
        assert!(error.contains(&h.shared) && error.contains("forklift heal"), "{}", error);

        sealed_utils::append(&[(SealOp::Record, h.shared.as_str())]).unwrap();
        let plan = plan_evict(&[h.held.clone()], &hold).unwrap();
        assert!(!plan.to_free.contains(&h.shared) && plan.to_free.contains(&h.dir));
    }

    #[test]
    fn plan_counts_an_origin_head_this_warehouse_lacks() {
        let _scratch = Scratch::new("not-lowered");
        let h = history();
        let hold = store_access();

        let plan = plan_evict(&[h.held.clone(), "e".repeat(64)], &hold).unwrap();
        assert_eq!(plan.heads_not_lowered, 1);
        assert_eq!(plan.to_free.len(), 4);
    }

    /// The record holds every object before the first one is freed: a free that fails at once
    /// leaves them all recorded and all present.
    #[cfg(unix)]
    #[test]
    fn carry_out_records_everything_before_freeing_anything() {
        use std::os::unix::fs::PermissionsExt;

        let _scratch = Scratch::new("record-first");
        let h = history();
        let hold = store_access();
        let plan = plan_evict(&[h.held.clone()], &hold).unwrap();

        let (folder, _) = file_utils::get_path_for_object(&plan.to_free[0]).unwrap();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();
        let outcome = carry(&plan, &hold);
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(outcome.is_err());
        let record = sealed_utils::read_sealed().unwrap();
        for hash in &plan.to_free {
            assert!(record.contains(hash), "{} is recorded before any free", hash);
        }
        assert!(present(&plan.to_free[0]));
    }

    /// Text that barely compresses, so a delta between two versions beats a full record, ending in
    /// `tail`.
    fn version(tail: &str) -> String {
        let lines: String = (0..200).map(|line| format!("{}\n", blake3::hash(format!("line {}", line).as_bytes()).to_hex())).collect();
        format!("{}{}\n", lines, tail)
    }

    /// Stores `content` as a blob in a pack of its own: in full, or as a delta against `base`.
    fn pack_blob(content: &str, base: Option<(&str, &str)>) -> String {
        let object = LooseObjectBuilder::build_blob(&Blob { content: content.as_bytes().to_vec() });
        let built_base = base.map(|(hash, bytes)| {
            (hash, LooseObjectBuilder::build_blob(&Blob { content: bytes.as_bytes().to_vec() }).content)
        });
        let mut ingest = pack_utils::StoreIngest::new().unwrap();
        let stored = ingest.store_with_base(&object, built_base.as_ref().map(|(hash, bytes)| {
            pack_utils::IngestBase { hash, bytes, depth: 0 }
        })).unwrap();
        assert_eq!(matches!(stored, pack_utils::IngestStored::Delta { .. }), base.is_some(), "the fixture's shape");
        ingest.finish().unwrap();
        object.hash
    }

    /// Four versions of `f.txt`, one per parcel, each parcel on the last: `v0` and `v1` lifted,
    /// `v2` and `v3` stacked here only. Every object is packed; `v2` is a delta against `v0` and `v3`
    /// a delta against `v2`. So evicting the lifted history drops both `v2`'s old base and its
    /// previous version at its path, and `v3` is a copied delta whose base is re-encoded.
    struct Versions {
        held: String,
        v: [String; 4],
        text: [String; 4],
        roots: Vec<String>,
    }

    fn versions() -> Versions {
        let text = [version("zero"), version("one"), version("two"), version("three")];
        let v0 = pack_blob(&text[0], None);
        let v1 = pack_blob(&text[1], None);
        let v2 = pack_blob(&text[2], Some((&v0, &text[0])));
        let v3 = pack_blob(&text[3], Some((&v2, &text[2])));

        let mut parents = Vec::new();
        let mut held = String::new();
        let mut roots = Vec::new();
        for (index, blob) in [&v0, &v1, &v2, &v3].into_iter().enumerate() {
            let root = store_tree(&[("f.txt", blob, DirEntryType::Normal)]);
            let parcel = store_parcel(&root, parents);
            roots.push(root);
            if index == 1 {
                held = parcel.clone();
            }
            parents = vec![parcel];
        }
        set_main(&parents[0]);
        position(None);
        // The trees too, as a cache filled by reads and then compacted holds them: the repack's path
        // walk then meets every evicted tree in an old pack, where only its filter stops it.
        pack_utils::compact(false, false, false, &Progress::NONE).unwrap();

        Versions { held, v: [v0, v1, v2, v3], text, roots }
    }

    /// The bytes `hash` reads as from the disk alone: every cache this process holds is dropped
    /// first, so nothing reconstructed against a deleted pack can answer.
    fn read_fresh(hash: &str) -> Result<Vec<u8>, String> {
        pack_utils::forget_warehouse();
        file_utils::retrieve_object_by_hash_uncached(hash)
    }

    /// The packed versions the lifted history holds are evicted by a repack, and the kept ones read
    /// back from the disk alone once the old packs are gone: `v2`, re-encoded because its base is
    /// dropped, whose previous version at its path is dropped too, and `v3`, a copied delta whose
    /// base was re-encoded. The kept root trees read too, whose previous versions are evicted.
    ///
    /// Revert: let an evicted blob be a path base → `v2` is re-encoded against `v1`, which the
    /// repack deletes, and no longer reads; an evicted tree → the third root, against the second.
    #[test]
    fn a_repack_drops_the_packed_versions_and_keeps_every_kept_version_readable() {
        let _scratch = Scratch::new("packed-versions");
        let f = versions();
        let hold = store_access();

        let plan = plan_evict(&[f.held.clone()], &hold).unwrap();
        assert!(plan.to_drop.contains(&f.v[0]) && plan.to_drop.contains(&f.v[1]), "{:?}", plan.to_drop);
        assert!(!plan.to_free.contains(&f.v[0]) && !plan.to_free.contains(&f.v[1]), "neither is loose");

        let stats = carry(&plan, &hold).unwrap();
        assert_eq!((stats.evicted, stats.still_packed), (plan.to_evict.len(), 0));
        assert!(stats.bytes_freed > 0);

        let record = sealed_utils::read_sealed().unwrap();
        for evicted in [&f.v[0], &f.v[1]] {
            assert!(!pack_utils::is_in_packs(evicted).unwrap() && read_fresh(evicted).is_err(), "{} is gone", evicted);
        }
        for root in &f.roots[..2] {
            assert!(record.contains(root), "the evicted root {} stays recorded under its parcel", root);
        }
        for kept in [2, 3] {
            let bytes = read_fresh(&f.v[kept]).unwrap_or_else(|e| panic!("v{} reads: {}", kept, e));
            let expected = LooseObjectBuilder::build_blob(&Blob { content: f.text[kept].as_bytes().to_vec() }).content;
            assert!(bytes == expected, "v{} reads back as written", kept);
            read_fresh(&f.roots[kept]).unwrap_or_else(|e| panic!("root {} reads: {}", kept, e));
        }

        let again = plan_evict(&[f.held.clone()], &hold).unwrap();
        assert!(again.to_evict.is_empty() && again.to_drop.is_empty(), "a second run has nothing to do");
    }

    /// A kept packed delta whose base only a loose copy holds survives that copy's eviction: a
    /// compaction in a cache can use an object a read fetched again as a path base, leaving it
    /// loose. The repack runs for it though no pack holds an evicted object, and before the free.
    ///
    /// Revert: repack only when a pack holds an evicted object → the delta's base is freed and it
    /// no longer reads; free before repacking → the repack cannot read the base and fails.
    #[test]
    fn a_kept_delta_on_a_loose_evicted_base_is_re_encoded_before_the_base_goes() {
        let _scratch = Scratch::new("loose-base");
        let text = [version("held"), version("local")];
        let held_blob = store_blob(&text[0]);
        let local_blob = pack_blob(&text[1], Some((&held_blob, &text[0])));
        let held_root = store_tree(&[("f.txt", &held_blob, DirEntryType::Normal)]);
        let held = store_parcel(&held_root, Vec::new());
        let local_root = store_tree(&[("f.txt", &local_blob, DirEntryType::Normal)]);
        set_main(&store_parcel(&local_root, vec![held.clone()]));
        position(None);
        let hold = store_access();

        let plan = plan_evict(&[held.clone()], &hold).unwrap();
        assert!(plan.to_drop.is_empty() && plan.to_free.contains(&held_blob), "sanity: the base is loose only");
        assert!(plan.repack, "a packed delta stands on it");

        carry(&plan, &hold).unwrap();
        assert!(read_fresh(&held_blob).is_err(), "the base is evicted");
        let expected = LooseObjectBuilder::build_blob(&Blob { content: text[1].as_bytes().to_vec() }).content;
        assert!(read_fresh(&local_blob).unwrap() == expected, "the kept delta reads back as written");
    }

    /// An evictable object both loose and packed loses both copies: the loose one is freed, the
    /// packed one dropped.
    ///
    /// Revert: free only objects no pack holds → the loose copy stays.
    #[test]
    fn an_object_both_loose_and_packed_loses_both_copies() {
        let _scratch = Scratch::new("loose-and-packed");
        let h = history();
        let (folder, file) = file_utils::get_path_for_object(&h.a).unwrap();
        let loose = PathBuf::from(folder).join(file);
        let bytes = std::fs::read(&loose).unwrap();
        pack_utils::compact(false, false, false, &Progress::NONE).unwrap();
        assert!(!loose.exists(), "sanity: compaction moved it into a pack");
        std::fs::write(&loose, &bytes).unwrap();
        let hold = store_access();

        let plan = plan_evict(&[h.held.clone()], &hold).unwrap();
        assert!(plan.to_free.contains(&h.a) && plan.to_drop.contains(&h.a), "both copies are planned");
        assert_eq!(plan.to_free, vec![h.a.clone()], "every other object is packed only");

        carry(&plan, &hold).unwrap();
        assert!(!loose.exists() && !pack_utils::is_in_packs(&h.a).unwrap());
        assert!(present(&h.local_root) && present(&h.b), "what only local history reaches stays");
    }

    /// `bytes_freed` counts a dropped pack though the repack also packs kept loose content: the new
    /// pack holds that content, which its loose files held before.
    ///
    /// Revert: leave the packed loose files off the old side → the new pack outweighs the old one
    /// and the drop counts as nothing.
    #[test]
    fn bytes_freed_counts_a_drop_while_the_repack_packs_kept_loose_content() {
        let _scratch = Scratch::new("bytes-freed");
        let held_blob = pack_blob(&version("held"), None);
        let pack_bytes: u64 = std::fs::read_dir(pack_utils::pack_folder()).unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum();
        let held_root = store_tree(&[("f.txt", &held_blob, DirEntryType::Normal)]);
        let held = store_parcel(&held_root, Vec::new());
        let local_text: String = (0..2000)
            .map(|line| format!("{}\n", blake3::hash(format!("local {}", line).as_bytes()).to_hex()))
            .collect();
        let local_blob = store_blob(&local_text);
        let local_root = store_tree(&[("f.txt", &local_blob, DirEntryType::Normal)]);
        set_main(&store_parcel(&local_root, vec![held.clone()]));
        position(None);
        let hold = store_access();

        let plan = plan_evict(&[held.clone()], &hold).unwrap();
        assert!(plan.to_drop.contains(&held_blob) && plan.repack, "sanity: the held blob is dropped");
        let stats = carry(&plan, &hold).unwrap();
        assert!(stats.bytes_freed * 2 >= pack_bytes, "{} freed, the dropped pack held {}", stats.bytes_freed, pack_bytes);
        assert!(present(&local_blob) && !file_utils::loose_dentry_exists(&local_blob).unwrap(), "sanity: packed");
    }

    /// An incremental compaction reports no pack bytes freed, though the pack it writes holds less
    /// than the loose files it packed: it frees no pack.
    ///
    /// Revert: count packed loose files on the old side for every compaction → it reports the
    /// delta's saving as freed pack bytes.
    #[test]
    fn an_incremental_compaction_frees_no_pack_bytes() {
        let _scratch = Scratch::new("incremental");
        let mut parents = Vec::new();
        for tail in ["zero", "one", "two", "three"] {
            let root = store_tree(&[("f.txt", &store_blob(&version(tail)), DirEntryType::Normal)]);
            parents = vec![store_parcel(&root, parents)];
        }
        set_main(&parents[0]);
        position(None);

        let stats = pack_utils::compact(false, false, false, &Progress::NONE).unwrap();
        assert!(stats.loose_removed > 0, "sanity: it packed the loose versions");
        assert_eq!(stats.pack_bytes_freed, 0);
    }

    /// A damaged pack refuses the plan before anything changes, whether or not a pack holds an
    /// evicted object: its deltas cannot say what they stand on, and the repack would leave it.
    ///
    /// Revert: pass over a pack whose index fails → the loose case plans; ask only when no pack
    /// holds an evicted object → the packed case plans.
    #[test]
    fn a_damaged_pack_refuses_the_plan() {
        for packed in [false, true] {
            let _scratch = Scratch::new(if packed { "damaged-packed" } else { "damaged-loose" });
            let held = if packed { versions().held } else { history().held };
            pack_utils::tests::pack_as_unreadable(&store_blob("stray\n"), pack_utils::tests::Unreadable::CorruptBucket);
            let hold = store_access();

            let Err(error) = plan_evict(&[held], &hold) else { panic!("packed {}: a damaged pack refuses", packed) };
            assert!(error.contains("damaged") && error.contains("nothing was changed"), "packed {}: {}", packed, error);
            assert!(sealed_utils::read_sealed().unwrap().is_empty(), "packed {}: nothing is recorded", packed);
        }
    }

    /// A repack that evicts keeps the signature of every parcel: parcels are never evicted.
    ///
    /// Revert: drop the evicted set's parcels' signatures too → a held parcel's signature is gone.
    #[test]
    fn a_repack_that_evicts_keeps_every_parcel_signature() {
        let _scratch = Scratch::new("signatures");
        let h = history();
        let signature = |seed: u8| {
            let key_id = b"test-key";
            let mut bytes = crate::util::byte_utils::number_to_vlq_bytes(1);
            bytes.extend(crate::util::byte_utils::number_to_vlq_bytes(key_id.len() as u64));
            bytes.extend_from_slice(key_id);
            bytes.extend(crate::util::byte_utils::number_to_vlq_bytes(64));
            bytes.extend_from_slice(&[seed; 64]);
            bytes
        };
        for (parcel, seed) in [(&h.held, 1), (&h.local, 2)] {
            crate::util::sign_utils::store_raw_parcel_signature(parcel, &signature(seed)).unwrap();
        }
        pack_utils::compact(false, false, false, &Progress::NONE).unwrap();
        let hold = store_access();

        let plan = plan_evict(&[h.held.clone()], &hold).unwrap();
        assert!(!plan.to_drop.is_empty(), "sanity: the repack runs");
        carry(&plan, &hold).unwrap();

        for (parcel, seed) in [(&h.held, 1), (&h.local, 2)] {
            assert_eq!(crate::util::sign_utils::load_raw_parcel_signature(parcel).unwrap(), Some(signature(seed)), "{}", parcel);
        }
    }

    /// Whether the record holds exactly `hashes`.
    fn assert_record(hashes: &[&String], why: &str) {
        let record = sealed_utils::read_sealed().unwrap();
        for hash in hashes {
            assert!(record.contains(hash), "{}: {} is recorded", why, hash);
        }
        assert_eq!(record.len(), hashes.len(), "{}: nothing else is recorded", why);
    }

    /// Evicting the lifted history leaves the records of its root tree, which its parcel names, and
    /// of `dir`, which the local root tree still names; `a.txt` and `dir/shared.txt` sit under
    /// evicted parents only and their records go.
    ///
    /// Revert: note no child of a present tree as referenced → `dir`'s record goes; root the walk at
    /// the origin's heads only → so does it, since only the local parcel reaches the local root;
    /// note no parcel's tree → the root tree's record goes.
    #[test]
    fn the_bound_keeps_only_what_a_present_object_names() {
        let _scratch = Scratch::new("bound");
        let h = history();
        let hold = store_access();

        let plan = plan_evict(&[h.held.clone()], &hold).unwrap();
        let stats = carry(&plan, &hold).unwrap();

        assert_eq!((stats.record_dropped, stats.record_bound_skipped.as_deref()), (2, None));
        assert_record(&[&h.held_root, &h.dir], "after the bound");
        let bytes = std::fs::read(sealed_utils::sealed_path()).unwrap();
        assert_eq!(bytes.len(), sealed_utils::header_bytes().len() + sealed_utils::frame_len(2), "one compacted frame of two entries");
    }

    /// A bound that cannot run fails nothing the run already did: the bay's position is unreadable
    /// once the plan is made, so the bound's root read fails, and the eviction still reports what
    /// it freed.
    ///
    /// Revert: let the bound's error fail the run → `carry_out` returns it.
    #[test]
    fn a_bound_that_cannot_run_is_reported_and_the_eviction_stands() {
        let _scratch = Scratch::new("bound-fails");
        let h = history();
        let hold = store_access();
        let plan = plan_evict(&[h.held.clone()], &hold).unwrap();
        assert!(!plan.repack, "sanity: no repack reads the bays after the plan");

        let mut broken = 0;
        for dir in bay_utils::all_bay_state_dirs().unwrap() {
            let position = dir.join(pallet_utils::FILE_NAME_POSITION);
            if position.exists() {
                std::fs::write(&position, b"garbage").unwrap();
                broken += 1;
            }
        }
        assert!(broken > 0, "sanity: a bay position was broken");

        let stats = carry(&plan, &hold).expect("the eviction stands");

        assert_eq!(stats.evicted, plan.to_evict.len());
        assert_eq!(stats.record_dropped, 0);
        let skipped = stats.record_bound_skipped.expect("the bound reports why it did not run");
        assert!(skipped.contains("position"), "{}", skipped);
        assert!(!file_utils::is_present_for_sweep(&h.held_root).unwrap(), "the root tree is still freed");
    }

    /// A refill of the evicted root tree records its children again before its own record goes:
    /// `a.txt`, whose record the bound dropped, is held by hash once more under the present root.
    ///
    /// Revert: skip the push-down for a child the record does not hold → `a.txt` is absent under a
    /// present tree and unrecorded.
    #[test]
    fn a_refill_after_the_bound_records_the_dropped_children_again() {
        let _scratch = Scratch::new("bound-refill");
        let h = history();
        let root_bytes = file_utils::retrieve_object_by_hash(&h.held_root).unwrap();
        let hold = store_access();

        carry(&plan_evict(&[h.held.clone()], &hold).unwrap(), &hold).unwrap();
        assert_record(&[&h.held_root, &h.dir], "sanity: bounded");

        sealed_utils::refill_all(&hold, &[(h.held_root.as_str(), root_bytes.as_slice())]).unwrap();

        assert!(present(&h.held_root));
        assert_record(&[&h.a, &h.dir], "after the refill");
    }

    /// An absent object the record does not hold, met under a present tree, stops the bound from
    /// dropping anything; the record is still compacted.
    ///
    /// Revert: walk past an unrecorded absence as a recorded one → `a.txt` and `dir/shared.txt`
    /// lose their records.
    #[test]
    fn the_bound_drops_nothing_past_an_unrecorded_absence() {
        let _scratch = Scratch::new("bound-damage");
        let h = history();
        // Only local history reaches `b.txt`, so the plan never meets its absence.
        let (folder, file) = file_utils::get_path_for_object(&h.b).unwrap();
        std::fs::remove_file(PathBuf::from(folder).join(file)).unwrap();
        let hold = store_access();

        let stats = carry(&plan_evict(&[h.held.clone()], &hold).unwrap(), &hold).unwrap();

        let skipped = stats.record_bound_skipped.expect("the bound is skipped");
        assert!(skipped.contains(&h.b) && skipped.contains(&h.local_root), "{}", skipped);
        assert_eq!(stats.record_dropped, 0);
        assert_record(&[&h.held_root, &h.a, &h.dir, &h.shared], "nothing dropped");
    }

    /// The bound keeps the record of an absent object a staged entry names, an absent parent a
    /// present parcel names, and a present object nothing names (unsealing it is `heal`'s and the
    /// plan's job, with its push-down), and drops one nothing names.
    ///
    /// Revert: leave the staged leaves out of the roots → the staged blob's record goes; leave the
    /// parcels' absent parents out → the parent's; drop unreferenced records whatever the store
    /// holds → the present blob's. The dropped one shows the walk drops at all.
    #[test]
    fn the_bound_keeps_what_staged_work_or_a_parcel_names_and_whatever_is_present() {
        let _scratch = Scratch::new("bound-roots");
        let staged = store_blob("staged\n");
        let (folder, file) = file_utils::get_path_for_object(&staged).unwrap();
        std::fs::remove_file(PathBuf::from(folder).join(file)).unwrap();
        let kid = store_blob("kid\n");
        let (folder, file) = file_utils::get_path_for_object(&kid).unwrap();
        std::fs::remove_file(PathBuf::from(folder).join(file)).unwrap();
        let staged_dir = store_tree(&[("kid.txt", &kid, DirEntryType::Normal)]);
        crate::util::gc_utils::tests::plant_staged_shard(
            "b", &[("s.txt", &staged, DirEntryType::Normal), ("d", &staged_dir, DirEntryType::Tree)],
        );

        // Named by the caller: a ref refuses a head whose history is incomplete.
        let parent = "e".repeat(64);
        let parcel = store_parcel(&store_tree(&[]), vec![parent.clone()]);

        let stray = store_blob("stray\n");
        let gone = "f".repeat(64);
        sealed_utils::append(&[
            (SealOp::Record, staged.as_str()),
            (SealOp::Record, kid.as_str()),
            (SealOp::Record, parent.as_str()),
            (SealOp::Record, stray.as_str()),
            (SealOp::Record, gone.as_str()),
        ]).unwrap();
        let hold = store_access();

        let bounded = sealed_utils::bound(&hold, &[parcel]).unwrap();

        assert_eq!((bounded.dropped, bounded.skipped), (vec![gone.clone()], None));
        assert_record(&[&staged, &kid, &parent, &stray], "kept");
    }
}
