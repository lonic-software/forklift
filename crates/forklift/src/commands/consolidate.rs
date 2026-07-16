use serde::Serialize;
use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
use forklift_core::enums::dir_entry_type::DirEntryType;
use forklift_core::enums::inventory_item_state::InventoryItemState;
use forklift_core::model::blob::Blob;
use forklift_core::model::inventory::InventoryItem;
use forklift_core::util::merge_utils::{ConsolidationState, MergeAction};
use forklift_core::util::stocktake_utils::ChangeKind;
use forklift_core::util::{
    inventory_utils, merge_utils, object_utils, office_utils, pallet_utils, scope_utils,
    shift_utils, stack_utils, stocktake_utils,
};
use crate::output::{self, CommandOutput};

/// Handle the consolidate command (git's "merge"; warehouse workers consolidate loads
/// onto one pallet): merge the head of the given pallet into the current pallet.
///
/// * When the current head already contains their head, nothing happens.
/// * When their head contains the current head, the current pallet fast-forwards.
/// * Otherwise a three-way merge against the common ancestor runs. Cleanly merged
///   changes are staged and the merge parcel (two parents) is stacked immediately;
///   conflicts are written to the working directory with markers, the entries are put
///   into a conflict state, and the consolidation stays in progress until the conflicts
///   are resolved, loaded, and stacked.
///
/// # Arguments
/// * `target` - The pallet to consolidate into the current one.
///
/// # Returns
/// * `Ok(())`      - If the consolidation completed (or was cleanly a no-op).
/// * `Err(String)` - If there was an error while handling the command.
pub async fn handle_command(target: &str) -> Result<(), String> {
    // A merge in a scoped bay resolves out-of-scope siblings by hash: a one-sided change
    // is adopted from theirs into the merge parcel's tree without materializing it; a genuine
    // out-of-scope conflict refuses (`out_of_scope_conflict`). In-scope content merges as usual.
    pallet_utils::validate_pallet_name(target)?;

    let current = pallet_utils::get_current_pallet_name()?;

    if target == current {
        return Err("A pallet cannot be consolidated into itself.".to_string());
    }

    if merge_utils::read_consolidation_state()?.is_some() {
        return Err(
            "A consolidation is already in progress. Resolve its conflicts and \"stack\", \
            or remove \".forklift/consolidation\" (and \".forklift/consolidation-skeleton\", \
            if present) to abort it.".to_string()
        );
    }

    if forklift_core::util::cherry_pick_utils::read_state()?.is_some() {
        return Err(
            "A cherry-pick is in progress. Complete it (resolve, \"load\", \"stack\") or abort \
            it before consolidating.".to_string()
        );
    }

    let Some(our_head) = pallet_utils::get_pallet_head(&current)? else {
        return Err(format!(
            "Pallet \"{}\" has nothing stacked yet; there is nothing to consolidate into.",
            current
        ));
    };

    let Some(their_head) = pallet_utils::get_pallet_head(&target)? else {
        return Err(format!("No pallet named \"{}\" exists (or it has nothing stacked).", target));
    };

    let our_tree_hash = object_utils::load_parcel(&our_head)?.tree_hash;

    ensure_warehouse_is_clean(&our_tree_hash).await?;

    if merge_utils::is_ancestor(&their_head, &our_head)? {
        output::emit("consolidate", &ConsolidateReport::up_to_date(&current, target));
        return Ok(());
    }

    let their_tree_hash = object_utils::load_parcel(&their_head)?.tree_hash;

    // A hand-made ref could point at an office parcel; its tracked-metadata namespace
    // must never be merged into a working pallet.
    office_utils::ensure_not_metadata_tree(&their_tree_hash)?;

    if merge_utils::is_ancestor(&our_head, &their_head)? {
        let head = fast_forward(&current, &target, &our_tree_hash, &their_head, &their_tree_hash)?;

        output::emit("consolidate", &ConsolidateReport::fast_forward(&current, target, &head));

        return Ok(());
    }

    match merge_head_into_current(&current, &our_head, &their_head, target, true).await? {
        MergeStatus::Merged(hash) =>
            output::emit("consolidate", &ConsolidateReport::merged(&current, target, &hash)),
        MergeStatus::Conflicts(conflicts) =>
            output::emit("consolidate", &ConsolidateReport::conflicts(&current, target, conflicts)),
    }

    Ok(())
}

/// What a consolidation did. `Conflicts` is the only outcome that leaves work for the
/// operator (resolve, load, stack); the rest are complete.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConsolidateOutcome {
    UpToDate,
    FastForward,
    Merged,
    Conflicts,
}

/// The result of a consolidate.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ConsolidateReport {
    outcome: ConsolidateOutcome,

    /// The pallet consolidated into (the current one).
    pallet: String,

    /// The pallet consolidated in.
    target: String,

    /// The merge (or fast-forward) parcel/head, when one resulted.
    #[serde(skip_serializing_if = "Option::is_none")]
    parcel: Option<String>,

    /// The conflicting paths, when the merge did not complete cleanly.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    conflicts: Vec<String>,
}

impl ConsolidateReport {
    fn up_to_date(current: &str, target: &str) -> ConsolidateReport {
        ConsolidateReport {
            outcome: ConsolidateOutcome::UpToDate,
            pallet: current.to_string(),
            target: target.to_string(),
            parcel: None,
            conflicts: Vec::new(),
        }
    }

    fn fast_forward(current: &str, target: &str, head: &str) -> ConsolidateReport {
        ConsolidateReport {
            outcome: ConsolidateOutcome::FastForward,
            pallet: current.to_string(),
            target: target.to_string(),
            parcel: Some(head.to_string()),
            conflicts: Vec::new(),
        }
    }

    fn merged(current: &str, target: &str, parcel: &str) -> ConsolidateReport {
        ConsolidateReport {
            outcome: ConsolidateOutcome::Merged,
            pallet: current.to_string(),
            target: target.to_string(),
            parcel: Some(parcel.to_string()),
            conflicts: Vec::new(),
        }
    }

    fn conflicts(current: &str, target: &str, conflicts: Vec<String>) -> ConsolidateReport {
        ConsolidateReport {
            outcome: ConsolidateOutcome::Conflicts,
            pallet: current.to_string(),
            target: target.to_string(),
            parcel: None,
            conflicts,
        }
    }
}

impl CommandOutput for ConsolidateReport {
    fn render_human(&self) {
        match self.outcome {
            ConsolidateOutcome::UpToDate => {
                println!(
                    "Already up to date: \"{}\" contains the head of \"{}\".",
                    self.pallet, self.target
                );
            }
            ConsolidateOutcome::FastForward => {
                println!(
                    "Fast-forwarded \"{}\" to the head of \"{}\" ({}).",
                    self.pallet, self.target, self.parcel.as_deref().unwrap_or("")
                );
            }
            ConsolidateOutcome::Merged => {
                println!(
                    "Consolidated \"{}\" into \"{}\": stacked merge parcel {}.",
                    self.target, self.pallet, self.parcel.as_deref().unwrap_or("")
                );
            }
            ConsolidateOutcome::Conflicts => {
                println!(
                    "Consolidating \"{}\" into \"{}\" produced {} conflict(s):",
                    self.target, self.pallet, self.conflicts.len()
                );

                for path in &self.conflicts {
                    println!("  conflict: {}", path);
                }

                println!(
                    "\nResolve the conflicts, \"load\" the resolved files, then \"stack\" to \
                    complete the consolidation."
                );
            }
        }
    }
}

/// The outcome of merging one head into the current pallet.
pub(crate) enum MergeStatus {
    /// A clean merge: the two-parent merge parcel was stacked (its hash).
    Merged(String),

    /// The merge conflicts on these paths.
    Conflicts(Vec<String>),
}

/// Three-way merge `their_head` into the current pallet against their common ancestor,
/// and — when clean — stack the two-parent merge parcel. Shared by `consolidate` and by
/// `lift`'s optimistic auto-merge (§7.7).
///
/// The caller must have established that this is a genuine divergence (not up-to-date, not
/// a fast-forward) and that the warehouse is clean. `their_label` names the other side in
/// messages and the consolidation state (a pallet name, or `"remote"` for a lift).
///
/// When `apply_conflicts` is false and the merge would conflict, **nothing is touched** and
/// `Conflicts` is returned — so the optimistic path can bail without dirtying the working
/// directory. Otherwise the merge is applied: a clean one is stacked (`Merged`), a
/// conflicting one is left in progress for the operator to resolve (`Conflicts`).
///
/// # Returns
/// * `Ok(MergeStatus)` - The outcome.
/// * `Err(String)`     - If the two share no history, or an operation failed.
pub(crate) async fn merge_head_into_current(current: &str,
                                            our_head: &str,
                                            their_head: &str,
                                            their_label: &str,
                                            apply_conflicts: bool) -> Result<MergeStatus, String> {
    let our_tree_hash = object_utils::load_parcel(our_head)?.tree_hash;
    let their_tree_hash = object_utils::load_parcel(their_head)?.tree_hash;

    office_utils::ensure_not_metadata_tree(&their_tree_hash)?;

    let base = merge_utils::find_merge_base(our_head, their_head)?
        .ok_or(format!("\"{}\" and \"{}\" share no history; they cannot be merged.", current, their_label))?;
    let base_tree_hash = object_utils::load_parcel(&base)?.tree_hash;

    // In a scoped (sparse) bay the classifier resolves out-of-scope siblings by hash and refuses
    // genuine out-of-scope conflicts before any object is loaded; a full scope leaves the merge
    // exactly as before.
    let scope = scope_utils::current_scope()?;

    let actions = merge_utils::compute_merge_actions(
        &base_tree_hash, &our_tree_hash, &their_tree_hash, current, their_label, &scope
    )?;

    // The optimistic path (apply_conflicts = false) refuses to touch the working directory
    // when the merge is not clean, so a diverged lift with overlapping changes still stops.
    let would_conflict = actions.iter().any(|action| matches!(action, MergeAction::Conflict { .. }));

    if would_conflict && !apply_conflicts {
        let mut paths: Vec<String> = actions.iter()
            .filter_map(|action| match action {
                MergeAction::Conflict { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect();
        paths.sort();

        return Ok(MergeStatus::Conflicts(paths));
    }

    ensure_no_untracked_collisions(&actions, their_label)?;

    let mut conflict_paths = apply_merge_actions(&actions)?;

    // Record the out-of-scope skeleton BEFORE the consolidation state, and unconditionally
    // — even when it is empty (a full-bay merge, or one that resolved nothing out of scope): the
    // completing `stack` (`stack_utils::stack_parcel`) requires the skeleton file to exist
    // whenever a consolidation is in progress, so this ordering guarantees a crash or a failed
    // write between the two can never leave consolidation state whose skeleton is silently
    // treated as empty — which would drop every adopted-by-hash entry from the committed tree.
    // Clearing first guards against a stale skeleton left behind by an aborted earlier merge in
    // this bay.
    merge_utils::OutOfScopeSkeleton::clear()?;
    merge_utils::OutOfScopeSkeleton::from_actions(&actions).write()?;

    merge_utils::write_consolidation_state(&ConsolidationState {
        their_head: their_head.to_string(),
        their_pallet: their_label.to_string(),
    })?;

    if conflict_paths.is_empty() {
        let description = format!("Consolidated \"{}\" into \"{}\".", their_label, current);
        let (hash, _) = stack_utils::stack_parcel(Some(description)).await?;

        Ok(MergeStatus::Merged(hash))
    } else {
        conflict_paths.sort();

        Ok(MergeStatus::Conflicts(conflict_paths))
    }
}

/// Ensure there are no staged or unstaged changes (untracked files are allowed) — the
/// precondition for a merge that materializes into the working directory. Public within
/// the crate so `lift`'s optimistic path can check before auto-merging.
pub(crate) async fn is_warehouse_clean(our_tree_hash: &str) -> Result<bool, String> {
    ensure_warehouse_is_clean(our_tree_hash).await.map(|_| true).or_else(|_| Ok(false))
}

/// Fast-forward the current pallet to their head: materialize the tree difference and
/// repopulate the inventory, exactly like a shift — but moving the current pallet's ref.
fn fast_forward(current: &str,
                target: &str,
                our_tree_hash: &str,
                their_head: &str,
                their_tree_hash: &str) -> Result<String, String> {
    let (ops, removed_dirs) = shift_utils::diff_trees(Some(our_tree_hash), their_tree_hash)?;

    let conflicts = shift_utils::collect_untracked_collisions(&ops)?;

    if !conflicts.is_empty() {
        return Err(format!(
            "Consolidating \"{}\" would overwrite these untracked files:\n  {}\n\
            Move them out of the way (or load and stack them) first.",
            target,
            conflicts.join("\n  ")
        ));
    }

    shift_utils::apply_ops(&ops, &removed_dirs)?;

    let shards = shift_utils::build_inventories_for_tree(their_tree_hash)?;
    inventory_utils::replace_all_inventories(&shards)?;

    pallet_utils::set_pallet_head(current, their_head)?;

    Ok(their_head.to_string())
}

/// Ensure there are no staged or unstaged changes (untracked files are allowed).
async fn ensure_warehouse_is_clean(our_tree_hash: &str) -> Result<(), String> {
    let staged = stocktake_utils::collect_staged_changes(Some(our_tree_hash)).await?;
    let unstaged: Vec<_> = stocktake_utils::collect_unstaged_changes().await?
        .into_iter()
        .filter(|change| change.kind != ChangeKind::Untracked)
        .collect();

    if staged.is_empty() && unstaged.is_empty() {
        return Ok(());
    }

    Err(
        "There are local changes that consolidating would overwrite. Stack them, restore \
        them, or park them first (see \"stocktake\" for the details).".to_string()
    )
}

/// Ensure the merge will not overwrite untracked files: every path the merge writes that
/// does not exist in our tree (`is_new` takes, delete/modify conflict re-adds) must not
/// exist in the working directory. Shared with `cherry-pick`, which applies the same
/// `MergeAction`s.
pub(crate) fn ensure_no_untracked_collisions(actions: &[MergeAction], target: &str) -> Result<(), String> {
    let mut collisions: Vec<&str> = Vec::new();

    for action in actions {
        let new_write = match action {
            MergeAction::TakeTheirs { path, hash, item_type, is_new: true } =>
                Some((path, ExpectedWrite::ByHash { hash, item_type: *item_type })),
            // A delete/modify conflict re-creates a file we deleted.
            MergeAction::Conflict { path, content: Some(_), .. }
                if !std::path::Path::new(path).exists() => None,
            MergeAction::Conflict { path, content: Some(content), item_type, .. } =>
                Some((path, ExpectedWrite::ByBytes { content, item_type: *item_type })),
            _ => None,
        };

        let Some((path, expected)) = new_write else { continue };

        // A tracked file cannot collide (tracked paths were verified clean). A tracked
        // directory with no untracked content beneath it cannot collide either — the merge
        // legitimately replaces it with the new entry (see `apply_merge_actions`'s
        // deletes-before-writes ordering); a directory is tracked by its own inventory
        // shard, not as an item in its parent's inventory, so it is checked separately.
        let is_tracked_file = inventory_lookup(path)?.is_some();
        let fs_path = std::path::Path::new(path);
        let is_replaceable_dir = fs_path.is_dir()
            && inventory_utils::directory_is_safe_to_replace(path)?;

        if is_tracked_file || is_replaceable_dir || !fs_path.exists() {
            continue;
        }

        // An untracked file already on disk is a real collision — unless it already holds
        // exactly the content this action would write there (mirrors `park pop`'s identical
        // recovery-retry check, and reuses the same underlying comparison for `TakeTheirs`): a
        // user who happens to have identical untracked content, or this merge's own leftover
        // working-directory write from a previous attempt whose `batch.publish()` failed after
        // every action's write landed but before its shard mutation was ever published — the
        // exact scenario `apply_merge_actions`'s own recovery advice (`restore --staged .` then
        // `restore .`) cannot itself clean up, since neither step deletes a path that is
        // untracked on the pallet head. Without this check the advice is self-defeating: the
        // retry it recommends would see the merge's own prior write as a permanent conflict with
        // itself. An unreadable file is conservatively still a listed conflict, not an aborted
        // scan — same infallible-scan discipline as `park pop`'s identical check.
        let matches = match expected {
            ExpectedWrite::ByHash { hash, item_type } =>
                object_utils::on_disk_file_matches_hash(fs_path, hash, item_type),
            ExpectedWrite::ByBytes { content, item_type } =>
                on_disk_file_matches_bytes(fs_path, content, item_type),
        };

        match matches {
            Ok(true) => {}
            Ok(false) | Err(_) => collisions.push(path),
        }
    }

    if collisions.is_empty() {
        return Ok(());
    }

    Err(format!(
        "Consolidating \"{}\" would overwrite these untracked files:\n  {}\n\
        Move them out of the way (or load and stack them) first.",
        target,
        collisions.join("\n  ")
    ))
}

/// What [`ensure_no_untracked_collisions`] expects an action to write at a path, in whichever
/// shape that action's own `MergeAction` variant already carries it — a target hash for
/// `TakeTheirs` (its content is already stored, addressed by hash), or the raw bytes for
/// `Conflict` (a line-merge result or a delete/modify conflict's content, held in memory, never
/// itself hashed as a standalone object).
enum ExpectedWrite<'a> {
    ByHash { hash: &'a str, item_type: DirEntryType },
    ByBytes { content: &'a [u8], item_type: DirEntryType },
}

/// Whether the on-disk file at `path` already holds exactly `content`/`item_type` — the
/// [`MergeAction::Conflict`] counterpart of [`object_utils::on_disk_file_matches_hash`] (used for
/// [`MergeAction::TakeTheirs`] above), which has no target *hash* to compare against: its content
/// is already fully in memory. Same [`DirEntryType::on_disk_kind`] type-normalization, a direct
/// byte comparison instead of a hash — `content` here is always a plain, non-chunked file's worth
/// of bytes (a chunked conflict always materializes from `entry_hash` instead, never carries
/// inline `content` — see `apply_merge_action_write`'s own doc comment), so reading it whole is
/// bounded the same way building it in memory already was.
fn on_disk_file_matches_bytes(path: &std::path::Path,
                              content: &[u8],
                              item_type: DirEntryType) -> Result<bool, String> {
    let metadata = forklift_core::util::file_utils::get_symlink_metadata_for_path(path)?;

    if forklift_core::util::file_utils::get_type_of_dir_entry(&metadata).on_disk_kind() != item_type.on_disk_kind() {
        return Ok(false);
    }

    let on_disk = std::fs::read(path)
        .map_err(|e| format!("Error while reading \"{}\": {}", path.to_string_lossy(), e))?;

    Ok(on_disk == content)
}

/// Look up the inventory entry for a warehouse path (`None` when the file is untracked).
fn inventory_lookup(path: &str) -> Result<Option<InventoryItem>, String> {
    let (parent_key, name) = match path.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", path),
    };

    let (_, bytes_opt) = forklift_core::util::file_utils::retrieve_inventory_or_none_by_key(parent_key)?;

    let Some(bytes) = bytes_opt else {
        return Ok(None);
    };

    let inventory = forklift_core::parser::inventory::inventory_parser::parse_inventory(&bytes)
        .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", parent_key, e))?;

    Ok(inventory.get_item_by_name(name).map(|item| (*item).clone()))
}

/// Apply every merge action to the working directory and the inventory, and return the
/// paths left in conflict. Shared with `cherry-pick`, which applies a parcel's diff through
/// the same `MergeAction`s.
///
/// Three passes, in this order: every delete's working-directory removal
/// ([`apply_delete_working_directory`]), then every other action's working-directory write
/// ([`apply_merge_action_write`]), then every applied action's shard decision
/// ([`apply_merge_action_decide`]) — collected into one shared
/// [`inventory_utils::ShardMutationBatch`] instead of paying `update_shard`'s full two-barrier
/// funnel per action. Deletes run before writes so a directory a write is about to replace (or a
/// file a write is about to turn into a directory) is emptied first — writing first would
/// otherwise fail (`EISDIR`/`EEXIST`) for a tracked type flip in either direction — but *deciding*
/// a delete's shard mutation is deferred to the third pass along with everything else.
///
/// This is what makes the batch's first-touch anchor for any shard more than one action lands in
/// sound *and* useful, not just the first: every real working-directory write (pass 1 or 2) for
/// the whole merge completes strictly before the decide pass (3) ever starts, so a shard touched
/// by several actions has its published mtime anchored after every one of them — satisfying
/// `is_entry_unchanged`'s `mtime < shard_mtime` stat-cache guard for every file, not only whichever
/// action happened to touch that shard first (mirrors `restore_shard_files_into`'s identical
/// write-then-decide mtime-anchor fix — see its own doc comment for the full reasoning). Two
/// actions touching the same directory still collapse into one read-modify-write of that shard —
/// `ShardMutationBatch::flush_if_full`, called once per decided action, also bounds this batch's
/// peak memory to a small, constant slice of shards instead of the whole merge (a merge diff can
/// be as large as the repository itself — a formatter run touching every tracked file, say — so
/// "bounded by the diff" was never actually a bound).
///
/// If a working-directory write fails partway through (pass 1 or 2), only the actions that
/// already wrote successfully reach the decide pass — every one of *those* is still decided and
/// published, the same keep-whatever-was-decided resilience `refresh_tracked_entries` gives a
/// per-shard failure (see its own doc comment): under the old per-action immediate-write loop this
/// replaced, every prior action was already durably applied by the time a later one failed, so
/// this preserves that same user-visible guarantee under batching instead of silently regressing
/// to all-or-nothing. If a *decision* fails instead (pass 3), every action decided before it is
/// likewise still published.
///
/// **A `batch.publish()`/`flush_if_full()` failure is a different, and wider, case than the old
/// code had:** every action's working-directory write still
/// happens unconditionally, before its shard mutation is ever staged — so by the time a flush or
/// the final `publish()` runs, the working directory may already reflect *every* action in this
/// call, even though publishing can fail for a reason unrelated to any specific action (an
/// unreadable ancestor shard, a mid-barrier I/O error). The old per-action immediate funnel could
/// only ever leave *one* action's file diverged from its shard this way (the one whose own
/// `write_shard_mutation` call failed — every action after it in the loop never even ran); this
/// batch can leave many, though bounded to at most one flush group's worth by the periodic
/// flushing above, not the whole merge. Not data loss (nothing here destroys anything — every real
/// change is either already sitting in the working directory or still recoverable from the
/// pre-merge head), but a real widening of the "working directory and inventory can disagree after
/// a failure" surface that batching introduces here, worth documenting rather than glossing over.
///
/// **Recovery is two commands, in this order, not one**: `restore --staged .` first
/// (unconditionally rebuilds every shard from the pallet head, regardless of whatever partial
/// state publishing left behind — see `restore_staged`'s own doc comment), *then* `restore .`
/// (now safe: with the inventory back at head, this rewrites the working directory to match it
/// exactly). `load .` alone is actively self-defeating here: it would stage the half-applied
/// working directory, and the resulting non-empty staged diff then makes `ensure_warehouse_is_
/// clean` *refuse* the very retry an operator would be attempting. Both recovery steps are cheap,
/// always-available, and require no reconciliation beyond themselves; a subsequent `consolidate`/
/// `cherry-pick` retry starts from a genuinely clean warehouse and, on success, records a proper
/// multi-parent merge — unlike `load .` followed by a plain `stack`, which would silently commit
/// the recovered content as an ordinary single-parent parcel with the target pallet recorded as
/// never merged.
pub(crate) fn apply_merge_actions(actions: &[MergeAction]) -> Result<Vec<String>, String> {
    let mut conflict_paths: Vec<String> = Vec::new();
    let mut batch = inventory_utils::ShardMutationBatch::new();

    // `result` accumulates the *first* failure — see the function's own doc comment for why this,
    // rather than propagating immediately, matters here. `applied` collects every action whose
    // working-directory step (pass 1 or 2) actually succeeded, in the same deletes-then-writes
    // relative order the old single interleaved loop used — the decide pass (3) below only ever
    // considers these, preserving the original resilience contract despite writes and decisions
    // no longer being interleaved per action.
    let mut result: Result<(), String> = Ok(());
    let mut applied: Vec<&MergeAction> = Vec::with_capacity(actions.len());

    'deletes: for action in actions {
        if matches!(action, MergeAction::Delete { .. }) {
            if let Err(e) = apply_delete_working_directory(action) {
                result = Err(e);
                break 'deletes;
            }

            applied.push(action);
        }
    }

    if result.is_ok() {
        'writes: for action in actions {
            if matches!(action, MergeAction::Delete { .. }) {
                continue;
            }

            if let Err(e) = apply_merge_action_write(action) {
                result = Err(e);
                break 'writes;
            }

            applied.push(action);
        }
    }

    'decide: for action in &applied {
        if let Err(e) = apply_merge_action_decide(action, &mut conflict_paths, &mut batch) {
            if result.is_ok() { result = Err(e); }
            break 'decide;
        }

        // Bounds peak memory to a small, constant slice of shards instead of the whole merge —
        // see `ShardMutationBatch::flush_if_full`'s own doc comment.
        if let Err(e) = batch.flush_if_full() {
            if result.is_ok() { result = Err(e); }
            break 'decide;
        }
    }

    // Every action decided above becomes durable now, through the shared join point. Attempted
    // even after a mid-loop failure, exactly like `refresh_tracked_entries`'s identical resilience
    // contract, for the same reason.
    if let Err(e) = batch.publish() {
        if result.is_ok() { result = Err(e); }
    }

    if let Err(e) = result {
        return Err(format!(
            "{}\nThe merge did not complete: some of its working-directory writes (and possibly \
            some of their shard content too) may have already landed without the merge as a \
            whole being staged. To recover: run \"restore --staged .\" (resets the inventory to \
            the pallet head) followed by \"restore .\" (rewrites the working directory to match) \
            — in that order — then retry.",
            e
        ));
    }

    Ok(conflict_paths)
}

/// Apply a [`MergeAction::Delete`]'s working-directory removal (and directory-chain cleanup) —
/// see [`apply_merge_actions`]'s own doc comment for why every working-directory step across the
/// whole merge happens before any shard decision. A no-op for any other action variant (deletes
/// are the only variant with a *separate* working-directory pass from the rest — see
/// [`apply_merge_action_write`]).
fn apply_delete_working_directory(action: &MergeAction) -> Result<(), String> {
    let MergeAction::Delete { path } = action else {
        return Ok(());
    };

    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("Error while removing \"{}\": {}", path, e)),
    }

    // Clean up the directory chain if the removal emptied it.
    let mut dir = std::path::Path::new(path).parent();

    while let Some(parent) = dir {
        if parent.as_os_str().is_empty() || std::fs::remove_dir(parent).is_err() {
            break;
        }

        dir = parent.parent();
    }

    Ok(())
}

/// Apply one non-delete merge action's working-directory write only — no shard decision, no blob
/// store; see [`apply_merge_actions`]'s own doc comment for why. A no-op for [`MergeAction::Delete`]
/// (handled separately, first, by [`apply_delete_working_directory`]) and
/// [`MergeAction::ResolveOutOfScope`] (never touches the working directory at all).
fn apply_merge_action_write(action: &MergeAction) -> Result<(), String> {
    match action {
        MergeAction::TakeTheirs { path, hash, item_type, .. } =>
            shift_utils::write_tracked_file(path, hash, *item_type),

        MergeAction::Merged { path, content, item_type } =>
            write_merged_file(path, content, *item_type),

        MergeAction::Conflict { path, content, entry_hash, item_type } => {
            if let Some(content) = content {
                write_merged_file(path, content, *item_type)
            } else if item_type.is_chunked() {
                // A chunked (binary) conflict carries no inline content: materialize the
                // should-be-on-disk version from its recipe (`entry_hash` is ours when we keep
                // ours, theirs when theirs is put back). Bounded, verified stream-assembly.
                shift_utils::write_tracked_file(path, entry_hash, *item_type)
            } else {
                Ok(())
            }
        }

        MergeAction::Delete { .. } | MergeAction::ResolveOutOfScope { .. } => Ok(()),
    }
}

/// Stage one merge action's inventory mutation (and, for [`MergeAction::Merged`], its new blob)
/// into `batch` — see [`apply_merge_actions`]'s own doc comment for why this always runs strictly
/// after every action's own working-directory write has already completed (mtime-anchor
/// soundness), not interleaved with it.
fn apply_merge_action_decide(action: &MergeAction,
                             conflict_paths: &mut Vec<String>,
                             batch: &mut inventory_utils::ShardMutationBatch) -> Result<(), String> {
    match action {
        MergeAction::TakeTheirs { path, hash, item_type, .. } =>
            inventory_utils::stage_file_entry_from_stat_into(batch, path, hash.clone(), *item_type),

        MergeAction::Delete { path } => {
            let (parent_key, name) = split_path(path);

            batch.update(parent_key, |inventory| {
                inventory.remove_item_by_name(name);
                Ok(())
            })
        }

        MergeAction::Merged { path, content, item_type } => {
            // The merged content is new — stage its blob into the batch's own blob barrier so the
            // next stack can point at it (durable before any shard content that might reference
            // it, never stored immediately here). A three-way merge only ever runs on plain text
            // files, so a `Merged` result is always a plain blob, never chunked.
            let mut object = LooseObjectBuilder::build_blob(&Blob { content: content.clone() });
            object.store_deferred(batch.blob_batch())?;

            inventory_utils::stage_file_entry_from_stat_into(batch, path, object.hash, *item_type)
        }

        MergeAction::Conflict { path, entry_hash, item_type, .. } => {
            let (parent_key, name) = split_path(path);

            let mut entry = inventory_utils::build_stale_inventory_item(
                name,
                entry_hash.clone(),
                *item_type
            );
            entry.state = InventoryItemState::FirstParentConflict;

            batch.update(parent_key, move |inventory| {
                inventory.add_item(entry);
                Ok(())
            })?;

            conflict_paths.push(path.clone());

            Ok(())
        }

        // An out-of-scope entry resolved by hash never touches the working directory or the
        // inventory: it is carried in the out-of-scope skeleton and spliced into the merge
        // parcel's tree by the completing stack's overlay. Nothing to apply here.
        MergeAction::ResolveOutOfScope { .. } => Ok(()),
    }
}

/// Write merged content to the working directory (creating parent directories), applying
/// the executable bit when needed.
fn write_merged_file(path: &str, content: &[u8], item_type: DirEntryType) -> Result<(), String> {
    let fs_path = std::path::Path::new(path);

    if let Some(parent) = fs_path.parent() {
        if !parent.as_os_str().is_empty() {
            forklift_core::util::file_utils::create_folder_if_not_exists(parent)?;
        }
    }

    std::fs::write(fs_path, content)
        .map_err(|e| format!("Error while writing \"{}\": {}", path, e))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = if item_type == DirEntryType::Executable { 0o755 } else { 0o644 };

        std::fs::set_permissions(fs_path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("Error while setting the permissions of \"{}\": {}", path, e))?;
    }

    #[cfg(windows)]
    let _ = item_type;

    Ok(())
}

/// Split a warehouse path into its parent directory key and file name.
fn split_path(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", path),
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("ConsolidateReport", schemars::schema_for!(ConsolidateReport)),
    ]
}
