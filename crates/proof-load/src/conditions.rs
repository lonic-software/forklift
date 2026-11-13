//! Environment faults: the disk filling under the faulted command, and its warehouse going
//! read-only. Each is set after the faulted record's snapshot is written and before its lines run,
//! and lifted before the readback and the retry that grade it: what is graded is whether the tool
//! recovers once the condition is gone, and whether anything it acknowledged was lost meanwhile.
//!
//! Disk full runs on a volume the caller made and capped (a size-capped tmpfs, a RAM disk), whose
//! free space the harness fills with a ballast file beside the scratch areas, never inside one. The
//! lift removes the ballast, so every leftover the tool wrote stays where it wrote it, and the run
//! keeps the absolute paths a bay records. Read-only clears the write bits of the work folder,
//! warehouse included, and the lift restores them: no unprivileged remount exists, and a folder
//! whose cleared bits still admit a write (root, or folder attributes that refuse nothing) makes
//! the fault not run rather than graded as one.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::corpus::Corpus;
use crate::damage::{self, Kind, Warehouse};
use crate::faulted::{self, Context, Graded, Programs};
use crate::runner::{self, RunError};
use crate::scenario::{Scenario, Tool};
use crate::scrub::{Environment, Scratch};
use crate::tool::Outcome;

/// The most the ballast may hold before the volume is judged not capped: a volume this large is not
/// the small dedicated one the fault needs, and filling it would fill a real disk.
pub const MAX_BALLAST: u64 = 2 << 30;

/// What the free space of a capped volume is, measured by filling it.
#[derive(Debug, PartialEq, Eq)]
pub enum FillError {
    /// The ballast reached [`MAX_BALLAST`] without the volume refusing a write.
    NotCapped,
    Io(String),
}

impl std::fmt::Display for FillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FillError::NotCapped => write!(f, "{} bytes written and the volume is not full: not a capped volume", MAX_BALLAST),
            FillError::Io(error) => write!(f, "{}", error),
        }
    }
}

/// Fills the volume holding `ballast` until it refuses a write, then gives back `leave` bytes of
/// what the ballast took; returns what the ballast held at the full point. Stops at `cap` bytes.
pub fn fill(ballast: &Path, leave: u64, cap: u64) -> Result<u64, FillError> {
    let io = |error: std::io::Error| FillError::Io(format!("{}: {}", ballast.display(), error));
    let file = std::fs::OpenOptions::new().write(true).create_new(true).open(ballast).map_err(io)?;
    let filled = fill_file(file, leave, cap).map_err(|error| match error {
        Some(error) => io(error),
        None => FillError::NotCapped,
    });
    // A fill that failed leaves no ballast behind: a full volume would fail every later fault.
    if filled.is_err() {
        let _ = damage::apply(ballast, Kind::Delete, 0);
    }
    filled
}

/// What [`fill`] does with the ballast once made: an error, or None at the cap.
fn fill_file(mut file: std::fs::File, leave: u64, cap: u64) -> Result<u64, Option<std::io::Error>> {
    // Large writes to reach the full point fast, then small ones to take what a large one left.
    for chunk in [1 << 20, 4 << 10, 512] {
        let block = vec![0xa5u8; chunk];
        loop {
            if file.metadata()?.len() >= cap {
                return Err(None);
            }
            match file.write_all(&block).and_then(|()| file.flush()) {
                Ok(()) => {}
                Err(error) if is_full(&error) => break,
                Err(error) => return Err(Some(error)),
            }
        }
    }
    let held = file.metadata()?.len();
    file.set_len(held.saturating_sub(leave))?;
    file.sync_all()?;
    Ok(held)
}

/// Whether a write failed because the volume is full: ENOSPC, or a quota the volume enforces.
fn is_full(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded) || error.raw_os_error() == Some(28)
}

/// The free-space points a disk-full fault runs at, given what the faulted command's lines consume
/// net (`consumed`): none left, then a quarter, a half and three quarters of what they need, each
/// once.
pub fn points(consumed: u64) -> Vec<u64> {
    let mut points: Vec<u64> = [0, consumed / 4, consumed / 2, consumed * 3 / 4].to_vec();
    points.dedup();
    points
}

/// One environment fault and what grading it established.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Lifted {
    pub scenario: String,
    /// The fault: `free <n> B` or `read-only`.
    pub condition: String,
    pub graded: Graded,
}

/// Why a fault was refused as not run: the faulted command landed every line under it.
pub const NOT_SET: &str = "the faulted command landed every line with the condition set, so it did not take";

/// Builds `scenario` and grades its faulted command with a condition set: `set` runs between the
/// snapshot and the lines and returns the lift, which runs once the lines end and before anything
/// is read back. `lift` names the lift as the outside-knowledge step it is.
#[allow(clippy::too_many_arguments)]
fn under(
    scenario: &Scenario, corpus: &Corpus, programs: &Programs, scratch: &Scratch, environment: &Environment, limit: Duration,
    lift: &str, set: impl FnOnce(&Context) -> Result<Box<dyn FnOnce() -> Result<(), RunError>>, RunError>,
) -> Result<Graded, RunError> {
    let built = runner::build(scenario, corpus, Tool::Forklift, &programs.forklift, programs.server.as_deref(), scratch, environment, limit, None)?;
    let corpus = &scenario.corpus(corpus);
    let (expected, index) = faulted::expected(scenario, corpus, &built, Warehouse::Client)?;
    let context = Context {
        corpus, programs, scratch, environment, limit, then: &scenario.then, url: built.host.as_ref().map(|host| host.url.as_str()),
        scope: scenario.scope(), via_host: false,
    };
    let mut graded = faulted::grade_after(&context, &expected, index, |context, steps: &mut Vec<Outcome>| {
        context.write_snapshot(index)?;
        let lift = set(context)?;
        let made = context.run_lines(index, steps);
        lift()?;
        // Every point is set short of what the lines need, and they write into the warehouse:
        // lines that all landed under the condition mean it did not take, and grading them would
        // claim a recovery from a fault that never happened.
        if made? {
            return Err(RunError::Io(NOT_SET.to_string()));
        }
        Ok(false)
    })?;
    graded.recoverability.outside_knowledge.insert(0, lift.to_string());
    Ok(graded)
}

/// Runs `scenario`'s faulted command once without a fault on `scratch`, on the capped volume, and
/// measures by filling it what its lines consume: the most free space they had taken after any one
/// line, the snapshot already written. A repack that frees space by its end has still consumed what
/// its record took first. Measured between lines only: a peak inside one line, a temporary file
/// it removes before exiting, is not seen.
pub fn consumed(scenario: &Scenario, corpus: &Corpus, programs: &Programs, scratch: &Scratch, environment: &Environment, limit: Duration, ballast: &Path) -> Result<u64, String> {
    let built = runner::build(scenario, corpus, Tool::Forklift, &programs.forklift, programs.server.as_deref(), scratch, environment, limit, None)
        .map_err(|error| error.to_string())?;
    let corpus = &scenario.corpus(corpus);
    let (_, index) = faulted::expected(scenario, corpus, &built, Warehouse::Client).map_err(|error| error.to_string())?;
    let context = Context {
        corpus, programs, scratch, environment, limit, then: &scenario.then, url: built.host.as_ref().map(|host| host.url.as_str()),
        scope: scenario.scope(), via_host: false,
    };
    context.write_snapshot(index).map_err(|error| error.to_string())?;
    let free = || free_space(ballast);
    let before = free()?;
    let mut deepest = 0;
    for arguments in context.lines(index).map_err(|error| error.to_string())? {
        let outcome = context.run(&programs.forklift, arguments, scratch.work()).map_err(|error| error.to_string())?;
        if !outcome.succeeded() {
            return Err(format!("the faulted command refused with no fault set: {}", outcome.arguments.join(" ")));
        }
        deepest = deepest.max(before.saturating_sub(free()?));
    }
    // Every faulted command writes at least its record: nothing measured is a measurement gone
    // wrong, and grading its one point as the whole sample would claim more than was run.
    if deepest == 0 {
        return Err("the faulted command measured as taking no free space, though it writes its record: the volume's free space is not a measure here".to_string());
    }
    Ok(deepest)
}

/// The free space of the volume holding `ballast`, measured by filling it, once it has settled.
fn free_space(ballast: &Path) -> Result<u64, String> {
    settled(|| {
        let held = fill(ballast, 0, MAX_BALLAST).map_err(|error| error.to_string())?;
        damage::apply(ballast, Kind::Delete, 0).map_err(|error| format!("{:?}", error))?;
        Ok(held)
    })
}

/// How many readings of the free space [`settled`] takes before giving up on two agreeing.
const SETTLE_READS: usize = 8;

/// A free-space reading taken until two in a row agree: a volume can give back what was removed
/// from it a while after the removal (HFS+ after a scratch area of an earlier fault went), and a
/// reading taken meanwhile is low.
fn settled(mut read: impl FnMut() -> Result<u64, String>) -> Result<u64, String> {
    let mut last = read()?;
    for _ in 1..SETTLE_READS {
        std::thread::sleep(Duration::from_millis(200));
        let next = read()?;
        if next == last {
            return Ok(next);
        }
        last = next;
    }
    Err(format!("the free space did not settle over {} readings", SETTLE_READS))
}

/// The disk-full fault: `free` bytes left on the capped volume when the faulted command's lines
/// start, the ballast removed once they end.
#[allow(clippy::too_many_arguments)]
pub fn disk_full(scenario: &Scenario, corpus: &Corpus, programs: &Programs, scratch: &Scratch, environment: &Environment, limit: Duration, ballast: &Path, free: u64) -> Result<Lifted, RunError> {
    let graded = under(scenario, corpus, programs, scratch, environment, limit, "free space on the volume (the harness removed its ballast)", |_| {
        // Space an earlier area gave back late would arrive after the fill and leave the point
        // with room it was not set to have: fill only once the volume has settled.
        free_space(ballast).map_err(RunError::Io)?;
        fill(ballast, free, MAX_BALLAST).map_err(|error| RunError::Io(error.to_string()))?;
        let ballast = ballast.to_path_buf();
        Ok(Box::new(move || damage::apply(&ballast, Kind::Delete, 0).map_err(|error| RunError::Io(format!("lifting the fault: {:?}", error)))))
    })?;
    Ok(Lifted { scenario: scenario.id.clone(), condition: format!("free {} B", free), graded })
}

/// Why a read-only fault did not run: the work folder took a write with its write bits cleared.
pub const STAYS_WRITABLE: &str = "the work folder still takes a write with its write bits cleared (run as root, or folder attributes that refuse nothing)";

/// The read-only fault, or None when clearing the write bits does not refuse a write here.
pub fn read_only(scenario: &Scenario, corpus: &Corpus, programs: &Programs, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<Option<Lifted>, RunError> {
    let mut writable = false;
    let graded = under(scenario, corpus, programs, scratch, environment, limit, "make the work folder writable again", |context| {
        let work = context.scratch.work();
        let saved = freeze(&work).map_err(|error| RunError::Io(format!("{}: {}", work.display(), error)))?;
        writable = takes_a_write(&work) || takes_a_write(&work.join(".forklift"));
        if writable {
            // Not a fault: restore and stop before the faulted command runs.
            thaw(&saved).map_err(|error| RunError::Io(error.to_string()))?;
            return Err(RunError::Io(STAYS_WRITABLE.to_string()));
        }
        Ok(Box::new(move || thaw(&saved).map_err(|error| RunError::Io(format!("lifting the fault: {}", error)))))
    });
    match graded {
        Err(_) if writable => Ok(None),
        Err(error) => Err(error),
        Ok(graded) => Ok(Some(Lifted { scenario: scenario.id.clone(), condition: "read-only".to_string(), graded })),
    }
}

/// Clears the write bits of `root` and everything under it, symlinks untouched, and returns what
/// each mode was.
pub fn freeze(root: &Path) -> std::io::Result<Vec<(PathBuf, std::fs::Permissions)>> {
    let mut saved = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            for entry in std::fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        }
        saved.push((path, metadata.permissions()));
    }
    // Everything is listed above before any write bit goes, so the order here does not matter.
    for (done, (path, permissions)) in saved.iter().enumerate() {
        let mut frozen = permissions.clone();
        frozen.set_readonly(true);
        if let Err(error) = std::fs::set_permissions(path, frozen) {
            // Half frozen is no fault to grade and no state to leave: restore what changed.
            let _ = thaw(&saved[..done]);
            return Err(std::io::Error::new(error.kind(), format!("{}: {}", path.display(), error)));
        }
    }
    Ok(saved)
}

/// Restores every mode `freeze` saved, reporting the first it could not.
pub fn thaw(saved: &[(PathBuf, std::fs::Permissions)]) -> std::io::Result<()> {
    let mut first = Ok(());
    for (path, permissions) in saved.iter().rev() {
        if let Err(error) = std::fs::set_permissions(path, permissions.clone()) {
            if first.is_ok() {
                first = Err(std::io::Error::new(error.kind(), format!("{}: {}", path.display(), error)));
            }
        }
    }
    first
}

/// Whether a new file can be made in `folder`; the probe leaves nothing behind.
fn takes_a_write(folder: &Path) -> bool {
    let probe = folder.join(".proof-load-probe");
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(file) => {
            drop(file);
            let _ = damage::apply(&probe, Kind::Delete, 0);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("proof-load-conditions-{}-{}-{}", label, std::process::id(), nanos));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A reading counts once two in a row agree; one that keeps moving is refused.
    ///
    /// Revert: take the first reading → a volume still giving space back reads low.
    #[test]
    fn a_free_space_reading_waits_for_two_that_agree() {
        let mut readings = [100, 140, 160, 160, 160].into_iter();
        assert_eq!(settled(|| Ok(readings.next().unwrap())), Ok(160));
        let mut moving = 0;
        assert!(settled(|| { moving += 1; Ok(moving) }).is_err());
    }

    /// The points are none left and three fractions of what the command needs, each once.
    ///
    /// Revert: drop the dedup → a command that consumes nothing runs the same point four times.
    #[test]
    fn the_points_spread_over_what_the_command_consumes() {
        assert_eq!(points(4096), vec![0, 1024, 2048, 3072]);
        assert_eq!(points(0), vec![0]);
        assert_eq!(points(2), vec![0, 1]);
    }

    /// On a volume that never fills, the ballast stops at the cap, is removed, and the volume is
    /// reported not capped, so no real disk is filled.
    ///
    /// Revert: drop the cap check → the fill writes until the real disk is full.
    #[test]
    fn an_uncapped_volume_is_refused_at_the_cap() {
        let dir = fresh("uncapped");
        let ballast = dir.join("ballast");
        assert_eq!(fill(&ballast, 0, 3 << 20), Err(FillError::NotCapped));
        assert!(std::fs::symlink_metadata(&ballast).is_err());
    }

    /// Clearing the write bits refuses a new file and an overwrite, and the lift restores every
    /// mode as it was. Skipped where the bits refuse nothing (root), as the lane is.
    ///
    /// Revert: thaw nothing → the folder stays unwritable; freeze only the root → a file under it
    /// still takes an overwrite.
    #[test]
    fn freeze_refuses_writes_and_thaw_restores_them() {
        let dir = fresh("freeze");
        std::fs::create_dir_all(dir.join("w/.forklift/objects")).unwrap();
        std::fs::write(dir.join("w/.forklift/objects/a"), b"a").unwrap();
        let before = std::fs::metadata(dir.join("w/.forklift/objects/a")).unwrap().permissions();
        let saved = freeze(&dir.join("w")).unwrap();
        if takes_a_write(&dir.join("w")) {
            thaw(&saved).unwrap();
            eprintln!("skipped: {}", STAYS_WRITABLE);
            return;
        }
        assert!(!takes_a_write(&dir.join("w/.forklift/objects")));
        assert!(std::fs::OpenOptions::new().write(true).open(dir.join("w/.forklift/objects/a")).is_err());
        thaw(&saved).unwrap();
        assert!(takes_a_write(&dir.join("w/.forklift/objects")));
        assert_eq!(std::fs::metadata(dir.join("w/.forklift/objects/a")).unwrap().permissions(), before);
        std::fs::write(dir.join("w/.forklift/objects/a"), b"b").unwrap();
    }
}
