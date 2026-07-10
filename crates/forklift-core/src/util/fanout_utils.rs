//! The canonical *flat* fan-out idiom.
//!
//! Several hot paths — `audit`'s per-parcel signature verify, `consolidate`'s deferred
//! three-way merges, `compact`'s per-object read+delta-compress, `diff`'s per-file
//! histogram diff — hand-rolled the same shape: a pre-collected, already-independent list
//! of items is split into `min(num_cpus, items.len())` contiguous chunks, one
//! `std::thread::scope` worker per chunk re-enters the caller's storage-root scope and maps
//! each item, and the results come back positionally aligned with the input so the caller
//! can reassemble deterministic (walk-order or path-order) output. [`fanout_map`] is that
//! shape, extracted once.
//!
//! ## When to use it
//! Compute-bound work over a flat list of items that are independent of each other (no
//! item's result depends on another's) and where the per-item cost is large enough that
//! `num_cpus` threads pay for themselves. That is audit, consolidate, compact and diff
//! above; each still keeps its own size threshold and calls this only above it (see each
//! site's `PARALLEL_THRESHOLD`/`PARALLEL_MERGE_THRESHOLD` constant) — the cutoff is a
//! property of *that* workload (how expensive one item is), not of the fan-out mechanism,
//! so it stays at the call site.
//!
//! ## When *not* to use it
//! - **Filesystem-metadata-bound writes** (`materialize`/checkout): the OS's per-directory
//!   and inode-allocation locks serialize concurrent small-file create/write/`chmod`
//!   regardless of how the work is split — a past attempt to parallelize this measured
//!   *slower* (341 → 391 ms, see `docs/PARALLELIZATION_PLAN.md`) and was reverted. This
//!   helper does not fix that; do not reach for it there.
//! - **Sequential-chain operations**, where item *N* depends on item *N-1* (compact's
//!   sliding delta window, the pack append itself, `blame`'s first-parent walk). Those stay
//!   serial by nature; only the independent sub-step (if any) is a `fanout_map` candidate.
//! - **Tree recursion**, where the work is a directory tree and a parent waits on its
//!   children (`stocktake`, `inventory build`, `stack`'s tree build). That shape is
//!   [`crate::model::task::TaskExecutor`]'s job, not this one — `fanout_map` is for a flat,
//!   already-collected `&[T]`, not a tree walk.
//!
//! ## The storage-scope contract
//! A [`crate::globals::StorageRootScope`] is thread-local and **not** inherited by spawned
//! threads. `fanout_map` captures the calling thread's [`crate::globals::current_scope_root`]
//! once, before spawning, and every worker re-enters it (`None` means resolution is by
//! working directory or the process-global bay context, which spawned threads already see)
//! before it calls `f` — so an object read inside `f` resolves under the same warehouse
//! root the caller was using, which matters on the server head (one process serving more
//! than one warehouse).
//!
//! ## Result semantics
//! `fanout_map` never short-circuits and never drops a result: it always returns exactly
//! `items.len()` results, positionally aligned with `items`, whether or not `f` "fails"
//! internally (`R` is often itself a `Result`). A caller that wants first-error,
//! original-order short-circuiting gets it for free —
//! `fanout_map(items, f).into_iter().collect::<Result<Vec<_>, _>>()` stops at the first
//! `Err` in item order, exactly like collecting a serial `.map(f)` would. A caller that
//! needs to inspect every result before deciding whether any is fatal (audit's
//! trust/distrust boundary resolution, which processes verdicts in discovery order and only
//! then decides) keeps the raw `Vec<R>`.

use crate::globals::{self, StorageRootScope};

/// Run `f` over every item in `items`, fanned out across `min(num_cpus, items.len())`
/// scoped threads in contiguous chunks, and return one result per item in input order.
///
/// See the module docs for when this is (and is not) the right tool, the storage-scope
/// re-entry contract, and the result semantics (never short-circuits, never drops a slot).
///
/// Spawns no threads for an empty `items` (returns `Vec::new()` immediately); a caller
/// deciding whether the batch is even worth parallelizing should check its own threshold
/// *before* calling this — `fanout_map` itself always parallelizes when there is more than
/// one worker to give the batch (a single-worker batch — one item, or `num_cpus() == 1` — maps
/// inline on the calling thread instead, since a `thread::scope` with one worker buys nothing).
pub fn fanout_map<T, R>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R>
where
    T: Sync,
    R: Send,
{
    if items.is_empty() {
        return Vec::new();
    }

    let workers = num_cpus::get().max(1).min(items.len());

    if workers == 1 {
        // No thread::scope re-entry needed here: we're still on the caller's own thread, so
        // its storage-root scope (thread-local) is already active for `f`.
        return items.iter().map(f).collect();
    }

    let chunk = items.len().div_ceil(workers);

    // Storage-root scopes are thread-local and not inherited by spawned threads; capture the
    // caller's so each worker resolves storage reads under the same warehouse root (the
    // server head serves more than one). `None` is cwd/bay resolution, which spawned threads
    // already share.
    let scope_root = globals::current_scope_root();

    std::thread::scope(|scope| {
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|slice| {
                let scope_root = scope_root.as_deref();
                let f = &f;

                scope.spawn(move || {
                    let _scope = scope_root.map(StorageRootScope::enter);

                    slice.iter().map(f).collect::<Vec<R>>()
                })
            })
            .collect();

        handles.into_iter()
            .flat_map(|handle| handle.join().expect("a fan-out worker panicked"))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn empty_input_returns_empty_output_and_spawns_nothing() {
        let items: Vec<u32> = Vec::new();
        let results = fanout_map(&items, |n| n * 2);

        assert!(results.is_empty());
    }

    #[test]
    fn single_item_is_mapped_correctly() {
        let items = vec![7u32];
        let results = fanout_map(&items, |n| n * 2);

        assert_eq!(results, vec![14]);
    }

    #[test]
    fn single_worker_path_runs_inline_on_the_calling_thread() {
        // items.len() == 1 forces workers == 1 regardless of num_cpus, which should take the
        // serial fast path — no thread::scope, no spawned worker. Confirm `f` actually ran on
        // the calling thread rather than a spawned one.
        let caller_thread = std::thread::current().id();
        let items = vec![7u32];
        let results = fanout_map(&items, |n| (*n * 2, std::thread::current().id()));

        assert_eq!(results, vec![(14, caller_thread)]);
    }

    #[test]
    fn order_is_preserved_across_many_items_and_workers() {
        // Every item's result encodes its own index, so any reordering (a chunking or
        // reassembly bug) shows up as a mismatch at that position.
        let items: Vec<usize> = (0..2000).collect();
        let results = fanout_map(&items, |n| *n);

        assert_eq!(results, items);
    }

    #[test]
    fn errors_are_kept_in_their_own_slot_not_short_circuited() {
        // Mirrors audit/merge's "keep every verdict" callers: a Vec<Result<_, _>> is
        // returned whole, with failures sitting in their own slots rather than aborting
        // the batch.
        let items: Vec<i32> = (0..500).collect();
        let results: Vec<Result<i32, String>> = fanout_map(&items, |n| {
            if *n % 97 == 0 {
                Err(format!("bad item {}", n))
            } else {
                Ok(*n)
            }
        });

        assert_eq!(results.len(), items.len());

        for (index, result) in results.iter().enumerate() {
            match result {
                Ok(value) => assert_eq!(*value, index as i32),
                Err(message) => assert_eq!(*message, format!("bad item {}", index)),
            }
        }

        // A caller that wants first-error, original-order short-circuiting gets it by
        // collecting into a Result — same as collecting a serial `.map(f)` would.
        let collected: Result<Vec<i32>, String> = results.into_iter().collect();
        assert_eq!(collected, Err("bad item 0".to_string()));
    }

    #[test]
    fn every_worker_re_enters_the_callers_storage_scope() {
        let temp = std::env::temp_dir()
            .join(format!("forklift-fanout-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        let expected = globals::current_scope_root().expect("scope was just entered");
        let items: Vec<u32> = (0..64).collect();

        let mismatches = AtomicUsize::new(0);
        let results = fanout_map(&items, |_| {
            let seen = globals::current_scope_root();
            if seen.as_deref() != Some(expected.as_path()) {
                mismatches.fetch_add(1, Ordering::Relaxed);
            }
        });

        assert_eq!(results.len(), items.len());
        assert_eq!(mismatches.load(Ordering::Relaxed), 0,
                   "every worker must see the caller's entered storage root");

        std::fs::remove_dir_all(&temp).ok();
    }
}
