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

    /// The shared join-point publish batch for this whole `load` (DESIGN.html §5.0 D item 8) —
    /// every directory's shard write is staged here (see [`ShardOutcome`]) and published as one
    /// durability barrier for the whole walk, instead of one per directory. Shard *content* is
    /// staged, and finished, only at the single-threaded join point in
    /// `inventory_utils::create_inventory_for_directory`.
    ///
    /// A changed or brand-new *small* file's blob is staged here too (DESIGN.html §5.0 D item 10,
    /// finding #1) — the one thing a concurrent per-directory task does touch this batch for
    /// directly (via `LooseObject::store_deferred`), since a content-addressed object write is
    /// safe to share across threads exactly like `stack`'s tree build already relies on (see
    /// [`file_utils::WriteBatch::stage`]'s doc comment). Landing blobs and shard content in the
    /// same batch means the same barrier — a blob is always durable no later than the shard that
    /// references it becomes visible. A large (chunked) file's recipe and chunks are a separate,
    /// still-unbatched write this batch never sees — see `inventory_utils::build_inventory`'s own
    /// doc comment for why that is out of scope here.
    pub batch: Arc<file_utils::WriteBatch>,

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
            batch: Arc::new(file_utils::WriteBatch::new()),
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