use std::collections::HashSet;
use serde::Serialize;
use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
use forklift_core::enums::inventory_item_state::InventoryItemState;
use forklift_core::model::blob::Blob;
use forklift_core::util::merge_utils::{ConsolidationState, MergeAction};
use forklift_core::util::operation_utils::{Operation, OperationKind};
use forklift_core::util::stocktake_utils::ChangeKind;
use forklift_core::util::{
    alias_utils, cherry_pick_utils, file_utils, inventory_utils, load_guard_utils, merge_utils, object_utils,
    office_utils, operation_utils, pallet_utils, scope_utils, shift_utils, stack_utils, stocktake_utils,
};
use forklift_core::util::shift_utils::HeadWrite;
use forklift_core::util::stack_utils::StackError;
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

    let position = pallet_utils::read_position()?;
    let current = position.pallet.clone();

    if target == current {
        return Err("A pallet cannot be consolidated into itself.".to_string());
    }

    if merge_utils::read_consolidation_state()?.is_some() {
        return Err(format!(
            "A consolidation is already in progress. Resolve its conflicts and \"stack\", or {} \
            to abort it.",
            merge_utils::abort_hint_in(&forklift_core::globals::bay_root())
        ));
    }

    if forklift_core::util::cherry_pick_utils::read_state()?.is_some() {
        return Err(
            "A cherry-pick is in progress. Complete it (resolve, \"load\", \"stack\") or abort \
            it before consolidating.".to_string()
        );
    }

    // The merge is built on the tree this bay holds, and moves the pallet from there.
    position.require_current("consolidated")?;

    let Some(our_head) = position.base.clone() else {
        return Err(format!(
            "Pallet \"{}\" has nothing stacked yet; there is nothing to consolidate into.",
            current
        ));
    };

    let Some(their_head) = pallet_utils::get_pallet_head(&target)? else {
        return Err(pallet_utils::pallet_not_found_refusal(&pallet_utils::PalletRef::user(target)).into());
    };

    let our_tree_hash = object_utils::load_parcel(&our_head)?.tree_hash;

    // Anchored before the cleanliness check, so a file edited after it is never trusted.
    let anchor = file_utils::take_stat_anchor()?;

    ensure_warehouse_is_clean(&our_tree_hash).await?;

    if merge_utils::is_ancestor(&their_head, &our_head)? {
        output::emit(&ConsolidateReport::up_to_date(&current, target));
        return Ok(());
    }

    let their_tree_hash = object_utils::load_parcel(&their_head)?.tree_hash;

    // A hand-made ref could point at an office parcel; its tracked-metadata namespace
    // must never be merged into a working pallet.
    office_utils::ensure_not_metadata_tree(&their_tree_hash)?;

    if merge_utils::is_ancestor(&our_head, &their_head)? {
        let head = fast_forward(&current, &target, &our_head, &our_tree_hash, &their_head, &their_tree_hash, anchor)?;

        output::emit(&ConsolidateReport::fast_forward(&current, target, &head));

        return Ok(());
    }

    // The fast-forward and up-to-date branches above return before this point and are done with
    // the warehouse (fast-forward wholesale-replaces the inventory from a known-good tree, which
    // is one of the recognized self-heals — see `load_guard_utils`'s clearing-scope section — so
    // it must never be blocked); the incomplete-load guard for the genuine-merge path below lives
    // inside `merge_head_into_current` itself, covering every caller (consolidate/haul/lift).
    match merge_head_into_current(&current, &our_head, &their_head, target, true, anchor).await? {
        MergeStatus::Merged(hash) =>
            output::emit(&ConsolidateReport::merged(&current, target, &hash)),
        MergeStatus::Conflicts(conflicts) =>
            output::emit(&ConsolidateReport::conflicts(&current, target, conflicts)),
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
/// directory. Otherwise the merge is applied: a clean one is stacked on `our_head`
/// (`Merged`, see [`stack_completion`]), a conflicting one is left in progress for the operator
/// to resolve (`Conflicts`). `anchor` was taken before the caller's cleanliness check.
///
/// # Returns
/// * `Ok(MergeStatus)` - The outcome.
/// * `Err(String)`     - If the two share no history, or an operation failed.
pub(crate) async fn merge_head_into_current(current: &str,
                                            our_head: &str,
                                            their_head: &str,
                                            their_label: &str,
                                            apply_conflicts: bool,
                                            anchor: file_utils::StatAnchor) -> Result<MergeStatus, String> {
    // This function writes real merge output into the working inventory and only afterwards
    // records that a consolidation is in progress, before ever reaching `stack_utils::stack_parcel`'s
    // own guard. A `load` that started but never finished cleanly leaves a marker
    // (`load_guard_utils`) behind; merging on top of that could silently commit an incomplete
    // staged inventory. Refuses here, before anything is written — every caller (`consolidate`,
    // `haul`'s merge, `lift`'s optimistic auto-merge) routes through this one function, and each
    // caller's fast-forward/up-to-date/already-merged branches return before ever reaching it, so
    // they are correctly not guarded. `stack`/`park` carry the identical check for the identical
    // reason (see `stack_utils::stack_parcel`'s doc comment).
    load_guard_utils::check_no_incomplete_load()?;
    operation_utils::check_none_standing()?;

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

    let mut actions = merge_utils::compute_merge_actions(
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

    keep_aliased_files(&mut actions, &our_tree_hash, &scope)?;
    let ledger = ensure_no_untracked_collisions(&actions, their_label)?;

    // Raised here, before the merge's first change to the bay, so every caller (consolidate,
    // haul's merge, lift's auto-merge) inherits it: a crash part-way through applying leaves a
    // half-applied merge staged with no consolidation state yet to say so.
    raise_merge_marker(OperationKind::Merge, current, our_head, &our_tree_hash, &their_tree_hash)?;

    let mut conflict_paths = apply_merge_actions(&ledger, &actions)?;

    #[cfg(test)]
    tests::AFTER_MERGE_APPLY.with(|seam| seam.take().map(|seam| seam())).transpose()?;

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
        our_head: our_head.to_string(),
        our_pallet: current.to_string(),
    })?;

    if conflict_paths.is_empty() {
        #[cfg(test)]
        tests::BEFORE_MERGE_STACK.with(|seam| seam.take().map(|seam| seam()));

        let description = format!("Consolidated \"{}\" into \"{}\".", their_label, current);
        let hash = stack_completion(Some(description), our_head, &our_tree_hash, anchor).await?;

        Ok(MergeStatus::Merged(hash))
    } else {
        // The consolidation state owns the bay from here.
        operation_utils::finish()?;
        conflict_paths.sort();

        Ok(MergeStatus::Conflicts(conflict_paths))
    }
}

/// Raise the marker of a merge or cherry-pick of `their_tree` onto `our_head` (whose tree is
/// `our_tree`) on `pallet`, the bay's position, before it applies anything. Its completing parcel
/// is recorded by `stack_utils::stack_parcel`, once it exists.
pub(crate) fn raise_merge_marker(kind: OperationKind,
                                 pallet: &str,
                                 our_head: &str,
                                 our_tree: &str,
                                 their_tree: &str) -> Result<(), String> {
    let source = pallet_utils::BayPosition::new(pallet, Some(our_head.to_string()));
    let mut operation = Operation::new(kind, source, Some(our_tree.to_string()), pallet_utils::BayPosition::new(pallet, None));
    operation.their_tree = Some(their_tree.to_string());

    operation_utils::raise(&operation)
}

/// Stack a merge or cherry-pick that completes inside the command that applied it, on
/// `our_head`, the head its output was built on (`our_tree` is that head's tree). Every such
/// command checked the warehouse clean against `our_head` first (`anchor` was taken before that
/// check), and applying the merge overwrites no untracked file, so if the pallet did not end up at
/// the new parcel — another bay or process moved it, or the write failed without landing — the
/// working tree and inventory are put back at `our_tree` and the consolidation or pick is cleared:
/// the command changed nothing, and running it again is safe. A write that landed and failed
/// keeps the parcel, and still fails.
pub(crate) async fn stack_completion(description: Option<String>,
                                     our_head: &str,
                                     our_tree: &str,
                                     anchor: file_utils::StatAnchor) -> Result<String, String> {
    // The merge's or pick's marker comes off once the bay is settled either way; an error before
    // that leaves it for `forklift heal`.
    let (error, pallet, parcel, tree) = match stack_utils::stack_parcel(description, Some(our_head)).await {
        Ok((parcel, _)) => {
            operation_utils::finish()?;
            return Ok(parcel);
        }
        Err(error @ (StackError::Other(_) | StackError::Behind { .. })) => {
            // Refused before its parcel was recorded (a signing failure, say): the
            // merge is applied and its consolidation state written, which owns the bay from here
            // as at conflicts, and `stack` completes it once the cause is fixed. With a parcel
            // recorded, the marker stays for `forklift heal`.
            if operation_utils::read()?.map_or(false, |operation| operation.target.base.is_none()) {
                operation_utils::finish()?;
            }
            return Err(error.into());
        }
        Err(StackError::HeadWrite { error, pallet, parcel, tree }) => (error, pallet, parcel, tree),
    };

    match shift_utils::settle_head_write(&pallet, &parcel, error, &tree, Some(our_tree), anchor)? {
        HeadWrite::Stands => {
            operation_utils::finish()?;
            Ok(parcel)
        }
        HeadWrite::Kept(error) => {
            operation_utils::finish()?;
            Err(error)
        }
        HeadWrite::RolledBack(error) => {
            // The state comes off before the skeleton, as `stack` orders them, so no window shows
            // a consolidation without its skeleton.
            merge_utils::clear_consolidation_state()?;
            merge_utils::OutOfScopeSkeleton::clear()?;
            cherry_pick_utils::clear_state()?;
            operation_utils::finish()?;

            Err(forklift_core::error::append_to_message(
                error,
                "Nothing was changed: the working tree and inventory are back at the pallet's \
                previous head, and nothing is left in progress.",
            ))
        }
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
                our_head: &str,
                our_tree_hash: &str,
                their_head: &str,
                their_tree_hash: &str,
                anchor: file_utils::StatAnchor) -> Result<String, String> {
    let (ops, removed_dirs) = shift_utils::diff_trees(Some(our_tree_hash), their_tree_hash)?;

    alias_utils::refuse_collisions(
        &format!("Consolidating \"{}\"", target),
        &shift_utils::collect_collisions(&ops, &removed_dirs)?,
    )?;

    // Finished by `write_head_or_roll_back` once the bay is settled at one head or the other.
    let mut operation = Operation::new(
        OperationKind::FastForward,
        pallet_utils::BayPosition::new(current, Some(our_head.to_string())),
        Some(our_tree_hash.to_string()),
        pallet_utils::BayPosition::new(current, Some(their_head.to_string())),
    );
    operation.target_tree = Some(their_tree_hash.to_string());
    operation.expected = Some(our_head.to_string());
    operation_utils::raise(&operation)?;

    shift_utils::apply_ops(&ops, &removed_dirs)?;

    let shards = shift_utils::build_inventories_for_tree(their_tree_hash, shift_utils::TreeProvenance::Written(anchor))?.shards;
    inventory_utils::replace_all_inventories(&shards)?;

    #[cfg(test)]
    tests::BEFORE_FAST_FORWARD_WRITE.with(|seam| seam.take().map(|seam| seam())).transpose()?;

    // A compare-and-set against the head this consolidation started from; the bay's work above
    // is undone if the pallet did not move (another bay moved it first, or the write failed).
    shift_utils::write_head_or_roll_back(current, Some(our_head), their_head, &pallet_utils::RefPreconditions::NONE, their_tree_hash, Some(our_tree_hash), anchor)?;

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
/// exist in the working directory — judged by what the file system resolves it to, so a
/// case-only rename on their side is a rename, and a name that lands on another entry through a
/// case alias is refused naming both (see [`alias_utils`]). Shared with `cherry-pick`, which
/// applies the same `MergeAction`s.
///
/// Every action that touches the working directory first has its path checked by
/// `shift_utils::check_planned_paths`, so a path that names a metadata folder, or lies inside
/// another working tree, refuses the whole merge before its first file changes.
///
/// Returns the shard ledger the check judged by, for the apply to reuse: neither writes the
/// metadata before the apply's batch publishes.
pub(crate) fn ensure_no_untracked_collisions(actions: &[MergeAction],
                                             target: &str) -> Result<inventory_utils::ShardLedger, String> {
    shift_utils::check_planned_paths(actions.iter().filter_map(|action| match action {
        MergeAction::TakeTheirs { path, .. }
        | MergeAction::Delete { path }
        | MergeAction::Merged { path, .. }
        | MergeAction::Conflict { path, .. } => Some(path.as_str()),
        // Recorded in the out-of-scope skeleton, never written to the working directory.
        MergeAction::ResolveOutOfScope { .. } => None,
    }))?;

    let removals = MergeRemovals(actions.iter()
        .filter_map(|action| match action {
            MergeAction::Delete { path } => Some(path.clone()),
            _ => None,
        })
        .collect());

    let ledger = inventory_utils::ShardLedger::read()?;
    let mut check = alias_utils::AliasCheck::over(&ledger);
    let mut collisions = Vec::new();
    let mut created = Vec::new();

    for action in actions {
        let new_path = match action {
            MergeAction::TakeTheirs { path, is_new: true, .. } => Some(path),
            // A delete/modify conflict re-creates a file we deleted.
            MergeAction::Conflict { path, .. } if conflict_writes(action) => Some(path),
            _ => None,
        };

        let Some(path) = new_path else { continue };

        // A tracked file cannot collide (tracked paths were verified clean).
        if check.tracked_file(path)? {
            continue;
        }

        created.push(path.as_str());

        match check.judge(path, &removals)? {
            alias_utils::Verdict::Proceed => {}
            alias_utils::Verdict::Refuse(collision) => collisions.push(collision),
            alias_utils::Verdict::Exists => {
                // A tracked directory with no untracked content beneath it cannot collide
                // either — the merge legitimately replaces it with the new entry (see
                // `apply_merge_action`'s deletes-before-writes ordering); a directory is tracked
                // by its own inventory shard, not as an item in its parent's inventory, so it is
                // checked separately.
                let fs_path = std::path::Path::new(path);

                if !(fs_path.is_dir() && inventory_utils::directory_is_safe_to_replace(&ledger, path)?) {
                    collisions.push(alias_utils::Collision::Untracked(path.clone()));
                }
            }
        }
    }

    collisions.extend(check.created_pairs(created, &removals)?);

    alias_utils::refuse_collisions(&format!("Consolidating \"{}\"", target), &collisions)?;

    Ok(ledger)
}

/// The files a merge deletes before it writes (its `Delete` actions).
struct MergeRemovals(std::collections::HashSet<String>);

impl alias_utils::Removals for MergeRemovals {
    fn removes_file(&self, key: &str) -> bool {
        self.0.contains(key)
    }

    /// A Delete removes each directory up its own chain once it is empty (see
    /// `apply_merge_action`), so a directory is removed when some deleted file lies beneath it.
    fn removes_directory(&self, key: &str) -> bool {
        let prefix = format!("{}/", key);
        self.0.iter().any(|path| path.starts_with(&prefix))
    }
}

/// Keep every file our tree holds, unchanged by the merge, that one of its deletions would unlink
/// through an alias: on a folding volume, deleting `readme` where our tree also holds `README`
/// unlinks the one file both name. Each is taken back from our tree, written after the deletions
/// under its own spelling (see [`shift_utils::kept_through_aliases`]). Shared with `cherry-pick`.
pub(crate) fn keep_aliased_files(actions: &mut Vec<MergeAction>,
                                 our_tree_hash: &str,
                                 scope: &scope_utils::MaterializationScope) -> Result<(), String> {
    let removed: Vec<String> = actions.iter()
        .filter_map(|action| match action {
            MergeAction::Delete { path } => Some(path.clone()),
            _ => None,
        })
        .collect();

    if removed.is_empty() {
        return Ok(());
    }

    let our_tree = object_utils::load_tree(our_tree_hash)?;
    let planned: HashSet<&str> = actions.iter().map(|action| match action {
        MergeAction::TakeTheirs { path, .. }
        | MergeAction::Delete { path }
        | MergeAction::Merged { path, .. }
        | MergeAction::Conflict { path, .. }
        | MergeAction::ResolveOutOfScope { path, .. } => path.as_str(),
    }).collect();

    let kept = shift_utils::kept_through_aliases(&our_tree, &removed, &planned, scope)?;

    actions.extend(kept.into_iter().map(|(path, item)| MergeAction::TakeTheirs {
        path,
        hash: item.hash,
        item_type: item.item_type,
        is_new: false,
    }));

    Ok(())
}

/// Whether applying a conflict writes its file: inline content, or a chunked file materialized
/// from its recipe.
fn conflict_writes(action: &MergeAction) -> bool {
    matches!(action, MergeAction::Conflict { content, item_type, .. } if content.is_some() || item_type.is_chunked())
}

/// Apply every merge action to the working directory and the inventory, and return the
/// paths left in conflict. Shared with `cherry-pick`, which applies a parcel's diff through
/// the same `MergeAction`s.
///
/// Every deletion is applied first, so a directory a write is about to replace (or a file a
/// write is about to turn into a directory) is emptied — and, via `apply_merge_action`'s
/// parent-chain cleanup, itself removed — before that write lands. Writing first would
/// otherwise fail (`EISDIR`/`EEXIST`) for a tracked type flip in either direction.
///
/// Every inventory change goes through one [`inventory_utils::ShardMutationBatch`], published
/// before this returns, so the caller records its in-progress state only over a published
/// inventory. An action that fails leaves the batch unpublished: a half-applied merge staged with
/// nothing in progress could leave conflict entries staged with no merge to complete or abort.
/// The caller has recorded nothing as in progress yet, and the error says so.
pub(crate) fn apply_merge_actions(ledger: &inventory_utils::ShardLedger,
                                  actions: &[MergeAction]) -> Result<Vec<String>, String> {
    let mut conflict_paths: Vec<String> = Vec::new();
    let mut batch = inventory_utils::ShardMutationBatch::over(ledger);

    // The batch publishes nothing before `publish`: an action that fails stages nothing, and a
    // publish that fails may have written some shards, or every shard but not the metadata that
    // registers a new one. So either failure claims only that some or all of the changes are
    // unstaged.
    apply_merge_actions_to(ledger, actions, &mut batch, &mut conflict_paths)
        .and_then(|()| batch.publish())
        .map_err(|e| format!(
            "{}\nThe working directory holds the changes applied so far, but some or all of them \
            are not staged. Nothing was recorded as in progress.",
            e
        ))?;

    Ok(conflict_paths)
}

/// The body of [`apply_merge_actions`]: every deletion, then every write, staged into `batch`.
fn apply_merge_actions_to(ledger: &inventory_utils::ShardLedger,
                          actions: &[MergeAction],
                          batch: &mut inventory_utils::ShardMutationBatch,
                          conflict_paths: &mut Vec<String>) -> Result<(), String> {
    // The batch writes nothing before it publishes, so `ledger` stays the registered set throughout.
    for action in actions {
        if matches!(action, MergeAction::Delete { .. }) {
            apply_merge_action(ledger, action, None, batch, conflict_paths)?;
        }
    }

    let writes: Vec<&MergeAction> = actions.iter().filter(|action| !matches!(action, MergeAction::Delete { .. })).collect();

    // Their blobs load ahead on worker threads while the writes land here, in order.
    shift_utils::write_in_order(&writes, |action| match action {
        MergeAction::TakeTheirs { hash, item_type, .. } => Some((hash.as_str(), *item_type)),
        _ => None,
    }, |action, content| apply_merge_action(ledger, action, content, batch, conflict_paths))
}

/// Apply one merge action to the working directory, and stage its inventory change into
/// `batch`.
///
/// `content` is a `TakeTheirs` blob already loaded by [`shift_utils::write_in_order`]'s
/// prefetch, or `None` to load it here. A written file is statted right after its write, as the
/// batch requires.
fn apply_merge_action(ledger: &inventory_utils::ShardLedger,
                      action: &MergeAction,
                      content: Option<Result<Vec<u8>, String>>,
                      batch: &mut inventory_utils::ShardMutationBatch,
                      conflict_paths: &mut Vec<String>) -> Result<(), String> {
    match action {
        MergeAction::TakeTheirs { path, hash, item_type, is_new } => {
            shift_utils::write_tracked_file_from(path, hash, *item_type, *is_new, content)?;
            batch.stage_file_from_stat(path, hash.clone(), *item_type)
        }

        MergeAction::Delete { path } => {
            shift_utils::check_working_path(path)?;

            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("Error while removing \"{}\": {}", path, e)),
            }

            let (parent_key, name) = inventory_utils::split_path_key(path);

            batch.update(parent_key, |inventory| { inventory.remove_item_by_name(name); })?;

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

        MergeAction::Merged { path, content, item_type } => {
            shift_utils::write_working_file(path, content, *item_type, false)?;

            // The merged content is new — the batch stores its blob so the next stack can point
            // at it. A three-way merge only ever runs on plain text files, so a `Merged` result
            // is always a plain blob, never chunked.
            let mut object = LooseObjectBuilder::build_blob(&Blob { content: content.clone() });

            batch.stage_file_storing_blob(path, &mut object, *item_type)
        }

        MergeAction::Conflict { path, content, entry_hash, item_type } => {
            // A conflict on a path the inventory does not hold re-creates a file we deleted, so
            // it is new to the working directory, like `ensure_no_untracked_collisions` judged it.
            let create = conflict_writes(action) && !alias_utils::tracked_file(ledger, path)?;

            if let Some(content) = content {
                shift_utils::write_working_file(path, content, *item_type, create)?;
            } else if item_type.is_chunked() {
                // A chunked (binary) conflict carries no inline content: materialize the
                // should-be-on-disk version from its recipe (`entry_hash` is ours when we keep
                // ours, theirs when theirs is put back). Bounded, verified stream-assembly.
                shift_utils::write_tracked_file(path, entry_hash, *item_type, create)?;
            }

            let (parent_key, name) = inventory_utils::split_path_key(path);

            let mut entry = inventory_utils::build_stale_inventory_item(
                name,
                entry_hash.clone(),
                *item_type
            );
            entry.state = InventoryItemState::FirstParentConflict;

            batch.update(parent_key, |inventory| inventory.add_item(entry))?;

            conflict_paths.push(path.clone());

            Ok(())
        }

        // An out-of-scope entry resolved by hash never touches the working directory or the
        // inventory: it is carried in the out-of-scope skeleton and spliced into the merge
        // parcel's tree by the completing stack's overlay. Nothing to apply here.
        MergeAction::ResolveOutOfScope { .. } => Ok(()),
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<ConsolidateReport>(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self, Warehouse};

    thread_local! {
        /// Runs once just before `fast_forward` writes the pallet's head: an error stands for a
        /// crash there.
        pub(super) static BEFORE_FAST_FORWARD_WRITE: std::cell::Cell<Option<Box<dyn FnOnce() -> Result<(), String>>>> =
            std::cell::Cell::new(None);

        /// Runs once just before a clean merge's parcel is stacked.
        pub(super) static BEFORE_MERGE_STACK: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
            std::cell::Cell::new(None);

        /// Runs once after a merge applied its output, before its consolidation state is written:
        /// an error stands for a crash there.
        pub(crate) static AFTER_MERGE_APPLY: std::cell::Cell<Option<Box<dyn FnOnce() -> Result<(), String>>>> =
            std::cell::Cell::new(None);
    }

    #[test]
    fn the_collision_check_reads_the_ledger_once_however_many_directories_flip_to_files() {
        let warehouse = Warehouse::with_working_tree("consolidate-ledger-reads");
        let flips = ["d1", "d2", "d3"];
        for dir in flips {
            std::fs::create_dir_all(warehouse.root.join(dir)).unwrap();
            std::fs::write(warehouse.root.join(dir).join("f.txt"), b"f\n").unwrap();
        }
        test_support::block_on(crate::commands::load::handle_command(".")).unwrap();

        // Their side replaces each tracked directory with a file of the same name.
        let blob = test_support::store_tree("x", b"x\n");
        let mut actions = Vec::new();
        for dir in flips {
            actions.push(MergeAction::Delete { path: format!("{}/f.txt", dir) });
            actions.push(MergeAction::TakeTheirs {
                path: dir.to_string(),
                hash: blob.clone(),
                item_type: forklift_core::enums::dir_entry_type::DirEntryType::Normal,
                is_new: true,
            });
        }

        let before = inventory_utils::ledger_read_count();
        ensure_no_untracked_collisions(&actions, "theirs")
            .expect("a tracked directory holding only tracked files may become a file");

        assert_eq!(inventory_utils::ledger_read_count() - before, 1,
            "one read serves the check and every directory it judges");
    }

    #[test]
    fn a_fast_forward_whose_pallet_moved_meanwhile_is_refused_and_rolled_back() {
        let warehouse = Warehouse::with_working_tree("consolidate-ff-moved");

        let our_tree = test_support::store_tree("file.txt", b"ours\n");
        let our_head = test_support::store_parcel(&our_tree, &[]);
        let their_tree = test_support::store_tree("file.txt", b"theirs\n");
        let their_head = test_support::store_parcel(&their_tree, &[&our_head]);
        let elsewhere = test_support::store_parcel(&test_support::store_tree("file.txt", b"elsewhere\n"), &[&our_head]);

        pallet_utils::update_pallet_head("main", None, &our_head, &pallet_utils::RefPreconditions::NONE).unwrap();
        pallet_utils::write_position(&pallet_utils::BayPosition::new("main", Some(our_head.clone()))).unwrap();
        pallet_utils::update_pallet_head("feature", None, &their_head, &pallet_utils::RefPreconditions::NONE).unwrap();
        test_support::check_out(&our_tree);

        // Another bay stacks on "main" while this consolidation is between its tree write and
        // its ref write.
        let moved_to = elsewhere.clone();
        BEFORE_FAST_FORWARD_WRITE.with(|seam| seam.set(Some(Box::new(move || {
            pallet_utils::update_pallet_head("main", Some(&our_head), &moved_to, &pallet_utils::RefPreconditions::NONE).unwrap();
            Ok(())
        }))));

        let error = test_support::block_on(handle_command("feature"))
            .expect_err("a fast-forward over a moved pallet must be refused");
        assert!(error.contains("moved"), "{}", error);

        assert_eq!(pallet_utils::get_pallet_head("main").unwrap(), Some(elsewhere),
            "the other bay's head must stand");
        assert!(test_support::file_holds(&warehouse.root, "file.txt", b"ours\n"),
            "the working tree must be back at the head this consolidation started from");

        let staged = test_support::block_on(stocktake_utils::collect_staged_changes(Some(&our_tree))).unwrap();
        let unstaged = test_support::block_on(stocktake_utils::collect_unstaged_changes()).unwrap();
        assert!(staged.is_empty() && unstaged.is_empty(),
            "the inventory must be back at our tree: staged {:?}, unstaged {:?}",
            staged.len(), unstaged.len());
    }

    #[test]
    fn a_clean_merge_whose_pallet_moved_meanwhile_is_refused_and_rolled_back() {
        let warehouse = Warehouse::with_working_tree("consolidate-merge-moved");
        test_support::configure_operator();

        let base = test_support::store_parcel(&test_support::store_files(&[("a.txt", b"base\n"), ("b.txt", b"base\n")]), &[]);
        let our_tree = test_support::store_files(&[("a.txt", b"ours\n"), ("b.txt", b"base\n")]);
        let our_head = test_support::store_parcel(&our_tree, &[&base]);
        let their_head = test_support::store_parcel(
            &test_support::store_files(&[("a.txt", b"base\n"), ("b.txt", b"theirs\n"), ("new.txt", b"new\n")]),
            &[&base],
        );
        let elsewhere = test_support::store_parcel(&test_support::store_tree("a.txt", b"elsewhere\n"), &[&our_head]);

        pallet_utils::update_pallet_head("main", None, &our_head, &pallet_utils::RefPreconditions::NONE).unwrap();
        pallet_utils::write_position(&pallet_utils::BayPosition::new("main", Some(our_head.clone()))).unwrap();
        pallet_utils::update_pallet_head("feature", None, &their_head, &pallet_utils::RefPreconditions::NONE).unwrap();
        test_support::check_out(&our_tree);

        // Another bay stacks on "main" after this merge was applied, before its parcel is stacked.
        let moved_to = elsewhere.clone();
        BEFORE_MERGE_STACK.with(|seam| seam.set(Some(Box::new(move || {
            pallet_utils::update_pallet_head("main", Some(&our_head), &moved_to, &pallet_utils::RefPreconditions::NONE).unwrap();
        }))));

        let error = test_support::block_on(handle_command("feature"))
            .expect_err("a merge over a moved pallet must be refused");
        assert!(error.contains("moved") && error.contains("Nothing was changed"), "{}", error);

        assert_eq!(pallet_utils::get_pallet_head("main").unwrap(), Some(elsewhere),
            "the other bay's parcel must stay the head");
        assert!(test_support::file_holds(&warehouse.root, "a.txt", b"ours\n")
                && test_support::file_holds(&warehouse.root, "b.txt", b"base\n")
                && !warehouse.root.join("new.txt").exists(),
            "the working tree must be back at the head this merge started from");

        let staged = test_support::block_on(stocktake_utils::collect_staged_changes(Some(&our_tree))).unwrap();
        let unstaged = test_support::block_on(stocktake_utils::collect_unstaged_changes()).unwrap();
        assert!(staged.is_empty() && unstaged.is_empty(),
            "the inventory must be back at our tree: staged {:?}, unstaged {:?}",
            staged.len(), unstaged.len());

        assert!(merge_utils::read_consolidation_state().unwrap().is_none(), "no consolidation may stay in progress");
        assert!(merge_utils::OutOfScopeSkeleton::read_required().is_err(), "no skeleton may stay behind");
    }

    use forklift_core::util::operation_utils::RecoveryOutcome;

    /// The two heads of a clean three-way merge: "main" (checked out) and "feature". Returns
    /// (our_head, our_tree, their_head).
    fn a_clean_merge_on_main() -> (String, String, String) {
        test_support::configure_operator();

        let base = test_support::store_parcel(&test_support::store_files(&[("a.txt", b"base\n"), ("b.txt", b"base\n")]), &[]);
        let our_tree = test_support::store_files(&[("a.txt", b"ours\n"), ("b.txt", b"base\n")]);
        let our_head = test_support::store_parcel(&our_tree, &[&base]);
        let their_head = test_support::store_parcel(
            &test_support::store_files(&[("a.txt", b"base\n"), ("b.txt", b"theirs\n"), ("new.txt", b"new\n")]),
            &[&base],
        );

        pallet_utils::update_pallet_head("main", None, &our_head, &pallet_utils::RefPreconditions::NONE).unwrap();
        pallet_utils::write_position(&pallet_utils::BayPosition::new("main", Some(our_head.clone()))).unwrap();
        pallet_utils::update_pallet_head("feature", None, &their_head, &pallet_utils::RefPreconditions::NONE).unwrap();
        test_support::check_out(&our_tree);

        (our_head, our_tree, their_head)
    }

    fn assert_back_at_ours(root: &std::path::Path, our_head: &str, our_tree: &str) {
        assert!(test_support::file_holds(root, "a.txt", b"ours\n")
                && test_support::file_holds(root, "b.txt", b"base\n")
                && !root.join("new.txt").exists(),
            "the working tree must be back at the head the merge started from");

        let staged = test_support::block_on(stocktake_utils::collect_staged_changes(Some(our_tree))).unwrap();
        let unstaged = test_support::block_on(stocktake_utils::collect_unstaged_changes()).unwrap();
        assert!(staged.is_empty() && unstaged.is_empty(),
            "the inventory must be back at our tree: staged {:?}, unstaged {:?}", staged.len(), unstaged.len());

        assert_eq!(pallet_utils::read_position().unwrap(), pallet_utils::BayPosition::new("main", Some(our_head.to_string())));
        assert!(merge_utils::read_consolidation_state().unwrap().is_none(), "no consolidation may stay in progress");
        assert!(operation_utils::read().unwrap().is_none(), "recovery clears the marker");
    }

    fn assert_stack_refused() {
        let error = test_support::block_on(crate::commands::stack::handle_command(Some("after crash".to_string())))
            .expect_err("a stack over an interrupted merge must be refused");
        assert!(error.contains("forklift heal"), "{}", error);
    }

    #[test]
    fn a_merge_interrupted_part_way_through_applying_cannot_be_stacked_and_heal_rolls_it_back() {
        let warehouse = Warehouse::with_working_tree("consolidate-merge-crash-apply");
        let (our_head, our_tree, _) = a_clean_merge_on_main();

        AFTER_MERGE_APPLY.with(|seam| seam.set(Some(Box::new(|| Err("crashed".to_string())))));
        assert_eq!(test_support::block_on(handle_command("feature")).unwrap_err(), "crashed");
        operation_utils::forget_raised_for_tests();

        // The merge's output is applied and staged, with no consolidation state to say so.
        assert!(test_support::file_holds(&warehouse.root, "new.txt", b"new\n"));
        assert_stack_refused();
        assert_eq!(pallet_utils::get_pallet_head("main").unwrap().as_deref(), Some(our_head.as_str()));

        let recovery = operation_utils::recover().unwrap().expect("a marker stands");
        assert_eq!((recovery.operation, recovery.outcome), (OperationKind::Merge, RecoveryOutcome::RolledBack));
        assert_back_at_ours(&warehouse.root, &our_head, &our_tree);
    }

    #[test]
    fn rolling_back_an_interrupted_merge_keeps_an_edit_made_since_the_crash() {
        let warehouse = Warehouse::with_working_tree("consolidate-merge-crash-edit");
        let (our_head, our_tree, _) = a_clean_merge_on_main();

        AFTER_MERGE_APPLY.with(|seam| seam.set(Some(Box::new(|| Err("crashed".to_string())))));
        assert_eq!(test_support::block_on(handle_command("feature")).unwrap_err(), "crashed");
        operation_utils::forget_raised_for_tests();

        // The merge wrote their b.txt and added new.txt; the user then wrote over both, before
        // running heal.
        std::fs::write(warehouse.root.join("b.txt"), b"notes\n").unwrap();
        std::fs::write(warehouse.root.join("new.txt"), b"mine\n").unwrap();

        let recovery = operation_utils::recover().unwrap().expect("a marker stands");
        assert_eq!(recovery.outcome, RecoveryOutcome::RolledBack);
        assert_eq!(recovery.unheld, vec!["b.txt".to_string()], "the edit is reported, not overwritten");
        assert!(test_support::file_holds(&warehouse.root, "b.txt", b"notes\n"));
        assert!(test_support::file_holds(&warehouse.root, "new.txt", b"mine\n"), "an edited addition is not removed");
        assert!(test_support::file_holds(&warehouse.root, "a.txt", b"ours\n"), "a file the merge left as it wrote it is put back");
        assert_eq!(pallet_utils::read_position().unwrap(), pallet_utils::BayPosition::new("main", Some(our_head)));
        let staged = test_support::block_on(stocktake_utils::collect_staged_changes(Some(&our_tree))).unwrap();
        assert!(staged.is_empty(), "staging is at our tree");
    }

    #[cfg(unix)]
    #[test]
    fn a_merge_whose_rollback_fails_too_cannot_be_stacked_and_heal_finishes_the_rollback() {
        use std::os::unix::fs::PermissionsExt;

        let warehouse = Warehouse::with_working_tree("consolidate-merge-rollback-fails");
        let (our_head, our_tree, _) = a_clean_merge_on_main();
        let elsewhere = test_support::store_parcel(&test_support::store_tree("a.txt", b"elsewhere\n"), &[&our_head]);

        // Another bay stacks first, so the head write fails; and the working directory refuses
        // the rollback's writes, so the rollback fails too.
        let (moved_to, root) = (elsewhere.clone(), warehouse.root.clone());
        let from = our_head.clone();
        BEFORE_MERGE_STACK.with(|seam| seam.set(Some(Box::new(move || {
            pallet_utils::update_pallet_head("main", Some(&from), &moved_to, &pallet_utils::RefPreconditions::NONE).unwrap();
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).unwrap();
        }))));

        let result = test_support::block_on(handle_command("feature"));
        std::fs::set_permissions(&warehouse.root, std::fs::Permissions::from_mode(0o755)).unwrap();
        result.expect_err("a merge whose rollback failed must not report success");
        operation_utils::forget_raised_for_tests();

        assert!(operation_utils::read().unwrap().expect("the marker stands").rolling_back,
            "the marker records that the rollback began");
        assert_stack_refused();

        let recovery = operation_utils::recover().unwrap().expect("a marker stands");
        assert_eq!(recovery.outcome, RecoveryOutcome::RolledBack);
        assert_eq!(pallet_utils::get_pallet_head("main").unwrap(), Some(elsewhere), "the other bay's parcel stands");
        assert_back_at_ours(&warehouse.root, &our_head, &our_tree);
    }

    #[cfg(unix)]
    #[test]
    fn a_merge_whose_stack_fails_before_recording_its_parcel_leaves_it_to_its_consolidation_state() {
        use std::os::unix::fs::PermissionsExt;

        let _warehouse = Warehouse::with_working_tree("consolidate-merge-stack-fails");
        a_clean_merge_on_main();

        // Every object directory refuses writes, so the stack fails before its parcel exists.
        fn set_dirs(dir: &std::path::Path, mode: u32) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() { set_dirs(&path, mode); }
            }
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let objects = std::path::PathBuf::from(file_utils::get_path_objects_root());
        let locked = objects.clone();
        BEFORE_MERGE_STACK.with(|seam| seam.set(Some(Box::new(move || set_dirs(&locked, 0o555)))));

        let result = test_support::block_on(handle_command("feature"));
        set_dirs(&objects, 0o755);
        result.expect_err("a merge whose stack failed must not report success");
        operation_utils::forget_raised_for_tests();

        // Its consolidation state owns the bay now, as at conflicts: `stack` completes it.
        assert!(operation_utils::read().unwrap().is_none(), "no interrupted operation stands");
        assert!(merge_utils::read_consolidation_state().unwrap().is_some());
        let (parcel, _) = test_support::block_on(stack_utils::stack_parcel(None, None)).expect("stack completes the merge");
        assert_eq!(pallet_utils::get_pallet_head("main").unwrap(), Some(parcel));
    }

    /// A fast-forward of "main" (checked out) to "feature", crashed just before its head write;
    /// `move_main` runs first, standing for another bay. Returns (our_head, our_tree, their_head).
    fn crash_a_fast_forward(move_main: impl FnOnce(&str) + 'static) -> (String, String, String) {
        let our_tree = test_support::store_tree("file.txt", b"ours\n");
        let our_head = test_support::store_parcel(&our_tree, &[]);
        let their_head = test_support::store_parcel(&test_support::store_tree("file.txt", b"theirs\n"), &[&our_head]);

        pallet_utils::update_pallet_head("main", None, &our_head, &pallet_utils::RefPreconditions::NONE).unwrap();
        pallet_utils::write_position(&pallet_utils::BayPosition::new("main", Some(our_head.clone()))).unwrap();
        pallet_utils::update_pallet_head("feature", None, &their_head, &pallet_utils::RefPreconditions::NONE).unwrap();
        test_support::check_out(&our_tree);

        let from = our_head.clone();
        BEFORE_FAST_FORWARD_WRITE.with(|seam| seam.set(Some(Box::new(move || {
            move_main(&from);
            Err("crashed".to_string())
        }))));
        assert_eq!(test_support::block_on(handle_command("feature")).unwrap_err(), "crashed");
        operation_utils::forget_raised_for_tests();

        (our_head, our_tree, their_head)
    }

    #[test]
    fn an_interrupted_fast_forward_whose_pallet_did_not_move_is_rolled_forward() {
        let warehouse = Warehouse::with_working_tree("consolidate-ff-crash-forward");
        let (_, _, their_head) = crash_a_fast_forward(|_| {});
        let their_tree = object_utils::load_parcel(&their_head).unwrap().tree_hash;

        let recovery = operation_utils::recover().unwrap().expect("a marker stands");
        assert_eq!((recovery.operation, recovery.outcome), (OperationKind::FastForward, RecoveryOutcome::RolledForward));
        assert_eq!(pallet_utils::get_pallet_head("main").unwrap().as_deref(), Some(their_head.as_str()));
        assert_eq!(recovery.position, pallet_utils::BayPosition::new("main", Some(their_head)));
        assert!(test_support::file_holds(&warehouse.root, "file.txt", b"theirs\n"));

        let staged = test_support::block_on(stocktake_utils::collect_staged_changes(Some(&their_tree))).unwrap();
        assert!(staged.is_empty(), "staging is at their tree");
    }

    #[test]
    fn an_interrupted_fast_forward_whose_pallet_moved_elsewhere_is_rolled_back_to_its_start() {
        let warehouse = Warehouse::with_working_tree("consolidate-ff-crash-back");
        let elsewhere = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let record = elsewhere.clone();
        let (our_head, our_tree, _) = crash_a_fast_forward(move |from| {
            let moved_to = test_support::store_parcel(&test_support::store_tree("file.txt", b"elsewhere\n"), &[from]);
            pallet_utils::update_pallet_head("main", Some(from), &moved_to, &pallet_utils::RefPreconditions::NONE).unwrap();
            *record.borrow_mut() = moved_to;
        });

        let recovery = operation_utils::recover().unwrap().expect("a marker stands");
        assert_eq!(recovery.outcome, RecoveryOutcome::RolledBack);
        assert_eq!(pallet_utils::get_pallet_head("main").unwrap(), Some(elsewhere.borrow().clone()));
        assert!(test_support::file_holds(&warehouse.root, "file.txt", b"ours\n"));
        assert_eq!(recovery.position, pallet_utils::BayPosition::new("main", Some(our_head)));

        let staged = test_support::block_on(stocktake_utils::collect_staged_changes(Some(&our_tree))).unwrap();
        let unstaged = test_support::block_on(stocktake_utils::collect_unstaged_changes()).unwrap();
        assert!(staged.is_empty() && unstaged.is_empty());
    }

    #[test]
    fn an_operation_that_landed_and_was_built_on_is_finished_without_touching_a_file() {
        for kind in [OperationKind::Merge, OperationKind::FastForward] {
            let warehouse = Warehouse::with_working_tree("consolidate-landed-built-on");
            let (our_head, our_tree, _) = a_clean_merge_on_main();

            // It landed (its parcel is the head and the bay's position), and was killed before its
            // marker came off; then another bay stacked on top.
            let landed = test_support::store_parcel(&our_tree, &[&our_head]);
            let mut operation = Operation::new(
                kind,
                pallet_utils::BayPosition::new("main", Some(our_head.clone())),
                Some(our_tree.clone()),
                pallet_utils::BayPosition::new("main", Some(landed.clone())),
            );
            operation.target_tree = Some(our_tree.clone());
            operation.expected = Some(our_head.clone());
            operation_utils::raise(&operation).unwrap();
            pallet_utils::update_pallet_head("main", Some(&our_head), &landed, &pallet_utils::RefPreconditions::NONE).unwrap();
            pallet_utils::write_position(&pallet_utils::BayPosition::new("main", Some(landed.clone()))).unwrap();
            operation_utils::forget_raised_for_tests();

            let later = test_support::store_parcel(&our_tree, &[&landed]);
            pallet_utils::update_pallet_head("main", Some(&landed), &later, &pallet_utils::RefPreconditions::NONE).unwrap();

            // Work the bay holds since: recovery must leave it alone.
            std::fs::write(warehouse.root.join("a.txt"), b"since\n").unwrap();

            let recovery = operation_utils::recover().unwrap().expect("a marker stands");
            assert_eq!(recovery.outcome, RecoveryOutcome::Finished, "{:?}", kind);
            assert_eq!(recovery.position, pallet_utils::BayPosition::new("main", Some(landed)));
            assert_eq!(pallet_utils::get_pallet_head("main").unwrap(), Some(later));
            assert!(test_support::file_holds(&warehouse.root, "a.txt", b"since\n"), "{:?}", kind);

            drop(warehouse);
        }
    }
}
