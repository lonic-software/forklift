use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::model::inventory::Inventory;
use crate::model::task::base_task_context::BaseTaskContext;
use crate::traits::task_context::TaskContext;
use crate::util::stocktake_utils::Change;

/// The context for the parallel change-collection walks (the staged and unstaged halves
/// of a stocktake): one task per directory, all appending to the shared change list.
pub struct ChangeWalkContext {
    base_context: Arc<BaseTaskContext<(), String>>,

    /// The collected changes, in no particular order — the caller sorts after the walk.
    pub changes: Arc<Mutex<Vec<Change>>>,

    /// The per-directory inventories the walk parsed along the way, keyed by warehouse path
    /// key — an invocation-scoped memo callers can consume afterwards instead of re-reading a
    /// shard the walk already read moments earlier (see `stocktake_utils`'s
    /// `collect_staged_changes_with_shards`/`collect_unstaged_changes_with_shards`, and
    /// `diff.rs`'s `inventory_content`). Dropped with this context at the end of one walk —
    /// never a cross-invocation cache, since a shard is a mutable file, not content-addressed.
    /// Only populated when [`collect_shards`](Self::collect_shards) is set — every other caller
    /// (`stocktake`, `narrow`, `consolidate`, `lower`, `shift`) never reads this map, so they
    /// must not pay for the extra lock per directory that filling it in costs.
    pub shards: Arc<Mutex<HashMap<String, Arc<Inventory>>>>,

    /// Whether the walk should populate [`shards`](Self::shards). `false` for every caller that
    /// only wants the change list — skipping the per-directory `shards` lock keeps their walk as
    /// contention-free as it was before this memo existed.
    pub collect_shards: bool,
}

impl ChangeWalkContext {
    /// Create a new change walk context.
    ///
    /// # Arguments
    /// * `collect_shards` - Whether the walk should populate `shards` (see the field docs). Pass
    ///   `false` unless the caller is actually going to consume the shard memo — populating it
    ///   costs a per-directory lock that a plain change-list walk has no reason to pay.
    ///
    /// # Returns
    /// * `ChangeWalkContext` - The new context.
    pub fn new(collect_shards: bool) -> Self {
        Self {
            base_context: Arc::new(BaseTaskContext::new()),
            changes: Arc::new(Mutex::new(Vec::new())),
            shards: Arc::new(Mutex::new(HashMap::new())),
            collect_shards,
        }
    }
}

impl TaskContext<(), String> for ChangeWalkContext {
    /// Get the base context.
    ///
    /// # Returns
    /// * `Arc<BaseTaskContext>` - The base context.
    fn get_base_context(&self) -> Arc<BaseTaskContext<(), String>> {
        Arc::clone(&self.base_context)
    }
}
