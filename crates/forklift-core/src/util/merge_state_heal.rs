//! Restoring the records of the merge in progress in the bay `forklift heal` runs in: the
//! consolidation state, its out-of-scope skeleton and the pick state.
//!
//! ## The copies
//!
//! Every write of one of these records puts the same bytes in a copy beside it first, then in the
//! record, and every clear removes the record first, then the copy
//! ([`merge_utils::write_record_and_copy`]). So while a record is present its copy holds the bytes
//! of that record's last write, and a copy with no record beside it is an interrupted clear or a
//! failed write, never a merge in progress.
//!
//! ## Usable
//!
//! A record, or its copy, is usable when it reads by the checks its reader applies. A skeleton is
//! usable only beside a usable consolidation state, and only when its bytes hash to the digest that
//! state records. Before heal writes a copy of a state back, it must also agree with the bay: name
//! the pallet and the head heal resolved for it.
//!
//! ## What [`repair`] does
//!
//! A record that is present and not usable, or a skeleton missing beside a usable state, is handled
//! by the operation marker standing in the bay:
//! - Under a merge or pick marker, it is written back from a usable copy, or removed with its copy:
//!   that operation's recovery clears all three records whatever its outcome. Never refused.
//! - With no marker, it is written back from a usable copy that agrees with the bay, or refused with
//!   `merge_state_damaged` and the record's way out.
//! - Under any other marker, it is written back as with no marker, or left for heal to refuse once
//!   that operation's recovery has lowered its marker, since the way out is refused while it stands.
//!
//! An absent state is never written back, even with a copy beside it: that is how every clear
//! leaves a finished, aborted or recovered merge. A usable record re-syncs a copy that is missing,
//! damaged or different. A record or copy a newer Forklift wrote is never written over.
//!
//! ## The next step
//!
//! A reader that refuses a damaged record names `forklift heal` exactly when heal is the step that
//! can run for it ([`heal_can_run`]): an operation marker stands in that bay, or none does and a
//! usable copy agrees with it. Otherwise it names the record's way out.

use std::path::Path;

use serde::Serialize;

use crate::error::CoreError;
use crate::globals::bay_root;
use crate::util::cherry_pick_utils::{self, FILE_NAME_CHERRY_PICK, FILE_NAME_CHERRY_PICK_COPY};
use crate::util::merge_utils::{self, OutOfScopeSkeleton, Unusable};
use crate::util::{file_utils, operation_utils, pallet_utils};
use crate::util::operation_utils::OperationKind;

/// The command a damaged merge record names when heal can run for it.
const HEAL_COMMAND: &str = "forklift heal";

/// What [`HEAL_COMMAND`] does for a damaged record with a usable copy, as a sentence that does not
/// name it.
const HEAL_FROM_COPY_EFFECT: &str = "A copy of that record kept beside it still reads, and heal writes it back.";

/// What [`HEAL_COMMAND`] does for a damaged record under an operation marker, as a sentence that does
/// not name it.
const HEAL_MARKER_EFFECT: &str = "An interrupted operation stands there too, which heal finishes or undoes; it writes \
    that record back from its copy where one can be used, and otherwise drops it with the merge that operation \
    settles or says what to run next.";

/// One of the records of a merge in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MergeRecord {
    /// The consolidation state.
    State,

    /// The out-of-scope skeleton beside it.
    Skeleton,

    /// The pick state.
    Pick,
}

impl MergeRecord {
    fn file_name(self) -> &'static str {
        match self {
            MergeRecord::State => merge_utils::FILE_NAME_CONSOLIDATION,
            MergeRecord::Skeleton => merge_utils::FILE_NAME_SKELETON,
            MergeRecord::Pick => FILE_NAME_CHERRY_PICK,
        }
    }

    fn copy_name(self) -> &'static str {
        match self {
            MergeRecord::State => merge_utils::FILE_NAME_CONSOLIDATION_COPY,
            MergeRecord::Skeleton => merge_utils::FILE_NAME_SKELETON_COPY,
            MergeRecord::Pick => FILE_NAME_CHERRY_PICK_COPY,
        }
    }

    /// The record in a sentence.
    fn what(self) -> &'static str {
        match self {
            MergeRecord::State => "consolidation state",
            MergeRecord::Skeleton => "consolidation skeleton",
            MergeRecord::Pick => "pick state",
        }
    }

    /// The way out when heal cannot run for it: what it does, and the command.
    fn way_out(self) -> (&'static str, &'static str) {
        match self {
            MergeRecord::State | MergeRecord::Pick => (merge_utils::KEEPING_FILES_EFFECT, merge_utils::ABORT_KEEPING_FILES),
            MergeRecord::Skeleton => (merge_utils::SKELETON_EFFECT, merge_utils::ABORT_COMMAND),
        }
    }
}

/// The pallet and head a copy of a state must name to agree with the bay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agreement {
    /// The bay's pallet.
    pub pallet: String,

    /// The parcel the bay's files stand on.
    pub head: String,
}

/// The agreement of the bay whose state lives in `dir`, read from its position: none when the
/// position does not read or stands on no parcel, so no copy agrees.
fn agreement_in(dir: &Path) -> Option<Agreement> {
    let position = pallet_utils::read_position_in(dir).ok()?;
    position.base.map(|head| Agreement { pallet: position.pallet, head })
}

/// The bytes of the copy of `record` in `dir` when heal could write them back: they read, and a
/// state's copy names `agreement` (any, when `agreement` is `Err(())`, as under a merge marker),
/// while a skeleton's hashes to `digest`.
fn restorable_copy(dir: &Path, record: MergeRecord, agreement: Result<Option<&Agreement>, ()>, digest: Option<&str>) -> Option<Vec<u8>> {
    let bytes = match merge_utils::read_record_bytes(&dir.join(record.copy_name())) {
        Ok(Some(bytes)) => bytes,
        _ => return None,
    };

    let agrees = |pallet: &str, head: &str| match agreement {
        Err(()) => true,
        Ok(Some(agreement)) => agreement.pallet == pallet && agreement.head == head,
        Ok(None) => false,
    };

    let usable = match record {
        MergeRecord::State => merge_utils::parse_consolidation_state(&bytes)
            .is_ok_and(|state| agrees(&state.our_pallet, &state.our_head)),
        MergeRecord::Pick => cherry_pick_utils::parse_state(&bytes)
            .is_ok_and(|state| agrees(&state.our_pallet, &state.our_head)),
        MergeRecord::Skeleton => digest.is_some_and(|digest| OutOfScopeSkeleton::parse(&bytes, digest).is_ok()),
    };

    usable.then_some(bytes)
}

/// Whether heal is the step that can run for a damaged `record` in the bay whose state lives in
/// `dir`: an operation marker stands there, or none does and a usable copy agrees with the bay
/// (`agreement`) and, for a skeleton, hashes to `digest`.
fn heal_can_run(dir: &Path, record: MergeRecord, agreement: Option<&Agreement>, digest: Option<&str>) -> bool {
    operation_utils::marker_stands_in(dir) || restorable_copy(dir, record, Ok(agreement), digest).is_some()
}

/// The `merge_state_damaged` refusal for `record` in the bay whose state lives in `dir`, whose
/// damage `found` says as a sentence: it names `forklift heal` when heal can run for it
/// ([`heal_can_run`], judged against the bay's position), and the record's way out otherwise.
///
/// # Arguments
/// * `digest` - For a skeleton, the digest its consolidation state records.
pub(crate) fn damaged_in(dir: &Path, found: &str, record: MergeRecord, digest: Option<&str>) -> String {
    damaged_with(dir, found, record, agreement_in(dir).as_ref(), digest)
}

/// [`damaged_in`] against `agreement`, the pallet and head heal resolved for the bay.
fn damaged_with(dir: &Path, found: &str, record: MergeRecord, agreement: Option<&Agreement>, digest: Option<&str>) -> String {
    let (effect, step) = match heal_can_run(dir, record, agreement, digest) {
        true if operation_utils::marker_stands_in(dir) => (HEAL_MARKER_EFFECT, HEAL_COMMAND),
        true => (HEAL_FROM_COPY_EFFECT, HEAL_COMMAND),
        false => record.way_out(),
    };
    merge_utils::merge_state_damaged(dir, found, effect, step)
}

/// A merge record `heal` wrote back from its copy, or removed with it.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MergeRecordRestored {
    /// The record: "consolidation state", "consolidation skeleton" or "pick state".
    pub record: String,

    /// Its file.
    pub file: String,

    /// Whether it was removed with its copy rather than written back: under an interrupted merge or
    /// pick, whose recovery clears it whatever its outcome, when no copy could be used.
    pub removed: bool,

    /// What the file held that could not be used.
    pub was: String,
}

/// The merge records of the checkout `heal` runs in that it wrote back or removed.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MergeStateRestored {
    /// The checkout: the warehouse's own working tree, or a bay.
    pub checkout: String,

    /// Each record, in the order heal judged them.
    pub records: Vec<MergeRecordRestored>,
}

impl MergeStateRestored {
    /// The sentences heal prints for it, one per record.
    pub fn sentences(&self) -> Vec<String> {
        self.records.iter().map(|record| match record.removed {
            false => format!(
                "Restored the {} of {} (\"{}\") from its copy, since its file {}.",
                record.record, self.checkout, record.file, record.was.trim_end_matches('.')
            ),
            true => format!(
                "Removed the {} of {} (\"{}\") and its copy, since its file {} and no copy could be used; the \
                interrupted operation's recovery clears it whatever its outcome.",
                record.record, self.checkout, record.file, record.was.trim_end_matches('.')
            ),
        }).collect()
    }
}

/// What [`repair`] did.
pub struct Repair {
    /// The records it wrote back or removed, if any.
    pub restored: Option<MergeStateRestored>,

    /// Whether a damaged record was left under a marker of another kind, for heal to refuse with
    /// [`check`] once that operation's recovery has lowered its marker.
    pub deferred: bool,
}

/// The operation marker standing in the bay, as the repair branches on it.
enum Marker {
    /// None stands.
    None,

    /// A merge or pick: its recovery clears all three records whatever its outcome.
    Merge,

    /// Any other kind, or one that does not read.
    Other,
}

/// How a record reads.
enum Status {
    /// Nothing is there.
    Absent,

    /// It reads: its bytes.
    Usable(Vec<u8>),

    /// It is present and cannot be used: why, as a phrase that follows "its file ".
    Damaged(String),

    /// A newer Forklift wrote it.
    Newer,
}

fn status(dir: &Path, record: MergeRecord, digest: Option<&str>) -> Result<Status, String> {
    let bytes = match merge_utils::read_record_bytes(&dir.join(record.file_name())) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(Status::Absent),
        Err(Ok(reason)) => return Ok(Status::Damaged(reason)),
        Err(Err(error)) => return Err(error),
    };

    let parsed = match record {
        MergeRecord::State => merge_utils::parse_consolidation_state(&bytes).map(|_| ()),
        MergeRecord::Pick => cherry_pick_utils::parse_state(&bytes).map(|_| ()),
        MergeRecord::Skeleton => OutOfScopeSkeleton::parse(&bytes, digest.unwrap_or_default()).map(|_| ()),
    };

    Ok(match parsed {
        Ok(()) => Status::Usable(bytes),
        Err(Unusable::Damaged(reason)) => Status::Damaged(reason),
        Err(Unusable::Newer(_)) => Status::Newer,
    })
}

/// Repair the merge records of the bay heal runs in from their copies, or re-sync the copies from
/// them (see the module doc comment).
///
/// # Arguments
/// * `agreement` - The pallet and head heal resolved for the bay, or `None` when it stands on no
///                 parcel: a state's copy must name them to be written back.
/// * `notes`     - Where a copy that could not be re-synced is noted.
///
/// # Returns
/// * `Ok(Repair)`     - What was written back or removed, and whether a refusal waits.
/// * `Err(CoreError)` - A `merge_state_damaged` refusal, or a file that could not be read or
///   written.
pub fn repair(agreement: Option<&Agreement>, notes: &mut Vec<String>) -> Result<Repair, CoreError> {
    let dir = bay_root();
    let marker = match operation_utils::marker_stands_in(&dir) {
        false => Marker::None,
        true => match operation_utils::read_in(&dir) {
            Ok(Some(operation)) if matches!(operation.kind, OperationKind::Merge | OperationKind::CherryPick) => Marker::Merge,
            _ => Marker::Other,
        },
    };
    let checkout = match crate::globals::active_bay() {
        Some(name) => format!("bay \"{}\"", name),
        None => "the warehouse's own working tree".to_string(),
    };

    let mut repaired = Repair { restored: None, deferred: false };
    let mut records: Vec<MergeRecordRestored> = Vec::new();

    // The state first: the skeleton is judged against the digest of the state that is left.
    let mut digest: Option<String> = None;
    for record in [MergeRecord::State, MergeRecord::Skeleton, MergeRecord::Pick] {
        if record == MergeRecord::Skeleton && digest.is_none() {
            continue;
        }

        let path = dir.join(record.file_name());
        let was = match status(&dir, record, digest.as_deref())? {
            Status::Newer => continue,
            Status::Absent if record != MergeRecord::Skeleton => continue,
            Status::Absent => "is missing".to_string(),
            Status::Usable(bytes) => {
                if record == MergeRecord::State {
                    digest = Some(state_digest(&bytes));
                }
                resync_copy(&dir, record, &bytes, &checkout, notes);
                continue;
            }
            Status::Damaged(reason) => reason,
        };

        let copy_agreement = match marker {
            Marker::Merge => Err(()),
            Marker::None | Marker::Other => Ok(agreement),
        };
        if let Some(bytes) = restorable_copy(&dir, record, copy_agreement, digest.as_deref()) {
            file_utils::write_file_atomically(&path, &bytes)
                .map_err(|e| format!("Error while restoring the {} \"{}\": {}", record.what(), path.to_string_lossy(), e))?;
            if record == MergeRecord::State {
                digest = Some(state_digest(&bytes));
            }
            records.push(MergeRecordRestored { record: record.what().to_string(), file: path.to_string_lossy().into_owned(), removed: false, was });
            continue;
        }

        match marker {
            Marker::Merge => {
                merge_utils::remove_record(&path)?;
                let copy = dir.join(record.copy_name());
                if !matches!(merge_utils::read_record_bytes(&copy), Ok(Some(bytes)) if copy_is_newer(record, &bytes)) {
                    merge_utils::remove_record(&copy)?;
                }
                records.push(MergeRecordRestored { record: record.what().to_string(), file: path.to_string_lossy().into_owned(), removed: true, was });
            }
            Marker::Other => repaired.deferred = true,
            Marker::None => {
                let found = format!("The {} file \"{}\" {}", record.what(), path.to_string_lossy(), with_stop(&was));
                let mut error = damaged_with(&dir, &found, record, agreement, digest.as_deref());
                let done = MergeStateRestored { checkout: checkout.clone(), records: records.clone() };
                for sentence in done.sentences() {
                    error = crate::error::append_to_message(error, &sentence);
                }
                return Err(CoreError::from(error));
            }
        }
    }

    if !records.is_empty() {
        repaired.restored = Some(MergeStateRestored { checkout, records });
    }
    Ok(repaired)
}

/// The skeleton digest a consolidation state's bytes record; they were already found usable.
fn state_digest(bytes: &[u8]) -> String {
    merge_utils::parse_consolidation_state(bytes).map(|state| state.skeleton_digest).unwrap_or_default()
}

/// `reason` ending in a full stop.
fn with_stop(reason: &str) -> String {
    format!("{}.", reason.trim_end_matches('.'))
}

/// Whether a copy's bytes were written by a newer Forklift, so they are never written over.
fn copy_is_newer(record: MergeRecord, bytes: &[u8]) -> bool {
    let parsed = match record {
        MergeRecord::State => merge_utils::parse_consolidation_state(bytes).err(),
        MergeRecord::Pick => cherry_pick_utils::parse_state(bytes).err(),
        // Any digest: only the header decides.
        MergeRecord::Skeleton => OutOfScopeSkeleton::parse(bytes, "").err(),
    };
    matches!(parsed, Some(Unusable::Newer(_)))
}

/// Rewrite the copy of a usable record when it is missing, damaged or different. A copy a newer
/// Forklift wrote is left as it is. A failure is a note, not an error: the record is sound.
fn resync_copy(dir: &Path, record: MergeRecord, bytes: &[u8], checkout: &str, notes: &mut Vec<String>) {
    let copy = dir.join(record.copy_name());
    match merge_utils::read_record_bytes(&copy) {
        Ok(Some(held)) if held == bytes || copy_is_newer(record, &held) => return,
        _ => {}
    }

    if let Err(error) = file_utils::write_file_atomically(&copy, bytes) {
        notes.push(format!(
            "The copy of the {} of {} could not be rewritten ({}), so a later loss of that record has no copy to \
            restore it from.",
            record.what(), checkout, error.trim_end_matches('.')
        ));
    }
}

/// Read every merge record of the bay heal runs in as the commands that use them do, for the
/// refusal a deferred repair ends heal with once the marker is lowered: the first record that does
/// not read is refused with the next step judged now.
pub fn check() -> Result<(), String> {
    if let Some(state) = merge_utils::read_consolidation_state()? {
        merge_utils::OutOfScopeSkeleton::read_required(&state)?;
    }
    cherry_pick_utils::read_state()?;
    Ok(())
}
