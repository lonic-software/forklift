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
//!   helper does not fix that; do not reach for it there. The *reads* feeding those writes
//!   are independent and CPU-bound (delta reconstruction), which is [`prefetch_in_order`]'s
//!   job: it loads ahead on worker threads and hands each result to one serial consumer, in
//!   order, so the writes stay on one thread.
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

                worker(scope, move || {
                    let _scope = scope_root.map(StorageRootScope::enter);

                    slice.iter().map(f).collect::<Vec<R>>()
                })
                .expect("failed to spawn a fan-out worker")
            })
            .collect();

        handles.into_iter()
            .flat_map(|handle| handle.join().expect("a fan-out worker panicked"))
            .collect()
    })
}

/// The stack a fan-out worker gets: the size `forklift`'s own worker thread runs with, not the
/// spawned-thread default (2 MiB). Workers load objects, and reconstructing a delta chain
/// recurses once per link up to the pack reader's depth limit; that limit has to be reached
/// as an error, not as a stack overflow, which aborts the process.
const WORKER_STACK_SIZE: usize = 8 * 1024 * 1024;

/// Spawn one scoped fan-out worker with [`WORKER_STACK_SIZE`].
fn worker<'scope, 'env, R: Send + 'scope>(scope: &'scope std::thread::Scope<'scope, 'env>,
                                          f: impl FnOnce() -> R + Send + 'scope)
                                          -> std::io::Result<std::thread::ScopedJoinHandle<'scope, R>> {
    std::thread::Builder::new().stack_size(WORKER_STACK_SIZE).spawn_scoped(scope, f)
}

/// A loaded item waiting for the consumer: its size in budget units, and the load's outcome
/// (a panic is carried here and re-raised on the consumer's thread, so it cannot strand the
/// consumer waiting for a slot that will never fill).
type Loaded<R> = (usize, std::thread::Result<R>);

/// The state [`prefetch_in_order`]'s workers and its consumer share, under one mutex.
struct PrefetchState<R> {
    /// The next index a worker may claim.
    next: usize,
    /// How many items the consumer has taken (the index it waits for next).
    consumed: usize,
    /// The total size of loaded items the consumer has not taken yet.
    held: usize,
    /// One slot per item, filled by the worker that loaded it.
    ready: Vec<Option<Loaded<R>>>,
    /// Set when the consumer is done, early or not: workers stop claiming.
    stop: bool,
    /// How many workers are running. A worker leaves only once it holds no claimed item, so
    /// when none is left, an empty slot will never fill.
    live: usize,
}

struct Prefetch<R> {
    state: std::sync::Mutex<PrefetchState<R>>,
    /// Signalled when a slot fills or a worker exits (the consumer waits on it).
    filled: std::sync::Condvar,
    /// Signalled when the consumer takes a slot or stops (workers wait on it).
    taken: std::sync::Condvar,
}

impl<R> Prefetch<R> {
    fn lock(&self) -> std::sync::MutexGuard<'_, PrefetchState<R>> {
        // Nothing panics while holding the lock (loads and the consumer run outside it), so a
        // poisoned lock still holds consistent state.
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Stops the workers when the consumer leaves, however it leaves (done, an error, or a panic,
/// spawning included), so the scope's join never waits on a worker blocked for room.
struct StopOnDrop<'a, R>(&'a Prefetch<R>);

impl<R> Drop for StopOnDrop<'_, R> {
    fn drop(&mut self) {
        self.0.lock().stop = true;
        self.0.taken.notify_all();
    }
}

/// Counts a worker out when it exits, however it exits, and wakes the consumer so it never
/// waits on a slot no worker is left to fill.
struct ExitOnDrop<'a, R>(&'a Prefetch<R>);

impl<R> Drop for ExitOnDrop<'_, R> {
    fn drop(&mut self) {
        self.0.lock().live -= 1;
        self.0.filled.notify_one();
    }
}

/// Load every item on worker threads, ahead of a single consumer that takes the results one at
/// a time **in input order** on the calling thread. For an ordered, serial side effect (writing
/// files) fed by independent, expensive reads (reconstructing blobs): the reads overlap each
/// other and the consumer, and the side effect never leaves the calling thread.
///
/// Workers stay at most `window` items ahead of the consumer, and stop claiming while the loaded
/// items it has not taken yet total `budget` or more (in `size`'s units). The item the consumer
/// waits for is always claimable, so the bound never stalls it; the most ever held is under
/// `budget` plus one item per worker.
///
/// The first error `consume` returns stops the run: it is returned, nothing after that item is
/// consumed, and the workers stop claiming (items already claimed finish loading and are dropped).
/// A panic in `load` or `size` is re-raised on the calling thread when the consumer reaches that
/// item, so every item before it is consumed first, as a serial loop would.
///
/// Below two workers (one item, or one CPU) it runs inline: load, then consume, item by item, as
/// it does if no worker thread can be spawned. Workers re-enter the caller's storage scope, as
/// [`fanout_map`]'s do.
///
/// # Arguments
/// * `items`   - The items, in the order the consumer must see them.
/// * `window`  - How many items workers may claim past the one the consumer waits for (at least 1).
/// * `budget`  - The size of loaded, untaken items at which workers stop claiming.
/// * `load`    - Loads one item; runs on a worker.
/// * `size`    - The budget units one loaded result holds (e.g. its bytes).
/// * `consume` - Takes one item and its loaded result, in input order, on the calling thread.
///
/// # Returns
/// * `Ok(())` - If `consume` accepted every item.
/// * `Err(E)` - The first error `consume` returned.
pub fn prefetch_in_order<T, R, E>(items: &[T],
                                  window: usize,
                                  budget: usize,
                                  load: impl Fn(&T) -> R + Sync,
                                  size: impl Fn(&R) -> usize + Sync,
                                  mut consume: impl FnMut(&T, R) -> Result<(), E>) -> Result<(), E>
where
    T: Sync,
    R: Send,
{
    let workers = num_cpus::get().max(1).min(items.len());

    if workers < 2 {
        for item in items {
            consume(item, load(item))?;
        }

        return Ok(());
    }

    let window = window.max(1);

    let shared = Prefetch {
        state: std::sync::Mutex::new(PrefetchState {
            next: 0,
            consumed: 0,
            held: 0,
            ready: (0..items.len()).map(|_| None).collect(),
            stop: false,
            live: 0,
        }),
        filled: std::sync::Condvar::new(),
        taken: std::sync::Condvar::new(),
    };

    let scope_root = globals::current_scope_root();

    std::thread::scope(|scope| {
        // Before the first spawn: a worker blocked for room must be released however the
        // consumer leaves, including by a panic out of this function before the loop below.
        let _stop = StopOnDrop(&shared);
        let mut spawned = 0;

        for _ in 0..workers {
            let shared = &shared;
            let load = &load;
            let size = &size;
            let scope_root = scope_root.as_deref();

            shared.lock().live += 1;

            let started = worker(scope, move || {
                let _exit = ExitOnDrop(shared);
                let _scope = scope_root.map(StorageRootScope::enter);

                loop {
                    let index = {
                        let mut state = shared.lock();

                        loop {
                            if state.stop || state.next >= items.len() {
                                return;
                            }

                            // The item the consumer waits for is always claimable. Nothing is held
                            // while it is unclaimed (everything held lies past it), so only a zero
                            // budget needs the second clause.
                            let in_window = state.next < state.consumed.saturating_add(window);
                            let has_room = state.held < budget || state.next == state.consumed;

                            if in_window && has_room {
                                break;
                            }

                            state = shared.taken.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
                        }

                        state.next += 1;
                        state.next - 1
                    };

                    let loaded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let result = load(&items[index]);
                        (size(&result), result)
                    }));

                    let (units, result) = match loaded {
                        Ok((units, result)) => (units, Ok(result)),
                        Err(payload) => (0, Err(payload)),
                    };

                    let mut state = shared.lock();
                    state.held += units;
                    state.ready[index] = Some((units, result));
                    shared.filled.notify_one();
                }
            });

            match started {
                Ok(_) => spawned += 1,
                Err(_) => {
                    // Out of threads: the workers already running carry the run.
                    shared.lock().live -= 1;
                    break;
                }
            }
        }

        if spawned == 0 {
            for item in items {
                consume(item, load(item))?;
            }

            return Ok(());
        }

        for (index, item) in items.iter().enumerate() {
            let result = {
                let mut state = shared.lock();

                let (units, result) = loop {
                    if let Some(loaded) = state.ready[index].take() {
                        break loaded;
                    }

                    // A worker exits holding no claimed item, so with none left this slot can
                    // never fill. Only a worker dying outside its load does that.
                    if state.live == 0 {
                        drop(state);
                        panic!("every prefetch worker exited before item {} was loaded", index);
                    }

                    state = shared.filled.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
                };

                let blocked = state.held >= budget;
                state.held -= units;
                state.consumed = index + 1;

                // The take moved the window one item, so one more claim can start, unless it
                // also took the held size back under the budget, which can release them all.
                if blocked && state.held < budget {
                    shared.taken.notify_all();
                } else {
                    shared.taken.notify_one();
                }

                result
            };

            match result {
                Ok(result) => consume(item, result)?,
                Err(payload) => std::panic::resume_unwind(payload),
            }
        }

        Ok(())
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

    /// A load whose finishing order differs from the input order: later items finish sooner.
    fn uneven_load(n: &usize) -> usize {
        std::thread::sleep(std::time::Duration::from_micros(((7 - n % 8) * 50) as u64));
        *n
    }

    fn has_workers() -> bool {
        num_cpus::get() >= 2
    }

    #[test]
    fn prefetch_consumes_every_item_once_in_input_order() {
        let items: Vec<usize> = (0..500).collect();
        let mut seen = Vec::new();

        let result: Result<(), String> = prefetch_in_order(&items, 16, usize::MAX, uneven_load, |_| 1, |item, loaded| {
            assert_eq!(*item, loaded, "a result was handed to the wrong item");
            seen.push(loaded);
            Ok(())
        });

        assert_eq!(result, Ok(()));
        assert_eq!(seen, items);
    }

    #[test]
    fn prefetch_runs_ahead_but_never_past_the_window() {
        if !has_workers() {
            return;
        }

        // `held` counts loaded items the consumer has not finished taking; a slow consumer lets
        // the workers run as far ahead as the bound allows.
        let held = AtomicUsize::new(0);
        let most = AtomicUsize::new(0);
        let items: Vec<usize> = (0..200).collect();

        let result: Result<(), ()> = prefetch_in_order(&items, 5, usize::MAX, |n| {
            let now = held.fetch_add(1, Ordering::SeqCst) + 1;
            most.fetch_max(now, Ordering::SeqCst);
            *n
        }, |_| 1, |_, _| {
            held.fetch_sub(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_micros(200));
            Ok(())
        });

        assert_eq!(result, Ok(()));
        // `held` still counts the item in the consumer's hands until its closure runs, so the
        // window of 5 ahead of it shows as at most 6.
        let most = most.load(Ordering::SeqCst);
        assert!(most <= 6, "{} items were held, past a window of 5 plus the one being consumed", most);
        assert!(most >= 2, "the workers never ran ahead of the consumer");
    }

    #[test]
    fn prefetch_stops_claiming_once_the_budget_is_held() {
        if !has_workers() {
            return;
        }

        // Every item is 10 units against a budget of 30, under a window too wide to bind: what
        // is held is bounded by the budget plus one item per worker, never by the window.
        let held = AtomicUsize::new(0);
        let most = AtomicUsize::new(0);
        let items: Vec<usize> = (0..200).collect();

        let result: Result<(), ()> = prefetch_in_order(&items, 1000, 30, |n| {
            let now = held.fetch_add(10, Ordering::SeqCst) + 10;
            most.fetch_max(now, Ordering::SeqCst);
            *n
        }, |_| 10, |_, _| {
            held.fetch_sub(10, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_micros(200));
            Ok(())
        });

        assert_eq!(result, Ok(()));
        // Workers claim only while under 30 held (so at 20 or less), each adding one item; the
        // test's count also includes the item in the consumer's hands.
        let ceiling = 20 + 10 * num_cpus::get().min(items.len()) + 10;
        let most = most.load(Ordering::SeqCst);
        assert!(most <= ceiling, "{} units were held, past the budget's ceiling of {}", most, ceiling);
    }

    #[test]
    fn prefetch_stops_at_the_first_consumer_error() {
        let loads = AtomicUsize::new(0);
        let items: Vec<usize> = (0..1000).collect();
        let mut seen = Vec::new();

        let result = prefetch_in_order(&items, 8, usize::MAX, |n| {
            loads.fetch_add(1, Ordering::SeqCst);
            *n
        }, |_| 1, |item, _| {
            seen.push(*item);
            if *item == 100 { Err(format!("failed at {}", item)) } else { Ok(()) }
        });

        assert_eq!(result, Err("failed at 100".to_string()));
        assert_eq!(seen, (0..=100).collect::<Vec<_>>());
        // The workers stopped claiming: at most the window past the failed item was loaded.
        let loads = loads.load(Ordering::SeqCst);
        assert!(loads <= 101 + 8, "{} items were loaded after the consumer stopped at item 100", loads);
    }

    #[test]
    fn a_load_panic_reaches_the_caller_after_every_earlier_item_is_consumed() {
        let items: Vec<usize> = (0..300).collect();
        let mut seen = Vec::new();

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            prefetch_in_order(&items, 8, usize::MAX, |n| {
                if *n == 50 {
                    panic!("load of item 50 failed");
                }
                *n
            }, |_| 1, |item, _| -> Result<(), ()> {
                seen.push(*item);
                Ok(())
            })
        }));

        let payload = outcome.expect_err("the load's panic must reach the caller");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"load of item 50 failed"));
        assert_eq!(seen, (0..50).collect::<Vec<_>>());
    }

    #[test]
    fn a_consumer_panic_reaches_the_caller_and_strands_no_worker() {
        let items: Vec<usize> = (0..300).collect();

        // With a window of 2 the workers are blocked for room when the consumer panics; the
        // scope's join would hang if leaving did not stop them.
        let outcome = std::panic::catch_unwind(|| {
            prefetch_in_order(&items, 2, usize::MAX, |n| *n, |_| 1, |item, _| -> Result<(), ()> {
                if *item == 10 {
                    panic!("consumer failed");
                }
                Ok(())
            })
        });

        let payload = outcome.expect_err("the consumer's panic must reach the caller");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"consumer failed"));
    }

    #[test]
    fn prefetch_workers_re_enter_the_callers_storage_scope() {
        let temp = std::env::temp_dir()
            .join(format!("forklift-prefetch-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        let expected = globals::current_scope_root().expect("scope was just entered");
        let items: Vec<u32> = (0..64).collect();
        let mut mismatches = 0;

        let result: Result<(), ()> = prefetch_in_order(&items, 8, usize::MAX, |_| globals::current_scope_root(), |_| 1, |_, seen| {
            if seen.as_deref() != Some(expected.as_path()) {
                mismatches += 1;
            }
            Ok(())
        });

        assert_eq!(result, Ok(()));
        assert_eq!(mismatches, 0, "every worker must see the caller's entered storage root");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn an_unbounded_window_consumes_every_item() {
        let items: Vec<usize> = (0..500).collect();
        let mut seen = Vec::new();

        // `usize::MAX` reads as "no window": the window's end must not overflow. A budget of two
        // items keeps workers waiting for room, so they test the window again after each take.
        let result: Result<(), ()> = prefetch_in_order(&items, usize::MAX, 2, |n| *n, |_| 1, |item, loaded| {
            assert_eq!(*item, loaded);
            seen.push(loaded);
            Ok(())
        });

        assert_eq!(result, Ok(()));
        assert_eq!(seen, items);
    }

    /// Recurses through 16 KiB of stack per level (3 MiB at 192 levels): more than a spawned
    /// thread's default (2 MiB), as a long delta chain's reconstruction can need. Overflowing
    /// aborts the test process.
    fn stack_hungry(depth: usize) -> u8 {
        let mut frame = [depth as u8; 16 * 1024];
        std::hint::black_box(&mut frame);

        if depth == 0 { frame[0] } else { stack_hungry(depth - 1).wrapping_add(frame[1]) }
    }

    #[test]
    fn fan_out_workers_have_the_stack_of_forklifts_own_thread() {
        if !has_workers() {
            return;
        }

        let items: Vec<usize> = (0..16).collect();

        let mapped = fanout_map(&items, |_| stack_hungry(192));
        assert_eq!(mapped.len(), items.len());

        let mut consumed = 0;
        let result: Result<(), ()> = prefetch_in_order(&items, 4, usize::MAX, |_| stack_hungry(192), |_| 1, |_, _| {
            consumed += 1;
            Ok(())
        });

        assert_eq!(result, Ok(()));
        assert_eq!(consumed, items.len());
    }
}
