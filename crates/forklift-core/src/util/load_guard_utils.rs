//! The incomplete-load guard: `load` can fail partway through a directory walk (a per-shard
//! failure keeps the siblings that already published; some failure modes publish nothing at
//! all), and a subsequent `stack` or `park` would otherwise silently commit the incomplete
//! staged inventory into a durable parcel — a wrong result indistinguishable from a correct one.
//! This module closes that: durable markers recorded at the *start* of every load, cleared only
//! once that load (or a later operation that makes the same region consistent again) resolves
//! them. `stack` and `park` both refuse while any marker is present (see
//! [`check_no_incomplete_load`]).
//!
//! Built on [`marker_utils`], the generic durable path-**set** primitive: this module owns only
//! the load-specific meaning of "a path is recorded" (an outstanding, possibly-incomplete load
//! rooted there) and where the marker file lives; every read/write/antichain concern is
//! `marker_utils`'s.
//!
//! ## Why a set, not a single path
//!
//! Two unrelated loads can each fail independently — `load src/a` fails, then (before it is
//! healed) `load src/b` fails too. Those are two disjoint incomplete regions; recording only one
//! at a time would either lose track of the first or never record the second, either way leaving
//! the other's staged (or half-staged) content unguarded. `marker_utils`'s set, and its
//! antichain-maintaining [`marker_utils::add`], handle this: both roots are recorded
//! side by side, and each is only cleared by an operation that actually covers it.
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
//! `inv_`-prefixed shard tree, never inside it. Every operation that replaces staging wholesale
//! or by subtree (`inventory_utils::replace_all_inventories`, `inventory_utils::
//! replace_subtree_inventories`) removes only that `inv_` subtree specifically — never the
//! inventory root folder itself — so none of them can delete the marker in passing; those same
//! two functions are also where this module's clearing side of the contract is wired in (see
//! [`clear_recorded_under`] and [`clear_all_recorded`]).
//!
//! ## Folding the marker's durability into a load's own barrier
//!
//! Recording a marker via a standalone [`file_utils::write_file_atomically`] call pays its own
//! durability barrier — on a platform where that barrier is expensive (macOS's `F_FULLFSYNC`),
//! doing this on every load roughly doubles the durability wait of a small one. The marker does
//! not need its own barrier: it only needs to be durable no later than the load's own first real
//! durable mutation, so folding it into that mutation's existing barrier is free.
//!
//! [`pending_start_marker`] computes the bytes a caller needs to durably publish (or `None` when
//! nothing needs to change) without writing them, so the caller can stage them into whatever
//! `WriteBatch` its own first barrier already uses ([`stage_start_marker`] does this directly
//! for a caller with a `WriteBatch` in hand):
//! * `inventory_utils::create_inventory_for_directory` (a directory `load`) stages it into the
//!   walk's own blob-publish batch, the walk's first durable mutation — see that function's own
//!   doc comment for the crash-ordering argument.
//! * `inventory_utils::add_file_to_inventory` (a single-file `load`) folds it into the loaded
//!   file's own shard-content publish via `inventory_utils::write_shard_mutation_with_extra` —
//!   that shard write is this path's first (and only) durable mutation.
//!
//! Crash analysis: a crash before either of those barriers commits leaves the marker's write
//! unpublished (an invisible staged temp — see `file_utils::WriteBatch::stage`) exactly like
//! every other write that barrier covers, and staging is unchanged either way — consistent. A
//! crash after the barrier commits makes the marker durable together with (never after) whatever
//! else that barrier published, so a load interrupted right past that point is still caught. In
//! neither case can the marker's absence and a durable staging mutation from the same load ever
//! disagree.
//!
//! ## Clearing scope
//!
//! Every read-modify-write of the recorded set follows one rule, enforced by `marker_utils`, with
//! no exception anywhere in this module: an operation whose own validation covers region `R` (see
//! [`path_utils::covers`]) may only remove a recorded root `P` when `R` covers `P` — never a
//! broader `P` that merely happens to cover `R` in the other direction. `R` being fully accounted
//! for proves nothing about the rest of a broader `P`'s region, which the operation never looked
//! at; clearing it anyway would silently launder an unverified region as resolved. (An earlier
//! version of [`clear_recorded_under`]'s underlying primitive also cleared in that other
//! direction, on the theory that a *target* having nothing outstanding was cause enough on its
//! own — live regression, since fixed: see `marker_utils::remove_covered_by`'s doc comment.)
//!
//! * **At load start** ([`pending_start_marker`], [`stage_start_marker`]): recording `R` absorbs
//!   (drops) any narrower already-recorded root `R` covers — see `marker_utils`'s antichain
//!   invariant.
//! * **At clean completion** ([`mark_load_completed`]): every recorded root the completed load's
//!   own root covers is cleared. A narrower, or disjoint, load leaves everything else alone — it
//!   never re-walked what it does not cover.
//! * **At a staging reset** ([`clear_recorded_under`], [`clear_all_recorded`]): `restore
//!   --staged <path>` (and `unload`) rebuild exactly the subtree at `<path>` from the pallet
//!   head, so every recorded root that subtree covers is now known-consistent — call
//!   [`clear_recorded_under`]. The same call also covers `restore --staged`'s vacuous case (a
//!   target neither in the inventory nor the head, but with zero staged entries anywhere beneath
//!   it either — see `restore::restore_staged`'s doc comment): the region it validated is exactly
//!   `<path>`'s own subtree, so the same "only what `R` covers" rule already gives the right
//!   answer with no separate function needed. `shift`, `park`'s post-push reset, a `consolidate`
//!   fast-forward, `import-git`'s initial checkout, and `lower`/`franchise`'s materialization all
//!   replace the *entire* staging area from a known-good tree — call [`clear_all_recorded`]. Both
//!   are wired once, inside `inventory_utils::replace_subtree_inventories` and `inventory_utils::
//!   replace_all_inventories` respectively, so every caller of either gets this for free.
//!
//! Removal is always best-effort (`marker_utils::remove_covered_by`/`clear` swallow every error):
//! a marker that resurrects after a crash right after a successful removal fails in the safe
//! direction — the next check still refuses, and the remedy is one cheap re-load or
//! `restore --staged`.
//!
//! ## Malformed marker
//!
//! Write-side ([`pending_start_marker`], [`stage_start_marker`]) and check-side
//! ([`check_no_incomplete_load`]) deliberately disagree about what a malformed marker means — see
//! `marker_utils`'s doc comment for why: a load must never be the thing its own remedy is blocked
//! on, so it self-heals by overwriting; a `stack`/`park` about to durably commit must never treat
//! "I could not read this" as "nothing is recorded", so it refuses.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use crate::error::{CoreError, RefusalCode};
use crate::util::{file_utils, marker_utils};

/// The name of the incomplete-load marker file (kept beside the inventory metadata file — see
/// the module doc comment on why this placement is safe against a whole-staging-area wipe).
const FILE_NAME_INCOMPLETE_LOAD_MARKER: &str = "incomplete-load";

/// The stable `incomplete_load` refusal code, re-exported from the typed [`RefusalCode`] (the
/// same convention `scope_utils` and `query_utils` use for their own codes).
pub const CODE_INCOMPLETE_LOAD: &str = RefusalCode::IncompleteLoad.as_str();

/// The path of the incomplete-load marker file, resolved against the active storage-root scope
/// (see [`file_utils::get_path_inventory_root`]) — this process's view of where it lives.
pub fn marker_path() -> PathBuf {
    PathBuf::from(file_utils::get_path_inventory_root()).join(FILE_NAME_INCOMPLETE_LOAD_MARKER)
}

/// The path of the incomplete-load marker file under a given warehouse root, independent of any
/// active process-wide storage-root scope — for a caller that already knows the target
/// warehouse's root directly rather than through the ambient scope [`marker_path`] resolves
/// against (a test driving `forklift` as a spawned subprocess, whose own working directory has
/// nothing to do with the warehouse being exercised). Assumes a plain, non-bay warehouse layout
/// (`<root>/.forklift/inventory/...`), which is all such a caller ever drives.
pub fn marker_path_under(warehouse_root: &Path) -> PathBuf {
    warehouse_root
        .join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)
        .join(crate::globals::FOLDER_NAME_INVENTORY_ROOT)
        .join(FILE_NAME_INCOMPLETE_LOAD_MARKER)
}

/// Compute the marker write a load rooted at `root_key` needs to durably publish, without
/// performing it — see the module doc comment's folding section.
///
/// # Returns
/// * `Some(bytes)` - `root_key` is not yet covered by what is recorded; stage this content at
///                   [`marker_path`] as part of the caller's own first durable barrier.
/// * `None`        - `root_key` is already covered by what is recorded; nothing to stage.
pub fn pending_start_marker(root_key: &str) -> Option<Vec<u8>> {
    marker_utils::pending_add(&marker_path(), root_key)
}

/// Stage the marker write (if any) for `root_key` into `batch` instead of writing it immediately
/// — see the module doc comment's folding section. A no-op (nothing staged) when `root_key` is
/// already covered by what is recorded.
///
/// The caller is responsible for calling `batch.finish()`; nothing staged here is durable until
/// that returns `Ok`.
pub fn stage_start_marker(batch: &file_utils::WriteBatch, root_key: &str) -> Result<(), String> {
    marker_utils::add_deferred(batch, &marker_path(), root_key)
}

/// Record that the load rooted at `root_key` completed cleanly: clears every recorded root
/// `root_key` covers — see the module doc comment's clearing-scope section. Best-effort
/// throughout (including a marker this cannot make sense of, which is left in place rather than
/// risk clearing state a real failure still needs) — a failed removal never fails the load that
/// already succeeded.
///
/// # Arguments
/// * `root_key` - The warehouse path key of the load that just completed cleanly.
pub fn mark_load_completed(root_key: &str) {
    clear_recorded_under(root_key);
}

/// Record that region `root_key` is now known-consistent — because a load rooted there just
/// finished cleanly, because some other operation rebuilt exactly that subtree from a known-good
/// source (`restore --staged`, `unload`), or because `restore --staged`'s vacuous-heal case (see
/// `restore::restore_staged`'s doc comment) has already confirmed `root_key`'s own subtree has
/// zero staged entries anywhere beneath it — that confirmation *is* `root_key`'s validation, so
/// the same "only clear what was actually validated" rule applies unchanged.
///
/// Clears every recorded root `root_key` covers; a root only partially overlapping `root_key`, or
/// *broader* than it, is left alone even if `root_key` itself is fully accounted for — see the
/// module doc comment's clearing-scope section for why the broader direction is never safe.
/// Best-effort (see [`mark_load_completed`]'s doc comment).
///
/// # Returns
/// The recorded root(s) actually cleared — empty when `root_key` did not cover any recorded root
/// at all (a caller like `restore --staged`'s vacuous-heal case uses this to fall back to its
/// ordinary, unrelated-path handling instead of reporting a heal).
pub fn clear_recorded_under(root_key: &str) -> BTreeSet<String> {
    marker_utils::remove_covered_by(&marker_path(), root_key)
}

/// Record that the *entire* staging area is now known-consistent — every operation that replaces
/// staging wholesale from a known-good tree (`shift`, `park`'s post-push reset, a `consolidate`
/// fast-forward, `import-git`'s initial checkout, `lower`/`franchise`'s materialization) calls
/// this once it has finished. Best-effort (see [`mark_load_completed`]'s doc comment).
pub fn clear_all_recorded() {
    marker_utils::clear(&marker_path());
}

/// Display a warehouse path key the way user-facing messages spell the warehouse root — the one
/// place this codebase's `""`-means-root convention becomes the string a user actually reads, so
/// every caller shares it instead of re-implementing the same `is_empty` check.
pub fn display_path(key: &str) -> &str {
    if key.is_empty() { "./" } else { key }
}

/// Format up to a handful of recorded roots for a message, with a count for the rest.
fn format_roots(recorded: &BTreeSet<String>) -> String {
    const SHOWN: usize = 3;

    let mut description = recorded.iter()
        .take(SHOWN)
        .map(|key| display_path(key))
        .collect::<Vec<_>>()
        .join(", ");

    if recorded.len() > SHOWN {
        description.push_str(&format!(", and {} more", recorded.len() - SHOWN));
    }

    description
}

/// Build the `incomplete_load` refusal naming the recorded root(s).
fn incomplete_load_refusal(recorded: &BTreeSet<String>) -> CoreError {
    let description = if recorded.len() == 1 {
        format!("The load of \"{}\"", display_path(recorded.iter().next().expect("len == 1")))
    } else {
        format!("{} loads ({})", recorded.len(), format_roots(recorded))
    };

    let next_step = if recorded.len() == 1 {
        let path = display_path(recorded.iter().next().expect("len == 1"));
        format!(
            "Run \"forklift load {}\" again, or \"forklift restore --staged {}\" to abandon it.",
            path, path
        )
    } else {
        format!(
            "Re-run \"forklift load\" over each affected path ({}), or \"forklift restore \
            --staged <path>\" to abandon it, for each.",
            format_roots(recorded)
        )
    };

    CoreError::refusal(
        RefusalCode::IncompleteLoad,
        format!(
            "{} did not finish cleanly; committing the staged inventory now could produce an \
            incomplete result. {}",
            description, next_step
        ),
        next_step,
    )
}

/// Build the `incomplete_load` refusal for a marker that could not be read at all — see the
/// module doc comment's malformed-marker section for why the check side refuses here rather than
/// treating this as "nothing recorded".
fn malformed_marker_refusal(read_error: &str) -> CoreError {
    let path = marker_path();
    let next_step = format!(
        "Fix or remove \"{}\" by hand, or run \"forklift restore --staged <path>\" over the \
        affected area, then try again.",
        path.to_string_lossy()
    );

    CoreError::refusal(
        RefusalCode::IncompleteLoad,
        format!(
            "The incomplete-load marker could not be read ({}); treating this warehouse as \
            having an incomplete load rather than risk committing an incomplete result. {}",
            read_error, next_step
        ),
        next_step,
    )
}

/// Refuse if an incomplete load is recorded. Called by `stack` and `park`'s push before any
/// warehouse mutation (alongside their other pre-checks — see `stack_utils::stack_parcel` and
/// `park::park_changes`) — both durably commit the staged inventory into a parcel, so both must
/// never do so while a load might still be incomplete.
///
/// # Returns
/// * `Ok(())`         - No incomplete load is recorded; committing may proceed.
/// * `Err(CoreError)` - An `incomplete_load` refusal naming the recorded root(s), or — if the
///                      marker exists but could not be read — refusing conservatively rather
///                      than risk treating unreadable state as "nothing recorded" (see the
///                      module doc comment's malformed-marker section).
pub fn check_no_incomplete_load() -> Result<(), CoreError> {
    match marker_utils::read(&marker_path()) {
        Ok(recorded) if recorded.is_empty() => Ok(()),
        Ok(recorded) => Err(incomplete_load_refusal(&recorded)),
        Err(read_error) => Err(malformed_marker_refusal(&read_error)),
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

    /// Record a load start the same way every real caller does — there is no standalone,
    /// immediately-writing entry point in production code (both `load` paths fold this into
    /// their own first durable barrier; see the module doc comment's folding section) — so this
    /// drives the tests below through that same deferred path instead of a test-only shortcut.
    fn start_load(root_key: &str) {
        let batch = file_utils::WriteBatch::new();
        stage_start_marker(&batch, root_key).unwrap();
        batch.finish().unwrap();
    }

    #[test]
    fn a_clean_load_of_the_same_root_clears_its_own_marker() {
        let temp = temp_warehouse("same-root");
        let _scope = StorageRootScope::enter(&temp);

        start_load("src");
        assert!(check_no_incomplete_load().is_err());

        mark_load_completed("src");
        assert!(check_no_incomplete_load().is_ok());

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn a_narrower_clean_load_does_not_clear_a_broader_failed_root() {
        let temp = temp_warehouse("narrower");
        let _scope = StorageRootScope::enter(&temp);

        start_load(""); // a whole-warehouse load starts (and never finishes)
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
    fn a_new_broader_load_absorbs_a_narrower_recorded_marker() {
        let temp = temp_warehouse("broader-absorb");
        let _scope = StorageRootScope::enter(&temp);

        start_load("src/api");
        start_load("src"); // covers the old marker's root — absorbs it

        mark_load_completed("src"); // now covers the (updated) recorded root — clears
        assert!(check_no_incomplete_load().is_ok());

        std::fs::remove_dir_all(&temp).ok();
    }

    /// Two disjoint failed loads must both be recorded — a single-path marker could only ever
    /// name one, silently leaving the other's staged (or
    /// half-staged) content unguarded. `stack` (proxied here by `check_no_incomplete_load`)
    /// must keep refusing, naming the still-outstanding root, until *both* heal.
    #[test]
    fn two_disjoint_failed_loads_are_both_recorded_until_each_heals() {
        let temp = temp_warehouse("disjoint");
        let _scope = StorageRootScope::enter(&temp);

        start_load("src/a");
        start_load("src/b"); // does not cover "src/a" — both recorded

        let error = check_no_incomplete_load().unwrap_err();
        match error {
            CoreError::Refusal { message, .. } => {
                assert!(message.contains("src/a"), "names the first root: {}", message);
                assert!(message.contains("src/b"), "names the second root: {}", message);
            }
            other => panic!("expected a refusal, got {:?}", other),
        }

        // Healing only "src/a" must not clear "src/b".
        mark_load_completed("src/a");
        let error = check_no_incomplete_load().unwrap_err();
        match error {
            CoreError::Refusal { message, .. } => {
                assert!(!message.contains("src/a"), "src/a healed, must not still be named: {}", message);
                assert!(message.contains("src/b"), "src/b still outstanding: {}", message);
            }
            other => panic!("expected a refusal, got {:?}", other),
        }

        // Healing "src/b" too clears the guard entirely.
        mark_load_completed("src/b");
        assert!(check_no_incomplete_load().is_ok());

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn no_marker_means_committing_is_never_refused() {
        let temp = temp_warehouse("no-marker");
        let _scope = StorageRootScope::enter(&temp);

        assert!(check_no_incomplete_load().is_ok());

        std::fs::remove_dir_all(&temp).ok();
    }

    /// A malformed marker never blocks the load that would fix it (self-heals by overwriting),
    /// but a check about to durably commit refuses conservatively rather than treat unreadable
    /// state as "nothing recorded".
    #[test]
    fn a_malformed_marker_self_heals_on_write_but_refuses_the_check() {
        let temp = temp_warehouse("malformed");
        let _scope = StorageRootScope::enter(&temp);

        std::fs::create_dir_all(marker_path().parent().unwrap()).unwrap();
        std::fs::write(marker_path(), [0xFF, 0xFE, 0x00, 0xFF]).unwrap(); // never valid UTF-8

        // The check side must fail closed, never silently pass.
        let error = check_no_incomplete_load().unwrap_err();
        assert!(matches!(error, CoreError::Refusal { code: RefusalCode::IncompleteLoad, .. }));

        // The write side self-heals: a load over the malformed marker proceeds and records
        // fresh, readable content.
        start_load("src");
        let error = check_no_incomplete_load().unwrap_err();
        match error {
            CoreError::Refusal { message, .. } => assert!(message.contains("src")),
            other => panic!("expected a refusal, got {:?}", other),
        }

        mark_load_completed("src");
        assert!(check_no_incomplete_load().is_ok());

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn clear_recorded_under_a_partial_overlap_leaves_a_broader_root_alone() {
        let temp = temp_warehouse("partial-overlap");
        let _scope = StorageRootScope::enter(&temp);

        start_load("src");
        clear_recorded_under("src/sub"); // does not cover "src" (its own ancestor)

        assert!(check_no_incomplete_load().is_err());

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn clear_all_recorded_clears_every_root_regardless_of_overlap() {
        let temp = temp_warehouse("clear-all");
        let _scope = StorageRootScope::enter(&temp);

        start_load("src/a");
        start_load("docs");

        clear_all_recorded();
        assert!(check_no_incomplete_load().is_ok());

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn pending_start_marker_is_none_when_already_covered_and_some_otherwise() {
        let temp = temp_warehouse("pending");
        let _scope = StorageRootScope::enter(&temp);

        assert!(pending_start_marker("src").is_some());
        start_load("src");
        assert!(pending_start_marker("src/api").is_none(), "already covered by \"src\"");
        assert!(pending_start_marker("docs").is_some(), "disjoint from \"src\"");

        std::fs::remove_dir_all(&temp).ok();
    }
}
