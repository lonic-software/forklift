use chrono::Utc;
use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
use forklift_core::enums::parcel_action_type::ParcelActionType;
use forklift_core::model::parcel::Parcel;
use forklift_core::model::parcel_action::ParcelAction;
use forklift_core::util::shift_utils::FileOp;
use serde::Serialize;
use forklift_core::util::{
    config_utils, inventory_utils, merge_utils, object_utils, pallet_utils, park_utils,
    scope_utils, shift_utils, sign_utils, stack_utils, tree_utils,
};
use crate::output::{self, CommandOutput};

// The park command (git's "stash"; a forklift operator parks the truck). One public
// function per subcommand; the CLI surface itself is defined in `cli.rs`.

/// Save the work in progress (the staged and unstaged changes of *tracked* files) as a
/// parked parcel and reset the warehouse to the pallet head. Untracked files are left
/// alone.
pub async fn park_changes() -> Result<(), String> {
    let operator = config_utils::get_operator()?;

    // Parked parcels are parcels: once trust is established they are signed too.
    let signing_key_id = stack_utils::resolve_signing_key(&operator)?;

    if merge_utils::read_consolidation_state()?.is_some() {
        return Err(
            "A consolidation is in progress; complete it (or abort it) before parking.".to_string()
        );
    }

    if inventory_utils::has_conflict_entries()? {
        return Err("There are unresolved conflicts in the inventory; parking is not possible.".to_string());
    }

    let pallet = pallet_utils::get_current_pallet_name()?;

    let Some(head) = pallet_utils::get_pallet_head(&pallet)? else {
        return Err(format!(
            "Pallet \"{}\" has nothing stacked yet; there is no state to park onto.",
            pallet
        ));
    };

    let head_tree_hash = object_utils::load_parcel(&head)?.tree_hash;

    // Stage the whole work in progress: modified tracked files are rehashed, deleted
    // tracked files become staged removals. Untracked files stay untracked.
    inventory_utils::refresh_tracked_entries()?;

    let partial_root = tree_utils::build_tree_from_inventory().await?
        .ok_or("There is nothing to park.".to_string())?;

    // In a scoped (sparse) bay the dock only materializes the in-scope subtree(s); splice it
    // onto the head's spine exactly like `stack` does (§3.2), so the parked parcel commits the
    // same root a full bay would — `park` is documented to inherit the overlay, and a truncated
    // parked tree would silently break that. The "nothing to park" check below must compare the
    // *spliced* root against head, or it never fires in a scoped bay.
    let scope = scope_utils::current_scope()?;

    let root_tree = if scope.is_full() {
        partial_root
    } else {
        // A park is a WIP snapshot, never a merge completion, so it has no out-of-scope skeleton:
        // every out-of-scope sibling is copied verbatim from the head.
        let overrides = std::collections::BTreeMap::new();

        tree_utils::build_scoped_root_tree(Some(&head_tree_hash), &partial_root, &scope, &overrides)?
    };

    if root_tree.hash == head_tree_hash {
        return Err("There is nothing to park: the warehouse matches the pallet head.".to_string());
    }

    // Parked parcels follow the same authorship convention as stacked ones: the author
    // is recorded explicitly, even though it is always the parking operator.
    let timestamp = Utc::now();

    let parcel = Parcel {
        tree_hash: root_tree.hash.clone(),
        parents: vec![head.clone()],
        actions: vec![
            ParcelAction {
                operator: operator.clone(),
                action: ParcelActionType::Author,
                description: None,
                timestamp,
            },
            ParcelAction {
                operator,
                action: ParcelActionType::Stack,
                description: None,
                timestamp,
            },
        ],
        description: Some(format!("Parked changes on pallet \"{}\".", pallet)),
    };

    let mut object = LooseObjectBuilder::build_parcel(&parcel);
    object.store()?;

    if let Some(key_id) = &signing_key_id {
        let signature = sign_utils::sign_parcel_hash(key_id, &object.hash)?;
        sign_utils::store_parcel_signature(&object.hash, &signature)?;
    }

    let mut parked = park_utils::read_parked()?;
    parked.push(object.hash.clone());
    park_utils::write_parked(&parked)?;

    // Reset the working directory and the inventory to the pallet head.
    let (ops, removed_dirs) = shift_utils::diff_trees(Some(&root_tree.hash), &head_tree_hash)?;

    for op in &ops {
        shift_utils::apply_file_op(op)?;
    }

    shift_utils::remove_empty_directories(&removed_dirs);

    let shards = shift_utils::build_inventories_for_tree(&head_tree_hash)?;
    inventory_utils::replace_all_inventories(&shards)?;

    output::message("park", format!(
        "Parked the work in progress as {} and reset to the pallet head.", object.hash
    ));

    Ok(())
}

/// Re-apply the most recently parked parcel (staging its changes) and drop it from the
/// parked list.
pub fn pop_parked() -> Result<(), String> {
    let mut parked = park_utils::read_parked()?;

    let Some(parked_hash) = parked.last().cloned() else {
        return Err("There are no parked changes.".to_string());
    };

    let parked_parcel = object_utils::load_parcel(&parked_hash)?;

    let Some(parked_base) = parked_parcel.parents.first().cloned() else {
        return Err(format!("Parked parcel {} has no parent; it cannot be re-applied.", parked_hash));
    };

    let parked_base_tree = object_utils::load_parcel(&parked_base)?.tree_hash;

    let pallet = pallet_utils::get_current_pallet_name()?;

    let Some(head) = pallet_utils::get_pallet_head(&pallet)? else {
        return Err(format!(
            "Pallet \"{}\" has nothing stacked yet; there is nothing to un-park onto.",
            pallet
        ));
    };

    let head_tree_hash = object_utils::load_parcel(&head)?.tree_hash;

    // The parked changes are the diff between the parked parcel and the head it was
    // parked on.
    let (ops, removed_dirs) = shift_utils::diff_trees(
        Some(&parked_base_tree),
        &parked_parcel.tree_hash
    )?;

    // Safety: every file the parked changes touch must be unchanged between the parked
    // base and the current head — this keeps un-parking a clean re-apply instead of a
    // merge. Anything else must go through "consolidate".
    let mut conflicts: Vec<&str> = Vec::new();

    for op in &ops {
        let path = match op {
            FileOp::Write { path, .. } => path,
            FileOp::Remove { path } => path,
        };

        let in_base = object_utils::resolve_tree_file(&parked_base_tree, path)?;
        let in_head = object_utils::resolve_tree_file(&head_tree_hash, path)?;

        if in_base != in_head {
            conflicts.push(path);
            continue;
        }

        // Untracked files must not be overwritten either.
        if in_head.is_none() && std::path::Path::new(path).exists() {
            conflicts.push(path);
        }
    }

    if !conflicts.is_empty() {
        return Err(format!(
            "The parked changes conflict with the current state of these files:\n  {}\n\
            Un-park on the head the changes were parked on (parcel {}), or resolve by hand.",
            conflicts.join("\n  "),
            parked_base
        ));
    }

    for op in &ops {
        shift_utils::apply_file_op(op)?;

        match op {
            FileOp::Write { path, hash, item_type, .. } => {
                inventory_utils::stage_file_entry_from_stat(path, hash.clone(), *item_type)?;
            }
            FileOp::Remove { path } => {
                let (parent_key, name) = match path.rsplit_once('/') {
                    Some((parent, name)) => (parent, name),
                    None => ("", path.as_str()),
                };

                inventory_utils::update_shard(parent_key, |inventory| {
                    inventory.mark_item_deleted(name);
                    Ok(())
                })?;
            }
        }
    }

    shift_utils::remove_empty_directories(&removed_dirs);

    parked.pop();
    park_utils::write_parked(&parked)?;

    output::message("park", format!("Re-applied the parked changes from {} (staged).", parked_hash));

    Ok(())
}

/// List the parked parcels, newest first.
pub fn list_parked() -> Result<(), String> {
    let parked = park_utils::read_parked()?;

    let mut entries = Vec::new();

    for hash in parked.iter().rev() {
        let description = object_utils::load_parcel(hash)?
            .description
            .unwrap_or_default();

        entries.push(ParkedEntry { parcel: hash.clone(), description });
    }

    output::emit("park", &ParkedList { parked: entries });

    Ok(())
}

/// The list of parked parcels, newest first.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ParkedList {
    parked: Vec<ParkedEntry>,
}

/// One parked parcel.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ParkedEntry {
    parcel: String,
    description: String,
}

impl CommandOutput for ParkedList {
    fn render_human(&self) {
        if self.parked.is_empty() {
            println!("There are no parked changes.");
            return;
        }

        for entry in &self.parked {
            println!("{} {}", entry.parcel, entry.description);
        }
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("ParkedList", schemars::schema_for!(ParkedList)),
    ]
}
