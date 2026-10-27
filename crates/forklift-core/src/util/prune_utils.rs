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
//! safe: an object the pruned path shares with a still-fetched path, with any meta pallet, or
//! with any bay's staged, parked or consolidating work is retained, never freed. Meta pallets
//! (office and the rest) are walked in full and never scoped — the carve-out this feature
//! depends on.
//!
//! Sealed is only safe for what the remote holds, since a sealed object comes back only by
//! fetching it. So the prune also keeps everything under the pruned path that history the
//! remote's heads do not reach needs: work stacked here and never lifted, and the parents its
//! lift compares against (FORK-251, [`retain_unlifted`]).
//!
//! A chunked large file (§9.4b) reaches one level deeper: its tree entry names a *recipe*, whose
//! chunks are content-addressed objects reachable only through it. Freeing such a file frees its
//! recipe *and* every chunk — the recipe is descended while still present to enumerate them, and
//! the chunks are freed before the recipe (children before parent, so a killed run stays
//! resumable). Symmetrically, the retained set descends every in-scope chunked file's recipe too,
//! so a chunk an out-of-scope pruned file shares with an in-scope kept file is retained, never
//! freed — the shared-content guarantee, one level below the shared-blob one.
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

use std::collections::{HashMap, HashSet, VecDeque};
use crate::enums::dir_entry_type::DirEntryType;
use crate::util::pallet_utils::PalletNamespace;
use crate::util::scope_utils::{MaterializationScope, ScopeClass};
use crate::util::sealed_utils::{self, SealedSet};
use crate::util::{
    bay_utils, file_utils, gc_utils, object_utils, office_utils, pack_utils, pallet_utils, recovery_utils,
};
use crate::util::lock_utils::StoreAccessExclusive;

/// The reclamation a prune would carry out, computed without mutating anything (safe for a
/// dry run).
pub struct PrunePlan {
    /// Present, loose objects to delete: under a pruned path, and needed by no retained scope.
    pub to_free: Vec<String>,

    /// Candidates present only inside a pack. A loose delete cannot reclaim a packed object
    /// (that is a repack concern), so they are counted and reported, never silently dropped.
    pub still_packed: usize,

    /// Candidates kept because they are shared (by content hash) with a still-retained scope
    /// or a meta pallet. Distinct from `still_packed`: this is content that stays *by design*,
    /// not content a repack could someday reclaim — so a caller reporting "nothing was freed"
    /// can tell the two apart instead of leaving the reason unstated.
    pub retained_shared: usize,

    /// Candidates kept because lifting history the remote is not known to hold reads them: work
    /// stacked here and never lifted, or lifted to a remote head the remote did not confirm it
    /// still holds. Some may be the only copy; the rest are the base trees that lift compares against.
    /// A later prune of the same path frees them once the remote's heads reach that history.
    /// Excludes what only a haul's, the git mirror's or a deleted pallet's history reads, counted
    /// in `kept_for_hauls`, `kept_for_mirror` and `kept_for_deleted`.
    pub kept_unlifted: usize,

    /// Candidates kept because lifting the unlifted history a haul not yet merged proposes reads
    /// them, where no pallet reaches it any more. Lifting cannot free these; merging the haul
    /// puts that history on its target, and a prune after the target is lifted frees them. A
    /// closed haul keeps them, since reopening it proposes the same head.
    pub kept_for_hauls: usize,

    /// Candidates kept because the git mirror's roots hold history the remote is not known to
    /// hold, where no pallet or haul reaches it. The mirror only exports what the remote held, so
    /// this is normally zero; a later prune frees them once the remote holds that history again.
    pub kept_for_mirror: usize,

    /// Candidates kept because lifting the unlifted history of a pallet deleted here, which
    /// `undo` can still put back, reads them. Lifting cannot free these; a later prune does once
    /// the delete can no longer be undone.
    pub kept_for_deleted: usize,

    /// The user pallets, as wire names, whose unlifted history changed a pruned path, so that
    /// lifting them (or lowering) lets a later prune free what this one kept. Empty when
    /// `kept_unlifted` is zero, and possibly empty otherwise when gaps in the history hide which
    /// pallet leads to the kept work. A pallet with unlifted work elsewhere is not listed.
    pub unlifted_pallets: Vec<String>,
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
/// * `remote_heads`    - The remote's pallet heads, from a handshake whose trust was checked, and
///   the recorded heads it confirmed it still holds. History they reach is what the remote holds;
///   every other parcel keeps its content under the pruned path (see [`retain_unlifted`]).
/// * `removed_heads`   - The heads of pallets deleted here whose delete can still be undone
///   (`journal_utils::removed_heads`). They join the local pallet heads, never `remote_heads`:
///   their unlifted history may be the only copy.
/// * `_store_access`   - Exclusive store access, held from before the caller read the fetch scope
///   until `free_objects` returns: a writer in another bay must not dedup against a planned
///   object and publish a reference to it between the plan and the deletion (FORK-150).
pub fn plan_prune(
    pruned_prefixes: &[String],
    post_prune: &MaterializationScope,
    remote_heads: &[String],
    removed_heads: &[String],
    _store_access: &StoreAccessExclusive,
) -> Result<PrunePlan, String> {
    plan_from(pruned_prefixes, post_prune, remote_heads, removed_heads, root_sources()?)
}

/// [`plan_prune`] over root sources already read, the haul heads among them: one read serves
/// both the history a haul holds and the local roots.
fn plan_from(
    pruned_prefixes: &[String],
    post_prune: &MaterializationScope,
    remote_heads: &[String],
    removed_heads: &[String],
    sources: recovery_utils::GcRootSources,
) -> Result<PrunePlan, String> {
    let (live_parcels, meta_parcels) = reachable_parcels()?;
    let sealed = sealed_utils::read_sealed().map_err(|error| format!(
        "{} Restore it from a good copy, or use the version of Forklift that wrote it, then run the \
        prune again.",
        error
    ))?;
    // A haul's proposed head joins the local pallet heads like a removed one: a merge reads the
    // recorded head, which no ref need still reach.
    let proposed = gc_utils::collect_reachable_for_sweep(&sources.haul_heads)?;
    // A tombstone of a served root holds a deleted pallet's head as this delete's removed heads do.
    let removed: Vec<String> = removed_heads.iter().chain(&sources.tombstone_heads).cloned().collect();
    let removed = gc_utils::collect_reachable_for_sweep(&removed)?;
    // The git mirror's roots are held the same way: the mirror exports only what the remote held,
    // but a remote reset since would leave the only copy here.
    let mirrored = gc_utils::collect_reachable_for_sweep(&sources.mirror_roots)?;
    let user_parcels = with_held(&live_parcels, proposed.iter().chain(&mirrored).chain(&removed));
    let local = local_roots(&user_parcels, &meta_parcels, sources)?;

    let mut retained = collect_retained(&user_parcels, &meta_parcels, post_prune, &sealed)?;
    retain_local_closure(&local, post_prune, &mut retained)?;

    let unlifted = unlifted_parcels(&user_parcels, remote_heads)?;

    // The live pallets' unlifted history, a haul's, the mirror's and a deleted pallet's are judged
    // apart, each once: the report counts the last three apart because lifting cannot free them.
    // A parcel several hold counts for the first of haul, mirror, delete. retain_unlifted judges one parcel at a time,
    // so the keeps together are exactly the keep of the whole.
    let live: HashSet<&String> = live_parcels.iter().collect();
    let (live_unlifted, held_unlifted): (HashSet<String>, HashSet<String>) =
        unlifted.iter().cloned().partition(|parcel| live.contains(parcel));
    let (haul_unlifted, rest_unlifted): (HashSet<String>, HashSet<String>) =
        held_unlifted.into_iter().partition(|parcel| proposed.contains(parcel));
    let (mirror_unlifted, deleted_unlifted): (HashSet<String>, HashSet<String>) =
        rest_unlifted.into_iter().partition(|parcel| mirrored.contains(parcel));
    let (kept_for_live, mut touching) = retain_unlifted(&live_unlifted, pruned_prefixes, &sealed)?;
    let (kept_for_proposed, touching_proposed) = retain_unlifted(&haul_unlifted, pruned_prefixes, &sealed)?;
    let (kept_for_mirrored, touching_mirrored) = retain_unlifted(&mirror_unlifted, pruned_prefixes, &sealed)?;
    let (kept_for_removed, touching_removed) = retain_unlifted(&deleted_unlifted, pruned_prefixes, &sealed)?;
    touching.extend(touching_proposed);
    touching.extend(touching_mirrored);
    touching.extend(touching_removed);

    // Ordered children-before-parents (see the module doc): filtering this sequence (never
    // reordering it) preserves that property in `to_free`, which is what makes a partial
    // `free_objects` run — and a later re-`plan_prune` call over the same path — resumable.
    let targets = collect_prune_targets(&user_parcels, pruned_prefixes, &sealed)?;

    let mut to_free: Vec<String> = Vec::new();
    let mut still_packed = 0usize;
    let mut retained_shared = 0usize;
    let mut kept_unlifted = 0usize;
    let mut kept_for_hauls = 0usize;
    let mut kept_for_mirror = 0usize;
    let mut kept_for_deleted = 0usize;

    for hash in &targets {
        // Shared with a retained path by content-addressing (or with a meta pallet): must stay.
        if retained.contains(hash) {
            retained_shared += 1;
            continue;
        }

        // A candidate present in a pack cannot be reclaimed by a loose delete; count it. Checked
        // before the unlifted keep: lifting would not make a packed object freeable either.
        if pack_utils::is_in_packs(hash)? {
            still_packed += 1;
            continue;
        }

        // Lifting history the remote is not known to hold reads it: it may be the only copy.
        if kept_for_live.contains(hash) {
            kept_unlifted += 1;
            continue;
        }

        if kept_for_proposed.contains(hash) {
            kept_for_hauls += 1;
            continue;
        }

        if kept_for_mirrored.contains(hash) {
            kept_for_mirror += 1;
            continue;
        }

        if kept_for_removed.contains(hash) {
            kept_for_deleted += 1;
            continue;
        }

        // Present loose and reclaimable. (An object neither packed nor loose is already gone —
        // freed by an earlier, interrupted prune — and needs no action; `collect_prune_targets`
        // already excluded it, having found it absent during its own walk.)
        if loose_object_exists(hash)? {
            to_free.push(hash.clone());
        }
    }

    let mut unlifted_pallets: Vec<String> = Vec::new();

    if kept_unlifted > 0 {
        let leading = leading_to_touching(&unlifted, &touching)?;

        for (pallet_ref, head) in pallet_utils::all_pallet_refs()? {
            if pallet_ref.namespace == PalletNamespace::User && leading.contains(&head) {
                unlifted_pallets.push(pallet_ref.to_wire());
            }
        }
    }

    Ok(PrunePlan { to_free, still_packed, retained_shared, kept_unlifted, kept_for_hauls, kept_for_mirror, kept_for_deleted, unlifted_pallets })
}

/// `live_parcels`, then every `held` parcel they do not reach, once: the user history a prune
/// judges once deleted pallets that can still be restored, and hauls not yet merged, count as
/// pallets.
fn with_held<'a>(live_parcels: &[String], held: impl Iterator<Item = &'a String>) -> Vec<String> {
    let mut seen: HashSet<&String> = live_parcels.iter().collect();
    let extra: Vec<String> = held.filter(|parcel| seen.insert(*parcel)).cloned().collect();

    live_parcels.iter().cloned().chain(extra).collect()
}

/// The user-pallet parcels the remote is not known to hold: every one that no remote head
/// present here reaches. A remote holds the full closure of each of its ref heads, since its
/// commit gate checks the closure before a ref moves, and of each head it has just confirmed it
/// still holds. So history those heads reach can be fetched again, and nothing else is known to
/// be: a parcel stacked here and never lifted, or one lifted to a head the remote did not
/// confirm.
///
/// Conservative: a remote head absent here reaches nothing, which only counts more parcels as
/// unlifted, and that frees less, never more.
///
/// # Arguments
/// * `user_parcels` - The parcels the user pallet heads reach.
/// * `remote_heads` - The remote's pallet heads, and the recorded heads it confirmed.
fn unlifted_parcels(user_parcels: &[String], remote_heads: &[String]) -> Result<HashSet<String>, String> {
    let user: HashSet<&String> = user_parcels.iter().collect();
    let mut on_remote: HashSet<String> = HashSet::new();
    let mut frontier: Vec<String> = remote_heads.to_vec();

    // Walked only through user history, which is all that can be unlifted: a meta head, or a
    // remote head held here only outside the user pallets, is skipped, which can only count
    // more parcels as unlifted.
    while let Some(hash) = frontier.pop() {
        if on_remote.contains(&hash) || !user.contains(&hash) || !file_utils::is_present_for_sweep(&hash)? {
            continue;
        }

        frontier.extend(object_utils::load_parcel(&hash)?.parents);
        on_remote.insert(hash);
    }

    Ok(user_parcels.iter().filter(|parcel| !on_remote.contains(*parcel)).cloned().collect())
}

/// Everything under the pruned path(s) that lifting history the remote is not known to hold
/// reads, so that a prune never changes what such a lift can do. This mirrors the lift's
/// changed-closure walk (`remote_utils::collect_changed_closure`) below each pruned prefix: for
/// each such parcel, its trees there that no parent's tree at the same path explains, the files
/// (and a chunked file's recipe and chunks) in them that no parent explains, and each parent's
/// tree at every such path, which the walk loads as a base. A subtree identical to a parent's is
/// on the remote with that parent, or kept for it when the parent is unlifted too, so it is
/// skipped, exactly as the lift skips it.
///
/// Presence-tolerant like [`retain_local_closure`]: an absent object, parcel or base has nothing
/// left to keep. An absent base explains nothing, so more is kept, never less. A path that runs
/// through a tree held by hash alone ([`resolve_unsealed_subtree`]) keeps nothing either: the
/// remote holds that tree and everything beneath it.
///
/// # Arguments
/// * `unlifted`        - The parcels from [`unlifted_parcels`].
/// * `pruned_prefixes` - The warehouse path keys being pruned.
/// * `sealed`          - The sealed-by-hash record.
///
/// # Returns
/// * `Ok((kept, touching))` - The objects to keep, and the unlifted parcels that changed some
///   pruned path, which are the ones whose lift needs any of them.
/// * `Err(String)`          - If a present object cannot be read.
fn retain_unlifted(unlifted: &HashSet<String>, pruned_prefixes: &[String], sealed: &SealedSet)
    -> Result<(HashSet<String>, HashSet<String>), String> {
    let mut kept: HashSet<String> = HashSet::new();
    let mut walked: HashSet<(String, Vec<String>)> = HashSet::new();
    let mut touching: HashSet<String> = HashSet::new();

    for parcel in unlifted {
        if !file_utils::is_present_for_sweep(parcel)? {
            continue;
        }

        let loaded = object_utils::load_parcel(parcel)?;
        let mut parent_roots: Vec<String> = Vec::new();

        for parent in &loaded.parents {
            if file_utils::is_present_for_sweep(parent)? {
                parent_roots.push(object_utils::load_parcel(parent)?.tree_hash);
            }
        }

        for prefix in pruned_prefixes {
            let Some(subtree) = resolve_unsealed_subtree(&loaded.tree_hash, prefix, sealed)? else {
                continue;
            };

            let mut bases: Vec<String> = Vec::new();
            for root in &parent_roots {
                if let Some(base) = resolve_unsealed_subtree(root, prefix, sealed)? {
                    bases.push(base);
                }
            }

            if !bases.contains(&subtree) {
                touching.insert(parcel.clone());
                retain_changed(&subtree, bases, &mut walked, &mut kept)?;
            }
        }
    }

    Ok((kept, touching))
}

/// One step of [`retain_unlifted`]'s walk: keep `tree_hash` unless a base explains it, keep each
/// base tree, keep each file no base explains, and descend each subtree against the bases'
/// subtrees of the same name. A (tree, bases) pair is walked once; the same tree under other
/// bases is walked again, since what those bases explain differs.
///
/// # Arguments
/// * `tree_hash`   - A tree of an unlifted parcel at some path.
/// * `base_hashes` - The parents' trees at the same path.
/// * `walked`      - The (tree, bases) pairs already walked.
/// * `kept`        - The objects kept so far.
fn retain_changed(tree_hash: &str,
                  mut base_hashes: Vec<String>,
                  walked: &mut HashSet<(String, Vec<String>)>,
                  kept: &mut HashSet<String>) -> Result<(), String> {
    if base_hashes.iter().any(|hash| hash == tree_hash) || !file_utils::is_present_for_sweep(tree_hash)? {
        return Ok(());
    }

    base_hashes.sort();
    base_hashes.dedup();

    if !walked.insert((tree_hash.to_string(), base_hashes.clone())) {
        return Ok(());
    }

    kept.insert(tree_hash.to_string());
    let tree = object_utils::load_tree(tree_hash)?;

    let mut base_files: HashMap<String, Vec<String>> = HashMap::new();
    let mut base_subtrees: HashMap<String, Vec<String>> = HashMap::new();

    for base in &base_hashes {
        if !file_utils::is_present_for_sweep(base)? {
            continue;
        }

        kept.insert(base.clone());
        let base_tree = object_utils::load_tree(base)?;

        for (name, file) in base_tree.get_files() {
            base_files.entry(name.clone()).or_default().push(file.hash.clone());
        }
        for (name, subtree) in base_tree.get_subtrees() {
            base_subtrees.entry(name.clone()).or_default().push(subtree.hash.clone());
        }
    }

    for (name, file) in tree.get_files() {
        let explained = base_files.get(name).is_some_and(|hashes| hashes.contains(&file.hash));

        if !explained && kept.insert(file.hash.clone()) && file.item_type.is_chunked() {
            retain_present_recipe_chunks(&file.hash, kept)?;
        }
    }

    for (name, subtree) in tree.get_subtrees() {
        let bases = base_subtrees.get(name).cloned().unwrap_or_default();
        retain_changed(&subtree.hash, bases, walked, kept)?;
    }

    Ok(())
}

/// The unlifted parcels whose lift is part of what lets a later prune free what this one kept:
/// each parcel in `touching` and every unlifted descendant of one, found in one pass over the
/// unlifted parcels' parent edges.
///
/// # Arguments
/// * `unlifted` - The parcels from [`unlifted_parcels`].
/// * `touching` - The unlifted parcels that changed a pruned path.
fn leading_to_touching(unlifted: &HashSet<String>, touching: &HashSet<String>) -> Result<HashSet<String>, String> {
    let mut children: HashMap<String, Vec<String>> = HashMap::new();

    for parcel in unlifted {
        if !file_utils::is_present_for_sweep(parcel)? {
            continue;
        }

        for parent in object_utils::load_parcel(parcel)?.parents {
            if unlifted.contains(&parent) {
                children.entry(parent).or_default().push(parcel.clone());
            }
        }
    }

    let mut leading: HashSet<String> = HashSet::new();
    let mut frontier: Vec<String> = touching.iter().cloned().collect();

    while let Some(hash) = frontier.pop() {
        if leading.insert(hash.clone()) {
            frontier.extend(children.remove(&hash).unwrap_or_default());
        }
    }

    Ok(leading)
}

/// Delete the planned loose objects, reclaiming their disk. Content objects (trees, blobs)
/// carry no signature sidecar — only parcels do, and a plan never contains a parcel — so
/// nothing rides along. Idempotent: an object already gone (a resumed prune) is the desired
/// end state, not an error.
///
/// # Arguments
/// * `to_free`       - The loose object hashes from a [`PrunePlan`].
/// * `_store_access` - The exclusive store access the plan was computed under, still held.
pub fn free_objects(to_free: &[String], _store_access: &StoreAccessExclusive) -> Result<PruneStats, String> {
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
/// Only the persistent pallet heads are roots here: they alone decide what a prune *targets*.
/// What it must *retain* also counts every other gc root, see [`local_roots`].
fn reachable_parcels() -> Result<(Vec<String>, Vec<String>), String> {
    let mut user_roots: Vec<String> = Vec::new();
    let mut meta_roots: Vec<String> = Vec::new();

    for (pallet_ref, head) in pallet_utils::all_pallet_refs()? {
        match pallet_ref.namespace {
            PalletNamespace::Meta => meta_roots.push(head),
            PalletNamespace::User => user_roots.push(head),
        }
    }

    let user: Vec<String> = gc_utils::collect_reachable_for_sweep(&user_roots)?.into_iter().collect();
    let meta: Vec<String> = gc_utils::collect_reachable_for_sweep(&meta_roots)?.into_iter().collect();

    Ok((user, meta))
}

/// The gc roots that are not pallet heads, which a prune must retain from even though it never
/// targets through them.
struct LocalRoots {
    /// Every parcel reachable from a bay's base or parked parcel, an in-progress consolidation or
    /// cherry-pick, or a trust pin, in any bay, that the pallet heads do not already reach. Walked scoped, like history,
    /// except a meta-shaped tree, which is walked in full (see [`retain_local_closure`]).
    parcels: Vec<String>,
    /// Every staged inventory shard leaf, in any bay. Retained whatever its path.
    staged: Vec<(String, DirEntryType)>,
}

/// The gc root sources a prune reads, fail-closed: a haul, bay, shard or office record that
/// cannot be read aborts the prune before anything is narrowed or freed.
fn root_sources() -> Result<recovery_utils::GcRootSources, String> {
    let bay_dirs = bay_utils::all_bay_state_dirs()?;
    recovery_utils::gc_root_sources(
        &bay_dirs, bay_utils::BayReadPolicy::FailClosed, office_utils::TrustPinReadPolicy::FailClosed,
    )
}

/// Collect [`LocalRoots`] from the [`root_sources`] the plan read.
///
/// These used to be omitted on the argument that whatever a transient state references under
/// the pruned path is out of scope after the prune. That holds per path but not per object:
/// content addressing lets a staged `b/y`, or a parked parcel's `b/y`, hold the very blob the
/// pruned `a/x` holds, and freeing it would lose the only copy (FORK-156). See
/// [`retain_local_closure`] for what each kind of root keeps.
///
/// # Arguments
/// * `user_parcels` - The parcels the user pallet heads reach, already retained.
/// * `meta_parcels` - The parcels the meta pallet heads reach, already retained.
/// * `sources`      - The root sources; their haul heads are already in `user_parcels`.
fn local_roots(user_parcels: &[String], meta_parcels: &[String], sources: recovery_utils::GcRootSources) -> Result<LocalRoots, String> {
    let known: HashSet<&String> = user_parcels.iter().chain(meta_parcels).collect();
    let mut roots: Vec<String> = sources.parcels.into_iter().filter(|p| !known.contains(p)).collect();
    roots.extend(sources.pin_parcels.into_iter().map(|(p, _)| p).filter(|p| !known.contains(p)));

    let parcels: Vec<String> = gc_utils::collect_reachable_for_sweep(&roots)?
        .into_iter()
        .filter(|p| !known.contains(p))
        .collect();

    Ok(LocalRoots { parcels, staged: sources.shard_leaves })
}

/// Every object pallet history needs to survive the prune if the remote holds all of it: exactly
/// what a fresh franchise scoped to the post-prune fetch scope would hold. Local state is added by
/// [`retain_local_closure`], and history the remote does not hold by [`retain_unlifted`].
///
/// User-pallet parcels are walked scoped — the spine and every in-scope subtree in full; at an
/// out-of-scope subtree the walk keeps nothing and stops, so the boundary object itself becomes
/// freeable while its hash stays sealed inside the retained parent spine tree. Meta-pallet
/// parcels are walked in full: the carve-out never scopes them. So is a user parcel whose tree is
/// meta-shaped ([`recovery_utils::walks_in_full`]), since a removed or proposed head is not
/// checked for its namespace (FORK-281).
///
/// A tree or recipe recorded in `sealed` and absent is held by hash alone: it is retained and not
/// descended, as a fresh franchise would hold it. A recorded one that is present is walked like
/// any other. Any other absent tree or recipe fails the plan.
fn collect_retained(
    user_parcels: &[String],
    meta_parcels: &[String],
    post_prune: &MaterializationScope,
    sealed: &SealedSet,
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
        if held_by_hash(sealed, &tree_hash)? {
            continue;
        }
        let tree = object_utils::load_tree(&tree_hash)?;

        // A meta-shaped root is walked in full, however it joined the user parcels: a removed
        // head or a haul's proposed head is not checked for its namespace (FORK-281).
        if recovery_utils::walks_in_full(&tree, &path) {
            full_frontier.push_back(tree_hash);
            continue;
        }

        for (name, file) in tree.get_files() {
            // A file is never spine; retain it unless it is out of scope (files carry no
            // children, so an out-of-scope file's blob is simply freeable).
            if post_prune.classify(&join_key(&path, name)) != ScopeClass::OutOfScope {
                retained.insert(file.hash.clone());

                // A retained chunked file names a recipe (its `file.hash`) whose chunks ride the
                // store as ordinary, content-addressed, *shareable* objects. Descend the recipe to
                // retain every chunk too: a chunk this in-scope file shares with an out-of-scope
                // pruned file must never be freed (the multi-bay/shared-content guard, one level
                // deeper than the shared-blob guard the subtraction already provides).
                if file.item_type.is_chunked() && !held_by_hash(sealed, &file.hash)? {
                    retain_recipe_chunks(&file.hash, &mut retained)?;
                }
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
        if held_by_hash(sealed, &tree_hash)? {
            continue;
        }
        let tree = object_utils::load_tree(&tree_hash)?;

        for (_, file) in tree.get_files() {
            retained.insert(file.hash.clone());

            // Everything under an in-scope (or meta) subtree is retained in full, chunks included
            // — the same recipe descent the spine walk above does, for the same reason.
            if file.item_type.is_chunked() && !held_by_hash(sealed, &file.hash)? {
                retain_recipe_chunks(&file.hash, &mut retained)?;
            }
        }

        for (_, subtree) in tree.get_subtrees() {
            full_frontier.push_back(subtree.hash.clone());
        }
    }

    Ok(retained)
}

/// Retain what [`LocalRoots`] reach: every staged leaf whatever its path (descending a chunked
/// entry's recipe), and every local parcel: user history scoped, meta history in full. Which is
/// which is read off the root tree's shape ([`recovery_utils::walks_in_full`], the rule
/// [`collect_retained`] applies to its user parcels), never off the root that reached it: a trust
/// pin can name either, and the `adopts` pin names office history that no meta head reaches after
/// a re-genesis (FORK-281).
///
/// Meta history is walked in full because the fetch scope never selects from it: its own content
/// is the history, and its metadata folder reads out of scope at the root. For user history,
/// scoped is enough, because a prune only ever frees its targets, and every
/// target is pallet history under the pruned path. A local parcel's own work (an edit parked
/// under that path) is a new object, never a target, so it survives whatever this walk does.
/// What a local parcel shares with a target is history: re-fetchable when the remote holds it,
/// and kept by [`retain_unlifted`] when it does not. The one thing at risk is a target a local parcel holds *in scope*, where
/// absence would read as missing, and the scoped walk keeps exactly that. Walking the parcels in
/// full instead would keep the whole pruned path whenever any bay has parked work, since a park
/// snapshots the whole tree.
///
/// Presence-tolerant, unlike [`collect_retained`]: a local parcel made in a narrower checkout,
/// or a pinned historical parcel, may seal subtrees this warehouse never fetched, and a staged
/// recipe may already be gone (FORK-157). An absent object has nothing left to keep and is not
/// a prune target either, so skipping it is the same rule gc's live set applies, including its
/// exception: an object only an unreadable pack index might hold fails the plan instead
/// ([`file_utils::is_present_for_sweep`]), since its children may be targets (FORK-193).
///
/// # Arguments
/// * `local`      - The non-head roots from [`local_roots`].
/// * `post_prune` - The warehouse fetch scope that will remain after the prune.
/// * `retained`   - The retained set to add to.
fn retain_local_closure(
    local: &LocalRoots,
    post_prune: &MaterializationScope,
    retained: &mut HashSet<String>,
) -> Result<(), String> {
    for (hash, item_type) in &local.staged {
        if retained.insert(hash.clone()) && item_type.is_chunked() {
            retain_present_recipe_chunks(hash, retained)?;
        }
    }

    // `Some(path)` walks a spine tree at that path, classifying its children; `None` walks an
    // in-scope tree in full. Deduped on both, as in `collect_retained`: one tree can be spine at
    // one path and in scope at another.
    let mut frontier: VecDeque<(String, Option<String>)> = VecDeque::new();
    let mut seen: HashSet<(String, Option<String>)> = HashSet::new();

    for parcel in &local.parcels {
        retained.insert(parcel.clone());
        frontier.push_back((object_utils::load_parcel(parcel)?.tree_hash, Some(String::new())));
    }

    while let Some((tree_hash, path)) = frontier.pop_front() {
        if !seen.insert((tree_hash.clone(), path.clone())) || !file_utils::is_present_for_sweep(&tree_hash)? {
            continue;
        }

        retained.insert(tree_hash.clone());
        let tree = object_utils::load_tree(&tree_hash)?;

        // A meta tree is retained in full whatever reached it, as in `collect_retained`: the
        // `adopts` pin names office history no meta head reaches after a re-genesis, and the scope
        // would read its metadata folder as out of scope (FORK-281).
        let path = path.filter(|path| !recovery_utils::walks_in_full(&tree, path));

        for (name, file) in tree.get_files() {
            let kept = match &path {
                None => true,
                Some(path) => post_prune.classify(&join_key(path, name)) != ScopeClass::OutOfScope,
            };

            if kept && retained.insert(file.hash.clone()) && file.item_type.is_chunked() {
                retain_present_recipe_chunks(&file.hash, retained)?;
            }
        }

        for (name, subtree) in tree.get_subtrees() {
            let Some(path) = &path else {
                frontier.push_back((subtree.hash.clone(), None));
                continue;
            };

            let child = join_key(path, name);
            match post_prune.classify(&child) {
                ScopeClass::InScope => frontier.push_back((subtree.hash.clone(), None)),
                ScopeClass::Spine => frontier.push_back((subtree.hash.clone(), Some(child))),
                ScopeClass::OutOfScope => {}
            }
        }
    }

    Ok(())
}

/// [`retain_recipe_chunks`] for a recipe that may be absent, which retains nothing. A recipe only
/// an unreadable pack index might hold is an error, as in [`retain_local_closure`].
fn retain_present_recipe_chunks(recipe_hash: &str, retained: &mut HashSet<String>) -> Result<(), String> {
    if !file_utils::is_present_for_sweep(recipe_hash)? {
        return Ok(());
    }

    retain_recipe_chunks(recipe_hash, retained)
}

/// Retain every chunk a retained chunked file's recipe names, so a chunk shared with a pruned
/// out-of-scope file survives (chunks are content-addressed and shared). The recipe hash itself is
/// already retained by the caller; this descends it to reach the chunk hashes.
///
/// The recipe is present by construction here — the caller only descends a file it has classified
/// in-scope and has not found held by hash alone, and an in-scope object was fetched — so a
/// missing one is genuine corruption and fails loudly (before `plan_prune` narrows or frees
/// anything: durable-before-destructive is preserved),
/// exactly as the surrounding `load_tree` calls do.
///
/// # Arguments
/// * `recipe_hash` - The hash of a retained chunked file's recipe.
/// * `retained`    - The live set to add the recipe's chunk hashes into.
fn retain_recipe_chunks(recipe_hash: &str, retained: &mut HashSet<String>) -> Result<(), String> {
    for chunk in object_utils::recipe_chunk_hashes(recipe_hash)? {
        retained.insert(chunk);
    }

    Ok(())
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
///
/// Presence here is the plain [`file_utils::does_object_exist`], not the fail-closed sweep form
/// the retained walks use. An object only an unreadable pack index might hold reads absent, so
/// what lies beneath it is not targeted: this walk only decides what may be freed, and missing a
/// target frees less, never more. For the same reason a parcel whose path to a pruned prefix runs
/// through a tree held by hash alone contributes no targets ([`resolve_unsealed_subtree`]).
fn collect_prune_targets(
    user_parcels: &[String],
    pruned_prefixes: &[String],
    sealed: &SealedSet,
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
            if let Some(subtree) = resolve_unsealed_subtree(&root, prefix, sealed)? {
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
                    if !seen.insert(file.hash.clone()) {
                        continue;
                    }

                    // Absent already means an earlier run freed it (a recipe, or a plain blob):
                    // the child-before-parent order guarantees everything beneath it is gone too,
                    // so skip it whole. (A blob is a leaf; a recipe's chunks were freed first.)
                    if !file_utils::does_object_exist(&file.hash)? {
                        continue;
                    }

                    // A chunked file's tree-entry hash names a recipe, whose chunks are reachable
                    // *only* through it. Descend the recipe WHILE IT IS STILL PRESENT to enumerate
                    // its chunks, and place them BEFORE the recipe in `ordered` (children before
                    // their parent) — so `free_objects` deletes chunks first, then the recipe, and
                    // a killed run stays resumable: on the next call an absent recipe proves its
                    // chunks were already freed, while a present recipe still names every chunk a
                    // partial free left behind. A chunk shared with a retained scope is filtered out
                    // later by `plan_prune`'s `retained` check, never here.
                    if file.item_type.is_chunked() {
                        for chunk in object_utils::recipe_chunk_hashes(&file.hash)? {
                            if seen.insert(chunk.clone()) && file_utils::does_object_exist(&chunk)? {
                                ordered.push(chunk);
                            }
                        }
                    }

                    ordered.push(file.hash.clone());
                }

                for (_, subtree) in tree.get_subtrees() {
                    stack.push(Step::Expand(subtree.hash.clone()));
                }
            }
        }
    }

    Ok(ordered)
}

/// The hash of the subtree at warehouse path `key` under `root_tree_hash`, as
/// [`tree_utils::resolve_subtree_hash`] finds it, or `None` when the key names nothing there or
/// a tree on the way to it is held by hash alone ([`held_by_hash`]): what lies beneath such a tree
/// is not in this store to walk. Any other absent tree on the way fails, as it does there.
///
/// [`tree_utils::resolve_subtree_hash`]: crate::util::tree_utils::resolve_subtree_hash
fn resolve_unsealed_subtree(root_tree_hash: &str, key: &str, sealed: &SealedSet) -> Result<Option<String>, String> {
    if key.is_empty() {
        return Ok(Some(root_tree_hash.to_string()));
    }

    let mut current = root_tree_hash.to_string();

    for component in key.split('/') {
        if held_by_hash(sealed, &current)? {
            return Ok(None);
        }

        let tree = object_utils::load_tree(&current)?;

        match tree.get_subtrees().find(|(name, _)| name.as_str() == component) {
            Some((_, item)) => current = item.hash.clone(),
            None => return Ok(None),
        }
    }

    Ok(Some(current))
}

/// Whether `hash` is held by hash alone: recorded in `sealed` and absent. A recorded object that
/// is present seals nothing. Presence is the fail-closed sweep form, so an object only an
/// unreadable pack index might hold fails rather than reading as held.
fn held_by_hash(sealed: &SealedSet, hash: &str) -> Result<bool, String> {
    Ok(sealed.contains(hash) && !file_utils::is_present_for_sweep(hash)?)
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
    use crate::util::pack_utils::tests::{for_each_unreadable, pack_as_unreadable, Unreadable};
    use std::path::PathBuf;

    /// Exclusive store access at the active storage root, as the scope-prune handler holds it.
    fn store_access() -> StoreAccessExclusive {
        crate::util::lock_utils::StoreAccessLock::try_exclusive().expect("no other holder in a test root")
    }

    /// Remote heads under which every local user pallet is lifted: the pallet heads themselves.
    fn lifted() -> Vec<String> {
        pallet_utils::all_pallet_refs().unwrap().into_iter()
            .filter(|(pallet_ref, _)| pallet_ref.namespace == PalletNamespace::User)
            .map(|(_, head)| head)
            .collect()
    }

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
            crate::util::pallet_utils::create_position_if_not_exists().unwrap();

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

    fn store_chunk(content: &[u8]) -> String {
        let mut object = LooseObjectBuilder::build_chunk(&crate::model::chunk::Chunk {
            content: content.to_vec(),
        });
        object.store().unwrap();
        object.hash
    }

    /// Store a recipe naming the given chunks (a chunked file's tree-entry object). `content_hash`
    /// is irrelevant to a prune (it enumerates chunk hashes, never re-derives the content), so a
    /// placeholder is fine here.
    fn store_recipe(chunks: &[(String, u64)]) -> String {
        let total_size = chunks.iter().map(|(_, size)| *size).sum();
        let recipe = crate::model::recipe::Recipe {
            content_hash: "f".repeat(64),
            total_size,
            chunks: chunks.iter()
                .map(|(h, s)| crate::model::recipe::RecipeChunk { hash: h.clone(), size: *s })
                .collect(),
        };
        let mut object = LooseObjectBuilder::build_recipe(&recipe);
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
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        Fixture { parcel, root_tree, src_tree, api_tree, api_blob, web_tree, web_blob, readme_blob }
    }

    #[test]
    fn plan_prune_frees_only_the_pruned_subtree_and_never_the_spine() {
        let _scratch = Scratch::new("frees-only-pruned");
        let f = build_fixture();

        // Prune src/web, keeping src/api in scope.
        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access()).unwrap();

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

    /// A parcel over a meta-shaped tree, `.forklift/tracked/cells/cell`, whose one cell is `blob`.
    fn office_parcel_holding(blob: &str) -> String {
        let cells = store_tree(&[("cell", blob, DirEntryType::Normal)]);
        let tracked = store_tree(&[("cells", &cells, DirEntryType::Tree)]);
        let metadata = store_tree(&[("tracked", &tracked, DirEntryType::Tree)]);
        let root = store_tree(&[(crate::globals::FOLDER_NAME_FORKLIFT_ROOT, &metadata, DirEntryType::Tree)]);
        store_parcel(&root, Vec::new())
    }

    /// FORK-281: after a re-genesis the anchor's `adopts` pin names office history no meta head
    /// reaches. Its tree is meta-shaped, so it is retained in full whatever the fetch scope, and
    /// a cell sharing its bytes with the pruned path stays. Mutation: drop the meta-tree rule in
    /// `retain_local_closure` → `.forklift` reads out of scope at the root, the shared blob is
    /// freed → red.
    #[test]
    fn plan_prune_keeps_an_adopted_office_cell_the_pruned_path_shares() {
        let _scratch = Scratch::new("keeps-adopted-office");
        let f = build_fixture();

        let adopted = office_parcel_holding(&f.web_blob);

        let mut sources = root_sources().unwrap();
        sources.pin_parcels.push((adopted.clone(), office_utils::PinAbsencePolicy::Loss));

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_from(&["src/web".to_string()], &post_prune, &lifted(), &[], sources).unwrap();

        assert!(!plan.to_free.contains(&f.web_blob),
            "a blob the adopted office history holds must not be freed: {:?}", plan.to_free);
        assert_eq!(plan.retained_shared, 1, "the shared blob is kept as shared content");
        // The guard engaged on a non-empty plan: the pruned tree itself is still freed.
        assert_eq!(plan.to_free, vec![f.web_tree.clone()]);
    }

    /// FORK-281: a trust pin over a user-shaped tree is still walked within the scope, so a blob it
    /// holds only out of scope is freed with the pruned path. Pins `adopts` apart from the other
    /// roots by the tree's shape, never by the pin. Mutation: walk every pinned parcel in full → the
    /// pinned `other/w.txt` keeps the shared blob → red.
    #[test]
    fn plan_prune_still_frees_a_blob_a_user_shaped_pin_holds_out_of_scope() {
        let _scratch = Scratch::new("frees-user-pin");
        let f = build_fixture();

        let other = store_tree(&[("w.txt", &f.web_blob, DirEntryType::Normal)]);
        let pinned = store_parcel(&store_tree(&[("other", &other, DirEntryType::Tree)]), Vec::new());

        let mut sources = root_sources().unwrap();
        sources.pin_parcels.push((pinned, office_utils::PinAbsencePolicy::Gap));

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_from(&["src/web".to_string()], &post_prune, &lifted(), &[], sources).unwrap();

        assert!(plan.to_free.contains(&f.web_blob),
            "a pinned user tree holds the blob only out of scope, so it is freed: {:?}", plan.to_free);
    }

    /// FORK-281: a meta-shaped tree that joins the user parcels, here as a removed head, is retained
    /// in full like meta history. Mutation: drop the meta-tree rule in `collect_retained` → the
    /// cell's bytes, shared with the pruned path, are freed → red.
    #[test]
    fn plan_prune_keeps_a_cell_a_meta_shaped_removed_head_holds() {
        let _scratch = Scratch::new("keeps-removed-office");
        let f = build_fixture();
        let removed = office_parcel_holding(&f.web_blob);

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[removed], &store_access()).unwrap();

        assert!(!plan.to_free.contains(&f.web_blob),
            "a blob a meta-shaped removed head holds must not be freed: {:?}", plan.to_free);
        assert_eq!(plan.to_free, vec![f.web_tree.clone()]);
    }

    #[test]
    fn plan_prune_keeps_what_history_the_remote_lacks_needs() {
        // Stacked and never lifted, the pruned path's content here is the only copy: the prune
        // keeps it, counts it, and names the pallet whose head the remote lacks (FORK-251).
        let _scratch = Scratch::new("keeps-unlifted");
        let f = build_fixture();

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &[], &[], &store_access()).unwrap();

        assert!(plan.to_free.is_empty(), "unlifted content must not be freed: {:?}", plan.to_free);
        assert_eq!(plan.kept_unlifted, 2, "the web tree and blob are kept for unlifted history");
        assert_eq!(plan.unlifted_pallets, vec!["main".to_string()]);

        // A remote head absent here reaches nothing, so it lifts nothing.
        let absent = "0".repeat(64);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &[absent], &[], &store_access()).unwrap();
        assert!(plan.to_free.is_empty(), "an absent remote head must not count as reaching history");
        assert!(loose_exists(&f.web_blob));
    }

    #[test]
    fn plan_prune_keeps_the_parent_side_of_an_unlifted_change() {
        // The remote holds P1 (web v1); P2 (web v2) is stacked on it and never lifted. Lifting P2
        // later uploads P2's new web tree and blob and loads P1's web tree as the base it compares
        // against, so all three stay. P1's blob is never read by that lift, so it is freed.
        // Once the remote holds P2, everything is freed.
        let _scratch = Scratch::new("keeps-unlifted-parent");
        let f = build_fixture();

        let web_blob_v2 = store_blob("web v2\n");
        let web_tree_v2 = store_tree(&[("w.txt", &web_blob_v2, DirEntryType::Normal)]);
        let src_tree_v2 = store_tree(&[
            ("api", &f.api_tree, DirEntryType::Tree),
            ("web", &web_tree_v2, DirEntryType::Tree),
        ]);
        let root_tree_v2 = store_tree(&[
            ("src", &src_tree_v2, DirEntryType::Tree),
            ("README.md", &f.readme_blob, DirEntryType::Normal),
        ]);
        let child = store_parcel(&root_tree_v2, vec![f.parcel.clone()]);
        pallet_utils::update_pallet_head(
            "main", Some(f.parcel.as_str()), &child, &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &[f.parcel.clone()], &[], &store_access())
            .unwrap();

        assert_eq!(plan.to_free, vec![f.web_blob.clone()], "only the parent's unread blob is freed");
        assert_eq!(plan.kept_unlifted, 3);
        assert_eq!(plan.unlifted_pallets, vec!["main".to_string()]);

        // An unlifted parcel on top that leaves src/web alone still leads to the change, so the
        // pallet is still named.
        let grandchild = store_parcel(&root_tree_v2, vec![child.clone()]);
        pallet_utils::update_pallet_head(
            "main", Some(child.as_str()), &grandchild, &pallet_utils::RefPreconditions::NONE,
        ).unwrap();
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &[f.parcel.clone()], &[], &store_access())
            .unwrap();
        assert_eq!(plan.kept_unlifted, 3);
        assert_eq!(plan.unlifted_pallets, vec!["main".to_string()], "named through its descendant head");

        let plan = plan_prune(&["src/web".to_string()], &post_prune, &[child], &[], &store_access()).unwrap();
        for freed in [&f.web_tree, &f.web_blob, &web_tree_v2, &web_blob_v2] {
            assert!(plan.to_free.contains(freed), "lifted history is freed: {}", freed);
        }
        assert_eq!(plan.kept_unlifted, 0);
        assert!(plan.unlifted_pallets.is_empty());
    }

    #[test]
    fn plan_prune_keeps_the_unlifted_history_behind_a_deleted_pallet() {
        // "main" holds P1, which the remote holds; "feature" stacked P2 (web v2) on it, never
        // lifted, and was then deleted. While the delete can be undone its head is passed as a
        // removed head: lifting the restored pallet uploads P2's web tree and blob and reads P1's
        // web tree, so all three stay, counted apart from the live pallets' keep. Without it,
        // P1's web tree is freed. Mutation: pass the removed heads as remote heads → red.
        let _scratch = Scratch::new("keeps-deleted");
        let f = build_fixture();

        let web_blob_v2 = store_blob("web v2\n");
        let web_tree_v2 = store_tree(&[("w.txt", &web_blob_v2, DirEntryType::Normal)]);
        let src_tree_v2 = store_tree(&[
            ("api", &f.api_tree, DirEntryType::Tree),
            ("web", &web_tree_v2, DirEntryType::Tree),
        ]);
        let root_tree_v2 = store_tree(&[
            ("src", &src_tree_v2, DirEntryType::Tree),
            ("README.md", &f.readme_blob, DirEntryType::Normal),
        ]);
        let deleted = store_parcel(&root_tree_v2, vec![f.parcel.clone()]);

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &[f.parcel.clone()], &[deleted.clone()], &store_access())
            .unwrap();

        assert_eq!(plan.to_free, vec![f.web_blob.clone()], "only the parent's unread blob is freed");
        assert_eq!(plan.kept_for_deleted, 3);
        assert_eq!(plan.kept_unlifted, 0, "no live pallet has unlifted work");
        assert!(plan.unlifted_pallets.is_empty(), "a deleted pallet is not named as one to lift");

        let plan = plan_prune(&["src/web".to_string()], &post_prune, &[f.parcel.clone()], &[], &store_access()).unwrap();
        assert!(plan.to_free.contains(&f.web_tree), "with no removed head the parent's web tree is freed");

        // A haul proposing that head keeps the same history, counted apart; held by both, it
        // counts for the haul. Mutation: leave the haul heads out of the held heads → the parent's
        // web tree is freed.
        let proposing = |head: &String| {
            let mut sources = root_sources().unwrap();
            sources.haul_heads = vec![head.clone()];
            sources
        };
        let plan = plan_from(&["src/web".to_string()], &post_prune, &[f.parcel.clone()], &[], proposing(&deleted)).unwrap();
        assert_eq!(plan.to_free, vec![f.web_blob.clone()], "a haul's proposed history is kept like a deleted pallet's");
        assert_eq!((plan.kept_for_hauls, plan.kept_for_deleted, plan.kept_unlifted), (3, 0, 0));
        let plan = plan_from(&["src/web".to_string()], &post_prune, &[f.parcel.clone()], &[deleted.clone()], proposing(&deleted)).unwrap();
        assert_eq!((plan.kept_for_hauls, plan.kept_for_deleted), (3, 0));

        // The git mirror's roots hold it the same way, counted apart; behind a haul in precedence.
        // Mutation: leave the mirror roots out of the held heads → the parent's web tree is freed.
        let mirroring = |head: &String| {
            let mut sources = root_sources().unwrap();
            sources.mirror_roots = vec![head.clone()];
            sources
        };
        let plan = plan_from(&["src/web".to_string()], &post_prune, &[f.parcel.clone()], &[], mirroring(&deleted)).unwrap();
        assert_eq!(plan.to_free, vec![f.web_blob.clone()], "the mirror's history is kept like a deleted pallet's");
        assert_eq!((plan.kept_for_mirror, plan.kept_for_deleted, plan.kept_unlifted), (3, 0, 0));
        let mut both = proposing(&deleted);
        both.mirror_roots = vec![deleted.clone()];
        let plan = plan_from(&["src/web".to_string()], &post_prune, &[f.parcel.clone()], &[], both).unwrap();
        assert_eq!((plan.kept_for_hauls, plan.kept_for_mirror), (3, 0));

        // A tombstone of a served root holds it as this delete's removed heads do. Mutation: leave
        // the tombstone heads out of the removed heads → the parent's web tree is freed.
        let mut tombstoned = root_sources().unwrap();
        tombstoned.tombstone_heads = vec![deleted.clone()];
        let plan = plan_from(&["src/web".to_string()], &post_prune, &[f.parcel.clone()], &[], tombstoned).unwrap();
        assert_eq!(plan.to_free, vec![f.web_blob.clone()], "a tombstone's history is kept like a deleted pallet's");
        assert_eq!((plan.kept_for_deleted, plan.kept_unlifted), (3, 0));

        // A live pallet sharing the unlifted history counts it as its own.
        pallet_utils::update_pallet_head("feature", None, &deleted, &pallet_utils::RefPreconditions::NONE).unwrap();
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &[f.parcel.clone()], &[deleted.clone()], &store_access())
            .unwrap();
        assert_eq!((plan.kept_unlifted, plan.kept_for_deleted), (3, 0));
        let plan = plan_from(&["src/web".to_string()], &post_prune, &[f.parcel.clone()], &[], proposing(&deleted)).unwrap();
        assert_eq!((plan.kept_unlifted, plan.kept_for_hauls), (3, 0), "a live pallet's own history counts for it");
    }

    #[test]
    fn plan_prune_keeps_nothing_for_unlifted_work_that_leaves_the_path_alone() {
        // "main" has an unlifted change under src/api only: lifting it never reads src/web, so
        // src/web is freed whole. "other" has an unlifted change under src/web, so only what its
        // lift reads is kept, and only "other" is named as the pallet to lift.
        let _scratch = Scratch::new("unlifted-elsewhere");
        let f = build_fixture();

        let api_blob_v2 = store_blob("api a v2\n");
        let api_tree_v2 = store_tree(&[("a.txt", &api_blob_v2, DirEntryType::Normal)]);
        let src_api_v2 = store_tree(&[
            ("api", &api_tree_v2, DirEntryType::Tree),
            ("web", &f.web_tree, DirEntryType::Tree),
        ]);
        let root_api_v2 = store_tree(&[
            ("src", &src_api_v2, DirEntryType::Tree),
            ("README.md", &f.readme_blob, DirEntryType::Normal),
        ]);
        let api_child = store_parcel(&root_api_v2, vec![f.parcel.clone()]);
        pallet_utils::update_pallet_head(
            "main", Some(f.parcel.as_str()), &api_child, &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &[f.parcel.clone()], &[], &store_access())
            .unwrap();
        assert_eq!(plan.kept_unlifted, 0);
        assert!(plan.unlifted_pallets.is_empty());
        assert!(plan.to_free.contains(&f.web_tree) && plan.to_free.contains(&f.web_blob), "{:?}", plan.to_free);

        let web_blob_v2 = store_blob("web v2\n");
        let web_tree_v2 = store_tree(&[("w.txt", &web_blob_v2, DirEntryType::Normal)]);
        let src_web_v2 = store_tree(&[
            ("api", &f.api_tree, DirEntryType::Tree),
            ("web", &web_tree_v2, DirEntryType::Tree),
        ]);
        let root_web_v2 = store_tree(&[
            ("src", &src_web_v2, DirEntryType::Tree),
            ("README.md", &f.readme_blob, DirEntryType::Normal),
        ]);
        let web_child = store_parcel(&root_web_v2, vec![f.parcel.clone()]);
        pallet_utils::update_pallet_head("other", None, &web_child, &pallet_utils::RefPreconditions::NONE)
            .unwrap();

        let plan = plan_prune(&["src/web".to_string()], &post_prune, &[f.parcel.clone()], &[], &store_access())
            .unwrap();
        assert_eq!(plan.kept_unlifted, 3, "other's new web tree and blob, and its base tree");
        assert_eq!(plan.unlifted_pallets, vec!["other".to_string()], "main's lift frees nothing here");
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
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access()).unwrap();

        // The shared blob stays (retained via src/api); only web's unique tree object is freed.
        assert!(!plan.to_free.contains(&shared_blob), "a blob shared with a retained path must not be freed");
        assert!(plan.to_free.contains(&web_tree), "the pruned subtree's unique object is freed");
        assert_eq!(plan.retained_shared, 1, "the shared blob is counted as retained-shared, not silently dropped");
    }

    #[test]
    fn plan_prune_keeps_a_blob_another_bays_staged_entry_shares() {
        // A staged entry is referenced by its bay's inventory shard and nothing else. When it
        // holds the same bytes as a pruned path, the shared blob is its only copy (FORK-156).
        let _scratch = Scratch::new("keeps-staged");
        let f = build_fixture();

        crate::util::gc_utils::tests::plant_staged_shard("b", &[
            ("src/api/y.txt", &f.web_blob, DirEntryType::Normal),
        ]);

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access()).unwrap();

        assert!(!plan.to_free.contains(&f.web_blob), "a blob a staged entry shares must not be freed");
        assert!(plan.to_free.contains(&f.web_tree), "the pruned subtree's unique object is still freed");
        assert_eq!(plan.retained_shared, 1);
    }

    /// Park `tree_hash` as bay `bay`'s only parked parcel, by path. Returns the parcel's hash.
    fn plant_parked(bay: &str, tree_hash: &str) -> String {
        let parcel = store_parcel(tree_hash, Vec::new());
        let dir = crate::util::bay_utils::bay_state_dir(bay);
        std::fs::create_dir_all(&dir).unwrap();
        crate::util::pallet_utils::write_position_in(&dir, &crate::util::pallet_utils::BayPosition::new("main", None)).unwrap();
        std::fs::write(dir.join("parked"), format!("{}\n", parcel)).unwrap();
        parcel
    }

    /// The root tree `src/<dir>/<file>` holding `blob`.
    fn src_tree_with(dir: &str, file: &str, blob: &str) -> String {
        let leaf = store_tree(&[(file, blob, DirEntryType::Normal)]);
        let src = store_tree(&[(dir, &leaf, DirEntryType::Tree)]);
        store_tree(&[("src", &src, DirEntryType::Tree)])
    }

    #[test]
    fn plan_prune_keeps_a_blob_a_parked_parcel_holds_in_scope() {
        // A parked `src/api/y.txt` holding the pruned blob's bytes: after the prune it is in
        // scope, where absence would read as missing.
        let _scratch = Scratch::new("parked-in-scope");
        let f = build_fixture();
        plant_parked("b", &src_tree_with("api", "y.txt", &f.web_blob));

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access()).unwrap();
        assert!(!plan.to_free.contains(&f.web_blob), "a blob parked in scope must not be freed");
        assert!(plan.to_free.contains(&f.web_tree), "the head's own unique tree is still freed");
    }

    #[test]
    fn plan_prune_never_frees_parked_edits_under_the_pruned_path_yet_still_reclaims_it() {
        // The flow `narrow` suggests: park uncommitted edits under the path, narrow, prune. The
        // park snapshots the whole tree, so it shares the head's own `src/web` content too.
        let _scratch = Scratch::new("parked-under-pruned");
        let f = build_fixture();
        let edit = store_blob("an edit nobody stacked\n");
        let web = store_tree(&[("w.txt", &f.web_blob, DirEntryType::Normal), ("new.txt", &edit, DirEntryType::Normal)]);
        let src = store_tree(&[("api", &f.api_tree, DirEntryType::Tree), ("web", &web, DirEntryType::Tree)]);
        let snapshot = store_tree(&[("src", &src, DirEntryType::Tree), ("README.md", &f.readme_blob, DirEntryType::Normal)]);
        plant_parked("b", &snapshot);

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access()).unwrap();

        // The parked work is no history, so never a target: it survives.
        for hash in [&edit, &web] {
            assert!(!plan.to_free.contains(hash), "parked work must never be freed: {}", hash);
        }
        // What it shares with the pruned history is sealed like the rest of that history, so the
        // snapshot does not stop the prune from reclaiming it.
        assert!(plan.to_free.contains(&f.web_blob), "a parked snapshot must not pin pruned history");
        assert!(plan.to_free.contains(&f.web_tree));
    }

    #[test]
    fn plan_prune_tolerates_a_parked_parcel_sealing_a_subtree_never_fetched() {
        // A parcel parked in a narrower checkout, or a pinned historical one, may seal a subtree
        // this warehouse never fetched, even under a path the prune keeps. The retained walk
        // skips the absent object (nothing to keep) instead of refusing the prune.
        let _scratch = Scratch::new("parked-sealed");
        let f = build_fixture();
        let never_fetched = "e".repeat(64);
        let api = store_tree(&[("old", &never_fetched, DirEntryType::Tree)]);
        let src = store_tree(&[("api", &api, DirEntryType::Tree)]);
        plant_parked("b", &store_tree(&[("src", &src, DirEntryType::Tree)]));

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access()).unwrap();
        assert!(plan.to_free.contains(&f.web_blob));
    }

    #[test]
    fn plan_prune_keeps_what_an_in_progress_consolidation_brings_in_scope() {
        // The other side of an unfinished consolidation is reachable only through the bay's
        // consolidation state until it is stacked. Here it brings the pruned blob's bytes in at
        // `src/api/t.txt`, which stays in scope, while its own `src/web` is sealed history.
        let _scratch = Scratch::new("keeps-consolidation");
        let f = build_fixture();
        let api = store_tree(&[("t.txt", &f.web_blob, DirEntryType::Normal)]);
        let their_old = store_blob("their web\n");
        let web = store_tree(&[("w.txt", &their_old, DirEntryType::Normal)]);
        let src = store_tree(&[("api", &api, DirEntryType::Tree), ("web", &web, DirEntryType::Tree)]);
        let theirs = store_parcel(&store_tree(&[("src", &src, DirEntryType::Tree)]), Vec::new());

        let dir = crate::util::bay_utils::bay_state_dir("b");
        std::fs::create_dir_all(&dir).unwrap();
        crate::util::pallet_utils::write_position_in(&dir, &crate::util::pallet_utils::BayPosition::new("main", None)).unwrap();
        let state = crate::util::merge_utils::ConsolidationState {
            their_head: theirs.clone(), their_pallet: "feature".to_string(), our_head: "e".repeat(64), our_pallet: "main".to_string(),
        };
        std::fs::write(dir.join("consolidation"), crate::util::merge_utils::consolidation_state_bytes(&state)).unwrap();

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access()).unwrap();
        assert!(!plan.to_free.contains(&f.web_blob), "a blob the consolidation holds in scope must not be freed");
        assert!(plan.to_free.contains(&f.web_tree), "the head's own unique tree is still freed");
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
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &p2,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access()).unwrap();

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
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access()).unwrap();

        assert!(loose_exists(&f.web_tree) && loose_exists(&f.web_blob), "the pruned objects start present");

        let stats = free_objects(&plan.to_free, &store_access()).unwrap();
        assert_eq!(stats.freed, 2, "both pruned objects are freed");
        assert!(!loose_exists(&f.web_tree), "the pruned subtree object is gone");
        assert!(!loose_exists(&f.web_blob), "the pruned blob is gone");

        // The spine and in-scope content survive, and the store still walks.
        assert!(loose_exists(&f.root_tree) && loose_exists(&f.src_tree), "the spine survives the prune");
        assert!(loose_exists(&f.api_tree) && loose_exists(&f.api_blob), "in-scope content survives");
        object_utils::load_tree(&f.src_tree).expect("the spine tree still loads: src/web stays sealed by hash");

        // A resumed prune (objects already gone) is not an error.
        let again = free_objects(&plan.to_free, &store_access()).unwrap();
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
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access()).unwrap();

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
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access()).unwrap();
        assert_eq!(plan.to_free.len(), 2, "src/web's tree and blob are the whole plan");

        // Simulate an interruption: free only the first item in plan order (a child, by
        // construction — never a parent, since children always precede parents in the plan).
        let (first, rest) = plan.to_free.split_at(1);
        assert_eq!(first, std::slice::from_ref(&f.web_blob), "the child is freed first, by the plan's own order");
        free_objects(first, &store_access()).unwrap();
        assert!(loose_exists(&f.web_tree), "the parent tree was NOT freed by the simulated interruption");

        // Re-planning over the exact same path must not error on the now-absent blob, and must
        // recompute exactly what the interruption left: just the parent tree.
        let resumed_plan = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access())
            .expect("re-planning after a partial free must tolerate the absent object");
        assert_eq!(resumed_plan.to_free, rest, "the resumed plan is exactly what the interruption left");

        let stats = free_objects(&resumed_plan.to_free, &store_access()).unwrap();
        assert_eq!(stats.freed, rest.len());

        // Everything is now gone, and the store is still coherent: the spine still loads and the
        // in-scope content is untouched.
        assert!(!loose_exists(&f.web_tree) && !loose_exists(&f.web_blob), "the resumed prune finished the job");
        object_utils::load_tree(&f.src_tree).expect("the spine still loads after a resumed prune");
        assert!(loose_exists(&f.api_tree) && loose_exists(&f.api_blob), "in-scope content is untouched");
    }

    #[test]
    fn plan_prune_frees_a_chunked_files_recipe_and_all_its_chunks() {
        // A pruned chunked large file reclaims its recipe AND every chunk (chunks are reachable
        // only through the recipe; a recipe-only free would orphan them), with the chunks ordered
        // before the recipe so a killed run stays resumable.
        let _scratch = Scratch::new("chunked-frees-all");

        let api_blob = store_blob("api a v1\n");
        let api_tree = store_tree(&[("a.txt", &api_blob, DirEntryType::Normal)]);

        let c1 = store_chunk(b"chunk one contents ...");
        let c2 = store_chunk(b"chunk two contents ...");
        let recipe = store_recipe(&[(c1.clone(), 22), (c2.clone(), 22)]);
        // The chunked file lives at web/big.bin — its tree entry names the recipe.
        let web_tree = store_tree(&[("big.bin", &recipe, DirEntryType::NormalChunked)]);

        let src_tree = store_tree(&[
            ("api", &api_tree, DirEntryType::Tree),
            ("web", &web_tree, DirEntryType::Tree),
        ]);
        let root_tree = store_tree(&[("src", &src_tree, DirEntryType::Tree)]);
        let parcel = store_parcel(&root_tree, Vec::new());
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access()).unwrap();

        // The recipe and both chunks and the web tree are freed — nothing else.
        for freed in [&recipe, &c1, &c2, &web_tree] {
            assert!(plan.to_free.contains(freed), "a chunked-file object must be freed: {}", freed);
        }
        assert_eq!(plan.to_free.len(), 4, "recipe + 2 chunks + web tree: {:?}", plan.to_free);

        // Children before parents: each chunk precedes the recipe, and the recipe precedes the
        // web tree that names it — the order that makes a killed free resumable.
        let pos = |h: &str| plan.to_free.iter().position(|x| x == h).unwrap();
        assert!(pos(&c1) < pos(&recipe) && pos(&c2) < pos(&recipe),
            "every chunk must precede its recipe: {:?}", plan.to_free);
        assert!(pos(&recipe) < pos(&web_tree),
            "the recipe must precede the tree that names it: {:?}", plan.to_free);

        // The in-scope side is untouched.
        for kept in [&api_tree, &api_blob, &src_tree, &root_tree, &parcel] {
            assert!(!plan.to_free.contains(kept), "an in-scope/spine object must survive: {}", kept);
        }

        // And a real free deletes exactly them.
        let stats = free_objects(&plan.to_free, &store_access()).unwrap();
        assert_eq!(stats.freed, 4);
        assert!(!loose_exists(&c1) && !loose_exists(&c2) && !loose_exists(&recipe),
            "the recipe and its chunks are gone");
        assert!(loose_exists(&api_blob), "the in-scope blob survives");
    }

    #[test]
    fn plan_prune_keeps_a_chunk_shared_between_a_pruned_and_a_kept_file() {
        // Content-addressing at chunk granularity: an appended-to / near-identical file shares
        // chunks. A chunk the pruned out-of-scope file shares with a still-fetched in-scope file
        // must be retained — freeing it would break the kept file. Only the pruned file's *unique*
        // chunk (and its recipe) is freed.
        let _scratch = Scratch::new("chunked-keeps-shared");

        let shared = store_chunk(b"a chunk both files share ...");
        let api_only = store_chunk(b"the api file's own tail ...");
        let web_only = store_chunk(b"the web file's own tail ...");

        // api (in scope) = [shared, api_only]; web (pruned) = [shared, web_only].
        let api_recipe = store_recipe(&[(shared.clone(), 28), (api_only.clone(), 26)]);
        let web_recipe = store_recipe(&[(shared.clone(), 28), (web_only.clone(), 26)]);

        let api_tree = store_tree(&[("a.bin", &api_recipe, DirEntryType::NormalChunked)]);
        let web_tree = store_tree(&[("w.bin", &web_recipe, DirEntryType::NormalChunked)]);
        let src_tree = store_tree(&[
            ("api", &api_tree, DirEntryType::Tree),
            ("web", &web_tree, DirEntryType::Tree),
        ]);
        let root_tree = store_tree(&[("src", &src_tree, DirEntryType::Tree)]);
        let parcel = store_parcel(&root_tree, Vec::new());
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access()).unwrap();

        // The shared chunk (and the api-only chunk and recipe) survive; web's unique chunk, its
        // recipe and its tree are freed.
        assert!(!plan.to_free.contains(&shared), "a chunk shared with a kept file must not be freed");
        assert!(!plan.to_free.contains(&api_only), "the kept file's own chunk must survive");
        assert!(!plan.to_free.contains(&api_recipe), "the kept file's recipe must survive");
        for freed in [&web_only, &web_recipe, &web_tree] {
            assert!(plan.to_free.contains(freed), "the pruned file's unique object is freed: {}", freed);
        }
        assert!(plan.retained_shared >= 1, "the shared chunk is counted as retained, not silently dropped");

        // Freeing must leave the kept file fully materializable.
        free_objects(&plan.to_free, &store_access()).unwrap();
        assert!(loose_exists(&shared) && loose_exists(&api_only) && loose_exists(&api_recipe),
            "the kept chunked file and its shared chunk survive the prune");
    }

    #[test]
    fn plan_prune_keeps_a_whole_recipe_shared_between_a_pruned_and_a_kept_file() {
        // File-level dedup (§9.4b): identical content at two paths chunks identically, so both
        // tree entries name the exact SAME recipe object (content-addressing, one level above the
        // shared-chunk case). Pruning one of the two paths must not free the recipe or any of its
        // chunks — the other, still-in-scope path names the identical hash and needs them all.
        let _scratch = Scratch::new("chunked-keeps-shared-recipe");

        let c1 = store_chunk(b"giant content chunk one ...");
        let c2 = store_chunk(b"giant content chunk two ...");
        let giant_recipe = store_recipe(&[(c1.clone(), 27), (c2.clone(), 27)]);

        // A distinguishing small file alongside the giant one in each directory, so api's and
        // web's own TREE objects differ (a tree with only the identical giant.bin entry would be
        // byte-identical too, which would trivially retain everything and prove nothing about the
        // recipe/chunk descent specifically).
        let api_note = store_blob("api's own note\n");
        let web_note = store_blob("web's own note\n");

        // api (kept) and web (pruned) both track the identical giant file — same recipe hash —
        // alongside their own distinct small file.
        let api_tree = store_tree(&[
            ("giant.bin", &giant_recipe, DirEntryType::NormalChunked),
            ("note.txt", &api_note, DirEntryType::Normal),
        ]);
        let web_tree = store_tree(&[
            ("giant.bin", &giant_recipe, DirEntryType::NormalChunked),
            ("note.txt", &web_note, DirEntryType::Normal),
        ]);
        assert_ne!(api_tree, web_tree, "the two directories must differ so this isn't a no-op prune");

        let src_tree = store_tree(&[
            ("api", &api_tree, DirEntryType::Tree),
            ("web", &web_tree, DirEntryType::Tree),
        ]);
        let root_tree = store_tree(&[("src", &src_tree, DirEntryType::Tree)]);
        let parcel = store_parcel(&root_tree, Vec::new());
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access()).unwrap();

        // Nothing of the giant file is freed: the recipe and both its chunks are still needed by
        // src/api/giant.bin. Only web's own (unshared) tree and note blob are freed.
        for kept in [&giant_recipe, &c1, &c2, &api_note] {
            assert!(!plan.to_free.contains(kept),
                "content shared with (or belonging to) a kept path must not be freed: {}", kept);
        }
        for freed in [&web_tree, &web_note] {
            assert!(plan.to_free.contains(freed), "web's own unshared object is freed: {}", freed);
        }
        assert_eq!(plan.to_free.len(), 2, "only web's tree and note are unique to the pruned path: {:?}", plan.to_free);
        assert!(plan.retained_shared >= 3,
            "the shared recipe and both its chunks are counted as retained, not silently dropped");

        // Freeing must leave the kept (identical) file fully materializable.
        free_objects(&plan.to_free, &store_access()).unwrap();
        assert!(loose_exists(&giant_recipe) && loose_exists(&c1) && loose_exists(&c2),
            "the recipe shared with the kept path, and its chunks, survive the prune");
        object_utils::load_recipe(&giant_recipe).expect("the shared recipe still loads");
    }

    #[test]
    fn plan_prune_of_a_chunked_file_is_resumable_after_a_partial_free() {
        // A killed free deletes a PREFIX of the ordered plan (chunks first, then the recipe). A
        // second plan_prune must tolerate the already-freed chunks and finish the job — the recipe
        // is still present (freed last), so its remaining chunks are still discoverable.
        let _scratch = Scratch::new("chunked-resumable");

        let c1 = store_chunk(b"chunk one ...");
        let c2 = store_chunk(b"chunk two ...");
        let recipe = store_recipe(&[(c1.clone(), 13), (c2.clone(), 13)]);
        let web_tree = store_tree(&[("big.bin", &recipe, DirEntryType::NormalChunked)]);
        let api_blob = store_blob("api\n");
        let api_tree = store_tree(&[("a.txt", &api_blob, DirEntryType::Normal)]);
        let src_tree = store_tree(&[
            ("api", &api_tree, DirEntryType::Tree),
            ("web", &web_tree, DirEntryType::Tree),
        ]);
        let root_tree = store_tree(&[("src", &src_tree, DirEntryType::Tree)]);
        let parcel = store_parcel(&root_tree, Vec::new());
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access()).unwrap();
        assert_eq!(plan.to_free.len(), 4, "2 chunks + recipe + web tree");

        // Simulate an interruption after freeing just the first chunk (the plan's own leading
        // element, a child by construction — the recipe and web tree are NOT yet freed).
        free_objects(&plan.to_free[..1], &store_access()).unwrap();
        assert!(!loose_exists(&plan.to_free[0]), "the first chunk was freed");
        assert!(loose_exists(&recipe), "the recipe was NOT freed by the interruption");

        // Re-planning must not error on the absent chunk, and must recompute exactly what is left:
        // the surviving chunk, the recipe (still present, still names its chunks) and the tree.
        let resumed = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access())
            .expect("re-planning after a partial chunk free must tolerate the absent chunk");
        assert_eq!(resumed.to_free, plan.to_free[1..].to_vec(),
            "the resumed plan is exactly the interruption's remainder");

        free_objects(&resumed.to_free, &store_access()).unwrap();
        assert!(!loose_exists(&c1) && !loose_exists(&c2) && !loose_exists(&recipe) && !loose_exists(&web_tree),
            "the resumed prune finished freeing the chunked file");
        object_utils::load_tree(&src_tree).expect("the spine still loads: src/web is sealed by hash");
        assert!(loose_exists(&api_blob), "in-scope content is untouched");
    }

    /// FORK-193: the retained walks are a sweep's liveness walk. An object only an unreadable pack
    /// index holds is not absent, and what it references may be all that keeps a target, so the
    /// plan must fail rather than skip it. Each test runs for both kinds of unreadable index.
    /// Returns what went wrong, if anything; freeing `kept` is checked first, since it is the harm
    /// the refusal exists to prevent.
    fn plan_outcome(how: Unreadable, unreadable: &str, kept: &str) -> Option<String> {
        let post_prune = MaterializationScope::from_prefixes(["src/api"]);

        match plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access()) {
            Ok(plan) if plan.to_free.contains(&kept.to_string()) => {
                Some(format!("{:?}: the plan frees {}, which the unreadable object references", how, kept))
            }
            Ok(_) => Some(format!("{:?}: the prune plan must refuse past an unreadable object", how)),
            Err(error) if !(error.contains(unreadable) && error.contains("cannot be read")) => {
                Some(format!("{:?}: the error must name the unreadable object: {}", how, error))
            }
            Err(_) => None,
        }
    }

    /// A parked `src/api/y.txt` holds the pruned blob's bytes, under a tree only an unreadable
    /// index holds. Mutation: in `retain_local_closure`, ask `does_object_exist` → the plan frees
    /// the blob (red).
    #[test]
    fn plan_prune_refuses_past_a_parked_tree_only_an_unreadable_index_holds() {
        for_each_unreadable(|how| {
            let _scratch = Scratch::new("parked-unreadable-tree");
            let f = build_fixture();
            let leaf = store_tree(&[("y.txt", &f.web_blob, DirEntryType::Normal)]);
            let src = store_tree(&[("api", &leaf, DirEntryType::Tree)]);
            plant_parked("b", &store_tree(&[("src", &src, DirEntryType::Tree)]));
            pack_as_unreadable(&leaf, how);

            plan_outcome(how, &leaf, &f.web_blob)
        });
    }

    /// A staged chunked file's recipe shares a chunk with the pruned file's recipe, and only an
    /// unreadable index holds the staged recipe. Mutation: in `retain_present_recipe_chunks`, ask
    /// `does_object_exist` → the plan frees the shared chunk (red).
    #[test]
    fn plan_prune_refuses_past_a_staged_recipe_only_an_unreadable_index_holds() {
        for_each_unreadable(|how| {
            let _scratch = Scratch::new("staged-unreadable-recipe");
            let shared = store_chunk(b"a chunk both recipes name");
            let other = store_chunk(b"a chunk only the pruned recipe names");
            let pruned_recipe = store_recipe(&[(shared.clone(), 25), (other.clone(), 36)]);
            let staged_recipe = store_recipe(&[(shared.clone(), 25)]);

            let web = store_tree(&[("big.bin", &pruned_recipe, DirEntryType::NormalChunked)]);
            let src = store_tree(&[("web", &web, DirEntryType::Tree)]);
            let parcel = store_parcel(&store_tree(&[("src", &src, DirEntryType::Tree)]), Vec::new());
            pallet_utils::update_pallet_head("main", None, &parcel, &pallet_utils::RefPreconditions::NONE).unwrap();

            crate::util::gc_utils::tests::plant_staged_shard("b", &[("big.bin", &staged_recipe, DirEntryType::NormalChunked)]);
            pack_as_unreadable(&staged_recipe, how);

            plan_outcome(how, &staged_recipe, &shared)
        });
    }

    /// A parked parcel whose tree holds the pruned blob in scope is itself only in an unreadable
    /// index. Mutation: in `collect_reachable_for_sweep`, ask `does_object_exist` → the parked
    /// parcel reads absent and the plan frees the blob (red).
    #[test]
    fn plan_prune_refuses_past_a_parked_parcel_only_an_unreadable_index_holds() {
        for_each_unreadable(|how| {
            let _scratch = Scratch::new("parked-unreadable-parcel");
            let f = build_fixture();
            let parked = plant_parked("b", &src_tree_with("api", "y.txt", &f.web_blob));
            pack_as_unreadable(&parked, how);

            plan_outcome(how, &parked, &f.web_blob)
        });
    }

    /// Write the sealed-by-hash record holding exactly `hashes`.
    fn seal(hashes: &[&str]) {
        let mut bytes = sealed_utils::header_bytes();
        for hash in hashes {
            bytes.extend(sealed_utils::encode_frame(sealed_utils::SealOp::Record, hash).unwrap());
        }
        std::fs::write(sealed_utils::sealed_path(), bytes).unwrap();
    }

    /// Point a second pallet, `other`, at a parcel over `tree_hash` with the given parents.
    fn plant_other(tree_hash: &str, parents: Vec<String>) -> String {
        let parcel = store_parcel(tree_hash, parents);
        let current = pallet_utils::get_pallet_head("other").unwrap();
        pallet_utils::tests::update_pallet_head_unverified(
            "other", current.as_deref(), &parcel, &pallet_utils::RefPreconditions::NONE,
        ).unwrap();
        parcel
    }

    /// FORK-133: an unlifted parcel whose `src` is absent fails the plan until the record holds
    /// that tree; then the scoped walk retains it without loading it, and neither the prune
    /// targets nor the unlifted keep look beneath it, because the remote holds what it closes
    /// over. The prune still frees the pruned path of the pallet that has it.
    ///
    /// Mutations, each red: drop the spine walk's held check in `collect_retained`; resolve the
    /// prune targets, or the unlifted parcel's own path, through
    /// `tree_utils::resolve_subtree_hash`.
    #[test]
    fn plan_prune_holds_a_recorded_absent_tree_on_the_pruned_path_by_hash_alone() {
        let _scratch = Scratch::new("sealed-path");
        let f = build_fixture();
        let held_src = "1".repeat(64);
        plant_other(&store_tree(&[
            ("src", &held_src, DirEntryType::Tree),
            ("README.md", &f.readme_blob, DirEntryType::Normal),
        ]), Vec::new());

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let remote = [f.parcel.clone()];
        let Err(error) = plan_prune(&["src/web".to_string()], &post_prune, &remote, &[], &store_access()) else {
            panic!("an unrecorded absent tree fails the plan");
        };
        assert!(error.contains(&held_src[2..]), "the plan fails on the absent tree itself: {}", error);

        seal(&[&held_src]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &remote, &[], &store_access()).unwrap();
        let freed: HashSet<String> = plan.to_free.iter().cloned().collect();
        assert_eq!(freed, [f.web_tree, f.web_blob].into_iter().collect());
        assert_eq!(plan.kept_unlifted, 0, "nothing beneath a tree held by hash alone is kept");
    }

    /// FORK-133: an unlifted parcel's base that runs through a tree held by hash alone explains
    /// nothing, so the change is kept in full rather than failing the plan. Mutation: resolve the
    /// bases through `tree_utils::resolve_subtree_hash` → red.
    #[test]
    fn plan_prune_keeps_an_unlifted_change_whose_base_is_held_by_hash_alone() {
        let _scratch = Scratch::new("sealed-base");
        let f = build_fixture();
        let held_src = "2".repeat(64);
        let base = plant_other(&store_tree(&[("src", &held_src, DirEntryType::Tree)]), Vec::new());
        plant_other(&f.root_tree, vec![base]);
        seal(&[&held_src]);

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &[f.parcel.clone()], &[], &store_access())
            .unwrap();
        assert!(plan.to_free.is_empty(), "the unlifted change keeps the whole pruned path: {:?}", plan.to_free);
        assert_eq!(plan.kept_unlifted, 2);
        assert_eq!(plan.unlifted_pallets, vec!["other".to_string()]);
    }

    /// FORK-133: an in-scope tree, an in-scope recipe under a present tree, and a spine-level
    /// in-scope recipe, each recorded and absent, are retained without being loaded. Mutations,
    /// each red: drop the full walk's held check, or the held check on either recipe descent.
    #[test]
    fn plan_prune_retains_a_recorded_absent_tree_or_recipe_in_scope_without_loading_it() {
        let _scratch = Scratch::new("sealed-in-scope");
        let f = build_fixture();
        let (held_lib, held_deep, held_top) = ("3".repeat(64), "4".repeat(64), "5".repeat(64));
        let api = store_tree(&[("h.bin", &held_deep, DirEntryType::NormalChunked)]);
        let src = store_tree(&[("api", &api, DirEntryType::Tree), ("web", &f.web_tree, DirEntryType::Tree)]);
        plant_other(&store_tree(&[
            ("src", &src, DirEntryType::Tree),
            ("lib", &held_lib, DirEntryType::Tree),
            ("big.bin", &held_top, DirEntryType::NormalChunked),
        ]), Vec::new());
        seal(&[&held_lib, &held_deep, &held_top]);

        let post_prune = MaterializationScope::from_prefixes(["src/api", "lib", "big.bin"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access()).unwrap();
        let freed: HashSet<String> = plan.to_free.iter().cloned().collect();
        assert_eq!(freed, [f.web_tree, f.web_blob].into_iter().collect());
    }

    /// FORK-133: a recorded tree that is present seals nothing: it is walked, so a blob it shares
    /// with the pruned path is kept. Mutation: drop the presence half of `held_by_hash` → the
    /// shared blob is freed → red.
    #[test]
    fn plan_prune_walks_a_recorded_tree_that_is_present() {
        let _scratch = Scratch::new("sealed-present");
        let f = build_fixture();
        let api = store_tree(&[("shared.txt", &f.web_blob, DirEntryType::Normal)]);
        let src = store_tree(&[("api", &api, DirEntryType::Tree), ("web", &f.web_tree, DirEntryType::Tree)]);
        plant_other(&store_tree(&[("src", &src, DirEntryType::Tree)]), Vec::new());
        seal(&[&api, &src]);

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let plan = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access()).unwrap();
        assert_eq!(plan.to_free, vec![f.web_tree], "the blob the present recorded tree names is kept");
    }

    /// FORK-133: a sealed-object record that cannot be read fails the plan before anything is
    /// freed: a prune that cannot tell held from lost must not guess. Mutation: read it as empty
    /// on error → the plan succeeds → red.
    #[test]
    fn plan_prune_fails_on_an_unreadable_sealed_record() {
        let _scratch = Scratch::new("sealed-unreadable");
        build_fixture();
        std::fs::write(sealed_utils::sealed_path(), b"not a sealed record").unwrap();

        let post_prune = MaterializationScope::from_prefixes(["src/api"]);
        let Err(error) = plan_prune(&["src/web".to_string()], &post_prune, &lifted(), &[], &store_access()) else {
            panic!("an unreadable record must fail the plan");
        };
        assert!(error.contains("sealed-object record") && error.contains("Restore it from a good copy"), "{}", error);
    }
}
