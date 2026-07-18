//! The crash / interrupted-write harness, part of the hardening test spine.
//!
//! "Durable before destructive" holds across power loss: every object, ref, inventory
//! shard and graph file is written to a temp file, fsynced, renamed, and the directory fsynced,
//! and a pallet's ref advances only *after* all the objects it names are durable. The claim is
//! that a crash at any instant leaves the store either at its old state or fully at the new one —
//! never a torn object at a real address, never a half-written ref.
//!
//! A unit test can assert the atomic-write contract (see `file_utils`), but only a real,
//! externally killed process exercises the whole `stack` pipeline under interruption. This test
//! SIGKILLs `stack` at a spread of delays that straddle the object-write/ref-update window, and
//! after each kill asserts the store is still internally consistent and usable. The assertions
//! hold at *every* kill point, so the test cannot flake — whether a given kill lands inside the
//! interesting window only affects coverage, never pass/fail. A crash that genuinely corrupted
//! the store (a torn object, a partial ref) is the only thing that fails it.
//!
//! The kill delays themselves *are* calibrated, though: a fixed millisecond spread that straddles
//! the write window on a fast dev laptop can land entirely before the first `stack` ever finishes
//! on a slow/cold CI runner, in which case no kill ever exercises the durable-ref-advance path and
//! the sanity guard below (rightly) refuses to pass. So before spawning any kills, this test times
//! a few uninterrupted `stack` runs on the same corpus in the same warehouse and derives the delay
//! spread from that measurement — proportional to how slow *this* machine actually is. If the
//! guard still trips (measurement noise, a GC pause, whatever), it retries once with a
//! re-measured, wider spread before failing for real.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const FORKLIFT: &str = env!("CARGO_BIN_EXE_forklift");

/// A scratch area: the warehouse, plus an isolated home for the global config and keys so the
/// test never touches the developer's real ones. Deleted when the test ends.
struct Area {
    root: PathBuf,
}

impl Area {
    fn new(name: &str) -> Area {
        let root = std::env::temp_dir().join(format!("forklift-crash-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("warehouse")).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        Area { root }
    }

    fn warehouse(&self) -> PathBuf {
        self.root.join("warehouse")
    }

    /// A command in the warehouse with the isolated global config and key directory.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(FORKLIFT);
        command
            .args(args)
            .current_dir(self.warehouse())
            .env("FORKLIFT_GLOBAL_CONFIG", self.root.join("home").join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.root.join("home").join("keys"));
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// A crashed `stack` leaves the warehouse lock behind (SIGKILL runs no destructor), exactly as
    /// a real power loss would; the operator clears it. Do the same before the next command so the
    /// lock is never the reason a later step fails — we are testing store integrity, not the lock.
    fn clear_stale_lock(&self) {
        let _ = std::fs::remove_file(self.warehouse().join(".forklift").join("lock"));
    }

    /// Assert the store is internally consistent right now: any pallet head is a whole 64-hex hash
    /// (an atomic ref write never leaves a partial one), and the commands that read the committed
    /// tree and history succeed (a torn object would fail the read-side hash check).
    fn assert_consistent(&self, context: &str) {
        let head_path = self.warehouse().join(".forklift").join("pallets").join("main");
        if let Ok(head) = std::fs::read_to_string(&head_path) {
            let head = head.trim();
            assert!(
                head.len() == 64 && head.bytes().all(|b| b.is_ascii_hexdigit()),
                "{context}: the pallet head must be a whole hash, found {head:?}",
            );

            let history = self.run(&["history"]);
            assert!(history.status.success(),
                "{context}: history must read the parcel chain, stderr: {}",
                String::from_utf8_lossy(&history.stderr));

            let peek = self.run(&["peek", head]);
            assert!(peek.status.success(),
                "{context}: peek of the head parcel must succeed, stderr: {}",
                String::from_utf8_lossy(&peek.stderr));
        }

        let stocktake = self.run(&["stocktake"]);
        assert!(stocktake.status.success(),
            "{context}: stocktake must read the head tree, stderr: {}",
            String::from_utf8_lossy(&stocktake.stderr));
    }
}

impl Drop for Area {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Overwrite the corpus file with a fresh, same-order-of-magnitude payload so each `stack` call
/// (calibration or real) has real hashing/compression/fsync work to do.
fn rewrite_corpus(file: &Path, base_line: &str, tag: &str) {
    std::fs::write(file, format!("{}{}\n", base_line.repeat(90_000), tag)).unwrap();
}

/// The current pallet head, if one exists yet.
fn current_head(warehouse: &Path) -> Option<String> {
    std::fs::read_to_string(warehouse.join(".forklift").join("pallets").join("main"))
        .ok()
        .map(|h| h.trim().to_string())
}

/// Time a few uninterrupted `stack` runs on the same corpus, in the same warehouse the kill loop
/// will use, and return the slowest of them. Using the max (not the mean/median) biases the
/// resulting delay spread wide rather than narrow: undershooting the true duration is what causes
/// every kill to land before the write ever starts, which is exactly the flake this is guarding
/// against. Each call fully completes and folds into the real history — that's fine, it's the same
/// warehouse the kill loop continues from, just with a known head established before it starts.
fn calibrate_stack_duration(area: &Area, file: &Path, base_line: &str, label: &str, samples: usize) -> Duration {
    // The caller may be re-entering right after a kill spread whose last kill landed mid-write and
    // left `.forklift/lock` behind (SIGKILL runs no destructor) — `run_kill_spread` only clears the
    // stale lock at the *start* of its own next iteration, so a caller that jumps straight from a
    // spread into recalibration (the attempt-1-landed-nothing retry) would otherwise hit a locked
    // warehouse on the very first `load`/`stack` below. Clear it here too, same as `run_kill_spread`
    // does, so calibration never fails on a lock left behind by the run it's re-measuring after.
    area.clear_stale_lock();

    let mut slowest = Duration::ZERO;
    for i in 0..samples {
        rewrite_corpus(file, base_line, &format!("{label} {i}"));
        let load = area.run(&["load", "."]);
        assert!(load.status.success(), "calibration load failed: {}", String::from_utf8_lossy(&load.stderr));

        let start = Instant::now();
        let stack = area.run(&["stack", &format!("{label} {i}")]);
        let elapsed = start.elapsed();
        assert!(stack.status.success(),
            "calibration stack failed: {}", String::from_utf8_lossy(&stack.stderr));

        slowest = slowest.max(elapsed);
    }
    slowest
}

/// Derive `count` kill delays spread across at least `[low_frac, high_frac]` of one measured,
/// uninterrupted `stack` duration, so the spread scales with how slow *this* machine actually is
/// instead of assuming a fixed millisecond budget. The fractions set a *floor* for the spread, not
/// an exact bound: consecutive delays are never closer than `MIN_STEP_MS` apart (Windows' ~15ms
/// timer-tick granularity quantizes finer sleeps away, which would collapse several
/// nominally-distinct delays onto the same wall-clock kill point), and on a fast measurement that
/// floor dominates the requested step, stretching the top of the spread well past `high_frac`. That
/// overshoot is desirable, not a bug to tighten: a fast machine gets extra post-completion coverage
/// at negligible cost, and the guard needs some kills to land after completion regardless of how
/// small the measured duration was.
fn kill_delay_spread(measured: Duration, count: usize, low_frac: f64, high_frac: f64) -> Vec<Duration> {
    const MIN_STEP_MS: u64 = 15;
    assert!(count >= 2);

    let measured_ms = (measured.as_millis() as u64).max(1);
    let low = ((measured_ms as f64) * low_frac).round().max(1.0) as u64;
    let high = ((measured_ms as f64) * high_frac).round().max(low as f64 + 1.0) as u64;
    let step = ((high - low) / (count as u64 - 1)).max(MIN_STEP_MS);

    (0..count as u64).map(|i| Duration::from_millis(low + step * i)).collect()
}

/// The calibration math in isolation, without spawning any processes: a spread derived from a
/// slow measurement must actually reach further out than one derived from a fast measurement
/// (the whole point — no more hard-coded 80ms ceiling that a slow runner can't clear), a spread
/// must never step by less than Windows' timer granularity, and widening the fractions (the
/// retry) must reach further than the original spread for the same measurement.
#[test]
fn kill_delay_spread_scales_with_measured_duration() {
    let fast = kill_delay_spread(Duration::from_micros(500), 24, 0.02, 1.30);
    assert_eq!(fast.first(), Some(&Duration::from_millis(1)));
    assert!(fast.windows(2).all(|w| w[1] - w[0] >= Duration::from_millis(15)),
        "delays must never step by less than one Windows timer tick: {fast:?}");

    // A slow, 800ms measurement (a loaded/cold CI runner) must spread proportionally further out —
    // not stay capped at whatever a fast dev laptop's measurement would have produced.
    let slow = kill_delay_spread(Duration::from_millis(800), 24, 0.02, 1.30);
    assert!(slow.last().unwrap() > &Duration::from_millis(900),
        "a slow measurement must produce a proportionally wide spread: {slow:?}");
    assert!(slow.windows(2).all(|w| w[1] > w[0]), "delays must be strictly increasing: {slow:?}");
    assert!(slow.last() > fast.last(), "a slower measurement must reach further than a fast one");

    // The bounded retry (wider fractions) must reach further still for the same measurement.
    let retry = kill_delay_spread(Duration::from_millis(800), 24, 0.0, 2.5);
    assert!(retry.last() > slow.last(), "the retry spread must widen beyond the first attempt");
}

/// Run one spread of kills against `area`, asserting consistency after every one. Returns how many
/// of them landed *after* a stack's ref update had already completed (a distinct new head appeared
/// between two consecutive checks) — the signal that the durable path, not just the
/// killed-before-anything path, was actually exercised. `prior_head` carries the last observed head
/// across calls (including across calibration bursts) so that head established before this spread
/// ran is never mistaken for one this spread produced.
fn run_kill_spread(
    area: &Area,
    file: &Path,
    base_line: &str,
    warehouse: &Path,
    delays: &[Duration],
    commit_tag: &str,
    prior_head: &mut Option<String>,
) -> usize {
    let mut advanced = 0usize;

    for (i, delay) in delays.iter().enumerate() {
        // 1. Recover from the previous kill and check it left the store consistent.
        area.clear_stale_lock();
        area.assert_consistent(&format!("after {commit_tag} kill #{i}"));

        // A head that advanced must be a *new* parcel, never a rewritten/rolled-back one.
        let head_now = current_head(warehouse);
        if let (Some(now), Some(prev)) = (&head_now, prior_head.as_ref()) {
            if now != prev {
                advanced += 1;
            }
        } else if head_now.is_some() && prior_head.is_none() {
            advanced += 1;
        }
        *prior_head = head_now;

        // 2. Make a fresh change and stage it.
        rewrite_corpus(file, base_line, &format!("{commit_tag} {i}"));
        let load = area.run(&["load", "."]);
        assert!(load.status.success(), "load failed: {}", String::from_utf8_lossy(&load.stderr));

        // 3. Spawn the stack and SIGKILL it mid-flight.
        let mut child = area.command(&["stack", &format!("{commit_tag} commit {i}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        std::thread::sleep(*delay);

        let _ = child.kill(); // a no-op if it already finished
        let _ = child.wait();
    }

    advanced
}

#[test]
fn killing_stack_midway_never_corrupts_the_store() {
    let area = Area::new("stack");
    let warehouse = area.warehouse();
    let file = warehouse.join("big.dat");

    // A few megabytes so hashing, compression, the fsync and the rename take long enough that some
    // of the kills below land inside the write window rather than always before or after it.
    let base_line = "the quick brown fox jumps over the lazy dog\n";
    std::fs::write(&file, base_line.repeat(90_000)).unwrap();

    assert!(area.run(&["prepare"]).status.success());
    assert!(area.run(&["config", "operator.name", "crash@forklift"]).status.success());
    assert!(area.run(&["config", "operator.identifier", "crash@forklift"]).status.success());

    const DELAY_COUNT: usize = 24;

    // Attempt 1: measure how long an uninterrupted `stack` actually takes on this machine, then
    // spread the kills across ~2%..130% of that — wide enough to straddle the write window whether
    // it's microseconds (a fast laptop) or hundreds of milliseconds (a cold, loaded CI runner).
    let measured_1 = calibrate_stack_duration(&area, &file, base_line, "calibration-1", 3);
    let delays_1 = kill_delay_spread(measured_1, DELAY_COUNT, 0.02, 1.30);
    let mut prior_head = current_head(&warehouse);

    let mut advanced = run_kill_spread(&area, &file, base_line, &warehouse, &delays_1, "a", &mut prior_head);

    // The guard below exists so this test can't silently pass by always killing before any real
    // work started. If the first spread never landed a completed stack, that's most likely
    // measurement noise (a cold cache on the very first calibration run, a scheduler hiccup) rather
    // than a structural problem — so re-measure and try a wider spread once, bounded, before
    // failing for real.
    let mut measured_2 = None;
    let mut delays_2 = None;
    if advanced == 0 {
        let measured = calibrate_stack_duration(&area, &file, base_line, "calibration-2", 3);
        let delays = kill_delay_spread(measured, DELAY_COUNT, 0.0, 2.5);
        // Recalibration itself completes real, uninterrupted stack calls — re-anchor the baseline
        // so the retry's own writes are never mistaken for a kill's ref update, same as attempt 1.
        prior_head = current_head(&warehouse);
        advanced += run_kill_spread(&area, &file, base_line, &warehouse, &delays, "b", &mut prior_head);
        measured_2 = Some(measured);
        delays_2 = Some(delays);
    }

    // 4. Final recovery: the store must still accept a clean write, and every object reachable from
    //    the final head must read back (export-git walks the whole graph — parcels, trees, blobs —
    //    so a torn object anywhere would fail here via the read-side hash check).
    area.clear_stale_lock();
    area.assert_consistent("final");

    assert!(area.run(&["load", "."]).status.success());
    let recover = area.run(&["stack", "recover"]);
    let recovered_ok = recover.status.success()
        || String::from_utf8_lossy(&recover.stderr).contains("Nothing to stack");
    assert!(recovered_ok, "the store must accept a write after the crashes, stderr: {}",
        String::from_utf8_lossy(&recover.stderr));

    area.assert_consistent("after recovery stack");

    let export_dir = area.root.join("git-export");
    let export = area.run(&["export-git", export_dir.to_str().unwrap()]);
    assert!(export.status.success(),
        "export-git must read every committed object without a torn read, stderr: {}",
        String::from_utf8_lossy(&export.stderr));

    // Sanity: across the run at least one kill fell after a completed ref update, so the durable
    // path (not just the "killed before anything" path) was actually exercised.
    assert!(advanced >= 1,
        "no stack ever completed across {} attempt(s) — the write window was never exercised. \
         attempt 1: measured {measured_1:?} uninterrupted, tried delays {delays_1:?}.{}",
        if measured_2.is_some() { 2 } else { 1 },
        match (measured_2, delays_2) {
            (Some(m), Some(d)) => format!(" attempt 2: measured {m:?} uninterrupted, tried delays {d:?}."),
            _ => String::new(),
        });
}

/// Time a few uninterrupted `load .` runs over the given multi-directory corpus and return the
/// slowest — the same reasoning as [`calibrate_stack_duration`], aimed at `load`'s own parallel
/// per-directory walk instead of `stack`'s tree build. Rewriting every file with a fresh tag each
/// sample keeps real hashing/compression/staging work on the table for every run, exactly as
/// `calibrate_stack_duration` does for its single corpus file.
fn calibrate_load_duration(area: &Area, files: &[PathBuf], base_line: &str, label: &str, samples: usize) -> Duration {
    area.clear_stale_lock();

    let mut slowest = Duration::ZERO;
    for i in 0..samples {
        for file in files {
            rewrite_corpus(file, base_line, &format!("{label} {i}"));
        }

        let start = Instant::now();
        let load = area.run(&["load", "."]);
        let elapsed = start.elapsed();
        assert!(load.status.success(), "calibration load failed: {}", String::from_utf8_lossy(&load.stderr));

        slowest = slowest.max(elapsed);
    }
    slowest
}

/// Run one spread of kills against `load .`, asserting store consistency after every one — the
/// `load` counterpart of [`run_kill_spread`]. `load` has no ref of its own to compare a "did it
/// advance" signal against (`stack` does — the pallet head), so instead every iteration tries to
/// build directly on whatever the (possibly interrupted) load left behind: `stack` it, with no
/// healing re-load first. This is deliberate, not an oversight — a healing re-load could mask
/// exactly the bug this test exists to catch, since the stat-cache fast path
/// (`is_entry_unchanged`) trusts an already-published shard's mtime without re-verifying its
/// entries, so a second `load .` would not necessarily re-touch (or heal) a shard a first,
/// interrupted `load .` already made durable and visible. `stack` itself never verifies a blob's
/// existence (it only carries hashes forward into tree entries), so the sharp check is deferred
/// to a final `export-git` after the whole spread (DESIGN.html §5.0 D item 10, finding #1) —
/// see the caller.
///
/// Returns how many iterations produced a completed `stack` (the signal that this iteration's
/// kill left something a later step could — and did — build on, not just "killed before
/// anything").
///
/// The incomplete-load guard (`load_guard_utils`) durably marks a load's root the instant it
/// starts — before this test's kill ever lands — precisely so a killed `load` is caught even
/// though it never got to report an error; a real `stack` now refuses on exactly the state this
/// function deliberately builds on. That marker is a different concern from the one under test
/// here (inventory completeness, not object/blob durability), so it is removed by hand — not
/// healed by a re-load, which the comment above already rules out for a different reason — right
/// before each iteration's direct `stack`, to keep exercising the same torn/missing-blob check
/// this test always has.
fn run_load_kill_spread(
    area: &Area,
    files: &[PathBuf],
    base_line: &str,
    delays: &[Duration],
    commit_tag: &str,
) -> usize {
    let mut stacked = 0usize;

    for (i, delay) in delays.iter().enumerate() {
        // 1. Recover from the previous iteration's kill and check it left the store consistent.
        area.clear_stale_lock();
        area.assert_consistent(&format!("after {commit_tag} kill #{i}"));

        // 2. A fresh change to every file in the corpus, so the walk below has real work to do
        //    across every directory, not just the ones already touched by an earlier iteration.
        for file in files {
            rewrite_corpus(file, base_line, &format!("{commit_tag} {i}"));
        }

        // 3. Spawn the load and SIGKILL it mid-flight.
        let mut child = area.command(&["load", "."])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        std::thread::sleep(*delay);

        let _ = child.kill(); // a no-op if it already finished
        let _ = child.wait();

        // 4. Build directly on whatever the (possibly interrupted) load left behind — no healing
        //    re-load first (see the doc comment above for why). The incomplete-load marker a
        //    killed load leaves behind is a different, orthogonal concern (see the function's own
        //    doc comment) — removed by hand so `stack` still reaches the torn/missing-blob check
        //    this test exists for, instead of refusing on the marker before ever getting there.
        area.clear_stale_lock();
        let _ = std::fs::remove_file(
            area.warehouse().join(".forklift").join("inventory").join("incomplete-load")
        );

        let stack = area.run(&["stack", &format!("{commit_tag} commit {i}")]);
        let stderr_lower = String::from_utf8_lossy(&stack.stderr).to_lowercase();
        let nothing_to_stack = stderr_lower.contains("nothing to stack");
        assert!(stack.status.success() || nothing_to_stack,
            "{commit_tag} kill #{i}: stack must either succeed or report nothing staged, stderr: {}",
            String::from_utf8_lossy(&stack.stderr));

        if stack.status.success() {
            stacked += 1;
        }

        // 5. Bring the working directory fully up to date before the next iteration's rewrite —
        //    an uninterrupted `load .`, so leftover partial state from this iteration's kill
        //    never carries over as ambiguity into the next one.
        let heal = area.run(&["load", "."]);
        assert!(heal.status.success(), "{commit_tag} kill #{i}: healing load failed: {}",
            String::from_utf8_lossy(&heal.stderr));
    }

    stacked
}

/// Extends the crash-consistency spine to `load`'s parallel per-directory walk (DESIGN.html §5.0
/// D item 10, finding #1): every changed file's blob is now staged into the walk's own shared
/// batch (`InventoryBuilderContext::batch`) instead of paying its own atomic-write barrier, and
/// that same batch is what the walk's single-threaded join point later publishes shard content
/// through — so a blob and the shard that references it can land in the very same durability
/// barrier. This test exists to prove that sharing never lets a shard become durable and visible
/// while a blob it references is still just an unpublished temp file: SIGKILL `load` at a spread
/// of delays straddling its write window, and after every kill, stack and (at the end) export
/// whatever survived — a torn or missing blob referenced by any committed tree fails the
/// read-side hash check `export-git` performs on every object it walks.
#[test]
fn killing_load_midway_never_leaves_a_shard_referencing_a_missing_or_torn_blob() {
    let area = Area::new("load");
    let warehouse = area.warehouse();

    // Several directories with several files each: `load`'s walk (DESIGN.html §5.0 D item 10,
    // finding #1) runs one concurrent task per directory, each staging its own changed files'
    // blobs into the same shared batch — this corpus shape is what actually exercises that
    // concurrency, unlike `stack`'s crash test above (a single flat file is enough there, since
    // `stack`'s object write concurrency comes from its tree build, not this walk).
    let base_line = "the quick brown fox jumps over the lazy dog\n";
    let mut files: Vec<PathBuf> = Vec::new();

    for dir in 0..6 {
        let dir_path = warehouse.join(format!("dir{dir}"));
        std::fs::create_dir_all(&dir_path).unwrap();

        for f in 0..4 {
            let file = dir_path.join(format!("file{f}.dat"));
            std::fs::write(&file, base_line.repeat(20_000)).unwrap();
            files.push(file);
        }
    }

    assert!(area.run(&["prepare"]).status.success());
    assert!(area.run(&["config", "operator.name", "crash@forklift"]).status.success());
    assert!(area.run(&["config", "operator.identifier", "crash@forklift"]).status.success());

    const DELAY_COUNT: usize = 20;

    // Attempt 1: measure how long an uninterrupted `load .` actually takes on this machine, then
    // spread the kills across ~2%..130% of that — see `calibrate_stack_duration`'s doc comment
    // for why this scales with the machine instead of assuming a fixed millisecond budget.
    let measured_1 = calibrate_load_duration(&area, &files, base_line, "calibration-1", 3);
    let delays_1 = kill_delay_spread(measured_1, DELAY_COUNT, 0.02, 1.30);

    let mut stacked = run_load_kill_spread(&area, &files, base_line, &delays_1, "a");

    // Same bounded-retry guard as the `stack` crash test: if the first spread never landed a
    // completed stack, re-measure and try a wider spread once before failing for real.
    let mut measured_2 = None;
    let mut delays_2 = None;
    if stacked == 0 {
        let measured = calibrate_load_duration(&area, &files, base_line, "calibration-2", 3);
        let delays = kill_delay_spread(measured, DELAY_COUNT, 0.0, 2.5);
        stacked += run_load_kill_spread(&area, &files, base_line, &delays, "b");
        measured_2 = Some(measured);
        delays_2 = Some(delays);
    }

    // Final recovery and the sharp check: every object reachable from every pallet's history —
    // including every blob any kill's `load` ever staged and published — must read back. A shard
    // durably referencing a missing or torn blob fails here via `export-git`'s read-side hash
    // check, exactly as it would for `stack`'s crash test above.
    area.clear_stale_lock();
    area.assert_consistent("final");

    let export_dir = area.root.join("git-export");
    let export = area.run(&["export-git", export_dir.to_str().unwrap()]);
    assert!(export.status.success(),
        "export-git must read every committed object without a torn or missing blob, stderr: {}",
        String::from_utf8_lossy(&export.stderr));

    // Sanity: across the run at least one kill left something a later `stack` actually built on,
    // so the shared blob/shard batch's write window (not just the "killed before anything" path)
    // was actually exercised.
    assert!(stacked >= 1,
        "no load-then-stack ever completed across {} attempt(s) — the write window was never \
         exercised. attempt 1: measured {measured_1:?} uninterrupted, tried delays {delays_1:?}.{}",
        if measured_2.is_some() { 2 } else { 1 },
        match (measured_2, delays_2) {
            (Some(m), Some(d)) => format!(" attempt 2: measured {m:?} uninterrupted, tried delays {d:?}."),
            _ => String::new(),
        });
}

/// Time a few uninterrupted `park` pushes over the given multi-directory corpus and return the
/// slowest — the `park` counterpart of [`calibrate_load_duration`]. Each sample parks, then
/// immediately pops (a `park` needs a clean, unparked tracked state to push a *new* parcel), so
/// every sample pays the same real work an uninterrupted `park` does.
fn calibrate_park_duration(area: &Area, files: &[PathBuf], base_line: &str, label: &str, samples: usize) -> Duration {
    area.clear_stale_lock();

    let mut slowest = Duration::ZERO;
    for i in 0..samples {
        for file in files {
            rewrite_corpus(file, base_line, &format!("{label} {i}"));
        }

        let start = Instant::now();
        let park = area.run(&["park"]);
        let elapsed = start.elapsed();
        assert!(park.status.success(), "calibration park failed: {}", String::from_utf8_lossy(&park.stderr));

        let pop = area.run(&["park", "pop"]);
        assert!(pop.status.success(), "calibration park pop failed: {}", String::from_utf8_lossy(&pop.stderr));

        slowest = slowest.max(elapsed);
    }
    slowest
}

/// The number of parked parcels currently recorded, read directly via `forklift-core` (not the
/// CLI) so a torn or malformed `.forklift/parked` file panics this test loudly instead of being
/// silently swallowed by output parsing.
fn parked_count(warehouse: &Path) -> usize {
    let _scope = forklift_core::globals::StorageRootScope::enter(warehouse);
    forklift_core::util::park_utils::read_parked().unwrap().len()
}

/// Run one spread of kills against `park`, asserting store consistency after every one — the
/// `park` counterpart of [`run_load_kill_spread`]. Whether a given iteration's `park` actually
/// committed (staged the tree/parcel object batch, wrote the signature, and appended to the
/// parked list) is read directly off `.forklift/parked`'s length rather than inferred the way the
/// `load` test infers it via a later `stack` — `park` has its own ref-adjacent record to check
/// directly, so no proxy is needed.
///
/// When an iteration's `park` did commit, this pops it right back — a real read of every tree and
/// blob the parcel references (`shift_utils::diff_trees`/`apply_file_op` walk the tree and
/// materialize every file's content), the sharp check that a torn or missing object referenced by
/// the newly committed parcel fails loudly on (DESIGN.html §5.0 D item 10, finding #3). When it
/// did not commit, `park`'s reset-to-head step (the only thing that touches the working
/// directory's tracked file *content*) never ran — it only ever runs after the parked-list write
/// already succeeded — so the working directory still holds this iteration's rewritten content
/// untouched, and the next iteration's rewrite is safe to proceed with no extra healing step.
///
/// Returns how many iterations produced a durably committed park.
fn run_park_kill_spread(
    area: &Area,
    files: &[PathBuf],
    base_line: &str,
    warehouse: &Path,
    delays: &[Duration],
    tag: &str,
) -> usize {
    let mut parked = 0usize;

    for (i, delay) in delays.iter().enumerate() {
        area.clear_stale_lock();
        area.assert_consistent(&format!("after park {tag} kill #{i}"));

        for file in files {
            rewrite_corpus(file, base_line, &format!("park-{tag} {i}"));
        }

        let before = parked_count(warehouse);

        let mut child = area.command(&["park"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        std::thread::sleep(*delay);

        let _ = child.kill(); // a no-op if it already finished
        let _ = child.wait();

        area.clear_stale_lock();

        let after = parked_count(warehouse);

        if after > before {
            parked += 1;

            let pop = area.run(&["park", "pop"]);
            assert!(pop.status.success(),
                "park {tag} kill #{i}: pop of a park that committed durably must read back every \
                 object it referenced, stderr: {}", String::from_utf8_lossy(&pop.stderr));
        }
    }

    parked
}

/// Extends the crash-consistency spine to `park` push's own object batch (DESIGN.html §5.0 D
/// item 10, finding #3): every tree object and the parcel object are now staged into one shared
/// `WriteBatch`, finished once before the signature sidecar and the parked-list record — the same
/// pattern `stack`'s crash test above already covers for `stack`, applied here to `park`'s own
/// distinct code path (and `refresh_tracked_entries`'s own separate blob batch, finished before
/// any shard it rewrites becomes visible). SIGKILL `park` at a spread of delays straddling its
/// write window, and after every kill either see no new parked entry (nothing to check) or pop
/// the one that did commit — reading back every tree and blob it references, which fails loudly
/// on a torn or missing object.
#[test]
fn killing_park_midway_never_leaves_a_parked_parcel_referencing_a_missing_or_torn_object() {
    let area = Area::new("park");
    let warehouse = area.warehouse();

    // Several directories with several files each — the same shape `load`'s crash test above
    // uses, so `park`'s own `refresh_tracked_entries` walk has real, spread-out work to rehash.
    let base_line = "the quick brown fox jumps over the lazy dog\n";
    let mut files: Vec<PathBuf> = Vec::new();

    for dir in 0..6 {
        let dir_path = warehouse.join(format!("dir{dir}"));
        std::fs::create_dir_all(&dir_path).unwrap();

        for f in 0..4 {
            let file = dir_path.join(format!("file{f}.dat"));
            std::fs::write(&file, base_line.repeat(20_000)).unwrap();
            files.push(file);
        }
    }

    assert!(area.run(&["prepare"]).status.success());
    assert!(area.run(&["config", "operator.name", "crash@forklift"]).status.success());
    assert!(area.run(&["config", "operator.identifier", "crash@forklift"]).status.success());

    // A pallet head to park onto — `park` refuses when the pallet has nothing stacked yet.
    assert!(area.run(&["load", "."]).status.success());
    assert!(area.run(&["stack", "base"]).status.success());

    const DELAY_COUNT: usize = 20;

    // Attempt 1: measure how long an uninterrupted `park` actually takes on this machine, then
    // spread the kills across ~2%..130% of that — see `calibrate_stack_duration`'s doc comment
    // for why this scales with the machine instead of assuming a fixed millisecond budget.
    let measured_1 = calibrate_park_duration(&area, &files, base_line, "calibration-1", 3);
    let delays_1 = kill_delay_spread(measured_1, DELAY_COUNT, 0.02, 1.30);

    let mut parked = run_park_kill_spread(&area, &files, base_line, &warehouse, &delays_1, "a");

    // Same bounded-retry guard as the `stack`/`load` crash tests: if the first spread never
    // landed a completed park, re-measure and try a wider spread once before failing for real.
    let mut measured_2 = None;
    let mut delays_2 = None;
    if parked == 0 {
        let measured = calibrate_park_duration(&area, &files, base_line, "calibration-2", 3);
        let delays = kill_delay_spread(measured, DELAY_COUNT, 0.0, 2.5);
        parked += run_park_kill_spread(&area, &files, base_line, &warehouse, &delays, "b");
        measured_2 = Some(measured);
        delays_2 = Some(delays);
    }

    area.clear_stale_lock();
    area.assert_consistent("final");

    // Sanity: across the run at least one kill left a park that actually committed and was
    // successfully read back by a pop, so the shared object batch's write window (not just the
    // "killed before anything" path) was actually exercised.
    assert!(parked >= 1,
        "no park ever committed across {} attempt(s) — the write window was never exercised. \
         attempt 1: measured {measured_1:?} uninterrupted, tried delays {delays_1:?}.{}",
        if measured_2.is_some() { 2 } else { 1 },
        match (measured_2, delays_2) {
            (Some(m), Some(d)) => format!(" attempt 2: measured {m:?} uninterrupted, tried delays {d:?}."),
            _ => String::new(),
        });
}

/// The `barrier-count: N` line the `FORKLIFT_DEBUG_BARRIER_COUNT` debug hook prints to stderr
/// (see `main.rs`) — the process-wide count of durability barriers actually paid, mirrors
/// `rollup_hash_skip.rs`'s `skip_count` helper for the analogous rollup-skip counter.
fn barrier_count(output: &std::process::Output) -> u64 {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr.lines()
        .find_map(|line| line.strip_prefix("barrier-count: "))
        .unwrap_or_else(|| panic!("no barrier-count line in stderr: {}", stderr))
        .trim()
        .parse()
        .unwrap()
}

/// One `load` run over a fixed set of directories, each holding `files_per_dir` already-tracked
/// files that are all given real content changes before the measured `load .`, with
/// `FORKLIFT_DEBUG_BARRIER_COUNT=1` set — returns the barrier count that run paid.
fn load_barrier_count_for(files_per_dir: usize) -> u64 {
    let area = Area::new(&format!("barrier-count-{}", files_per_dir));
    let warehouse = area.warehouse();

    const DIR_COUNT: usize = 5;

    for dir in 0..DIR_COUNT {
        let dir_path = warehouse.join(format!("dir{dir}"));
        std::fs::create_dir_all(&dir_path).unwrap();
        for f in 0..files_per_dir {
            std::fs::write(dir_path.join(format!("file{f}.txt")), format!("v1 {dir} {f}\n")).unwrap();
        }
    }

    assert!(area.run(&["prepare"]).status.success());
    assert!(area.run(&["config", "operator.name", "barrier@forklift"]).status.success());
    assert!(area.run(&["config", "operator.identifier", "barrier@forklift"]).status.success());
    assert!(area.run(&["load", "."]).status.success());
    assert!(area.run(&["stack", "base"]).status.success());

    // A real content change to every already-tracked file: every one of the `DIR_COUNT`
    // directories has a genuine content change, so the same single ancestor (the root) needs
    // clearing regardless of `files_per_dir` — only the number of *changed files within* each
    // already-touched directory differs between calls.
    for dir in 0..DIR_COUNT {
        let dir_path = warehouse.join(format!("dir{dir}"));
        for f in 0..files_per_dir {
            std::fs::write(dir_path.join(format!("file{f}.txt")), format!("v2 {dir} {f}\n")).unwrap();
        }
    }

    let mut command = area.command(&["load", "."]);
    command.env("FORKLIFT_DEBUG_BARRIER_COUNT", "1");
    let output = command.output().unwrap();
    assert!(output.status.success(), "load failed: {}", String::from_utf8_lossy(&output.stderr));

    barrier_count(&output)
}

/// DESIGN.html §5.0 D item 10, finding #10: `file_utils::barrier_count` exists so a test can
/// prove a burst of writes actually collapsed to a constant number of barriers, not just that
/// the resulting state happens to be correct. This is that test for `load`'s join point
/// (findings #1/#7's fix restructured it into three barriers — a blob barrier, an
/// ancestor-clear barrier, and a shard-content barrier — still a constant number regardless of
/// how many files changed within the touched directories, not one that scales with the changed
/// file count the way the pre-batching baseline did).
#[test]
fn load_pays_a_constant_number_of_barriers_regardless_of_changed_file_count() {
    let count_1 = load_barrier_count_for(1);
    let count_10 = load_barrier_count_for(10);

    assert_eq!(count_1, count_10,
        "load's barrier count must not scale with the number of changed files: {count_1} for 1 \
         changed file/directory, {count_10} for 10 changed files/directory (the same 5 \
         directories touched either way)");

    // Sanity: the counter is not just always zero, which would trivially "pass" the equality
    // above without proving anything about batching actually happening.
    assert!(count_1 > 0, "the counter must observe real barrier work, got {count_1}");
}
