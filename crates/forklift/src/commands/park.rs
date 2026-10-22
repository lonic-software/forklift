use chrono::Utc;
use forklift_core::error::sanitize;
use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
use forklift_core::enums::parcel_action_type::ParcelActionType;
use forklift_core::model::parcel::Parcel;
use forklift_core::model::parcel_action::ParcelAction;
use forklift_core::util::shift_utils::FileOp;
use serde::Serialize;
use forklift_core::util::{
    alias_utils, config_utils, file_utils, inventory_utils, load_guard_utils, merge_utils, object_utils,
    pallet_utils, park_utils, scope_utils, shift_utils, sign_utils, stack_utils, tree_utils,
};
use crate::output::{self, CommandOutput};

// The park command (git's "stash"; a forklift operator parks the truck). One public
// function per subcommand; the CLI surface itself is defined in `cli.rs`.

/// Save the work in progress (the staged and unstaged changes of *tracked* files) as a
/// parked parcel and reset the warehouse to the pallet head. Untracked files are left
/// alone.
pub async fn park_changes() -> Result<(), String> {
    // The cheapest possible pre-check runs first, exactly like `stack_utils::stack_parcel`'s
    // identical guard: `park` builds a tree straight from the staged inventory and durably
    // commits it as a parked parcel below (a real commit-to-history event, not just a working-
    // directory snapshot — popping it later re-stages the content, but the parcel itself is
    // already permanent) — so it must refuse exactly like `stack` while a load that started but
    // never finished cleanly is still recorded (`load_guard_utils`).
    load_guard_utils::check_no_incomplete_load()?;

    let operator = config_utils::get_operator()?;

    // Parked parcels are parcels: once trust is established they are signed too.
    // Park writes no pallet head, so only the key is kept.
    let signing_key_id = stack_utils::resolve_signing_key(&operator)?.key_id().map(str::to_string);

    if merge_utils::read_consolidation_state()?.is_some() {
        return Err(
            "A consolidation is in progress; complete it (or abort it) before parking.".to_string()
        );
    }

    if inventory_utils::has_conflict_entries()? {
        return Err("There are unresolved conflicts in the inventory; parking is not possible.".to_string());
    }

    // The tree this bay holds: its base, not the pallet's ref, which another bay can move.
    let position = pallet_utils::read_position()?;
    let pallet = position.pallet.clone();

    let Some(head) = position.base.clone() else {
        return Err(format!(
            "Pallet \"{}\" has nothing stacked yet; there is no state to park onto.",
            pallet
        ));
    };

    let head_tree_hash = object_utils::load_parcel(&head)?.tree_hash;

    // Stage the whole work in progress: modified tracked files are rehashed, deleted
    // tracked files become staged removals. Untracked files stay untracked. Its own blob
    // stores are already batched internally (DESIGN.html §5.0 D item 10, finding #3). The
    // rebuild below anchors before it, so a file edited after the refresh is never trusted.
    let anchor = file_utils::take_stat_anchor()?;

    inventory_utils::refresh_tracked_entries()?;

    // In a scoped (sparse) bay the dock only materializes the in-scope subtree(s); splice it
    // onto the head's spine exactly like `stack` does (§3.2), so the parked parcel commits the
    // same root a full bay would — `park` is documented to inherit the overlay, and a truncated
    // parked tree would silently break that. The "nothing to park" check below must compare the
    // *spliced* root against head, or it never fires in a scoped bay. Computed before the tree
    // read below (not just for the splice branch) because the rollup-based skip needs it too —
    // see `inventory_utils::prepare_stack_inventory`'s scoped-bay caveat.
    let scope = scope_utils::current_scope()?;

    // Every tree object below (and, further down, the parcel object) is staged into `batch`
    // instead of fsynced immediately — one durability barrier for the whole push instead of one
    // per object (DESIGN.html §5.0 D item 10, finding #3). Nothing may depend on a staged
    // object's durability or visibility until `batch.finish()` below returns `Ok` — see
    // `stack_utils::stack_parcel`'s identical batch, which this mirrors.
    let batch = std::sync::Arc::new(file_utils::WriteBatch::new());

    // Read fresh (not `stack`'s shared `PreparedInventory`, which exists to avoid re-parsing
    // shards across several steps of one `stack` — `park` only reads the inventory once here),
    // but after `refresh_tracked_entries` above so this snapshot reflects the just-rehashed
    // working directory, not stale pre-refresh content. Passing the head and the scope gives
    // this read and the tree build the same rollup-based skip `stack` has: a subtree whose
    // shard's rollup already matches the head subtree hash is not read here, nor hashed or
    // (re)stored — its parent's tree includes it by hash. `park` as a whole still reads every
    // shard: the conflict check and `refresh_tracked_entries` above both do.
    let prepared = std::sync::Arc::new(inventory_utils::prepare_stack_inventory(Some((&head_tree_hash, &scope)))?);

    // `prepare_stack_inventory` stops collecting at the first conflict entry it finds (sorted key
    // order), so `prepared.shards` is silently *incomplete* past that point when `has_conflict`
    // is true — exactly like `stack_parcel`'s identical guard, checked immediately after building
    // its own `PreparedInventory` for the same reason. The `has_conflict_entries()` check at the
    // top of this function already refuses a park before any conflict exists anywhere in the
    // warehouse (so this can only ever be reached with `has_conflict` false in practice — nothing
    // between the two checks can introduce a conflict state), but this makes that safety a
    // guarantee at the point `prepared` is actually consumed, not an invariant borrowed from a
    // decoupled check far above.
    if inventory_utils::has_conflict_entries_in(&prepared) {
        return Err(
            "There are unresolved conflicts in the inventory; parking is not possible.".to_string()
        );
    }

    // The per-directory tree hashes the build can return are `stack`-only bookkeeping (for
    // stamping rollups after a successful stack): `park` overwrites every shard from head a few
    // lines down (`replace_all_inventories`), so it passes `track_tree_hashes: false`.
    let (partial_root, _tree_hashes) = tree_utils::build_tree_from_inventory_deferred(&prepared, &batch, false).await?;
    let partial_root = partial_root.ok_or("There is nothing to park.".to_string())?;

    // Same refusal as `stack`'s, for the same reason, and sharper here: the reset below removes
    // the working copy, so a parked parcel naming lost content could never be popped back.
    tree_utils::require_staged_content_present(&partial_root, Some(&head_tree_hash))?;

    let root_tree = if scope.is_full() {
        partial_root
    } else {
        // A park is a WIP snapshot, never a merge completion, so it has no out-of-scope skeleton:
        // every out-of-scope sibling is copied verbatim from the head. Runs sequentially (no
        // `TaskExecutor` here), but staged into the very same batch as the tree build above so
        // every spine object it writes joins the same barrier.
        let overrides = std::collections::BTreeMap::new();

        tree_utils::build_scoped_root_tree(
            Some(&head_tree_hash), &partial_root, &scope, &overrides, &batch,
        )?
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

    // The parcel object itself joins the same batch as the tree objects it points to: the parked
    // list will reference it directly, so it must be just as durable as everything beneath it
    // before anything downstream can see it.
    let mut object = LooseObjectBuilder::build_parcel(&parcel);
    object.store_deferred(&batch)?;

    // The barrier: every tree object and the parcel object staged above become durable and
    // visible at their final content-addressed paths now, all at once. Must complete (return
    // `Ok`) before anything below is allowed to reference what it just published — the signature
    // sidecar and the parked-list record are both written only from this point on, deliberately
    // kept out of this same batch (see `stack_utils::stack_parcel`'s identical ordering note).
    batch.finish()?;

    // The reset back to the pallet head is checked before anything references the parked
    // parcel: a staged removal leaves a path the head writes back, and untracked content (or,
    // on a case-insensitive volume, another spelling) may sit there now. A refusal here leaves
    // the bay as it was; the parcel object stays unreferenced.
    let (ops, removed_dirs) = shift_utils::diff_trees(Some(&root_tree.hash), &head_tree_hash)?;

    alias_utils::refuse_collisions("Parking", &shift_utils::collect_collisions(&ops, &removed_dirs)?)?;

    if let Some(key_id) = &signing_key_id {
        let signature = sign_utils::sign_parcel_hash(key_id, &object.hash)?;
        sign_utils::store_parcel_signature(&object.hash, &signature)?;
    }

    let mut parked = park_utils::read_parked()?;
    parked.push(object.hash.clone());
    park_utils::write_parked(&parked)?;

    // Reset the working directory and the inventory to the pallet head.
    shift_utils::apply_ops(&ops, &removed_dirs)?;

    let shards = shift_utils::build_inventories_for_tree(&head_tree_hash, shift_utils::TreeProvenance::Written(anchor))?.shards;
    inventory_utils::replace_all_inventories(&shards)?;

    output::message(format!(
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

    // The tree this bay holds: its base, not the pallet's ref, which another bay can move.
    let position = pallet_utils::read_position()?;
    let pallet = position.pallet.clone();

    let Some(head) = position.base.clone() else {
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
        }
    }

    // Untracked files must not be overwritten either, nor another entry through a case alias.
    // A path new to the parked base is new to the head too (every touched path is the same in
    // both, checked above), so the diff's own `is_new` marks the writes to judge.
    let (untracked, aliases) = alias_utils::split_collisions(&shift_utils::collect_collisions(&ops, &removed_dirs)?);
    conflicts.extend(untracked.iter().map(String::as_str));

    if !aliases.is_empty() {
        return Err(format!(
            "Un-parking would write two entries this file system treats as one file:\n  {}\n\
            Rename one of them on a case-sensitive file system first.",
            aliases.join("\n  ")
        ));
    }

    if !conflicts.is_empty() {
        return Err(format!(
            "The parked changes conflict with the current state of these files:\n  {}\n\
            Un-park on the head the changes were parked on (parcel {}), or resolve by hand.",
            conflicts.join("\n  "),
            parked_base
        ));
    }

    // Every removal (and the directories it empties) before every write: across a case-only
    // rename the write would otherwise land on the old name through the alias, and the removal
    // after it would delete it.
    // Every entry is staged through one batch, each written file statted right after its write.
    let mut batch = inventory_utils::ShardMutationBatch::new();

    let applied = shift_utils::apply_ops_each(&ops, &removed_dirs, |op| match op {
        FileOp::Write { path, hash, item_type, .. } => batch.stage_file_from_stat(path, hash.clone(), *item_type),
        FileOp::Remove { path } => {
            let (parent_key, name) = inventory_utils::split_path_key(path);

            batch.update(parent_key, |inventory| { inventory.mark_item_deleted(name); })
        }
    });

    // The batch publishes whatever was applied before a failure; the parcel stays parked on any
    // failure, and the error says how far the pop got.
    match (applied, batch.publish()) {
        (Ok(()), Ok(())) => {}
        (Err(e), Ok(())) => {
            return Err(format!(
                "{}\nThe parked changes applied before this error are in the working directory and \
                staged. The parcel is still parked.",
                e
            ));
        }
        (applied, Err(publish)) => {
            let first = applied.err().map(|e| format!("{}\n", e)).unwrap_or_default();

            return Err(format!(
                "{}{}\nThe working directory holds the parked changes applied so far, but staging \
                them failed, so some or all of them are not staged. The parcel is still parked.",
                first, publish
            ));
        }
    }

    parked.pop();
    park_utils::write_parked(&parked)?;

    output::message(format!("Re-applied the parked changes from {} (staged).", parked_hash));

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

    output::emit(&ParkedList { parked: entries });

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
            println!("{} {}", entry.parcel, sanitize(&entry.description));
        }
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<ParkedList>(),
    ]
}
