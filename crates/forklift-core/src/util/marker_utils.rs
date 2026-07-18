//! A durable marker file recording a **set** of normalized warehouse-relative path keys — the
//! generic primitive behind [`crate::util::load_guard_utils`]'s incomplete-load guard. A second,
//! unrelated consumer (a future storage-completeness marker) is expected to reuse this same
//! primitive for its own set of recorded keys; this module knows nothing about what recording a
//! key *means* to either consumer, only how to keep a set of them durable on disk.
//!
//! ## Why a set, not a single path
//!
//! A marker that can only ever name one path cannot describe two unrelated regions at once: if a
//! second, disjoint region needs recording while the first is still outstanding, a single-slot
//! marker is forced to either drop the first (silently losing what it was protecting) or refuse
//! to record the second (silently *not* protecting it). Neither is safe. A set has no such limit.
//!
//! ## Antichain invariant
//!
//! The recorded set never contains two keys where one [`covers`](path_utils::covers) the other —
//! [`add`] enforces this on every write (see its doc comment). This keeps the set an accurate,
//! non-redundant description of "which regions are outstanding": a broader recorded key already
//! implies every narrower one nested inside it, so there is never anything to gain by keeping
//! both.
//!
//! ## On-disk format
//!
//! One normalized path key per line, sorted. The warehouse root is a literal blank line (`""`),
//! never specially spelled or omitted — [`str::lines`] round-trips this correctly (a genuinely
//! empty file, or a missing one, yields zero lines; a file containing a lone `"\n"` yields exactly
//! one, the empty string). A missing file reads as the empty set.
//!
//! ## Malformed content
//!
//! The only way this module considers a marker file malformed is if its bytes are not valid
//! UTF-8 ([`read`] returns `Err` in that case) — the line-per-key format has no other structural
//! requirement to violate. Every consumer built on this module is expected to follow one
//! convention for handling that:
//! * **Write side** (recording a new key): treat malformed existing content exactly like a
//!   missing file — self-heal by overwriting with a fresh set. [`add`] does this unconditionally.
//! * **Check side** (deciding whether to refuse based on what is recorded): malformed content
//!   must never be silently treated as "nothing recorded" — that defeats whatever the recorded
//!   set exists to guard against. A consumer's check must treat a malformed marker as at least as
//!   restrictive as a non-empty one, and its message should point at a recovery path.
//!
//! ## Durability
//!
//! [`add`] durably records its change before returning, via
//! [`file_utils::write_file_atomically`]. A consumer whose first real durable mutation is a
//! [`file_utils::WriteBatch`] it already pays for can fold the marker's write into that same
//! barrier instead of paying a second one — see [`pending_add`], which computes the bytes to
//! stage without writing them. [`remove_covered_by`] and [`clear`] are best-effort: the safe
//! failure direction for both is "the marker survives" (a caller re-checks and correctly refuses
//! again), never "the marker silently vanishes" (which would defeat the guard).

use std::collections::BTreeSet;
use std::path::Path;
use crate::util::{file_utils, path_utils};

/// Read the set of path keys currently recorded at `path`.
///
/// # Returns
/// * `Ok(set)`     - Possibly empty (a missing file reads as the empty set).
/// * `Err(String)` - The file exists but its content is not valid UTF-8.
pub fn read(path: &Path) -> Result<BTreeSet<String>, String> {
    if !path.exists() {
        return Ok(BTreeSet::new());
    }

    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Error while reading \"{}\": {}", path.to_string_lossy(), e))?;

    Ok(content.lines().map(str::to_string).collect())
}

/// Serialize `set` to its on-disk form — see the module doc comment's format section.
fn serialize(set: &BTreeSet<String>) -> Vec<u8> {
    let mut content = String::new();
    for key in set {
        content.push_str(key);
        content.push('\n');
    }
    content.into_bytes()
}

/// The antichain-maintaining update [`add`], [`pending_add`] and [`add_deferred`] share: absorb
/// `key` into `existing` (see the module doc comment's antichain-invariant section).
///
/// * Every entry `key` covers is dropped — it is now redundant, since `key` already implies it.
/// * If, after that, some *remaining* entry already covers `key`, `key` itself is not inserted —
///   recording it would be redundant the other way around (that broader entry already implies
///   `key`).
/// * Otherwise `key` is inserted.
fn absorb(existing: BTreeSet<String>, key: &str) -> BTreeSet<String> {
    let mut updated: BTreeSet<String> = existing.into_iter()
        .filter(|recorded| !path_utils::covers(key, recorded))
        .collect();

    if !updated.iter().any(|recorded| path_utils::covers(recorded, key)) {
        updated.insert(key.to_string());
    }

    updated
}

/// Compute the write [`add`] would need to durably perform to record `key`, without performing
/// it — for a caller that wants to fold that write into a batch it already controls (see the
/// module doc comment's durability section) instead of calling [`add`]/[`add_deferred`] directly.
///
/// Malformed existing content is treated as the empty set (the module doc comment's write-side
/// convention), so this never fails.
///
/// # Returns
/// * `Some(bytes)` - `key` is not yet covered by what is recorded; this is the new file content.
/// * `None`        - `key` is already covered by what is recorded; nothing needs to change.
pub fn pending_add(path: &Path, key: &str) -> Option<Vec<u8>> {
    let existing = read(path).unwrap_or_default();
    let updated = absorb(existing.clone(), key);

    if updated == existing { None } else { Some(serialize(&updated)) }
}

/// Add `key` to the set recorded at `path`, durably, maintaining the antichain invariant (see the
/// module doc comment). A no-op (no write at all) when `key` is already covered by what is
/// recorded.
///
/// # Returns
/// * `Ok(())`      - `key` is now covered by the recorded set (freshly added, or already was).
/// * `Err(String)` - The write failed.
pub fn add(path: &Path, key: &str) -> Result<(), String> {
    match pending_add(path, key) {
        None => Ok(()),
        Some(bytes) => write_bytes(path, &bytes),
    }
}

/// Like [`add`], but stages the write into `batch` instead of fsyncing it on its own — see the
/// module doc comment's durability section. A no-op (nothing staged) when `key` is already
/// covered by what is recorded. Ensures `path`'s parent folder exists first (a fresh warehouse's
/// very first write here may not have created it yet), exactly like [`add`].
///
/// The caller is responsible for calling `batch.finish()`; nothing staged here is durable until
/// that returns `Ok`.
pub fn add_deferred(batch: &file_utils::WriteBatch, path: &Path, key: &str) -> Result<(), String> {
    let Some(bytes) = pending_add(path, key) else { return Ok(()) };

    ensure_parent_folder(path)?;
    batch.stage(path, &bytes)
}

/// Remove every recorded entry covered by `by`, durably, best-effort (see the module doc
/// comment's durability section): a marker this cannot make sense of, or a failed write, is left
/// exactly as it was. Never fails the caller — there is nothing meaningful to return that a
/// caller could act on differently, since the safe fallback (the marker survives) is already
/// exactly what happens.
pub fn remove_covered_by(path: &Path, by: &str) {
    let Ok(existing) = read(path) else { return };

    let updated: BTreeSet<String> = existing.iter()
        .filter(|recorded| !path_utils::covers(by, recorded))
        .cloned()
        .collect();

    if updated.len() != existing.len() {
        let _ = write_bytes_or_clear(path, &updated);
    }
}

/// Remove every recorded entry that stands in a [`covers`](path_utils::covers) relationship with
/// `target` in *either* direction — the entry covers `target`, or `target` covers the entry —
/// durably, best-effort (same failure-direction contract as [`remove_covered_by`]).
///
/// # Returns
/// The entries actually removed (empty if none were, including on a read failure — same
/// best-effort contract as [`remove_covered_by`]) — for a caller that wants to report which
/// record(s) it healed. Computed against whatever was actually read, so it reflects reality even
/// if the eventual write then fails (that failure still leaves the marker exactly as it was, per
/// the module doc comment — this return value describes what *should* have changed, matching
/// [`remove_covered_by`]'s own best-effort framing).
pub fn remove_involving(path: &Path, target: &str) -> BTreeSet<String> {
    let Ok(existing) = read(path) else { return BTreeSet::new() };

    let (removed, kept): (BTreeSet<String>, BTreeSet<String>) = existing.into_iter()
        .partition(|recorded| path_utils::covers(recorded, target) || path_utils::covers(target, recorded));

    if !removed.is_empty() {
        let _ = write_bytes_or_clear(path, &kept);
    }

    removed
}

/// Unconditionally clear the marker, best-effort — see [`remove_covered_by`]'s doc comment on
/// why failures are swallowed here too.
pub fn clear(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Write `set`'s serialized bytes to `path`, or remove `path` entirely when `set` is empty — a
/// marker with nothing recorded is represented by a missing file, never an empty one (see the
/// module doc comment's format section), so [`read`] only ever has to distinguish "missing" from
/// "malformed", never a third "present but empty" case.
fn write_bytes_or_clear(path: &Path, set: &BTreeSet<String>) -> Result<(), String> {
    if set.is_empty() {
        clear(path);
        return Ok(());
    }

    write_bytes(path, &serialize(set))
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    ensure_parent_folder(path)?;
    file_utils::write_file_atomically(path, bytes)
}

fn ensure_parent_folder(path: &Path) -> Result<(), String> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() =>
            file_utils::create_folder_if_not_exists(parent).map(|_| ()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("forklift-marker-utils-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("marker")
    }

    #[test]
    fn a_missing_file_reads_as_the_empty_set() {
        let path = scratch("missing");
        assert_eq!(read(&path).unwrap(), BTreeSet::new());
    }

    #[test]
    fn add_records_a_key_and_read_sees_it() {
        let path = scratch("add-read");
        add(&path, "src").unwrap();
        assert_eq!(read(&path).unwrap(), BTreeSet::from(["src".to_string()]));
    }

    #[test]
    fn add_absorbs_narrower_keys_it_covers() {
        let path = scratch("absorb-narrower");
        add(&path, "src/api").unwrap();
        add(&path, "src").unwrap(); // covers "src/api" -> replaces it
        assert_eq!(read(&path).unwrap(), BTreeSet::from(["src".to_string()]));
    }

    #[test]
    fn add_skips_a_key_already_covered_by_a_broader_recorded_entry() {
        let path = scratch("skip-redundant");
        add(&path, "src").unwrap();
        add(&path, "src/api").unwrap(); // already covered by "src" -> no change
        assert_eq!(read(&path).unwrap(), BTreeSet::from(["src".to_string()]));
    }

    #[test]
    fn add_keeps_disjoint_keys_side_by_side() {
        let path = scratch("disjoint");
        add(&path, "src/a").unwrap();
        add(&path, "src/b").unwrap();
        assert_eq!(read(&path).unwrap(), BTreeSet::from(["src/a".to_string(), "src/b".to_string()]));
    }

    #[test]
    fn remove_covered_by_only_removes_what_is_covered() {
        let path = scratch("remove-covered");
        add(&path, "src/a").unwrap();
        add(&path, "src/b").unwrap();
        add(&path, "docs").unwrap();

        remove_covered_by(&path, "src"); // covers "src/a" and "src/b", not "docs"
        assert_eq!(read(&path).unwrap(), BTreeSet::from(["docs".to_string()]));
    }

    #[test]
    fn remove_covered_by_a_partial_overlap_leaves_the_entry_alone() {
        let path = scratch("partial-overlap");
        add(&path, "src").unwrap();

        remove_covered_by(&path, "src/sub"); // does not cover "src" (its own ancestor)
        assert_eq!(read(&path).unwrap(), BTreeSet::from(["src".to_string()]));
    }

    #[test]
    fn remove_covered_by_emptying_the_set_removes_the_file() {
        let path = scratch("remove-all");
        add(&path, "src").unwrap();

        remove_covered_by(&path, ""); // the empty key covers everything
        assert!(!path.exists());
        assert_eq!(read(&path).unwrap(), BTreeSet::new());
    }

    #[test]
    fn remove_involving_removes_a_broader_entry_that_covers_the_target() {
        let path = scratch("involving-broader");
        add(&path, "").unwrap(); // the whole warehouse

        let removed = remove_involving(&path, "bigdir"); // "" covers "bigdir"
        assert_eq!(removed, BTreeSet::from(["".to_string()]));
        assert_eq!(read(&path).unwrap(), BTreeSet::new());
    }

    #[test]
    fn remove_involving_removes_a_narrower_entry_the_target_covers() {
        let path = scratch("involving-narrower");
        add(&path, "src/api").unwrap();

        let removed = remove_involving(&path, "src"); // "src" covers "src/api"
        assert_eq!(removed, BTreeSet::from(["src/api".to_string()]));
        assert_eq!(read(&path).unwrap(), BTreeSet::new());
    }

    #[test]
    fn remove_involving_leaves_disjoint_entries_alone() {
        let path = scratch("involving-disjoint");
        add(&path, "docs").unwrap();

        let removed = remove_involving(&path, "src"); // neither covers the other
        assert!(removed.is_empty());
        assert_eq!(read(&path).unwrap(), BTreeSet::from(["docs".to_string()]));
    }

    #[test]
    fn clear_removes_the_file_unconditionally() {
        let path = scratch("clear");
        add(&path, "src").unwrap();
        add(&path, "docs").unwrap();

        clear(&path);
        assert!(!path.exists());
    }

    #[test]
    fn pending_add_reports_none_when_nothing_would_change() {
        let path = scratch("pending-none");
        add(&path, "src").unwrap();
        assert!(pending_add(&path, "src/api").is_none());
    }

    #[test]
    fn pending_add_reports_the_bytes_add_would_write() {
        let path = scratch("pending-some");
        let bytes = pending_add(&path, "src").expect("a fresh key must produce bytes to write");
        assert!(!path.exists(), "pending_add must not perform the write itself");

        // Writing those bytes by hand must produce exactly what `add` itself would have written.
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(read(&path).unwrap(), BTreeSet::from(["src".to_string()]));
    }

    #[test]
    fn add_deferred_stages_into_the_batch_without_writing_immediately() {
        let path = scratch("deferred");
        let batch = file_utils::WriteBatch::new();

        add_deferred(&batch, &path, "src").unwrap();
        assert!(!path.exists(), "a deferred add must not be visible before the batch finishes");

        batch.finish().unwrap();
        assert_eq!(read(&path).unwrap(), BTreeSet::from(["src".to_string()]));
    }

    #[test]
    fn add_deferred_stages_nothing_when_already_covered() {
        let path = scratch("deferred-noop");
        add(&path, "src").unwrap();

        let batch = file_utils::WriteBatch::new();
        add_deferred(&batch, &path, "src/api").unwrap();
        batch.finish().unwrap();

        // Unchanged: the deferred add for the already-covered key never staged anything.
        assert_eq!(read(&path).unwrap(), BTreeSet::from(["src".to_string()]));
    }

    #[test]
    fn read_reports_malformed_content_as_an_error() {
        let path = scratch("malformed");
        std::fs::write(&path, [0xFF, 0xFE, 0x00, 0xFF]).unwrap(); // never valid UTF-8
        assert!(read(&path).is_err());
    }

    #[test]
    fn add_self_heals_malformed_content() {
        let path = scratch("self-heal");
        std::fs::write(&path, [0xFF, 0xFE, 0x00, 0xFF]).unwrap();

        add(&path, "src").unwrap();
        assert_eq!(read(&path).unwrap(), BTreeSet::from(["src".to_string()]));
    }

    #[test]
    fn a_lone_blank_line_round_trips_as_the_root_key() {
        let path = scratch("root-key");
        add(&path, "").unwrap();
        assert_eq!(read(&path).unwrap(), BTreeSet::from(["".to_string()]));
    }
}
