use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio::sync::Mutex;
use crate::model::task::base_task_context::BaseTaskContext;
use crate::model::tree_item::TreeItem;
use crate::traits::task_context::TaskContext;
use crate::util::file_utils;
use crate::util::inventory_utils;

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

    /// The already-parsed shard snapshot every directory's task reads its content from —
    /// `stack` and `park` both build one (`inventory_utils::prepare_stack_inventory`) and share
    /// it across every step that used to read+parse shards independently. Set once at
    /// construction, read (never mutated) by every parallel task, so no lock is needed.
    pub prepared: Arc<inventory_utils::PreparedInventory>,

    /// Where each built tree object is staged — every caller of this tree build defers its
    /// object writes into one shared [`file_utils::WriteBatch`] instead of fsyncing each as it
    /// is built, so the whole build (and, for `park`, the parcel object too) pays one durability
    /// barrier instead of one per object — see `WriteBatch`'s doc comment for why `stack`'s
    /// parallel tree-build workers need this rather than [`file_utils::BulkStoreSession`]. Set
    /// once at construction, read (never mutated) by every parallel task, so no lock is needed.
    pub batch: Arc<file_utils::WriteBatch>,

    /// Every built directory's tree hash by warehouse path key, kept for the whole build
    /// (unlike `built`, whose entries a parent removes once it consumes them) — the per-key
    /// rollup a caller like `stack` can stamp shards with afterward (DESIGN.html §5.0 D item
    /// 8). Includes synthesized ancestors that have no shard of their own (harmless: a caller
    /// stamping rollups only ever looks up keys that do have a shard). Only ever populated when
    /// [`track_tree_hashes`](Self::track_tree_hashes) is set — see its doc comment.
    pub tree_hashes: Arc<Mutex<HashMap<String, String>>>,

    /// Whether the per-directory build task should bother recording this build's `tree_hashes`
    /// at all. `stack`'s optimized path needs the map (to stamp shards' rollups afterward);
    /// `park` (DESIGN.html §5.0 D item 10, finding #8) passes `false` — it discards the map
    /// immediately (it overwrites every shard from head right afterward, see `park::park_changes`)
    /// but, before this flag existed, still paid the Mutex traffic of every one of its thousands
    /// of per-directory tasks recording into a map nobody read. Set once at construction, read
    /// (never mutated) by every parallel task, so no lock is needed.
    pub track_tree_hashes: bool,

    /// The rollup-skip plan's verbatim injections: for a directory key whose task graph was
    /// *not* pruned, the `(name, head_hash)` pairs of its immediate children that a matching
    /// rollup let the build skip entirely — added directly into that directory's tree with no
    /// load, no task, no `built` lookup (mirrors `build_scoped_root_tree`'s
    /// `splice_out_of_scope_entry` by-hash pattern). Empty when no skip plan applies (every
    /// caller except `stack`'s optimized path, or the kill switch). Set once at construction,
    /// read (never mutated) by every parallel task, so no lock is needed.
    pub injections: Arc<BTreeMap<String, Vec<(String, String)>>>,
}

impl TreeBuilderContext {
    /// Create a new tree builder context.
    ///
    /// # Arguments
    /// * `pending_children`   - The initial child counts per directory key.
    /// * `prepared`           - The already-parsed shard snapshot to read directory content from.
    /// * `batch`              - Where each built tree object should be staged.
    /// * `injections`         - The rollup-skip plan's verbatim injections (empty when no skip
    ///                          plan applies).
    /// * `track_tree_hashes`  - Whether to populate `tree_hashes` at all — see its doc comment.
    ///
    /// # Returns
    /// * `TreeBuilderContext` - The new context.
    pub fn new(pending_children: HashMap<String, usize>,
              prepared: Arc<inventory_utils::PreparedInventory>,
              batch: Arc<file_utils::WriteBatch>,
              injections: Arc<BTreeMap<String, Vec<(String, String)>>>,
              track_tree_hashes: bool) -> Self {
        Self {
            base_context: Arc::new(BaseTaskContext::new()),
            built: Arc::new(Mutex::new(HashMap::new())),
            pending_children: Arc::new(Mutex::new(pending_children)),
            prepared,
            batch,
            tree_hashes: Arc::new(Mutex::new(HashMap::new())),
            track_tree_hashes,
            injections,
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
