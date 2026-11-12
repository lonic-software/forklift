//! Cherry-pick (§9.1 #8): apply one parcel's diff onto the current pallet as a new,
//! author-preserving, freshly-signed parcel.
//!
//! Cherry-pick is the sharpest gap left by declining rebase (§9.6), and it fits the
//! philosophy because it only *adds*: no rewrite, no force-push, nothing an audit has to
//! bless twice. The source parcel's change (the diff against its first parent) is three-way
//! merged onto the current head — exactly the merge machinery consolidate uses — and the
//! result is stacked as a **single-parent** parcel that preserves the source's authors and
//! records the picker as the stacker (the same Author/Stack split as git import and export and
//! deliver). A clean pick is stacked immediately; a conflicting one leaves markers and a
//! cherry-pick state, and the next `stack` completes it — still single-parent, still
//! author-preserving.
//!
//! The state lives in the bay (a pick in progress belongs to the bay resolving it),
//! mutually exclusive with a consolidation.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use crate::enums::parcel_action_type::ParcelActionType;
use crate::globals::bay_root;
use crate::model::operator::Operator;
use crate::util::{file_utils, object_utils, record_utils};

/// The name of the cherry-pick-state file (inside the bay root). While a pick is in
/// progress (its merge left conflicts), it records the source parcel, the head and pallet the pick
/// was applied to, and the intended description; the next `stack` reads it to complete the pick
/// single-parent and remove it.
const FILE_NAME_CHERRY_PICK: &str = "cherry-pick";

/// The state record kind of the cherry-pick-state file. Its entries are the source, the head, the
/// pallet, and then the description when there is one.
const CHERRY_PICK_RECORD: &str = "cherry-pick";

/// The version of the cherry-pick-state record.
const CHERRY_PICK_VERSION: u32 = 1;

/// A cherry-pick in progress: the source parcel being applied, and the description the
/// completing `stack` should default to.
pub struct CherryPickState {
    /// The parcel whose diff is being applied (its authors are preserved on completion).
    pub source: String,

    /// The description the completing parcel should carry when the user gives none.
    pub description: Option<String>,

    /// The current pallet's head the pick was applied to: the completing parcel's parent, and the
    /// head the completing `stack` expects the pallet still to hold.
    pub our_head: String,

    /// The pallet the pick was applied to, and the only one `stack` completes it on.
    pub our_pallet: String,
}

/// Get the path of the cherry-pick-state file (bay-local).
fn state_path() -> PathBuf {
    bay_root().join(FILE_NAME_CHERRY_PICK)
}

/// How to abandon the pick in progress in this bay without putting anything back: a phrase such as
/// `remove "<path>"`, naming the bay's own state file. For where `abort` cannot run: an
/// unreadable state or source parcel, or a bay no longer at the head the pick was applied on.
pub fn state_removal_hint() -> String {
    format!("remove \"{}\"", state_path().to_string_lossy())
}

/// Read the cherry-pick state, if a pick is in progress.
///
/// # Returns
/// * `Ok(Some(CherryPickState))` - The state of the pick in progress.
/// * `Ok(None)`                  - If no pick is in progress.
/// * `Err(String)`               - If the state file exists but is malformed.
pub fn read_state() -> Result<Option<CherryPickState>, String> {
    read_state_in(&bay_root())
}

/// Read the cherry-pick state of the bay whose state lives in `dir`, if a pick is in progress
/// there — see [`read_state`].
///
/// # Arguments
/// * `dir` - The bay's state directory.
pub fn read_state_in(dir: &Path) -> Result<Option<CherryPickState>, String> {
    let path = dir.join(FILE_NAME_CHERRY_PICK);

    if !path.exists() {
        return Ok(None);
    }

    let bytes = std::fs::read(&path)
        .map_err(|e| format!("Error while reading \"{}\": {}", path.to_string_lossy(), e))?;

    let malformed = || format!(
        "The pick state file \"{}\" is malformed; remove it to abort the pick.",
        path.to_string_lossy()
    );

    let entries = record_utils::from_bytes(&bytes, CHERRY_PICK_RECORD, CHERRY_PICK_VERSION).map_err(|e| format!(
        "The pick state file \"{}\" {} Remove it to abort the pick.", path.to_string_lossy(), e.describe()
    ))?;

    let mut entries = entries.into_iter();
    let (Some(source), Some(our_head), Some(our_pallet)) = (entries.next(), entries.next(), entries.next()) else {
        return Err(malformed());
    };
    let description = entries.next();
    if entries.next().is_some() {
        return Err(malformed());
    }

    let is_valid = object_utils::is_object_hash(&source) && object_utils::is_object_hash(&our_head)
        && crate::util::pallet_utils::validate_pallet_name(&our_pallet).is_ok();

    if !is_valid {
        return Err(malformed());
    }

    let state = CherryPickState { source, description, our_head, our_pallet };

    Ok(Some(state))
}

/// Write the cherry-pick state (atomically).
///
/// # Arguments
/// * `state` - The state to write.
///
/// # Returns
/// * `Ok(())`      - If the state was written.
/// * `Err(String)` - If the file could not be written.
pub fn write_state(state: &CherryPickState) -> Result<(), String> {
    file_utils::write_file_atomically(&state_path(), &state_bytes(state))
}

/// The bytes [`write_state`] writes for `state`.
pub(crate) fn state_bytes(state: &CherryPickState) -> Vec<u8> {
    let mut entries = vec![state.source.as_str(), state.our_head.as_str(), state.our_pallet.as_str()];
    entries.extend(state.description.as_deref());

    record_utils::to_bytes(CHERRY_PICK_RECORD, CHERRY_PICK_VERSION, entries)
}

/// Remove the cherry-pick state file (a no-op when none exists).
///
/// # Returns
/// * `Ok(())`      - If the state file is gone.
/// * `Err(String)` - If the file exists but could not be removed.
pub fn clear_state() -> Result<(), String> {
    let path = state_path();

    match file_utils::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("Error while removing \"{}\": {}", path.to_string_lossy(), e)),
    }
}

/// The distinct authors of a source parcel, in first-seen order (by identifier) — the
/// authors a cherry-pick preserves. Falls back to the picker when the source records none.
///
/// # Arguments
/// * `source` - The hash of the source parcel.
/// * `picker` - The operator performing the pick (the fallback author).
///
/// # Returns
/// * `Ok(Vec<Operator>)` - The preserved authors.
/// * `Err(String)`       - If the source parcel could not be loaded.
pub fn collect_source_authors(source: &str, picker: &Operator) -> Result<Vec<Operator>, String> {
    let parcel = object_utils::load_parcel(source)?;

    let mut authors: Vec<Operator> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for action in parcel.actions {
        if matches!(action.action, ParcelActionType::Author)
            && seen.insert(action.operator.identifier.clone()) {
            authors.push(action.operator);
        }
    }

    if authors.is_empty() {
        authors.push(picker.clone());
    }

    Ok(authors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forklift-pick-state-{}-{}", tag, std::process::id()));
        let _ = file_utils::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn state(description: Option<&str>) -> CherryPickState {
        CherryPickState {
            source: object_utils::hash_object_bytes(b"source"),
            description: description.map(str::to_string),
            our_head: object_utils::hash_object_bytes(b"head"),
            our_pallet: "main".to_string(),
        }
    }

    fn write(dir: &Path, bytes: &[u8]) {
        std::fs::write(dir.join(FILE_NAME_CHERRY_PICK), bytes).unwrap();
    }

    /// A state reads back field for field, with a description (one holding a line break included)
    /// and without one.
    #[test]
    fn the_pick_state_record_round_trips() {
        let dir = scratch("round-trip");

        for description in [Some("a pick\nover two lines"), None] {
            let written = state(description);
            write(&dir, &state_bytes(&written));
            let read = read_state_in(&dir).unwrap().unwrap();

            assert_eq!(read.source, written.source);
            assert_eq!(read.our_head, written.our_head);
            assert_eq!(read.our_pallet, written.our_pallet);
            assert_eq!(read.description, written.description);
        }
        let _ = file_utils::remove_dir_all(&dir);
    }

    /// A state file without a header, such as the JSON written before it was a record, is refused.
    #[test]
    fn a_pick_state_without_a_header_is_refused() {
        let dir = scratch("unversioned");
        write(&dir, b"{\"source\":\"x\"}\n");

        let error = read_state_in(&dir).err().unwrap();
        assert!(error.contains("has no format header"), "{}", error);
        assert!(error.contains("Remove it to abort the pick."), "{}", error);
        let _ = file_utils::remove_dir_all(&dir);
    }

    /// A record with fewer than three entries or more than four is refused, as is one whose head is
    /// not a hash.
    #[test]
    fn a_pick_state_with_the_wrong_entries_is_refused() {
        let dir = scratch("entries");
        let good = state(Some("description"));

        let short = [good.source.as_str(), good.our_head.as_str()];
        let long = [good.source.as_str(), good.our_head.as_str(), "main", "description", "extra"];
        let bad_head = [good.source.as_str(), "not-a-hash", "main"];

        for entries in [&short[..], &long[..], &bad_head[..]] {
            write(&dir, &record_utils::to_bytes(CHERRY_PICK_RECORD, CHERRY_PICK_VERSION, entries.iter().copied()));
            let error = read_state_in(&dir).err().unwrap_or_else(|| panic!("{:?} must be refused", entries));
            assert!(error.contains("is malformed"), "{}", error);
        }
        let _ = file_utils::remove_dir_all(&dir);
    }
}
