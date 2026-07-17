use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::Mutex;
use crate::types::task::Task;

pub struct BaseTaskContext<O, E> {
    /// Used for sending tasks to the workers.
    pub task_sender: flume::Sender<Task<O, E>>,

    /// Used by the workers to receive tasks.
    pub task_receiver: flume::Receiver<Task<O, E>>,

    /// Used to keep track of the number of tasks
    /// that are currently being executed / waiting to be executed.
    pub task_counter: Arc<AtomicUsize>,

    // While checking if err_value is Some would make this redundant,
    // this is not behind a mutex so it might be worth keeping.
    /// Used to keep track of whether an error occurred.
    /// When a worker encounters an error, it will set this value.
    /// All workers will stop receiving new tasks.
    pub error_occurred: Arc<AtomicBool>,

    /// Used to store the error value.
    /// When a worker encounters an error, it will set this value.
    ///
    /// Deliberately `std::sync::Mutex`, not `tokio::sync::Mutex`: every critical section that
    /// takes this lock is a synchronous two-line store, never held across an `.await`. Using
    /// tokio's Mutex here would put a fresh await point in `worker`'s failure path — one that
    /// `abort_all`'s pending cancellation could land on before the value/flag are stored,
    /// silently losing a valued failure (see `TaskExecutor::execute`'s doc and `worker`'s
    /// failure-branch comment). A guard from this Mutex held across an `.await` would fail to
    /// compile in a worker future (`MutexGuard` is `!Send`, and worker futures must be `Send`),
    /// so nothing here needs runtime enforcement — but keep the critical sections synchronous
    /// regardless if this type ever changes.
    pub error_value: Arc<Mutex<Option<E>>>,
}

impl <O: Send, E: Clone + Send> BaseTaskContext<O, E> {
    /// Create a new base task context.
    ///
    /// # Returns
    /// * `BaseTaskContext` - The new base task context.
    pub fn new() -> Self {
        let (task_sender, task_receiver) = flume::unbounded();

        // Shared state
        let task_counter = Arc::new(AtomicUsize::new(0));
        let error_occurred = Arc::new(AtomicBool::new(false));
        let error_value = Arc::new(Mutex::new(None));

        Self {
            task_sender,
            task_receiver,
            task_counter,
            error_occurred,
            error_value,
        }
    }
}