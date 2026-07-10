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
