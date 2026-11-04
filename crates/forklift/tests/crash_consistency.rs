//! The crash / interrupted-write harness, part of the hardening test spine.
//!
//! "Durable before destructive" holds across power loss: every object, ref and inventory
//! shard is written to a temp file, fsynced, renamed, and the directory fsynced, and a pallet's
//! ref advances only *after* all the objects it names are durable. The claim is
//! that a crash at any instant leaves the store either at its old state or fully at the new one —
//! never a torn object at a real address, never a half-written ref. Commit-graph shards are the
//! exception by design: a rebuildable, checksummed cache, renamed into place with no barrier
//! (`building_the_commit_graph_pays_no_durability_barrier` pins that).
//!
//! A unit test can assert the atomic-write contract (see `file_utils`), but only a real process
//! stopped partway exercises a whole command's publish order. The crash tests below stop `stack`,
//! `load` and `park` at every rename they make in turn (`FORKLIFT_DEBUG_CRASH_AT_RENAME`, see
//! `file_utils::crash_point`), and after each stop assert the store is still internally consistent
//! and usable. A rename is where a written file becomes visible, so stepping through every one
//! visits every state such a crash can leave, in order and on every machine. Killing the process
//! at guessed delays, which these tests did before, reached a given state only when the timing
//! happened to land there: with the blob barrier of `load` moved after its shard publish, 20 kills
//! caught the bug in 4 runs out of 14.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use forklift_core::util::file_utils::CRASH_POINT_EXIT_CODE;

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

    /// A command in the warehouse with the isolated global config and key directory, and fsync
    /// forced on: durability is what this suite checks, so a `FORKLIFT_FSYNC=0` in the
    /// environment the tests were started from must not reach the binary.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(FORKLIFT);
        command
            .args(args)
            .current_dir(self.warehouse())
            .env("FORKLIFT_FSYNC", "1")
            .env("FORKLIFT_GLOBAL_CONFIG", self.root.join("home").join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.root.join("home").join("keys"));
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// A crashed command leaves the warehouse lock behind (no destructor runs), exactly as a real
    /// power loss would; the operator clears it. Do the same before the next command so the lock
    /// is never the reason a later step fails — we are testing store integrity, not the lock.
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

            let peek = self.run(&["store", "peek", head]);
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

/// The most crash points one command is stepped through before the test gives up on it ever
/// completing.
const MAX_CRASH_POINTS: usize = 500;

/// Run `args` in `area` once per rename it makes, stopped just before the first rename, then the
/// second, and so on, until a run makes fewer renames than its crash point and completes.
/// `prepare` runs before each attempt with the attempt's crash point, and `check` after it with
/// whether the run stopped there. Returns how many runs stopped.
///
/// A run that neither stops at its crash point nor succeeds fails the test: the command under test
/// is given a state it can complete from, and the crash point is the only thing allowed to stop it.
fn crash_at_every_rename(
    area: &Area,
    args: &[&str],
    mut prepare: impl FnMut(usize),
    mut check: impl FnMut(usize, bool),
) -> usize {
    for at in 1..=MAX_CRASH_POINTS {
        prepare(at);

        let output = area.command(args).env("FORKLIFT_DEBUG_CRASH_AT_RENAME", at.to_string()).output().unwrap();
        let stopped = output.status.code() == Some(CRASH_POINT_EXIT_CODE);

        assert!(stopped || output.status.success(),
            "{args:?} stopped before rename {at} must exit {CRASH_POINT_EXIT_CODE} or complete, \
             exited {:?}, stderr: {}", output.status.code(), String::from_utf8_lossy(&output.stderr));

        check(at, stopped);

        if !stopped {
            return at - 1;
        }
    }

    panic!("{args:?} still had a rename left after {MAX_CRASH_POINTS} crash points");
}

/// Overwrite each corpus file with content unique to it and to `tag`, so every run under test has a
/// real change in every file to hash, store and publish. Identical files would be stored as one
/// content-addressed blob, so the content names the file's directory and name, and only those two
/// components, so it is the same on every run and OS.
fn rewrite_corpus(files: &[PathBuf], tag: &str) {
    for file in files {
        let dir = file.parent().and_then(|p| p.file_name()).unwrap().to_str().unwrap();
        let name = file.file_name().unwrap().to_str().unwrap();
        std::fs::write(file, format!("{tag} {dir}/{name}\n")).unwrap();
    }
}

/// A corpus of `dirs` directories holding `files_per_dir` files each, written under `warehouse`.
/// Several directories are what `load`'s walk runs concurrently, one task per directory.
fn spread_corpus(warehouse: &Path, dirs: usize, files_per_dir: usize) -> Vec<PathBuf> {
    let mut files = Vec::new();

    for dir in 0..dirs {
        let dir_path = warehouse.join(format!("dir{dir}"));
        std::fs::create_dir_all(&dir_path).unwrap();

        for f in 0..files_per_dir {
            files.push(dir_path.join(format!("file{f}.dat")));
        }
    }

    rewrite_corpus(&files, "seed");
    files
}

/// The current pallet head, if one exists yet.
fn current_head(warehouse: &Path) -> Option<String> {
    std::fs::read_to_string(warehouse.join(".forklift").join("pallets").join("main"))
        .ok()
        .map(|h| h.trim().to_string())
}

/// The same check `crash_at_every_rename` makes of a stopped run, made of the binary's own exit:
/// stopping before a rename the run reaches stops it, with the crash point's exit code and nothing
/// after it published; a crash point past the run's last rename never fires. Without this, a hook
/// that never fired would let every crash test pass on its first, complete run.
#[test]
fn a_crash_point_stops_the_run_before_that_rename() {
    let area = Area::new("crash-point");
    assert!(area.run(&["prepare"]).status.success());
    assert!(area.run(&["config", "set", "operator.identifier", "crash@forklift"]).status.success());

    std::fs::write(area.warehouse().join("file.txt"), "content\n").unwrap();
    assert!(area.run(&["load", "."]).status.success());

    let stopped = area.command(&["stack", "-m", "stopped"]).env("FORKLIFT_DEBUG_CRASH_AT_RENAME", "1").output().unwrap();
    assert_eq!(stopped.status.code(), Some(CRASH_POINT_EXIT_CODE),
        "a stack stopped before its first rename must exit with the crash point's code, stderr: {}",
        String::from_utf8_lossy(&stopped.stderr));
    assert_eq!(current_head(&area.warehouse()), None, "a stack stopped before its first rename publishes no head");

    area.clear_stale_lock();
    let completed = area.command(&["stack", "-m", "completed"]).env("FORKLIFT_DEBUG_CRASH_AT_RENAME", "100000").output().unwrap();
    assert!(completed.status.success(), "a crash point past the last rename must never fire, stderr: {}",
        String::from_utf8_lossy(&completed.stderr));
    assert!(current_head(&area.warehouse()).is_some());
}

#[test]
fn stopping_stack_at_every_rename_never_corrupts_the_store() {
    let area = Area::new("stack");
    let warehouse = area.warehouse();
    let files = spread_corpus(&warehouse, 2, 2);

    assert!(area.run(&["prepare"]).status.success());
    assert!(area.run(&["config", "set", "operator.identifier", "crash@forklift"]).status.success());

    // One stack first, so the runs below build on a parent and move an existing head.
    assert!(area.run(&["load", "."]).status.success());
    assert!(area.run(&["stack", "-m", "base"]).status.success());
    let base = current_head(&warehouse);

    let stopped = crash_at_every_rename(&area, &["stack", "-m", "under test"],
        |at| {
            // Recover from the previous stop and check it left the store consistent.
            area.clear_stale_lock();
            area.assert_consistent(&format!("before stack crash point {at}"));

            // A stop between the head write and the position write leaves the bay behind its own
            // parcel, with files that already match it; catching up records the head, and is a
            // no-op otherwise. Without it every later stack would be refused as stale.
            let caught_up = area.run(&["shift", "main"]);
            assert!(caught_up.status.success(), "catch-up before stack crash point {at} failed: {}",
                String::from_utf8_lossy(&caught_up.stderr));

            rewrite_corpus(&files, &format!("stack {at}"));
            let load = area.run(&["load", "."]);
            assert!(load.status.success(), "load failed: {}", String::from_utf8_lossy(&load.stderr));
        },
        |_, _| {});

    assert!(stopped >= 2, "stack must have been stopped at its renames, it was stopped {stopped} times");
    assert_ne!(current_head(&warehouse), base, "the completed stack must have moved the head");

    // Final recovery: every object reachable from the final head must read back (git export walks
    // the whole graph — parcels, trees, blobs — so a torn or missing object anywhere fails here via
    // the read-side hash check).
    area.clear_stale_lock();
    area.assert_consistent("final");

    let export_dir = area.root.join("git-export");
    let export = area.run(&["git", "export", export_dir.to_str().unwrap()]);
    assert!(export.status.success(),
        "git export must read every committed object without a torn read, stderr: {}",
        String::from_utf8_lossy(&export.stderr));
}

/// An uninterrupted `load .` and a `stack` of whatever it staged, so nothing is left staged for
/// the next `stack` to pick up. `context` names the step in a failure and is the commit message.
fn settle(area: &Area, context: &str) {
    let load = area.run(&["load", "."]);
    assert!(load.status.success(), "{context}: load failed: {}",
        String::from_utf8_lossy(&load.stderr));
    stack_or_nothing(area, context);
}

/// Run `stack` with `message` and return whether it committed. Anything other than a commit or
/// "nothing to stack" fails the test.
fn stack_or_nothing(area: &Area, message: &str) -> bool {
    let stack = area.run(&["stack", "-m", message]);
    let nothing_to_stack = String::from_utf8_lossy(&stack.stderr).to_lowercase().contains("nothing to stack");
    assert!(stack.status.success() || nothing_to_stack,
        "{message}: stack must either succeed or report nothing staged, stderr: {}",
        String::from_utf8_lossy(&stack.stderr));
    stack.status.success()
}

/// Extends the crash-consistency spine to `load`'s parallel per-directory walk (DESIGN.html §5.0
/// D item 10, finding #1): every changed file's blob is staged into the walk's own shared batch
/// (`InventoryBuilderContext::blob_batch`), which the join point finishes before it publishes any
/// shard naming those blobs. This test proves no shard ever becomes visible while a blob it
/// references is still an unpublished temp file: stop `load` at every rename, and after each stop
/// build directly on whatever it left behind.
///
/// That build is a `stack`, with no healing re-load first. A healing re-load could mask exactly the
/// bug this test exists to catch, since the stat-cache fast path (`is_entry_unchanged`) trusts an
/// already-published shard's proven entries without re-reading their files, so a second `load .`
/// would not necessarily re-touch (or heal) a shard a stopped `load .` already made visible.
/// `stack` refuses staged content missing from the store, and the final `git export` reads every
/// committed object back.
///
/// The incomplete-load guard (`load_guard_utils`) durably marks a load's root before anything else
/// it publishes, precisely so a stopped `load` is caught even though it never got to report an
/// error, and a real `stack` refuses on it. That marker is a different concern from the one under
/// test here (inventory completeness, not blob durability), so it is removed by hand right before
/// each direct `stack`, to keep reaching the missing-blob check.
#[test]
fn stopping_load_at_every_rename_never_leaves_a_shard_referencing_a_missing_or_torn_blob() {
    let area = Area::new("load");
    let warehouse = area.warehouse();
    let files = spread_corpus(&warehouse, 2, 2);

    assert!(area.run(&["prepare"]).status.success());
    assert!(area.run(&["config", "set", "operator.identifier", "crash@forklift"]).status.success());
    settle(&area, "base");

    let mut stacked = 0usize;

    let stopped = crash_at_every_rename(&area, &["load", "."],
        |at| {
            area.clear_stale_lock();
            area.assert_consistent(&format!("before load crash point {at}"));
            rewrite_corpus(&files, &format!("load {at}"));
        },
        |at, _| {
            area.clear_stale_lock();
            let _ = std::fs::remove_file(
                forklift_core::util::load_guard_utils::marker_path_under(&area.warehouse())
            );

            if stack_or_nothing(&area, &format!("load {at} commit")) {
                stacked += 1;
            }

            // Stage and commit the working directory's current content uninterrupted, so nothing
            // a stop left staged carries over, and nothing is staged when the next run starts.
            settle(&area, &format!("load {at} heal"));
        });

    assert!(stopped >= 2, "load must have been stopped at its renames, it was stopped {stopped} times");
    assert!(stacked >= 1, "the completed load must have staged something a stack committed");

    area.clear_stale_lock();
    area.assert_consistent("final");

    let export_dir = area.root.join("git-export");
    let export = area.run(&["git", "export", export_dir.to_str().unwrap()]);
    assert!(export.status.success(),
        "git export must read every committed object without a torn or missing blob, stderr: {}",
        String::from_utf8_lossy(&export.stderr));
}

/// The number of parked parcels currently recorded, read directly via `forklift-core` (not the
/// CLI) so a torn or malformed `.forklift/parked` file panics this test loudly instead of being
/// silently swallowed by output parsing.
fn parked_count(warehouse: &Path) -> usize {
    let _scope = forklift_core::globals::StorageRootScope::enter(warehouse);
    forklift_core::util::park_utils::read_parked().unwrap().len()
}

/// Whether an interrupted operation's marker stands in the warehouse's main bay.
fn operation_standing(warehouse: &Path) -> bool {
    let _scope = forklift_core::globals::StorageRootScope::enter(warehouse);
    forklift_core::util::operation_utils::read().unwrap().is_some()
}

/// Extends the crash-consistency spine to `park` push's own object batch (DESIGN.html §5.0 D
/// item 10, finding #3): every tree object and the parcel object are staged into one shared
/// `WriteBatch`, finished once before the signature sidecar and the parked-list record, and
/// `refresh_tracked_entries`'s own blob batch is finished before any shard it rewrites becomes
/// visible. Stop `park` at every rename, and after each stop either see no new parked entry
/// (nothing to check) or pop the one that did commit — a real read of every tree and blob the
/// parcel references, which fails loudly on a torn or missing object.
///
/// A stop inside park's operation bracket leaves its marker, and every later park refuses until
/// heal settles it: back to where it was if the parked list does not name the parcel yet, forward
/// to the head if it does. Rolling forward finishes the reset, so the pop runs over a clean bay
/// either way. When the park did not commit, its reset-to-head step (the only thing that touches
/// tracked file content) never ran, so the working directory still holds this run's content.
#[test]
fn stopping_park_at_every_rename_never_leaves_a_parked_parcel_referencing_a_missing_or_torn_object() {
    let area = Area::new("park");
    let warehouse = area.warehouse();
    let files = spread_corpus(&warehouse, 2, 2);

    assert!(area.run(&["prepare"]).status.success());
    assert!(area.run(&["config", "set", "operator.identifier", "crash@forklift"]).status.success());

    // A pallet head to park onto — `park` refuses when the pallet has nothing stacked yet.
    assert!(area.run(&["load", "."]).status.success());
    assert!(area.run(&["stack", "-m", "base"]).status.success());

    let before = std::cell::Cell::new(0usize);
    let mut parked = 0usize;

    let stopped = crash_at_every_rename(&area, &["park"],
        |at| {
            area.clear_stale_lock();
            area.assert_consistent(&format!("before park crash point {at}"));
            rewrite_corpus(&files, &format!("park {at}"));
            before.set(parked_count(&warehouse));
        },
        |at, _| {
            area.clear_stale_lock();

            if operation_standing(&warehouse) {
                let heal = area.run(&["heal"]);
                assert!(heal.status.success(), "park crash point {at}: heal must settle the interrupted park, stderr: {}",
                    String::from_utf8_lossy(&heal.stderr));
                assert!(!operation_standing(&warehouse), "park crash point {at}: heal must clear the marker");
            }

            if parked_count(&warehouse) > before.get() {
                parked += 1;

                let pop = area.run(&["park", "pop"]);
                assert!(pop.status.success(),
                    "park crash point {at}: pop of a park that committed must read back every object \
                     it referenced, stderr: {}", String::from_utf8_lossy(&pop.stderr));
            }
        });

    assert!(stopped >= 2, "park must have been stopped at its renames, it was stopped {stopped} times");
    assert!(parked >= 1, "the completed park must have been parked and popped");

    area.clear_stale_lock();
    area.assert_consistent("final");
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
    assert!(area.run(&["config", "set", "operator.identifier", "barrier@forklift"]).status.success());
    assert!(area.run(&["load", "."]).status.success());
    assert!(area.run(&["stack", "-m", "base"]).status.success());

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

/// One `restore .` over 5 directories, each holding `files_per_dir` tracked files that were all
/// edited after the stack, with `FORKLIFT_DEBUG_BARRIER_COUNT=1` set — returns the barrier count
/// that run paid.
fn restore_barrier_count_for(files_per_dir: usize) -> u64 {
    let area = Area::new(&format!("restore-barrier-count-{}", files_per_dir));
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
    assert!(area.run(&["config", "set", "operator.identifier", "barrier@forklift"]).status.success());
    assert!(area.run(&["load", "."]).status.success());
    assert!(area.run(&["stack", "-m", "base"]).status.success());

    for dir in 0..DIR_COUNT {
        let dir_path = warehouse.join(format!("dir{dir}"));
        for f in 0..files_per_dir {
            std::fs::write(dir_path.join(format!("file{f}.txt")), format!("edited {dir} {f}\n")).unwrap();
        }
    }

    let mut command = area.command(&["restore", "--confirm", "."]);
    command.env("FORKLIFT_DEBUG_BARRIER_COUNT", "1");
    let output = command.output().unwrap();
    assert!(output.status.success(), "restore failed: {}", String::from_utf8_lossy(&output.stderr));

    // The restore really rewrote every file, so the count below measures a full restore.
    for dir in 0..DIR_COUNT {
        let dir_path = warehouse.join(format!("dir{dir}"));
        for f in 0..files_per_dir {
            assert_eq!(std::fs::read_to_string(dir_path.join(format!("file{f}.txt"))).unwrap(),
                format!("v1 {dir} {f}\n"));
        }
    }

    barrier_count(&output)
}

/// `restore` of a directory publishes every shard it refreshes in one batch, so its barrier
/// count does not grow with the number of files it rewrites — before the batch it paid one
/// shard publish per file.
#[test]
fn restore_pays_a_constant_number_of_barriers_regardless_of_restored_file_count() {
    let count_1 = restore_barrier_count_for(1);
    let count_10 = restore_barrier_count_for(10);

    assert_eq!(count_1, count_10,
        "restore's barrier count must not scale with the number of restored files: {count_1} for 1 \
         file/directory, {count_10} for 10 files/directory (the same 5 directories either way)");
    assert!(count_1 > 0, "the counter must observe real barrier work, got {count_1}");
}

/// One `park pop` over 5 directories, each holding `files_per_dir` tracked files that were all
/// edited and `files_per_dir` more that were removed before the `park`, with
/// `FORKLIFT_DEBUG_BARRIER_COUNT=1` set — returns the barrier count that run paid.
fn park_pop_barrier_count_for(files_per_dir: usize) -> u64 {
    let area = Area::new(&format!("park-pop-barrier-count-{}", files_per_dir));
    let warehouse = area.warehouse();

    const DIR_COUNT: usize = 5;

    for dir in 0..DIR_COUNT {
        let dir_path = warehouse.join(format!("dir{dir}"));
        std::fs::create_dir_all(&dir_path).unwrap();
        for f in 0..files_per_dir {
            std::fs::write(dir_path.join(format!("file{f}.txt")), format!("v1 {dir} {f}\n")).unwrap();
            std::fs::write(dir_path.join(format!("gone{f}.txt")), format!("gone {dir} {f}\n")).unwrap();
        }
    }

    assert!(area.run(&["prepare"]).status.success());
    assert!(area.run(&["config", "set", "operator.identifier", "barrier@forklift"]).status.success());
    assert!(area.run(&["load", "."]).status.success());
    assert!(area.run(&["stack", "-m", "base"]).status.success());

    for dir in 0..DIR_COUNT {
        let dir_path = warehouse.join(format!("dir{dir}"));
        for f in 0..files_per_dir {
            std::fs::write(dir_path.join(format!("file{f}.txt")), format!("parked {dir} {f}\n")).unwrap();
            std::fs::remove_file(dir_path.join(format!("gone{f}.txt"))).unwrap();
        }
    }

    assert!(area.run(&["load", "."]).status.success());
    assert!(area.run(&["park"]).status.success());

    // The park put the head back, so the pop below re-applies every edit and every removal.
    assert_eq!(std::fs::read_to_string(warehouse.join("dir0").join("file0.txt")).unwrap(), "v1 0 0\n");
    assert!(warehouse.join("dir0").join("gone0.txt").exists());

    let mut command = area.command(&["park", "pop"]);
    command.env("FORKLIFT_DEBUG_BARRIER_COUNT", "1");
    let output = command.output().unwrap();
    assert!(output.status.success(), "park pop failed: {}", String::from_utf8_lossy(&output.stderr));

    for dir in 0..DIR_COUNT {
        let dir_path = warehouse.join(format!("dir{dir}"));
        for f in 0..files_per_dir {
            assert_eq!(std::fs::read_to_string(dir_path.join(format!("file{f}.txt"))).unwrap(),
                format!("parked {dir} {f}\n"));
            assert!(!dir_path.join(format!("gone{f}.txt")).exists());
        }
    }

    barrier_count(&output)
}

/// `park pop` stages every entry it re-applies through one batch, so its barrier count does not
/// grow with the number of files it writes — before the batch it paid one shard publish per file.
#[test]
fn park_pop_pays_a_constant_number_of_barriers_regardless_of_file_count() {
    let count_1 = park_pop_barrier_count_for(1);
    let count_10 = park_pop_barrier_count_for(10);

    assert_eq!(count_1, count_10,
        "park pop's barrier count must not scale with the number of files it writes: {count_1} \
         for 1 file/directory, {count_10} for 10 files/directory (the same 5 directories either way)");
    assert!(count_1 > 0, "the counter must observe real barrier work, got {count_1}");
}

/// A `park pop` whose staging fails after it rewrote the working directory leaves the parcel
/// parked and says so; the files it wrote stay on disk. The staging is made to fail by making one
/// shard's folder read-only, so nothing can be published into it.
#[cfg(unix)]
#[test]
fn park_pop_whose_staging_fails_keeps_the_parcel_parked() {
    use std::os::unix::fs::PermissionsExt;

    let area = Area::new("park-pop-publish-fails");
    let warehouse = area.warehouse();

    for dir in 0..2 {
        std::fs::create_dir_all(warehouse.join(format!("dir{dir}"))).unwrap();
        std::fs::write(warehouse.join(format!("dir{dir}")).join("file.txt"), "v1\n").unwrap();
    }

    assert!(area.run(&["prepare"]).status.success());
    assert!(area.run(&["config", "set", "operator.identifier", "barrier@forklift"]).status.success());
    assert!(area.run(&["load", "."]).status.success());
    assert!(area.run(&["stack", "-m", "base"]).status.success());

    for dir in 0..2 {
        std::fs::write(warehouse.join(format!("dir{dir}")).join("file.txt"), "parked\n").unwrap();
    }

    assert!(area.run(&["load", "."]).status.success());
    assert!(area.run(&["park"]).status.success());
    assert_eq!(std::fs::read_to_string(warehouse.join("dir0").join("file.txt")).unwrap(), "v1\n");

    let shard_folder = shard_folder_of(&warehouse, "dir0");
    assert!(shard_folder.is_dir(), "dir0 has a shard folder");

    std::fs::set_permissions(&shard_folder, std::fs::Permissions::from_mode(0o555)).unwrap();

    // Root writes through a read-only folder, so the failure cannot be staged; nothing to test.
    if std::fs::write(shard_folder.join("probe"), "").is_ok() {
        std::fs::set_permissions(&shard_folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let output = area.run(&["park", "pop"]);
    std::fs::set_permissions(&shard_folder, std::fs::Permissions::from_mode(0o755)).unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "park pop must fail when its staging fails");
    assert!(stderr.contains("The parcel is still parked."), "the failure says the parcel is still parked: {stderr}");

    // The working directory was rewritten before the staging failed.
    assert_eq!(std::fs::read_to_string(warehouse.join("dir0").join("file.txt")).unwrap(), "parked\n");

    // ...and the shard that could not be published still lists the head's version, so the
    // rewritten file shows as an unstaged change.
    let status = area.run(&["--json", "stocktake"]);
    assert!(status.status.success());
    let status_json: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert!(status_json["data"]["unstaged"].as_array().unwrap().iter()
            .any(|entry| entry["path"] == "dir0/file.txt" && entry["kind"] == "modified"),
        "dir0/file.txt is not staged: {status_json}");

    let list = area.run(&["--json", "park", "list"]);
    assert!(list.status.success());
    let list_json: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(list_json["data"]["parked"].as_array().map(Vec::len), Some(1),
        "the parcel is still parked: {list_json}");
}

/// A `consolidate` one of whose actions fails stages none of the actions applied before it, and
/// records nothing as in progress. The failure is a removal in a read-only directory; the
/// removals in the directories before it already happened on disk.
#[cfg(unix)]
#[test]
fn consolidate_whose_action_fails_stages_nothing() {
    use std::os::unix::fs::PermissionsExt;

    let area = Area::new("consolidate-action-fails");
    let warehouse = area.warehouse();
    diverged_merge_fixture(&area, 1, false);

    let locked = warehouse.join("dir4");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();

    // Root removes from a read-only folder, so the failure cannot be staged; nothing to test.
    if std::fs::write(locked.join("probe"), "").is_ok() {
        std::fs::remove_file(locked.join("probe")).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let output = area.run(&["consolidate", "feature"]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "consolidate must fail when an action fails");
    assert!(stderr.contains("dir4/gone0.txt"), "the failure names the action that failed: {stderr}");
    assert!(stderr.contains("but some or all of them are not staged. Nothing was recorded as in progress."),
        "the failure says what is staged and that nothing is in progress: {stderr}");
    assert!(!warehouse.join(".forklift").join("consolidation").exists(), "no consolidation is in progress");

    // An earlier removal happened on disk...
    assert!(!warehouse.join("dir0").join("gone0.txt").exists());

    // ...and was not staged: the inventory still lists the file, so it reads as an unstaged
    // deletion, and nothing is staged at all.
    let status = area.run(&["--json", "stocktake"]);
    assert!(status.status.success());
    let status_json: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status_json["data"]["staged_count"], 0, "nothing is staged: {status_json}");
    assert!(status_json["data"]["unstaged"].as_array().unwrap().iter()
            .any(|entry| entry["path"] == "dir0/gone0.txt" && entry["kind"] == "removed"),
        "dir0/gone0.txt is an unstaged deletion: {status_json}");
}

/// The shard folder of directory `key` in the staged inventory of `warehouse`.
#[cfg(unix)]
fn shard_folder_of(warehouse: &Path, key: &str) -> PathBuf {
    warehouse.join(".forklift/inventory").join(forklift_core::util::file_utils::shard_folder_relative(key))
}

/// Five lines whose first `feature` edits and whose last `main` edits, so the two merge cleanly.
/// The middle line is the file's own, so no two merged files share a blob.
fn merge_lines(first: &str, middle: &str, last: &str) -> String {
    format!("{first}\nb\n{middle}\nd\n{last}\n")
}

/// A warehouse with 5 directories, each holding `files_per_dir` files of every kind of merge
/// action: `file*` the `feature` pallet edits, `gone*` it removes, and `merged*` both pallets
/// edit in different lines; with `clash`, also `clash*`, which both edit in the same line. `main`
/// also moves on in a file of its own, so the two always diverge. Leaves `main` checked out, and
/// returns the `feature` head.
fn diverged_merge_fixture(area: &Area, files_per_dir: usize, clash: bool) -> String {
    let warehouse = area.warehouse();

    const DIR_COUNT: usize = 5;

    for dir in 0..DIR_COUNT {
        let dir_path = warehouse.join(format!("dir{dir}"));
        std::fs::create_dir_all(&dir_path).unwrap();
        for f in 0..files_per_dir {
            std::fs::write(dir_path.join(format!("file{f}.txt")), format!("v1 {dir} {f}\n")).unwrap();
            std::fs::write(dir_path.join(format!("gone{f}.txt")), format!("gone {dir} {f}\n")).unwrap();
            std::fs::write(dir_path.join(format!("merged{f}.txt")), merge_lines("a", &format!("{dir} {f}"), "e")).unwrap();
            if clash {
                std::fs::write(dir_path.join(format!("clash{f}.txt")), format!("clash {dir} {f}\n")).unwrap();
            }
        }
    }
    std::fs::write(warehouse.join("main.txt"), "base\n").unwrap();

    assert!(area.run(&["prepare"]).status.success());
    assert!(area.run(&["config", "set", "operator.identifier", "barrier@forklift"]).status.success());
    assert!(area.run(&["load", "."]).status.success());
    assert!(area.run(&["stack", "-m", "base"]).status.success());
    assert!(area.run(&["pallet", "create", "feature"]).status.success());

    for dir in 0..DIR_COUNT {
        let dir_path = warehouse.join(format!("dir{dir}"));
        for f in 0..files_per_dir {
            std::fs::write(dir_path.join(format!("file{f}.txt")), format!("theirs {dir} {f}\n")).unwrap();
            std::fs::remove_file(dir_path.join(format!("gone{f}.txt"))).unwrap();
            std::fs::write(dir_path.join(format!("merged{f}.txt")), merge_lines("theirs", &format!("{dir} {f}"), "e")).unwrap();
            if clash {
                std::fs::write(dir_path.join(format!("clash{f}.txt")), format!("theirs {dir} {f}\n")).unwrap();
            }
        }
    }

    assert!(area.run(&["load", "."]).status.success());
    assert!(area.run(&["stack", "-m", "feature work"]).status.success());
    let feature = std::fs::read_to_string(warehouse.join(".forklift").join("pallets").join("feature"))
        .unwrap().trim().to_string();

    assert!(area.run(&["shift", "main"]).status.success());
    std::fs::write(warehouse.join("main.txt"), "main moved on\n").unwrap();
    for dir in 0..DIR_COUNT {
        let dir_path = warehouse.join(format!("dir{dir}"));
        for f in 0..files_per_dir {
            std::fs::write(dir_path.join(format!("merged{f}.txt")), merge_lines("a", &format!("{dir} {f}"), "ours")).unwrap();
            if clash {
                std::fs::write(dir_path.join(format!("clash{f}.txt")), format!("ours {dir} {f}\n")).unwrap();
            }
        }
    }
    assert!(area.run(&["load", "."]).status.success());
    assert!(area.run(&["stack", "-m", "main work"]).status.success());

    // The merge below applies every edit and every removal.
    assert_eq!(std::fs::read_to_string(warehouse.join("dir0").join("file0.txt")).unwrap(), "v1 0 0\n");
    assert!(warehouse.join("dir0").join("gone0.txt").exists());

    feature
}

/// Every edit and removal of [`diverged_merge_fixture`] is in the working directory, and the
/// merge left nothing unstaged or in progress behind it.
fn assert_diverged_merge_applied(area: &Area, files_per_dir: usize) {
    let warehouse = area.warehouse();

    for dir in 0..5 {
        let dir_path = warehouse.join(format!("dir{dir}"));
        for f in 0..files_per_dir {
            assert_eq!(std::fs::read_to_string(dir_path.join(format!("file{f}.txt"))).unwrap(),
                format!("theirs {dir} {f}\n"));
            assert!(!dir_path.join(format!("gone{f}.txt")).exists());
            assert_eq!(std::fs::read_to_string(dir_path.join(format!("merged{f}.txt"))).unwrap(),
                merge_lines("theirs", &format!("{dir} {f}"), "ours"));
        }
    }

    let status = area.run(&["--json", "stocktake"]);
    assert!(status.status.success());
    let status_json: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status_json["data"]["unstaged_count"], 0, "{status_json}");
    assert_eq!(status_json["data"]["staged_count"], 0, "{status_json}");
}

/// The barrier count of one clean `consolidate feature` over [`diverged_merge_fixture`].
fn consolidate_barrier_count_for(files_per_dir: usize) -> u64 {
    let area = Area::new(&format!("consolidate-barrier-count-{}", files_per_dir));
    diverged_merge_fixture(&area, files_per_dir, false);

    let mut command = area.command(&["consolidate", "feature"]);
    command.env("FORKLIFT_DEBUG_BARRIER_COUNT", "1");
    let output = command.output().unwrap();
    assert!(output.status.success(), "consolidate failed: {}", String::from_utf8_lossy(&output.stderr));

    assert_diverged_merge_applied(&area, files_per_dir);

    barrier_count(&output)
}

/// The barrier count of one clean `cherry-pick` of `feature`'s head over
/// [`diverged_merge_fixture`].
fn cherry_pick_barrier_count_for(files_per_dir: usize) -> u64 {
    let area = Area::new(&format!("cherry-pick-barrier-count-{}", files_per_dir));
    let feature = diverged_merge_fixture(&area, files_per_dir, false);

    let mut command = area.command(&["pick", &feature]);
    command.env("FORKLIFT_DEBUG_BARRIER_COUNT", "1");
    let output = command.output().unwrap();
    assert!(output.status.success(), "cherry-pick failed: {}", String::from_utf8_lossy(&output.stderr));

    assert_diverged_merge_applied(&area, files_per_dir);

    barrier_count(&output)
}

/// `consolidate` stages every merge action through one batch, so its barrier count does not grow
/// with the number of files the merge writes or removes — before the batch it paid one shard
/// publish per action.
#[test]
fn consolidate_pays_a_constant_number_of_barriers_regardless_of_action_count() {
    let count_1 = consolidate_barrier_count_for(1);
    let count_10 = consolidate_barrier_count_for(10);

    assert_eq!(count_1, count_10,
        "consolidate's barrier count must not scale with the number of merge actions: {count_1} \
         for 1 action of each kind/directory, {count_10} for 10 (the same 5 directories)");
    assert!(count_1 > 0, "the counter must observe real barrier work, got {count_1}");
}

/// A `consolidate` that stops on conflicts — one per `clash*` file — stages them through the same
/// batch, so its barrier count does not grow with the conflict count either.
#[test]
fn consolidate_with_conflicts_pays_a_constant_number_of_barriers_regardless_of_action_count() {
    let count_for = |files_per_dir: usize| {
        let area = Area::new(&format!("consolidate-conflict-barrier-count-{}", files_per_dir));
        let warehouse = area.warehouse();
        diverged_merge_fixture(&area, files_per_dir, true);

        let mut command = area.command(&["consolidate", "feature"]);
        command.env("FORKLIFT_DEBUG_BARRIER_COUNT", "1");
        let output = command.output().unwrap();
        assert!(output.status.success(), "consolidate failed: {}", String::from_utf8_lossy(&output.stderr));
        assert!(warehouse.join(".forklift").join("consolidation").exists(), "the merge stopped on its conflicts");

        let status = area.run(&["--json", "stocktake"]);
        let status_json: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
        let conflicts = ["staged", "unstaged"].iter()
            .flat_map(|side| status_json["data"][side].as_array().into_iter().flatten())
            .filter(|entry| entry["kind"] == "conflict")
            .count();
        assert_eq!(conflicts, 5 * files_per_dir, "every clash is a conflict: {status_json}");

        barrier_count(&output)
    };

    let count_1 = count_for(1);
    let count_10 = count_for(10);

    assert_eq!(count_1, count_10,
        "a conflicting consolidate's barrier count must not scale with the number of merge actions: \
         {count_1} for 1 action of each kind/directory, {count_10} for 10 (the same 5 directories)");
    assert!(count_1 > 0, "the counter must observe real barrier work, got {count_1}");
}

/// `cherry-pick` applies the same merge actions through the same batch.
#[test]
fn cherry_pick_pays_a_constant_number_of_barriers_regardless_of_action_count() {
    let count_1 = cherry_pick_barrier_count_for(1);
    let count_10 = cherry_pick_barrier_count_for(10);

    assert_eq!(count_1, count_10,
        "cherry-pick's barrier count must not scale with the number of merge actions: {count_1} \
         for 1 action of each kind/directory, {count_10} for 10 (the same 5 directories)");
    assert!(count_1 > 0, "the counter must observe real barrier work, got {count_1}");
}

/// A conflict the merge records in a deep directory changes only its entry's state there (its
/// hash stays ours), yet it must still clear every ancestor's rollup, or stocktake would trust
/// the rollups above it and never see the conflict.
#[test]
fn a_merge_conflict_clears_the_rollups_above_it() {
    let area = Area::new("merge-conflict-rollups");
    let warehouse = area.warehouse();
    let deep = warehouse.join("a").join("b").join("c");

    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("f.txt"), "base\n").unwrap();

    assert!(area.run(&["prepare"]).status.success());
    assert!(area.run(&["config", "set", "operator.identifier", "barrier@forklift"]).status.success());
    assert!(area.run(&["load", "."]).status.success());
    assert!(area.run(&["stack", "-m", "base"]).status.success());
    assert!(area.run(&["pallet", "create", "feature"]).status.success());
    std::fs::write(deep.join("f.txt"), "theirs\n").unwrap();
    assert!(area.run(&["load", "."]).status.success());
    assert!(area.run(&["stack", "-m", "feature work"]).status.success());
    assert!(area.run(&["shift", "main"]).status.success());
    std::fs::write(deep.join("f.txt"), "ours\n").unwrap();
    assert!(area.run(&["load", "."]).status.success());
    assert!(area.run(&["stack", "-m", "main work"]).status.success());

    // A clean warehouse whose rollups stocktake trusts, before the merge.
    let before = area.run(&["--json", "stocktake"]);
    let before_json: serde_json::Value = serde_json::from_slice(&before.stdout).unwrap();
    assert_eq!(before_json["data"]["staged_count"], 0, "{before_json}");
    assert_eq!(before_json["data"]["unstaged_count"], 0, "{before_json}");

    let output = area.run(&["consolidate", "feature"]);
    assert!(output.status.success(), "consolidate failed: {}", String::from_utf8_lossy(&output.stderr));
    assert!(warehouse.join(".forklift").join("consolidation").exists(), "the merge stopped on its conflict");

    let status = area.run(&["--json", "stocktake"]);
    let status_json: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let all: Vec<&serde_json::Value> = ["staged", "unstaged"].iter()
        .flat_map(|side| status_json["data"][side].as_array().into_iter().flatten())
        .collect();
    assert!(all.iter().any(|entry| entry["path"] == "a/b/c/f.txt" && entry["kind"] == "conflict"),
        "stocktake reports the conflict: {status_json}");
}

/// A `consolidate` whose staging fails after it rewrote the working directory records nothing as
/// in progress and says so; the files it wrote stay on disk, unstaged. The staging is made to
/// fail by making one shard's folder read-only, so nothing can be published into it.
#[cfg(unix)]
#[test]
fn consolidate_whose_staging_fails_records_nothing_in_progress() {
    use std::os::unix::fs::PermissionsExt;

    let area = Area::new("consolidate-publish-fails");
    let warehouse = area.warehouse();
    diverged_merge_fixture(&area, 1, false);

    let shard_folder = shard_folder_of(&warehouse, "dir0");
    assert!(shard_folder.is_dir(), "dir0 has a shard folder");

    std::fs::set_permissions(&shard_folder, std::fs::Permissions::from_mode(0o555)).unwrap();

    // Root writes through a read-only folder, so the failure cannot be staged; nothing to test.
    if std::fs::write(shard_folder.join("probe"), "").is_ok() {
        std::fs::set_permissions(&shard_folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let output = area.run(&["consolidate", "feature"]);
    std::fs::set_permissions(&shard_folder, std::fs::Permissions::from_mode(0o755)).unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "consolidate must fail when its staging fails");
    assert!(stderr.contains("but some or all of them are not staged. Nothing was recorded as in progress."),
        "the failure says what is staged and that nothing is in progress: {stderr}");
    assert!(!warehouse.join(".forklift").join("consolidation").exists(), "no consolidation is in progress");

    // The working directory was rewritten before the staging failed...
    assert_eq!(std::fs::read_to_string(warehouse.join("dir0").join("file0.txt")).unwrap(), "theirs 0 0\n");

    // ...and the shard that could not be published still lists the head's version, so the
    // rewritten file shows as an unstaged change.
    let status = area.run(&["--json", "stocktake"]);
    assert!(status.status.success());
    let status_json: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert!(status_json["data"]["unstaged"].as_array().unwrap().iter()
            .any(|entry| entry["path"] == "dir0/file0.txt" && entry["kind"] == "modified"),
        "dir0/file0.txt is not staged: {status_json}");
}

/// The incomplete-load marker (`load_guard_utils`) must ride a load's own first durable barrier
/// instead of paying a second, standalone one. For a directory `load`
/// with real content to publish, that first barrier is the walk's blob-publish batch (Phase 0 —
/// see `inventory_utils::create_inventory_for_directory`'s doc comment), so folding the marker in
/// costs nothing: this fixture's own join point already pays exactly two barriers (blob-publish,
/// then shard-content), with or without the marker.
///
/// The exact count is pinned, not just cross-checked for equality like the test above, because
/// equality alone cannot catch a regression that adds a *constant* extra barrier to every run —
/// exactly what a reintroduced standalone marker write would do. Verified by hand against this
/// fixture: reintroducing a standalone, immediately-written marker call (e.g. a bare
/// `marker_utils::add` at the top of `inventory_utils::add_changes_to_inventory`, undoing the
/// fold) raises this to 3.
#[test]
fn a_small_directory_load_pays_no_extra_barrier_for_the_incomplete_load_marker() {
    let area = Area::new("barrier-pin-directory");
    let warehouse = area.warehouse();

    std::fs::create_dir_all(warehouse.join("dir0")).unwrap();
    std::fs::write(warehouse.join("dir0").join("file0.txt"), "v1\n").unwrap();

    assert!(area.run(&["prepare"]).status.success());
    assert!(area.run(&["config", "set", "operator.identifier", "barrier@forklift"]).status.success());

    let mut command = area.command(&["load", "."]);
    command.env("FORKLIFT_DEBUG_BARRIER_COUNT", "1");
    let output = command.output().unwrap();
    assert!(output.status.success(), "load failed: {}", String::from_utf8_lossy(&output.stderr));

    assert_eq!(barrier_count(&output), 2);
}

/// The single-file `load` counterpart of the pin above. A single-file load never builds a
/// walk-wide blob batch (see `inventory_utils::add_file_to_inventory`'s doc comment — there is no
/// walk, just this one file), so the marker instead folds into the loaded file's own
/// shard-content publish (the `extra` of `inventory_utils::shard_write::publish_shards`). A root-level file
/// has no ancestor rollup to clear, so that shard write is this load's only real barrier — with
/// or without the marker.
///
/// Verified by hand: reintroducing a standalone marker write raises this to 3.
#[test]
fn a_small_single_file_load_pays_no_extra_barrier_for_the_incomplete_load_marker() {
    let area = Area::new("barrier-pin-file");
    let warehouse = area.warehouse();

    std::fs::write(warehouse.join("a.txt"), "v1\n").unwrap();

    assert!(area.run(&["prepare"]).status.success());
    assert!(area.run(&["config", "set", "operator.identifier", "barrier@forklift"]).status.success());

    let mut command = area.command(&["load", "a.txt"]);
    command.env("FORKLIFT_DEBUG_BARRIER_COUNT", "1");
    let output = command.output().unwrap();
    assert!(output.status.success(), "load failed: {}", String::from_utf8_lossy(&output.stderr));

    assert_eq!(barrier_count(&output), 2);
}

/// The commit-graph is a rebuildable cache, so its shards are published with no durability
/// barrier: a `blame` that builds the graph from cold pays none at all (it used to pay about two
/// per parcel). The `stack` beforehand proves the counter is live in this run — fsync is on, so
/// a zero below means the graph wrote without a barrier, not that nothing was counted.
#[test]
fn building_the_commit_graph_pays_no_durability_barrier() {
    let area = Area::new("graph-barriers");
    let warehouse = area.warehouse();

    assert!(area.run(&["prepare"]).status.success());
    assert!(area.run(&["config", "set", "operator.identifier", "graph@forklift"]).status.success());

    for i in 0..8 {
        std::fs::write(warehouse.join("f.txt"), format!("line {i}\n").repeat(i + 1)).unwrap();
        assert!(area.run(&["load", "f.txt"]).status.success());
        let mut stack = area.command(&["stack", "-m", &format!("change {i}")]);
        stack.env("FORKLIFT_DEBUG_BARRIER_COUNT", "1");
        let output = stack.output().unwrap();
        assert!(output.status.success(), "stack failed: {}", String::from_utf8_lossy(&output.stderr));
        assert!(barrier_count(&output) > 0, "sanity: a stack pays durability barriers with fsync on");
    }

    let graph_root = warehouse.join(".forklift").join("graph");
    let _ = std::fs::remove_dir_all(&graph_root);

    let mut blame = area.command(&["blame", "f.txt"]);
    blame.env("FORKLIFT_DEBUG_BARRIER_COUNT", "1");
    let output = blame.output().unwrap();
    assert!(output.status.success(), "blame failed: {}", String::from_utf8_lossy(&output.stderr));

    let shards = std::fs::read_dir(&graph_root).map(|entries| entries.count()).unwrap_or(0);
    assert!(shards > 0, "sanity: the cold blame must have built the commit-graph");
    assert_eq!(barrier_count(&output), 0, "building the commit-graph must not pay a durability barrier");
}
