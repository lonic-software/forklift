use serde::Serialize;
use forklift_core::util::stocktake_utils::ChangeKind;
use forklift_core::util::{alias_utils, cherry_pick_utils, file_utils, inventory_utils, merge_utils, object_utils, pallet_utils, shift_utils, stocktake_utils};
use crate::output::{self, CommandOutput};

/// Handle the shift command (git's "checkout"): materialize the head tree of the target
/// pallet in the working directory, repopulate the inventory from it (the dock's
/// "repopulate on shift" behavior), and make the target the current pallet.
///
/// Shifting refuses to run when there are staged or unstaged changes (they would be
/// overwritten), or while a consolidation or cherry-pick is in progress. Untracked files are
/// tolerated, unless the target wants to write one of their paths.
///
/// Shifting to the current pallet catches up with it instead (see [`catch_up`]).
///
/// # Arguments
/// * `target` - The name of the pallet to shift to.
///
/// # Returns
/// * `Ok(())`      - If the shift completed successfully.
/// * `Err(String)` - If there was an error while handling the command.
pub async fn handle_command(target: &str) -> Result<(), String> {
    let position = pallet_utils::read_position()?;

    if target == position.pallet {
        output::emit(&catch_up(position).await?);
        return Ok(());
    }

    let head = shift_to(target).await?;

    output::emit(&Shifted { pallet: target.to_string(), head });

    Ok(())
}

/// The pallet a `shift` moved to and its head.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub struct Shifted {
    pub pallet: String,
    pub head: String,
}

impl CommandOutput for Shifted {
    fn render_human(&self) {
        println!("Shifted to pallet \"{}\" (head {}).", self.pallet, self.head);
    }
}

/// How a bay caught up with its pallet (see [`catch_up`]).
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize, Debug, PartialEq)]
pub struct CaughtUp {
    pub pallet: String,

    /// The parcel the bay's files stood on before (empty for an unborn pallet).
    pub from: String,

    /// The pallet's head, which the bay now stands on (empty for an unborn pallet).
    pub head: String,

    /// Whether the working directory and inventory were moved to the head's tree. False when the
    /// bay already stood on the head, or its files already matched the head's tree.
    pub moved_files: bool,
}

impl CommandOutput for CaughtUp {
    fn render_human(&self) {
        if self.from == self.head {
            println!("Already at the head of pallet \"{}\" (parcel {}).", self.pallet, self.head);
        } else if self.moved_files {
            println!("Caught up with pallet \"{}\": moved the files from parcel {} to its head, {}.", self.pallet, self.from, self.head);
        } else {
            println!("Caught up with pallet \"{}\" (head {}): the files already matched it.", self.pallet, self.head);
        }
    }
}

/// Catch the bay up with its own pallet, which another bay or process may have moved since the
/// bay's files were last materialized. Refused while a consolidation or cherry-pick is in progress,
/// since it was built on the base this would move. Then, by what the files hold:
///
/// * the base is the head: nothing to do;
/// * clean against the base: the files move from the base's tree to the head's, as a shift does —
///   a general tree move, since another bay's undo can leave the head behind or beside the base;
/// * clean against the head (a stack or undo interrupted between its writes, or files that already
///   reached the head another way): only the position is written, and no file is touched;
/// * neither: refused, naming the park route, which keeps changes that do not overlap the
///   pallet's new parcels.
///
/// Never journaled: it moves no ref, and undoing it could only put back a base the pallet has
/// moved past.
pub async fn catch_up(position: pallet_utils::BayPosition) -> Result<CaughtUp, String> {
    refuse_mid_merge("catching up")?;

    let pallet = position.pallet.clone();
    let head = position.head()?;
    let caught_up = |moved_files: bool| CaughtUp {
        pallet: pallet.clone(),
        from: position.base.clone().unwrap_or_default(),
        head: head.clone().unwrap_or_default(),
        moved_files,
    };

    if head == position.base {
        return Ok(caught_up(false));
    }

    let Some(head_hash) = &head else {
        return Err(format!(
            "Pallet \"{}\" has no head, but this bay's files stand on parcel {}. Nothing was changed.",
            pallet, position.base.as_deref().unwrap_or_default()
        ));
    };

    let base_tree = match &position.base {
        Some(base) => Some(object_utils::load_parcel(base)?.tree_hash),
        None => None,
    };
    let head_tree = object_utils::load_parcel(head_hash)?.tree_hash;

    // Anchored before the cleanliness check, so a file edited after it is never trusted.
    let anchor = file_utils::take_stat_anchor()?;

    if !has_local_changes(base_tree.as_deref()).await? {
        let (ops, removed_dirs) = shift_utils::diff_trees(base_tree.as_deref(), &head_tree)?;

        alias_utils::refuse_collisions(
            &format!("Catching up with \"{}\"", pallet),
            &shift_utils::collect_collisions(&ops, &removed_dirs)?,
        )?;

        shift_utils::apply_ops(&ops, &removed_dirs)?;

        let shards = shift_utils::build_inventories_for_tree(&head_tree, shift_utils::TreeProvenance::Written(anchor))?;
        inventory_utils::replace_all_inventories(&shards)?;

        pallet_utils::write_position(&pallet_utils::BayPosition::new(&pallet, head.clone()))?;

        return Ok(caught_up(true));
    }

    if !has_local_changes(Some(&head_tree)).await? {
        pallet_utils::write_position(&pallet_utils::BayPosition::new(&pallet, head.clone()))?;

        return Ok(caught_up(false));
    }

    Err(format!(
        "This bay has local changes, and pallet \"{}\" moved since its files were materialized \
        (from parcel {} to {}), so catching up would overwrite them. Nothing was changed. Park \
        them (\"forklift park\"), run \"forklift shift {}\" again, then \"forklift park pop\". That \
        brings back only changes to paths the pallet's new parcels left alone; for changes that \
        overlap them, palletize a new pallet here, stack them on it, and consolidate the two.",
        pallet, position.base.as_deref().unwrap_or("none"), head_hash, pallet
    ))
}

/// Refuse a switch of the current pallet while a consolidation or cherry-pick is in progress: it
/// is completed on the pallet it started on (`stack` refuses it anywhere else).
///
/// # Arguments
/// * `doing` - The switch, as a gerund for the message (e.g. "shifting").
pub(crate) fn refuse_mid_merge(doing: &str) -> Result<(), String> {
    if merge_utils::read_consolidation_state()?.is_some() {
        return Err(format!(
            "A consolidation is in progress. Complete it (resolve, \"load\", \"stack\") or abort it \
            ({}) before {}.",
            merge_utils::abort_hint_in(&forklift_core::globals::bay_root()),
            doing
        ));
    }

    if cherry_pick_utils::read_state()?.is_some() {
        return Err(format!(
            "A cherry-pick is in progress. Resolve its conflicts and \"stack\", or {} to abort it, \
            before {}.",
            cherry_pick_utils::abort_hint(),
            doing
        ));
    }

    Ok(())
}

/// Shift to the given pallet: clean-check, materialize its head tree, repopulate the
/// inventory, and make it the current pallet. Also used by `palletize` when a new pallet
/// is created at a parcel other than the current head.
///
/// # Arguments
/// * `target` - The name of the pallet to shift to. Must exist (have a ref file).
///
/// # Returns
/// * `Ok(String)`  - The head parcel of the pallet shifted to (so callers can report
///                   it; this function never prints — the caller owns presentation).
/// * `Err(String)` - If the pallet does not exist, the warehouse is dirty, or an
///                   operation failed.
pub async fn shift_to(target: &str) -> Result<String, String> {
    pallet_utils::validate_pallet_name(target)?;

    refuse_mid_merge("shifting")?;

    let Some(target_head) = pallet_utils::get_pallet_head(target)? else {
        return Err(format!(
            "No pallet named \"{}\" exists. Use the \"palletize\" command to create it.",
            target
        ));
    };

    // The files move from the tree this bay holds, its base, whatever the pallet's ref says now.
    let current_head = pallet_utils::read_position()?.base;

    let current_tree_hash = match &current_head {
        Some(hash) => Some(object_utils::load_parcel(hash)?.tree_hash),
        None => None,
    };
    let target_tree_hash = object_utils::load_parcel(&target_head)?.tree_hash;

    // Anchored before the cleanliness check, so a file edited after it is never trusted.
    let anchor = file_utils::take_stat_anchor()?;

    ensure_warehouse_is_clean(current_tree_hash.as_deref()).await?;

    let (ops, removed_dirs) = shift_utils::diff_trees(
        current_tree_hash.as_deref(),
        &target_tree_hash
    )?;

    // New files must never overwrite untracked content, nor land on another entry through a
    // case alias. This is checked up front, before anything is touched, so a conflict aborts
    // the shift with the warehouse unchanged.
    alias_utils::refuse_collisions(
        &format!("Shifting to \"{}\"", target),
        &shift_utils::collect_collisions(&ops, &removed_dirs)?,
    )?;

    shift_utils::apply_ops(&ops, &removed_dirs)?;

    // Repopulate the staging area from the target tree ("repopulate on shift"): entries
    // carry the just-materialized files' stat data and the hashes from the tree, so the
    // whole warehouse is "clean" without reading a single file back.
    let shards = shift_utils::build_inventories_for_tree(&target_tree_hash, shift_utils::TreeProvenance::Written(anchor))?;
    inventory_utils::replace_all_inventories(&shards)?;

    pallet_utils::write_position(&pallet_utils::BayPosition::new(target, Some(target_head.clone())))?;

    Ok(target_head)
}

/// Ensure there are no staged or unstaged changes (untracked files are allowed).
///
/// # Arguments
/// * `current_tree_hash` - The tree hash of the current pallet's head (or `None`).
///
/// # Returns
/// * `Ok(())`      - If the warehouse is clean.
/// * `Err(String)` - If there are changes that a shift would overwrite.
async fn ensure_warehouse_is_clean(current_tree_hash: Option<&str>) -> Result<(), String> {
    if !has_local_changes(current_tree_hash).await? {
        return Ok(());
    }

    Err(
        "There are local changes that shifting would overwrite. Stack them, restore them, \
        or park them first (see \"stocktake\" for the details).".to_string()
    )
}


/// Whether the bay has staged changes against `tree_hash` (`None` for the empty tree) or unstaged
/// ones (untracked files do not count).
async fn has_local_changes(tree_hash: Option<&str>) -> Result<bool, String> {
    let staged = stocktake_utils::collect_staged_changes(tree_hash).await?;
    let unstaged = stocktake_utils::collect_unstaged_changes().await?
        .into_iter()
        .any(|change| change.kind != ChangeKind::Untracked);

    Ok(!staged.is_empty() || unstaged)
}

/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<Shifted>(),
        crate::docgen::OutputSchema::of::<CaughtUp>(),
    ]
}
