//! Isolated integration test for a PR #61 review finding (#6) on `refresh_tracked_entries`
//! (`park`'s working-directory refresh): a shard rewritten only because its stat data drifted
//! (no real content change) used to publish with the wall clock's value *when the whole refresh
//! finished deciding every tracked shard*, not the instant this particular shard was actually
//! verified — widening the "racily clean" stat-cache trust window `is_entry_unchanged` relies on
//! (a file edited in the gap between the true verification instant and the wrongly-late publish
//! time would be silently trusted as unchanged forever after).
//!
//! `park`'s own CLI surface cannot observe this: `park_changes` always resets the whole working
//! directory and inventory to the pallet head right after `refresh_tracked_entries` returns
//! (`inventory_utils::replace_all_inventories`, which unconditionally rewrites every shard with
//! "now" of its own), so anything `refresh_tracked_entries` itself stamped is immediately
//! overwritten a few lines later — measuring shard mtimes after a full `park` command measures
//! the reset step, not this one. This test instead calls `inventory_utils::refresh_tracked_entries`
//! directly.
//!
//! This is its own dedicated integration test binary (not folded into a shared `tests/*.rs`
//! file) specifically so it can safely call `std::env::set_current_dir`: `WarehousePath::to_fs_path`
//! (and therefore every real-file stat/read `refresh_tracked_entries` performs) resolves relative
//! to the *process* working directory, unlike object-store paths, which go through the
//! thread-local `StorageRootScope` — a cwd-dependent test must not risk racing a concurrently
//! running test elsewhere in the same process. Cargo compiles each `tests/*.rs` file to its own
//! test binary/process, and this file has exactly one `#[test]`, so there is nothing else in this
//! process for the cwd change to race against.

use std::time::{Duration, SystemTime};
use forklift_core::globals::StorageRootScope;
use forklift_core::util::{file_utils, inventory_utils, path_utils};

#[test]
fn refresh_tracked_entries_stamps_a_stat_only_rewrite_with_its_own_verification_time() {
    let root = std::env::temp_dir().join(format!(
        "forklift-refresh-verified-at-{}", std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join(".forklift")).unwrap();

    std::env::set_current_dir(&root).unwrap();
    let _scope = StorageRootScope::enter(&root);

    // Enough tracked directories that deciding all of them (each needs a real stat + rehash,
    // since every file below is touched identically before the refresh) takes measurably longer
    // than deciding just the first one — the gap this test asserts on is between "the instant
    // dir_0000 itself is decided" (a roughly constant, tiny cost, independent of how many *other*
    // shards follow it) and "the instant the whole decide pass finishes" (which grows with the
    // shard count) — a large count widens that gap without changing dir_0000's own cost, so a
    // bigger `DIRS` only makes the discrimination this test relies on more robust, never less.
    const DIRS: usize = 1000;
    for i in 0..DIRS {
        let dir = root.join(format!("dir_{:04}", i));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file.txt"), format!("v1 {i}\n")).unwrap();
    }

    // A first-time load establishes every directory's shard (via the real, proven walker —
    // not hand-rolled shard construction, so this test's setup exercises the same code path a
    // real `load .` would).
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let path = path_utils::WarehousePath::from_user_input(".").unwrap();
    runtime.block_on(inventory_utils::create_inventory_for_directory(&path))
        .expect("the initial load must succeed");

    // Rewrite every tracked file with byte-identical content: every one of the 400 shards needs
    // a stat-only rebuild (`changed=true, content_changed=false`) during the next
    // `refresh_tracked_entries` call — real, unavoidable per-shard work (a stat, a rehash) for
    // the whole set, not just the one shard this test inspects afterward.
    for i in 0..DIRS {
        let dir = root.join(format!("dir_{:04}", i));
        std::fs::write(dir.join("file.txt"), format!("v1 {i}\n")).unwrap();
    }

    let before = SystemTime::now();
    inventory_utils::refresh_tracked_entries().expect("the refresh must succeed");
    let elapsed = before.elapsed().unwrap_or(Duration::ZERO);

    // Sanity: the scenario must actually take measurable wall-clock time, or the test proves
    // nothing either way (a near-instant run can't discriminate "decided early" from "decided
    // late" via mtimes at all — both would round to the same instant).
    assert!(elapsed.as_millis() >= 5,
        "the 400-shard refresh must take measurable time for this test to discriminate, took {elapsed:?}");

    // "dir_0000" sorts first among the "dir_NNNN" metadata entries (the root shard "./" sorts
    // before all of them, but it has no file entries of its own — no tracked directory lists its
    // subdirectories inside its own shard — so it decides nothing and costs no real time).
    let shard_path = file_utils::get_inventory_data_path_for_key("dir_0000");
    let published_mtime = std::fs::metadata(&shard_path).unwrap().modified().unwrap();

    // The published mtime of the *first*-decided shard must fall well within the *early* part of
    // the whole call's wall-clock window — not close to when the call returned. A generous (not
    // exact-midpoint) threshold: comfortably discriminates "stamped at decision time" from
    // "stamped at publish time" without being sensitive to scheduling noise.
    // The published mtime of the *first*-decided shard must fall very close to when the call
    // *started*, not somewhere later in its run. This is an absolute bound rather than a fraction
    // of `elapsed`: `elapsed` itself is not a fair yardstick here, because the very bug this test
    // targets (publishing with "now" at the end of the decide pass rather than each shard's own
    // decision time) and the *publish*-side batching fix from the same PR (one barrier instead of
    // `DIRS` of them) both change `elapsed` in ways that swamp a purely relative comparison — a
    // relative threshold generous enough to tolerate the batching win ends up too generous to
    // catch this bug too (verified empirically: an unfixed run showed the first shard published
    // ~16ms after the call started against a ~3.2s total dominated by `DIRS` individual fsyncs —
    // only ~0.5% of `elapsed`, the same order of magnitude a *correct* run's tiny fraction is).
    // The absolute bound instead targets what actually distinguishes the two: "verified_at,
    // captured at this one shard's own decision" is a small, ~constant cost independent of `DIRS`;
    // "now, captured once the whole decide pass over all `DIRS` shards has finished" is not.
    let since_start = published_mtime.duration_since(before).unwrap_or(Duration::ZERO);
    let absolute_bound = Duration::from_millis(10);

    assert!(since_start < absolute_bound,
        "dir_0000's shard must be stamped near when *it* was verified (a single shard's own \
         decision, expected well under {absolute_bound:?}), not near when the whole {DIRS}-shard \
         refresh finished deciding every shard ({elapsed:?} total): stamped {since_start:?} after \
         the call started");

    std::fs::remove_dir_all(&root).ok();
}
