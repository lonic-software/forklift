use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::model::task::base_task_context::BaseTaskContext;
use crate::model::tree_item::TreeItem;
use crate::traits::task_context::TaskContext;
use crate::util::file_utils;
use crate::util::inventory_utils;

/// Where a directory's inventory shard content comes from during a tree build.
///
/// `Disk` is the original behavior (every caller except `stack`'s optimized path): read and
/// parse the shard fresh from disk for every directory. `Prepared` looks the shard up in an
/// already-parsed snapshot instead — `stack` reads and parses every shard exactly once
/// (`inventory_utils::prepare_stack_inventory`) and shares that one snapshot across its
/// conflict check, this tree build, and its post-stack cleanup, instead of paying the full
/// shard-directory read+parse pass three separate times.
pub enum ShardSource {
    Disk,
    Prepared(Arc<inventory_utils::PreparedInventory>),
}

/// Where a freshly built tree object is written during a tree build.
///
/// `Immediate` is the original per-object durability: write (and fsync) each object as soon as
/// it is built. `Deferred` stages each write into a [`file_utils::WriteBatch`] instead, so the
/// caller can run one durability barrier for the whole build — see `WriteBatch`'s doc comment
/// for why `stack`'s parallel tree-build workers need this rather than
/// [`file_utils::BulkStoreSession`].
pub enum ObjectSink {
    Immediate,
    Deferred(Arc<file_utils::WriteBatch>),
}

/// The context for the parallel (bottom-up) tree build of a stack: one task per
/// inventoried directory, scheduled by dependency — a directory's task runs once all of
/// its child directories are built. The leaves are enqueued first; each completing task
/// decrements its parent's counter and enqueues the parent when it reaches zero.
pub struct TreeBuilderContext {
    base_context: Arc<BaseTaskContext<(), String>>,

    /// The built trees by directory key. A parent's task moves its children out of this
    /// map; after the walk only the root (key `""`) remains.
    pub built: Arc<Mutex<HashMap<String, TreeItem>>>,

    /// The number of unbuilt child directories per directory key. The task of a
    /// directory is enqueued exactly when its counter reaches zero.
    pub pending_children: Arc<Mutex<HashMap<String, usize>>>,

    /// Where each directory's shard content comes from — see [`ShardSource`]. Set once at
    /// construction, read (never mutated) by every parallel task, so no lock is needed.
    pub shard_source: ShardSource,

    /// Where each built tree object is written — see [`ObjectSink`]. Set once at construction,
    /// read (never mutated) by every parallel task, so no lock is needed.
    pub object_sink: ObjectSink,
}

impl TreeBuilderContext {
    /// Create a new tree builder context.
    ///
    /// # Arguments
    /// * `pending_children` - The initial child counts per directory key.
    /// * `shard_source`     - Where each directory's shard content should be read from.
    /// * `object_sink`      - Where each built tree object should be written.
    ///
    /// # Returns
    /// * `TreeBuilderContext` - The new context.
    pub fn new(pending_children: HashMap<String, usize>,
              shard_source: ShardSource,
              object_sink: ObjectSink) -> Self {
        Self {
            base_context: Arc::new(BaseTaskContext::new()),
            built: Arc::new(Mutex::new(HashMap::new())),
            pending_children: Arc::new(Mutex::new(pending_children)),
            shard_source,
            object_sink,
        }
    }
}

impl TaskContext<(), String> for TreeBuilderContext {
    /// Get the base context.
    ///
    /// # Returns
    /// * `Arc<BaseTaskContext>` - The base context.
    fn get_base_context(&self) -> Arc<BaseTaskContext<(), String>> {
        Arc::clone(&self.base_context)
    }
}
