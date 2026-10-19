use serde::Serialize;
use forklift_core::util::stocktake_utils::ChangeKind;
use forklift_core::util::{alias_utils, cherry_pick_utils, file_utils, inventory_utils, merge_utils, object_utils, pallet_utils, shift_utils, stocktake_utils};
use crate::output::{self, CommandOutput};

/// Handle the shift command (git's "checkout"): materialize the head tree of the target
/// pallet in the working directory, repopulate the inventory from it (the dock's
/// "repopulate on shift" behavior), and make the target the current pallet.
///
/// Shifting refuses to run when there are staged or unstaged changes (they would be
/// overwritten), or while a consolidation or cherry-pick is in progress. Untracked files are tolerated, unless the target wants to write one of
/// their paths.
///
/// # Arguments
/// * `target` - The name of the pallet to shift to.
///
/// # Returns
/// * `Ok(())`      - If the shift completed successfully.
/// * `Err(String)` - If there was an error while handling the command.
pub async fn handle_command(target: &str) -> Result<(), String> {
    let current = pallet_utils::get_current_pallet_name()?;

    if target == current {
        return Err(format!("Already on pallet \"{}\".", target));
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

    // An in-progress consolidation or cherry-pick is completed on the head it recorded, which
    // belongs to this pallet: completing it from another one would be refused as a move.
    if merge_utils::read_consolidation_state()?.is_some() {
        return Err(format!(
            "A consolidation is in progress. Complete it (resolve, \"load\", \"stack\") or abort it \
            ({}) before shifting.",
            merge_utils::abort_hint_in(&forklift_core::globals::bay_root())
        ));
    }

    if cherry_pick_utils::read_state()?.is_some() {
        return Err(format!(
            "A cherry-pick is in progress. Resolve its conflicts and \"stack\", or {} to abort it, \
            before shifting.",
            cherry_pick_utils::abort_hint()
        ));
    }

    let Some(target_head) = pallet_utils::get_pallet_head(target)? else {
        return Err(format!(
            "No pallet named \"{}\" exists. Use the \"palletize\" command to create it.",
            target
        ));
    };

    let current = pallet_utils::get_current_pallet_name()?;

    let current_head = pallet_utils::get_pallet_head(&current)?;

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

    pallet_utils::set_current_pallet_name(target)?;

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
    let staged = stocktake_utils::collect_staged_changes(current_tree_hash).await?;
    let unstaged: Vec<_> = stocktake_utils::collect_unstaged_changes().await?
        .into_iter()
        .filter(|change| change.kind != ChangeKind::Untracked)
        .collect();

    if staged.is_empty() && unstaged.is_empty() {
        return Ok(());
    }

    Err(
        "There are local changes that shifting would overwrite. Stack them, restore them, \
        or park them first (see \"stocktake\" for the details).".to_string()
    )
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<Shifted>(),
    ]
}
