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
    // A merge in a scoped bay resolves out-of-scope siblings by hash (§7.6): a one-sided change
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
#[derive(Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ConsolidateOutcome {
    UpToDate,
    FastForward,
    Merged,
    Conflicts,
}

/// The result of a consolidate.
#[derive(Serialize)]
struct ConsolidateReport {
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

    let mut conflict_paths: Vec<String> = Vec::new();

    for action in &actions {
        apply_merge_action(action, &mut conflict_paths)?;
    }

    // Record the out-of-scope skeleton (§7.6) BEFORE the consolidation state, and unconditionally
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

    let conflicts: Vec<&String> = ops.iter()
        .filter_map(|op| match op {
            shift_utils::FileOp::Write { path, is_new: true, .. }
                if std::path::Path::new(path).exists() => Some(path),
            _ => None,
        })
        .collect();

    if !conflicts.is_empty() {
        return Err(format!(
            "Consolidating \"{}\" would overwrite these untracked files:\n  {}\n\
            Move them out of the way (or load and stack them) first.",
            target,
            conflicts.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n  ")
        ));
    }

    for op in &ops {
        shift_utils::apply_file_op(op)?;
    }

    shift_utils::remove_empty_directories(&removed_dirs);

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
        let new_path = match action {
            MergeAction::TakeTheirs { path, is_new: true, .. } => Some(path),
            // A delete/modify conflict re-creates a file we deleted.
            MergeAction::Conflict { path, content: Some(_), .. }
                if !std::path::Path::new(path).exists() => None,
            MergeAction::Conflict { path, content: Some(_), .. } => Some(path),
            _ => None,
        };

        if let Some(path) = new_path {
            // Only paths we do not track can collide; tracked paths were verified clean.
            let is_tracked = inventory_lookup(path)?.is_some();

            if !is_tracked && std::path::Path::new(path).exists() {
                collisions.push(path);
            }
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

/// Apply one merge action to the working directory and the inventory. Shared with
/// `cherry-pick`, which applies a parcel's diff through the same `MergeAction`s.
pub(crate) fn apply_merge_action(action: &MergeAction, conflict_paths: &mut Vec<String>) -> Result<(), String> {
    match action {
        MergeAction::TakeTheirs { path, hash, item_type, .. } => {
            shift_utils::write_tracked_file(path, hash, *item_type)?;
            inventory_utils::stage_file_entry_from_stat(path, hash.clone())
        }

        MergeAction::Delete { path } => {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("Error while removing \"{}\": {}", path, e)),
            }

            let (parent_key, name) = split_path(path);

            inventory_utils::update_shard(parent_key, |inventory| {
                inventory.remove_item_by_name(name);
                Ok(())
            })?;

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
            write_merged_file(path, content, *item_type)?;

            // The merged content is new — store its blob so the next stack can point at it.
            let mut object = LooseObjectBuilder::build_blob(&Blob { content: content.clone() });
            object.store()?;

            inventory_utils::stage_file_entry_from_stat(path, object.hash)
        }

        MergeAction::Conflict { path, content, entry_hash, item_type } => {
            if let Some(content) = content {
                write_merged_file(path, content, *item_type)?;
            }

            let (parent_key, name) = split_path(path);

            let mut entry = inventory_utils::build_stale_inventory_item(
                name,
                entry_hash.clone(),
                *item_type
            );
            entry.state = InventoryItemState::FirstParentConflict;

            inventory_utils::update_shard(parent_key, |inventory| {
                inventory.add_item(entry);
                Ok(())
            })?;

            conflict_paths.push(path.clone());

            Ok(())
        }

        // An out-of-scope entry resolved by hash never touches the working directory or the
        // inventory (§7.6): it is carried in the out-of-scope skeleton and spliced into the merge
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
