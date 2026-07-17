use std::sync::Arc;
use std::sync::atomic::Ordering;
use crate::model::task::base_task_context::BaseTaskContext;
use crate::types::task::Task;

pub trait TaskContext<O: Send, E: Clone + Send> {
    /// Get the base task context.
    fn get_base_context(&self) -> Arc<BaseTaskContext<O, E>>;

    /// Send a task to the task queue.
    /// This task will be executed by one of the workers.
    ///
    /// Latency note: if the caller is itself running inside a task body, this call can wake an
    /// idle worker waiting on the same queue — and tokio places a task woken from thread N into
    /// thread N's own LIFO slot, which no other worker thread can steal from. If the sending task
    /// then continues on with its own long synchronous work (no further `.await`), the woken
    /// worker sits in that unpollable slot for however long the sender keeps thread N busy — a
    /// real shape in this codebase (the tree builder enqueues child directories, then keeps
    /// walking its own directory's entries). This is a latency hazard, not a correctness one: the
    /// woken task still runs eventually, once the sender yields or finishes. A caller for whom
    /// that delay matters should send from outside any task body still doing long synchronous
    /// work afterward, or `.await` promptly after sending.
    ///
    /// # Arguments
    /// * `task` - The task to send.
    ///
    /// # Returns
    /// * `Ok(())`      - If the task was sent successfully.
    /// * `Err(String)` - If an error occurred while sending the task.
    fn send_task(&self, task: Task<O, E>) -> Result<(), String> {
        let base_context = self.get_base_context();
        base_context.task_counter.fetch_add(1, Ordering::SeqCst);

        base_context.task_sender.send(task).map_err(|e|
            format!("Error while sending task: {}", e)
        )
    }
}