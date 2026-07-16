use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::model::task::base_task_context::BaseTaskContext;
use crate::traits::task_context::TaskContext;
use crate::util::file_utils;
use crate::util::inventory_utils::ShardOutcome;

/// The context for the inventory builder task.
pub struct InventoryBuilderContext {
    base_context: Arc<BaseTaskContext<(), String>>,
    /// The paths of the new inventory files.
    pub new_inventory_paths: Arc<Mutex<BTreeSet<String>>>,

    /// The paths of existing inventory files.
    /// When comparing the working directory, the path of the given inventory file should be removed.
    /// Remaining paths are considered dirty  (their corresponding directories have been removed).
    /// These inventories should be removed.
    pub dirty_inventory_paths: Arc<Mutex<BTreeSet<String>>>,

    /// The shared batch for this whole `load`'s per-file blob stores (DESIGN.html §5.0 D item 10)
    /// — the one thing a concurrent per-directory task touches directly (via
    /// `LooseObject::store_deferred`), since a content-addressed object write is safe to share
    /// across threads exactly like `stack`'s tree build already relies on (see
    /// [`file_utils::WriteBatch::stage`]'s doc comment).
    ///
    /// Finished on its own, strictly *before* any shard content is staged (see
    /// `create_inventory_for_directory`'s join point, which staged shard content through its own
    /// local batch inside `publish_shard_outcomes`, not through this context at all): a shard
    /// published afterward can name one of these blobs'
    /// hashes, so the blob must already be durable — not merely staged in some batch that has not
    /// been through its own `finish()` yet — before that shard's rename can land. Sharing one
    /// `WriteBatch` (and hence one `run_write_barrier` call) between blobs and shard content would
    /// not give that ordering: `touched_parents` there is a `BTreeSet<PathBuf>`, so
    /// `.forklift/inventory/` directories sort (and get fsynced) before `.forklift/objects/` ones,
    /// and a crash between the two could durably publish a shard naming a blob whose own rename
    /// never became durable. A large (chunked) file's recipe and chunks are a separate,
    /// still-unbatched write this batch never sees — see `inventory_utils::build_inventory`'s own
    /// doc comment for why that is out of scope here.
    pub blob_batch: Arc<file_utils::WriteBatch>,

    /// Every ancestor key some directory's real content change (or the post-walk dirty-path
    /// deleted-marking) requires invalidated. Collected here by every task instead of cleared
    /// reactively (which would need cross-task locking — see `inventory_utils::build_inventory`'s
    /// own doc comment), and drained once at the single-threaded join point.
    pub clear_keys: Arc<Mutex<BTreeSet<String>>>,

    /// Every directory's write decision this walk, collected instead of written immediately —
    /// see [`ShardOutcome`] and the join point.
    pub outcomes: Arc<Mutex<BTreeMap<String, ShardOutcome>>>,
}

impl InventoryBuilderContext {
    /// Create a new inventory builder context.
    ///
    /// # Returns
    /// * `InventoryBuilderContext` - The new inventory builder context.
    pub fn new() -> Self {
        Self {
            base_context: Arc::new(BaseTaskContext::new()),
            new_inventory_paths: Arc::new(Mutex::new(BTreeSet::new())),
            dirty_inventory_paths: Arc::new(Mutex::new(BTreeSet::new())),
            blob_batch: Arc::new(file_utils::WriteBatch::new()),
            clear_keys: Arc::new(Mutex::new(BTreeSet::new())),
            outcomes: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
}

impl TaskContext<(), String> for InventoryBuilderContext {
    /// Get the base context.
    ///
    /// # Returns
    /// * `Arc<BaseTaskContext>` - The base context.
    fn get_base_context(&self) -> Arc<BaseTaskContext<(), String>> {
        Arc::clone(&self.base_context)
    }
}