//! Volume faults: a work tree on a volume that folds case, or keeps no executable bit (exFAT, vfat).
//!
//! Only the work tree goes there, and only from the clone on: the history before it is recorded on
//! the scratch volume, since a file first tracked on a volume without the bit is recorded without
//! one, and the readback lays each record out under the scratch root, since a volume without the
//! bit reads every file the same and would lose what is being judged. The clone moves the work tree
//! ([`Scratch::move_work`]); everything after it, the faulted command, retry, remedies and heal,
//! runs there.
//!
//! These are standing conditions, not lifted ones. The other environment faults are lifted before
//! the retry that grades them (see `conditions`), because a user can free space or restore write
//! access; nobody lifts the file system their checkout lives on, so here the retry, every remedy
//! and heal run on the same volume, and what is graded is whether the tool records correctly there
//! at all.
//!
//! What the volume is, is measured, never assumed: a probe writes a file and reads it back under
//! another case, and sets the executable bit both off and on, as Forklift's own probe does, since
//! a volume that reads every file executable would pass a one-way check. A cell runs only where
//! its property holds, so an ordinary folder passed by mistake can never report coverage.

use std::path::Path;
use std::time::Duration;

use crate::corpus::Corpus;
use crate::damage::{self, Kind, Warehouse};
use crate::faulted::{self, Context, Graded, Programs};
use crate::report::Category;
use crate::runner::{self, RunError};
use crate::scenario::{Scenario, Tool};
use crate::scrub::{Environment, Scratch};

/// What the probe found the folding volume to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Probed {
    /// A file written under one case is found under another.
    pub folds_case: bool,
    /// The executable bit reads back off after it is cleared and on after it is set.
    pub keeps_executable_bit: bool,
}

/// Probes the empty folder `folder` for both properties, leaving nothing behind. Fails loudly on
/// anything the probe cannot judge.
pub fn probe(folder: &Path) -> Result<Probed, String> {
    fn io(path: &Path) -> impl FnOnce(std::io::Error) -> String + '_ {
        move |error| format!("probing {}: {}", path.display(), error)
    }
    let upper = folder.join("PROOF-LOAD-PROBE");
    std::fs::OpenOptions::new().write(true).create_new(true).open(&upper).map_err(io(&upper))?;
    let lower = folder.join("proof-load-probe");
    let folds_case = std::fs::symlink_metadata(&lower).is_ok();
    let keeps = keeps_executable_bit(&upper);
    damage::apply(&upper, Kind::Delete, 0).map_err(|error| format!("removing the probe {}: {:?}", upper.display(), error))?;
    Ok(Probed { folds_case, keeps_executable_bit: keeps.map_err(io(&upper))? })
}

/// Whether `file`'s volume keeps the executable bit, measured as [`keeps_bit_when`] says.
fn keeps_executable_bit(file: &Path) -> std::io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        keeps_bit_when(
            |mode| std::fs::set_permissions(file, std::fs::Permissions::from_mode(mode)),
            || std::fs::symlink_metadata(file).map(|metadata| crate::oracle::executable_when(true, crate::oracle::mode(&metadata))),
        )
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "no executable bit is probed on this platform"))
    }
}

/// The two-way bit probe over `set` (change the mode) and `read` (whether the file now reads
/// executable): mode 0644 must read not executable and then 0755 executable. A change of mode the
/// volume refuses as not permitted or not supported means it keeps no bit (vfat answers EPERM);
/// any other error is no answer and fails the probe.
pub fn keeps_bit_when(mut set: impl FnMut(u32) -> std::io::Result<()>, mut read: impl FnMut() -> std::io::Result<bool>) -> std::io::Result<bool> {
    for (mode, executable) in [(0o644, false), (0o755, true)] {
        match set(mode) {
            Ok(()) => {}
            Err(error) if matches!(error.kind(), std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported) => return Ok(false),
            Err(error) => return Err(error),
        }
        if read()? != executable {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Why the cell of volume fault `category` is not run, given what the probe found; None where it
/// runs.
pub fn not_run_because(probed: &Probed, category: Category) -> Option<&'static str> {
    match category {
        Category::CaseFolding if !probed.folds_case => Some("the folding volume given does not fold case"),
        Category::NoExec if probed.keeps_executable_bit => Some("the folding volume given keeps the executable bit"),
        Category::CaseFolding | Category::NoExec => None,
        _ => Some("not a volume fault"),
    }
}

/// Builds `scenario` with its clone on the folding volume `scratch` names, and grades its faulted
/// command there: the readback, one plain retry, the remedies, and heal, the volume standing
/// throughout. A history whose clone did not move the work tree graded nothing on the volume, and
/// fails rather than passing as a run.
pub fn folded(scenario: &Scenario, corpus: &Corpus, programs: &Programs, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<Graded, RunError> {
    let built = runner::build(scenario, corpus, Tool::Forklift, &programs.forklift, programs.server.as_deref(), scratch, environment, limit, None)?;
    if !scratch.work_moved() {
        return Err(RunError::Io(format!("scenario {} built without moving its work tree onto the folding volume: no clone moved it", scenario.id)));
    }
    let corpus = &scenario.corpus(corpus);
    let (expected, index) = faulted::expected(scenario, corpus, &built, Warehouse::Client)?;
    let context = Context {
        corpus, programs, scratch, environment, limit, then: &scenario.then, url: built.host.as_ref().map(|host| host.url.as_str()),
        scope: scenario.scope(), via_host: false,
    };
    faulted::grade_after(&context, &expected, index, |context, steps| context.record(index, steps))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn refused(kind: std::io::ErrorKind) -> std::io::Error {
        std::io::Error::new(kind, "refused")
    }

    /// The bit probe reads both directions: a volume reading every file executable keeps no bit,
    /// though setting the bit "works" there; one refusing the change as not permitted keeps none;
    /// any other error is no answer.
    ///
    /// Revert: probe only that 0755 reads executable → the always-executable volume (exFAT) is
    /// called one that keeps the bit, and the no-exec cell never runs; count every error as no bit
    /// → a failing disk passes as a no-exec volume.
    #[test]
    fn the_bit_probe_reads_both_directions() {
        let modes = |answers: &[bool]| {
            let answers = RefCell::new(answers.to_vec());
            move || Ok(answers.borrow_mut().remove(0))
        };
        assert!(keeps_bit_when(|_| Ok(()), modes(&[false, true])).unwrap(), "an ordinary volume");
        assert!(!keeps_bit_when(|_| Ok(()), modes(&[true, true])).unwrap(), "every file executable: exFAT");
        assert!(!keeps_bit_when(|_| Ok(()), modes(&[false, false])).unwrap(), "no file executable");
        assert!(!keeps_bit_when(|_| Err(refused(std::io::ErrorKind::PermissionDenied)), modes(&[])).unwrap(), "vfat answers EPERM");
        assert!(!keeps_bit_when(|_| Err(refused(std::io::ErrorKind::Unsupported)), modes(&[])).unwrap());
        assert!(keeps_bit_when(|_| Err(refused(std::io::ErrorKind::Other)), modes(&[])).is_err(), "no answer is not an answer");
        assert!(keeps_bit_when(|_| Ok(()), || Err(refused(std::io::ErrorKind::NotFound))).is_err());
    }

    /// Each cell runs only where its property holds, so an ordinary folder passed by mistake
    /// reports neither.
    ///
    /// Revert: run the case-folding cell whatever the probe said → a case-sensitive folder
    /// reports case-folding coverage it never had.
    #[test]
    fn a_cell_runs_only_where_its_property_holds() {
        let volume = |folds_case, keeps_executable_bit| Probed { folds_case, keeps_executable_bit };
        let (case, exec) = (Category::CaseFolding, Category::NoExec);
        assert_eq!(not_run_because(&volume(true, false), case), None, "exFAT");
        assert_eq!(not_run_because(&volume(true, false), exec), None);
        assert!(not_run_because(&volume(false, true), case).is_some(), "ext4");
        assert!(not_run_because(&volume(false, true), exec).is_some());
        assert_eq!(not_run_because(&volume(true, true), case), None, "APFS folds case and keeps the bit");
        assert!(not_run_because(&volume(true, true), exec).is_some());
        assert!(not_run_because(&volume(true, false), Category::DiskFull).is_some());
    }

    /// The probe on the test's own temp folder answers, leaves the folder empty, and on Unix
    /// finds the bit kept, as every volume the tests run on does.
    #[cfg(unix)]
    #[test]
    fn the_probe_leaves_nothing_behind() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let folder = std::env::temp_dir().join(format!("proof-load-probe-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(&folder).unwrap();
        let probed = probe(&folder).unwrap();
        assert!(probed.keeps_executable_bit);
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 0);
    }
}
