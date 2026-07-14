//! `BulkStoreSession` behavior, exercised through the crate's public API.
//!
//! These live in their own integration-test binary rather than `file_utils`'s unit tests:
//! `BulkStoreSession` is backed by a genuine process-global registry (by design — see its doc
//! comment), and `cargo test` runs a crate's unit tests as many threads inside one process. A
//! session opened by one of these tests would otherwise be able to intercept an unrelated unit
//! test's unrelated `write_file_atomically` call running concurrently on another thread. Putting
//! them in a separate integration-test binary gives them their own process, so the only thing
//! that can race a session here is another test in *this* file — which `lock_session` below
//! serializes.

use std::sync::Mutex;
use forklift_core::util::file_utils::{write_file_atomically, BulkStoreSession};

/// Only this file's tests open a `BulkStoreSession` (exactly one may be active at a time), and
/// this binary still runs its tests on multiple threads — so they still need to take turns.
fn lock_session() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: Mutex<()> = Mutex::new(());
    GUARD.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn a_bulk_store_session_defers_publication_until_finish() {
    // The whole point of the session: while it is open, a staged write's final name must not
    // exist yet (only its bytes, under a temp name) — and once `finish` runs, the final name
    // exists with exactly the written content.
    let _guard = lock_session();
    let temp = std::env::temp_dir().join(format!("forklift-bulk-finish-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp).unwrap();

    let target = temp.join("staged-object");
    let session = BulkStoreSession::open().unwrap();
    write_file_atomically(&target, b"bulk content").unwrap();

    assert!(!target.exists(), "the final name must not exist while the session is still open");

    session.finish().unwrap();

    assert!(target.exists(), "finish must publish the staged write");
    assert_eq!(std::fs::read(&target).unwrap(), b"bulk content");

    std::fs::remove_dir_all(&temp).ok();
}

#[test]
fn dropping_a_bulk_store_session_without_finish_publishes_nothing() {
    // Abort semantics: a session dropped without `finish` must remove its staged temp files and
    // must never let the final name come into existence.
    let _guard = lock_session();
    let temp = std::env::temp_dir().join(format!("forklift-bulk-abort-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp).unwrap();

    let target = temp.join("never-published");
    {
        let _session = BulkStoreSession::open().unwrap();
        write_file_atomically(&target, b"should vanish").unwrap();
        assert!(!target.exists());
        // `_session` drops here without `finish` being called.
    }

    assert!(!target.exists(), "an aborted session must never publish its staged writes");

    // No stray temp file survives the abort either.
    let leftovers: Vec<_> = std::fs::read_dir(&temp).unwrap().filter_map(|e| e.ok()).collect();
    assert!(leftovers.is_empty(), "an aborted session must remove its staged temp files, found {:?}",
        leftovers.iter().map(|e| e.file_name()).collect::<Vec<_>>());

    // A fresh session can be opened immediately afterwards — the abort must fully release the
    // one-active-session slot.
    let session = BulkStoreSession::open().unwrap();
    session.finish().unwrap();

    std::fs::remove_dir_all(&temp).ok();
}

#[test]
fn writes_outside_any_session_are_unaffected_by_bulk_sessions() {
    // A write made with no session active must behave exactly as before (sync-then-rename,
    // immediately visible) — including right after a session has been used and finished, to
    // guard against the registry being left in a stuck "active" state.
    let _guard = lock_session();
    let temp = std::env::temp_dir().join(format!("forklift-bulk-outside-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp).unwrap();

    // Use and finish a session first.
    let inside_target = temp.join("inside-session");
    let session = BulkStoreSession::open().unwrap();
    write_file_atomically(&inside_target, b"staged").unwrap();
    session.finish().unwrap();

    // Now write with no session active: the final name must exist immediately, with no
    // registry-driven staging in play.
    let outside_target = temp.join("outside-session");
    write_file_atomically(&outside_target, b"plain write").unwrap();

    assert!(outside_target.exists(), "a write outside any session must publish immediately");
    assert_eq!(std::fs::read(&outside_target).unwrap(), b"plain write");

    let leftovers: Vec<_> = std::fs::read_dir(&temp).unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "no temporary file should survive a plain write");

    std::fs::remove_dir_all(&temp).ok();
}

#[test]
fn a_failed_finish_removes_every_staged_temp_and_publishes_nothing() {
    // Unlike the pack folder, the loose store has no stale-temp sweeper — so a barrier that
    // returns an error (disk full, a permission flip) must not strand any staged temp behind,
    // or it leaks forever. Sabotage one staged write (delete its temp out from under the
    // session) so its fsync fails, and check the *other*, otherwise-healthy staged write is
    // cleaned up too, and neither ever got published.
    let _guard = lock_session();
    let temp = std::env::temp_dir().join(format!("forklift-bulk-fail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp).unwrap();

    let target_a = temp.join("a");
    let target_b = temp.join("b");
    let session = BulkStoreSession::open().unwrap();
    write_file_atomically(&target_a, b"alpha").unwrap();
    write_file_atomically(&target_b, b"beta").unwrap();

    let staged_temps: Vec<_> = std::fs::read_dir(&temp).unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert_eq!(staged_temps.len(), 2, "expected exactly two staged temp files before sabotage");
    std::fs::remove_file(staged_temps[0].path()).unwrap();

    let result = session.finish();
    assert!(result.is_err(), "finish must surface the sabotaged fsync as an error");

    // Neither write was published...
    assert!(!target_a.exists() && !target_b.exists(), "a failed finish must publish nothing");
    // ...and no temp file — sabotaged or not — is left behind.
    let leftovers: Vec<_> = std::fs::read_dir(&temp).unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "a failed finish must remove every staged temp, found {:?}",
        leftovers.iter().map(|e| e.file_name()).collect::<Vec<_>>());

    // The one-active-session slot must be released even after a failed finish.
    let session = BulkStoreSession::open().unwrap();
    session.finish().unwrap();

    std::fs::remove_dir_all(&temp).ok();
}
