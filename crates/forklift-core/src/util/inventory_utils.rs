use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use file_id::FileId;
use regex::Regex;
use crate::builder::inventory::InventoryBuilder;
use crate::enums::inventory_item_state::InventoryItemState;
use crate::enums::dir_entry_type::DirEntryType;
use crate::model::inventory::{Inventory, InventoryItem};
use crate::model::object::loose_object::LooseObject;
use crate::model::task::inventory_builder::inventory_builder_context::InventoryBuilderContext;
use crate::model::task::inventory_builder::inventory_builder_task::InventoryBuilderTask;
use crate::model::task::TaskExecutor;
use crate::parser;
use crate::traits::task_context::TaskContext;
use crate::util::{fanout_utils, file_utils, object_utils};
use crate::util::object_utils::IngestMode;
use crate::util::path_utils::WarehousePath;

/// The metadata entry used for the warehouse root (its key is the empty string,
/// which would be confusing as a line in the metadata file).
const METADATA_ENTRY_ROOT: &str = "./";

/// Serializes the read-modify-write step of rollup maintenance (clearing an ancestor's rollup,
/// or writing a mutated shard) against every other such step in this process.
///
/// This is needed because `load`'s per-directory walker (`build_inventory`, below) is genuinely
/// concurrent: a directory's task fires off its subdirectories' tasks and then writes its own
/// shard without waiting for them (see the task's own doc comment). So a directory's shard can
/// be mid-write on one task while a descendant's task is independently walking up to invalidate
/// that same directory's rollup as one of its ancestors. Without serializing the two, a
/// read-modify-write ancestor clear could read stale content, race a sibling task's fresh
/// rewrite of the same file, and lose that rewrite's real content changes — not just its rollup.
///
/// The lock is held only across the (cheap) parse-decide-rewrite of a shard file, never across
/// the (expensive) filesystem walk, hashing or object I/O that produces the content to write —
/// so it costs real parallelism only for that narrow step, and is uncontended (near-free)
/// outside `load`, where every other writer touches shards one at a time to begin with.
static SHARD_MUTATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The warehouse path keys of every strict ancestor of `key`, from the root (`""`) down to
/// (but not including) `key` itself.
fn ancestor_keys_root_first(key: &str) -> Vec<String> {
    let mut chain: Vec<String> = Vec::new();
    let mut current = key;

    while !current.is_empty() {
        current = current.rsplit_once('/').map(|(parent, _)| parent).unwrap_or("");
        chain.push(current.to_string());
    }

    chain.reverse();
    chain
}

/// Stage a single shard's rollup clear into `batch`, if it exists and currently has one. A
/// missing shard, or a shard whose rollup is already `None`, is left untouched — not even
/// staged. The batched counterpart of the old per-shard immediate clear (DESIGN.html §5.0 D
/// item 8, stage 2b): a caller invalidating several ancestors stages them all here and calls
/// [`file_utils::WriteBatch::finish`] once, instead of paying a full atomic-write barrier per
/// shard — see [`clear_ancestors_batched`].
///
/// The staged write preserves the shard's *current* mtime (via
/// [`file_utils::WriteBatch::stage_with_mtime`], set on the temp file before it is ever
/// renamed — no post-`finish` fix-up, and so no crash window where a durable, renamed shard
/// briefly carries the wrong mtime). This matters because a rollup clear is invisible
/// *content*-wise to every other reader (it only ever removes a value nothing outside the
/// rollup machinery consults), but a shard's own mtime is not: `load`'s "racily clean"
/// stat-cache fast path (`is_entry_unchanged`) treats a shard's mtime as proof that its entries
/// were verified against the file system no earlier than that moment. A rollup-only rewrite
/// verifies nothing — so if it were allowed to advance the shard's mtime, a file edited just
/// before the clear could satisfy `mtime < shard_mtime` on a stale cached hash and be wrongly
/// reported unchanged. Preserving the mtime keeps the clear invisible to the stat cache too,
/// exactly as if only the rollup field had changed in place.
///
/// # Returns
/// * `Ok(())`      - The clear was staged (or there was nothing to clear).
/// * `Err(String)` - If the shard could not be read, parsed, or staged.
fn stage_rollup_clear(key: &str, batch: &file_utils::WriteBatch) -> Result<(), String> {
    let (shard_path, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

    let Some(bytes) = bytes_opt else {
        return Ok(());
    };

    let mut inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
        .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?;

    if inventory.get_rollup_hash().is_none() {
        return Ok(());
    }

    // Captured before the rewrite below is even staged — this is the mtime the temp file
    // (and, after the barrier, the renamed final file) will carry instead of "now".
    let original_mtime = file_utils::get_symlink_metadata_for_path(&shard_path).ok()
        .and_then(|m| m.modified().ok());

    inventory.set_rollup_hash(None);
    let bytes = ensure_inventory_folder_and_build(&inventory, &shard_path)?;

    match original_mtime {
        Some(mtime) => batch.stage_with_mtime(&shard_path, &bytes, mtime),
        // No mtime could be determined (the shard's metadata is unreadable, however that
        // happened) — this is not cosmetic: staging a plain write here would carry "now" as the
        // published mtime, which is exactly the racily-clean widening this function exists to
        // avoid (see the doc comment above). With no real mtime to preserve, the safe direction
        // is the most conservative one — an epoch mtime collapses this shard's stat-cache trust
        // entirely, so every one of its entries gets re-verified against the file system on the
        // next load, instead of possibly being trusted on the strength of a timestamp that never
        // actually verified anything.
        None => batch.stage_with_mtime(&shard_path, &bytes, std::time::SystemTime::UNIX_EPOCH),
    }
}

/// Clear the rollup hash of every existing ancestor shard of `key`, from the root down to (but
/// not including) `key` itself, as one batched durability barrier (DESIGN.html §5.0 D item 8,
/// stage 2b) — every ancestor's clear is staged first, then fsynced, renamed and its directory
/// fsynced together in [`file_utils::WriteBatch::finish`], instead of once per ancestor. Must
/// run to completion before a caller writes new content at `key` — see [`write_shard_mutation`],
/// the funnel that does both, in the correct order, as two separate barriers (not one shared
/// batch — see its doc comment for why the boundary between them matters).
fn clear_ancestors_batched(key: &str) -> Result<(), String> {
    let batch = file_utils::WriteBatch::new();

    for ancestor_key in ancestor_keys_root_first(key) {
        stage_rollup_clear(&ancestor_key, &batch)?;
    }

    batch.finish()
}

/// Clear the rollup hash of every existing ancestor shard of `key` — see
/// [`clear_ancestors_batched`] for the batching. Public on its own for a caller that removes
/// (rather than rewrites) the shard at `key` — e.g. `remove_inventories_under` — which still
/// needs its ancestors invalidated but has no new content of its own to write there.
///
/// # Arguments
/// * `key` - The warehouse path key whose ancestors should be invalidated.
///
/// # Returns
/// * `Ok(())`      - Every existing ancestor shard's rollup is now cleared on disk.
/// * `Err(String)` - If an ancestor shard could not be read, parsed or written.
pub fn clear_ancestor_rollups(key: &str) -> Result<(), String> {
    let _guard = SHARD_MUTATION_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    clear_ancestors_batched(key)
}

/// Write a shard whose effective content (any entry's name/type/hash/state) just changed: the
/// rollup hash of every ancestor shard is cleared first, top-down (root first, batched into one
/// durability barrier — see [`clear_ancestors_batched`]), then this shard's own rollup is
/// cleared and its new content written as a second, separate barrier.
///
/// Every writer that changes a shard's effective content must go through this instead of
/// writing the shard directly — a direct write would leave a stale-but-still-matching rollup on
/// an ancestor above the change, silently hiding it from a future rollup-based skip
/// (DESIGN.html §5.0 D item 8). A writer that only refreshes stat data (mtime/ctime/inode) for
/// an otherwise-identical entry should *not* go through this — it may write the shard directly
/// and carry its existing rollup forward unchanged.
///
/// Ordering matters for crash safety: nothing above the mutated shard is ever left stale once
/// this shard's write is durable, because every ancestor is cleared (and durable — the ancestor
/// batch's `finish` has already returned `Ok`) first; a crash before this shard's write only
/// costs a few lost skips (ancestors cleared for a mutation that, from disk's perspective, never
/// actually happened yet), never a wrong one. The two phases are deliberately *not* merged into
/// one shared batch: a single `WriteBatch::finish` gives no ordering guarantee between the
/// renames inside it, only that all of them are durable before it returns — merging the two
/// phases could let the mutated shard's rename land (and become visible) before an ancestor's,
/// which is exactly the ordering this function exists to prevent.
///
/// # Arguments
/// * `key`       - The warehouse path key of the shard being written.
/// * `inventory` - The new content. Its rollup hash is overwritten with `None` here — callers
///                 never need to (and must not) set it themselves.
///
/// # Returns
/// * `Ok(())`      - Every ancestor was invalidated and the shard was written.
/// * `Err(String)` - If an ancestor or this shard could not be read, parsed or written.
pub fn write_shard_mutation(key: &str, inventory: &mut Inventory) -> Result<(), String> {
    let _guard = SHARD_MUTATION_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    clear_ancestors_batched(key)?;

    inventory.set_rollup_hash(None);
    save_inventory(inventory, &file_utils::get_inventory_data_path_for_key(key))
}

/// Process-wide count of rollup-based skips actually applied (DESIGN.html §5.0 D item 8, stage
/// 2) — incremented once per skipped subtree root, by both consumers (the staged-changes walk,
/// `stack`'s tree build). Not a performance feature: a cheap, always-on observability hook the
/// tests use to prove a skip *actually fired* (not just that its output happens to be correct,
/// which the equivalence tests already cover) — see `rollup_skip_count`.
static ROLLUP_SKIP_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Record that a rollup-based skip was just applied to one subtree root. Call exactly once per
/// skip decision (never once per key it covers — a skip's whole point is never visiting its
/// descendants to begin with).
pub fn record_rollup_skip() {
    ROLLUP_SKIP_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The current value of the process-wide rollup-skip counter — see [`record_rollup_skip`].
pub fn rollup_skip_count() -> u64 {
    ROLLUP_SKIP_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether the rollup-based skip (DESIGN.html §5.0 D item 8, stage 2) is currently allowed.
/// Test-only kill switch: set `FORKLIFT_DISABLE_ROLLUP_SKIP=1` to force every consumer (the
/// staged-changes walk in `stocktake_utils`, `stack`'s tree build in `tree_utils`) to behave
/// exactly as if no shard ever carried a rollup — a full walk/build every time. Both consumers
/// call this (never re-implement their own env check) so the equivalence tests can flip one
/// switch and diff the result against the optimized path. Undocumented; not a supported setting.
pub fn rollup_skip_enabled() -> bool {
    std::env::var("FORKLIFT_DISABLE_ROLLUP_SKIP").is_err()
}

/// Peek a shard's rollup hash without parsing its entries. Missing shard, absent rollup, or an
/// old-version shard (which never carries one) all read as `None` — the caller decides what
/// that means (usually: no skip, fall back to the ordinary walk/build).
///
/// # Arguments
/// * `key` - The warehouse path key of the directory.
///
/// # Returns
/// * `Ok(Some(String))` - The shard exists and carries a rollup.
/// * `Ok(None)`         - The shard is missing or carries no rollup.
/// * `Err(String)`      - If the shard exists but its header could not be parsed.
pub fn peek_shard_rollup(key: &str) -> Result<Option<String>, String> {
    let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

    let Some(bytes) = bytes_opt else {
        return Ok(None);
    };

    parser::inventory::inventory_parser::peek_rollup_hash(&bytes)
}

/// Whether two inventories carry the same *effective* content — the same set of entries, each
/// with the same name, type, hash and state. Stat fields (timestamps, device/inode, size,
/// ownership) are deliberately excluded: they can differ (e.g. after a stat-only refresh)
/// without the tree `stack` would build from either inventory differing at all. Used to decide
/// whether a shard rewrite may carry its existing rollup forward instead of invalidating it.
fn inventory_content_matches(a: &Inventory, b: &Inventory) -> bool {
    a.get_items_count() == b.get_items_count()
        && a.get_items().all(|(name, item)| {
            b.get_item_by_name(name).is_some_and(|other| {
                other.hash == item.hash && other.item_type == item.item_type && other.state == item.state
            })
        })
}

/// Add a file or directory to its corresponding inventory.
/// If no inventory exists for the given directory, a new inventory file will be created.
///
/// # Arguments
/// * `path` - The path of the file or directory to add to the inventory.
///
/// # Returns
/// * `Ok(())`      - If the operation was successful.
/// * `Err(String)` - If there was an error.
pub async fn add_changes_to_inventory(path: &WarehousePath) -> Result<(), String> {
    let is_directory = file_utils::is_directory(&path.to_fs_path())?;

    if is_directory {
        create_inventory_for_directory(path).await?;
    } else {
        add_file_to_inventory(path)?;
    }

    Ok(())
}

/// Stage a file or directory for removal: its inventory entries are marked as
/// `Deleted` instead of being erased, so the staged removal is remembered until the next
/// parcel is stacked (and can be reported by status-like commands). The working directory
/// is never touched.
///
/// # Arguments
/// * `path` - The path of the file or directory to stage for removal.
///
/// # Returns
/// * `Ok(())`      - If the operation was successful.
/// * `Err(String)` - If there was an error.
pub fn stage_removal(path: &WarehousePath) -> Result<(), String> {
    // A directory is recognized by its inventory shard, not by the file system state:
    // the subject may already be gone from the working directory, and staging its removal
    // must still work in that case.
    let has_shard = file_utils::get_inventory_data_path_for_key(path.as_key()).exists();

    if path.is_root() || has_shard {
        return stage_removal_for_directory(path);
    }

    let fs_path = path.to_fs_path();

    if fs_path.exists() && file_utils::is_directory(&fs_path)? {
        return Err(format!("No inventory found for folder \"{}\".", path.as_key()));
    }

    stage_removal_for_file(path)
}

/// Create an inventory for the specified directory (and all subdirectories).
///
/// A failure while deciding one directory's content does not cost the whole walk by itself:
/// whatever every other task decided is still published (and registered in the metadata file),
/// so previously loaded, unrelated inventories are never destroyed. But every blob this walk
/// stages shares one batch, and a producer that fails between winning a blob's reservation and
/// finishing its write leaks that reservation — `WriteBatch::finish` then refuses to publish the
/// *whole* batch, so nothing this walk decided is published this round (see the phase-0 comment
/// in the function body for the full reasoning). A producer that fails before ever winning a
/// reservation leaks nothing and only costs its own shard's decision, same as any other per-shard
/// failure. Either way, re-running the load after fixing the problem completes the inventory —
/// nothing already published on disk is ever destroyed by a later, failed pass.
///
/// # Arguments
/// * `path` - The path to the directory.
///
/// # Returns
/// * `Ok(())`      - If the inventory was successfully created.
/// * `Err(String)` - If an error occurred while creating the inventory.
pub async fn create_inventory_for_directory(path: &WarehousePath) -> Result<(), String> {
    let context = Arc::new(InventoryBuilderContext::new());
    let executor = TaskExecutor::new(Arc::clone(&context));
    let ignored_paths = file_utils::get_ignored_paths()?;

    // Every previously inventoried directory inside the loaded subtree starts out "dirty";
    // the walk removes each directory it visits. Whatever is left afterwards no longer
    // exists in the working directory (or is ignored now), so its entries are staged
    // as removals.
    populate_dirty_inventory_paths(&context, path).await?;

    let root_task: InventoryBuilderTask = Box::pin(build_inventory(
        Arc::clone(&context),
        Arc::new(path.clone()),
        Arc::new(ignored_paths)
    ));

    // `result` accumulates the *first* failure from anywhere in this function — the walk itself,
    // or any fallible step of the join point below. Every join-point step still runs its
    // best-effort work even once `result` already holds an error (exactly as the walk's own
    // failure already tolerated: whatever the tasks that did complete decided is still published
    // — see the join point's own doc comment), but never lets a later, possibly less useful,
    // error overwrite the first one a caller would want to see.
    let mut result: Result<(), Option<String>> = executor.execute(root_task).await;

    // Phase 0: every changed or brand-new small file's blob the walk staged becomes durable now,
    // in its own barrier — strictly before any shard content is even staged below (DESIGN.html
    // §5.0 D item 10, findings #1/#7). A shard phase B publishes can name one of these blobs'
    // hashes, so the blob must already be durable — not merely staged in a batch that has not yet
    // been through its own `finish()` — before that shard's rename is allowed to land. See
    // `publish_shard_outcomes`'s doc comment for why blobs and shard content cannot share one
    // batch and still give that ordering.
    //
    // A failure here is different from every other failure this join point tolerates: this batch
    // covers *every* blob the whole walk decided, and `WriteBatch::finish` can fail more than one
    // way (see its own `# Returns` for the full breakdown) — no failure mode leaves a per-hash way
    // to tell which blobs actually landed, and some (a rename partway, or the barrier's own
    // trailing directory sync — see `run_write_barrier` for why that is not all-or-nothing) leave
    // some of this batch's blobs durable and others not. Either way, publishing any shard content
    // below could then durably name a blob that never actually landed. So a failure here — unlike
    // the walk's own failure, which still publishes whatever it *did* decide — skips phase A/B
    // entirely (`blob_batch_published` below): nothing this walk decided is published this round,
    // and a retry starts over. Still strictly better than the pre-batching baseline, where a torn
    // single-object write could only ever cost that one file, never an entire load, and losing a
    // whole walk's *decisions* (not their eventual content — nothing here is destructive) to a
    // retry is a fair trade for closing finding #7.
    //
    // The blast radius is deliberate, but only for the producer failure that actually causes it: a
    // producer that fails *between* winning a `reserve_final_path` reservation (see
    // `LooseObject::store_deferred`) and finishing its write leaves that reservation leaked, and
    // `WriteBatch::finish` refuses the whole batch before anything is renamed (see its own doc
    // comment) — every other occurrence of that same content already saw the reservation lost and
    // staged nothing itself, so the missing blob could be named by any shard's decision, not just
    // the one whose producer failed. A producer that fails *before* ever winning a reservation (the
    // object-ceiling check, `does_object_exist`, or the file read itself) leaks nothing, so it does
    // not by itself force that outcome — that shard's own decision simply fails, exactly like any
    // other per-shard failure this join point already tolerates.
    let mut blob_batch_published = true;

    if let Err(e) = context.blob_batch.finish() {
        if result.is_ok() { result = Err(Some(e)); }
        blob_batch_published = false;
    }

    // The join point (DESIGN.html §5.0 D item 8, stage 2b): every per-directory task only ever
    // *decided* what to write (`ShardOutcome`, collected in `context.outcomes`) and which
    // ancestors that decision invalidates (`context.clear_keys`) — nothing was written to disk
    // by any task. Publishing happens here, once, single-threaded, after every task that managed
    // to run has reported in (whether or not the walk as a whole succeeded — see below).
    let mut clear_keys: BTreeSet<String> = std::mem::take(&mut *context.clear_keys.lock().await);
    let mut outcomes: BTreeMap<String, ShardOutcome> = std::mem::take(&mut *context.outcomes.lock().await);
    let mut stale_keys: BTreeSet<String> = BTreeSet::new();

    // Directories that are gone from the working directory (deleted, or ignored now) keep their
    // inventory shard, with every entry marked as a staged removal. Folded into the same join
    // point as every other mutation this walk makes, rather than its own separate atomic write
    // per directory. Only ever considered after a fully successful walk — on failure, the walk
    // may not have reached every directory, so a shard still marked "dirty" here might simply be
    // one it hasn't visited yet, not one that is actually gone (the original contract, preserved
    // below: dirty inventories are never removed from metadata unless this pass actually proved
    // them stale).
    //
    // A failure partway through this pass (an unreadable or corrupt leftover shard) stops the
    // pass right there — exactly like a walk failure, whatever was classified before the failure
    // is kept, and the loop's own error is folded into `result` instead of aborting the function:
    // the failure-resilience branch below must still run so the shards this walk *did* manage to
    // publish stay registered, and the user sees the same "entries loaded so far were kept"
    // message a walk failure gives, not a raw, unwrapped parse error.
    if result.is_ok() {
        let dirty_paths = context.dirty_inventory_paths.lock().await;

        for dirty_key in dirty_paths.iter() {
            // Captured right when this pass starts reading this shard — the same "no later than
            // any verification actually performed" anchor `build_inventory`'s own tasks use (see
            // `ShardOutcome`'s doc comment); here, "verification" is this read itself.
            let verified_at = std::time::SystemTime::now();

            let bytes_opt = match file_utils::retrieve_inventory_or_none_by_key(dirty_key) {
                Ok((_, bytes_opt)) => bytes_opt,
                Err(e) => { result = Err(Some(e)); break; }
            };

            let Some(bytes) = bytes_opt else {
                // The shard itself is gone — every ancestor's rollup that named a tree including
                // this directory is now wrong (it would still match head, but head no longer
                // reflects a subtree that no longer exists), so they must be invalidated exactly
                // like any other real content change, not just dropped from metadata.
                clear_keys.extend(ancestor_keys_root_first(dirty_key));
                stale_keys.insert(dirty_key.clone());
                continue;
            };

            let inventory_result = parser::inventory::inventory_parser::parse_inventory(&bytes)
                .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", dirty_key, e));

            let mut inventory = match inventory_result {
                Ok(inventory) => inventory,
                Err(e) => { result = Err(Some(e)); break; }
            };

            if inventory.mark_all_items_deleted() {
                clear_keys.extend(ancestor_keys_root_first(dirty_key));
                inventory.set_rollup_hash(None);
                outcomes.insert(dirty_key.clone(), ShardOutcome::Changed(inventory, verified_at));
            }
        }
    }

    // Steps 1-3 (phase A ancestor clears, then phase B shard content): shared with `park`'s
    // working-directory refresh via `publish_shard_outcomes` — see its own doc comment for the
    // exact ordering. Only attempted when phase 0 (the blob barrier above) fully succeeded — see
    // its own doc comment for why a blob-batch failure must not let any shard content publish at
    // all. A failure in either phase A or phase B here is folded into `result` instead of
    // propagated immediately, and `phase_b_published` (gating metadata registration below) is set
    // `false` on any failure in either phase — but a batch's `finish` failing does not mean
    // nothing it staged was renamed (see its own `# Returns`): a rename partway through leaves
    // every entry renamed before it visible, and the barrier's own trailing directory sync failing
    // (every rename already succeeded — see `run_write_barrier`) leaves the whole batch visible,
    // with only directory durability left unproven. For phase A that is harmless either way — the
    // shards it clears were already registered from before this walk, so a clear landing or not
    // costs only a few lost rollup skips next time, never a wrong one (see `stage_rollup_clear`'s
    // doc comment). For phase B it is not: a rename or trailing-sync failure partway through
    // publishing new shard content can leave one or more of those shards durably visible on disk
    // while `phase_b_published` is still `false` — `keys_to_add` below then excludes exactly those
    // keys from `update_inventory_metadata`, so a freshly published directory can sit on disk,
    // correct and durable, yet go unregistered and invisible to stocktake until some later pass
    // redecides and republishes it.
    let phase_b_published = if blob_batch_published {
        match publish_shard_outcomes(&clear_keys, &mut outcomes) {
            Ok(()) => true,
            Err(e) => { if result.is_ok() { result = Err(Some(e)); } false }
        }
    } else {
        false
    };

    // Metadata must never claim a directory's shard is on disk when publishing it may not have
    // actually happened: a key with a recorded `ShardOutcome` only belongs in the registered set
    // when phase B is known to have durably published every outcome (one shared batch — see
    // above). Every other visited directory (no outcome: its shard was already correct and
    // durable from before this walk, untouched either way) is unaffected by phase A/B and is
    // always safe to (re-)register.
    let keys_to_add: BTreeSet<String> = {
        let new_inventory_paths = context.new_inventory_paths.lock().await;

        if phase_b_published {
            new_inventory_paths.clone()
        } else {
            new_inventory_paths.iter().filter(|key| !outcomes.contains_key(key.as_str())).cloned().collect()
        }
    };

    if let Err(e) = result {
        // Register every inventory that was actually published, even on failure, so the metadata
        // file stays consistent with what exists on disk. Stale keys are only ever populated by
        // a dirty-path pass that ran (in full, or up to the point it failed) over a fully
        // successful walk, so removing them here is exactly as safe as it is on the ordinary
        // success path below.
        update_inventory_metadata(&keys_to_add, &stale_keys)?;

        let message = e.unwrap_or("An unknown error occurred while building the inventory.".to_string());

        return Err(format!(
            "{}\nThe load did not complete; entries loaded so far were kept. \
            Re-run the load once the problem is fixed.",
            message
        ));
    }

    update_inventory_metadata(&keys_to_add, &stale_keys)?;

    Ok(())
}

/// Mark every previously inventoried directory inside the given subtree as dirty.
/// Directories visited by the inventory build remove themselves from this set, so the
/// directories remaining after the walk are the ones deleted from the working directory.
///
/// # Arguments
/// * `context` - The inventory builder context.
/// * `path`    - The root of the subtree being loaded.
///
/// # Returns
/// * `Ok(())`      - If the dirty set was populated successfully.
/// * `Err(String)` - If the inventory metadata could not be read.
async fn populate_dirty_inventory_paths(context: &InventoryBuilderContext,
                                        path: &WarehousePath) -> Result<(), String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    let Some(metadata) = metadata_opt else {
        return Ok(());
    };

    let subtree_prefix = format!("{}/", path.as_key());
    let mut dirty = context.dirty_inventory_paths.lock().await;

    for entry in metadata {
        let key = metadata_entry_to_key(&entry);

        let is_in_subtree = path.is_root()
            || key == path.as_key()
            || key.starts_with(&subtree_prefix);

        if is_in_subtree {
            dirty.insert(key.to_string());
        }
    }

    Ok(())
}

/// Convert an inventory metadata file entry to a warehouse path key.
/// The warehouse root is stored as `./` in the metadata file, but its key is the empty string.
pub fn metadata_entry_to_key(entry: &str) -> &str {
    if entry == METADATA_ENTRY_ROOT { "" } else { entry }
}

/// Convert a warehouse path key to its inventory metadata file entry.
fn key_to_metadata_entry(key: &str) -> String {
    if key.is_empty() { String::from(METADATA_ENTRY_ROOT) } else { key.to_string() }
}

/// Stage the removal of a directory: mark every entry in its inventory (and in the
/// inventories of all of its subdirectories) as `Deleted`. The inventory shards and their
/// metadata entries are kept — they are the record of the staged removals.
///
/// # Arguments
/// * `path` - The path of the folder in the working dir.
///
/// # Returns
/// * `Ok(())`      - If the removals were staged successfully.
/// * `Err(String)` - If no inventory exists for the folder, or there was an error.
fn stage_removal_for_directory(path: &WarehousePath) -> Result<(), String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    // The directory's own key is processed even when the metadata file is missing or
    // inconsistent; the metadata supplies the subdirectory shards.
    let mut keys: BTreeSet<String> = BTreeSet::new();
    keys.insert(path.as_key().to_string());

    if let Some(metadata) = metadata_opt {
        let subtree_prefix = format!("{}/", path.as_key());

        for entry in &metadata {
            let key = metadata_entry_to_key(entry);

            if path.is_root() || key.starts_with(&subtree_prefix) {
                keys.insert(key.to_string());
            }
        }
    }

    let mut found_any_shard = false;

    for key in &keys {
        if mark_shard_entries_deleted(key)? {
            found_any_shard = true;
        }
    }

    if !found_any_shard {
        return Err(format!(
            "No inventory found for folder \"{}\".",
            if path.is_root() { METADATA_ENTRY_ROOT } else { path.as_key() }
        ));
    }

    Ok(())
}

/// Mark every entry of the inventory shard with the given key as `Deleted`.
/// The shard is only rewritten when an entry actually changed state.
///
/// # Arguments
/// * `key` - The warehouse path key of the directory whose shard should be marked.
///
/// # Returns
/// * `Ok(true)`    - If the shard exists (whether or not entries changed state).
/// * `Ok(false)`   - If no shard exists for the given key.
/// * `Err(String)` - If the shard could not be read, parsed or written.
fn mark_shard_entries_deleted(key: &str) -> Result<bool, String> {
    let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

    let Some(bytes) = bytes_opt else {
        return Ok(false);
    };

    let mut inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
        .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?;

    if inventory.mark_all_items_deleted() {
        write_shard_mutation(key, &mut inventory)?;
    }

    Ok(true)
}

/// One shard's write decision, collected for a single-threaded join point to publish
/// (`publish_shard_outcomes`) instead of written immediately from whatever decided it. Two
/// producers share this shape: `load`'s parallel walk (`build_inventory`, one task per
/// directory, joined by `create_inventory_for_directory`) and `park`'s working-directory refresh
/// (`refresh_tracked_entries`, a single-threaded loop over every tracked shard) — see each
/// producer's own doc comment for why a decision never touches disk directly.
pub enum ShardOutcome {
    /// Content unchanged from the previous shard (same name/type/hash/state for every entry),
    /// but it still needs rewriting: stat data drifted, or at least one entry had to be
    /// re-verified (read and rehashed) even though it ended up matching. Carries the previous
    /// rollup forward — the join point drops it back to `None` instead if this key turns out to
    /// be an ancestor of some other real change in the same batch.
    ///
    /// The `SystemTime` is the shard's *published* mtime — see the join point's publish step for
    /// why it is not simply "now".
    Carry(Inventory, std::time::SystemTime),

    /// Effective content changed (a brand-new shard, an entry added or removed, or any entry's
    /// name/type/hash/state differs from before) — or every non-`Deleted` item was just staged
    /// for removal (`load`'s post-walk dirty-path pass, or a file gone from disk during a
    /// refresh). Always published with rollup `None`.
    ///
    /// The `SystemTime` is the shard's *published* mtime — see the join point's publish step for
    /// why it is not simply "now".
    Changed(Inventory, std::time::SystemTime),
}

/// Publish a batch of [`ShardOutcome`] decisions through the same two-phase ordering `load`'s
/// join point (`create_inventory_for_directory`) and `park`'s working-directory refresh
/// (`refresh_tracked_entries`) both need: drop a carried rollup that turns out to be an ancestor
/// of some other real change in the very same batch (step 1, in-memory only — this is what closes
/// DESIGN.html §5.0 D item 10, finding #1: a decision made *before* a sibling's later-processed
/// real change is never blindly restamped over the ancestor clear that change requires, because
/// this check runs after every decision in the batch is already known), then clear every *other*
/// invalidation target as one batched, mtime-preserving barrier (phase A), then publish every
/// outcome's own new content as a second, separate barrier (phase B). The two phases are
/// deliberately never merged into one shared batch — see [`write_shard_mutation`]'s doc comment
/// for why the ordering boundary between "an ancestor's clear is durable" and "the mutated
/// shard's own content is durable" matters.
///
/// Any blob a published outcome's content might reference must already be durable *before* this
/// is called — neither phase here stages a blob write; each caller finishes its own blob batch
/// first (DESIGN.html §5.0 D item 10, finding #7): `create_inventory_for_directory`'s
/// `context.blob_batch`, `refresh_tracked_entries`'s own local one.
///
/// A failure in either phase is folded into the returned `Err` and the *other* phase is skipped
/// (phase A failing skips phase B entirely, preserving `write_shard_mutation`'s ordering
/// invariant; phase B is simply not attempted if phase A never returns `Ok`) — every caller
/// treats an `Err` here as "nothing in `outcomes` was published", exactly as if this were still
/// one inline step of its own join point.
///
/// # Arguments
/// * `clear_keys` - Every ancestor key some outcome's real content change invalidates.
/// * `outcomes`   - Every shard this batch decided to (re)write, by key. Mutated in place (a
///                  carried rollup may be dropped) before anything is staged.
///
/// # Returns
/// * `Ok(())`      - Phase A and phase B both fully succeeded: every outcome in `outcomes` is
///                   durably published, and every ancestor clear this batch owed is durable too.
/// * `Err(String)` - Phase A or phase B failed; nothing in `outcomes` may be treated as published.
fn publish_shard_outcomes(clear_keys: &BTreeSet<String>,
                          outcomes: &mut BTreeMap<String, ShardOutcome>) -> Result<(), String> {
    for (key, outcome) in outcomes.iter_mut() {
        if let ShardOutcome::Carry(inventory, _) = outcome {
            if clear_keys.contains(key) {
                inventory.set_rollup_hash(None);
            }
        }
    }

    let phase_a_batch = file_utils::WriteBatch::new();

    for key in clear_keys.iter().filter(|key| !outcomes.contains_key(key.as_str())) {
        stage_rollup_clear(key, &phase_a_batch)?;
    }

    phase_a_batch.finish()?;

    let phase_b_batch = file_utils::WriteBatch::new();

    for (key, outcome) in outcomes.iter() {
        let (inventory, verified_at) = match outcome {
            ShardOutcome::Carry(inventory, verified_at) | ShardOutcome::Changed(inventory, verified_at) =>
                (inventory, *verified_at),
        };

        save_inventory_deferred_with_mtime(
            inventory, &file_utils::get_inventory_data_path_for_key(key), &phase_b_batch, verified_at,
        )?;
    }

    phase_b_batch.finish()
}

/// A task for building an inventory file for a given directory.
/// When encountering a subdirectory, a new task is created to build the inventory for that directory.
///
/// Never writes *shard content* to disk itself: it only *decides* this directory's
/// [`ShardOutcome`] (or that nothing changed at all, in which case it decides nothing) and
/// records that decision — plus, for a real content change, this directory's ancestor keys — in
/// the shared [`InventoryBuilderContext`], for `create_inventory_for_directory`'s single-threaded
/// join point to publish once every task has run. This is deliberate: this walker is
/// fire-and-forget concurrent (a directory's task fires off its subdirectories' tasks and returns
/// without waiting for them), so writing — and, worse, invalidating ancestors — directly from
/// here would need the same cross-task synchronization every other writer's funnel
/// ([`write_shard_mutation`]) uses, one durability barrier at a time. Deferring the decision to
/// a join point instead turns the whole walk into at most three barriers total (DESIGN.html §5.0
/// D item 10, finding #7 — one more than the two of the original stage 2b design, because the
/// blob barrier below must complete, on its own, before shard content publishes), and needs no
/// lock at all: nothing concurrent ever touches another task's key.
///
/// A changed or brand-new *small* file's blob is the one exception (DESIGN.html §5.0 D item 10,
/// finding #1): its content is content-addressed and its write is staged (not fsynced) via
/// [`file_utils::WriteBatch::stage`] into `context.blob_batch` — a batch the join point finishes,
/// on its own, strictly *before* it stages any shard content (see `publish_shard_outcomes`'s doc
/// comment for why the two must not share one batch) — instead of paying its own atomic-write
/// barrier per file. This is safe to do straight from a concurrent task (unlike a shard's
/// content): `WriteBatch::stage` is documented safe for concurrent callers. Two tasks staging the
/// same hash never both pay the compress-and-write cost either (DESIGN.html §5.0 D item 10,
/// finding #2): `LooseObject::store_deferred` reserves the
/// object's final path in the batch before compressing, so only the first occurrence actually
/// stages anything — see [`file_utils::WriteBatch::reserve_final_path`]'s doc comment. Every blob
/// staged here is durable — `blob_batch.finish()` has already returned `Ok` — strictly before any
/// shard content that might reference it is even staged, never merely "in the same barrier".
///
/// A large (chunked) file's recipe and chunks are a separate, still-unbatched exception this
/// finding does not touch: `object_utils::ingest_file`'s `IngestMode::Store` path stores each of
/// them immediately (`flush_chunk_batch`/the recipe's own `store()`), still from within this same
/// concurrent task. Out of scope here — chunking has its own design (§9.4b) and its own
/// parallelism (`fanout_utils::fanout_map`, not this walk's per-directory `TaskExecutor`) — and
/// `build_item_and_object_for_file` only ever returns a blob (the `Option<LooseObject>` above) for
/// a small file to begin with; a chunked file's `object` is always `None` here.
///
/// # Arguments
/// * `context`         - The task context.
/// * `path`            - The warehouse path of the directory.
/// * `paths_to_ignore` - Paths of files and directories that should be ignored. The patterns are
/// matched against warehouse path keys (see `WarehousePath::as_key`).
///
/// # Returns
/// * `Ok(())`      - If the directory was classified successfully.
/// * `Err(String)` - If there was an error during the operation.
fn build_inventory(context: Arc<InventoryBuilderContext>,
                   path: Arc<WarehousePath>,
                   paths_to_ignore: Arc<Vec<Regex>>) -> impl Future<Output = Result<(), String>> + Send {
    async move {
        // Captured before this task reads, stats or hashes anything below — the timestamp this
        // directory's published shard will carry as its mtime (see the join point's publish
        // step). Deliberately the task's *start*, not its end or the join point's later publish
        // time: it must be no later than any verification (a stat, a rehash) this task actually
        // performs, so a file edited after that verification but before the join point publishes
        // is never mistaken, on a future load, for one that was already accounted for.
        let verified_at = std::time::SystemTime::now();

        let directory = file_utils::read_directory(&path.to_fs_path())?;

        if !path.is_root() && file_utils::is_path_ignored(path.as_key(), &paths_to_ignore) {
            return Ok(());
        }

        // The existing inventory of this directory (if any) is the stat cache: entries whose
        // file metadata is unchanged are reused without reading or hashing the file.
        // An unreadable or unparsable shard simply means a full rebuild of this directory.
        // The shard's own modification time is needed to reject "racily clean" entries
        // (see `is_entry_unchanged`). Its rollup (if any) is captured here too, at task start —
        // the tentative "carry forward" value if this directory's content turns out unchanged.
        let existing_inventory = match file_utils::retrieve_inventory_or_none_by_key(path.as_key()) {
            Ok((shard_path, Some(bytes))) => {
                let shard_mtime = file_utils::get_symlink_metadata_for_path(&shard_path).ok()
                    .and_then(|m| file_utils::get_content_modification_timestamp_for_file(&m).ok());

                parser::inventory::inventory_parser::parse_inventory(&bytes).ok().zip(shard_mtime)
            }
            _ => None,
        };

        let mut inventory = Inventory::new();
        // Whether any entry needed more than the pure stat-cache fast path (read, rehashed, or
        // newly built) — see the case (a)/(b) split below.
        let mut any_entry_rebuilt = false;

        {
            let key = path.as_key().to_string();
            context.new_inventory_paths.lock().await.insert(key.clone());
            context.dirty_inventory_paths.lock().await.remove(&key);
        }

        for entry_result in directory {
            let entry = entry_result.map_err(|e|
                format!("Error while reading directory entry: {}", e)
            )?;

            let name = file_utils::get_name_for_file_or_directory(&entry)?;
            let metadata = file_utils::get_symlink_metadata_for_path(&entry.path())?;
            let item_type = file_utils::get_type_of_dir_entry(&metadata);
            let entry_path = path.child(&name);

            if file_utils::is_path_ignored(entry_path.as_key(), &paths_to_ignore) {
                continue;
            }

            if item_type.is_file() {
                let existing_entry = existing_inventory.as_ref()
                    .and_then(|(inv, shard_mtime)| {
                        inv.get_item_by_name(&name).map(|item| (item, *shard_mtime))
                    });

                let index_item = match existing_entry {
                    Some((item, shard_mtime)) => {
                        let verdict = classify_file_against_entry(
                            &item, &metadata, item_type, &entry.path(), &name, shard_mtime,
                            IngestMode::Store,
                        )?;

                        match verdict {
                            FileVerdict::UnchangedByStat => {
                                // Loading stages the *current* state: a file that is present
                                // on disk is staged as Normal even if it was staged for
                                // removal before (the same way "git add" re-stages a file
                                // after "git rm --cached"). A state flip here (Deleted ->
                                // Normal) is a real content change, caught below by
                                // `inventory_content_matches` comparing `.state` too — no
                                // special-casing needed here.
                                let mut item = (*item).clone();
                                item.state = InventoryItemState::Normal;
                                item
                            }
                            // Storing on the unchanged-by-hash path too keeps load
                            // self-healing: a blob that went missing from the object
                            // store comes back on the next re-load. A chunked file's objects
                            // were already stored during ingest (`object` is `None`).
                            //
                            // Staged, not stored immediately (DESIGN.html §5.0 D item 10, finding
                            // #1): see `context.blob_batch`'s and this task's own doc comments for
                            // why staging straight from a concurrent task is safe here.
                            FileVerdict::UnchangedByHash(fresh, object)
                                | FileVerdict::Modified(fresh, object) => {
                                any_entry_rebuilt = true;
                                if let Some(mut object) = object {
                                    object.store_deferred(&context.blob_batch)?;
                                }
                                fresh
                            }
                        }
                    }
                    None => {
                        any_entry_rebuilt = true;
                        build_inventory_item_from_file_deferred(&entry.path(), name.as_str(), item_type, &context.blob_batch)?
                    }
                };

                inventory.add_item(index_item);
            } else {
                let new_task = Box::pin(build_inventory(
                    context.clone(),
                    Arc::new(entry_path),
                    Arc::clone(&paths_to_ignore)
                ));

                context.send_task(new_task)?;
            }
        }

        // Entries of the old inventory whose file is no longer in the directory (deleted,
        // renamed, newly ignored, or replaced by a directory) are carried over as staged
        // removals — this is the "present only in the shard → Deleted" half of the
        // per-directory merge-join. A real content change (a state flip to `Deleted`), caught
        // the same way as any other by `inventory_content_matches` below.
        if let Some((old_inventory, _)) = existing_inventory.as_ref() {
            carry_over_missing_entries_as_deleted(old_inventory, &mut inventory);
        }

        let key = path.as_key().to_string();

        let is_carry = existing_inventory.as_ref()
            .is_some_and(|(old_inventory, _)| inventory_content_matches(&inventory, old_inventory));

        if is_carry {
            // Case (a): every entry hit the pure stat-cache fast path and the entry set is
            // unchanged — literally nothing here differs from what is already on disk (not even
            // stat data). Write nothing at all: the old shard's mtime (and rollup) stay exactly
            // as they were, so the "racily clean" stat-cache guard on this directory's *parent*
            // stays exactly as conservative as it already was.
            if any_entry_rebuilt {
                // Case (b): content unchanged, but at least one entry was re-verified (or stat
                // data drifted). Tentatively carry the rollup this task read at start — the join
                // point drops it if this key is also an ancestor of some other real change.
                let carried_rollup = existing_inventory.as_ref()
                    .and_then(|(old_inventory, _)| old_inventory.get_rollup_hash().cloned());
                inventory.set_rollup_hash(carried_rollup);

                context.outcomes.lock().await.insert(key, ShardOutcome::Carry(inventory, verified_at));
            }
        } else {
            // Case (c): effective content changed (or this is a brand-new shard). Always
            // published with rollup `None`; every ancestor becomes an invalidation target.
            inventory.set_rollup_hash(None);

            context.clear_keys.lock().await.extend(ancestor_keys_root_first(&key));
            context.outcomes.lock().await.insert(key, ShardOutcome::Changed(inventory, verified_at));
        }

        Ok(())
    }
}

/// Build an inventory item for a file whose blob is already stored (its hash is known,
/// e.g. from a tree object): only the file's metadata is gathered, nothing is read or
/// hashed. Used when repopulating the inventory after materializing a tree.
///
/// # Arguments
/// * `path`      - The path of the file.
/// * `name`      - The name of the file.
/// * `hash`      - The (already known) blob or recipe hash of the file's content.
/// * `item_type` - The entry type from the authoritative source (the tree / merge action). It is
///   **not** re-derived from `stat`: a `stat` cannot tell a chunked file from a plain one
///   (chunking is a storage choice), so the tree's `NormalChunked`/`ExecutableChunked` must be
///   carried through here or the next stack would emit a wrong (plain) tree entry over a recipe
///   hash. For a plain file this equals what `stat` reports; for a symlink it is `SymbolicLink`.
///
/// # Returns
/// * `Ok(InventoryItem)` - The inventory item.
/// * `Err(String)`       - If the file's metadata could not be gathered.
pub fn build_inventory_item_from_stat(path: &Path,
                                      name: &str,
                                      hash: String,
                                      item_type: DirEntryType) -> Result<InventoryItem, String> {
    let metadata = file_utils::get_symlink_metadata_for_path(path)?;

    let mtime = file_utils::get_content_modification_timestamp_for_file(&metadata)?;
    let ctime = file_utils::get_metadata_modification_timestamp_for_file(&metadata);

    let file_id = file_utils::get_file_id_for_file(path)?;

    let (device_id, inode) = match file_id {
        FileId::Inode { device_id, inode_number } => Ok((device_id, inode_number)),
        FileId::LowRes { volume_serial_number, file_index } => Ok((volume_serial_number as u64, file_index)),
        FileId::HighRes { .. } => Err("High resolution file IDs are not supported.".to_string()),
    }?;

    let (user_id, group_id) = file_utils::get_owners_for_file(&metadata);

    Ok(
        InventoryItem {
            metadata_change_timestamp: ctime,
            content_change_timestamp: mtime,
            device: device_id,
            inode,
            item_type,
            user_id,
            group_id,
            file_size: metadata.len(),
            hash,
            file_name_length: name.len() as u64,
            state: InventoryItemState::Normal,
            name: String::from(name),
        }
    )
}

/// Stage a fresh inventory entry (with current stat data) for a file whose blob or recipe is
/// already stored (e.g. one just written from a tree or merge).
///
/// # Arguments
/// * `path`      - The warehouse path of the file.
/// * `hash`      - The blob or recipe hash of the file's content.
/// * `item_type` - The authoritative entry type (from the tree / merge action), carried through
///   so a chunked entry keeps its `*Chunked` type in the inventory rather than being demoted to
///   a plain type a `stat` would report.
///
/// # Returns
/// * `Ok(())`      - If the entry was staged.
/// * `Err(String)` - If the file's metadata could not be gathered or the shard written.
pub fn stage_file_entry_from_stat(path: &str, hash: String, item_type: DirEntryType) -> Result<(), String> {
    let (parent_key, name) = match path.rsplit_once(file_utils::PATH_SEPARATOR_CHAR) {
        Some((parent, name)) => (parent, name),
        None => ("", path),
    };

    let entry = build_inventory_item_from_stat(Path::new(path), name, hash, item_type)?;

    update_shard(parent_key, |inventory| {
        inventory.add_item(entry);
        Ok(())
    })
}

/// Whether the directory at `path` may be safely replaced (by a file, or cleared to make
/// way for one) without losing data: it is tracked — represented by its own inventory
/// shard, the sharded-inventory way a directory is recognized (see `stage_removal`) — and
/// every entry beneath it, recursively, is tracked too. Called for a path a merge or shift
/// wants to write a new file to that already exists as a directory on disk (a tracked
/// dir→file flip); the caller still refuses when this returns `false`, exactly as it does
/// for a plain untracked file at the path.
///
/// # Arguments
/// * `path` - The warehouse path of the directory (assumed to exist on disk).
///
/// # Returns
/// * `Ok(true)`    - The directory is tracked and has no untracked content beneath it.
/// * `Ok(false)`   - The directory is untracked, or has untracked content beneath it.
/// * `Err(String)` - If a directory entry or a shard could not be read or parsed.
pub fn directory_is_safe_to_replace(path: &str) -> Result<bool, String> {
    if !file_utils::get_inventory_data_path_for_key(path).exists() {
        return Ok(false);
    }

    let ignored_paths = Arc::new(file_utils::get_ignored_paths()?);

    directory_has_no_untracked_content(path, ignored_paths)
}

/// Recursively check a tracked directory for untracked content (the body of
/// `directory_is_safe_to_replace`). Ignored entries are skipped, matching the rest of the
/// inventory machinery (`walk_directory_unstaged` in `stocktake_utils`): they are invisible
/// to tracking, not a collision.
///
/// # Arguments
/// * `key`           - The warehouse path key of the directory.
/// * `ignored_paths` - The ignore patterns, computed once by the caller and threaded through
///                     the recursion instead of being reloaded and recompiled at every level.
fn directory_has_no_untracked_content(key: &str, ignored_paths: Arc<Vec<Regex>>) -> Result<bool, String> {
    let fs_path = if key.is_empty() { std::path::PathBuf::from(".") } else { std::path::PathBuf::from(key) };

    let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;
    let inventory = match bytes_opt {
        Some(bytes) => parser::inventory::inventory_parser::parse_inventory(&bytes)
            .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?,
        None => Inventory::new(),
    };

    for entry_result in file_utils::read_directory(&fs_path)? {
        let entry = entry_result.map_err(|e| format!("Error while reading directory entry: {}", e))?;
        let name = file_utils::get_name_for_file_or_directory(&entry)?;
        let entry_key = if key.is_empty() { name.clone() } else { format!("{}/{}", key, name) };

        if file_utils::is_path_ignored(&entry_key, &ignored_paths) {
            continue;
        }

        let metadata = file_utils::get_symlink_metadata_for_path(&entry.path())?;
        let item_type = file_utils::get_type_of_dir_entry(&metadata);

        let is_tracked = if item_type.is_file() {
            matches!(
                inventory.get_item_by_name(&name),
                Some(item) if item.state != InventoryItemState::Deleted
            )
        } else {
            file_utils::get_inventory_data_path_for_key(&entry_key).exists()
                && directory_has_no_untracked_content(&entry_key, Arc::clone(&ignored_paths))?
        };

        if !is_tracked {
            return Ok(false);
        }
    }

    Ok(true)
}

/// Refresh every *tracked* entry of the inventory from the working directory: modified
/// files are re-hashed (their blobs stored) and re-staged, files gone from disk become
/// staged removals. Untracked files are deliberately left alone — this is `park`'s way of
/// staging the whole work in progress without swallowing untracked content.
///
/// Decides every shard's new content first, without writing any of it (DESIGN.html §5.0 D item
/// 10, finding #3) — one [`ShardOutcome`] per shard that needs rewriting, the same shape `load`'s
/// parallel walk decides per directory — then publishes through [`publish_shard_outcomes`], the
/// join-point machinery shared with `load`'s own `create_inventory_for_directory`. Sharing that
/// machinery (rather than a bespoke second implementation) is what closes two correctness gaps an
/// earlier version of this batching had:
///
/// * **Finding #1.** A shard decided early in this loop (say, because only its stat data drifted)
///   must not blindly restamp its *decision-time* rollup over an ancestor clear a *later*-decided
///   sibling's real content change requires — `publish_shard_outcomes`'s step 1 drops a carried
///   rollup for any key that ends up in `clear_keys`, computed from every decision in this whole
///   batch, not just the ones already processed when a given shard was decided.
/// * **Finding #6.** Each decision's `verified_at` is captured at that decision's *start* — before
///   it reads, stats or rehashes anything — and carried through to publish as that shard's mtime
///   (`save_inventory_deferred_with_mtime`, inside `publish_shard_outcomes`), never "now" at
///   publish time. `is_entry_unchanged`'s "racily clean" guard trusts a cached entry only when its
///   mtime predates the shard's own mtime; publishing with "now" (the moment every shard in this
///   whole refresh has finished being decided, which can be well after this one was) would let a
///   file edited in that gap satisfy the guard on a stale cached hash and be silently missed on
///   every future load, park or stack.
///
/// Every rebuilt file's blob is staged into its own batch, finished on its own — durable —
/// strictly before `publish_shard_outcomes` stages any shard content (DESIGN.html §5.0 D item 10,
/// finding #7; see `publish_shard_outcomes`'s doc comment for why the two must not share one
/// batch).
///
/// If deciding a shard's new content fails partway through (an unreadable file, a corrupt
/// shard), that failure alone does not cost the whole pass: every shard decided *before* it is
/// still handed to the publish step below, the same keep-whatever-was-decided resilience
/// `create_inventory_for_directory` gives `load` (see its own doc comment). But every rebuilt
/// file's blob this pass stages shares one batch, and if finishing that batch fails — including
/// the leaked-reservation case where a producer won a reservation but never finished staging it —
/// `WriteBatch::finish` refuses to publish the whole batch (see the blob-batch comment below), so
/// nothing this pass decided is published, not just the shards decided after that failure. A
/// retry after a decide-loop failure only has to redo the shards from the failure onward; a retry
/// after a blob-batch failure has to redo the whole tracked set.
///
/// # Returns
/// * `Ok(())`      - If the refresh completed.
/// * `Err(String)` - If a shard or file could not be processed, or the blob batch failed to
///                   publish. Every shard decided before a decide-loop failure was still handed
///                   to the publish step; a blob-batch failure instead publishes nothing from
///                   this pass — see the doc comment above.
pub fn refresh_tracked_entries() -> Result<(), String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    let Some(metadata) = metadata_opt else {
        return Ok(());
    };

    // Pass 1: decide every shard's new content without writing any of it yet — see the function's
    // own doc comment for why this mirrors `load`'s per-directory decisions rather than the old
    // immediate-write loop. Every rebuilt file's blob is staged into one shared batch instead of
    // paying its own atomic-write barrier, finished below before any decision is published.
    let blob_batch = file_utils::WriteBatch::new();
    let mut outcomes: BTreeMap<String, ShardOutcome> = BTreeMap::new();
    let mut clear_keys: BTreeSet<String> = BTreeSet::new();

    // The first failure hit while deciding a shard, if any — recorded instead of propagated
    // immediately (see the function's own doc comment on why: everything decided before this
    // point must still reach the publish step below, exactly as it would have under the old
    // per-shard, immediate-write loop this replaced).
    let mut result: Result<(), String> = Ok(());

    for entry in &metadata {
        let key = metadata_entry_to_key(entry);

        // Captured before this decision reads, stats or rehashes anything below — see the
        // function's own doc comment (finding #6) for why this, and not the moment every shard in
        // this refresh has been decided, is the timestamp this shard's published content carries.
        let verified_at = std::time::SystemTime::now();

        // `Ok(Some((inventory, content_changed)))` when this shard needs rewriting; `Ok(None)`
        // when nothing about it changed at all.
        let decision = (|| -> Result<Option<(Inventory, bool)>, String> {
            let (shard_path, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

            let Some(bytes) = bytes_opt else {
                return Ok(None);
            };

            let shard_mtime = file_utils::get_symlink_metadata_for_path(&shard_path).ok()
                .and_then(|m| file_utils::get_content_modification_timestamp_for_file(&m).ok())
                .unwrap_or(0);

            let mut inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
                .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?;

            let names: Vec<String> = inventory.get_items().map(|(name, _)| name.clone()).collect();
            let mut changed = false;
            // Whether any entry's *effective* content (hash/type/state) actually changed, as
            // opposed to only its stat fields going stale (e.g. a re-save with identical bytes).
            // Only a real content change invalidates the rollup — see `write_shard_mutation`.
            let mut content_changed = false;

            for name in names {
                let item = inventory.get_item_by_name(&name).unwrap();

                if item.state == InventoryItemState::Deleted {
                    continue;
                }

                let file_path = if key.is_empty() {
                    std::path::PathBuf::from(&name)
                } else {
                    std::path::PathBuf::from(format!("{}/{}", key, name))
                };

                let Ok(metadata) = file_utils::get_symlink_metadata_for_path(&file_path) else {
                    // The file is gone from disk: its removal becomes staged.
                    inventory.mark_item_deleted(&name);
                    changed = true;
                    content_changed = true;
                    continue;
                };

                let item_type = file_utils::get_type_of_dir_entry(&metadata);

                if is_entry_unchanged(&item, &metadata, item_type, &file_path, shard_mtime) {
                    continue;
                }

                // The entry is rebuilt even when only the stat data went stale (same
                // content), so the refreshed shard keeps the fast path warm. Staged, not stored
                // immediately — see the batch comment above.
                let rebuilt = build_inventory_item_from_file_deferred(&file_path, &name, item_type, &blob_batch)?;
                changed = true;

                if rebuilt.hash != item.hash || rebuilt.item_type != item.item_type || rebuilt.state != item.state {
                    content_changed = true;
                }

                inventory.add_item(rebuilt);
            }

            Ok(changed.then_some((inventory, content_changed)))
        })();

        match decision {
            Ok(Some((mut inventory, true))) => {
                // A real content change: always published with rollup `None`, and every ancestor
                // becomes an invalidation target — the same `ShardOutcome::Changed` semantics
                // `load`'s walk uses for the same reason (see its own doc comment).
                inventory.set_rollup_hash(None);
                clear_keys.extend(ancestor_keys_root_first(key));
                outcomes.insert(key.to_string(), ShardOutcome::Changed(inventory, verified_at));
            }
            Ok(Some((inventory, false))) => {
                // Only stat data went stale: carry the rollup forward tentatively —
                // `publish_shard_outcomes` drops it if this key also turns out to be an ancestor
                // of some other shard's real change decided elsewhere in this same pass (finding
                // #1 — see the function's own doc comment).
                outcomes.insert(key.to_string(), ShardOutcome::Carry(inventory, verified_at));
            }
            Ok(None) => {}
            // Matches the old loop's own behavior: a shard's failure stops the walk right there
            // (never proceeds to a later shard in metadata order) — only *what happens to
            // already-decided shards* changes here, not which shards get a chance to be decided.
            Err(e) => { result = Err(e); break; }
        }
    }

    // Every rebuilt file's blob decided above becomes durable now, in its own barrier — strictly
    // before `publish_shard_outcomes` below even stages any shard content (DESIGN.html §5.0 D
    // item 10, finding #7): a published shard's entries may carry one of these blobs' hashes, so
    // the blob must already be durable — not merely staged — before that shard's rename can land.
    //
    // A failure here skips `publish_shard_outcomes` entirely, rather than the "keep whatever was
    // decided" resilience the decide loop above gives a per-shard failure. `WriteBatch::finish` can
    // fail more than one way (see its own `# Returns`; `run_write_barrier` for why a rename-partway
    // or trailing-sync failure is not all-or-nothing) — no failure mode leaves a per-hash way to
    // tell which of this pass's blobs actually landed, and some leave a subset durable and the rest
    // not, so publishing any shard content below could durably name a blob that never actually
    // landed — see `create_inventory_for_directory`'s identical phase-0 gating for the same
    // reasoning (and the leaked-reservation blast radius, scoped here to a path *this pass*
    // reserved but a fallible step between reservation and stage failed to finish staging — this
    // function has no concurrent workers) in more detail.
    let blob_batch_published = match blob_batch.finish() {
        Ok(()) => true,
        Err(e) => { if result.is_ok() { result = Err(e); } false }
    };

    // Publish every shard this pass decided to rewrite — see `publish_shard_outcomes`'s doc
    // comment for the exact ordering (and how it closes findings #1 and #6, per this function's
    // own doc comment above).
    if blob_batch_published {
        if let Err(e) = publish_shard_outcomes(&clear_keys, &mut outcomes) {
            if result.is_ok() { result = Err(e); }
        }
    }

    result
}

/// A single read-and-parse pass over every registered inventory shard.
///
/// Built once per `stack` (`prepare_stack_inventory`) and shared by three steps that used to
/// each read+parse the whole shard set independently — `has_conflict_entries`, the tree build
/// and `cleanup_after_stack` (§ perf: on a large tree this was three full O(shard count) passes
/// over the same on-disk state per stacked parcel). See
/// `stack_utils::stack_parcel`: it builds this once, checks conflicts on it (still strictly
/// before any warehouse mutation — parse-then-check-then-write is preserved), threads it into
/// the tree build, and reuses it again for the post-stack cleanup's rewrite decision.
///
/// `park` push (`park::park_changes`, DESIGN.html §5.0 D item 10, finding #3) also builds one now
/// — its own, single-use snapshot (it has no second or third step to share it with, unlike
/// `stack`) — for the same reason: `build_tree_from_inventory_deferred` needs a
/// `PreparedInventory` to read shard content from. `park` checks `has_conflict_entries_in` on its
/// own snapshot immediately after building it, exactly like `stack_parcel` does, before the
/// snapshot's possibly-incomplete `shards` map (see below) is ever consumed by the tree build.
///
/// Held only for the duration of one `stack_parcel` (or `park_changes`) call — the same
/// transient window the tree build's own parallel read pass already held the parsed shards for,
/// so peak memory retention is unchanged (this does not hold anything longer than the code
/// already did; it just stops re-reading and re-parsing the same bytes two more times).
pub struct PreparedInventory {
    /// `None` when there is no inventory metadata file at all (nothing was ever loaded).
    /// `Some` (possibly empty) mirrors the metadata file's own registered directory keys —
    /// kept distinct from an empty set purely so callers can reproduce the exact behavior of
    /// the original functions where "no file" and "empty file" diverge (e.g.
    /// `cleanup_after_stack_with`'s early return when there was never a file to rewrite).
    pub metadata: Option<BTreeSet<String>>,

    /// Every registered directory's parsed inventory, keyed by its warehouse path key. A key
    /// present in `metadata` but absent here means its shard file could not be found on disk
    /// (a stale metadata entry) — every original caller silently skips such an entry, and so
    /// does everything built on this snapshot.
    ///
    /// Incomplete (missing entries for keys past the one that tripped it) when `has_conflict` is
    /// `true` — see [`prepare_stack_inventory`]'s doc comment. Harmless: `stack_parcel` aborts on
    /// a conflict before this is ever consulted again.
    pub shards: BTreeMap<String, Inventory>,

    /// Whether a conflict entry (an unresolved consolidation) was found while parsing. Set — and
    /// the scan stopped — at the *first* shard (in sorted key order) found to have one; see
    /// [`prepare_stack_inventory`]'s doc comment for why.
    pub has_conflict: bool,
}

/// Read the inventory metadata file and parse every registered shard exactly once. See
/// [`PreparedInventory`] for why (`stack` used to pay this read+parse cost three times per
/// parcel).
///
/// The read-and-parse work itself is fanned out across every core (DESIGN.html §5.0 D item 10,
/// finding #5) — [`fanout_utils::fanout_map`], order-independent I/O plus CPU-bound parsing over
/// a flat, already-collected key list. `park` push (`park::park_changes`) is the reason this
/// matters here specifically: before it was switched onto this shared snapshot, its own tree
/// build read every shard from inside its own per-directory `TaskExecutor` task, in parallel; a
/// serial `prepare_stack_inventory` would have been a straight N-core-to-1-core regression on
/// that read phase for a caller (`park`) that has no second or third step to amortize the serial
/// cost against, unlike `stack`.
///
/// What is *not* parallelized: the observable "first conflict, or first parse error, in sorted
/// key order wins" priority the old serial loop gave (and `has_conflict_entries` below still
/// gives, unchanged) — a real, actionable conflict in an earlier shard must never be masked by an
/// unrelated corrupt shard later in the set reporting its parse error instead. That is preserved
/// by processing every shard's result (already computed, off the lock, by the parallel pass
/// above) in one final sorted-order fold that stops at the first anomaly, exactly reproducing the
/// old loop's byte-for-byte reported error and `has_conflict`/`shards` result — the trade is that
/// every shard is now read and parsed even when an early conflict exists (the old loop stopped
/// there), instead of only the ones up to and including it. Conflicts are rare — a mid-merge
/// state, not the common path — so this trade is a clear win on the path this fix exists for.
///
/// # Returns
/// * `Ok(PreparedInventory)` - The snapshot (empty when there is nothing staged; incomplete, with
///                             `has_conflict` set, when a conflict was found and the scan
///                             stopped there).
/// * `Err(String)`           - If the metadata file, or a shard at or before the first conflict in
///                             sorted key order, could not be read or parsed.
pub fn prepare_stack_inventory() -> Result<PreparedInventory, String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    let Some(metadata) = metadata_opt else {
        return Ok(PreparedInventory { metadata: None, shards: BTreeMap::new(), has_conflict: false });
    };

    let keys: Vec<String> = metadata.iter().map(|entry| metadata_entry_to_key(entry).to_string()).collect();

    let parsed: Vec<Result<Option<(Inventory, bool)>, String>> = fanout_utils::fanout_map(&keys, |key| {
        let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

        let Some(bytes) = bytes_opt else { return Ok(None); };

        let inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
            .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?;

        let has_conflict = inventory.get_items().any(|(_, item)| matches!(
            item.state,
            InventoryItemState::FirstParentConflict
                | InventoryItemState::SecondParentConflict
                | InventoryItemState::ThirdParentConflict
        ));

        Ok(Some((inventory, has_conflict)))
    });

    // The deterministic fold: `parsed` is positionally aligned with `keys` (`fanout_map`'s own
    // contract), which is already in sorted key order (`metadata` is a `BTreeSet`) — so walking
    // it in this order and stopping at the first `Err` or conflict reproduces the old serial
    // loop's exact priority, whatever order the parallel workers actually finished in.
    let mut shards: BTreeMap<String, Inventory> = BTreeMap::new();

    for (key, result) in keys.iter().zip(parsed) {
        match result {
            Ok(None) => {}
            Ok(Some((inventory, has_conflict))) => {
                shards.insert(key.clone(), inventory);

                if has_conflict {
                    return Ok(PreparedInventory { metadata: Some(metadata), shards, has_conflict: true });
                }
            }
            Err(e) => return Err(e),
        }
    }

    Ok(PreparedInventory { metadata: Some(metadata), shards, has_conflict: false })
}

/// Check whether any inventory entry is in a conflict state (an unresolved consolidation).
///
/// # Returns
/// * `Ok(bool)`    - Whether at least one entry is in conflict.
/// * `Err(String)` - If a shard could not be read or parsed.
pub fn has_conflict_entries() -> Result<bool, String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    let Some(metadata) = metadata_opt else {
        return Ok(false);
    };

    for entry in &metadata {
        let key = metadata_entry_to_key(entry);
        let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

        let Some(bytes) = bytes_opt else {
            continue;
        };

        let inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
            .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?;

        let has_conflict = inventory.get_items().any(|(_, item)| matches!(
            item.state,
            InventoryItemState::FirstParentConflict
                | InventoryItemState::SecondParentConflict
                | InventoryItemState::ThirdParentConflict
        ));

        if has_conflict {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Like [`has_conflict_entries`], but against an already-parsed [`PreparedInventory`] snapshot
/// instead of reading and parsing the shards again. Used by `stack` (`stack_utils::stack_parcel`),
/// which must still abort before any warehouse mutation when this is true.
///
/// A thin accessor: [`prepare_stack_inventory`] already determines this — and stops scanning as
/// soon as it does, so a real conflict is never masked by a later shard's parse error — so there
/// is nothing left to scan here.
///
/// # Returns
/// * `true`  - At least one entry is in conflict.
/// * `false` - No entry is in conflict.
pub fn has_conflict_entries_in(prepared: &PreparedInventory) -> bool {
    prepared.has_conflict
}

/// List the warehouse paths of every inventory entry in a conflict state (an
/// unresolved consolidation), sorted. The counterpart of [`has_conflict_entries`] for
/// callers that need the paths themselves (the `conflicts` command).
///
/// # Returns
/// * `Ok(Vec<String>)` - The conflicted paths (empty when there are none).
/// * `Err(String)`     - If a shard could not be read or parsed.
pub fn list_conflict_paths() -> Result<Vec<String>, String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    let Some(metadata) = metadata_opt else {
        return Ok(Vec::new());
    };

    let mut paths = Vec::new();

    for entry in &metadata {
        let key = metadata_entry_to_key(entry);
        let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

        let Some(bytes) = bytes_opt else {
            continue;
        };

        let inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
            .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?;

        for (name, item) in inventory.get_items() {
            let is_conflict = matches!(
                item.state,
                InventoryItemState::FirstParentConflict
                    | InventoryItemState::SecondParentConflict
                    | InventoryItemState::ThirdParentConflict
            );

            if is_conflict {
                paths.push(if key.is_empty() { name.clone() } else { format!("{}/{}", key, name) });
            }
        }
    }

    paths.sort();

    Ok(paths)
}

/// Build a "stale" inventory item: the hash and type are known (e.g. from a head tree),
/// but the stat fields are zeroed on purpose, so the stat cache can never trust the entry
/// — the next comparison against the working directory always rehashes the file. Used by
/// `restore --staged`, where the file on disk may not match the recorded hash.
///
/// # Arguments
/// * `name`      - The name of the file.
/// * `hash`      - The blob hash of the entry's content.
/// * `item_type` - The type of the entry.
///
/// # Returns
/// * `InventoryItem` - The stale inventory item.
pub fn build_stale_inventory_item(name: &str, hash: String, item_type: DirEntryType) -> InventoryItem {
    InventoryItem {
        metadata_change_timestamp: 0,
        content_change_timestamp: 0,
        device: 0,
        inode: 0,
        item_type,
        user_id: 0,
        group_id: 0,
        file_size: 0,
        hash,
        file_name_length: name.len() as u64,
        state: InventoryItemState::Normal,
        name: String::from(name),
    }
}

/// Load the inventory shard for the given key (or an empty one), apply the given change,
/// and save it back.
///
/// # Arguments
/// * `key`    - The warehouse path key of the directory.
/// * `change` - The change to apply to the inventory.
///
/// # Returns
/// * `Ok(())`      - If the shard was updated.
/// * `Err(String)` - If the shard could not be read, parsed or written.
pub fn update_shard(key: &str,
                    change: impl FnOnce(&mut Inventory) -> Result<(), String>) -> Result<(), String> {
    let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

    let mut inventory = match bytes_opt {
        Some(bytes) => parser::inventory::inventory_parser::parse_inventory(&bytes)
            .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?,
        None => Inventory::new(),
    };

    change(&mut inventory)?;

    write_shard_mutation(key, &mut inventory)?;

    let mut new_keys: BTreeSet<String> = BTreeSet::new();
    new_keys.insert(key.to_string());

    update_inventory_metadata(&new_keys, &BTreeSet::new())
}

/// Replace the staging area below the given directory with the given shards: the existing
/// inventory folders under the key are removed, the given shards are written, and the
/// metadata file is updated accordingly. Used by `restore --staged` to reset a subtree of
/// the inventory to the pallet head.
///
/// # Arguments
/// * `key`    - The warehouse path key of the subtree to replace (`""` for everything).
/// * `shards` - Warehouse path key → inventory for the new state of the subtree.
///
/// # Returns
/// * `Ok(())`      - If the subtree was replaced.
/// * `Err(String)` - If a folder or file operation failed.
pub fn replace_subtree_inventories(key: &str,
                                   shards: &std::collections::BTreeMap<String, Inventory>) -> Result<(), String> {
    // The subtree at `key` is about to be replaced wholesale — a content change from any
    // ancestor's point of view — so their rollups (if any) must be cleared before the new
    // content lands, exactly as `write_shard_mutation` orders it. `shards` itself carries
    // correctly-stamped rollups already (the caller builds them from a known tree), so nothing
    // further is needed for `key` and below.
    clear_ancestor_rollups(key)?;

    let folder = file_utils::get_inventory_folder_for_key(key);

    if folder.exists() {
        std::fs::remove_dir_all(&folder).map_err(|e|
            format!("Error while clearing the staging area of \"{}\": {}", key, e)
        )?;
    }

    let (metadata_path, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;
    let mut metadata = metadata_opt.unwrap_or_default();

    if key.is_empty() {
        metadata.clear();
    } else {
        let prefix = format!("{}/", key);
        metadata.retain(|entry| {
            let entry_key = metadata_entry_to_key(entry);
            entry_key != key && !entry_key.starts_with(&prefix)
        });
    }

    // One durability barrier for the whole replacement burst instead of one per shard
    // (DESIGN.html §5.0 D item 8, stage 2b) — every shard here carries a caller-computed,
    // already-correct rollup (or none), so these writes have no ordering requirement against
    // each other, only against the ancestor clear above (already durable by the time this runs).
    let batch = file_utils::WriteBatch::new();

    for (shard_key, inventory) in shards {
        save_inventory_deferred(inventory, &file_utils::get_inventory_data_path_for_key(shard_key), &batch)?;
        metadata.insert(key_to_metadata_entry(shard_key));
    }

    batch.finish()?;

    write_metadata_to_file(&metadata_path, &metadata)
}

/// Replace the whole staging area with the given shards: the existing inventory folders
/// are removed, the given shards are written, and the metadata file is rewritten to list
/// exactly their directories. Used when `shift` repopulates the inventory from the target
/// pallet's tree.
///
/// # Arguments
/// * `shards` - Warehouse path key → inventory, for every tracked directory.
///
/// # Returns
/// * `Ok(())`      - If the staging area was replaced.
/// * `Err(String)` - If a folder or file operation failed.
pub fn replace_all_inventories(shards: &std::collections::BTreeMap<String, Inventory>) -> Result<(), String> {
    let root_folder = file_utils::get_inventory_folder_for_key("");

    if root_folder.exists() {
        std::fs::remove_dir_all(&root_folder).map_err(|e|
            format!("Error while clearing the staging area: {}", e)
        )?;
    }

    let mut metadata: BTreeSet<String> = BTreeSet::new();

    // One durability barrier for the whole warehouse instead of one per shard (DESIGN.html
    // §5.0 D item 8, stage 2b) — the whole staging area was just wiped above, so there is no
    // ancestor ordering to preserve here at all, only the fresh content itself.
    let batch = file_utils::WriteBatch::new();

    for (key, inventory) in shards {
        save_inventory_deferred(inventory, &file_utils::get_inventory_data_path_for_key(key), &batch)?;
        metadata.insert(key_to_metadata_entry(key));
    }

    batch.finish()?;

    let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none()?;

    write_metadata_to_file(&metadata_path, &metadata)
}

/// Clean up the staged state after a parcel was stacked: the parcel consumed every staged
/// removal, so `Deleted` entries are dropped from their shards, and the shards of
/// directories that no longer exist in the working directory are removed entirely
/// (together with their metadata entries).
///
/// Reuses an already-parsed [`PreparedInventory`] snapshot — the same one `stack` used for its
/// conflict check and tree build — instead of reading and parsing every shard a third time (§
/// perf; see [`PreparedInventory`]). The snapshot was taken before `stack` wrote anything, but
/// nothing between then and this call mutates a shard's *content* on disk (tree/parcel objects,
/// the pallet ref and the signature sidecar are the only writes stacking does before this point,
/// none of them touch an inventory shard), so reusing it here produces exactly the same result
/// as a fresh read would. The directory-existence checks below are unaffected — they always
/// read the working directory fresh.
///
/// After a stack, staged state *is* the new head — so every surviving shard's rollup ends up in
/// one of three states:
///  * **Stamp** — `key` is in `stamp_hashes`: rewrite it with the just-built subtree hash from
///    the same tree build `stack` just ran.
///  * **Untouched** — `key` is in `untouched_keys` (a rollup-skipped subtree `stack`'s tree
///    build never even read): its on-disk rollup already names the right tree (that is *why* it
///    was skipped) and its staged state provably did not change this stack, so it is left
///    exactly as it sits — no read beyond the [`PreparedInventory`] snapshot already parsed
///    up front, no write.
///  * **Clear** — everything else (a key in neither set: an out-of-scope/spine shard in a
///    scoped bay, or a genuinely empty subtree): stamped `None` instead of guessed, exactly as
///    before stage 2 existed.
///
/// A key that is in `untouched_keys` but unexpectedly still carries a staged removal (the
/// invariant `write_shard_mutation` maintains — a pending `Deleted` entry always clears its own
/// rollup, so a shard with one still staged could never have had a rollup for the skip to have
/// matched on in the first place — is violated) falls through to the normal stamp-or-clear
/// handling below instead of being silently trusted.
///
/// # Arguments
/// * `prepared`       - The snapshot from [`prepare_stack_inventory`].
/// * `stamp_hashes`   - Warehouse path key → the subtree hash `stack` just built there. Trusted
///                      for every key it names (see `stack_utils::stack_parcel`, which omits any
///                      key a scoped bay's spine splice could have changed).
/// * `untouched_keys` - Warehouse path keys `stack`'s tree build proved unchanged and skipped
///                      entirely (a rollup-skipped subtree's root and every descendant).
///
/// # Returns
/// * `Ok(())`      - If the cleanup completed.
/// * `Err(String)` - If a shard could not be written, or a folder could not be removed.
pub fn cleanup_after_stack_with(prepared: &PreparedInventory,
                                stamp_hashes: &BTreeMap<String, String>,
                                untouched_keys: &BTreeSet<String>) -> Result<(), String> {
    let Some(metadata) = &prepared.metadata else {
        return Ok(());
    };

    let mut removed_keys: BTreeSet<String> = BTreeSet::new();

    // Every shard rewritten below (a stamp or a `Deleted`-entry drop) is staged here and
    // published as one durability barrier at the end, instead of once per shard (DESIGN.html
    // §5.0 D item 8, stage 2b) — these rewrites carry no ordering requirement against each
    // other: a stamp is only ever consulted after `stack`'s ref move is already durable (see
    // `stack_utils::stack_parcel`), so losing some of them to a crash mid-barrier only costs a
    // few lost skips next time, never a wrong one.
    let batch = file_utils::WriteBatch::new();

    for entry in metadata {
        let key = metadata_entry_to_key(entry);

        let dir_path = if key.is_empty() {
            Path::new(".").to_path_buf()
        } else {
            std::path::PathBuf::from(key)
        };

        if !dir_path.is_dir() {
            // The directory is gone from the working tree, and the parcel that was just
            // stacked recorded its removal; its shard has served its purpose.
            let folder = file_utils::get_inventory_folder_for_key(key);

            // A parent directory earlier in the (sorted) set may have removed this folder.
            if folder.exists() {
                std::fs::remove_dir_all(&folder).map_err(|e|
                    format!("Error while removing the inventory of folder \"{}\": {}", key, e)
                )?;
            }

            removed_keys.insert(key.to_string());
            continue;
        }

        let Some(inventory) = prepared.shards.get(key) else {
            continue;
        };

        let has_staged_removals = inventory.get_items()
            .any(|(_, item)| item.state == InventoryItemState::Deleted);

        if untouched_keys.contains(key) && !has_staged_removals {
            continue;
        }

        let target_rollup = stamp_hashes.get(key).cloned();

        // A rewrite is needed either to drop the now-consumed `Deleted` entries (unchanged
        // behavior — dropping them never changes the tree `stack` already built, since the tree
        // build itself excludes `Deleted` entries) or, even with nothing to drop, purely to
        // stamp a rollup this shard did not already carry.
        if has_staged_removals || inventory.get_rollup_hash() != target_rollup.as_ref() {
            let mut rebuilt = Inventory::new();

            for (_, item) in inventory.get_items() {
                if item.state != InventoryItemState::Deleted {
                    rebuilt.add_item((**item).clone());
                }
            }

            rebuilt.set_rollup_hash(target_rollup);

            let shard_path = file_utils::get_inventory_data_path_for_key(key);

            // Neither case rewritten here re-verifies a single entry against the file system —
            // the stamp comes from `stack`'s own just-finished tree build, and a Deleted-drop
            // just copies `prepared`'s already-parsed entries — so this write is exactly the
            // "rewrite that verifies nothing" hazard `load`'s join point was fixed against (see
            // `ShardOutcome`'s doc comment): publishing it with "now" would let a file edited
            // between this shard's last real verification and this cleanup satisfy
            // `is_entry_unchanged`'s `mtime < shard_mtime` on a now-stale cached hash, forever.
            // The shard's own current mtime is captured and carried through unchanged instead —
            // exactly like `stage_rollup_clear`'s rollup-only rewrite, and for the same reason.
            let original_mtime = file_utils::get_symlink_metadata_for_path(&shard_path).ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

            save_inventory_deferred_with_mtime(&rebuilt, &shard_path, &batch, original_mtime)?;
        }
    }

    batch.finish()?;

    update_inventory_metadata(&BTreeSet::new(), &removed_keys)
}

/// Remove every inventory shard at or under a warehouse path prefix, dropping those keys from
/// the metadata too. Unlike [`stage_removal_for_directory`], this leaves no `Deleted` record —
/// the entries vanish, as if the subtree had never been inventoried. Used by `narrow` when a
/// subtree leaves the checkout's materialization scope: it should stop being reported entirely,
/// not appear as a staged removal to be committed.
///
/// # Arguments
/// * `prefix` - The warehouse path key of the subtree leaving scope (never the root).
///
/// # Returns
/// * `Ok(())`      - If the shards under the prefix were removed.
/// * `Err(String)` - If a shard folder or the metadata could not be updated.
pub fn remove_inventories_under(prefix: &str) -> Result<(), String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    let Some(metadata) = metadata_opt else {
        return Ok(());
    };

    // Conservative: the removed shards themselves have no content left to invalidate (their
    // files are about to be deleted below), but every ancestor above `prefix` may have had a
    // rollup describing a subtree that included them — clear those before anything is removed.
    clear_ancestor_rollups(prefix)?;

    let mut removed_keys: BTreeSet<String> = BTreeSet::new();

    for entry in &metadata {
        let key = metadata_entry_to_key(entry);

        // The prefix directory itself, or a directory strictly under it.
        let under = key == prefix
            || (key.len() > prefix.len()
                && key.as_bytes()[prefix.len()] == b'/'
                && key.starts_with(prefix));

        if !under {
            continue;
        }

        let folder = file_utils::get_inventory_folder_for_key(key);

        // A parent shard folder earlier in the (sorted) set may have removed this one already.
        if folder.exists() {
            std::fs::remove_dir_all(&folder).map_err(|e|
                format!("Error while removing the inventory of folder \"{}\": {}", key, e)
            )?;
        }

        removed_keys.insert(key.to_string());
    }

    update_inventory_metadata(&BTreeSet::new(), &removed_keys)
}

/// Carry over the entries of the previous inventory that were not re-added by the directory
/// walk (their file was deleted, renamed, newly ignored, or replaced by a directory),
/// marking them as staged removals.
///
/// # Arguments
/// * `old_inventory` - The inventory of the previous load.
/// * `new_inventory` - The inventory being rebuilt from the working directory.
fn carry_over_missing_entries_as_deleted(old_inventory: &Inventory, new_inventory: &mut Inventory) {
    let missing_items: Vec<InventoryItem> = old_inventory.get_items()
        .filter(|(name, _)| new_inventory.get_item_by_name(name).is_none())
        .map(|(_, item)| (**item).clone())
        .collect();

    for mut item in missing_items {
        item.state = InventoryItemState::Deleted;
        new_inventory.add_item(item);
    }
}

/// Update the inventory metadata file (a text file that contains the paths of all
/// inventoried directories, sorted alphabetically) in a single write.
///
/// # Arguments
/// * `keys_to_add`    - Warehouse path keys of directories to register.
/// * `keys_to_remove` - Warehouse path keys of directories to remove.
///
/// # Returns
/// * `Ok(())`      - If the metadata was successfully updated.
/// * `Err(String)` - If an error occurred while updating the metadata.
fn update_inventory_metadata(keys_to_add: &BTreeSet<String>,
                             keys_to_remove: &BTreeSet<String>) -> Result<(), String> {
    let (metadata_path, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;
    let mut metadata = metadata_opt.unwrap_or(BTreeSet::new());

    for key in keys_to_add {
        metadata.insert(key_to_metadata_entry(key));
    }

    for key in keys_to_remove {
        metadata.remove(&key_to_metadata_entry(key));
    }

    write_metadata_to_file(&metadata_path, &metadata)
}

/// Save the inventory metadata to file.
///
/// # Arguments
/// * `path`     - The path of the file where the metadata should be saved.
/// * `metadata` - Inventory metadata. A `BTreeSet` consisting of paths of directories where
/// inventories are stored.
///
/// # Returns
/// * `Ok(())`      - If the metadata file was saved successfully.
/// * `Err(String)` - If there was an error while saving the metadata file.
fn write_metadata_to_file(path: &Path, metadata: &BTreeSet<String>) -> Result<(), String> {
    let mut metadata_bytes: Vec<u8> = Vec::new();

    for inv_path in metadata {
        metadata_bytes.extend(inv_path.as_bytes());
        object_utils::push_new_line(&mut metadata_bytes);
    }

    std::fs::write(path, metadata_bytes).map_err(|e|
        format!("Error while writing inventory metadata to file \"{}\": {}", path.to_string_lossy(), e)
    )
}

/// Check whether a file is unchanged compared to its existing inventory entry, based purely
/// on file metadata (no file content is read). This is the stat-cache fast path: matching
/// ctime, mtime, device, inode, type (and size, for non-symlinks) means the stored hash is
/// still valid, exactly like git's index stat cache.
///
/// Any error while gathering metadata simply reports "changed", falling back to the full
/// read-and-hash path.
///
/// Note that editors which save via write-new-then-rename replace the inode on every save;
/// an inode mismatch therefore just means "changed" (rehash), never "different file".
///
/// "Racily clean" protection: timestamps have second granularity, so a file modified in the
/// same second the shard was written could keep identical mtime/ctime/size and slip past the
/// stat check. Entries are therefore only trusted when their mtime is *strictly older* than
/// the shard itself — anything as new as the shard (or newer, e.g. clock skew) is rehashed.
///
/// # Arguments
/// * `existing`    - The inventory entry from the previous load.
/// * `metadata`    - The current (symlink) metadata of the file.
/// * `item_type`   - The current type of the directory entry.
/// * `path`        - The path of the file.
/// * `shard_mtime` - The modification timestamp of the inventory shard the entry came from.
///
/// # Returns
/// * `true`  - If the file is unchanged and the existing entry can be reused.
/// * `false` - If the file changed (or freshness could not be determined).
pub fn is_entry_unchanged(existing: &InventoryItem,
                      metadata: &std::fs::Metadata,
                      item_type: DirEntryType,
                      path: &Path,
                      shard_mtime: u64) -> bool {
    // Compare the on-disk kind, not the chunked storage decision: `item_type` is derived from a
    // fresh `stat` and can only ever be `Normal`/`Executable`/`SymbolicLink`, while the inventory
    // entry may hold a `*Chunked` variant for the same file. Chunking is a storage choice a stat
    // cannot see, so an unchanged giant (stat says `Normal`, inventory says `NormalChunked`) must
    // still hit this fast path and never be re-chunked. A genuine normal↔executable flip is still
    // caught (their on-disk kinds differ).
    if existing.item_type.on_disk_kind() != item_type.on_disk_kind() {
        return false;
    }

    let Ok(mtime) = file_utils::get_content_modification_timestamp_for_file(metadata) else {
        return false;
    };
    let ctime = file_utils::get_metadata_modification_timestamp_for_file(metadata);

    let Ok(file_id) = file_utils::get_file_id_for_file(path) else {
        return false;
    };
    let (device, inode) = match file_id {
        FileId::Inode { device_id, inode_number } => (device_id, inode_number),
        FileId::LowRes { volume_serial_number, file_index } => (volume_serial_number as u64, file_index),
        FileId::HighRes { .. } => return false,
    };

    // For symlinks the stored size is the length of the target path, which is not comparable
    // to the metadata size on every platform; the other fields are sufficient for them.
    let size_matches = item_type == DirEntryType::SymbolicLink
        || existing.file_size == metadata.len();

    mtime < shard_mtime
        && existing.content_change_timestamp == mtime
        && existing.metadata_change_timestamp == ctime
        && existing.device == device
        && existing.inode == inode
        && size_matches
}

/// Create an inventory item for a file.
/// If the given file does not exist the object store, a new blob is created and stored.
///
/// # Arguments
/// * `path`      - The path of the file.
/// * `name`      - The name of the file.
/// * `item_type` - The type of the directory entry.
///
/// # Returns
/// * `Ok(InventoryItem)` - The inventory item for the file.
/// * `Err(String)`       - The error message if the inventory item could not be created.
pub fn build_inventory_item_from_file(path: &Path,
                                      name: &str,
                                      item_type: DirEntryType) -> Result<InventoryItem, String> {
    // A first-time `load` of a file: persist its objects. A small file returns its blob for us to
    // store; a chunked file already stored its chunks and recipe during ingest (`None`).
    let (item, object) = build_item_and_object_for_file(path, name, item_type, IngestMode::Store)?;

    if let Some(mut object) = object {
        object.store()?;
    }

    Ok(item)
}

/// Like [`build_inventory_item_from_file`], but stages the file's blob into `batch` (see
/// [`file_utils::WriteBatch`]) instead of writing and fsyncing it immediately (DESIGN.html §5.0 D
/// item 10, finding #1). Used by every caller that rebuilds several files in one operation
/// (`load`'s parallel walk, `park`'s working-directory refresh), so what used to be one
/// atomic-write barrier per file collapses into the caller's own shared batch.
///
/// # Arguments
/// * `path`      - The path of the file.
/// * `name`      - The name of the file.
/// * `item_type` - The type of the directory entry.
/// * `batch`     - Where the file's blob (if any) is staged.
///
/// # Returns
/// * `Ok(InventoryItem)` - The inventory item for the file. Its blob, if any, is staged but not
///                         yet durable — the caller must call `batch.finish()` (and it must
///                         return `Ok`) before anything may depend on it.
/// * `Err(String)`       - The error message if the inventory item could not be created or its
///                         blob could not be staged.
pub fn build_inventory_item_from_file_deferred(path: &Path,
                                               name: &str,
                                               item_type: DirEntryType,
                                               batch: &file_utils::WriteBatch) -> Result<InventoryItem, String> {
    let (item, object) = build_item_and_object_for_file(path, name, item_type, IngestMode::Store)?;

    if let Some(mut object) = object {
        object.store_deferred(batch)?;
    }

    Ok(item)
}

/// Create an inventory item for a file, together with the built-but-unstored blob object for a
/// small file. A file at or above the chunk threshold is ingested as a recipe plus chunks
/// instead (its entry type becomes a `*Chunked` variant, its hash is the recipe hash) — those
/// objects are handled per `mode`, so the returned object is `None` for a chunked file.
///
/// The read-only stocktake/diff caller passes `IngestMode::ComputeOnly` (nothing is written to
/// the store, not even a chunked giant's chunks); `load`/`park` pass `IngestMode::Store`. Either
/// way the returned blob (small files only) is unstored — writers store it, read-only callers
/// drop it, exactly as before.
///
/// # Arguments
/// * `path`      - The path of the file.
/// * `name`      - The name of the file.
/// * `item_type` - The type of the directory entry (a `*Chunked` upgrade is decided here).
/// * `mode`      - Whether to persist a chunked file's objects or only compute their hashes.
///
/// # Returns
/// * `Ok((InventoryItem, Option<LooseObject>))` - The item, and the unstored blob for a small
///   file (`None` for a chunked file, whose objects were handled per `mode`).
/// * `Err(String)`                              - If the file could not be read or stat'ed.
fn build_item_and_object_for_file(path: &Path,
                                  name: &str,
                                  item_type: DirEntryType,
                                  mode: IngestMode)
                                  -> Result<(InventoryItem, Option<LooseObject>), String> {
    let metadata = file_utils::get_symlink_metadata_for_path(path)?;

    let mtime = file_utils::get_content_modification_timestamp_for_file(&metadata)?;
    let ctime = file_utils::get_metadata_modification_timestamp_for_file(&metadata);

    let file_id = file_utils::get_file_id_for_file(path)?;

    let (device_id, inode) = match file_id {
        FileId::Inode { device_id, inode_number } => Ok((device_id, inode_number)),
        FileId::LowRes { volume_serial_number, file_index } => Ok((volume_serial_number as u64, file_index)),
        FileId::HighRes { .. } => Err("High resolution file IDs are not supported.".to_string()),
    }?;

    let (user_id, group_id) = file_utils::get_owners_for_file(&metadata);
    let ingested = object_utils::ingest_file(name, path, item_type, mode)?;

    let item = InventoryItem {
        metadata_change_timestamp: ctime,
        content_change_timestamp: mtime,
        device: device_id,
        inode,
        item_type: ingested.item_type,
        user_id,
        group_id,
        file_size: ingested.file_size,
        hash: ingested.hash,
        file_name_length: name.len() as u64,
        state: InventoryItemState::Normal,
        name: String::from(name),
    };

    Ok((item, ingested.deferred))
}

/// The verdict of classifying one on-disk file against its existing inventory entry.
/// This is the shared per-file core of the per-directory merge-join (§3.2.1): `load` and
/// the unstaged stocktake walk both classify with it, so their verdicts can never drift
/// apart. The verdict carries facts, not policy — what an untracked file or a staged
/// removal *means* stays with the caller.
pub enum FileVerdict {
    /// The stat cache proves the entry still matches the file — nothing was read or hashed.
    UnchangedByStat,

    /// The stat cache missed, but the content hash matches the entry: the file is
    /// unchanged. Carries the rebuilt item (same hash, fresh stat data) and, for a small
    /// file, the unstored blob object — writers store it anyway (a cheap no-op when present),
    /// which is what makes a re-load heal a blob that went missing from the object store. A
    /// chunked file carries `None` (its chunks/recipe were handled per the ingest mode).
    UnchangedByHash(InventoryItem, Option<LooseObject>),

    /// The content changed. Carries the rebuilt item (new hash, fresh stat data) and, for a
    /// small file, the unstored blob object, so a writer can store it without reading the file
    /// again — and a read-only caller simply drops it. A chunked file carries `None`.
    Modified(InventoryItem, Option<LooseObject>),
}

/// Classify one on-disk file against its existing inventory entry: the stat-cache fast
/// path first (see `is_entry_unchanged`, including the racily-clean protection), then a
/// read-and-hash comparison. Nothing is written to the object store or the inventory.
///
/// # Arguments
/// * `existing`    - The inventory entry the file is compared against.
/// * `metadata`    - The current (symlink) metadata of the file.
/// * `item_type`   - The current type of the directory entry (the filesystem-visible kind).
/// * `path`        - The path of the file.
/// * `name`        - The name of the file.
/// * `shard_mtime` - The modification timestamp of the shard the entry came from.
/// * `mode`        - Whether a re-chunked giant's objects are stored (`load`) or only hashed
///   (read-only stocktake/diff).
///
/// # Returns
/// * `Ok(FileVerdict)` - The verdict.
/// * `Err(String)`     - If the file could not be read or stat'ed.
pub fn classify_file_against_entry(existing: &InventoryItem,
                                   metadata: &std::fs::Metadata,
                                   item_type: DirEntryType,
                                   path: &Path,
                                   name: &str,
                                   shard_mtime: u64,
                                   mode: IngestMode) -> Result<FileVerdict, String> {
    if is_entry_unchanged(existing, metadata, item_type, path, shard_mtime) {
        return Ok(FileVerdict::UnchangedByStat);
    }

    let (item, object) = build_item_and_object_for_file(path, name, item_type, mode)?;

    if item.hash == existing.hash {
        Ok(FileVerdict::UnchangedByHash(item, object))
    } else {
        Ok(FileVerdict::Modified(item, object))
    }
}

/// Add a single file to its corresponding inventory file.
/// If the file is already in the inventory, its entry is updated.
///
/// # Arguments
/// * `path` - The path of the file.
///
/// # Returns
/// * `Ok(())`      - If the file was successfully added to the inventory.
/// * `Err(String)` - If there was an error during the operation.
fn add_file_to_inventory(path: &WarehousePath) -> Result<(), String> {
    let (parent, file_name) = path.split_parent()?;

    let (_, mut inventory) = retrieve_inventory_or_empty(&parent)?;

    let fs_path = path.to_fs_path();
    let file_metadata = file_utils::get_symlink_metadata_for_path(&fs_path)?;

    let item = build_inventory_item_from_file(
        &fs_path,
        &file_name,
        file_utils::get_type_of_dir_entry(&file_metadata)
    )?;

    inventory.add_item(item);

    write_shard_mutation(parent.as_key(), &mut inventory)?;

    let mut new_items: BTreeSet<String> = BTreeSet::new();
    new_items.insert(parent.as_key().to_string());

    update_inventory_metadata(&new_items, &BTreeSet::new())?;

    Ok(())
}

/// Stage the removal of a single file: mark its entry in its parent's inventory as `Deleted`.
/// Staging the removal of a file that is already staged for removal is a no-op that
/// still succeeds.
///
/// # Arguments
/// * `path` - The path of the file whose removal should be staged.
///
/// # Returns
/// * `Ok(())`      - If the removal was staged successfully.
/// * `Err(String)` - If the file is not in the inventory, or there was an error.
fn stage_removal_for_file(path: &WarehousePath) -> Result<(), String> {
    let (parent, file_name) = path.split_parent()?;

    let (_, inventory_bytes) = file_utils::retrieve_inventory_or_none_by_key(parent.as_key())?;
    let mut inventory = match inventory_bytes {
        Some(bytes) => parser::inventory::inventory_parser::parse_inventory(&bytes)?,
        None => return Err(format!("\"{}\" is not in the inventory.", path.as_key())),
    };

    if !inventory.mark_item_deleted(&file_name) {
        return Err(format!("\"{}\" is not in the inventory.", path.as_key()));
    }

    write_shard_mutation(parent.as_key(), &mut inventory)?;

    Ok(())
}

/// Retrieve the associated inventory for the given directory
/// (or an empty inventory, if it does not have one yet).
///
/// # Arguments
/// * `parent` - The warehouse path of the directory.
///
/// # Returns
/// * `Ok((PathBuf, Inventory))`:
///    * `PathBuf`   - The path to the inventory file (if the inventory file was not found, this is
///                    the path where it should have been).
///    * `Inventory` - The inventory found (or an empty inventory otherwise).
/// * `Err(String)` - If there was an error.
fn retrieve_inventory_or_empty(parent: &WarehousePath) -> Result<(std::path::PathBuf, Inventory), String> {
    let (inventory_path, inventory_bytes) = file_utils::retrieve_inventory_or_none_by_key(parent.as_key())?;

    let inventory = match inventory_bytes {
        Some(bytes) => parser::inventory::inventory_parser::parse_inventory(&bytes)?,
        None => Inventory::new(),
    };

    Ok((inventory_path, inventory))
}

/// Save the given inventory to the given path.
///
/// # Arguments
/// * `inventory`      - The inventory data that should be written to the file.
/// * `inventory_path` - The file path of the inventory file (including file name).
///
/// # Returns
/// * `Ok(())`      - If the inventory was saved successfully.
/// * `Err(String)` - If there was an error.
fn save_inventory(inventory: &Inventory, inventory_path: &Path) -> Result<(), String> {
    let bytes = ensure_inventory_folder_and_build(inventory, inventory_path)?;

    // Atomic (temp file, fsync, rename, directory fsync) — the store-wide "durable before
    // destructive" contract. This matters far more now than it used to: post-stack rollup
    // stamping (`cleanup_after_stack_with`) can rewrite every registered shard on a single
    // `stack`, not just the ones with consumed staged removals, so a shard write is on the hot
    // path of a crash-safety-sensitive operation far more often than before. A caller writing
    // more than one shard for the same logical operation should prefer
    // [`save_inventory_deferred`] instead — see its doc comment for why a burst of these,
    // fsynced and renamed one at a time, is the dominant cost stage 2b's benchmark caught.
    file_utils::write_file_atomically(inventory_path, &bytes)
}

/// Stage a shard write into `batch` instead of writing (and fsyncing) it immediately — the
/// deferred counterpart of [`save_inventory`], for a caller writing several shards as one
/// logical operation (`cleanup_after_stack_with`'s post-stack stamping, a materializer's
/// wholesale shard replacement, the mutation funnel's ancestor-clear set). Batches the fsync and
/// rename of every staged shard into one durability barrier — see [`file_utils::WriteBatch`] —
/// instead of paying a full write-fsync-rename-directory-fsync cycle per shard: on a synthetic
/// 300-directory corpus this was the entire gap between stage 1 (per-shard atomic writes) and
/// pre-stage-1 `stack` timings (DESIGN.html §5.0 D item 8, stage 2b measurement).
///
/// The caller must call [`file_utils::WriteBatch::finish`] once every shard for this operation
/// has been staged — nothing staged here is visible or durable before that returns `Ok`.
fn save_inventory_deferred(inventory: &Inventory,
                           inventory_path: &Path,
                           batch: &file_utils::WriteBatch) -> Result<(), String> {
    let bytes = ensure_inventory_folder_and_build(inventory, inventory_path)?;

    batch.stage(inventory_path, &bytes)
}

/// Like [`save_inventory_deferred`], but publishes with an explicit mtime instead of "now" — see
/// [`file_utils::WriteBatch::stage_with_mtime`]. Every caller uses this for the same reason: the
/// rewrite being published does not itself verify anything against the file system, so it must
/// not be allowed to advance a shard's mtime — doing so would overstate `is_entry_unchanged`'s
/// proof that this shard's entries were checked against the file system no earlier than that
/// moment, and a file edited between the shard's real last verification and this publish would
/// then satisfy `mtime < shard_mtime` on a now-stale cached hash and be silently missed forever.
///
/// * `load`'s join point (`create_inventory_for_directory`) publishes each [`ShardOutcome`] with
///   the timestamp captured when it was actually verified, not the (potentially much later)
///   moment the whole walk finishes and publishes.
/// * `cleanup_after_stack_with`'s post-stack rewrite (a rollup stamp, or a `Deleted`-entry drop)
///   publishes with the shard's own pre-rewrite mtime — neither rewrite re-verifies a single
///   entry, so "now" would be just as wrong there.
fn save_inventory_deferred_with_mtime(inventory: &Inventory,
                                      inventory_path: &Path,
                                      batch: &file_utils::WriteBatch,
                                      mtime: std::time::SystemTime) -> Result<(), String> {
    let bytes = ensure_inventory_folder_and_build(inventory, inventory_path)?;

    batch.stage_with_mtime(inventory_path, &bytes, mtime)
}

/// The shared prelude of [`save_inventory`] and [`save_inventory_deferred`]: make sure the
/// shard's parent folder exists, and serialize its bytes.
fn ensure_inventory_folder_and_build(inventory: &Inventory, inventory_path: &Path) -> Result<Vec<u8>, String> {
    let mut parent_path = std::path::PathBuf::from(inventory_path);
    parent_path.pop();

    file_utils::create_folder_if_not_exists(&parent_path)?;

    Ok(InventoryBuilder::build(inventory))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, inode: u64, state: InventoryItemState) -> InventoryItem {
        InventoryItem {
            metadata_change_timestamp: 0,
            content_change_timestamp: 0,
            device: 1,
            inode,
            item_type: DirEntryType::Normal,
            user_id: 0,
            group_id: 0,
            file_size: 0,
            hash: "hash".to_string(),
            file_name_length: name.len() as u64,
            state,
            name: name.to_string(),
        }
    }

    #[test]
    fn carry_over_marks_missing_entries_as_deleted() {
        let mut old_inventory = Inventory::new();
        old_inventory.add_item(item("kept.txt", 1, InventoryItemState::Normal));
        old_inventory.add_item(item("gone.txt", 2, InventoryItemState::Normal));

        // The rebuilt inventory only found "kept.txt" on disk.
        let mut new_inventory = Inventory::new();
        new_inventory.add_item(item("kept.txt", 1, InventoryItemState::Normal));

        carry_over_missing_entries_as_deleted(&old_inventory, &mut new_inventory);

        assert_eq!(new_inventory.get_items_count(), 2);
        assert!(new_inventory.get_item_by_name("kept.txt").unwrap().state == InventoryItemState::Normal);
        assert!(new_inventory.get_item_by_name("gone.txt").unwrap().state == InventoryItemState::Deleted);
    }

    #[test]
    fn carry_over_keeps_already_staged_removals() {
        let mut old_inventory = Inventory::new();
        old_inventory.add_item(item("removed.txt", 1, InventoryItemState::Deleted));

        let mut new_inventory = Inventory::new();

        carry_over_missing_entries_as_deleted(&old_inventory, &mut new_inventory);

        assert!(new_inventory.get_item_by_name("removed.txt").unwrap().state == InventoryItemState::Deleted);
    }

    /// A fresh warehouse root for one test — mirrors `journal_utils::tests::Scratch`.
    struct Scratch {
        root: std::path::PathBuf,
        _scope: crate::globals::StorageRootScope,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let root = std::env::temp_dir().join(format!(
                "forklift-inventory-test-{}-{}-{}", name, std::process::id(), id
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = crate::globals::StorageRootScope::enter(&root);

            Scratch { root, _scope: scope }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn prepare_stack_inventory_reports_a_conflict_found_before_a_later_corrupt_shard() {
        // Regression: a single up-front parse pass must not let an unrelated corrupt shard mask
        // a real, actionable conflict discovered earlier in the (sorted) scan. The old
        // `has_conflict_entries` short-circuits — check, then parse, then check, one shard at a
        // time — so a conflict in an earlier-sorting shard is always what the user sees, even
        // when a later shard happens to be corrupt. `prepare_stack_inventory` must reproduce
        // that prioritization, not just accidentally match it when there is nothing corrupt.
        let _scratch = Scratch::new("conflict-before-corrupt");

        // "aaa" sorts before "zzz" and has a genuine unresolved conflict.
        let mut conflicted = Inventory::new();
        conflicted.add_item(item("file.txt", 1, InventoryItemState::FirstParentConflict));
        save_inventory(&conflicted, &file_utils::get_inventory_data_path_for_key("aaa")).unwrap();

        // "zzz" sorts after "aaa" and is not a valid inventory shard at all.
        let corrupt_path = file_utils::get_inventory_data_path_for_key("zzz");
        std::fs::create_dir_all(corrupt_path.parent().unwrap()).unwrap();
        std::fs::write(&corrupt_path, b"not a valid inventory shard").unwrap();

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        metadata.insert("aaa".to_string());
        metadata.insert("zzz".to_string());
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        let prepared = prepare_stack_inventory()
            .expect("the conflict in \"aaa\" must be reported, not \"zzz\"'s parse error");

        assert!(prepared.has_conflict, "the conflict found while scanning must be recorded");
        assert!(has_conflict_entries_in(&prepared));

        // The scan stopped at "aaa": "zzz" was never reached, so it is absent from the snapshot
        // rather than having failed to parse into it.
        assert!(prepared.shards.contains_key("aaa"));
        assert!(!prepared.shards.contains_key("zzz"));
    }

    #[test]
    fn prepare_stack_inventory_still_surfaces_a_corrupt_shard_when_nothing_is_in_conflict() {
        // The other half of the same fix: when there is no conflict anywhere, a corrupt shard
        // must still fail loudly (never silently skipped) — the short-circuit only changes
        // *priority* between two real problems, it must not hide either one on its own.
        let _scratch = Scratch::new("corrupt-with-no-conflict");

        let mut clean = Inventory::new();
        clean.add_item(item("file.txt", 1, InventoryItemState::Normal));
        save_inventory(&clean, &file_utils::get_inventory_data_path_for_key("aaa")).unwrap();

        let corrupt_path = file_utils::get_inventory_data_path_for_key("zzz");
        std::fs::create_dir_all(corrupt_path.parent().unwrap()).unwrap();
        std::fs::write(&corrupt_path, b"not a valid inventory shard").unwrap();

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        metadata.insert("aaa".to_string());
        metadata.insert("zzz".to_string());
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        let error = match prepare_stack_inventory() {
            Ok(_) => panic!("a corrupt shard must still fail the scan"),
            Err(message) => message,
        };
        assert!(error.contains("zzz"), "the error should name the offending shard, got: {error}");
    }

    /// Write a shard directly (bypassing the funnel) with the given rollup hash already
    /// stamped — simulates a shard as it would sit right after a `stack` (see
    /// `cleanup_after_stack_with`), without needing a real tree build.
    fn write_stamped_shard(key: &str, rollup_hash: Option<&str>) {
        let mut inventory = Inventory::new();
        inventory.add_item(item("file.txt", 1, InventoryItemState::Normal));
        inventory.set_rollup_hash(rollup_hash.map(|h| h.to_string()));
        save_inventory(&inventory, &file_utils::get_inventory_data_path_for_key(key)).unwrap();
    }

    fn read_rollup(key: &str) -> Option<String> {
        let (_, bytes) = file_utils::retrieve_inventory_or_none_by_key(key).unwrap();
        let inventory = parser::inventory::inventory_parser::parse_inventory(&bytes.unwrap()).unwrap();
        inventory.get_rollup_hash().cloned()
    }

    #[test]
    fn write_shard_mutation_clears_every_ancestor_top_down_but_spares_an_unrelated_sibling() {
        let _scratch = Scratch::new("mutation-clears-ancestors");

        // A chain of previously-stamped shards from root down to a depth-3 directory, plus an
        // unrelated sibling subtree stamped the same way.
        for key in ["", "a", "a/b", "a/b/c", "x"] {
            write_stamped_shard(key, Some("stale-rollup"));
        }

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        for key in ["", "a", "a/b", "a/b/c", "x"] {
            metadata.insert(key_to_metadata_entry(key));
        }
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        // A content-changing mutation three levels deep.
        update_shard("a/b/c", |inventory| {
            inventory.add_item(item("new.txt", 99, InventoryItemState::Normal));
            Ok(())
        }).unwrap();

        // Every ancestor, root first, is cleared — and the mutated shard itself never keeps a
        // stale rollup (the funnel always writes `None` for the shard it mutates).
        assert_eq!(read_rollup(""), None, "the root's stale rollup must be cleared");
        assert_eq!(read_rollup("a"), None, "an intermediate ancestor's stale rollup must be cleared");
        assert_eq!(read_rollup("a/b"), None, "the immediate parent's stale rollup must be cleared");
        assert_eq!(read_rollup("a/b/c"), None, "the mutated shard itself must not keep a rollup");

        // An unrelated sibling subtree is untouched.
        assert_eq!(read_rollup("x").as_deref(), Some("stale-rollup"),
            "a sibling subtree outside the mutated chain must keep its rollup");
    }

    #[test]
    fn write_shard_mutation_is_a_no_op_on_an_already_clear_ancestor() {
        // A shard whose rollup is already `None` must not be rewritten by ancestor invalidation
        // — a light regression guard for the "no-op, don't rewrite" contract, checked via the
        // shard file's absence of change (it was never given a rollup to lose either way, but
        // this at least exercises the skip path without a real tree build).
        let _scratch = Scratch::new("mutation-noop-ancestor");

        write_stamped_shard("", None);
        write_stamped_shard("a", None);

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        metadata.insert(key_to_metadata_entry(""));
        metadata.insert(key_to_metadata_entry("a"));
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        update_shard("a", |inventory| {
            inventory.add_item(item("new.txt", 1, InventoryItemState::Normal));
            Ok(())
        }).unwrap();

        assert_eq!(read_rollup(""), None);
        assert_eq!(read_rollup("a"), None);
    }

    #[test]
    fn remove_inventories_under_clears_ancestor_rollups_conservatively() {
        let _scratch = Scratch::new("narrow-clears-ancestors");

        for key in ["", "a", "a/b"] {
            write_stamped_shard(key, Some("stale-rollup"));
        }

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        for key in ["", "a", "a/b"] {
            metadata.insert(key_to_metadata_entry(key));
        }
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        remove_inventories_under("a/b").unwrap();

        assert_eq!(read_rollup(""), None, "narrow must conservatively clear the root's rollup");
        assert_eq!(read_rollup("a"), None, "narrow must conservatively clear the parent's rollup");
        assert!(!file_utils::get_inventory_data_path_for_key("a/b").exists());
    }

    #[test]
    fn build_inventory_preserves_rollup_across_a_pure_stat_refresh() {
        // The comparison helper that decides whether a rebuilt shard's content is truly
        // unchanged (so its rollup may be carried forward) versus genuinely different (so it
        // must go through the funnel).
        let mut a = Inventory::new();
        a.add_item(item("file.txt", 1, InventoryItemState::Normal));

        let mut b = Inventory::new();
        b.add_item(item("file.txt", 2, InventoryItemState::Normal)); // different inode only

        assert!(inventory_content_matches(&a, &b),
            "a stat-only difference (inode) must not count as a content change");

        let mut c = Inventory::new();
        let mut changed = item("file.txt", 1, InventoryItemState::Normal);
        changed.hash = "different-hash".to_string();
        c.add_item(changed);

        assert!(!inventory_content_matches(&a, &c), "a real hash change must count as a content change");
    }

    #[test]
    fn a_published_shard_carries_its_outcome_verified_at_mtime_not_now() {
        // `load`'s join point publishes every `ShardOutcome` with the timestamp captured when it
        // was verified, not "now" (the join point's own, potentially much later, publish time —
        // see `ShardOutcome`'s doc comment for the staleness hazard that closes). Pin that the
        // publish path (`save_inventory_deferred_with_mtime`) actually honours the outcome's own
        // timestamp field, using a deliberately old anchor so a stray `SystemTime::now()`
        // anywhere in the path would be caught immediately.
        let _scratch = Scratch::new("publish-mtime-anchor");

        let anchor = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(3600))
            .unwrap();
        let outcome = ShardOutcome::Changed(Inventory::new(), anchor);

        let (inventory, verified_at) = match &outcome {
            ShardOutcome::Carry(inventory, verified_at) | ShardOutcome::Changed(inventory, verified_at) =>
                (inventory, *verified_at),
        };

        let batch = file_utils::WriteBatch::new();
        let path = file_utils::get_inventory_data_path_for_key("some/key");
        save_inventory_deferred_with_mtime(inventory, &path, &batch, verified_at).unwrap();
        batch.finish().unwrap();

        let published_mtime = file_utils::get_symlink_metadata_for_path(&path).unwrap()
            .modified().unwrap();

        // Filesystem mtime resolution can be coarser than `SystemTime`'s own precision (whole
        // seconds on some filesystems), so compare within a small tolerance rather than
        // bit-for-bit — the anchor is an hour old, so any real bug (publishing with "now")
        // would miss by nearly an hour, nowhere near this tolerance.
        let diff = published_mtime.duration_since(anchor).unwrap_or_else(|e| e.duration());
        assert!(diff < std::time::Duration::from_secs(2),
            "a published shard's mtime must equal its ShardOutcome's verified_at timestamp, not \
            \"now\": got {:?}, expected {:?} (diff {:?})", published_mtime, anchor, diff);
    }

    /// Set a file's modification time directly, through an open write handle — never a
    /// reopen-by-path (the project's Windows fsync convention; see `WriteBatch::stage_with_mtime`).
    fn set_mtime(path: &Path, mtime: std::time::SystemTime) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(mtime).unwrap();
    }

    #[test]
    fn cleanup_after_stack_does_not_advance_a_stamped_shard_s_mtime() {
        // Regression (multi-agent review of PR #59, finding 1): `cleanup_after_stack_with`'s
        // rollup-stamp rewrite never re-verifies a single entry against the file system (the
        // entries come straight from the already-parsed `PreparedInventory` snapshot) — exactly
        // the "rewrite that verifies nothing" hazard the join-point redesign was already fixed
        // against for `load` itself. Publishing this rewrite with "now" would widen the
        // racily-clean trust window the same way; the shard's own current mtime must survive it.
        let _scratch = Scratch::new("cleanup-mtime-stamp");

        write_stamped_shard("", Some("stale-rollup"));
        let shard_path = file_utils::get_inventory_data_path_for_key("");

        let old_mtime = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(3600)).unwrap();
        set_mtime(&shard_path, old_mtime);

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        metadata.insert(key_to_metadata_entry(""));
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        let prepared = prepare_stack_inventory().unwrap();

        let mut stamp_hashes: BTreeMap<String, String> = BTreeMap::new();
        stamp_hashes.insert("".to_string(), "fresh-rollup".to_string());

        cleanup_after_stack_with(&prepared, &stamp_hashes, &BTreeSet::new()).unwrap();

        // The stamp itself must still take effect — this test is about the mtime, not about
        // whether the rewrite happens at all.
        assert_eq!(read_rollup(""), Some("fresh-rollup".to_string()));

        let published_mtime = file_utils::get_symlink_metadata_for_path(&shard_path).unwrap()
            .modified().unwrap();
        let diff = published_mtime.duration_since(old_mtime).unwrap_or_else(|e| e.duration());
        assert!(diff < std::time::Duration::from_secs(2),
            "a rollup stamp verifies nothing, so it must not advance the shard's mtime: got {:?}, \
            expected close to {:?} (diff {:?})", published_mtime, old_mtime, diff);
    }

    #[test]
    fn cleanup_after_stack_does_not_advance_mtime_when_dropping_deleted_entries() {
        // The other rewrite `cleanup_after_stack_with` performs — dropping now-consumed
        // `Deleted` entries — is exactly as unverified as the stamp above (same regression,
        // same fix): the survivors are copied straight out of `PreparedInventory`, nothing here
        // is re-checked against the file system either.
        let _scratch = Scratch::new("cleanup-mtime-deleted-drop");

        let mut inventory = Inventory::new();
        inventory.add_item(item("kept.txt", 1, InventoryItemState::Normal));
        inventory.add_item(item("removed.txt", 2, InventoryItemState::Deleted));
        save_inventory(&inventory, &file_utils::get_inventory_data_path_for_key("")).unwrap();

        let shard_path = file_utils::get_inventory_data_path_for_key("");
        let old_mtime = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(3600)).unwrap();
        set_mtime(&shard_path, old_mtime);

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        metadata.insert(key_to_metadata_entry(""));
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        let prepared = prepare_stack_inventory().unwrap();

        // No stamp target at all — the rewrite is triggered purely by the staged `Deleted`
        // entry that needs dropping, not by a rollup mismatch.
        cleanup_after_stack_with(&prepared, &BTreeMap::new(), &BTreeSet::new()).unwrap();

        assert!(read_rollup("").is_none());

        let (_, bytes) = file_utils::retrieve_inventory_or_none_by_key("").unwrap();
        let rebuilt = parser::inventory::inventory_parser::parse_inventory(&bytes.unwrap()).unwrap();
        assert!(rebuilt.get_item_by_name("kept.txt").is_some(), "a surviving entry must be kept");
        assert!(rebuilt.get_item_by_name("removed.txt").is_none(), "the Deleted entry must be dropped");

        let published_mtime = file_utils::get_symlink_metadata_for_path(&shard_path).unwrap()
            .modified().unwrap();
        let diff = published_mtime.duration_since(old_mtime).unwrap_or_else(|e| e.duration());
        assert!(diff < std::time::Duration::from_secs(2),
            "dropping a Deleted entry verifies nothing either, so it must not advance the shard's \
            mtime: got {:?}, expected close to {:?} (diff {:?})", published_mtime, old_mtime, diff);
    }

    #[test]
    fn a_corrupt_dirty_shard_during_load_yields_the_resilience_message_not_a_raw_parse_error() {
        // Regression (multi-agent review of PR #59, finding 2): a join-point failure (here, a
        // leftover shard that fails to parse) used to `?`-propagate straight out of
        // `create_inventory_for_directory`, bypassing the failure-resilience branch entirely —
        // skipping `update_inventory_metadata` for whatever *was* successfully published this
        // walk, and surfacing a raw parse error instead of the documented "entries loaded so far
        // were kept, re-run" message a walk failure already gives.
        let _scratch = Scratch::new("load-corrupt-dirty-shard");

        // A previously-registered directory whose shard is corrupt, with no counterpart left in
        // the working directory — exactly what `load`'s post-walk "leftover dirty entries" pass
        // consults, so this is the shard the join point's dirty-path loop tries (and fails) to
        // parse.
        let corrupt_path = file_utils::get_inventory_data_path_for_key("gone");
        std::fs::create_dir_all(corrupt_path.parent().unwrap()).unwrap();
        std::fs::write(&corrupt_path, b"not a valid inventory shard").unwrap();

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        metadata.insert(key_to_metadata_entry("gone"));
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        // A real, trackable file in the actual working directory — proves the walk's own
        // successful work is still published despite the dirty-path failure that comes after it.
        std::fs::write(_scratch.root.join("real.txt"), b"hello").unwrap();

        let path = crate::util::path_utils::WarehousePath::from_user_input(".").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let result = runtime.block_on(create_inventory_for_directory(&path));

        let error = match result {
            Ok(()) => panic!("a corrupt dirty shard must fail the load"),
            Err(message) => message,
        };
        assert!(error.contains("did not complete"),
            "must use the graceful-failure resilience message, not a raw parse error: {error}");
        assert!(error.contains("gone"),
            "the underlying parse error should still name the offending shard: {error}");

        // The walk's own real work is still durable and registered: "entries loaded so far were
        // kept" is a real guarantee, not just wording in the message.
        assert!(file_utils::get_inventory_data_path_for_key("").exists(),
            "the root shard this walk produced must still exist on disk");
        assert!(file_utils::get_inventory_data_path_for_key("real.txt").parent().is_some());

        let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        let metadata_after = metadata_opt.unwrap();
        assert!(metadata_after.contains(&key_to_metadata_entry("")),
            "the newly discovered root directory must still be registered: {:?}", metadata_after);
    }
}
