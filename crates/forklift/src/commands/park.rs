use chrono::Utc;
use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
use forklift_core::enums::parcel_action_type::ParcelActionType;
use forklift_core::model::parcel::Parcel;
use forklift_core::model::parcel_action::ParcelAction;
use forklift_core::util::shift_utils::FileOp;
use serde::Serialize;
use forklift_core::util::{
    config_utils, file_utils, inventory_utils, merge_utils, object_utils, pallet_utils, park_utils,
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
    // tracked files become staged removals. Untracked files stay untracked. Its own blob
    // stores are already batched internally (DESIGN.html §5.0 D item 10, finding #3).
    inventory_utils::refresh_tracked_entries()?;

    // In a scoped (sparse) bay the dock only materializes the in-scope subtree(s); splice it
    // onto the head's spine exactly like `stack` does (§3.2), so the parked parcel commits the
    // same root a full bay would — `park` is documented to inherit the overlay, and a truncated
    // parked tree would silently break that. The "nothing to park" check below must compare the
    // *spliced* root against head, or it never fires in a scoped bay. Computed before the tree
    // build below (not just for the splice branch, as before) because the rollup-based skip
    // (stage 2) needs it too — see `tree_utils::compute_rollup_skip_plan`'s scoped-bay caveat.
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
    // working directory, not stale pre-refresh content.
    let prepared = std::sync::Arc::new(inventory_utils::prepare_stack_inventory()?);

    // `prepare_stack_inventory` stops parsing at the first conflict entry it finds (sorted key
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

    // `Some(&head_tree_hash)` gives `park` the same rollup-based skip (stage 2) `stack` already
    // has: a directory whose shard's rollup already matches the corresponding head subtree hash
    // is never hashed or (re)stored — its subtree is spliced into its parent verbatim by hash
    // instead of walked and rebuilt. Its shard *is* still fully read and parsed, by
    // `prepare_stack_inventory` above, not just peeked past its header (DESIGN.html §5.0 D item
    // 10, finding #5): the skip plan reads every candidate's rollup out of that already-parsed
    // `prepared` snapshot (an in-memory lookup), which is what makes `prepare_stack_inventory`'s
    // own read+parse pass worth parallelizing — it is real, unavoidable work here, not a header
    // peek a skip could make disappear. The per-directory tree hashes this call also returns are
    // `stack`-only bookkeeping (for stamping rollups after a successful stack) — `park`
    // immediately overwrites every shard from head a few lines down (`replace_all_inventories`),
    // so it passes `track_tree_hashes: false` (DESIGN.html §5.0 D item 10, finding #8) instead of
    // making every one of the build's per-directory tasks pay a `Mutex` acquisition to populate a
    // map nobody reads; the untouched-key set is discarded for the same reason.
    let (partial_root, _tree_hashes, _untouched) = tree_utils::build_tree_from_inventory_deferred(
        &prepared, &batch, Some(&head_tree_hash), &scope, false,
    ).await?;
    let partial_root = partial_root.ok_or("There is nothing to park.".to_string())?;

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

    // Every op's working-directory write happens immediately (unfsynced, same as always), but its
    // shard mutation is only *decided* here — collected into one shared
    // `inventory_utils::ShardMutationBatch` instead of paying `stage_file_entry_from_stat`'s/
    // `update_shard`'s full two-barrier funnel per op (DESIGN.html §5.0 D item 10, finding #4).
    // Several ops in the same shard collapse into one read-modify-write of it, published once at
    // the end. If an op's decision step fails partway through, every op decided before the
    // failure is still published — see `apply_merge_actions`'s identical resilience contract for
    // the same reasoning, including its caveat: a `batch.publish()` failure (not a per-op decision
    // failure) can leave more ops' working-directory writes ahead of their shard entries than the
    // old per-op immediate funnel ever could. Unlike `consolidate`, this call site is safe to just
    // retry without any extra reconciliation step: `ops` is always recomputed fresh from the
    // parked parcel's and its base's immutable tree hashes above, and `parked.pop()`/
    // `park_utils::write_parked` below never run on any failure path, so the parked entry stays
    // listed and a retried `park pop` redoes exactly the same (idempotent) work.
    let mut batch = inventory_utils::ShardMutationBatch::new();
    let mut result: Result<(), String> = Ok(());

    for op in &ops {
        if let Err(e) = shift_utils::apply_file_op(op) {
            result = Err(e);
            break;
        }

        let decision = match op {
            FileOp::Write { path, hash, item_type, .. } => {
                inventory_utils::stage_file_entry_from_stat_into(&mut batch, path, hash.clone(), *item_type)
            }
            FileOp::Remove { path } => {
                let (parent_key, name) = match path.rsplit_once('/') {
                    Some((parent, name)) => (parent, name),
                    None => ("", path.as_str()),
                };

                batch.update(parent_key, |inventory| {
                    inventory.mark_item_deleted(name);
                    Ok(())
                })
            }
        };

        if let Err(e) = decision {
            result = Err(e);
            break;
        }
    }

    if let Err(e) = batch.publish() {
        if result.is_ok() { result = Err(e); }
    }

    if let Err(e) = result {
        return Err(format!(
            "{}\nThe pop did not complete: some files may already have been rewritten without \
            being staged. The parked parcel is still listed — re-run \"park pop\" once the \
            problem is fixed; it recomputes the same change from scratch and is safe to retry.",
            e
        ));
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
