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
    /// # Arguments
    /// * `task` - The task to execute.
    ///
    /// # Returns
    /// * `Ok(())`       - Every task ran to completion without error.
    /// * `Err(Some(e))` - A task returned `Err(e)`; `e` is that task's error value.
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

        // `abort_all` only *signals* cancellation; a worker parked at the `recv_async` await
        // point in the loop below lands it immediately, but a worker inside synchronous,
        // non-yielding work (e.g. mid-`compress()` on a multi-MB blob) does not — it keeps
        // running until that work finishes on its own. Drain every worker's join result so this
        // method cannot return while a task body is still executing: callers finalize resources
        // shared with workers (e.g. `WriteBatch::finish`) right after `execute` returns, and that
        // is only sound if nothing is still writing to them.
        while let Some(drain_result) = worker_join_set.join_next().await {
            match drain_result {
                // Expected: `abort_all` cancels every worker still parked at `recv_async`, and a
                // cancelled join reports as an `Err` with `is_cancelled() == true`. This is not a
                // failure — every healthy run leaves idle workers to cancel here, so treating it
                // as one would turn every successful build into a reported failure.
                Err(ref join_error) if join_error.is_cancelled() => {}
                // A worker panicked after the main loop above already broke out (e.g. two workers
                // panicked back-to-back). Same rationale as the main loop's panic arm: a panicked
                // build must not read as success.
                Err(ref join_error) if join_error.is_panic() => {
                    base_context.error_occurred.store(true, Ordering::SeqCst);
                }
                // Any other join error — treat defensively the same as a panic rather than
                // silently dropping it.
                Err(_) => {
                    base_context.error_occurred.store(true, Ordering::SeqCst);
                }
                // The worker returned normally. `Ok(Err(()))` is a task failure surfacing during
                // the drain, but the worker that produced it already stored the error value and
                // set `error_occurred` itself before returning it (see `worker`'s error branch),
                // so there is nothing left to do with the value here.
                Ok(_) => {}
            }
        }

        // Check if an error occurred. If so, return the error.
        if base_context.error_occurred.load(Ordering::SeqCst) {
            let error = base_context.error_value.lock().await;

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
                    // If an error occurs, store the error value first, and only then set the
                    // error flag: readers of the flag must be able to rely on the value being
                    // present once the flag is set.
                    {
                        let mut error_value = context.error_value.lock().await;
                        *error_value = Some(e);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    /// Minimal `TaskContext` for exercising `TaskExecutor::execute` directly, without pulling in
    /// any of the real inventory/tree-builder contexts that normally sit on top of it.
    struct TestContext {
        base: Arc<BaseTaskContext<(), ()>>,
    }

    impl TestContext {
        fn new() -> Self {
            Self { base: Arc::new(BaseTaskContext::new()) }
        }
    }

    impl TaskContext<(), ()> for TestContext {
        fn get_base_context(&self) -> Arc<BaseTaskContext<(), ()>> {
            Arc::clone(&self.base)
        }
    }

    /// `execute` must not return while a worker is still running synchronous, non-yielding work —
    /// the real case being `LooseObject::store_deferred`'s `compress()`, which has no await point
    /// between winning a write reservation and finishing its write. This reproduces that shape
    /// without touching the object store: two "slow" tasks record that they started, then block
    /// their worker's OS thread for a fixed duration with `std::thread::sleep` (never
    /// `tokio::time::sleep`, which would give cancellation somewhere to land), then record that
    /// they completed. A third task enqueues those two, then blocking-spin-waits until both have
    /// started — guaranteeing they are already past their only await point (`recv_async`), and
    /// so inside the unyielding sleep, before anything can be cancelled — and only then fails.
    ///
    /// If `execute` returned as soon as the failure was observed (the bug this guards), the two
    /// slow tasks would be signaled to abort mid-sleep but keep running past the call, racing the
    /// caller; with a real workload the caller can finalize shared state (e.g.
    /// `WriteBatch::finish`) while a task is still writing to it. With the fix, `execute` waits
    /// for every worker to actually terminate, so both slow tasks always reach "completed" first.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn execute_does_not_return_while_a_worker_is_still_running_synchronous_work() {
        const SLOW_TASKS: usize = 2;

        let context = Arc::new(TestContext::new());
        let started = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));

        let failing_task: Task<(), ()> = {
            let context = Arc::clone(&context);
            let started = Arc::clone(&started);
            let completed = Arc::clone(&completed);

            Box::pin(async move {
                for _ in 0..SLOW_TASKS {
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

                // Blocking spin-wait, bounded so a real regression times out this test instead of
                // hanging CI forever, until both slow tasks are confirmed to have started — i.e.
                // past `recv_async` and inside the unyielding sleep — before this task fails.
                let deadline = Instant::now() + Duration::from_secs(10);

                while started.load(Ordering::SeqCst) < SLOW_TASKS {
                    assert!(Instant::now() < deadline, "timed out waiting for slow tasks to start");
                    std::thread::sleep(Duration::from_millis(5));
                }

                Err(())
            })
        };

        let executor = TaskExecutor::new(Arc::clone(&context));
        let result = executor.execute(failing_task).await;

        assert!(result.is_err(), "the failing task's error must propagate");
        assert_eq!(
            completed.load(Ordering::SeqCst),
            SLOW_TASKS,
            "every slow task must run to completion before `execute` returns, not be cut off mid-sleep"
        );
    }
}
