//! `WriteBatch` under `FORKLIFT_FSYNC=0`, exercised through the crate's public API.
//!
//! `file_utils::fsync_enabled` reads `FORKLIFT_FSYNC` once and caches the result in a
//! process-wide `OnceLock` (deliberately — see its doc comment: durability is a process-wide
//! policy). That means a test cannot toggle it at runtime and observe the change within a
//! process that already called `fsync_enabled` with the variable unset (as `cargo test`'s
//! default environment does — see `file_utils`'s own unit tests, several of which write through
//! `write_file_atomically` and so already latch the default, "on", value for their process).
//! This lives in its own integration-test binary — its own fresh process — specifically so it
//! can set `FORKLIFT_FSYNC=0` before anything in it has ever called `fsync_enabled`, and so
//! genuinely observe the disabled path rather than a already-latched default.

use forklift_core::util::file_utils::WriteBatch;

#[test]
fn staged_writes_stay_invisible_until_finish_even_with_fsync_disabled() {
    // The deferred-rename structure is what preserves the atomic-visibility invariant, not the
    // fsyncing — `fsync_enabled` only gates *whether* durability barriers run, never *whether*
    // `stage` defers the rename. With fsync fully off, a buggy implementation could plausibly
    // decide there is "nothing to wait for" and publish immediately instead of staging; this
    // pins that it does not.
    unsafe {
        std::env::set_var("FORKLIFT_FSYNC", "0");
    }

    let temp = std::env::temp_dir().join(format!("forklift-writebatch-fsync-off-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp).unwrap();

    let target = temp.join("staged-with-fsync-off");
    let batch = WriteBatch::new();
    batch.stage(&target, b"fsync-off content").unwrap();

    assert!(!target.exists(), "the final name must not exist while the batch is still open, \
        even with FORKLIFT_FSYNC=0 — the rename is deferred regardless of the fsync setting");

    // A staged-but-unpublished temp file must exist under the same directory (proof the write
    // actually happened, just not at its final name yet).
    let staged_temps: Vec<_> = std::fs::read_dir(&temp).unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert_eq!(staged_temps.len(), 1, "expected exactly one staged temp file before finish");

    batch.finish().unwrap();

    assert!(target.exists(), "finish must publish the staged write even with fsync disabled");
    assert_eq!(std::fs::read(&target).unwrap(), b"fsync-off content");

    let leftovers: Vec<_> = std::fs::read_dir(&temp).unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "no temporary file should survive a successful finish, found {:?}",
        leftovers.iter().map(|e| e.file_name()).collect::<Vec<_>>());

    std::fs::remove_dir_all(&temp).ok();
}

#[test]
fn a_dropped_batch_publishes_nothing_even_with_fsync_disabled() {
    unsafe {
        std::env::set_var("FORKLIFT_FSYNC", "0");
    }

    let temp = std::env::temp_dir().join(format!("forklift-writebatch-fsync-off-abort-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp).unwrap();

    let target = temp.join("never-published");
    {
        let batch = WriteBatch::new();
        batch.stage(&target, b"should vanish").unwrap();
        assert!(!target.exists());
        // `batch` drops here without `finish` being called.
    }

    assert!(!target.exists(), "a dropped batch must never publish, fsync setting notwithstanding");

    let leftovers: Vec<_> = std::fs::read_dir(&temp).unwrap().filter_map(|e| e.ok()).collect();
    assert!(leftovers.is_empty(), "a dropped batch must remove every staged temp, found {:?}",
        leftovers.iter().map(|e| e.file_name()).collect::<Vec<_>>());

    std::fs::remove_dir_all(&temp).ok();
}
