//! The operation marker: a durable record that an operation which rewrites the bay's staging
//! wholesale is under way, so that a crash or an error part-way through it is never committed.
//!
//! Every operation that ends in [`inventory_utils::replace_all_inventories`] — shift, park,
//! consolidate's fast-forward, lower, bay creation, import-git, every merge and cherry-pick, and
//! franchise — raises the marker before its first durable change and finishes it only once its own
//! final write has landed. Raising it around the rewrite alone would not be enough: shift records
//! its new position only after the rewrite, so a crash between the two leaves files and staging at
//! the target under a position still on the source, and a `load ./` followed by `stack` would
//! commit the target's tree onto the source pallet. The marker belongs to the operation.
//!
//! While a marker stands, every command that changes the bay or commits staged content refuses
//! ([`check_none_standing`]), and `load` does not clear it. `forklift heal` recovers ([`recover`]):
//! it reads the bay position first, and rolls the recorded operation forward or back.
//!
//! [`inventory_utils::replace_all_inventories`] refuses unless this thread raised a marker that
//! still stands ([`require_raised`]), so an operation that forgets to raise one fails every time,
//! not only after a crash.
//!
//! ## Storage
//!
//! One file, `operation`, in the bay-local state dir beside the bay's position. It is a
//! [`record_utils`] record; a file this build cannot read refuses, never reads as "nothing in
//! progress".

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use crate::error::{CoreError, RefusalCode};
use crate::util::pallet_utils::{self, AnchorPrecondition, BayPosition, OfficePrecondition, RefPreconditions};
use crate::util::shift_utils::{self, FileOp, TreeProvenance};
use crate::util::{cherry_pick_utils, file_utils, inventory_utils, merge_utils, object_utils, park_utils, record_utils};

/// The name of the marker file in the bay-local state dir.
const FILE_NAME_OPERATION: &str = "operation";

/// The [`record_utils`] kind and version of the marker file.
const OPERATION_RECORD: &str = "operation";
const OPERATION_VERSION: u32 = 2;

/// The operations that raise the marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationKind {
    Shift,
    Park,
    FastForward,
    Lower,
    BayCreation,
    ImportGit,
    Merge,
    CherryPick,
    Franchise,
}

impl OperationKind {
    /// The name the marker file records, and the one messages and reports show.
    pub fn as_str(self) -> &'static str {
        match self {
            OperationKind::Shift => "shift",
            OperationKind::Park => "park",
            OperationKind::FastForward => "fast-forward",
            OperationKind::Lower => "lower",
            OperationKind::BayCreation => "bay creation",
            OperationKind::ImportGit => "import-git",
            OperationKind::Merge => "merge",
            OperationKind::CherryPick => "cherry-pick",
            OperationKind::Franchise => "franchise",
        }
    }

    fn parse(name: &str) -> Option<OperationKind> {
        [
            OperationKind::Shift, OperationKind::Park, OperationKind::FastForward, OperationKind::Lower,
            OperationKind::BayCreation, OperationKind::ImportGit, OperationKind::Merge,
            OperationKind::CherryPick, OperationKind::Franchise,
        ].into_iter().find(|kind| kind.as_str() == name)
    }

    /// Whether the operation ends by moving the bay to its target with a position write that is
    /// its last durable step, so a position already at the target means it finished.
    fn ends_with_position(self) -> bool {
        match self {
            OperationKind::Shift | OperationKind::FastForward | OperationKind::Lower
            | OperationKind::Merge | OperationKind::CherryPick => true,
            // Park does not move the bay; bay creation and import-git write the position BEFORE
            // their rewrite; franchise only refuses.
            OperationKind::Park | OperationKind::BayCreation | OperationKind::ImportGit
            | OperationKind::Franchise => false,
        }
    }
}

/// What a standing marker records: the operation, where it started, and where it was going.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation {
    pub kind: OperationKind,

    /// Set by [`shift_utils::settle_head_write`] before it puts the bay back at `source`: the
    /// rollback never moves the position, so recovery must not read a position at `source` as
    /// "finished", and always completes the rollback.
    pub rolling_back: bool,

    /// The bay position before the operation.
    pub source: BayPosition,

    /// The tree the bay stood on before the operation (`None` for an unborn pallet).
    pub source_tree: Option<String>,

    /// The position the operation ends at. For a merge or cherry-pick its base is the completing
    /// parcel, unknown (`None`) until [`record_completing_parcel`] records it.
    pub target: BayPosition,

    /// The tree the operation materializes (`None` for an empty or not-yet-known one).
    pub target_tree: Option<String>,

    /// Fast-forward and lower: the head the pallet held when the operation started, which its
    /// compare-and-set moves it from.
    pub expected: Option<String>,

    /// Park: the parcel the work in progress was parked as.
    pub parked: Option<String>,

    /// Merge and cherry-pick: the other side's tree. Every path the merge can have written differs
    /// between it and `source_tree`, which bounds what a rollback puts back.
    pub their_tree: Option<String>,

    /// Lower: the office and anchor its take was verified against, which its write — and heal's
    /// roll-forward of it — is conditioned on, so a change to either since rolls the take back.
    pub office: OfficePrecondition,
    pub anchor: AnchorPrecondition,
}

impl Operation {
    /// An operation of `kind` from `source` (standing on `source_tree`) to `target`, with nothing
    /// else recorded.
    pub fn new(kind: OperationKind, source: BayPosition, source_tree: Option<String>, target: BayPosition) -> Operation {
        Operation {
            kind,
            rolling_back: false,
            source,
            source_tree,
            target,
            target_tree: None,
            expected: None,
            parked: None,
            their_tree: None,
            office: OfficePrecondition::NotConsumed,
            anchor: AnchorPrecondition::NotConsumed,
        }
    }

    /// The preconditions of the operation's ref write: the office and anchor it recorded.
    pub fn preconditions(&self) -> RefPreconditions {
        RefPreconditions { office: self.office.clone(), anchor: self.anchor.clone(), ..RefPreconditions::NONE }
    }

    fn to_bytes(&self) -> Vec<u8> {
        let or_empty = |value: &Option<String>| value.clone().unwrap_or_default();

        record_utils::to_bytes(OPERATION_RECORD, OPERATION_VERSION, [
            self.kind.as_str().to_string(),
            if self.rolling_back { "rolling-back" } else { "running" }.to_string(),
            self.source.pallet.clone(),
            or_empty(&self.source.base),
            or_empty(&self.source_tree),
            self.target.pallet.clone(),
            or_empty(&self.target.base),
            or_empty(&self.target_tree),
            or_empty(&self.expected),
            or_empty(&self.parked),
            or_empty(&self.their_tree),
            match &self.office {
                OfficePrecondition::NotConsumed => String::new(),
                OfficePrecondition::At(None) => "unborn".to_string(),
                OfficePrecondition::At(Some(head)) => head.clone(),
            },
            match &self.anchor {
                AnchorPrecondition::NotConsumed => String::new(),
                AnchorPrecondition::Absent => "absent".to_string(),
                AnchorPrecondition::Bytes(bytes) => format!("bytes {}", crate::util::sign_utils::to_hex(bytes)),
            },
        ])
    }

    fn from_bytes(bytes: &[u8]) -> Result<Operation, String> {
        let entries = record_utils::from_bytes(bytes, OPERATION_RECORD, OPERATION_VERSION).map_err(|e| e.describe())?;

        let [kind, phase, source_pallet, source_base, source_tree, target_pallet, target_base, target_tree, expected, parked, their_tree, office, anchor] =
            <[String; 13]>::try_from(entries).map_err(|_| "is malformed.".to_string())?;

        let kind = OperationKind::parse(&kind).ok_or(format!("names an operation this version of Forklift does not know (\"{}\").", kind.escape_debug()))?;

        let rolling_back = match phase.as_str() {
            "running" => false,
            "rolling-back" => true,
            _ => return Err("is malformed.".to_string()),
        };

        let hash = |value: String| -> Result<Option<String>, String> {
            match value.as_str() {
                "" => Ok(None),
                hash if object_utils::is_object_hash(hash) => Ok(Some(value)),
                _ => Err("is malformed.".to_string()),
            }
        };
        let pallet = |name: String| -> Result<String, String> {
            pallet_utils::validate_pallet_name(&name).map_err(|e| format!("names an invalid pallet: {}", e))?;
            Ok(name)
        };

        Ok(Operation {
            kind,
            rolling_back,
            source: BayPosition::new(pallet(source_pallet)?, hash(source_base)?),
            source_tree: hash(source_tree)?,
            target: BayPosition::new(pallet(target_pallet)?, hash(target_base)?),
            target_tree: hash(target_tree)?,
            expected: hash(expected)?,
            parked: hash(parked)?,
            their_tree: hash(their_tree)?,
            office: match office.as_str() {
                "" => OfficePrecondition::NotConsumed,
                "unborn" => OfficePrecondition::At(None),
                _ => OfficePrecondition::At(Some(hash(office)?.ok_or("is malformed.".to_string())?)),
            },
            anchor: match anchor.as_str() {
                "" => AnchorPrecondition::NotConsumed,
                "absent" => AnchorPrecondition::Absent,
                _ => match anchor.strip_prefix("bytes ").map(crate::util::sign_utils::from_hex) {
                    Some(Ok(bytes)) => AnchorPrecondition::Bytes(bytes),
                    _ => return Err("is malformed.".to_string()),
                },
            },
        })
    }
}

thread_local! {
    /// The marker this thread raised and has not finished: its path. Per thread, because the
    /// warehouse a command runs against is per thread (see `globals`), so tests running side by
    /// side each own their own.
    static RAISED: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// The path of the active bay's marker file.
pub fn marker_path() -> PathBuf {
    crate::globals::bay_root().join(FILE_NAME_OPERATION)
}

/// `path` made absolute, so a marker raised before a command enters a bay (bay creation) is still
/// recognized once it has.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Whether this thread raised the marker at `path` and has not finished it.
fn raised_here(path: &Path) -> bool {
    let path = absolute(path);
    RAISED.with(|raised| raised.borrow().as_deref() == Some(path.as_path()))
}

fn set_raised(path: Option<&Path>) {
    RAISED.with(|raised| *raised.borrow_mut() = path.map(absolute));
}

/// Raise the marker for `operation` in the active bay, durably, before the operation's first
/// durable change. Refuses if a marker already stands.
///
/// # Returns
/// * `Ok(())`      - The marker is durable, and this thread owns it.
/// * `Err(String)` - A marker already stands, or the write failed (nothing was changed then).
pub fn raise(operation: &Operation) -> Result<(), String> {
    raise_in(&crate::globals::bay_root(), operation)
}

/// [`raise`] in the bay-local state dir `dir`, whichever bay it belongs to: bay creation raises
/// the new bay's marker before it enters the bay.
pub fn raise_in(dir: &Path, operation: &Operation) -> Result<(), String> {
    let path = dir.join(FILE_NAME_OPERATION);

    if path.exists() {
        return Err(standing_refusal_message(&path));
    }

    write_marker(&path, operation)?;
    set_raised(Some(&path));

    Ok(())
}

fn write_marker(path: &Path, operation: &Operation) -> Result<(), String> {
    file_utils::replace_file_atomically(path, &operation.to_bytes())
        .map_err(|e| format!("Error while recording the {} in progress: {}", operation.kind.as_str(), e))
}

/// The marker this thread raised, if one stands.
fn raised_operation() -> Result<Option<Operation>, String> {
    let path = marker_path();

    if !raised_here(&path) {
        return Ok(None);
    }

    read_at(&path)
}

/// Record `parcel` as the completing parcel of the merge or cherry-pick whose marker this thread
/// raised, durably, before the head write that lands it: once that write lands, recovery tells a
/// finished merge from an unfinished one by this hash. Does nothing when no such marker stands
/// (an ordinary `stack`).
pub fn record_completing_parcel(parcel: &str) -> Result<(), String> {
    let Some(mut operation) = raised_operation()? else {
        return Ok(());
    };

    if !matches!(operation.kind, OperationKind::Merge | OperationKind::CherryPick) {
        return Ok(());
    }

    operation.target.base = Some(parcel.to_string());
    write_marker(&marker_path(), &operation)
}

/// Flag the marker this thread raised as rolling back, durably, before the rollback's rewrite
/// starts (see [`Operation::rolling_back`]). Refuses if this thread raised none: a rollback that
/// rewrites staging must be recoverable.
pub fn mark_rolling_back() -> Result<(), String> {
    let mut operation = raised_operation()?
        .ok_or("Internal error: a rollback started with no operation recorded in progress.".to_string())?;

    operation.rolling_back = true;
    write_marker(&marker_path(), &operation)
}

/// Finish the marker this thread raised, once the operation's own final write has landed: remove
/// it durably. Does nothing if this thread raised none.
///
/// # Returns
/// * `Ok(())`      - No marker this thread raised stands any more.
/// * `Err(String)` - The removal failed; the marker stands, and `forklift heal` clears it.
pub fn finish() -> Result<(), String> {
    let path = marker_path();

    if !raised_here(&path) {
        return Ok(());
    }

    remove_marker(&path)?;
    set_raised(None);

    Ok(())
}

fn remove_marker(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("Error while clearing the operation record \"{}\": {}", path.to_string_lossy(), e)),
    }

    match path.parent() {
        Some(parent) => file_utils::sync_dir(parent),
        None => Ok(()),
    }
}

/// Refuse unless this thread raised a marker that still stands: called by every wholesale rewrite
/// of staging, so an operation that forgot to raise one fails every time.
pub fn require_raised(what: &str) -> Result<(), String> {
    let path = marker_path();

    if raised_here(&path) && path.exists() {
        return Ok(());
    }

    Err(format!(
        "Internal error: {} was attempted with no operation recorded in progress, so a crash part-way \
        through it could not be recovered. Nothing was changed.",
        what
    ))
}

/// Read the active bay's marker.
///
/// # Returns
/// * `Ok(None)`            - No marker stands.
/// * `Ok(Some(Operation))` - The operation it records.
/// * `Err(String)`         - It exists and cannot be read.
pub fn read() -> Result<Option<Operation>, String> {
    read_at(&marker_path())
}

fn read_at(path: &Path) -> Result<Option<Operation>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Error while reading the operation record \"{}\": {}", path.to_string_lossy(), e)),
    };

    Operation::from_bytes(&bytes)
        .map(Some)
        .map_err(|reason| format!("The operation record \"{}\" {}", path.to_string_lossy(), reason))
}

fn standing_refusal_message(path: &Path) -> String {
    match read_at(path) {
        Ok(Some(operation)) => format!(
            "The {} that ran in this bay was interrupted before it finished, so the staged inventory may be \
            incomplete; changing the bay or committing now could record a wrong result. Run \
            \"forklift heal\" to finish or undo it.",
            operation.kind.as_str()
        ),
        Ok(None) => "An operation in this bay was interrupted before it finished. Run \"forklift heal\" \
            to finish or undo it.".to_string(),
        Err(error) => format!(
            "{} An operation in this bay was interrupted, and this version of Forklift cannot tell \
            which one, so it cannot finish or undo it. Run \"forklift heal\" with the version that \
            started it.",
            error
        ),
    }
}

/// Refuse while a marker stands in the active bay that this thread did not raise: every command
/// that changes the bay or commits staged content asks this first. `forklift heal` is the way out.
pub fn check_none_standing() -> Result<(), CoreError> {
    let path = marker_path();

    if raised_here(&path) || !path.exists() {
        return Ok(());
    }

    Err(CoreError::refusal(RefusalCode::IncompleteLoad, standing_refusal_message(&path), "forklift heal"))
}

/// How [`recover`] settled an interrupted operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// The operation had already finished; only its record was left.
    Finished,

    /// The operation was completed.
    RolledForward,

    /// The operation was undone: the bay is back where it started.
    RolledBack,

    /// The operation had changed nothing that needs undoing; its record was cleared.
    Cleared,
}

/// What [`recover`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recovery {
    pub operation: OperationKind,
    pub outcome: RecoveryOutcome,

    /// The bay's position afterwards.
    pub position: BayPosition,

    /// Tracked files the working directory does not hold at their recorded content: they read as
    /// local changes, never staged ones. `restore` brings them back.
    pub unheld: Vec<String>,
}

/// Recover the operation the active bay's marker records, if any: finish it, complete it, or undo
/// it, then clear the marker. Runs under the warehouse lock.
///
/// The position is read first: an operation whose last durable step is its position write
/// finished if the position already records its target — even if another bay has stacked on the
/// pallet since. A marker flagged rolling-back skips that test, and always finishes the rollback.
///
/// # Returns
/// * `Ok(None)`           - No marker stands.
/// * `Ok(Some(Recovery))` - What was done; the marker is cleared.
/// * `Err(String)`        - The marker could not be read, or recovery failed; the marker stands.
pub fn recover() -> Result<Option<Recovery>, String> {
    let path = marker_path();

    let Some(operation) = read_at(&path)? else {
        return Ok(None);
    };

    // Recovery owns the marker from here: its own rewrites need one standing.
    set_raised(Some(&path));

    let recovered = recover_operation(&operation);

    let (outcome, unheld) = match recovered {
        Ok(done) => done,
        Err(error) => {
            set_raised(None);
            return Err(error);
        }
    };

    finish()?;

    Ok(Some(Recovery {
        operation: operation.kind,
        outcome,
        position: pallet_utils::read_position()?,
        unheld,
    }))
}

fn recover_operation(operation: &Operation) -> Result<(RecoveryOutcome, Vec<String>), String> {
    if operation.kind == OperationKind::Franchise {
        return Err(
            "A franchise into this directory was interrupted before it finished. It held no work of \
            its own yet: remove the directory and franchise again.".to_string()
        );
    }

    if operation.rolling_back {
        return roll_back(operation);
    }

    let position = pallet_utils::read_position()?;

    if operation.kind.ends_with_position() && operation.target.base.is_some() && position == operation.target {
        clear_in_progress_state(operation)?;
        return Ok((RecoveryOutcome::Finished, Vec::new()));
    }

    match operation.kind {
        OperationKind::Shift => match &operation.target_tree {
            Some(tree) if object_utils::load_tree(tree).is_err() => roll_back(operation),
            target_tree => {
                let target_tree = tree_or_empty(target_tree.as_deref())?;
                put_back(&tree_or_empty(operation.source_tree.as_deref())?, &target_tree)?;
                let unheld = rebuild_staging(Some(&target_tree))?;
                pallet_utils::write_position(&operation.target)?;
                Ok((RecoveryOutcome::RolledForward, unheld))
            }
        },

        OperationKind::Park => {
            // The position does not change across a park, so the parked list is its signal: the
            // work is safe there only if the list names the parked parcel.
            let parked = match operation.parked.as_deref() {
                Some(hash) if park_utils::read_parked()?.iter().any(|entry| entry == hash) => Some(hash),
                _ => None,
            };

            if let Some(parcel) = parked {
                // The work is safe in the list, so the reset to the head is finished: a file still
                // holding the parked content goes back to the head.
                let head_tree = tree_or_empty(operation.source_tree.as_deref())?;
                put_back(&object_utils::load_parcel(parcel)?.tree_hash, &head_tree)?;
                Ok((RecoveryOutcome::RolledForward, rebuild_staging(Some(&head_tree))?))
            } else {
                // The working files were never touched, and staging holds only what they hold.
                Ok((RecoveryOutcome::Cleared, Vec::new()))
            }
        }

        OperationKind::FastForward | OperationKind::Lower => {
            let new = operation.target.base.as_deref()
                .ok_or("The operation record is malformed: it names no new head.".to_string())?;
            let head = pallet_utils::get_pallet_head(&operation.target.pallet)?;

            let landed = match head.as_deref() {
                Some(head) if head == new => true,
                Some(head) => crate::util::merge_utils::is_ancestor(new, head)?,
                None => false,
            };

            // Under the preconditions the operation recorded: a lower's take lands only while the
            // office and anchor it was verified against stand, and is rolled back otherwise.
            let landed = landed || (head == operation.expected && pallet_utils::update_pallet_head(
                &operation.target.pallet, operation.expected.as_deref(), new, &operation.preconditions()
            ).is_ok());

            if landed {
                let target_tree = tree_or_empty(operation.target_tree.as_deref())?;
                put_back(&tree_or_empty(operation.source_tree.as_deref())?, &target_tree)?;
                let unheld = rebuild_staging(Some(&target_tree))?;
                pallet_utils::record_landed(&operation.target.pallet, new)?;
                Ok((RecoveryOutcome::RolledForward, unheld))
            } else {
                roll_back(operation)
            }
        }

        OperationKind::Merge | OperationKind::CherryPick => {
            let landed = match (operation.target.base.as_deref(), pallet_utils::get_pallet_head(&operation.target.pallet)?) {
                (Some(parcel), Some(head)) => head == parcel || crate::util::merge_utils::is_ancestor(parcel, &head)?,
                _ => false,
            };

            if landed {
                let parcel = operation.target.base.as_deref().expect("landed implies a completing parcel");
                let tree = object_utils::load_parcel(parcel)?.tree_hash;
                let unheld = rebuild_staging(Some(&tree))?;

                // As `stack` orders it: the in-progress state comes off before the position moves.
                clear_in_progress_state(operation)?;
                pallet_utils::record_landed(&operation.target.pallet, parcel)?;
                Ok((RecoveryOutcome::RolledForward, unheld))
            } else {
                roll_back(operation)
            }
        }

        OperationKind::BayCreation => {
            // A bay being created holds no work of its own, so its files are written outright.
            if let Some(tree) = &operation.target_tree {
                put_back(&object_utils::empty_tree_hash()?, tree)?;
            }

            Ok((RecoveryOutcome::RolledForward, rebuild_staging(operation.target_tree.as_deref())?))
        }

        OperationKind::ImportGit => {
            // Raised before the position, so the position may still be the earlier one.
            let unheld = rebuild_staging(operation.target_tree.as_deref())?;
            pallet_utils::write_position(&operation.target)?;
            Ok((RecoveryOutcome::RolledForward, unheld))
        }

        OperationKind::Franchise => unreachable!("franchise refuses above"),
    }
}

/// Undo `operation`: put the files it may have written back at the source tree, rebuild staging
/// from the source tree, restore the source position, and clear any in-progress state.
fn roll_back(operation: &Operation) -> Result<(RecoveryOutcome, Vec<String>), String> {
    let source_tree = tree_or_empty(operation.source_tree.as_deref())?;

    // Each of these checked the bay clean against the source before it wrote a file, so a file
    // holding the tree it was writing is its output, and goes back. A merge's file that took both
    // sides' changes holds neither tree, so it stays, reported as a local change, beside any edit
    // made since the crash.
    let written_from = match operation.kind {
        OperationKind::FastForward | OperationKind::Lower => operation.target_tree.clone(),
        OperationKind::Merge | OperationKind::CherryPick => operation.their_tree.clone(),
        _ => None,
    };

    if let Some(written_from) = written_from {
        put_back(&written_from, &source_tree)?;
    }

    let unheld = rebuild_staging(Some(&source_tree))?;
    clear_in_progress_state(operation)?;
    pallet_utils::write_position(&operation.source)?;

    Ok((RecoveryOutcome::RolledBack, unheld))
}

/// Clear the consolidation, skeleton and pick state a merge or cherry-pick left, as `stack` orders
/// them (the state before the skeleton).
fn clear_in_progress_state(operation: &Operation) -> Result<(), String> {
    if matches!(operation.kind, OperationKind::Merge | OperationKind::CherryPick) {
        merge_utils::clear_consolidation_state()?;
        merge_utils::OutOfScopeSkeleton::clear()?;
        cherry_pick_utils::clear_state()?;
    }

    Ok(())
}

/// Bring every path that differs between `from` and `to` to `to`'s content, where the working
/// directory still holds what the operation could have left there: `from`'s content, `to`'s (then
/// the path is done already), or nothing. A file holding anything else is the user's work since the
/// crash, or an untracked file where `to` adds one, so it is left alone, and the rebuild after
/// reports it as a local change.
fn put_back(from: &str, to: &str) -> Result<(), String> {
    let (ops, removed_dirs) = shift_utils::diff_trees(Some(from), to)?;

    // Every file not at a tree's content, a missing one included, comes back from its rebuild.
    let unheld = |tree: &str| -> Result<std::collections::HashSet<String>, String> {
        Ok(shift_utils::build_inventories_for_tree(tree, TreeProvenance::Verify)?.unheld.into_iter().collect())
    };
    let (off_from, off_to) = (unheld(from)?, unheld(to)?);
    let present = |path: &str| crate::globals::warehouse_root().join(path).symlink_metadata().is_ok();

    let ops: Vec<FileOp> = ops.into_iter().filter_map(|op| match op {
        FileOp::Write { path, hash, item_type, is_new } => {
            let writable = off_to.contains(&path) && (!present(&path) || (!is_new && !off_from.contains(&path)));
            writable.then_some(FileOp::Write { path, hash, item_type, is_new: false })
        }
        FileOp::Remove { path } => (present(&path) && !off_from.contains(&path)).then_some(FileOp::Remove { path }),
    }).collect();

    shift_utils::ensure_writes_present(&ops)?;
    shift_utils::apply_ops(&ops, &removed_dirs)
}

/// `tree`, or the empty tree for `None`.
fn tree_or_empty(tree: Option<&str>) -> Result<String, String> {
    match tree {
        Some(tree) => Ok(tree.to_string()),
        None => object_utils::empty_tree_hash(),
    }
}

/// Rebuild staging from `tree` (the empty tree for `None`), re-proving every file, so a file a
/// crash left at other content reads as a local change and is never staged.
///
/// # Returns
/// * `Ok(Vec<String>)` - The tracked files the working directory does not hold at `tree`'s content.
/// * `Err(String)`     - If the tree could not be loaded, a file could not be read, or the rewrite
///                       failed.
fn rebuild_staging(tree: Option<&str>) -> Result<Vec<String>, String> {
    let tree = match tree {
        Some(tree) => tree.to_string(),
        None => object_utils::empty_tree_hash()?,
    };

    let built = shift_utils::build_inventories_for_tree(&tree, TreeProvenance::Verify)?;
    inventory_utils::replace_all_inventories(&built.shards)?;

    Ok(built.unheld)
}

/// Forget that this thread raised the marker, as the end of the process that raised it would: the
/// marker stands, and every command refuses over it until `forklift heal`. For tests that run a
/// command in process and then stand in for the next one.
#[doc(hidden)]
pub fn forget_raised_for_tests() {
    set_raised(None);
}

/// Test-only: raise a marker for a fixture that rewrites staging outside any operation, unless
/// this thread already raised one here.
#[cfg(test)]
pub(crate) fn raise_for_fixture() {
    if raised_here(&marker_path()) && marker_path().exists() {
        return;
    }

    let position = BayPosition::new("main", None);
    raise(&Operation::new(OperationKind::Shift, position.clone(), None, position)).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operation() -> Operation {
        let mut operation = Operation::new(
            OperationKind::Merge,
            BayPosition::new("main", Some("a".repeat(64))),
            Some("b".repeat(64)),
            BayPosition::new("main", None),
        );
        operation.their_tree = Some("c".repeat(64));
        operation
    }

    #[test]
    fn a_record_reads_back_as_written() {
        let mut operation = operation();
        operation.rolling_back = true;
        operation.target.base = Some("d".repeat(64));

        assert_eq!(Operation::from_bytes(&operation.to_bytes()), Ok(operation));
    }

    #[test]
    fn a_lower_record_reads_back_its_preconditions() {
        let mut operation = operation();
        operation.kind = OperationKind::Lower;

        for (office, anchor) in [
            (OfficePrecondition::At(Some("e".repeat(64))), AnchorPrecondition::Bytes(b"anchor\nbytes".to_vec())),
            (OfficePrecondition::At(None), AnchorPrecondition::Absent),
            (OfficePrecondition::NotConsumed, AnchorPrecondition::NotConsumed),
        ] {
            operation.office = office;
            operation.anchor = anchor;
            assert_eq!(Operation::from_bytes(&operation.to_bytes()), Ok(operation.clone()));
        }

        let text = String::from_utf8(operation.to_bytes()).unwrap();
        let unknown = format!("{}bytes zz\n", text.trim_end_matches('\n').rsplit_once('\n').unwrap().0.to_string() + "\n");
        assert_eq!(Operation::from_bytes(unknown.as_bytes()), Err("is malformed.".to_string()));
    }

    #[test]
    fn a_record_of_an_unknown_operation_or_another_version_refuses() {
        let text = String::from_utf8(operation().to_bytes()).unwrap();

        let unknown = text.replace("\nmerge\n", "\nrebase\n");
        assert!(Operation::from_bytes(unknown.as_bytes()).unwrap_err().contains("does not know (\"rebase\")"));

        let newer = text.replace(
            &format!("/forklift-operation {}", OPERATION_VERSION), &format!("/forklift-operation {}", OPERATION_VERSION + 1)
        );
        assert_ne!(newer, text);
        assert!(Operation::from_bytes(newer.as_bytes()).unwrap_err().contains("does not know"));

        let short = text.trim_end_matches('\n').rsplit_once('\n').unwrap().0.to_string() + "\n";
        assert_eq!(Operation::from_bytes(short.as_bytes()), Err("is malformed.".to_string()));
    }

    #[test]
    fn a_wholesale_rewrite_of_staging_refuses_unless_this_process_raised_a_marker() {
        let root = std::env::temp_dir().join(format!("forklift-operation-required-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let scope = crate::globals::StorageRootScope::enter(&root);

        let error = crate::util::inventory_utils::replace_all_inventories(&Default::default())
            .expect_err("a rewrite with no marker must refuse");
        assert!(error.contains("Nothing was changed"), "{}", error);

        // A marker another process left does not authorize it either.
        raise(&operation()).unwrap();
        forget_raised_for_tests();
        crate::util::inventory_utils::replace_all_inventories(&Default::default())
            .expect_err("a marker this process did not raise must not authorize a rewrite");

        set_raised(Some(&marker_path()));
        crate::util::inventory_utils::replace_all_inventories(&Default::default())
            .expect("the owner of the marker may rewrite");
        finish().unwrap();
        assert!(read().unwrap().is_none());

        drop(scope);
        std::fs::remove_dir_all(&root).ok();
    }
}
