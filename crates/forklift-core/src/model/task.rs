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
    /// * `Err(Some(e))` - The first task failure observed; `e` is that failure's error value. A
    ///                    later failure (its worker may still have been running unyielding work
    ///                    during the drain above) sets the same failure state but never
    ///                    overwrites this value — see `worker`'s error branch.
    /// * `Err(None)`    - An error occurred with no associated value: either the initial task
    ///                    could not be enqueued, or a worker panicked (a panic carries no `E`).
    pub async fn execute(&self, task: Task<O, E>) -> Result<(), Option<E>> {
        let num_workers = num_cpus::get();
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
                    // or not this one's value won the race to be recorded. Store the value (when
                    // it wins) before setting the flag: readers of the flag must be able to rely
                    // on the value being present once the flag is set.
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

/// Several scenarios below need at least 2 (or 3) concurrently-resident `TaskExecutor` workers
/// to reproduce the timing they exercise — see `sufficient_workers`. On a box with fewer logical
/// CPUs than a scenario needs, those tests print why and skip (green) instead of failing. That
/// residual is accepted deliberately, not overlooked: this is a parallelism-first project (worker
/// count tracks `num_cpus::get()`), CI runners are multi-core, and a hard failure on a low-core
/// box would misattribute an environment limit to the code under test rather than reporting it as
/// what it actually is.
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

    /// `execute` spawns exactly `num_cpus::get()` workers — independent of the tokio runtime's
    /// own OS thread pool size, which only bounds how many of those workers' blocking sections
    /// can run concurrently, not how many exist. That worker count is the real ceiling on how
    /// many tasks a test can keep concurrently resident (in flight) at once: each worker is its
    /// own loop pulling one task at a time from the shared queue, so with fewer workers than a
    /// scenario needs, the "extra" tasks simply queue behind however many workers do exist,
    /// serializing what the test assumed would run in parallel — or, if a task spin-waits on
    /// another task that can never be dequeued (every worker already busy with something that
    /// itself never yields), starving it outright. Returns `None` (after printing why) when the
    /// box hosting the test has fewer than `min_workers` logical CPUs, so a low-core machine
    /// skips the scenario cleanly instead of hanging or failing for an environmental reason a
    /// bare assertion failure would misattribute to the code under test.
    fn sufficient_workers(min_workers: usize) -> Option<usize> {
        let workers = num_cpus::get();

        if workers < min_workers {
            eprintln!(
                "skipping: {workers} logical CPU(s) available, need at least {min_workers} for \
                 this scenario's concurrently-resident `TaskExecutor` workers"
            );

            return None;
        }

        Some(workers)
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
    /// they completed. A third task enqueues those, then uses `spin_until` to block until all of
    /// them are confirmed started — guaranteeing they are already past their only await point
    /// (`recv_async`), and so inside the unyielding sleep, before anything can be cancelled — and
    /// only then fails.
    ///
    /// If `execute` returned as soon as the failure was observed (the bug this guards — see the
    /// method's doc comment for why `abort_all` alone does not stop these slow tasks), they would
    /// keep running past the call, racing the caller. With the fix, `execute` waits for every
    /// worker to actually terminate, so every slow task always reaches "completed" first, and the
    /// failing task's error value survives intact.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn execute_does_not_return_while_a_worker_is_still_running_synchronous_work() {
        // See `sufficient_workers`: the failing task and every slow task must be concurrently
        // resident, so at least 2 workers are required (1 failing + 1 slow); a third slow task
        // is only added when there is a worker to spare for it.
        let Some(workers) = sufficient_workers(2) else { return };
        let slow_tasks: usize = if workers >= 3 { 2 } else { 1 };

        let context = Arc::new(TestContext::<()>::new());
        let started = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let starved = Arc::new(AtomicBool::new(false));

        let failing_task: Task<(), ()> = {
            let context = Arc::clone(&context);
            let started = Arc::clone(&started);
            let completed = Arc::clone(&completed);
            let starved = Arc::clone(&starved);

            Box::pin(async move {
                for _ in 0..slow_tasks {
                    let started = Arc::clone(&started);
                    let completed = Arc::clone(&completed);

                    let slow_task: Task<(), ()> = Box::pin(async move {
                        started.fetch_add(1, Ordering::SeqCst);

                        // Synchronous, non-yielding "work" with no await point between the start
                        // and completion markers — mirrors `compress()` on a multi-MB blob.
                        // Cancellation cannot land anywhere inside this call.
                        std::thread::sleep(Duration::from_millis(300));

                        completed.fetch_add(1, Ordering::SeqCst);

                        Ok(())
                    });

                    context.send_task(slow_task).expect("send_task must succeed for the slow tasks");
                }

                // Bounded so a real regression times out this test instead of hanging CI forever;
                // waits until every slow task is confirmed to have started — i.e. past
                // `recv_async` and inside the unyielding sleep — before this task fails.
                if !spin_until(|| started.load(Ordering::SeqCst) >= slow_tasks, &starved) {
                    return Err(());
                }

                Err(())
            })
        };

        let executor = TaskExecutor::new(Arc::clone(&context));
        let result = executor.execute(failing_task).await;

        assert!(
            !starved.load(Ordering::SeqCst),
            "test environment starved the workers (not every slow task got scheduled \
             concurrently with the failing task within the deadline) — not a drain bug"
        );
        assert_eq!(
            result,
            Err(Some(())),
            "the failing task's error must propagate with its value intact"
        );
        assert_eq!(
            completed.load(Ordering::SeqCst),
            slow_tasks,
            "every slow task must run to completion before `execute` returns, not be cut off mid-sleep"
        );
    }

    /// When more than one task fails, `execute` must report the *first* failure observed, not
    /// whichever one happens to finish last — see `inventory_utils::create_inventory_for_directory`'s
    /// `result` doc comment for why the codebase relies on this (a later, possibly less
    /// actionable error, e.g. one surfaced by best-effort cleanup after the real failure, must
    /// never overwrite the earlier, more actionable one).
    ///
    /// Task `X` enqueues task `Y`, `spin_until`s `Y` has provably started, then fails immediately
    /// with error `1`. `Y` sleeps briefly (unyielding `std::thread::sleep`, never
    /// `tokio::time::sleep`) so it is genuinely still mid-work when `X`'s failure breaks the main
    /// loop, then itself `spin_until`s `error_occurred` before failing with error `2` — so `Y`'s
    /// failure is deterministically ordered *after* `X`'s value is already stored, not merely
    /// likely to be by wall-clock margin (no scheduler preemption can invert it). Without the
    /// first-error-wins guard in `worker`, `Y`'s later, drain-window failure would still
    /// deterministically overwrite `X`'s value on every run — this is not a race to catch, `Y` is
    /// made to always lose, so the bug is 100% reproducible.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn execute_reports_the_first_task_failure_not_the_last() {
        // Both X and Y must be concurrently resident (X spin-waits on Y having started), so at
        // least 2 workers are required — see `sufficient_workers`.
        let Some(_workers) = sufficient_workers(2) else { return };

        let context = Arc::new(TestContext::<u8>::new());
        let y_started = Arc::new(AtomicBool::new(false));
        let starved = Arc::new(AtomicBool::new(false));

        let task_x: Task<(), u8> = {
            let context = Arc::clone(&context);
            let y_started = Arc::clone(&y_started);
            let starved = Arc::clone(&starved);

            Box::pin(async move {
                let task_y: Task<(), u8> = {
                    let context = Arc::clone(&context);
                    let y_started = Arc::clone(&y_started);
                    let starved = Arc::clone(&starved);

                    Box::pin(async move {
                        y_started.store(true, Ordering::SeqCst);

                        // Short, unyielding "work" — mirrors `compress()` — so Y is genuinely
                        // still mid-task when X's failure (below) breaks `execute`'s main loop.
                        std::thread::sleep(Duration::from_millis(50));

                        // Wait until X's failure is observably recorded before failing, so Y's
                        // failure is deterministically ordered after X's value is stored — not
                        // merely likely to be by wall-clock margin.
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

                if !spin_until(|| y_started.load(Ordering::SeqCst), &starved) {
                    return Err(1u8);
                }

                Err(1u8)
            })
        };

        let executor = TaskExecutor::new(Arc::clone(&context));
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
    /// `X` `spin_until`s `Y` has provably started, then panics — a panic carries no `E`, so if it
    /// were the only recorded failure, `execute` would return `Err(None)`. `Y` sleeps (unyielding
    /// `std::thread::sleep`) long enough to still be running when `X`'s panic reaches the main
    /// loop and breaks it, then fails with a distinguishable value — guaranteed to land during the
    /// post-abort drain, exactly the window the fixed lock needs to cross without an await point.
    /// Asserting `execute` returns `Err(Some(Y_VALUE))`, not `Err(None)`, proves Y's valued
    /// failure survives even though the *first* failure observed (X's) was a valueless panic.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn execute_preserves_a_drain_window_failure_value_after_a_sibling_panics() {
        // X and Y must be concurrently resident (X spin-waits on Y having started, then panics
        // while Y is still sleeping), so at least 2 workers are required — see
        // `sufficient_workers`.
        let Some(_workers) = sufficient_workers(2) else { return };

        const Y_VALUE: u8 = 9;

        let context = Arc::new(TestContext::<u8>::new());
        let y_started = Arc::new(AtomicBool::new(false));
        let starved = Arc::new(AtomicBool::new(false));

        let task_x: Task<(), u8> = {
            let context = Arc::clone(&context);
            let y_started = Arc::clone(&y_started);
            let starved = Arc::clone(&starved);

            Box::pin(async move {
                let task_y: Task<(), u8> = {
                    let y_started = Arc::clone(&y_started);

                    Box::pin(async move {
                        y_started.store(true, Ordering::SeqCst);

                        // Synchronous, non-yielding "work" — mirrors `compress()`. X's panic
                        // (below) will already have broken `execute`'s main loop well before this
                        // wakes up, so this failure is guaranteed to land during the post-abort
                        // drain — exactly the window the fixed `error_value` lock needs to cross
                        // without an await point.
                        std::thread::sleep(Duration::from_millis(300));

                        Err(Y_VALUE)
                    })
                };

                context.send_task(task_y).expect("send_task must succeed for task Y");

                if !spin_until(|| y_started.load(Ordering::SeqCst), &starved) {
                    return Err(0u8);
                }

                panic!(
                    "task X: intentional panic to exercise the panic-then-drain-window-failure \
                     race (expected in the test log — the panic is this test's scenario, not a \
                     test-harness failure)"
                );
            })
        };

        let executor = TaskExecutor::new(Arc::clone(&context));
        let result = executor.execute(task_x).await;

        assert!(
            !starved.load(Ordering::SeqCst),
            "test environment starved the workers (task Y never started concurrently with task \
             X within the deadline) — not the race under test"
        );
        assert_eq!(
            result,
            Err(Some(Y_VALUE)),
            "Y's valued failure must survive the drain-window cancellation race even though the \
             first failure observed (X's) was a valueless panic"
        );
    }
}
