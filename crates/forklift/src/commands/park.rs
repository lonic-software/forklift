use chrono::Utc;
use forklift_core::error::sanitize;
use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
use forklift_core::enums::parcel_action_type::ParcelActionType;
use forklift_core::model::parcel::Parcel;
use forklift_core::model::parcel_action::ParcelAction;
use forklift_core::util::shift_utils::{Applied, FileOp};
use forklift_core::util::stocktake_utils::ChangeKind;
use serde::Serialize;
use forklift_core::util::{
    alias_utils, cherry_pick_utils, config_utils, file_utils, inventory_utils, load_guard_utils, merge_utils, object_utils,
    operation_utils, pallet_utils, park_utils, redaction_utils, scope_utils, shift_utils, sign_utils, stack_utils,
    tree_utils,
};
use forklift_core::util::lock_utils::ParkedLock;
use forklift_core::util::scope_utils::PathScope;
use forklift_core::util::path_utils::{self, WarehousePath};
use std::collections::BTreeSet;
use forklift_core::util::operation_utils::{Operation, OperationKind};
use crate::commands::merge_abort;
use crate::output::{self, CommandOutput};

// The park command (git's "stash"; a forklift operator parks the truck). One public
// function per subcommand; the CLI surface itself is defined in `cli.rs`.

/// Save the work in progress (the staged and unstaged changes of *tracked* files) as a
/// parked parcel listed as `name`, or under a default name, and reset the warehouse to the pallet
/// head. Untracked files are left alone. Given `paths`, only the changes at or under them are
/// parked and reset; every other file keeps its working content and its staging.
pub async fn park_changes(name: Option<&str>, paths: &[String]) -> Result<(), String> {
    if let Some(name) = name {
        park_utils::validate_name(name)?;
    }

    let paths = pathspec(paths)?;

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

    // Parked, a merge's output would leave its state behind, for a later stack or abort to act on
    // a bay that no longer holds it.
    let in_progress = match (merge_utils::read_consolidation_state()?, cherry_pick_utils::read_state()?) {
        (Some(_), _) => Some("consolidation"),
        (None, Some(_)) => Some("pick"),
        (None, None) => None,
    };
    if let Some(what) = in_progress {
        return Err(format!(
            "A {} is in progress; complete it with \"forklift stack\", or abandon it with \"{}\", before parking.",
            what, merge_abort::ABORT_COMMAND
        ));
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

    if paths.is_empty() {
        inventory_utils::refresh_tracked_entries(|path, hash| park_refusal(path, hash))?;
    } else {
        inventory_utils::refresh_tracked_entries_under(&paths, |path, hash| park_refusal(path, hash))?;
    }

    // In a scoped (sparse) bay the inventory holds only the in-scope subtree(s); splice it
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

    let root_tree_hash = if !paths.is_empty() {
        // Only the trees on the way to the paths change: everything else is the head's, by hash,
        // whatever is staged there. The splice reads the staged trees, so they are made durable
        // first, and its own trees join a barrier of their own. The paths lie in the bay's scope,
        // so no out-of-scope sibling is on the way to one.
        batch.finish()?;
        let spliced = std::sync::Arc::new(file_utils::WriteBatch::new());
        let root = tree_utils::splice_paths(&head_tree_hash, &partial_root.hash, &paths, &spliced)?;
        spliced.finish()?;

        refuse_unmatched(&paths, &head_tree_hash, &root)?;
        root
    } else if scope.is_full() {
        partial_root.hash
    } else {
        // A park is a WIP snapshot, never a merge completion, so it has no out-of-scope skeleton:
        // every out-of-scope sibling is copied verbatim from the head. Runs sequentially (no
        // `TaskExecutor` here), but staged into the very same batch as the tree build above so
        // every spine object it writes joins the same barrier.
        let overrides = std::collections::BTreeMap::new();

        tree_utils::build_scoped_root_tree(
            Some(&head_tree_hash), &partial_root, &scope, &overrides, &batch,
        )?.hash
    };

    if root_tree_hash == head_tree_hash {
        return Err(if paths.is_empty() {
            "There is nothing to park: the warehouse matches the pallet head.".to_string()
        } else {
            "There is nothing to park under those paths: they match the pallet head.".to_string()
        });
    }

    // The barrier: every tree object staged above becomes durable and visible at its final
    // content-addressed path now, all at once. The parcel object is not among them: it is stored
    // only in the hold that lists it, so a park refused below stores no parcel at all.
    batch.finish()?;

    // A parked parcel is no pallet head, but popping it would bring the content back, so the
    // parked tree is judged whole, as a head's is: the refresh above judged only what it restaged.
    // Judged once the batch is readable; a refusal leaves the bay as it was.
    if let Some(guard) = redaction_utils::staging_guard()? {
        if let Some((path, hash)) = guard.names_tree(&root_tree_hash)? {
            return Err(park_refusal(&path, &hash));
        }
    }

    // The reset back to the pallet head is checked before anything references the parked
    // parcel: a staged removal leaves a path the head writes back, and untracked content (or,
    // on a case-insensitive volume, another spelling) may sit there now. A refusal here leaves
    // the bay as it was.
    let (ops, removed_dirs) = shift_utils::diff_trees(Some(&root_tree_hash), &head_tree_hash)?;

    alias_utils::refuse_collisions("Parking", &shift_utils::collect_collisions(&ops, &removed_dirs)?)?;
    shift_utils::ensure_writes_present(&ops)?;

    // The record is read before anything is signed, so damage refuses the park with nothing
    // written; the name it picks is checked again in the hold below.
    let timestamp = Utc::now();
    let parked = park_utils::read_parked()?;
    let name = match name {
        Some(name) if parked.iter().any(|entry| entry.name == name) => return Err(format!(
            "Work is already parked as \"{}\". Nothing was parked; choose another name, or drop that park first.",
            sanitize(name)
        )),
        Some(name) => name.to_string(),
        None => park_utils::unique_name(&park_utils::default_name(&pallet, timestamp), &parked),
    };

    // Parked parcels follow the same authorship convention as stacked ones: the author is
    // recorded explicitly, even though it is always the parking operator. The Park action names
    // the park, so a store scan can bring the name back with the parcel.
    let parcel = Parcel {
        tree_hash: root_tree_hash.clone(),
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
                action: ParcelActionType::Park,
                description: Some(name.clone()),
                timestamp,
            },
        ],
        description: Some(format!("Parked changes on pallet \"{}\".", pallet)),
    };
    let mut object = LooseObjectBuilder::build_parcel(&parcel);

    // Signed outside the hold: a signature can wait on a key agent, which the hold never does.
    let signature = signing_key_id.as_deref()
        .map(|key_id| sign_utils::sign_parcel_hash(key_id, &object.hash))
        .transpose()?;

    {
        let lock = ParkedLock::acquire()?;
        let mut parked = park_utils::read_parked()?;

        if parked.iter().any(|entry| entry.name == name) {
            return Err(format!(
                "Other work was parked as \"{}\" while this park ran. Nothing was parked; park again.",
                sanitize(&name)
            ));
        }

        object.store()?;
        if let Some(signature) = &signature {
            sign_utils::store_parcel_signature(&object.hash, signature)?;
        }

        // Raised before the record names the parcel, and finished once staging is back at the
        // head. Recovery tells the two sides of the record write apart by `listed`, written in
        // this same hold, and else by the record itself, read under this lock.
        let mut operation = Operation::new(
            OperationKind::Park, position.clone(), Some(head_tree_hash.clone()), position.clone(),
        );
        operation.target_tree = Some(head_tree_hash.clone());
        operation.parked = Some(object.hash.clone());
        operation.park_name = Some(name.clone());
        operation.pathspec = paths.clone();
        operation_utils::raise(&operation)?;

        #[cfg(test)]
        tests::BEFORE_PARKED_WRITE.with(|seam| seam.take().map(|seam| seam())).transpose()?;

        parked.push(park_utils::ParkedEntry { parcel: object.hash.clone(), recovered: false, name: name.clone() });
        park_utils::write_parked(&lock, &parked)?;

        #[cfg(test)]
        tests::AFTER_PARKED_WRITE.with(|seam| seam.take().map(|seam| seam())).transpose()?;

        operation_utils::record_listed()?;

        #[cfg(test)]
        tests::AFTER_LISTED.with(|seam| seam.take().map(|seam| seam())).transpose()?;
    }

    // Reset the working directory and the inventory to the pallet head: the ops, diffed from the
    // parked tree, touch only the paths when there are any, and so does the staging reset.
    shift_utils::apply_ops(&ops, &removed_dirs)?;

    if paths.is_empty() {
        let shards = shift_utils::build_inventories_for_tree(&head_tree_hash, shift_utils::TreeProvenance::Written(anchor))?.shards;
        inventory_utils::replace_all_inventories(&shards)?;
    } else {
        shift_utils::reset_staging_under(&head_tree_hash, &paths, shift_utils::TreeProvenance::Written(anchor))?;
    }
    operation_utils::finish()?;

    output::message(if paths.is_empty() {
        format!("Parked the work in progress as \"{}\" ({}) and reset to the pallet head.", sanitize(&name), object.hash)
    } else {
        format!(
            "Parked the work in progress under the given paths as \"{}\" ({}) and reset them to the pallet head; \
            every other change is left as it was.",
            sanitize(&name), object.hash
        )
    });

    Ok(())
}

/// The warehouse path keys of a partial park's `raw` paths, each in the bay's scope, sorted, and none
/// at or under another: empty, a whole park, when there are none or one is the warehouse root.
fn pathspec(raw: &[String]) -> Result<Vec<String>, String> {
    let mut keys = BTreeSet::new();

    for raw in raw {
        let path = WarehousePath::from_user_input(raw)?;
        if path.is_root() {
            return Ok(Vec::new());
        }
        crate::commands::scope::ensure_path_in_scope(path.as_key())?;
        keys.insert(path.as_key().to_string());
    }

    // Sorted, an ancestor comes before everything under it.
    let mut paths: Vec<String> = Vec::new();
    for key in keys {
        if !paths.iter().any(|path| path_utils::covers(path, &key)) {
            paths.push(key);
        }
    }

    Ok(paths)
}

/// Refuse a partial park naming a path that nothing holds: not the head, not the parked tree, and
/// not the working directory. A misspelled path would otherwise be parked as nothing, silently.
fn refuse_unmatched(paths: &[String], head_tree: &str, parked_tree: &str) -> Result<(), String> {
    for path in paths {
        let on_disk = std::path::Path::new(path).symlink_metadata().is_ok();
        if !on_disk && !tree_holds(head_tree, path)? && !tree_holds(parked_tree, path)? {
            return Err(format!(
                "\"{}\" names nothing in the warehouse: no file or folder is there, in the pallet head or in \
                the work in progress. Nothing was parked.",
                sanitize(path)
            ));
        }
    }

    Ok(())
}

/// Whether the tree `tree` holds a file or folder at the warehouse path key `key`.
fn tree_holds(tree: &str, key: &str) -> Result<bool, String> {
    let mut level = object_utils::load_tree(tree)?;
    let mut components = key.split('/').peekable();

    while let Some(component) = components.next() {
        if components.peek().is_none() {
            return Ok(level.get_file(component).is_some() || level.get_subtree(component).is_some());
        }
        let Some(subtree) = level.get_subtree(component) else {
            return Ok(false);
        };
        level = object_utils::load_tree(&subtree.hash)?;
    }

    Ok(false)
}

/// The refusal of a park that would park the file at `path`, whose content `hash` is redacted.
fn park_refusal(path: &str, hash: &str) -> String {
    redaction_utils::staging_refusal(
        "park",
        path,
        hash,
        "Nothing was parked.",
        format!("change or delete \"{}\" so it no longer holds that content, then park again", path),
    )
}

/// Re-apply the park named `name`, or the newest (staging its changes), and take it off the
/// parked list.
pub fn pop_parked(name: Option<&str>) -> Result<(), String> {
    unpark(name, Unpark::Pop)
}

/// Re-apply the park named `name`, or the newest (staging its changes), and keep it parked.
pub fn apply_parked(name: Option<&str>) -> Result<(), String> {
    unpark(name, Unpark::Apply)
}

/// Whether an un-park keeps the park or takes it off the list.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Unpark {
    Apply,
    Pop,
}

/// The sentence an un-park of an entry `heal` recovered adds.
const RECOVERED_NOTE: &str = "Heal recovered this park from the object store, so it may be work that was \
    already popped or dropped.";

fn unpark(name: Option<&str>, how: Unpark) -> Result<(), String> {
    let entry = {
        let parked = park_utils::read_parked()?;
        parked[park_utils::pick(&parked, name)?].clone()
    };
    let parked_hash = entry.parcel.clone();

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
    // parked on, within this bay's scope. Parks are shared by every bay, so one made in a wider
    // bay can change files this one does not hold: a pop would apply only part of it and take
    // all of it off the list, so it refuses; an apply keeps the park and says what it left out.
    let scope = scope_utils::current_scope()?;
    let (ops, removed_dirs) = shift_utils::diff_trees_in(
        Some(&parked_base_tree),
        &parked_parcel.tree_hash,
        &scope
    )?;
    let outside = outside_scope(Some(&parked_base_tree), &parked_parcel.tree_hash, &scope)?;

    if how == Unpark::Apply && ops.is_empty() && !outside.is_empty() {
        return Err(format!(
            "Every file the parked work \"{}\" ({}) changes is outside this bay's scope, so applying it \
            here applies nothing; it stays parked. Apply or pop it in a bay whose scope holds them.",
            sanitize(&entry.name), parked_hash
        ));
    }

    if how == Unpark::Pop && !outside.is_empty() {
        return Err(format!(
            "The parked work \"{}\" ({}) also changes files outside this bay's scope:\n  {}\n\
            Popping it here would apply only part of it, so nothing was applied. \"forklift park apply\" \
            applies the part inside this scope and keeps it parked; a pop in a bay whose scope holds \
            these files applies all of it.",
            sanitize(&entry.name), parked_hash,
            outside.iter().map(|path| sanitize(path)).collect::<Vec<_>>().join("\n  ")
        ));
    }

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
            Un-park on the head the changes were parked on (parcel {}), or resolve by hand; \
            \"forklift park drop\" discards a park that is no longer wanted.{}",
            conflicts.join("\n  "),
            parked_base,
            if entry.recovered { format!(" {}", RECOVERED_NOTE) } else { String::new() }
        ));
    }

    // Whatever the apply needs from an origin is fetched only once the checks above pass, and
    // before a pop takes the lock that every bay's park, pop and drop wait on.
    shift_utils::ensure_writes_present(&ops)?;

    // A pop holds the parked-work lock from here until the entry is off the list, so two bays
    // popping one park never both apply it. The entry is found again under the lock by its name
    // and parcel: one another command took meanwhile is refused with nothing applied, while a
    // park added since is no reason to.
    #[cfg(test)]
    tests::BEFORE_POP_HOLDS.with(|seam| seam.take().map(|seam| seam())).transpose()?;

    let lock = match how {
        Unpark::Pop => {
            let lock = ParkedLock::acquire()?;
            let parked = park_utils::read_parked()?;
            let Some(index) = parked.iter().position(|now| now.name == entry.name && now.parcel == parked_hash) else {
                return Err(format!(
                    "The parked work \"{}\" ({}) was popped or dropped by another command while this \
                    pop ran. Nothing was applied.",
                    sanitize(&entry.name), parked_hash
                ));
            };
            Some((lock, parked, index))
        }
        Unpark::Apply => None,
    };

    // Every removal (and the directories it empties) before every write: across a case-only
    // rename the write would otherwise land on the old name through the alias, and the removal
    // after it would delete it.
    // Every entry is staged through one batch, each written file statted right after its write.
    let mut batch = inventory_utils::ShardMutationBatch::new();

    let applied = shift_utils::apply_ops_each(&ops, &removed_dirs, |op, applied| match op {
        FileOp::Write { path, hash, item_type, .. } if applied == Applied::NotCheckedOut => {
            batch.stage_unavailable(path, hash.clone(), *item_type)
        }
        FileOp::Write { path, hash, item_type, .. } => batch.stage_file_from_stat(path, hash.clone(), *item_type),
        FileOp::Remove { path } => {
            let (parent_key, name) = inventory_utils::split_path_key(path);

            batch.update(parent_key, |inventory| { inventory.mark_item_deleted(name); })
        }
    });

    // The batch publishes whatever was applied before a failure; the parcel stays parked on any
    // failure, and the error says how far the un-park got.
    match (applied, batch.publish()) {
        (Ok(()), Ok(())) => {}
        (Err(e), Ok(())) => {
            return Err(format!(
                "{}\nThe parked changes applied before this error are in the working directory and \
                staged. The parcel is still parked, so once the cause is fixed, un-parking it again \
                applies the rest.",
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

    #[cfg(test)]
    tests::BEFORE_POP_REMOVES.with(|seam| seam.take().map(|seam| seam())).transpose()?;

    let mut note = if entry.recovered { format!(" {}", RECOVERED_NOTE) } else { String::new() };
    if !outside.is_empty() {
        note.push_str(&format!(
            " It also changes {} outside this bay's scope, not applied here; \"forklift park show\" \
            lists them.",
            if outside.len() == 1 { "1 file".to_string() } else { format!("{} files", outside.len()) }
        ));
    }

    match lock {
        Some((lock, mut parked, index)) => {
            parked.remove(index);
            park_utils::write_parked(&lock, &parked)?;

            output::message(format!(
                "Re-applied the parked work \"{}\" ({}) (staged) and took it off the parked list.{}",
                sanitize(&entry.name), parked_hash, note
            ));
        }
        None => output::message(format!(
            "Re-applied the parked work \"{}\" ({}) (staged); it stays parked.{}",
            sanitize(&entry.name), parked_hash, note
        )),
    }

    Ok(())
}

/// Take the park named `name`, or the newest, off the parked list without applying it. Its
/// parcel is left to garbage collection, and until then `pick` brings its changes back, so a drop
/// is not on the surface's destructive list.
pub fn drop_parked(name: Option<&str>) -> Result<(), String> {
    let lock = ParkedLock::acquire()?;
    let mut parked = park_utils::read_parked()?;
    let entry = parked.remove(park_utils::pick(&parked, name)?);
    park_utils::write_parked(&lock, &parked)?;

    output::message(format!(
        "Dropped the parked work \"{}\" ({}); its changes were not applied. Until gc frees the parcel, \
        \"forklift pick {}\" stacks its changes onto the current pallet.",
        sanitize(&entry.name), entry.parcel, entry.parcel
    ));

    Ok(())
}

/// Show the park named `name`, or the newest: its parcel, the head it was parked on, when, and
/// how it changes each file.
pub fn show_parked(name: Option<&str>) -> Result<(), String> {
    output::emit(&park_show(name)?);

    Ok(())
}

fn park_show(name: Option<&str>) -> Result<ParkedShow, String> {
    let entry = {
        let parked = park_utils::read_parked()?;
        parked[park_utils::pick(&parked, name)?].clone()
    };
    let parcel = object_utils::load_parcel(&entry.parcel)?;
    let base = parcel.parents.first().cloned();

    // When it was parked: its Park action, or for a parcel parked before that marker, its Author
    // action.
    let parked_at = parcel.actions.iter().find(|action| matches!(action.action, ParcelActionType::Park))
        .or_else(|| parcel.actions.iter().find(|action| matches!(action.action, ParcelActionType::Author)))
        .map(|action| action.timestamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));

    let base_tree = base.as_deref().map(|base| object_utils::load_parcel(base).map(|base| base.tree_hash)).transpose()?;
    // A plain tree-to-tree comparison, as `diff` makes: not the apply plan, which depends on this
    // file system's case folding.
    // Parks are shared by every bay, so every file is listed, those this bay's scope leaves out
    // marked.
    let scope = scope_utils::current_scope()?;
    let outside = outside_scope(base_tree.as_deref(), &parcel.tree_hash, &scope)?;
    let files = crate::commands::diff::tree_file_changes(base_tree.as_deref(), &parcel.tree_hash, &PathScope::full())?
        .into_iter().map(|(change, path, moved_from)| {
            let outside_scope = outside.contains(&path) || moved_from.as_ref().is_some_and(|from| outside.contains(from));
            ParkedFile { path, change, moved_from, outside_scope }
        }).collect();

    Ok(ParkedShow { name: entry.name, parcel: entry.parcel, base, recovered: entry.recovered, parked_at, files })
}

/// The paths a change from tree `from` to tree `to` touches (a move touches both its ends) that a
/// comparison within `scope` leaves out: none in a full scope.
fn outside_scope(from: Option<&str>, to: &str, scope: &PathScope) -> Result<BTreeSet<String>, String> {
    if scope.is_full() {
        return Ok(BTreeSet::new());
    }
    let touched = |scope: &PathScope| -> Result<BTreeSet<String>, String> {
        Ok(crate::commands::diff::tree_file_changes(from, to, scope)?.into_iter()
            .flat_map(|(_, path, moved_from)| std::iter::once(path).chain(moved_from))
            .collect())
    };
    let inside = touched(scope)?;
    Ok(touched(&PathScope::full())?.into_iter().filter(|path| !inside.contains(path)).collect())
}

/// One park, with how it changes each file.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ParkedShow {
    /// The name the work is parked under.
    name: String,
    /// The parked parcel.
    parcel: String,
    /// The head the work was parked on, which un-parking applies the changes against.
    base: Option<String>,
    /// Whether `heal` found the parcel in the store rather than in the parked-work record: it may
    /// be work that was popped or dropped before the record was lost.
    recovered: bool,
    /// When the work was parked (UTC, RFC 3339).
    parked_at: Option<String>,
    /// Every file the park changes, against its base, in this bay's scope or not.
    files: Vec<ParkedFile>,
}

/// One file a park changes.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ParkedFile {
    path: String,
    /// How the park changes the file, in the words `stocktake` and `diff` use.
    change: ChangeKind,
    /// Where a moved file moved from.
    #[serde(skip_serializing_if = "Option::is_none")]
    moved_from: Option<String>,
    /// Whether the change reaches outside this bay's scope, so un-parking here does not apply it.
    outside_scope: bool,
}

impl CommandOutput for ParkedShow {
    fn render_human(&self) {
        let recovered = if self.recovered { " (recovered by heal)" } else { "" };
        println!("Park \"{}\"{}", sanitize(&self.name), recovered);
        println!("  parcel    {}", self.parcel);
        println!("  base      {}", self.base.as_deref().unwrap_or("(none)"));
        if let Some(parked_at) = &self.parked_at {
            println!("  parked    {}", parked_at);
        }
        for file in &self.files {
            let outside = if file.outside_scope { "  (outside this bay's scope)" } else { "" };
            match &file.moved_from {
                Some(from) => println!("  {:<9} {} -> {}{}", file.change, sanitize(from), sanitize(&file.path), outside),
                None => println!("  {:<9} {}{}", file.change, sanitize(&file.path), outside),
            }
        }
    }
}

/// List the parked parcels, newest first.
pub fn list_parked() -> Result<(), String> {
    let parked = park_utils::read_parked()?;

    let mut entries = Vec::new();

    for entry in parked.iter().rev() {
        let description = object_utils::load_parcel(&entry.parcel)?
            .description
            .unwrap_or_default();

        entries.push(ParkedEntry { parcel: entry.parcel.clone(), name: entry.name.clone(), recovered: entry.recovered, description });
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
    /// The name the work is parked under.
    name: String,
    /// Whether `heal` found the parcel in the store rather than in the parked-work record: it may
    /// be work that was popped or dropped before the record was lost.
    recovered: bool,
    description: String,
}

impl CommandOutput for ParkedList {
    fn render_human(&self) {
        if self.parked.is_empty() {
            println!("There are no parked changes.");
            return;
        }

        for entry in &self.parked {
            let recovered = if entry.recovered { " (recovered by heal)" } else { "" };
            println!("{} {}{}", entry.parcel, sanitize(&entry.name), recovered);
        }
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<ParkedList>(),
        crate::docgen::OutputSchema::of::<ParkedShow>(),
    ]
}

#[cfg(test)]
mod tests {
    /// A test's stand-in for a crash at one point of a park: an error it returns is the crash.
    type Seam = std::cell::Cell<Option<Box<dyn FnOnce() -> Result<(), String>>>>;

    thread_local! {
        /// Runs once after park raised its marker, before the parked list names the parcel: an
        /// error stands for a crash there.
        pub(crate) static BEFORE_PARKED_WRITE: Seam = std::cell::Cell::new(None);

        /// Runs once after the parked list names the parcel, before the bay is reset: an error
        /// stands for a crash there.
        pub(crate) static AFTER_PARKED_WRITE: Seam = std::cell::Cell::new(None);

        /// Runs once after the marker records that the parked list names the parcel, before the
        /// bay is reset: an error stands for a crash there.
        pub(crate) static AFTER_LISTED: Seam = std::cell::Cell::new(None);

        /// Runs once after a pop picked its park and fetched what it needs, before it takes the
        /// parked-work lock: a test stands in for another bay's command there.
        pub(crate) static BEFORE_POP_HOLDS: Seam = std::cell::Cell::new(None);

        /// Runs once after a pop applied and staged the parked changes, before it takes them out
        /// of the parked list: a test checks the lock is held there.
        pub(crate) static BEFORE_POP_REMOVES: Seam = std::cell::Cell::new(None);
    }

    use super::*;
    use crate::test_support::{self, Warehouse};
    use forklift_core::util::operation_utils::RecoveryOutcome;
    use forklift_core::util::stocktake_utils;

    /// "main" at a tree of a.txt, checked out, with a.txt edited and loaded. Returns the head's tree.
    fn work_in_progress_on_main(root: &std::path::Path) -> String {
        test_support::configure_operator();

        let tree = test_support::store_tree("a.txt", b"one\n");
        let head = test_support::store_parcel(&tree, &[]);
        pallet_utils::update_pallet_head("main", None, &head, &pallet_utils::RefPreconditions::NONE).unwrap();
        pallet_utils::write_position(&pallet_utils::BayPosition::new("main", Some(head))).unwrap();
        test_support::check_out(&tree);

        std::fs::write(root.join("a.txt"), b"work\n").unwrap();
        test_support::block_on(crate::commands::load::handle_command(".")).unwrap();

        tree
    }

    fn staged_paths(tree: &str) -> Vec<String> {
        test_support::block_on(stocktake_utils::collect_staged_changes(Some(tree))).unwrap()
            .into_iter().map(|change| change.path).collect()
    }

    #[test]
    fn a_park_interrupted_before_the_parked_list_names_its_parcel_leaves_the_work_in_place() {
        let warehouse = Warehouse::with_working_tree("park-crash-before-list");
        let tree = work_in_progress_on_main(&warehouse.root);

        BEFORE_PARKED_WRITE.with(|seam| seam.set(Some(Box::new(|| Err("crashed".to_string())))));
        assert_eq!(test_support::block_on(park_changes(None, &[])).unwrap_err(), "crashed");
        operation_utils::forget_raised_for_tests();

        let recovery = operation_utils::recover().unwrap().expect("a marker stands");
        assert_eq!((recovery.operation, recovery.outcome), (OperationKind::Park, RecoveryOutcome::Cleared));

        assert!(park_utils::read_parked().unwrap().is_empty(), "nothing was parked");
        assert!(test_support::file_holds(&warehouse.root, "a.txt", b"work\n"), "the work stays in the file");
        assert_eq!(staged_paths(&tree), vec!["a.txt".to_string()], "and staged, as it was");
    }

    #[test]
    fn a_park_interrupted_after_the_parked_list_names_its_parcel_resets_staging_to_the_head() {
        let warehouse = Warehouse::with_working_tree("park-crash-after-list");
        let tree = work_in_progress_on_main(&warehouse.root);

        AFTER_PARKED_WRITE.with(|seam| seam.set(Some(Box::new(|| Err("crashed".to_string())))));
        assert_eq!(test_support::block_on(park_changes(None, &[])).unwrap_err(), "crashed");
        operation_utils::forget_raised_for_tests();

        let recovery = operation_utils::recover().unwrap().expect("a marker stands");
        assert_eq!((recovery.operation, recovery.outcome), (OperationKind::Park, RecoveryOutcome::RolledForward));

        let parked = park_utils::read_parked().unwrap();
        assert_eq!(parked.len(), 1, "the work is safe in the parked list");
        let parked_tree = object_utils::load_parcel(&parked[0].parcel).unwrap().tree_hash;
        assert!(object_utils::load_tree(&parked_tree).is_ok(), "the parked parcel's tree is stored");
        assert!(staged_paths(&tree).is_empty(), "staging is back at the head");

        // The reset is finished: the file still held the parked content, so it is back at the head.
        assert!(recovery.unheld.is_empty(), "{:?}", recovery.unheld);
        assert!(test_support::file_holds(&warehouse.root, "a.txt", b"one\n"));
    }

    /// Park "main"'s work twice, the second after a fresh edit. Returns the two entries.
    fn two_parks(root: &std::path::Path) -> Vec<park_utils::ParkedEntry> {
        work_in_progress_on_main(root);
        test_support::block_on(park_changes(None, &[])).unwrap();
        std::fs::write(root.join("a.txt"), b"more work\n").unwrap();
        test_support::block_on(crate::commands::load::handle_command(".")).unwrap();
        test_support::block_on(park_changes(None, &[])).unwrap();

        let parked = park_utils::read_parked().unwrap();
        assert_eq!(parked.len(), 2, "sanity: two parks are listed");
        parked
    }

    fn remove_record() {
        forklift_core::util::file_utils::remove_file(park_utils::parked_path()).unwrap();
    }

    /// A crash after the marker records `listed` rolls forward even when the record is gone by
    /// recovery: the marker alone says the parcel was listed. Revert the `operation.listed` arm in
    /// recovery and it reads the missing record, which refuses.
    #[test]
    fn a_park_interrupted_after_it_was_listed_rolls_forward_without_reading_the_record() {
        let warehouse = Warehouse::with_working_tree("park-crash-after-listed");
        let tree = work_in_progress_on_main(&warehouse.root);

        AFTER_LISTED.with(|seam| seam.set(Some(Box::new(|| Err("crashed".to_string())))));
        assert_eq!(test_support::block_on(park_changes(None, &[])).unwrap_err(), "crashed");
        operation_utils::forget_raised_for_tests();
        remove_record();

        let recovery = operation_utils::recover().unwrap().expect("a marker stands");
        assert_eq!((recovery.operation, recovery.outcome), (OperationKind::Park, RecoveryOutcome::RolledForward));
        assert!(staged_paths(&tree).is_empty(), "staging is back at the head");
    }

    /// A crash before the record names the parcel, then damage to the record: the rebuild finds
    /// the stored parcel and lists it as recovered, and recovery then clears the park, taking the
    /// recovered entry out, so the work is in the file once and not also parked.
    #[test]
    fn a_park_interrupted_before_listing_clears_and_takes_out_the_entry_a_rebuild_recovered() {
        let warehouse = Warehouse::with_working_tree("park-crash-before-list-rebuilt");
        let tree = work_in_progress_on_main(&warehouse.root);

        BEFORE_PARKED_WRITE.with(|seam| seam.set(Some(Box::new(|| Err("crashed".to_string())))));
        assert_eq!(test_support::block_on(park_changes(None, &[])).unwrap_err(), "crashed");
        operation_utils::forget_raised_for_tests();
        remove_record();

        let rebuilt = park_utils::rebuild().unwrap().expect("the missing record is rebuilt");
        assert_eq!(rebuilt.recovered, 1, "{:?}", rebuilt);
        let parked = park_utils::read_parked().unwrap();
        assert!(parked.len() == 1 && parked[0].recovered, "{:?}", parked);

        let recovery = operation_utils::recover().unwrap().expect("a marker stands");
        assert_eq!((recovery.operation, recovery.outcome), (OperationKind::Park, RecoveryOutcome::Cleared));
        assert!(park_utils::read_parked().unwrap().is_empty(), "the recovered entry is taken out");
        assert!(test_support::file_holds(&warehouse.root, "a.txt", b"work\n"), "the work stays in the file");
        assert_eq!(staged_paths(&tree), vec!["a.txt".to_string()], "and staged, as it was");
    }

    /// Heal rebuilds the record before it recovers the interrupted park: recovery reads the
    /// record, so the other order refuses on the missing one.
    #[test]
    fn heal_rebuilds_the_parked_record_before_it_recovers_an_interrupted_park() {
        let warehouse = Warehouse::with_working_tree("park-heal-order");
        work_in_progress_on_main(&warehouse.root);

        BEFORE_PARKED_WRITE.with(|seam| seam.set(Some(Box::new(|| Err("crashed".to_string())))));
        assert_eq!(test_support::block_on(park_changes(None, &[])).unwrap_err(), "crashed");
        operation_utils::forget_raised_for_tests();
        remove_record();

        test_support::block_on(crate::commands::heal::handle_command(None, &[], &[], &[])).unwrap();

        assert!(park_utils::read_parked().unwrap().is_empty(), "the recovered entry is taken out");
        assert!(operation_utils::read().unwrap().is_none(), "the marker is cleared");
        assert!(test_support::file_holds(&warehouse.root, "a.txt", b"work\n"));
    }

    /// A park refused on a missing record stores no parcel: a rebuild then finds nothing parked.
    #[test]
    fn a_park_refused_on_a_missing_record_stores_no_parked_parcel() {
        let warehouse = Warehouse::with_working_tree("park-refused-missing");
        work_in_progress_on_main(&warehouse.root);
        remove_record();

        let error = test_support::block_on(park_changes(None, &[])).unwrap_err();
        assert!(error.contains("is missing") && error.contains("forklift heal"), "{}", error);

        let rebuilt = park_utils::rebuild().unwrap().expect("the missing record is rebuilt");
        assert_eq!(rebuilt.recovered, 0, "{:?}", rebuilt);
        assert!(park_utils::read_parked().unwrap().is_empty());
    }

    /// The parked parcel carries a Park action naming the park, which is what a store scan reads.
    #[test]
    fn a_parked_parcel_is_marked_with_its_name() {
        let warehouse = Warehouse::with_working_tree("park-marker");
        work_in_progress_on_main(&warehouse.root);
        test_support::block_on(park_changes(None, &[])).unwrap();

        let entry = park_utils::read_parked().unwrap().remove(0);
        let parcel = object_utils::load_parcel(&entry.parcel).unwrap();
        let marks: Vec<_> = parcel.actions.iter()
            .filter(|action| matches!(action.action, ParcelActionType::Park))
            .map(|action| action.description.clone())
            .collect();
        assert_eq!(marks, vec![Some(entry.name.clone())], "one Park action, naming the park");
        assert!(entry.name.starts_with("main "), "the default name starts with the pallet: {}", entry.name);
    }

    /// Every damage the record can take is rebuilt to the same parks in the same order: a flipped
    /// digest and a cut tail keep each entry as written, and a record emptied or deleted brings
    /// each back from the store, named from its Park action and flagged as recovered.
    #[test]
    fn a_damaged_record_is_rebuilt_to_the_same_parks_in_the_same_order() {
        let warehouse = Warehouse::with_working_tree("park-rebuild-damage");
        let parked = two_parks(&warehouse.root);
        let bytes = std::fs::read(park_utils::parked_path()).unwrap();

        let flipped = {
            let mut bytes = bytes.clone();
            // A hex digit of the digest, on the record's last line.
            let at = bytes.len() - 3;
            bytes[at] = if bytes[at] == b'0' { b'1' } else { b'0' };
            bytes
        };
        let cut = bytes[..bytes.len() - 1].to_vec();

        for (damage, written, kept) in [("flipped", Some(flipped), true), ("cut", Some(cut), true),
                                        ("emptied", Some(Vec::new()), false), ("deleted", None, false)] {
            match written {
                Some(written) => std::fs::write(park_utils::parked_path(), written).unwrap(),
                None => remove_record(),
            }
            assert!(park_utils::read_parked().is_err(), "{}: sanity: the record reads as damaged", damage);

            park_utils::rebuild().unwrap().unwrap_or_else(|| panic!("{}: the record is rebuilt", damage));
            let rebuilt = park_utils::read_parked().unwrap();

            let expected: Vec<_> = parked.iter()
                .map(|entry| park_utils::ParkedEntry { recovered: !kept, ..entry.clone() })
                .collect();
            assert_eq!(rebuilt, expected, "{}", damage);

            // Back to the record as written, for the next damage.
            std::fs::write(park_utils::parked_path(), &bytes).unwrap();
        }
    }

    /// A version-1 record lists bare hashes, of parcels parked as Stack-marked ones. Its entries
    /// are kept by the rebuild under a default name: a parcel the store holds is kept whatever
    /// marks it.
    #[test]
    fn a_version_1_record_keeps_its_parcels_under_a_default_name() {
        let _warehouse = Warehouse::with_working_tree("park-rebuild-v1");
        test_support::configure_operator();
        let parcel = test_support::store_parcel(&test_support::store_tree("a.txt", b"old\n"), &[]);
        std::fs::write(park_utils::parked_path(), format!("/forklift-parked 1\n{}\n", parcel)).unwrap();
        let error = park_utils::read_parked().unwrap_err();
        assert!(error.contains("older Forklift, as version 1") && error.contains("forklift heal"), "{}", error);

        let rebuilt = park_utils::rebuild().unwrap().expect("a version-1 record is rebuilt");
        assert_eq!((rebuilt.kept, rebuilt.recovered), (1, 0), "{:?}", rebuilt);
        let parked = park_utils::read_parked().unwrap();
        assert_eq!(parked.len(), 1);
        assert_eq!((parked[0].parcel.as_str(), parked[0].recovered), (parcel.as_str(), false));
        assert!(!parked[0].name.is_empty());
    }

    /// A bay's leftover parked file is merged into the shared record and then removed.
    #[test]
    fn a_bays_leftover_parked_file_is_merged_and_removed() {
        let warehouse = Warehouse::with_working_tree("park-rebuild-leftover");
        let parked = two_parks(&warehouse.root);

        let bay = forklift_core::util::bay_utils::bay_state_dir("b");
        std::fs::create_dir_all(&bay).unwrap();
        let leftover = bay.join(park_utils::FILE_NAME_PARKED);
        test_support::configure_operator();
        let old = test_support::store_parcel(&test_support::store_tree("b.txt", b"old\n"), &[]);
        std::fs::write(&leftover, format!("/forklift-parked 1\n{}\n", old)).unwrap();
        assert!(park_utils::read_parked().is_err(), "a leftover is damage");

        let rebuilt = park_utils::rebuild().unwrap().expect("the leftover is merged");
        assert_eq!(rebuilt.merged_bays, 1, "{:?}", rebuilt);
        assert_eq!((rebuilt.record, rebuilt.set_aside.as_ref()), (park_utils::RecordWas::Whole, None),
            "a record that read is not called damaged, nor copied aside");
        let now: Vec<_> = park_utils::read_parked().unwrap().into_iter().map(|entry| entry.parcel).collect();
        assert_eq!(now, vec![parked[0].parcel.clone(), parked[1].parcel.clone(), old], "kept first, then merged");
        assert!(!leftover.exists(), "the leftover is removed once merged");
    }

    /// A parcel the rebuild brings back goes in by the time it was parked, so the newest park
    /// stays last and `park pop` takes it. Parks A and B, B popped (its parcel stays in the
    /// store), then C: the rebuilt record is A, B (recovered), C. Revert the insert by time to an
    /// append and B lands last, so the next pop re-applies work already popped.
    #[test]
    fn a_rebuild_puts_a_recovered_parcel_by_its_time_and_keeps_the_newest_last() {
        // Park times are whole seconds, so the three parks are a second apart.
        let next_second = || std::thread::sleep(std::time::Duration::from_millis(1100));
        let warehouse = Warehouse::with_working_tree("park-rebuild-order");
        work_in_progress_on_main(&warehouse.root);
        test_support::block_on(park_changes(None, &[])).unwrap();
        next_second();
        std::fs::write(warehouse.root.join("a.txt"), b"b work\n").unwrap();
        test_support::block_on(crate::commands::load::handle_command(".")).unwrap();
        test_support::block_on(park_changes(None, &[])).unwrap();
        let popped = park_utils::read_parked().unwrap()[1].parcel.clone();
        pop_parked(None).unwrap();
        next_second();
        std::fs::write(warehouse.root.join("a.txt"), b"c work\n").unwrap();
        test_support::block_on(crate::commands::load::handle_command(".")).unwrap();
        test_support::block_on(park_changes(None, &[])).unwrap();
        let listed = park_utils::read_parked().unwrap();
        assert_eq!(listed.len(), 2, "sanity: A and C are listed");

        // Flip a hex digit of the digest, so every entry still reads and is kept.
        let mut bytes = std::fs::read(park_utils::parked_path()).unwrap();
        let at = bytes.len() - 3;
        bytes[at] = if bytes[at] == b'0' { b'1' } else { b'0' };
        std::fs::write(park_utils::parked_path(), bytes).unwrap();
        park_utils::rebuild().unwrap().expect("the damaged record is rebuilt");

        let rebuilt: Vec<_> = park_utils::read_parked().unwrap().into_iter()
            .map(|entry| (entry.parcel, entry.recovered)).collect();
        assert_eq!(rebuilt, vec![
            (listed[0].parcel.clone(), false), (popped, true), (listed[1].parcel.clone(), false),
        ]);
    }

    /// A pop whose park another command popped, dropped or parked over between its pick and its
    /// hold applies nothing. Revert the re-pick under the lock and the pop applies the work.
    #[test]
    fn a_pop_whose_park_was_taken_before_it_held_the_lock_applies_nothing() {
        let warehouse = Warehouse::with_working_tree("park-pop-raced");
        work_in_progress_on_main(&warehouse.root);
        test_support::block_on(park_changes(None, &[])).unwrap();

        BEFORE_POP_HOLDS.with(|seam| seam.set(Some(Box::new(|| {
            let lock = ParkedLock::acquire()?;
            park_utils::write_parked(&lock, &[])
        }))));
        let error = pop_parked(None).unwrap_err();
        assert!(error.contains("popped or dropped by another command"), "{}", error);
        assert!(error.contains("Nothing was applied"), "{}", error);
        assert!(test_support::file_holds(&warehouse.root, "a.txt", b"one\n"), "the head stays in the file");
    }

    /// A pop holds the parked-work lock from its re-pick through applying the changes, so a
    /// second bay's pop waits rather than applying the same park. Revert the hold and another
    /// thread takes the lock while the pop is applying.
    #[test]
    fn a_pop_holds_the_parked_lock_while_it_applies() {
        let warehouse = Warehouse::with_working_tree("park-pop-holds");
        work_in_progress_on_main(&warehouse.root);
        test_support::block_on(park_changes(None, &[])).unwrap();

        let root = warehouse.root.clone();
        BEFORE_POP_REMOVES.with(|seam| seam.set(Some(Box::new(move || {
            assert!(test_support::file_holds(&root, "a.txt", b"work\n"), "the changes are applied");
            let other = std::thread::scope(|scope| scope.spawn(|| {
                let _scope = forklift_core::globals::StorageRootScope::enter(&root);
                ParkedLock::try_acquire().map(|lock| lock.is_some())
            }).join().unwrap())?;
            assert!(!other, "another bay cannot take the lock while the pop applies");
            Ok(())
        }))));
        pop_parked(None).unwrap();
        assert!(park_utils::read_parked().unwrap().is_empty(), "the pop took the park off the list");
    }

    /// A pop takes exactly the park it picked: a park added meanwhile, even one naming the same
    /// parcel, stays listed. Revert to re-picking "the newest" under the lock and the pop refuses;
    /// revert to removing by parcel and the other name goes too.
    #[test]
    fn a_pop_takes_only_its_own_park_when_another_was_added_meanwhile() {
        let warehouse = Warehouse::with_working_tree("park-pop-added");
        work_in_progress_on_main(&warehouse.root);
        test_support::block_on(park_changes(Some("mine"), &[])).unwrap();

        BEFORE_POP_HOLDS.with(|seam| seam.set(Some(Box::new(|| {
            let lock = ParkedLock::acquire()?;
            let mut parked = park_utils::read_parked()?;
            let mine = parked[0].clone();
            parked.push(park_utils::ParkedEntry { name: "twin".to_string(), ..mine.clone() });
            parked.push(park_utils::ParkedEntry { name: "newer".to_string(), parcel: "0".repeat(64), ..mine });
            park_utils::write_parked(&lock, &parked)
        }))));
        pop_parked(None).unwrap();

        assert!(test_support::file_holds(&warehouse.root, "a.txt", b"work\n"), "the pop applied its park");
        let names: Vec<_> = park_utils::read_parked().unwrap().into_iter().map(|entry| entry.name).collect();
        assert_eq!(names, vec!["twin", "newer"]);
    }

    /// Apply keeps the park listed; pop by name takes that park, not the newest; drop leaves the
    /// working files alone. Revert apply's keep and the list empties.
    #[test]
    fn apply_keeps_a_park_pop_takes_the_one_named_and_drop_leaves_the_files() {
        let warehouse = Warehouse::with_working_tree("park-named");
        work_in_progress_on_main(&warehouse.root);
        test_support::block_on(park_changes(Some("first"), &[])).unwrap();
        std::fs::write(warehouse.root.join("a.txt"), b"second\n").unwrap();
        test_support::block_on(crate::commands::load::handle_command(".")).unwrap();
        test_support::block_on(park_changes(Some("second"), &[])).unwrap();

        let names = || park_utils::read_parked().unwrap().into_iter().map(|entry| entry.name).collect::<Vec<_>>();
        assert_eq!(names(), vec!["first", "second"]);

        apply_parked(Some("first")).unwrap();
        assert!(test_support::file_holds(&warehouse.root, "a.txt", b"work\n"), "the named park is applied");
        assert_eq!(names(), vec!["first", "second"], "apply keeps it parked");

        drop_parked(None).unwrap();
        assert_eq!(names(), vec!["first"], "drop takes the newest");
        assert!(test_support::file_holds(&warehouse.root, "a.txt", b"work\n"), "and leaves the files alone");

        assert!(drop_parked(Some("second")).unwrap_err().contains("No work is parked as \"second\""));
    }

    /// A name already parked is refused before anything is parked. Revert the check and the
    /// park reaches the hold's collision refusal after signing a parcel.
    #[test]
    fn a_park_under_a_name_already_parked_is_refused_with_the_work_in_place() {
        let warehouse = Warehouse::with_working_tree("park-name-taken");
        work_in_progress_on_main(&warehouse.root);
        test_support::block_on(park_changes(Some("wip"), &[])).unwrap();
        std::fs::write(warehouse.root.join("a.txt"), b"more\n").unwrap();
        test_support::block_on(crate::commands::load::handle_command(".")).unwrap();

        let error = test_support::block_on(park_changes(Some("wip"), &[])).unwrap_err();
        assert!(error.contains("Work is already parked as \"wip\""), "{}", error);
        assert!(test_support::file_holds(&warehouse.root, "a.txt", b"more\n"), "the work stays");
        assert_eq!(park_utils::read_parked().unwrap().len(), 1);
    }

    /// Show names the park's base and each file it changes, by kind.
    #[test]
    fn show_lists_each_file_a_park_changes() {
        let warehouse = Warehouse::with_working_tree("park-show");
        work_in_progress_on_main(&warehouse.root);
        std::fs::write(warehouse.root.join("b.txt"), b"new\n").unwrap();
        test_support::block_on(crate::commands::load::handle_command(".")).unwrap();
        test_support::block_on(park_changes(Some("wip"), &[])).unwrap();

        let shown = park_show(Some("wip")).unwrap();
        assert_eq!(shown.base, pallet_utils::get_pallet_head("main").unwrap());
        assert!(shown.parked_at.is_some());
        let files: Vec<_> = shown.files.iter().map(|file| (file.path.as_str(), file.change.to_string(), file.outside_scope)).collect();
        assert_eq!(files, vec![("a.txt", "modified".to_string(), false), ("b.txt", "added".to_string(), false)]);
    }

    /// A store whose history cannot be walked does not stop the rebuild: a parked parcel is
    /// listed as recovered even though whether a ref reaches it cannot be told. Revert to
    /// propagating the walk's error and the rebuild fails, leaving every reader refusing.
    #[test]
    fn a_rebuild_lists_parked_parcels_when_the_history_cannot_be_walked() {
        let warehouse = Warehouse::with_working_tree("park-rebuild-unwalkable");
        work_in_progress_on_main(&warehouse.root);
        test_support::block_on(park_changes(None, &[])).unwrap();
        let parked = park_utils::read_parked().unwrap().remove(0);
        remove_record();

        // The head parcel is present but corrupt, so the walk from "main" cannot finish.
        let head = pallet_utils::get_pallet_head("main").unwrap().unwrap();
        let head_path = warehouse.root.join(".forklift/objects").join(&head[..2]).join(&head[2..]);
        std::fs::write(&head_path, b"not a parcel").unwrap();

        let rebuilt = park_utils::rebuild().unwrap().expect("the missing record is rebuilt");
        assert_eq!(rebuilt.recovered, 1, "{:?}", rebuilt);
        assert!(rebuilt.unreadable >= 1, "the walk that failed is counted: {:?}", rebuilt);
        assert_eq!(park_utils::read_parked().unwrap()[0].parcel, parked.parcel);
    }

    /// Store a tree of `folders` (each a name and its flat files) beside the flat `files`; returns
    /// the tree's hash.
    fn store_folders(folders: &[(&str, &[(&str, &[u8])])], files: &[(&str, &[u8])]) -> String {
        use forklift_core::enums::dir_entry_type::DirEntryType;
        use forklift_core::model::tree_item::TreeItem;

        let flat = object_utils::load_tree(&test_support::store_files(files)).unwrap();
        let mut root = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        for (_, file) in flat.get_files() {
            root.add_child(TreeItem::new(file.name.clone(), file.hash.clone(), file.item_type));
        }
        for (name, children) in folders {
            root.add_child(TreeItem::new(name.to_string(), test_support::store_files(children), DirEntryType::Tree));
        }
        let mut tree = LooseObjectBuilder::build_tree(&root);
        tree.store().unwrap();
        tree.hash
    }

    /// "main" at a tree of a/x, b/y, b/z and top.txt, checked out, with a/x edited and loaded, and
    /// b/y edited, loaded, and edited again. Returns the head's tree.
    fn work_in_two_folders(root: &std::path::Path) -> String {
        test_support::configure_operator();

        let tree = store_folders(
            &[("a", &[("x", b"one\n")]), ("b", &[("y", b"one\n"), ("z", b"one\n")])],
            &[("top.txt", b"one\n")],
        );
        let head = test_support::store_parcel(&tree, &[]);
        pallet_utils::update_pallet_head("main", None, &head, &pallet_utils::RefPreconditions::NONE).unwrap();
        pallet_utils::write_position(&pallet_utils::BayPosition::new("main", Some(head))).unwrap();
        test_support::check_out(&tree);

        std::fs::write(root.join("a/x"), b"work\n").unwrap();
        std::fs::write(root.join("b/y"), b"staged\n").unwrap();
        test_support::block_on(crate::commands::load::handle_command(".")).unwrap();
        std::fs::write(root.join("b/y"), b"unstaged\n").unwrap();

        tree
    }

    fn unstaged_paths() -> Vec<String> {
        test_support::block_on(stocktake_utils::collect_unstaged_changes()).unwrap()
            .into_iter().map(|change| change.path).collect()
    }

    fn subtree_hash(tree: &str, name: &str) -> String {
        object_utils::load_tree(tree).unwrap().get_subtree(name).unwrap().hash.clone()
    }

    /// A park of "a" parks and resets only a: b keeps its staged change and its working one, and
    /// the parked tree holds the head's b, not the staged one. Revert the splice to park the
    /// staged tree whole and the parked b is the staged one; revert the reset to the whole tree
    /// and b's staged change is gone.
    #[test]
    fn a_partial_park_parks_and_resets_only_its_paths() {
        let warehouse = Warehouse::with_working_tree("park-partial");
        let tree = work_in_two_folders(&warehouse.root);

        test_support::block_on(park_changes(Some("wip"), &["a".to_string()])).unwrap();

        assert!(test_support::file_holds(&warehouse.root, "a/x", b"one\n"), "a is back at the head");
        assert!(test_support::file_holds(&warehouse.root, "b/y", b"unstaged\n"), "b keeps its working content");
        assert_eq!(staged_paths(&tree), vec!["b/y".to_string()], "and its staging");
        assert_eq!(unstaged_paths(), vec!["b/y".to_string()]);

        let parked = park_utils::read_parked().unwrap().remove(0);
        let parked_tree = object_utils::load_parcel(&parked.parcel).unwrap().tree_hash;
        assert_eq!(subtree_hash(&parked_tree, "b"), subtree_hash(&tree, "b"), "the parked b is the head's");
        assert_ne!(subtree_hash(&parked_tree, "a"), subtree_hash(&tree, "a"), "the parked a holds the work");

        // Popped, the work in a comes back staged beside b's.
        pop_parked(Some("wip")).unwrap();
        assert!(test_support::file_holds(&warehouse.root, "a/x", b"work\n"));
        assert_eq!(staged_paths(&tree), vec!["a/x".to_string(), "b/y".to_string()]);
    }

    /// A partial park interrupted once it was listed rolls forward over its paths only: the
    /// recovery resets a, and b keeps its staged and working changes. Revert recovery to rebuild
    /// staging whole and b's staged change is gone.
    #[test]
    fn a_partial_park_interrupted_after_it_was_listed_rolls_forward_over_its_paths_only() {
        let warehouse = Warehouse::with_working_tree("park-partial-crash-after-listed");
        let tree = work_in_two_folders(&warehouse.root);

        AFTER_LISTED.with(|seam| seam.set(Some(Box::new(|| Err("crashed".to_string())))));
        assert_eq!(test_support::block_on(park_changes(None, &["a".to_string()])).unwrap_err(), "crashed");
        operation_utils::forget_raised_for_tests();

        let recovery = operation_utils::recover().unwrap().expect("a marker stands");
        assert_eq!((recovery.operation, recovery.outcome), (OperationKind::Park, RecoveryOutcome::RolledForward));

        assert!(test_support::file_holds(&warehouse.root, "a/x", b"one\n"), "a is back at the head");
        assert!(test_support::file_holds(&warehouse.root, "b/y", b"unstaged\n"), "b keeps its working content");
        assert_eq!(staged_paths(&tree), vec!["b/y".to_string()], "and its staging");
    }

    /// A file can be parked alone: its neighbour's staged change stays.
    #[test]
    fn a_partial_park_of_one_file_leaves_its_neighbour_staged() {
        let warehouse = Warehouse::with_working_tree("park-partial-file");
        let tree = work_in_progress_on_main(&warehouse.root);
        std::fs::write(warehouse.root.join("b.txt"), b"new\n").unwrap();
        test_support::block_on(crate::commands::load::handle_command(".")).unwrap();

        test_support::block_on(park_changes(None, &["a.txt".to_string()])).unwrap();

        assert!(test_support::file_holds(&warehouse.root, "a.txt", b"one\n"));
        assert!(test_support::file_holds(&warehouse.root, "b.txt", b"new\n"));
        assert_eq!(staged_paths(&tree), vec!["b.txt".to_string()]);
        let shown = park_show(None).unwrap();
        let files: Vec<_> = shown.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(files, vec!["a.txt"]);
    }

    /// Paths with nothing to park refuse and park nothing: unchanged ones, and one that names
    /// nothing, which would otherwise be parked as nothing beside the others.
    #[test]
    fn a_partial_park_refuses_paths_with_nothing_to_park() {
        let warehouse = Warehouse::with_working_tree("park-partial-nothing");
        let tree = work_in_two_folders(&warehouse.root);

        let error = test_support::block_on(park_changes(None, &["top.txt".to_string()])).unwrap_err();
        assert_eq!(error, "There is nothing to park under those paths: they match the pallet head.");

        let error = test_support::block_on(park_changes(None, &["a".to_string(), "nope".to_string()])).unwrap_err();
        assert!(error.starts_with("\"nope\" names nothing in the warehouse"), "{}", error);

        assert!(park_utils::read_parked().unwrap().is_empty());
        assert_eq!(staged_paths(&tree), vec!["a/x".to_string(), "b/y".to_string()]);
        assert!(test_support::file_holds(&warehouse.root, "a/x", b"work\n"));
    }

    /// The paths are taken as keys: one under another goes, and the root parks everything.
    #[test]
    fn a_pathspec_drops_covered_paths_and_the_root_is_a_whole_park() {
        let _warehouse = Warehouse::with_working_tree("park-pathspec");

        let raw = |paths: &[&str]| paths.iter().map(|path| path.to_string()).collect::<Vec<_>>();
        assert_eq!(pathspec(&raw(&["b", "a/x", "./a", "b"])).unwrap(), raw(&["a", "b"]));
        assert_eq!(pathspec(&raw(&["a/x", "."])).unwrap(), Vec::<String>::new());
        assert_eq!(pathspec(&[]).unwrap(), Vec::<String>::new());
    }
}
