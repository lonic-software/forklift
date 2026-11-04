use std::collections::{BTreeMap, BTreeSet};
use serde::Serialize;
use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
use forklift_core::enums::dir_entry_type::DirEntryType;
use forklift_core::enums::inventory_item_state::InventoryItemState;
use forklift_core::error::{CoreError, RefusalCode};
use forklift_core::model::blob::Blob;
use forklift_core::util::merge_utils::MergeAction;
use forklift_core::util::operation_utils::{self, Operation, OperationKind};
use forklift_core::util::shift_utils::FileOp;
use forklift_core::util::{
    cherry_pick_utils, file_utils, inventory_utils, journal_utils, load_guard_utils, merge_utils, object_utils,
    pallet_utils, scope_utils, shift_utils, stocktake_utils,
};
use crate::commands::{consolidate, restore};
use crate::output::{self, CommandOutput};

/// Which kind of in-progress merge an abort abandons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeKind {
    /// A consolidation, started by `consolidate` or a haul's consolidate.
    Consolidation,

    /// A pick.
    Pick,
}

/// The command that aborts a merge in progress, as the user types it.
pub const ABORT_COMMAND: &str = "forklift abort";

impl MergeKind {
    fn noun(self) -> &'static str {
        match self {
            MergeKind::Consolidation => "consolidation",
            MergeKind::Pick => "pick",
        }
    }
}

/// Handle `abort`: abandon the consolidation or pick in progress, putting back what it changed
/// (see [`abort`]), and report it. A bay holds at most one: each refuses to start over the other.
///
/// # Arguments
/// * `confirm` - Whether to discard the changes made since the merge to the files it touched.
///
/// # Returns
/// * `Ok(())`      - If the merge was aborted.
/// * `Err(String)` - If no merge is in progress, the abort would discard changes made since the
///                   merge and `confirm` is not set, or the abort failed.
pub async fn handle_command(confirm: bool) -> Result<(), String> {
    let kind = match (merge_utils::read_consolidation_state()?, cherry_pick_utils::read_state()?) {
        (Some(_), _) => MergeKind::Consolidation,
        (None, Some(_)) => MergeKind::Pick,
        (None, None) => return Err("No consolidation or pick is in progress; there is nothing to abort.".to_string()),
    };

    let aborted = abort(kind, confirm, ABORT_COMMAND).await?;
    output::emit(&aborted);
    Ok(())
}

/// Abort the `kind` of merge in progress in this bay: every path it touched goes back to the head
/// it was built on, in the working directory and the inventory, and its in-progress state is
/// cleared. A change staged since to a path it left alone stays staged, an unstaged one stays
/// unstaged, and such paths are never written.
///
/// A touched path may hold its pre-merge content or what the merge wrote there and be put back
/// freely: the first is what the abort restores, the second what consolidating again writes. Any
/// other content (a resolution, loaded or not; a file re-created where the merge deleted it) is
/// work the merge cannot reproduce, so it is listed and nothing is changed unless `confirm` is set.
/// The merge is recomputed from the recorded state to know what it wrote; should the recompute
/// differ from the original (the bay's scope changed since), it can only list more, or leave a
/// path the original wrote as a local change, carried like any other.
///
/// The abort runs under a merge's operation marker, so `heal` finishes one that was interrupted
/// the way it rolls back an interrupted merge (see `operation_utils`). The journal entry of the
/// operation that started the merge is popped before the first change (see
/// [`journal_utils::pop_if_it_left_in_progress`]), so `undo` reaches past it.
///
/// # Arguments
/// * `kind`    - Which kind of merge to abort.
/// * `confirm` - Whether to discard the changes made since the merge to the files it touched.
/// * `command` - The command the confirmation refusal names, without `--confirm`.
///
/// # Returns
/// * `Ok(Aborted)` - What was aborted.
/// * `Err(String)` - If none of that kind is in progress, the bay is not where the merge left it,
///                   the abort would discard changes and `confirm` is not set, or it failed.
pub(crate) async fn abort(kind: MergeKind, confirm: bool, command: &str) -> Result<Aborted, String> {
    let consolidation = merge_utils::read_consolidation_state()?;
    let pick = cherry_pick_utils::read_state()?;

    let recorded = match (kind, consolidation, pick) {
        (MergeKind::Consolidation, Some(state), _) => Recorded {
            our_head: state.our_head,
            our_pallet: state.our_pallet,
            other: state.their_pallet,
            source: Source::Pallet(state.their_head),
        },
        (MergeKind::Pick, _, Some(state)) => Recorded {
            our_head: state.our_head,
            our_pallet: state.our_pallet,
            other: state.source.clone(),
            source: Source::Parcel(state.source),
        },
        _ => return Err(format!("No {} is in progress; there is nothing to abort.", kind.noun())),
    };

    load_guard_utils::check_no_incomplete_load()?;

    let position = pallet_utils::read_position()?;
    if position.pallet != recorded.our_pallet || position.base.as_deref() != Some(recorded.our_head.as_str()) {
        return Err(format!(
            "The {} was started on pallet \"{}\" at {}, but this bay stands on \"{}\" at {}. Nothing was \
            aborted: the abort puts back that head's files, and would put them over another one's. To \
            abandon it and leave every file as it is, {}.",
            kind.noun(), recorded.our_pallet, recorded.our_head, position.pallet,
            position.base.as_deref().unwrap_or("no parcel"), removal_hint(kind)
        ));
    }

    let our_tree = object_utils::load_parcel(&recorded.our_head)?.tree_hash;
    let (written, their_tree) = merge_output(&recorded, &our_tree).map_err(|e| format!(
        "Cannot abort the {}: what it wrote cannot be worked out ({}). Nothing was aborted. To abandon it \
        and leave every file as it is, {}.",
        kind.noun(), e, removal_hint(kind)
    ))?;

    // Every path the merge wrote, and every conflict it left staged whatever the recompute says.
    let mut touched: BTreeSet<String> = written.keys().cloned().collect();
    touched.extend(inventory_utils::list_conflict_paths()?);

    let anchor = file_utils::take_stat_anchor()?;
    let mut shards: BTreeMap<String, forklift_core::model::inventory::Inventory> = BTreeMap::new();
    let mut ops = Vec::new();
    let mut removed = Vec::new();
    let mut discards = Vec::new();
    let mut folders = Vec::new();

    for path in &touched {
        let ours = object_utils::resolve_tree_file(&our_tree, path)?;
        let output = written.get(path);

        // A folder where the head has a file: the other side's files in it are removed below, and
        // anything else in it is checked once they are known.
        let is_folder = std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir());
        if is_folder {
            folders.push(path.clone());
        }

        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
        if !shards.contains_key(dir) {
            shards.insert(dir.to_string(), stocktake_utils::load_shard_or_empty(dir)?);
        }
        let entry = shards[dir].get_item_by_name(name)
            .filter(|item| item.state != InventoryItemState::Deleted)
            .map(|item| (item.hash.clone(), item.item_type));

        let working_kept = is_folder
            || holds(path, ours.as_ref(), anchor)
            || output.is_some_and(|output| holds(path, output.working.as_ref(), anchor));
        let entry_kept = entry == ours || output.is_some_and(|output| entry == output.entry);

        if !(working_kept && entry_kept) {
            discards.push(path.clone());
        }

        match ours {
            Some((hash, item_type)) => ops.push(FileOp::Write { path: path.clone(), hash, item_type, is_new: false }),
            None if !is_folder && std::fs::symlink_metadata(path).is_ok() => {
                ops.push(FileOp::Remove { path: path.clone() });
                removed.push(path.clone());
            },
            None => removed.push(path.clone()),
        }
    }

    // Changes to paths the merge left alone are carried across the rebuild, as `lower` carries
    // them; one the put-back would reach (a file where it writes a folder, say) cannot be.
    let carried: Vec<shift_utils::CarriedEntry> = shift_utils::collect_carried(Some(&our_tree)).await?
        .into_iter()
        .filter(|carried| !touched.contains(&carried.path))
        .collect();
    let removed_dirs = emptied_directories(&removed, &our_tree)?;

    // Checked before the first write: a file the abort does not remove would keep the folder in
    // place, and the head's file could not be written there.
    for folder in &folders {
        if let Some(path) = first_file_not_in(folder, &removed)? {
            return Err(format!(
                "Cannot abort the {}: \"{}\" lies in the folder \"{}\", where the head has a file, and \
                the abort would have to delete it. Move it aside first.",
                kind.noun(), path, folder
            ));
        }
    }

    if let Some(path) = shift_utils::carried_overlaps(&carried, &ops, &removed_dirs).first() {
        return Err(format!(
            "Cannot abort the {}: the local change to \"{}\" lies where the {} put a file or folder, \
            and putting it back would overwrite the change. Move it aside first.",
            kind.noun(), path, kind.noun()
        ));
    }

    if !discards.is_empty() && !confirm {
        return Err(discard_refusal(kind, &discards, command));
    }

    shift_utils::ensure_writes_present(&ops)?;

    // Popped before the first change: the abort reverses that operation itself, and an abort that
    // fails from here on leaves the merge to `heal` or to another abort, never to `undo`.
    journal_utils::pop_if_it_left_in_progress()?;

    // Interrupted from here, `heal` finishes the abort as it rolls back a merge: every file at the
    // other side's content goes back to `our_tree`, any other stays a local change, the carried
    // changes keep their entries, and the in-progress state is cleared.
    let kind_of_operation = match kind {
        MergeKind::Consolidation => OperationKind::Merge,
        MergeKind::Pick => OperationKind::CherryPick,
    };
    let mut operation = Operation::new(
        kind_of_operation,
        pallet_utils::BayPosition::new(&recorded.our_pallet, Some(recorded.our_head.clone())),
        Some(our_tree.clone()),
        pallet_utils::BayPosition::new(&recorded.our_pallet, None),
    );
    operation.their_tree = Some(their_tree);
    operation.carried = carried.clone();
    operation_utils::raise(&operation)?;

    shift_utils::apply_ops(&ops, &removed_dirs)?;

    let built = shift_utils::build_inventories_carrying(&our_tree, shift_utils::TreeProvenance::Written(anchor), &carried)?;
    inventory_utils::replace_all_inventories(&built.shards)?;

    #[cfg(test)]
    tests::BEFORE_STATE_CLEAR.with(|seam| seam.take().map(|seam| seam())).transpose()?;

    // As `stack` orders it: the state before the skeleton.
    match kind {
        MergeKind::Consolidation => {
            merge_utils::clear_consolidation_state()?;
            merge_utils::OutOfScopeSkeleton::clear()?;
        },
        MergeKind::Pick => cherry_pick_utils::clear_state()?,
    }

    operation_utils::finish()?;

    Ok(Aborted {
        kind: kind.noun().to_string(),
        other: recorded.other,
        pallet: recorded.our_pallet,
        head: recorded.our_head,
        restored: touched.len(),
        discarded: if confirm { discards } else { Vec::new() },
    })
}

/// What a merge in progress recorded about itself.
struct Recorded {
    our_head: String,
    our_pallet: String,

    /// The other side, for the report: the consolidated pallet's label, or the picked parcel.
    other: String,

    source: Source,
}

/// Where a merge's other side came from.
enum Source {
    /// A consolidation: the head merged in.
    Pallet(String),

    /// A pick: the parcel applied.
    Parcel(String),
}

/// What a merge wrote at one path: the working content and the inventory entry, each a hash and
/// type, or `None` for absent.
struct Written {
    working: Option<(String, DirEntryType)>,
    entry: Option<(String, DirEntryType)>,
}

/// Recompute the merge `recorded` describes on `our_tree`, as `consolidate` and `pick` built it,
/// and return what it wrote at each in-scope path it touched.
fn merge_output(recorded: &Recorded, our_tree: &str) -> Result<(BTreeMap<String, Written>, String), String> {
    let (base_tree, their_tree, label, scope) = match &recorded.source {
        Source::Pallet(their_head) => {
            let their_tree = object_utils::load_parcel(their_head)?.tree_hash;
            let Some(base) = merge_utils::find_merge_base(&recorded.our_head, their_head)? else {
                return Ok((BTreeMap::new(), their_tree));
            };

            (
                object_utils::load_parcel(&base)?.tree_hash,
                their_tree,
                recorded.other.clone(),
                scope_utils::current_scope()?,
            )
        },
        Source::Parcel(source) => {
            let parcel = object_utils::load_parcel(source)?;
            let base_tree = match parcel.parents.first() {
                Some(parent) => object_utils::load_parcel(parent)?.tree_hash,
                None => object_utils::empty_tree_hash()?,
            };

            (base_tree, parcel.tree_hash, format!("pick {}", &source[..source.len().min(10)]), scope_utils::PathScope::full())
        },
    };

    let mut actions = merge_utils::compute_merge_actions(&base_tree, our_tree, &their_tree, &recorded.our_pallet, &label, &scope)?;
    consolidate::keep_aliased_files(&mut actions, our_tree, &scope)?;

    let blob_hash = |content: &[u8]| LooseObjectBuilder::build_blob(&Blob { content: content.to_vec() }).hash;

    let written = actions.into_iter().filter_map(|action| match action {
        MergeAction::TakeTheirs { path, hash, item_type, .. } => {
            Some((path, Written { working: Some((hash.clone(), item_type)), entry: Some((hash, item_type)) }))
        },
        MergeAction::Delete { path } => Some((path, Written { working: None, entry: None })),
        MergeAction::Merged { path, content, item_type } => {
            let hash = blob_hash(&content);
            Some((path, Written { working: Some((hash.clone(), item_type)), entry: Some((hash, item_type)) }))
        },
        MergeAction::Conflict { path, content, entry_hash, item_type } => {
            // Without content, a chunked file is written from its recipe and any other keeps ours,
            // which the abort treats as kept anyway.
            let working = match &content {
                Some(content) => Some((blob_hash(content), item_type)),
                None => Some((entry_hash.clone(), item_type)),
            };
            Some((path, Written { working, entry: Some((entry_hash, item_type)) }))
        },
        MergeAction::ResolveOutOfScope { .. } => None,
    }).collect();

    Ok((written, their_tree))
}

/// Whether the working file at `path` holds `expected`: its content and kind, or absence.
fn holds(path: &str, expected: Option<&(String, DirEntryType)>, anchor: file_utils::StatAnchor) -> bool {
    match expected {
        None => std::fs::symlink_metadata(path).is_err(),
        Some((hash, item_type)) => {
            let name = path.rsplit_once('/').map_or(path, |(_, name)| name);
            let item = inventory_utils::build_stale_inventory_item(name, hash.clone(), *item_type);
            restore::matching_content(path, &item, anchor).is_some()
        },
    }
}

/// The first file or link under `folder` (never followed) that is not in `removed`, if any.
fn first_file_not_in(folder: &str, removed: &[String]) -> Result<Option<String>, String> {
    let entries = std::fs::read_dir(folder).map_err(|e| format!("Error while reading \"{}\": {}", folder, e))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("Error while reading \"{}\": {}", folder, e))?;
        let path = format!("{}/{}", folder, entry.file_name().to_string_lossy());
        let is_dir = std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir());

        let found = match is_dir {
            true => first_file_not_in(&path, removed)?,
            false => (!removed.contains(&path)).then_some(path),
        };
        if found.is_some() {
            return Ok(found);
        }
    }

    Ok(None)
}

/// The folders that removing `removed` may leave empty and `our_tree` does not hold, deepest
/// first, for [`shift_utils::apply_ops`] to remove where they are empty.
fn emptied_directories(removed: &[String], our_tree: &str) -> Result<Vec<String>, String> {
    let mut dirs = BTreeSet::new();

    for path in removed {
        let mut rest = path.as_str();
        while let Some((parent, _)) = rest.rsplit_once('/') {
            if forklift_core::util::tree_utils::resolve_subtree_hash(our_tree, parent)?.is_some() {
                break;
            }
            dirs.insert(parent.to_string());
            rest = parent;
        }
    }

    let mut dirs: Vec<String> = dirs.into_iter().collect();
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.matches('/').count()));

    Ok(dirs)
}

/// How to abandon the `kind` of merge without the abort: remove its state files.
fn removal_hint(kind: MergeKind) -> String {
    match kind {
        MergeKind::Consolidation => merge_utils::state_removal_hint_in(&forklift_core::globals::bay_root()),
        MergeKind::Pick => cherry_pick_utils::state_removal_hint(),
    }
}

/// The `confirmation_required` refusal for an abort that would discard the changes made since the
/// merge to `files`, which its details list, naming `command` with `--confirm`.
fn discard_refusal(kind: MergeKind, files: &[String], command: &str) -> String {
    let shown: Vec<String> = files.iter().take(20).map(|file| format!("  {}", file)).collect();
    let more = match files.len() > shown.len() {
        true => format!("\n  ... and {} more", files.len() - shown.len()),
        false => String::new(),
    };

    crate::human!("Aborting the {} would discard the changes made since it started to:\n{}{}", kind.noun(), shown.join("\n"), more);

    CoreError::refusal_with_details(
        RefusalCode::ConfirmationRequired,
        format!("Aborting the {} would discard the changes to {} file(s); nothing was changed.", kind.noun(), files.len()),
        format!(
            "Keep any change you want first (copy it aside), then run \"{} --confirm\" to discard the rest.",
            command
        ),
        serde_json::json!({ "kind": kind.noun(), "discards": files }),
    ).into()
}

/// The result of an abort.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Debug, Serialize)]
pub(crate) struct Aborted {
    /// What was aborted: `consolidation` or `pick`.
    kind: String,

    /// The other side: the consolidated pallet (or haul source), or the picked parcel.
    other: String,

    /// The pallet the merge was started on, current still.
    pub(crate) pallet: String,

    /// The pallet head the touched files are back at.
    pub(crate) head: String,

    /// How many paths the merge touched, each now back at the head.
    pub(crate) restored: usize,

    /// With --confirm: the files whose changes since the merge were discarded.
    discarded: Vec<String>,
}

impl CommandOutput for Aborted {
    fn render_human(&self) {
        let other = match self.kind.as_str() {
            "pick" => format!("of {}", &self.other[..self.other.len().min(10)]),
            _ => format!("of \"{}\"", forklift_core::error::sanitize(&self.other)),
        };

        println!("Aborted the {} {} into \"{}\".", self.kind, other, self.pallet);
        println!("The {} file(s) it touched are back at the head ({}).", self.restored, self.head);

        if !self.discarded.is_empty() {
            println!("Discarded the changes made since it started to {} file(s).", self.discarded.len());
        }
    }
}

/// The `--json` `data` schema(s) the abort forms emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<Aborted>(),
    ]
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use forklift_core::util::operation_utils::RecoveryOutcome;
    use forklift_core::util::stocktake_utils;
    use super::*;
    use crate::test_support::{self, Warehouse};

    type Seam = Option<Box<dyn FnOnce() -> Result<(), String>>>;

    thread_local! {
        /// Run after the abort rebuilt the inventory and before it clears the in-progress state:
        /// where an interrupted abort stops.
        pub(super) static BEFORE_STATE_CLEAR: RefCell<Seam> = RefCell::new(None);
    }

    /// An abort interrupted after it put the files back, with the consolidation state still
    /// recorded, must not read as a take-ours resolution: `stack` would record a merge parcel
    /// holding none of their changes. The marker refuses the stack, and `heal` finishes the abort.
    #[test]
    fn an_abort_interrupted_before_clearing_the_state_cannot_be_stacked_and_heal_finishes_it() {
        let warehouse = Warehouse::with_working_tree("merge-abort-crash");
        test_support::configure_operator();

        let base = test_support::store_parcel(&test_support::store_files(&[("a.txt", b"base\n"), ("b.txt", b"base\n")]), &[]);
        let our_tree = test_support::store_files(&[("a.txt", b"ours\n"), ("b.txt", b"base\n")]);
        let our_head = test_support::store_parcel(&our_tree, &[&base]);
        let their_head = test_support::store_parcel(
            &test_support::store_files(&[("a.txt", b"theirs\n"), ("b.txt", b"theirs\n"), ("new.txt", b"new\n")]),
            &[&base],
        );

        pallet_utils::update_pallet_head("main", None, &our_head, &pallet_utils::RefPreconditions::NONE).unwrap();
        pallet_utils::write_position(&pallet_utils::BayPosition::new("main", Some(our_head.clone()))).unwrap();
        pallet_utils::update_pallet_head("feature", None, &their_head, &pallet_utils::RefPreconditions::NONE).unwrap();
        test_support::check_out(&our_tree);

        test_support::block_on(consolidate::handle_command("feature")).unwrap();
        assert!(merge_utils::read_consolidation_state().unwrap().is_some(), "sanity: the merge stopped at a conflict");

        BEFORE_STATE_CLEAR.with(|seam| seam.replace(Some(Box::new(|| Err("crashed".to_string())))));
        let error = test_support::block_on(abort(MergeKind::Consolidation, false, ABORT_COMMAND))
            .unwrap_err();
        assert_eq!(error, "crashed");
        operation_utils::forget_raised_for_tests();

        assert!(merge_utils::read_consolidation_state().unwrap().is_some(), "sanity: the crash left the state recorded");
        let refused = test_support::block_on(crate::commands::stack::handle_command(Some("after crash".to_string())))
            .expect_err("a stack over an interrupted abort must be refused");
        assert!(refused.contains("forklift heal"), "{}", refused);
        assert_eq!(pallet_utils::get_pallet_head("main").unwrap().as_deref(), Some(our_head.as_str()));

        let recovery = operation_utils::recover().unwrap().expect("a marker stands");
        assert_eq!((recovery.operation, recovery.outcome), (OperationKind::Merge, RecoveryOutcome::RolledBack));

        assert!(test_support::file_holds(&warehouse.root, "a.txt", b"ours\n")
                && test_support::file_holds(&warehouse.root, "b.txt", b"base\n")
                && !warehouse.root.join("new.txt").exists(),
            "the working tree must be back at the head the merge started from");
        let staged = test_support::block_on(stocktake_utils::collect_staged_changes(Some(&our_tree))).unwrap();
        let unstaged = test_support::block_on(stocktake_utils::collect_unstaged_changes()).unwrap();
        assert!(staged.is_empty() && unstaged.is_empty(),
            "the inventory must be back at our tree: staged {:?}, unstaged {:?}", staged.len(), unstaged.len());
        assert!(merge_utils::read_consolidation_state().unwrap().is_none(), "heal clears the consolidation");
        assert!(operation_utils::read().unwrap().is_none(), "recovery clears the marker");
    }

    /// The abort carries the changes made to paths the merge left alone, and `heal` finishing an
    /// interrupted one must keep them, even an edit back to the other side's content or a
    /// deletion, which a merge's own roll-back would take for its output and put back.
    #[test]
    fn heal_of_an_interrupted_abort_keeps_the_changes_it_carried() {
        let warehouse = Warehouse::with_working_tree("merge-abort-crash-carried");
        test_support::configure_operator();

        let base = test_support::store_parcel(&test_support::store_files(&[
            ("a.txt", b"base\n"), ("c.txt", b"base\n"), ("d.txt", b"base\n"),
        ]), &[]);
        let our_tree = test_support::store_files(&[("a.txt", b"ours\n"), ("c.txt", b"mine\n"), ("d.txt", b"mine\n")]);
        let our_head = test_support::store_parcel(&our_tree, &[&base]);
        let their_head = test_support::store_parcel(
            &test_support::store_files(&[("a.txt", b"theirs\n"), ("c.txt", b"base\n"), ("d.txt", b"base\n")]),
            &[&base],
        );

        pallet_utils::update_pallet_head("main", None, &our_head, &pallet_utils::RefPreconditions::NONE).unwrap();
        pallet_utils::write_position(&pallet_utils::BayPosition::new("main", Some(our_head.clone()))).unwrap();
        pallet_utils::update_pallet_head("feature", None, &their_head, &pallet_utils::RefPreconditions::NONE).unwrap();
        test_support::check_out(&our_tree);

        test_support::block_on(consolidate::handle_command("feature")).unwrap();
        assert!(merge_utils::read_consolidation_state().unwrap().is_some(), "sanity: the merge stopped at a conflict");

        // Made while the merge waits, to paths only our side changed: the merge left both alone.
        std::fs::write(warehouse.root.join("c.txt"), b"base\n").unwrap();
        std::fs::remove_file(warehouse.root.join("d.txt")).unwrap();

        BEFORE_STATE_CLEAR.with(|seam| seam.replace(Some(Box::new(|| Err("crashed".to_string())))));
        let error = test_support::block_on(abort(MergeKind::Consolidation, false, ABORT_COMMAND))
            .unwrap_err();
        assert_eq!(error, "crashed");
        operation_utils::forget_raised_for_tests();

        let recovery = operation_utils::recover().unwrap().expect("a marker stands");
        assert_eq!((recovery.operation, recovery.outcome), (OperationKind::Merge, RecoveryOutcome::RolledBack));

        assert!(test_support::file_holds(&warehouse.root, "a.txt", b"ours\n"), "the merge's own file goes back");
        assert!(test_support::file_holds(&warehouse.root, "c.txt", b"base\n"), "the edit back to their content is kept");
        assert!(!warehouse.root.join("d.txt").exists(), "the deletion is kept");
        assert!(merge_utils::read_consolidation_state().unwrap().is_none(), "heal clears the consolidation");
    }
}
