//! The incomplete-load guard: `load` can fail partway through a directory walk (a per-shard
//! failure keeps the siblings that already published; some failure modes publish nothing at
//! all), and a subsequent `stack` would otherwise silently commit the incomplete staged
//! inventory into a durable parcel — a wrong result indistinguishable from a correct one. This
//! module closes that: a small durable marker recorded at the *start* of every load, cleared
//! only once that same load (or a later one covering the same root) completes cleanly. `stack`
//! refuses while a marker is present (see [`check_no_incomplete_load`]).
//!
//! ## Why the marker is written at load *start*, not on failure
//!
//! Marking only on failure would miss a load killed outright (a crash, `SIGKILL`, a power
//! loss) partway through its walk — the process never reaches its own error handling, so
//! nothing would ever record that the walk was interrupted. Writing the marker durably
//! *before* the walk begins, and clearing it only after the walk fully succeeds, catches both
//! failure modes with one mechanism: an explicit error return leaves the marker in place
//! exactly like a hard kill does.
//!
//! ## Storage
//!
//! The marker lives beside the inventory metadata file — directly under
//! [`file_utils::get_path_inventory_root`] (the bay-local staging root), a **sibling** of the
//! `inv_`-prefixed shard tree, never inside it. Every operation that wipes the staging area
//! wholesale (`inventory_utils::replace_all_inventories`, used by `park`'s push, and
//! `inventory_utils::replace_subtree_inventories` with an empty key, used by `restore --staged`
//! at the root) does so with `remove_dir_all` on that `inv_` subtree specifically — never on
//! the inventory root folder itself — so none of them can delete the marker in passing.
//!
//! ## Clearing scope
//!
//! The marker records a single warehouse path key: the root of whichever load(s) might still be
//! incomplete. Every read-modify-write of it follows one rule: a load rooted at `R` may only
//! touch a marker recorded at `P` when `R` *covers* `P` (`R` is `P` itself or a strict ancestor
//! of it — see [`covers`]; the empty key, the warehouse root, covers everything).
//!
//! * **At load start** ([`mark_load_started`]): no marker yet → write one for `R`. A marker
//!   already recorded at `P`, with `R` covering `P` → overwrite with `R` — still conservative,
//!   since `R` is at least as broad as `P`, so recording `R` alone still covers the old
//!   incomplete region. A marker at `P` that `R` does *not* cover (`P` is broader, or the two
//!   are unrelated) → leave it untouched: the marker is a single path, not a set, so it cannot
//!   always name two disjoint incomplete regions at once, but it must never *lose* the one it
//!   already has. `stack` still refuses either way — worst case it names the older path instead
//!   of the newer one.
//! * **At clean completion** ([`mark_load_completed`]): read whatever is currently recorded;
//!   clear it only if this load's root covers it (this load's walk fully re-verified everything
//!   the marker was protecting). A narrower, or disjoint, load leaves it alone — it never healed
//!   the broader failure.
//!
//! Removal is best-effort ([`mark_load_completed`] swallows every error): a marker that
//! resurrects after a crash right after a successful removal fails in the safe direction —
//! `stack` refuses, and the remedy is one cheap re-load — so a failed unlink here is not itself
//! a load failure.

use std::path::PathBuf;
use chrono::Utc;
use crate::error::{CoreError, RefusalCode};
use crate::util::file_utils;

/// The name of the incomplete-load marker file (kept beside the inventory metadata file — see
/// the module doc comment on why this placement is safe against a whole-staging-area wipe).
const FILE_NAME_INCOMPLETE_LOAD_MARKER: &str = "incomplete-load";

/// The stable `incomplete_load` refusal code, re-exported from the typed [`RefusalCode`] (the
/// same convention `scope_utils` and `query_utils` use for their own codes).
pub const CODE_INCOMPLETE_LOAD: &str = RefusalCode::IncompleteLoad.as_str();

/// A recorded incomplete-load marker.
struct Marker {
    /// The warehouse path key of the load root that may still be incomplete.
    path: String,
    /// When that load started (RFC 3339; informational only, shown in the refusal message).
    started_at: String,
}

/// The path of the incomplete-load marker file.
fn marker_path() -> PathBuf {
    PathBuf::from(file_utils::get_path_inventory_root()).join(FILE_NAME_INCOMPLETE_LOAD_MARKER)
}

/// Whether `root` "covers" `key` — `root` is `key` itself or a strict ancestor of it (the empty
/// key, the warehouse root, covers everything). The same subtree-membership test
/// `inventory_utils::populate_dirty_inventory_paths` and `restore`'s directory walk both use for
/// "is this key at or under this path".
fn covers(root: &str, key: &str) -> bool {
    root.is_empty() || key == root || key.starts_with(&format!("{}/", root))
}

/// Read the current marker, if any.
///
/// # Returns
/// * `Ok(Some(Marker))` - A marker is recorded.
/// * `Ok(None)`         - No marker is recorded.
/// * `Err(String)`      - The marker file exists but could not be read or is malformed.
fn read_marker() -> Result<Option<Marker>, String> {
    let path = marker_path();

    if !path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Error while reading \"{}\": {}", path.to_string_lossy(), e))?;

    let mut lines = content.lines();
    let (Some(marker_path_line), Some(started_at)) = (lines.next(), lines.next()) else {
        return Err(format!(
            "The incomplete-load marker \"{}\" is malformed; fix it by hand (or remove it and \
            re-run \"forklift load\" over the affected path).",
            path.to_string_lossy()
        ));
    };

    Ok(Some(Marker { path: marker_path_line.to_string(), started_at: started_at.to_string() }))
}

/// Write the marker for `root_key`, unconditionally — callers decide whether to call this (see
/// [`mark_load_started`]).
fn write_marker(root_key: &str) -> Result<(), String> {
    file_utils::create_folder_if_not_exists(&PathBuf::from(file_utils::get_path_inventory_root()))?;

    let content = format!("{}\n{}\n", root_key, Utc::now().to_rfc3339());
    file_utils::write_file_atomically(&marker_path(), content.as_bytes())
}

/// Remove the marker file, best-effort (a missing file, or a failed removal, is not an error —
/// see the module doc comment).
fn clear_marker() {
    let _ = std::fs::remove_file(marker_path());
}

/// Record that a load rooted at `root_key` is starting, durably, before any staging mutation —
/// see the module doc comment for the crash-safety rationale and the covers-based update rule.
///
/// # Arguments
/// * `root_key` - The warehouse path key of the load's root (the normalized `load` argument).
///
/// # Returns
/// * `Ok(())`      - The marker was written, or an existing broader-or-equal one was kept as is.
/// * `Err(String)` - The marker could not be read or written.
pub fn mark_load_started(root_key: &str) -> Result<(), String> {
    let should_write = match read_marker()? {
        None => true,
        Some(existing) => covers(root_key, &existing.path),
    };

    if should_write {
        write_marker(root_key)?;
    }

    Ok(())
}

/// Record that the load rooted at `root_key` completed cleanly: clears the marker if (and only
/// if) `root_key` covers whatever is currently recorded — see the module doc comment. Best-effort
/// throughout, including the read: a marker this cannot make sense of is left in place rather
/// than risk clearing state a real failure still needs, and a failed removal never fails the
/// load that already succeeded.
///
/// # Arguments
/// * `root_key` - The warehouse path key of the load that just completed cleanly.
pub fn mark_load_completed(root_key: &str) {
    let Ok(Some(existing)) = read_marker() else { return };

    if covers(root_key, &existing.path) {
        clear_marker();
    }
}

/// Display a warehouse path key the way user-facing messages spell the warehouse root.
fn display_path(key: &str) -> &str {
    if key.is_empty() { "./" } else { key }
}

/// Build the `incomplete_load` refusal naming the recorded path and when that load started.
fn incomplete_load_refusal(marker: &Marker) -> CoreError {
    let path = display_path(&marker.path);
    let next_step = format!("Run \"forklift load {}\" again, then stack.", path);

    CoreError::refusal(
        RefusalCode::IncompleteLoad,
        format!(
            "The last load of \"{}\" (started {}) did not finish; stacking now could commit an \
            incomplete inventory. {}",
            path, marker.started_at, next_step
        ),
        next_step,
    )
}

/// Refuse if an incomplete load is recorded. Called by `stack` before any warehouse mutation
/// (alongside its other pre-checks — see `stack_utils::stack_parcel`).
///
/// # Returns
/// * `Ok(())`         - No incomplete load is recorded; stacking may proceed.
/// * `Err(CoreError)` - An `incomplete_load` refusal naming the recorded path, or the marker
///                      could not be read.
pub fn check_no_incomplete_load() -> Result<(), CoreError> {
    match read_marker().map_err(CoreError::Other)? {
        Some(marker) => Err(incomplete_load_refusal(&marker)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::StorageRootScope;

    fn temp_warehouse(name: &str) -> std::path::PathBuf {
        let temp = std::env::temp_dir().join(format!("forklift-load-guard-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(temp.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        temp
    }

    #[test]
    fn a_clean_load_of_the_same_root_clears_its_own_marker() {
        let temp = temp_warehouse("same-root");
        let _scope = StorageRootScope::enter(&temp);

        mark_load_started("src").unwrap();
        assert!(check_no_incomplete_load().is_err());

        mark_load_completed("src");
        assert!(check_no_incomplete_load().is_ok());

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn a_narrower_clean_load_does_not_clear_a_broader_failed_root() {
        let temp = temp_warehouse("narrower");
        let _scope = StorageRootScope::enter(&temp);

        mark_load_started("").unwrap(); // a whole-warehouse load starts (and never finishes)
        mark_load_completed("src"); // a narrower, unrelated load finishes cleanly afterward

        let error = check_no_incomplete_load().unwrap_err();
        match error {
            CoreError::Refusal { code, message, .. } => {
                assert_eq!(code, RefusalCode::IncompleteLoad);
                assert!(message.contains("./"), "names the broader recorded root: {}", message);
            }
            other => panic!("expected a refusal, got {:?}", other),
        }

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn a_new_broader_load_overwrites_a_narrower_recorded_marker() {
        let temp = temp_warehouse("broader-overwrite");
        let _scope = StorageRootScope::enter(&temp);

        mark_load_started("src/api").unwrap();
        mark_load_started("src").unwrap(); // covers the old marker's root — overwrites

        mark_load_completed("src"); // now covers the (updated) recorded root — clears
        assert!(check_no_incomplete_load().is_ok());

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn a_disjoint_new_load_does_not_overwrite_an_unrelated_recorded_marker() {
        let temp = temp_warehouse("disjoint");
        let _scope = StorageRootScope::enter(&temp);

        mark_load_started("src/a").unwrap();
        mark_load_started("src/b").unwrap(); // does not cover "src/a" — leaves it recorded

        let error = check_no_incomplete_load().unwrap_err();
        match error {
            CoreError::Refusal { message, .. } => {
                assert!(message.contains("src/a"), "keeps naming the original root: {}", message);
            }
            other => panic!("expected a refusal, got {:?}", other),
        }

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn no_marker_means_stack_is_never_refused() {
        let temp = temp_warehouse("no-marker");
        let _scope = StorageRootScope::enter(&temp);

        assert!(check_no_incomplete_load().is_ok());

        std::fs::remove_dir_all(&temp).ok();
    }
}
