//! Cherry-pick (§9.1 #8): apply one parcel's diff onto the current pallet as a new,
//! author-preserving, freshly-signed parcel.
//!
//! Cherry-pick is the sharpest gap left by declining rebase (§9.6), and it fits the
//! philosophy because it only *adds*: no rewrite, no force-push, nothing an audit has to
//! bless twice. The source parcel's change (the diff against its first parent) is three-way
//! merged onto the current head — exactly the merge machinery consolidate uses — and the
//! result is stacked as a **single-parent** parcel that preserves the source's authors and
//! records the picker as the stacker (the same Author/Stack split as import/export-git and
//! deliver). A clean pick is stacked immediately; a conflicting one leaves markers and a
//! cherry-pick state, and the next `stack` completes it — still single-parent, still
//! author-preserving.
//!
//! The state lives in the bay (a pick in progress belongs to the bay resolving it),
//! mutually exclusive with a consolidation.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use crate::enums::parcel_action_type::ParcelActionType;
use crate::globals::bay_root;
use crate::model::operator::Operator;
use crate::util::{file_utils, object_utils};

/// The name of the cherry-pick-state file (inside the bay root). While a pick is in
/// progress (its merge left conflicts), it records the source parcel, the intended description,
/// and the head and pallet the pick was applied to; the next `stack` reads it to complete the pick
/// single-parent and remove it. A file written before the head and pallet were recorded lacks
/// those fields and is refused as malformed.
const FILE_NAME_CHERRY_PICK: &str = "cherry-pick";

/// A cherry-pick in progress: the source parcel being applied, and the description the
/// completing `stack` should default to.
#[derive(Serialize, Deserialize)]
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

/// How to abort the cherry-pick in progress in this bay: a phrase such as `remove "<path>"`,
/// naming the bay's own state file.
pub fn abort_hint() -> String {
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

    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Error while reading \"{}\": {}", path.to_string_lossy(), e))?;

    let state: CherryPickState = serde_json::from_str(&content).map_err(|_| format!(
        "The cherry-pick state file \"{}\" is malformed; remove it to abort the pick.",
        path.to_string_lossy()
    ))?;

    let is_valid = object_utils::is_object_hash(&state.source) && object_utils::is_object_hash(&state.our_head)
        && crate::util::pallet_utils::validate_pallet_name(&state.our_pallet).is_ok();

    if !is_valid {
        return Err(format!(
            "The cherry-pick state file \"{}\" is malformed; remove it to abort the pick.",
            path.to_string_lossy()
        ));
    }

    Ok(Some(state))
}

/// Write the cherry-pick state (atomically).
///
/// # Arguments
/// * `state` - The state to write.
///
/// # Returns
/// * `Ok(())`      - If the state was written.
/// * `Err(String)` - If the file could not be serialized or written.
pub fn write_state(state: &CherryPickState) -> Result<(), String> {
    let json = serde_json::to_string(state)
        .map_err(|e| format!("Error while serializing the cherry-pick state: {}", e))?;

    file_utils::write_file_atomically(&state_path(), json.as_bytes())
}

/// Remove the cherry-pick state file (a no-op when none exists).
///
/// # Returns
/// * `Ok(())`      - If the state file is gone.
/// * `Err(String)` - If the file exists but could not be removed.
pub fn clear_state() -> Result<(), String> {
    let path = state_path();

    match std::fs::remove_file(&path) {
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
