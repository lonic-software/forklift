use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::model::inventory::Inventory;
use crate::model::task::base_task_context::BaseTaskContext;
use crate::traits::task_context::TaskContext;
use crate::util::pack_utils::{PackPreload, PackSetLocation};
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
    /// (`stocktake`, `scope narrow`, `consolidate`, `lower`, `shift`) never reads this map, so they
    /// must not pay for the extra lock per directory that filling it in costs.
    pub shards: Arc<Mutex<HashMap<String, Arc<Inventory>>>>,

    /// Whether the walk should populate [`shards`](Self::shards). `false` for every caller that
    /// only wants the change list — skipping the per-directory `shards` lock keeps their walk as
    /// contention-free as it was before this memo existed.
    pub collect_shards: bool,

    /// The pack set to read ahead for a content render, if the caller asked for one (see
    /// [`with_pack_preload`](Self::with_pack_preload)).
    pack_preload: Option<PackPreloadPlan>,
}

/// What a walk feeding a content render reads ahead: which changes the render will look up in the
/// object store, where that store's packs are, and the read once it has started.
struct PackPreloadPlan {
    renders: Box<dyn Fn(&Change) -> bool + Send + Sync>,
    location: PackSetLocation,
    started: std::sync::Mutex<Option<PackPreload>>,
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
            pack_preload: None,
        }
    }

    /// Read the active warehouse's pack set ahead, in the background, as soon as the walk finds a
    /// change `renders` accepts: one whose stored side the caller will look up afterwards. The
    /// render's first object lookup would otherwise open every pack after the walk instead of
    /// during it, and a walk that finds no such change never pays for the read.
    ///
    /// The pack set's location is resolved here, on the calling thread, because the walk's tasks
    /// run on executor threads that do not carry its storage-root scope.
    pub fn with_pack_preload(mut self, renders: impl Fn(&Change) -> bool + Send + Sync + 'static) -> Self {
        self.pack_preload = Some(PackPreloadPlan {
            renders: Box::new(renders),
            location: PackSetLocation::current(),
            started: std::sync::Mutex::new(None),
        });

        self
    }

    /// Start the pack-set read once per walk if any of `found` is a change the caller renders from
    /// the store. A no-op without [`with_pack_preload`](Self::with_pack_preload).
    pub fn preload_packs_for(&self, found: &[Change]) {
        let Some(plan) = &self.pack_preload else { return; };

        if !found.iter().any(|change| (plan.renders)(change)) {
            return;
        }

        let mut started = plan.started.lock().expect("the pack preload lock is poisoned");

        if started.is_none() {
            *started = Some(plan.location.preload());
        }
    }

    /// Wait for the read [`preload_packs_for`](Self::preload_packs_for) started, if any.
    pub async fn wait_for_packs(&self) {
        let started = self.pack_preload.as_ref()
            .and_then(|plan| plan.started.lock().expect("the pack preload lock is poisoned").take());

        if let Some(preload) = started {
            preload.landed().await;
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
