pub mod inventory_builder;
pub mod base_task_context;
pub mod change_walk;
pub mod tree_builder;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::task::JoinSet;
use crate::model::task::base_task_context::BaseTaskContext;
use crate::traits::task_context::TaskContext;
use crate::types::task::Task;

/// A task executor. It executes tasks in parallel using multiple workers.
///
/// # Arguments
/// * `OkType`      - The success result type of the tasks.
/// * `ErrType`     - The error result type of the tasks.
/// * `ContextType` - The type of the task context.
pub struct TaskExecutor<OkType: Send + Clone, ErrType: Clone + Send, ContextType>
where
    ContextType: TaskContext<OkType, ErrType>
{
    context: Arc<ContextType>,
    _marker: std::marker::PhantomData<(OkType, ErrType)>,
    /// Test-only worker-count override — see [`Self::with_workers`]. Not present in a
    /// production build at all: `cfg(test)`, not `pub` visibility, is the actual boundary that
    /// keeps this out of production, since visibility alone cannot stop an in-crate caller.
    #[cfg(test)]
    worker_override: Option<usize>,
}

impl<O, E, C> TaskExecutor<O, E, C>
where
    O: Send + Clone + 'static,
    E: Clone + Send + 'static,
    C: TaskContext<O, E>
{
    /// Create a new task executor.
    /// Note that creating a task executor does not start the workers.
    ///
    /// # Arguments
    /// * `context` - The task context.
    ///
    /// # Returns
    /// * `TaskExecutor` - The new task executor.
    pub fn new(context: Arc<C>) -> Self {
        Self {
            context,
            _marker: std::marker::PhantomData,
            #[cfg(test)]
            worker_override: None,
        }
    }

    /// Test-only alternate constructor: pins the exact worker count [`execute`](Self::execute)
    /// spawns, bypassing `num_cpus::get()`.
    ///
    /// Not exposed outside `#[cfg(test)]`, deliberately: it has no production caller — every
    /// real caller wants worker count to track the host's logical CPUs, which is exactly what
    /// the default path (`Self::new`) still does — and this codebase's review history has
    /// repeatedly cut public surface that exists for a single test rather than an actual
    /// production need. Gating on `cfg(test)` (not just leaving it `pub` and unused) keeps that
    /// true structurally: this method, and the field behind it, are not compiled into a
    /// production build at all.
    ///
    /// Lets the `execute_*` tests below pin a small, fixed number of concurrently-resident
    /// workers so their concurrency scenarios are reproducible on any host's tokio runtime,
    /// instead of depending on `num_cpus::get()` — which used to gate whether a scenario could
    /// even run on a given machine, and at low counts made the LIFO-slot race those tests guard
    /// against far more likely to also starve the test itself (see `mod tests`'s doc comment).
    ///
    /// # Arguments
    /// * `context` - The task context (same as [`Self::new`]).
    /// * `workers` - The exact worker count `execute` will spawn; clamped to at least 1.
    #[cfg(test)]
    pub fn with_workers(context: Arc<C>, workers: usize) -> Self {
        Self {
            context,
            _marker: std::marker::PhantomData,
            worker_override: Some(workers.max(1)),
        }
    }

    /// Execute the given task and all tasks that are enqueued by the task in parallel,
    /// using multiple workers.
    ///
    /// The number of workers is equal to the number of logical CPUs.
    /// Since the tasks are executed in parallel, it is advised to store the results in the context.
    ///
    /// Note that calling this method will start the workers, so it is advised not to
    /// run multiple task executors at the same time (it should be safe, but affects performance).
    ///
    /// By the time this method returns — `Ok` or `Err` — every worker task has fully terminated;
    /// no task body is still executing. Aborting a worker only *signals* cancellation, and tokio
    /// can only land that signal at an await point, so a worker running synchronous, non-yielding
    /// work (e.g. compressing a large blob with no await between winning a write reservation and
    /// finishing the write) would otherwise keep running after this method already returned. This
    /// method drains every worker's join result before returning specifically to close that gap.
    /// Callers may rely on this guarantee to finalize resources a task shared with the workers
    /// immediately after `execute` returns — for example a `WriteBatch` that worker tasks staged
    /// into: its `finish` drains the batch's staged set, so it must never run concurrently with a
    /// task still staging.
    ///
    /// This guarantee holds only for a future that is polled to completion. Dropping this
    /// method's future part-way through — e.g. wrapping the call in `tokio::time::timeout` or
    /// racing it in a `select!` against another branch — drops the internal `JoinSet` before the
    /// drain loop runs, which aborts every worker *without* draining their join results. That
    /// reinstates exactly the task-outlives-the-call race this guarantee exists to remove: a task
    /// can still be running after the caller has moved on. Callers must not timeout-wrap or race
    /// `execute`.
    ///
    /// This guarantee has a cost: the return is gated on the *slowest* non-yielding task body
    /// still running when the drain starts. A task stuck indefinitely in synchronous work (e.g. a
    /// write against a stalled mount) blocks `execute` indefinitely along with it — there is
    /// deliberately no timeout on the drain, because returning early is exactly the unsoundness
    /// this guarantee exists to remove.
    ///
    /// A corollary worth stating as a hard rule: once any task fails, every task still sitting in
    /// the queue is guaranteed to never be dequeued — `worker` exits as soon as it observes
    /// `error_occurred`, and `abort_all` cancels every worker still parked at `recv_async` waiting
    /// for one. A task body that synchronously waits (no `.await`) for a sibling task it enqueued
    /// — e.g. spin-waiting on a flag the sibling only sets once it runs — can therefore spin
    /// forever if that sibling never gets scheduled before a failure, and this method's drain
    /// waits for exactly that spin to finish: `execute` hangs. No task shipped in this codebase
    /// does this today (the write-reservation losers in `LooseObject::store_deferred` skip their
    /// own work rather than block on the winner), but nothing here forbids it, so: **task bodies
    /// must never synchronously wait on the progress of another task in the same executor
    /// round.** (The bounded, deadline-escaping spin-waits in this file's own tests are the
    /// permitted shape — they give up and report starvation instead of waiting unconditionally.)
    ///
    /// # Arguments
    /// * `task` - The task to execute.
    ///
    /// # Returns
    /// * `Ok(())`       - Every task ran to completion without error.
    /// * `Err(Some(e))` - At least one task failed with a value; `e` is the first such value
    ///                    recorded. A later valued failure (its worker may still have been running
    ///                    unyielding work during the drain above) sets the same failure state but
    ///                    never overwrites this value — see `worker`'s error branch. A worker may
    ///                    *also* have panicked: a panic carries no `E`, so it does not displace a
    ///                    valued failure here — but the default panic hook still prints it to
    ///                    stderr, so a valued return never hides that a panic happened.
    /// * `Err(None)`    - A failure occurred but no task carried a value: either the initial task
    ///                    could not be enqueued, or every failure was a panic (a panic carries no
    ///                    `E`).
    pub async fn execute(&self, task: Task<O, E>) -> Result<(), Option<E>> {
        let num_workers = self.worker_count();
        let base_context = self.context.get_base_context();
        let mut worker_join_set = JoinSet::new();

        // Send the task to the task queue. This task may enqueue more tasks.
        if self.context.send_task(task).is_err() {
            return Err(None);
        }

        // Start worker task.
        for _ in 0..num_workers {
            worker_join_set.spawn(worker(Arc::clone(&base_context)));
        }

        // Wait for all workers to finish (or an error to occur).
        while let Some(join_result) = worker_join_set.join_next().await {
            match join_result {
                // A worker died without reporting an error (i.e. it panicked). This must be
                // treated as a failure, otherwise the caller would mistake the aborted build
                // for a successful one.
                Err(_) => {
                    base_context.error_occurred.store(true, Ordering::SeqCst);
                    break;
                }
                Ok(Err(_)) => break,
                Ok(Ok(is_finished)) if is_finished => break,
                _ => {}
            }
        }

        // Make sure to abort workers that are still waiting for tasks,
        // as there are no more tasks to be executed.
        worker_join_set.abort_all();

        // `abort_all` only signals cancellation — see this method's doc comment for why that
        // means a worker can still be mid-task here. Drain every worker's join result before
        // returning so this method cannot hand control back to the caller while a task body is
        // still executing.
        while let Some(drain_result) = worker_join_set.join_next().await {
            match drain_result {
                // Expected: `abort_all` cancels every worker still parked at `recv_async`, and a
                // cancelled join reports as an `Err` with `is_cancelled() == true`. This is not a
                // failure — every healthy run leaves idle workers to cancel here, so treating it
                // as one would turn every successful build into a reported failure.
                Err(ref join_error) if join_error.is_cancelled() => {}
                // `JoinError` is exhaustively cancelled-or-panicked, so anything not caught above
                // is a worker that panicked after the main loop already broke out (e.g. two
                // workers panicked back-to-back). On every currently reachable path
                // `error_occurred` is already `true` by the time we get here: a panic that broke
                // the main loop above stores the flag before `break`ing, and an `Ok(Err(_))` that
                // broke it comes from `worker`'s failure branch, which stores the flag before
                // returning (`is_finished` implies no worker is still in flight to panic here at
                // all). This store is not load-bearing on any path today; it is kept defensively
                // so a future restructuring of the main loop cannot silently let a drain-window
                // panic read as success.
                Err(_) => {
                    base_context.error_occurred.store(true, Ordering::SeqCst);
                }
                // The worker returned normally. A drain-window `Ok(Err(()))` comes from either of
                // two places in `worker`: the task-failure branch, which stores the error value
                // (first-error-wins guarded — see its own comment) before setting
                // `error_occurred` and returning; or the top-of-loop `error_occurred` check, a
                // healthy worker simply noticing the flag is already true and stopping — it
                // never touches the value itself, and the flag can be true there with no value
                // set at all yet (a worker panic, above or in the main loop, sets only the flag).
                // Either way there is nothing left to do with this result; `error_value` is read
                // once, after the drain, below.
                Ok(_) => {}
            }
        }

        // Check if an error occurred. If so, return the error.
        if base_context.error_occurred.load(Ordering::SeqCst) {
            // `error_value` is a `std::sync::Mutex` (see `BaseTaskContext`); a poisoned lock here
            // means some earlier holder of this same lock panicked mid-store, which given the
            // critical sections are two-line stores would itself be exceptional — recover the
            // guard rather than let that panic propagate and mask whichever `E` was already
            // recorded before the poisoning store.
            let error = base_context.error_value.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

            return Err(error.as_ref().cloned());
        }

        Ok(())
    }

    /// The number of workers [`execute`](Self::execute) spawns. Its own method so `execute`'s
    /// body reads identically regardless of `cfg(test)`; only this helper's implementation
    /// differs, and the production variant below is textually the same expression `execute` used
    /// before [`Self::with_workers`] existed — the override path is additive, not a behavior
    /// change on the default constructor.
    #[cfg(test)]
    fn worker_count(&self) -> usize {
        self.worker_override.unwrap_or_else(num_cpus::get)
    }

    /// The number of workers [`execute`](Self::execute) spawns: the host's logical CPU count,
    /// read fresh on every call — there is no test-only override in a production build.
    #[cfg(not(test))]
    fn worker_count(&self) -> usize {
        num_cpus::get()
    }
}

/// A worker. It receives tasks from the task queue in the context and executes them.
///
/// # Arguments
/// * `context` - The base task context.
///
/// # Returns
/// * `Ok(true)`  - If there are no more tasks to be executed. All workers should be stopped.
/// * `Ok(false)` - If there are still tasks to be executed.
/// * `Err(())`   - If an error occurred while executing a task. All workers should be stopped.
async fn worker<O: Send, E: Clone + Send>(context: Arc<BaseTaskContext<O, E>>) -> Result<bool, ()> {
    loop {
        if context.error_occurred.load(Ordering::SeqCst) {
            return Err(());
        }

        if context.task_counter.load(Ordering::SeqCst) == 0 {
            return Ok(true);
        }

        // Receive a task from the queue
        let task_result = context.task_receiver.recv_async().await;

        match task_result {
            Ok(task) => {
                // Execute the task
                if let Err(e) = task.await {
                    // First-error-wins: only record the value if none is stored yet (mirrors the
                    // codebase's existing contract — see `inventory_utils::create_inventory_for_directory`'s
                    // `result` comment). Without this guard, a failure discovered later — e.g. a
                    // task whose worker was still running unyielding work during `execute`'s
                    // post-abort drain, and so is guaranteed to finish after an earlier failure
                    // already broke the main loop — would deterministically overwrite a more
                    // actionable earlier error with a later, possibly generic one. The flag below
                    // is still set unconditionally: every failure must still stop the run, whether
                    // or not this one's value won the race to be recorded. Storing the value
                    // before setting the flag only guarantees this branch's own pair is never
                    // torn: no reader can observe this store's flag as true while its value is
                    // not yet visible. It is not a guarantee that the flag always comes with a
                    // value — the main loop's and drain's panic arms set only the flag, never
                    // `error_value` — so a reader must still handle flag-true/value-None; see the
                    // drain's `Ok(_)` comment, and `execute`'s post-drain read, which already does
                    // (returning `Err(None)` in that case).
                    //
                    // This critical section must stay yield-free: `error_value` is a
                    // `std::sync::Mutex` specifically so this store has no `.await` in it. After
                    // `abort_all`, tokio can land a pending cancellation at *any* await point a
                    // worker next reaches — including one it wasn't parked at when `abort_all`
                    // ran, from lock contention or even uncontended coop-budget exhaustion. A
                    // `tokio::sync::Mutex` here would put exactly such an await point between a
                    // task failing and its value being recorded: a worker cancelled inside it
                    // would join as a plain (benign-looking) cancellation, its failure's value
                    // never stored and `error_occurred` never set — silently downgrading a valued
                    // failure to `execute` returning `Err(None)`.
                    {
                        let mut error_value = context.error_value.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        if error_value.is_none() {
                            *error_value = Some(e);
                        }
                    }

                    context.error_occurred.store(true, Ordering::SeqCst);

                    return Err(())
                } else {
                    // Decrement the task counter
                    let remaining = context.task_counter.fetch_sub(1, Ordering::SeqCst);

                    // There are no more tasks to be executed.
                    // We let the main process know by returning true.
                    // `fetch_sub` returns the previous value, so 1 means this was the
                    // last task (and `remaining - 1` would underflow the usize at 0).
                    if remaining <= 1 {
                        return Ok(true);
                    }
                }
            }
            // All senders are disconnected, so there will be no more tasks to execute.
            Err(_) => return Ok(true),
        }
    }
}

/// Every scenario below pins its worker count via [`TaskExecutor::with_workers`] instead of
/// letting it track `num_cpus::get()` (an earlier version of these tests depended on the host
/// having enough logical CPUs to host each scenario's concurrency, printing why and skipping
/// otherwise; that skip path no longer exists, superseded by pinning). Worker futures are
/// ordinary tokio tasks competing for this file's `worker_threads = 8` preemptive runtime, so a
/// small pinned worker count is concurrently resident on any host these tests run on — the number
/// of logical CPUs was never actually the binding constraint on these scenarios, only the size
/// `execute` used to derive its spawn count from was.
///
/// Every test below that needs a second task concurrently resident alongside its initial task
/// sends that second task with `context.send_task(...)` from the *test body*, before calling
/// `executor.execute(...)` — never from inside the initial task's own poll (see
/// [`TaskContext::send_task`]'s doc for the general hazard this sidesteps). The two are not
/// equivalent here for a tokio-specific reason: a task sent from inside another task's poll wakes
/// a parked worker, and tokio places a task woken from thread N into thread N's own LIFO slot,
/// which no other thread can steal from. Every initial task below then blocks its own thread
/// synchronously (in `spin_until`), so if it were also the one doing the sending, the woken
/// worker would sit in that unpollable slot for as long as the sending task's thread stayed
/// blocked — measured, on an earlier version of these tests that sent from inside the initial
/// task, at the full 10-second `spin_until` deadline, reproducing 26/30 and 9/30 observed
/// failures at 2 and 3 workers respectively. A `tokio::task::yield_now().await` inserted between
/// the send and the spin was measured insufficient (4/30 still failing at 2 workers, 7/30 at 3):
/// with a second slow/sibling task sent right after the first, the sending thread re-enters its
/// own unyielding `std::thread::sleep` before the yield reliably gives the runtime a chance to
/// clear the first woken worker's LIFO slot, so the trap still lands often enough to matter.
/// Sending every second task before `execute` avoids the wake path entirely: it is
/// already sitting in the shared channel when the workers spawn, so every worker's first
/// `recv_async` poll finds a task directly, with no wake and no LIFO slot involved. `send_task`
/// is `TaskContext`'s public API, and `task_counter` already accounts for a task sent ahead of
/// `execute`'s own initial send (see `TaskContext::send_task`'s doc) — this is a legitimate call
/// site, not a workaround.
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::time::{Duration, Instant};

    /// Minimal `TaskContext` for exercising `TaskExecutor::execute` directly, without pulling in
    /// any of the real inventory/tree-builder contexts that normally sit on top of it. Generic
    /// over the error type so tests can use distinguishable error values (see
    /// `execute_reports_the_first_task_failure_not_the_last`) instead of `()`.
    struct TestContext<E> {
        base: Arc<BaseTaskContext<(), E>>,
    }

    impl<E: Clone + Send> TestContext<E> {
        fn new() -> Self {
            Self { base: Arc::new(BaseTaskContext::new()) }
        }
    }

    impl<E: Clone + Send> TaskContext<(), E> for TestContext<E> {
        fn get_base_context(&self) -> Arc<BaseTaskContext<(), E>> {
            Arc::clone(&self.base)
        }
    }

    /// Bounded blocking spin-wait shared by the tests below that need a task body to
    /// synchronously (no `.await`) wait on a condition set by another concurrently-resident task
    /// — e.g. "has the sibling task started" or "has the main loop recorded a failure yet". This
    /// is the one place in this file where a task deliberately waits on another task's progress;
    /// unlike the pattern `TaskExecutor::execute`'s doc now forbids in production task bodies, it
    /// is bounded and reports timeout as a distinct, non-panicking failure (`starved`) instead of
    /// hanging or waiting unconditionally.
    ///
    /// Polls `cond` every 5ms for up to 10s. Returns `true` as soon as `cond` is observed true;
    /// on timeout, sets `starved` and returns `false` instead of panicking — a panic here happens
    /// inside a `TaskExecutor` worker, which `JoinSet` converts into an ordinary `Err` rather than
    /// a test-harness panic, so it would otherwise surface only as an unrelated assertion failing
    /// downstream, wrongly implicating the code under test instead of the environment.
    fn spin_until(cond: impl Fn() -> bool, starved: &AtomicBool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);

        while !cond() {
            if Instant::now() >= deadline {
                starved.store(true, Ordering::SeqCst);
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        true
    }

    /// `execute` must not return while a worker is still running synchronous, non-yielding work —
    /// the real case being `LooseObject::store_deferred`'s `compress()`, which has no await point
    /// between winning a write reservation and finishing its write. This reproduces that shape
    /// without touching the object store: `slow_tasks` "slow" tasks record that they started,
    /// then block their worker's OS thread for a fixed duration with `std::thread::sleep` (never
    /// `tokio::time::sleep`, which would give cancellation somewhere to land), then record that
    /// they completed. They are pre-sent to `context` below, before `execute` is even called (see
    /// the module doc above for why); the failing task passed to `execute` only `spin_until`s all
    /// of them are confirmed started — guaranteeing they are already past their only await point
    /// (`recv_async`), and so inside the unyielding sleep, before anything can be cancelled — and
    /// only then fails.
    ///
    /// If `execute` returned as soon as the failure was observed (the bug this guards — see the
    /// method's doc comment for why `abort_all` alone does not stop these slow tasks), they would
    /// keep running past the call, racing the caller. With the fix, `execute` waits for every
    /// worker to actually terminate, so every slow task always reaches "completed" first, and the
    /// failing task's error value survives intact.
    ///
    /// `completed_at_fail` is a vacuity probe, not a timing assumption: the failing task snapshots
    /// how many slow tasks had already finished at the instant it decides to fail, before
    /// `execute`'s main loop can even react. If that snapshot were not strictly less than
    /// `SLOW_TASKS`, every slow task would already be done by the time the drain starts, the
    /// drain would have nothing left to wait for, and the run below would pass without having
    /// exercised the fix at all. Asserting this directly means non-vacuity is enforced by the
    /// test, not derived by reasoning about scheduling — reasoning that has been wrong more than
    /// once in this file's history.
    ///
    /// Pins 3 workers via `TaskExecutor::with_workers` — see the module doc above for why a
    /// pinned count, not `num_cpus::get()`, is what makes this scenario's 3 concurrently-resident
    /// tasks (1 failing + `SLOW_TASKS`) reproducible on any host.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn execute_does_not_return_while_a_worker_is_still_running_synchronous_work() {
        const SLOW_TASKS: usize = 2;

        let context = Arc::new(TestContext::<()>::new());
        let started = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let completed_at_fail = Arc::new(AtomicUsize::new(0));
        let starved = Arc::new(AtomicBool::new(false));

        // Pre-send every slow task before `execute` is called — see the module doc above for why
        // this, not a send from inside the failing task's own poll, is required to avoid starving
        // a woken worker in a non-stealable LIFO slot for the length of the spin-wait below.
        for _ in 0..SLOW_TASKS {
            let started = Arc::clone(&started);
            let completed = Arc::clone(&completed);

            let slow_task: Task<(), ()> = Box::pin(async move {
                started.fetch_add(1, Ordering::SeqCst);

                // Synchronous, non-yielding "work" with no await point between the start and
                // completion markers — mirrors `compress()` on a multi-MB blob. Cancellation
                // cannot land anywhere inside this call.
                std::thread::sleep(Duration::from_millis(300));

                completed.fetch_add(1, Ordering::SeqCst);

                Ok(())
            });

            context.send_task(slow_task).expect("send_task must succeed for the slow tasks");
        }

        let failing_task: Task<(), ()> = {
            let started = Arc::clone(&started);
            let completed = Arc::clone(&completed);
            let completed_at_fail = Arc::clone(&completed_at_fail);
            let starved = Arc::clone(&starved);

            Box::pin(async move {
                // Bounded so a real regression times out this test instead of hanging CI forever;
                // waits until every slow task is confirmed to have started — i.e. past
                // `recv_async` and inside the unyielding sleep — before this task fails.
                if !spin_until(|| started.load(Ordering::SeqCst) >= SLOW_TASKS, &starved) {
                    return Err(());
                }

                // Vacuity probe — see this test's doc comment. Snapshot before failing, so the
                // count reflects what the drain will actually have to wait for.
                completed_at_fail.store(completed.load(Ordering::SeqCst), Ordering::SeqCst);

                Err(())
            })
        };

        let executor = TaskExecutor::with_workers(Arc::clone(&context), 3);
        let result = executor.execute(failing_task).await;

        assert!(
            !starved.load(Ordering::SeqCst),
            "test environment starved the workers (not every slow task got scheduled \
             concurrently with the failing task within the deadline) — not a drain bug"
        );
        assert!(
            completed_at_fail.load(Ordering::SeqCst) < SLOW_TASKS,
            "every slow task had already finished before the failure fired — the drain had \
             nothing to wait for this run; environment stall, not a drain bug"
        );
        assert_eq!(
            result,
            Err(Some(())),
            "the failing task's error must propagate with its value intact"
        );
        assert_eq!(
            completed.load(Ordering::SeqCst),
            SLOW_TASKS,
            "every slow task must run to completion before `execute` returns, not be cut off mid-sleep"
        );
    }

    /// When more than one task fails, `execute` must report the *first* failure observed, not
    /// whichever one happens to finish last — see `inventory_utils::create_inventory_for_directory`'s
    /// `result` doc comment for why the codebase relies on this (a later, possibly less
    /// actionable error, e.g. one surfaced by best-effort cleanup after the real failure, must
    /// never overwrite the earlier, more actionable one).
    ///
    /// Task `Y` is pre-sent to `context` below, before `execute` is even called (see the module
    /// doc above for why); task `X`, passed to `execute`, only `spin_until`s `Y` has provably
    /// started, then fails immediately with error `1`. `Y` sleeps briefly (unyielding
    /// `std::thread::sleep`, never `tokio::time::sleep`) so it is genuinely still mid-work when
    /// `X`'s failure breaks the main loop, then itself `spin_until`s `error_occurred` before
    /// failing with error `2` — so `Y`'s failure is deterministically ordered *after* `X`'s value
    /// is already stored, not merely likely to be by wall-clock margin (no scheduler preemption
    /// can invert it). Without the first-error-wins guard in `worker`, `Y`'s later, drain-window
    /// failure would still deterministically overwrite `X`'s value on every run — this is not a
    /// race to catch, `Y` is made to always lose, so the bug is 100% reproducible.
    ///
    /// Pins 3 workers via `TaskExecutor::with_workers` for consistency with the other two tests
    /// in this file (only 2 — X and Y concurrently resident — are actually needed here).
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn execute_reports_the_first_task_failure_not_the_last() {
        let context = Arc::new(TestContext::<u8>::new());
        let y_started = Arc::new(AtomicBool::new(false));
        let starved = Arc::new(AtomicBool::new(false));

        // Pre-send Y before calling `execute` — see the module doc above for why this, not a
        // send from inside X's own poll, is required.
        let task_y: Task<(), u8> = {
            let context = Arc::clone(&context);
            let y_started = Arc::clone(&y_started);
            let starved = Arc::clone(&starved);

            Box::pin(async move {
                y_started.store(true, Ordering::SeqCst);

                // Short, unyielding "work" — mirrors `compress()` — so Y is genuinely still
                // mid-task when X's failure (below) breaks `execute`'s main loop.
                std::thread::sleep(Duration::from_millis(50));

                // Wait until X's failure is observably recorded before failing, so Y's failure
                // is deterministically ordered after X's value is stored — not merely likely to
                // be by wall-clock margin.
                if !spin_until(
                    || context.get_base_context().error_occurred.load(Ordering::SeqCst),
                    &starved,
                ) {
                    return Err(2u8);
                }

                Err(2u8)
            })
        };

        context.send_task(task_y).expect("send_task must succeed for task Y");

        let task_x: Task<(), u8> = {
            let y_started = Arc::clone(&y_started);
            let starved = Arc::clone(&starved);

            Box::pin(async move {
                if !spin_until(|| y_started.load(Ordering::SeqCst), &starved) {
                    return Err(1u8);
                }

                Err(1u8)
            })
        };

        let executor = TaskExecutor::with_workers(Arc::clone(&context), 3);
        let result = executor.execute(task_x).await;

        assert!(
            !starved.load(Ordering::SeqCst),
            "test environment starved the workers (task Y never started concurrently with task \
             X within the deadline, or never observed X's recorded failure) — not a \
             first-error-wins bug"
        );
        assert_eq!(
            result,
            Err(Some(1)),
            "the first failure observed (X's) must win over Y's later, drain-window failure"
        );
    }

    /// Regression test for a fixed cancellation-window race in `worker`'s task-failure branch: it
    /// used to acquire `error_value`'s lock with a `tokio::sync::Mutex` `.await` between a task
    /// failing and its value/flag being stored. After `abort_all` runs, tokio can land a pending
    /// cancellation at *any* await point a worker next reaches — including a lock acquisition it
    /// was not parked at when `abort_all` was called, from lock contention or even uncontended
    /// coop-budget exhaustion — so a worker landing in that gap could be cancelled before it ever
    /// stored its (distinguishable, valued) failure, and `execute` would report `Err(None)`
    /// despite a valued failure having actually occurred. Swapping `error_value` to a
    /// `std::sync::Mutex` (see `BaseTaskContext`) makes the critical section a synchronous
    /// two-line store with no await point at all, closing the window.
    ///
    /// `Y` is pre-sent to `context` below, before `execute` is even called (see the module doc
    /// above for why). `X`, passed to `execute`, only `spin_until`s `Y` has provably started,
    /// then panics — a panic carries no `E`, so if it were the only recorded failure, `execute`
    /// would return `Err(None)`. `Y` sleeps (unyielding `std::thread::sleep`) long enough to still
    /// be running when `X`'s panic reaches the main loop and breaks it, then fails with a
    /// distinguishable value — guaranteed to land during the post-abort drain, exactly the window
    /// the fixed lock needs to cross without an await point. Asserting `execute` returns
    /// `Err(Some(Y_VALUE))`, not `Err(None)`, proves Y's valued failure survives even though the
    /// *first* failure observed (X's) was a valueless panic.
    ///
    /// `y_failed_at_panic` is this test's vacuity probe (see the join test's doc comment above
    /// for the general rationale): X snapshots whether Y has already failed at the instant X
    /// decides to panic. If Y had already failed by then, Y's failure would have landed in
    /// `execute`'s main loop rather than the drain window this test targets, and the run below
    /// would pass without ever exercising the race under test.
    ///
    /// Pins 3 workers via `TaskExecutor::with_workers` for consistency with the other two tests
    /// in this file (only 2 — X and Y concurrently resident — are actually needed here).
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn execute_preserves_a_drain_window_failure_value_after_a_sibling_panics() {
        const Y_VALUE: u8 = 9;

        let context = Arc::new(TestContext::<u8>::new());
        let y_started = Arc::new(AtomicBool::new(false));
        let y_failed = Arc::new(AtomicBool::new(false));
        let y_failed_at_panic = Arc::new(AtomicBool::new(false));
        let starved = Arc::new(AtomicBool::new(false));

        // Pre-send Y before calling `execute` — see the module doc above for why this, not a
        // send from inside X's own poll, is required.
        let task_y: Task<(), u8> = {
            let y_started = Arc::clone(&y_started);
            let y_failed = Arc::clone(&y_failed);

            Box::pin(async move {
                y_started.store(true, Ordering::SeqCst);

                // Synchronous, non-yielding "work" — mirrors `compress()`. X's panic (below)
                // will already have broken `execute`'s main loop well before this wakes up, so
                // this failure is guaranteed to land during the post-abort drain — exactly the
                // window the fixed `error_value` lock needs to cross without an await point.
                std::thread::sleep(Duration::from_millis(300));

                y_failed.store(true, Ordering::SeqCst);

                Err(Y_VALUE)
            })
        };

        context.send_task(task_y).expect("send_task must succeed for task Y");

        let task_x: Task<(), u8> = {
            let y_started = Arc::clone(&y_started);
            let y_failed = Arc::clone(&y_failed);
            let y_failed_at_panic = Arc::clone(&y_failed_at_panic);
            let starved = Arc::clone(&starved);

            Box::pin(async move {
                if !spin_until(|| y_started.load(Ordering::SeqCst), &starved) {
                    return Err(0u8);
                }

                // Vacuity probe — see this test's doc comment. Snapshot before panicking, so it
                // reflects Y's state at the instant the drain-window race actually starts.
                y_failed_at_panic.store(y_failed.load(Ordering::SeqCst), Ordering::SeqCst);

                panic!(
                    "task X: intentional panic to exercise the panic-then-drain-window-failure \
                     race (expected in the test log — the panic is this test's scenario, not a \
                     test-harness failure)"
                );
            })
        };

        let executor = TaskExecutor::with_workers(Arc::clone(&context), 3);
        let result = executor.execute(task_x).await;

        assert!(
            !starved.load(Ordering::SeqCst),
            "test environment starved the workers (task Y never started concurrently with task \
             X within the deadline) — not the race under test"
        );
        assert!(
            !y_failed_at_panic.load(Ordering::SeqCst),
            "Y had already finished before X's panic fired — the drain-window race this test \
             exercises never happened this run; environment stall, not a bug"
        );
        assert_eq!(
            result,
            Err(Some(Y_VALUE)),
            "Y's valued failure must survive the drain-window cancellation race even though the \
             first failure observed (X's) was a valueless panic"
        );
    }
}
