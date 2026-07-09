//! The sync/async seam (R4, decided 2026-07-09).
//!
//! # The decision
//!
//! [`ObjectStore`](crate::store::ObjectStore) and [`RefStore`](crate::store::RefStore) stay
//! **synchronous**, and the async boundary lives in the runtime adapter: it runs the whole
//! [`Head`](crate::Head) call on a blocking thread (`tokio::task::spawn_blocking`), exactly
//! as the shipped `forklift-server` runs every handler's filesystem work. An AWS-SDK-backed
//! store — whose every call is a future — bridges each one with [`AsyncBridge`].
//!
//! # Why not async traits
//!
//! Because the audit would have to become async with them, and it cannot:
//!
//! * The audit *is* `forklift_core`, reached through the scratch bridge, and `forklift_core`
//!   is synchronous filesystem code to its roots. An async `ObjectStore` would need an async
//!   audit, which means a second, async implementation of the security-critical verification
//!   path — precisely the drift the scratch bridge exists to prevent (`scratch.rs`).
//! * `audit_utils::verify_parcel_closure_with` takes the blob-existence check as a
//!   `&dyn Fn(&str) -> Result<bool, String>`. A store method awaited from inside it would
//!   have to block anyway.
//! * `StorageRootScope` is a thread-local. It must never cross an `.await`, or a task
//!   resumed on another worker thread would resolve storage paths against the wrong
//!   warehouse — a correctness hole with a tenant boundary on the far side of it. Keeping
//!   the whole `Head` call on one blocking thread makes that unrepresentable.
//!
//! So the seam sits where the runtime already is, and `Head` never learns the word `async`.
//!
//! # Why a multi-thread runtime is required
//!
//! [`AsyncBridge::current`] refuses a `current_thread` runtime. On one, `Handle::block_on`
//! cannot drive the IO and timer drivers: a bridged SDK call only makes progress while some
//! *other* thread happens to be inside `Runtime::block_on`. That is a property no store
//! implementation can check locally, and its failure mode is a hang, not an error — so the
//! bridge refuses up front instead. Lambda's `#[tokio::main]` is multi-thread by default.

use std::future::Future;

use tokio::runtime::{Handle, RuntimeFlavor};

/// Drives an async future to completion from inside a synchronous trait method — the one
/// sanctioned way an async backend (the AWS SDK) implements the sync store traits.
///
/// Construct it on the runtime thread during adapter setup and move it into the store; it is
/// `Send`, `Sync` and cheap to clone. Then call [`block_on`](AsyncBridge::block_on) from the
/// blocking thread the `Head` runs on.
///
/// ```ignore
/// impl ObjectStore for S3ObjectStore {
///     fn exists(&self, hash: &str) -> Result<bool, String> {
///         self.bridge.block_on(async {
///             self.client.head_object().key(hash).send().await // …
///         })
///     }
/// }
/// ```
#[derive(Clone, Debug)]
pub struct AsyncBridge {
    handle: Handle,
}

impl AsyncBridge {
    /// Capture the current runtime. Call this from the runtime thread, before handing the
    /// store to a blocking task.
    ///
    /// Fails when there is no runtime, and when the runtime is `current_thread` (see the
    /// module docs: the bridge would hang rather than fail there).
    pub fn current() -> Result<AsyncBridge, String> {
        let handle = Handle::try_current().map_err(|_| {
            "No tokio runtime is running; an async-backed store must be built inside one so \
             its blocking calls have a runtime to drive them."
                .to_string()
        })?;

        AsyncBridge::from_handle(handle)
    }

    /// Wrap a runtime handle captured elsewhere, applying the same flavour check.
    pub fn from_handle(handle: Handle) -> Result<AsyncBridge, String> {
        if handle.runtime_flavor() == RuntimeFlavor::CurrentThread {
            return Err("This head needs a multi-thread tokio runtime. On a current_thread \
                        runtime a bridged call cannot drive the IO driver and would hang \
                        instead of failing."
                .to_string());
        }

        Ok(AsyncBridge { handle })
    }

    /// Run `future` to completion, blocking this thread until it resolves.
    ///
    /// Must be called from a blocking thread — `spawn_blocking`, or a plain thread outside
    /// the runtime — never from a runtime worker, where tokio refuses to let a thread block.
    /// The `Head` contract (run the call inside `spawn_blocking`) is what guarantees that.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.handle.block_on(future)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_runtime_the_bridge_refuses() {
        let error = AsyncBridge::current().expect_err("no runtime is running");

        assert!(error.contains("No tokio runtime"), "{}", error);
    }

    /// `#[tokio::test]` is a `current_thread` runtime — the flavour the bridge rejects.
    #[tokio::test]
    async fn a_current_thread_runtime_is_refused_rather_than_left_to_hang() {
        let error = AsyncBridge::current().expect_err("current_thread is refused");

        assert!(error.contains("multi-thread"), "{}", error);
    }

    /// The seam itself: a bridge captured on the runtime thread drives a genuinely
    /// suspending future from inside a blocking task — what every SDK-backed store method
    /// does. A future with a real `.await` point proves the driver is running underneath.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_bridged_future_runs_to_completion_on_a_blocking_thread() {
        let bridge = AsyncBridge::current().expect("a multi-thread runtime");

        let answer = tokio::task::spawn_blocking(move || {
            bridge.block_on(async {
                // Suspend and resume: not a future that resolves on first poll.
                tokio::task::yield_now().await;
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;

                41 + 1
            })
        })
        .await
        .expect("the blocking task");

        assert_eq!(answer, 42);
    }

    /// And the reason the `spawn_blocking` contract is a contract and not a style note:
    /// tokio refuses to let a runtime worker thread block on a future. A `Head` driven
    /// straight from an async handler would take this panic on its first store call.
    #[tokio::test(flavor = "multi_thread")]
    async fn blocking_a_runtime_worker_is_refused_by_tokio() {
        let bridge = AsyncBridge::current().expect("a multi-thread runtime");

        // We are on a worker thread here, not a blocking one.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            bridge.block_on(async { 1 })
        }));

        assert!(outcome.is_err(), "tokio must refuse to block a worker thread");
    }
}
