//! The faulted run for damaged files: build a scenario's history, damage one path a binding's
//! census name resolves to, make the next record as the faulted command, then read every record
//! back through the tool into fresh folders and grade what came back.
//!
//! The readback is `forklift git export` into a fresh folder, then a git checkout of each commit
//! whose subject is a record's message into a folder of its own. Export is the tool reading its
//! own history whole; git only lays out the bytes forklift handed it, so a wrong byte in the
//! readback is one the tool served.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::corpus::{self, Corpus};
use crate::damage::{self, Kind, Warehouse};
use crate::oracle::{self, Admissible, Evidence, Expectation, Grade, Readback, Recoverability, Rescue, Verdict};
use crate::remedy::{self, Remedy};
use crate::runner::{self, RunError, Taken, TakenBy};
use crate::scenario::{bind, Mark, Op, Scenario, Tool};
use crate::scrub::{Environment, Scratch};
use crate::tool::{self, Invocation, Outcome};

/// The programs a faulted run drives, each by absolute path.
#[derive(Clone, Debug)]
pub struct Programs {
    pub forklift: PathBuf,
    /// The git the harness lays out exported commits with.
    pub git: PathBuf,
    /// The forklift-server a scenario's host runs; a scenario with a host is not built without it.
    pub server: Option<PathBuf>,
}

impl Programs {
    /// The program `tool` runs as: forklift, or git at its defaults or tuned.
    pub fn of(&self, tool: Tool) -> &Path {
        match tool {
            Tool::Forklift => &self.forklift,
            Tool::Git | Tool::GitTuned => &self.git,
        }
    }
}

/// The concrete path, relative to the state folder, that `pattern` resolves to in a built
/// warehouse: one of the walked paths it matches, chosen by `seed` so a corpus of seeds spreads
/// the damage over every match. None when nothing matches.
pub fn resolve(pattern: &str, walked: &[String], seed: u64) -> Option<String> {
    let matching: Vec<&String> = walked.iter().filter(|path| crate::census::matches(pattern, path)).collect();
    if matching.is_empty() {
        return None;
    }
    Some(matching[(seed % matching.len() as u64) as usize].trim_end_matches('/').to_string())
}

/// What one record read back as.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RecordReadback {
    pub mark: Mark,
    /// One verdict per exported commit carrying the record's message; empty when none did.
    pub verdicts: Vec<Verdict>,
}

/// One readback of the whole history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryReadback {
    /// The readback's own command: the export, or in a scoped bay the opening of the side bay it
    /// reads through. A refusal here refuses every record.
    pub export: Outcome,
    pub records: Vec<RecordReadback>,
    /// The parked work read back, when the history left work parked.
    pub parked: Option<ParkedReadback>,
    /// Over a real history, the commands that resolved and laid out each sampled record, in order;
    /// a refusal here refuses the record it ran for. Forklift's bay for a record opens at the
    /// current head before moving to the record, so damage only that head holds refuses every
    /// forklift record, where git still serves the records that do not hold it.
    pub reads: Vec<Outcome>,
}

/// Written with each successful read's stdout left out, its length kept, unless the readback is
/// silent: over a real history those bytes ran to some 100 MB a readback (CI, 2026-11-16), so a
/// run's details came to 3.1 GB. A refused read is kept whole, and so is every read of a silent
/// readback, whose served bytes are what shows the wrong tree.
impl serde::Serialize for HistoryReadback {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        struct Written<'a> {
            export: &'a Outcome,
            records: &'a [RecordReadback],
            #[serde(skip_serializing_if = "Option::is_none")]
            parked: Option<&'a ParkedReadback>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            reads: Vec<Read<'a>>,
        }
        #[derive(serde::Serialize)]
        struct Read<'a> {
            arguments: &'a [String],
            exit: Option<i32>,
            #[serde(skip_serializing_if = "Option::is_none")]
            stdout: Option<&'a [u8]>,
            #[serde(skip_serializing_if = "Option::is_none")]
            stdout_bytes: Option<usize>,
            stderr: &'a [u8],
            failure: &'a Option<tool::Failure>,
        }
        // Destructured whole, so a field added to either is a compile error here, not one the
        // details silently leave out.
        let HistoryReadback { export, records, parked, reads } = self;
        let silent = self.silent();
        let reads = reads.iter().map(|read| {
            let Outcome { arguments, exit, stdout, stderr, failure } = read;
            let kept = silent || !read.succeeded();
            Read {
                arguments, exit: *exit, stdout: kept.then_some(stdout.as_slice()),
                stdout_bytes: (!kept).then_some(stdout.len()), stderr, failure,
            }
        }).collect();
        Written { export, records, parked: parked.as_ref(), reads }.serialize(serializer)
    }
}

/// What `park list` said of the work the history left parked.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ParkedReadback {
    pub listing: Outcome,
    /// The parked parcels the listing did not name; all of them when it refused.
    pub missing: Vec<String>,
    /// Whether the listing owes every park, so leaving one out with exit 0 serves it as missing,
    /// which is silent. Not in a fresh clone, which holds only what reached the remote: a park it
    /// lacks is lost.
    pub owed: bool,
}

impl HistoryReadback {
    fn refused(&self) -> bool {
        !self.export.succeeded() || self.parked.as_ref().is_some_and(|parked| !parked.listing.succeeded())
    }

    /// Any record served wrong with exit 0, or parked work a listing that exited 0 left out.
    fn silent(&self) -> bool {
        self.records.iter().any(|record| record.verdicts.contains(&Verdict::Silent))
            || self.parked.as_ref().is_some_and(|parked| parked.owed && parked.listing.succeeded() && !parked.missing.is_empty())
    }

    /// Every record in `wanted` served, and served as expected, and the work parked still listed.
    fn reads(&self, wanted: &[Mark]) -> bool {
        !self.refused() && self.parked.as_ref().is_none_or(|parked| parked.missing.is_empty())
            && wanted.iter().all(|mark| self.records.iter().any(|record| record.mark == *mark
            && !record.verdicts.is_empty() && record.verdicts.iter().all(|verdict| *verdict == Verdict::Expected)))
    }
}

/// What a faulted run did and the grade it earned.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Faulted {
    pub scenario: String,
    pub name: String,
    pub warehouse: Warehouse,
    pub kind: Kind,
    /// The damaged path, relative to the state folder of `warehouse`.
    pub target: String,
    pub grade: Grade,
    pub evidence: Evidence,
    pub recoverability: Recoverability,
    /// Every command after the fault, in order: the faulted command, retries, remedies, the
    /// subject's own remedy set.
    pub steps: Vec<Outcome>,
    /// Every readback taken, in order, except that a settle's own follows the acknowledged one it
    /// took and did not settle by: a settle's last readback is the one it judged by.
    pub readbacks: Vec<HistoryReadback>,
    /// The harness's reads that decided the own remedy set ([`Graded::probes`]).
    pub probes: Vec<Outcome>,
    /// Remedies named in a form the harness does not run.
    pub unextractable: Vec<String>,
    /// Over a real history, the whole-store check that found damage the sample did not reach,
    /// once every sampled record read back as expected: "damage not reached by the sample".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unreached: Option<Outcome>,
}

/// The scenario every damage row over a real history names, apart from the fixture scenarios', so
/// a row over the history never pairs with a fixture row.
pub const HISTORY_SCENARIO: &str = "real-history";

/// The ref each history subject records the probe on: a git branch, a forklift pallet, made at the
/// history's current tip and current before the pristine subject is copied.
pub const PROBE_REF: &str = "proof-load-probe";
/// The file the probe record writes at the work tree's root.
pub const PROBE_FILE: &str = "proof-load-probe";
/// What the probe file holds.
pub const PROBE_BYTES: &[u8] = b"proof-load probe\n";

/// A record of a real history named by position: a ref, as a git branch name that is also the
/// forklift pallet's, and a first-parent depth from its tip (0 is the tip).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct Position {
    pub reference: String,
    pub depth: usize,
}

/// A history readback before judging: the readback's own command; per mark, what it read back
/// as, whether the subject reached it (a forklift walk that exits 0 short of the depth reads as
/// served empty, which is not reaching it), and the indexes of the reads that read that mark;
/// and the commands that read them.
type Served = (Outcome, Vec<(Mark, Readback, bool, Vec<usize>)>, Vec<Outcome>);

/// The parts of a faulted run that stay fixed across its steps.
pub(crate) struct Context<'a> {
    /// The subject the faulted command, its retries and its readback run as.
    pub(crate) tool: Tool,
    pub(crate) corpus: &'a Corpus,
    pub(crate) programs: &'a Programs,
    pub(crate) scratch: &'a Scratch,
    pub(crate) environment: &'a Environment,
    pub(crate) limit: Duration,
    /// What the faulted command runs after its record.
    pub(crate) then: &'a [Op],
    /// The host's URL, when the scenario serves one.
    pub(crate) url: Option<&'a str>,
    /// The folder the work tree is scoped to, when a sparse clone left it scoped.
    pub(crate) scope: Option<&'a str>,
    /// Read the history back through a fresh clone of the host rather than from the work tree:
    /// the damage is the host's.
    pub(crate) via_host: bool,
    /// Over a real history: the sampled positions every readback reads, by [`Mark::Position`]
    /// index. The faulted command then writes the probe file alone and records it on [`PROBE_REF`],
    /// which reads back as the faulted record, [`Mark::Record`] 0.
    pub(crate) history: Option<&'a [Position]>,
}

impl Context<'_> {
    /// `arguments` as the subject under test runs them: git tuned with its configuration first,
    /// as every command of its own is bound.
    fn subject(&self, arguments: Vec<String>) -> Vec<String> {
        match self.tool {
            Tool::GitTuned => crate::scenario::GIT_TUNED.iter().map(|argument| argument.to_string()).chain(arguments).collect(),
            Tool::Forklift | Tool::Git => arguments,
        }
    }

    /// The binary of the subject under test.
    pub(crate) fn program(&self) -> &Path {
        self.programs.of(self.tool)
    }

    pub(crate) fn run(&self, program: &Path, arguments: Vec<String>, directory: PathBuf) -> Result<Outcome, RunError> {
        let invocation = Invocation { program: program.to_path_buf(), arguments, directory, limit: self.limit };
        tool::run(&invocation, self.scratch, self.environment).map_err(RunError::Tool)
    }

    /// A folder in the scratch area that does not exist yet.
    fn fresh(&self, label: &str) -> PathBuf {
        let mut n = 0;
        loop {
            let path = self.scratch.root().join(format!("{}-{}", label, n));
            if std::fs::symlink_metadata(&path).is_err() {
                return path;
            }
            n += 1;
        }
    }

    /// The faulted command: corpus record `index` written into the work tree, then the record
    /// operation's lines and those of the scenario's `then` run in order until one fails.
    pub(crate) fn record(&self, index: usize, steps: &mut Vec<Outcome>) -> Result<bool, RunError> {
        self.write_snapshot(index)?;
        self.run_lines(index, steps)
    }

    /// The faulted command retried at line `from`, the one an earlier attempt did not land: the
    /// snapshot written again, as the user's files still hold it, then the lines from `from` on.
    /// Says whether every line landed, and where the next retry resumes.
    fn retry_from(&self, index: usize, from: Resume, steps: &mut Vec<Outcome>) -> Result<(bool, Resume), RunError> {
        self.write_snapshot(index)?;
        let lines = self.lines(index)?;
        resume_lines(lines, from, steps, |arguments| self.run(self.program(), arguments, self.scratch.work()))
    }

    /// The faulted command's lines for corpus record `index`, run in order until one fails, its
    /// snapshot already written: an environment fault is set between the two.
    pub(crate) fn run_lines(&self, index: usize, steps: &mut Vec<Outcome>) -> Result<bool, RunError> {
        for arguments in self.lines(index)? {
            let outcome = self.run(self.program(), arguments, self.scratch.work())?;
            let succeeded = outcome.succeeded();
            steps.push(outcome);
            if !succeeded {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Writes corpus record `index` into the work tree, as the faulted command does first.
    pub(crate) fn write_snapshot(&self, index: usize) -> Result<(), RunError> {
        // Over a real history the probe file is written beside the history's own files, which stay.
        if self.history.is_some() {
            let probe = self.scratch.work().join(PROBE_FILE);
            return std::fs::write(&probe, PROBE_BYTES).map_err(|error| RunError::Io(format!("{}: {}", probe.display(), error)));
        }
        let snapshot = &self.corpus.records[index];
        let snapshot = match self.scope { Some(scope) => corpus::within(snapshot, scope), None => snapshot.clone() };
        corpus::write(&snapshot, &self.corpus.modes, &self.scratch.work(), self.scratch.work_moved()).map_err(|error| RunError::Io(error.to_string()))
    }

    /// The command lines of the faulted command for corpus record `index`: the record operation's,
    /// then those of the scenario's `then`, in the order they run.
    pub(crate) fn lines(&self, index: usize) -> Result<Vec<Vec<String>>, RunError> {
        let mut lines = Vec::new();
        for op in std::iter::once(&Op::Record { index }).chain(self.then) {
            let bound = bind(self.tool, op).ok_or_else(|| RunError::Io(format!("{:?} has no {:?} binding", op, self.tool)))?;
            for arguments in bound {
                lines.push(runner::fill(arguments, self.url, &self.scratch.work())
                    .map_err(|placeholder| RunError::Io(format!("{:?} needs {} and the scenario hosts nothing", op, placeholder)))?);
            }
        }
        Ok(lines)
    }

    /// How many of the faulted command's leading lines acknowledge its record, as
    /// [`acknowledging`] counts them for this run's `then` and readback.
    pub(crate) fn acknowledging(&self) -> Result<usize, RunError> {
        acknowledging(self.tool, self.then, self.via_host).map_err(RunError::Io)
    }

    /// Reads every record in `marks` back through the tool, from `bay`: the work tree, or the fresh
    /// clone the own remedy set made in its place. `expectation` gives what each may hold; a record
    /// in `required` that an export exiting 0 leaves out was served as missing, which is silent.
    fn read_back(&self, bay: &Path, marks: &[Mark], required: &[Mark], expectation: impl Fn(Mark) -> Expectation) -> Result<HistoryReadback, RunError> {
        if let Some(positions) = self.history {
            return self.read_back_history(bay, positions, marks, expectation);
        }
        if self.tool != Tool::Forklift {
            return self.read_back_git(bay, marks, required, expectation);
        }
        if let Some(scope) = self.scope {
            return self.read_back_scoped(bay, scope, marks, required, |mark| within(expectation(mark), scope));
        }
        // The warehouse read: the work tree's, or a fresh clone of the host. A clone that fails
        // refuses every record, as a failed export does.
        let bay = if self.via_host {
            let url = self.url.ok_or_else(|| RunError::Io("a host-side readback needs a host".to_string()))?;
            let clone = self.fresh("clone");
            let cloned = self.run(&self.programs.forklift, vec!["--json".into(), "clone".into(), url.to_string(), clone.display().to_string()], self.scratch.root().to_path_buf())?;
            if !cloned.succeeded() {
                return Ok(HistoryReadback { export: cloned, records: Vec::new(), parked: None, reads: Vec::new() });
            }
            clone
        } else {
            bay.to_path_buf()
        };
        let exported = self.fresh("readback");
        let export = self.run(&self.programs.forklift, vec!["--json".into(), "git".into(), "export".into(), exported.display().to_string()], bay.clone())?;
        if !export.succeeded() {
            return Ok(HistoryReadback { export, records: Vec::new(), parked: None, reads: Vec::new() });
        }
        // The listing names the records the export left files out of, so it is read only when the
        // export left something out. When it refuses, the records go unnamed, and each file the
        // export documents leaving out is admitted as documented in every record whose
        // expectation opens that path: the tool did tell the user, and the bytes it did serve are
        // still judged. A record that never held those bytes there gets no entry, so a correct
        // read of it is not taken for a silent one.
        let document = runner::document(&export.stdout);
        let left_out = if leaves_out(&document) {
            let listing = self.run(&self.programs.forklift, vec!["--json".into(), "history".into()], bay)?;
            if listing.succeeded() {
                left_out(&document, &runner::described_parcels(&listing.stdout))
            } else {
                where_transformed(left_out(&document, &in_every_record(&document, marks)), marks, &expectation)
            }
        } else {
            Default::default()
        };

        let git = |arguments: &[&str]| self.run(&self.programs.git, arguments.iter().map(|argument| argument.to_string()).collect(), exported.clone());
        let log = git(&["log", "--all", "--format=%H%x09%s"])?;
        // An export that exits 0 but leaves no readable repository served nothing.
        let subjects: Vec<(String, String)> = if log.succeeded() {
            String::from_utf8_lossy(&log.stdout).lines()
                .filter_map(|line| line.split_once('\t')).map(|(hash, subject)| (hash.to_string(), subject.to_string()))
                .collect()
        } else {
            Vec::new()
        };

        let mut records = Vec::new();
        for &mark in marks {
            let message = mark.message();
            let mut verdicts = Vec::new();
            let mut hashes: Vec<&String> = subjects.iter().filter(|(_, subject)| *subject == message).map(|(hash, _)| hash).collect();
            hashes.dedup();
            for hash in hashes {
                let laid_out = self.fresh("record");
                std::fs::create_dir(&laid_out).map_err(|error| RunError::Io(format!("{}: {}", laid_out.display(), error)))?;
                let work_tree = format!("--work-tree={}", laid_out.display());
                let checkout = git(&[&work_tree, "checkout", "-f", "-q", hash, "--", "."])?;
                let readback = if checkout.succeeded() {
                    let tree = oracle::read_tree_when(&laid_out, self.corpus.modes.judged()).map_err(|error| RunError::Io(error.to_string()))?;
                    Readback::Served { tree, documented: left_out.get(&message).cloned().unwrap_or_default() }
                } else {
                    // The export wrote a commit git cannot lay out: what the tool served is broken.
                    Readback::Served { tree: Default::default(), documented: Default::default() }
                };
                verdicts.push(oracle::judge(&readback, &expectation(mark)));
            }
            if verdicts.is_empty() && required.contains(&mark) {
                verdicts.push(Verdict::Silent);
            }
            records.push(RecordReadback { mark, verdicts });
        }
        Ok(HistoryReadback { export, records, parked: None, reads: Vec::new() })
    }

    /// Git's readback, the git half of forklift's: the log of the work tree's repository, then each
    /// commit whose subject is a record's message read into a scratch index of its own
    /// (`GIT_INDEX_FILE`, `read-tree`) and laid out whole into a fresh folder (`checkout-index`).
    /// So git is read from its history, never its index: a damaged index or a stale index lock is
    /// not in the way, and the repository's own index is left as it was. A log, read or layout git
    /// refuses is git refusing to read, never an empty tree served; a record a log that exits 0
    /// leaves out was served as missing. Read through the host, the history read is that of a
    /// fresh clone of it.
    fn read_back_git(&self, bay: &Path, marks: &[Mark], required: &[Mark], expectation: impl Fn(Mark) -> Expectation) -> Result<HistoryReadback, RunError> {
        if self.scope.is_some() {
            return Err(RunError::Io("git reads back only an unscoped history".to_string()));
        }
        // Through the host: a fresh clone of the bare repository by the subject, through git's
        // transport (a local clone would hardlink the host's objects and check nothing), then
        // read like a work tree. Nothing is checked out, since the read is from history: a clone
        // that fails refuses every record.
        let clone;
        let bay = if self.via_host {
            let url = self.url.ok_or_else(|| RunError::Io("a host-side readback needs a host".to_string()))?;
            clone = self.fresh("clone");
            let arguments = self.subject(vec!["clone".into(), "-q".into(), "--no-local".into(), "--no-checkout".into(), url.to_string(), clone.display().to_string()]);
            let cloned = self.run(self.program(), arguments, self.scratch.root().to_path_buf())?;
            if !cloned.succeeded() {
                return Ok(HistoryReadback { export: cloned, records: Vec::new(), parked: None, reads: Vec::new() });
            }
            clone.as_path()
        } else {
            bay
        };
        let log = self.git_with(bay, &self.fresh("index"), &["log", "--all", "--format=%H%x09%s"])?;
        if !log.succeeded() {
            return Ok(HistoryReadback { export: log, records: Vec::new(), parked: None, reads: Vec::new() });
        }
        let subjects: Vec<(String, String)> = String::from_utf8_lossy(&log.stdout).lines()
            .filter_map(|line| line.split_once('\t')).map(|(hash, subject)| (hash.to_string(), subject.to_string()))
            .collect();

        let mut records = Vec::new();
        for &mark in marks {
            let message = mark.message();
            let mut verdicts = Vec::new();
            let mut hashes: Vec<&String> = subjects.iter().filter(|(_, subject)| *subject == message).map(|(hash, _)| hash).collect();
            hashes.dedup();
            for hash in hashes {
                let (readback, _) = self.lay_out_git(bay, hash)?;
                verdicts.push(oracle::judge(&readback, &expectation(mark)));
            }
            if verdicts.is_empty() && required.contains(&mark) {
                verdicts.push(Verdict::Silent);
            }
            records.push(RecordReadback { mark, verdicts });
        }
        Ok(HistoryReadback { export: log, records, parked: None, reads: Vec::new() })
    }

    /// Git run by the harness in the repository at `bay`, with `index` as its index file
    /// (`GIT_INDEX_FILE`), so the repository's own index is neither read nor written.
    fn git_with(&self, bay: &Path, index: &Path, arguments: &[&str]) -> Result<Outcome, RunError> {
        let mut environment = self.environment.clone();
        environment.variables.insert("GIT_INDEX_FILE".to_string(), index.display().to_string());
        let invocation = Invocation {
            program: self.programs.git.clone(), arguments: arguments.iter().map(|argument| argument.to_string()).collect(),
            directory: bay.to_path_buf(), limit: self.limit,
        };
        tool::run(&invocation, self.scratch, &environment).map_err(RunError::Tool)
    }

    /// The tree of commit `hash` in the repository at `bay`, read into an index of its own and laid
    /// out whole into a fresh folder, so an empty tree is served empty and no record's entries
    /// carry into the next; a read or layout git refuses is a refusal. With the commands it ran.
    fn lay_out_git(&self, bay: &Path, hash: &str) -> Result<(Readback, Vec<Outcome>), RunError> {
        let laid_out = self.fresh("record");
        std::fs::create_dir(&laid_out).map_err(|error| RunError::Io(format!("{}: {}", laid_out.display(), error)))?;
        let index = self.fresh("index");
        let work_tree = format!("--work-tree={}", laid_out.display());
        let read = self.git_with(bay, &index, &["read-tree", hash])?;
        if !read.succeeded() {
            return Ok((Readback::Refused { code: None }, vec![read]));
        }
        let checkout = self.git_with(bay, &index, &[&work_tree, "checkout-index", "-a", "-f"])?;
        let readback = if checkout.succeeded() {
            Readback::Served { tree: oracle::read_tree_when(&laid_out, self.corpus.modes.judged()).map_err(|error| RunError::Io(error.to_string()))?, documented: Default::default() }
        } else {
            Readback::Refused { code: None }
        };
        Ok((readback, vec![read, checkout]))
    }

    /// The readback over a real history: the probe record on [`PROBE_REF`]'s tip and each
    /// [`Mark::Position`] that many first parents along its ref, each resolved inside `bay` itself,
    /// so damage to refs and parent links shows, then laid out into a fresh folder and judged.
    /// The readback's own command lists the refs (git's `for-each-ref`, forklift's `pallet list`);
    /// a refusal there refuses every record. Git then resolves each record with `rev-parse` and
    /// lays it out through a scratch index ([`Context::lay_out_git`]). Forklift reads natively, in
    /// a copy of `bay` taken for this readback, so nothing is written into the warehouse being
    /// graded: the pallet's `history` walked by first parents from its listed head, then a fresh
    /// bay per record with a pallet made at that parcel, read minus `.forklift`. One shared bay is
    /// not used: an old position's ignore file wedges it for every later one. The bay opens at the
    /// current head first: see [`HistoryReadback::reads`]. What forklift prints names the copy, so
    /// the copy's path is written back as `bay`'s, and a remedy named by path acts on the
    /// warehouse being graded. A resolve or layout the subject refuses refuses that record; a walk
    /// that exits 0 and ends before the depth served the record as missing, which is silent.
    fn read_back_history(&self, bay: &Path, positions: &[Position], marks: &[Mark], expectation: impl Fn(Mark) -> Expectation) -> Result<HistoryReadback, RunError> {
        let (export, served, reads) = self.serve_history(bay, positions, marks)?;
        let records = served.into_iter().map(|(mark, readback, reached, _)| RecordReadback { mark, verdicts: vec![judged_history(&readback, reached, &expectation(mark))] }).collect();
        Ok(HistoryReadback { export, records, parked: None, reads })
    }

    /// What [`Context::read_back_history`] judges: the readback's own command, what each mark
    /// read back as (nothing when that command refused), and the commands that read them.
    fn serve_history(&self, bay: &Path, positions: &[Position], marks: &[Mark]) -> Result<Served, RunError> {
        let forklift = |arguments: &[&str], directory: PathBuf| {
            self.run(&self.programs.forklift, std::iter::once("--json").chain(arguments.iter().copied()).map(str::to_string).collect(), directory)
        };
        let graded = bay;
        let copy;
        let (export, bay) = match self.tool {
            Tool::Forklift => {
                copy = self.fresh("readback");
                crate::scrub::copy_tree(bay, &copy).map_err(|error| RunError::Io(format!("{:?}", error)))?;
                (forklift(&["pallet", "list"], copy.clone())?, copy.as_path())
            }
            Tool::Git | Tool::GitTuned => (self.git_with(bay, &self.fresh("index"), &["for-each-ref", "--format=%(refname) %(objectname)"])?, bay),
        };
        let rebase = |outcome: Outcome| if self.tool == Tool::Forklift { rebased(outcome, bay, graded) } else { outcome };
        let export = rebase(export);
        if !export.succeeded() {
            return Ok((export, Vec::new(), Vec::new()));
        }
        let mut deepest: BTreeMap<&str, usize> = BTreeMap::new();
        for &mark in marks {
            if let Some(position) = match mark { Mark::Position(index) => positions.get(index), _ => None } {
                let depth = deepest.entry(position.reference.as_str()).or_default();
                *depth = (*depth).max(position.depth);
            }
        }
        let heads = if self.tool == Tool::Forklift { pallet_heads(&export.stdout) } else { BTreeMap::new() };
        let mut chains: BTreeMap<&str, Option<Vec<String>>> = BTreeMap::new();
        let mut listed: BTreeMap<&str, usize> = BTreeMap::new();
        let mut reads = Vec::new();
        let mut records = Vec::new();
        for &mark in marks {
            let (reference, depth) = match mark {
                Mark::Position(index) => positions.get(index).map(|position| (position.reference.as_str(), position.depth))
                    .ok_or_else(|| RunError::Io(format!("position {} is past the sample's {}", index, positions.len())))?,
                Mark::Record(0) => (PROBE_REF, 0),
                other => return Err(RunError::Io(format!("{:?} is no record of a real history", other))),
            };
            let first = reads.len();
            let mut reached = true;
            let readback = match self.tool {
                Tool::Git | Tool::GitTuned => {
                    let revision = format!("refs/heads/{}~{}^{{commit}}", reference, depth);
                    let resolved = self.git_with(bay, &self.fresh("index"), &["rev-parse", "--verify", "-q", &revision])?;
                    let hash = resolved.succeeded().then(|| String::from_utf8_lossy(&resolved.stdout).trim().to_string());
                    reads.push(resolved);
                    match hash {
                        Some(hash) => {
                            let (readback, said) = self.lay_out_git(bay, &hash)?;
                            reads.extend(said);
                            readback
                        }
                        None => Readback::Refused { code: None },
                    }
                }
                Tool::Forklift => {
                    if !chains.contains_key(reference) {
                        let listing = forklift(&["history", reference], bay.to_path_buf())?;
                        let wanted = deepest.get(reference).copied().unwrap_or_default() + 1;
                        let chain = listing.succeeded().then(|| first_parents(heads.get(reference).map(String::as_str), &listing.stdout, wanted));
                        listed.insert(reference, reads.len());
                        reads.push(listing);
                        chains.insert(reference, chain);
                    }
                    match chains.get(reference).cloned().flatten() {
                        None => Readback::Refused { code: None },
                        // A walk that exits 0 without reaching the depth leaves the record out.
                        Some(chain) if chain.len() <= depth => {
                            reached = false;
                            Readback::Served { tree: Default::default(), documented: Default::default() }
                        }
                        Some(chain) => {
                            let side = self.fresh("position");
                            let name = side.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
                            let added = forklift(&["bay", "add", &name, &side.display().to_string()], bay.to_path_buf())?;
                            let opened = added.succeeded();
                            reads.push(added);
                            // The bay checks out a pallet of its own name at the current head, so the
                            // position's pallet is named apart from it.
                            let at = format!("{}-at", name);
                            let created = if opened { Some(forklift(&["pallet", "create", &at, &chain[depth]], side.clone())?) } else { None };
                            let laid_out = created.as_ref().is_some_and(Outcome::succeeded);
                            reads.extend(created);
                            if laid_out {
                                Readback::Served { tree: oracle::read_tree_when(&side, self.corpus.modes.judged()).map_err(|error| RunError::Io(error.to_string()))?, documented: Default::default() }
                            } else {
                                Readback::Refused { code: None }
                            }
                        }
                    }
                }
            };
            // A walk listed for an earlier mark on the same ref is this mark's evidence too.
            let evidence = listed.get(reference).copied().filter(|&index| index < first).into_iter().chain(first..reads.len()).collect();
            records.push((mark, readback, reached, evidence));
        }
        Ok((export, records, reads.into_iter().map(rebase).collect()))
    }

    /// What the subject's listing of parked work says of the `parked` entries, read in `bay`:
    /// `park list` naming each parcel, or `git stash list` each stash commit; `owed` when `bay`
    /// is the work tree, which must still list them. Nothing when none were parked.
    fn read_back_parked(&self, bay: &Path, parked: &[String], owed: bool) -> Result<Option<ParkedReadback>, RunError> {
        if parked.is_empty() {
            return Ok(None);
        }
        type ReadListing = fn(&[u8]) -> Option<Vec<String>>;
        let (arguments, read): (Vec<String>, ReadListing) = match self.tool {
            Tool::Forklift => (vec!["--json".into(), "park".into(), "list".into()], runner::parked_parcels),
            Tool::Git | Tool::GitTuned => (runner::STASH_LIST.iter().map(|argument| argument.to_string()).collect(), runner::stashed_commits),
        };
        let listing = self.run(self.program(), arguments, bay.to_path_buf())?;
        // A listing that exits 0 but cannot be read names nothing, so every park is missing.
        let listed = if listing.succeeded() { read(&listing.stdout).unwrap_or_default() } else { Vec::new() };
        let missing = parked.iter().filter(|parcel| !listed.contains(parcel)).cloned().collect();
        Ok(Some(ParkedReadback { listing, missing, owed }))
    }

    /// The readback of a scoped bay, where `git export` refuses and `show` serves no binary file's
    /// bytes: a side bay with the same scope, opened fresh, and in it a new pallet at each parcel
    /// whose description is a record's message, which materializes that record's scope there. The
    /// work tree under test, dirty or not, is left as it is. The parcels come from the history
    /// listing; a listing that cannot be read served nothing.
    fn read_back_scoped(&self, bay: &Path, scope: &str, marks: &[Mark], required: &[Mark], expectation: impl Fn(Mark) -> Expectation) -> Result<HistoryReadback, RunError> {
        let work = bay.to_path_buf();
        let listing = self.run(&self.programs.forklift, vec!["--json".into(), "history".into()], work.clone())?;
        if !listing.succeeded() {
            return Ok(HistoryReadback { export: listing, records: Vec::new(), parked: None, reads: Vec::new() });
        }
        let parcels = runner::described_parcels(&listing.stdout);

        let side = self.fresh("readback");
        let name = side.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        let opened = self.run(&self.programs.forklift, vec![
            "--json".into(), "bay".into(), "add".into(), name.clone(), side.display().to_string(), "--scope".into(), scope.to_string(),
        ], work)?;
        if !opened.succeeded() {
            return Ok(HistoryReadback { export: opened, records: Vec::new(), parked: None, reads: Vec::new() });
        }

        let mut records = Vec::new();
        let mut pallets = 0;
        for &mark in marks {
            let message = mark.message();
            let mut wanted: Vec<&String> = parcels.iter().filter(|(description, _)| *description == message).map(|(_, parcel)| parcel).collect();
            wanted.dedup();
            let mut verdicts = Vec::new();
            for parcel in wanted {
                pallets += 1;
                let created = self.run(&self.programs.forklift, vec![
                    "--json".into(), "pallet".into(), "create".into(), format!("{}-{}", name, pallets), parcel.clone(),
                ], side.clone())?;
                let readback = if created.succeeded() {
                    Readback::Served { tree: oracle::read_tree_when(&side, self.corpus.modes.judged()).map_err(|error| RunError::Io(error.to_string()))?, documented: Default::default() }
                } else {
                    Readback::Refused { code: created.failure.as_ref().map(|failure| failure.code.clone()) }
                };
                verdicts.push(oracle::judge(&readback, &expectation(mark)));
            }
            if verdicts.is_empty() && required.contains(&mark) {
                verdicts.push(Verdict::Silent);
            }
            records.push(RecordReadback { mark, verdicts });
        }
        Ok(HistoryReadback { export: opened, records, parked: None, reads: Vec::new() })
    }
}

/// Whether `outcome` is a stack refused because its record already stands: the inventory matches
/// the pallet's head, so there is nothing left to stack.
fn already_stacked(outcome: &Outcome) -> bool {
    outcome.arguments.iter().any(|argument| argument == "stack")
        && outcome.failure.as_ref().is_some_and(|failure| failure.message.starts_with("Nothing to stack"))
}

/// Whether an export reported leaving any file out, withheld or redacted.
fn leaves_out(export: &serde_json::Value) -> bool {
    ["withheld", "redacted"].iter().any(|key| export["data"][*key].as_array().is_some_and(|entries| !entries.is_empty()))
}

/// Every parcel an export names a left-out file in, paired with every record's message: what
/// [`left_out`] is given when the history listing that maps parcels to records refuses.
fn in_every_record(export: &serde_json::Value, marks: &[Mark]) -> Vec<(String, String)> {
    let parcels: std::collections::BTreeSet<&str> = ["withheld", "redacted"].iter()
        .flat_map(|key| export["data"][*key].as_array().into_iter().flatten())
        .flat_map(|entry| entry["parcels"].as_array().into_iter().flatten().filter_map(|parcel| parcel.as_str()))
        .collect();
    marks.iter().flat_map(|mark| parcels.iter().map(move |parcel| (mark.message(), parcel.to_string()))).collect()
}

/// `left_out` keeping, per record, only the paths that record's expectation opens as transformed.
fn where_transformed(
    mut left_out: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>, marks: &[Mark],
    expectation: impl Fn(Mark) -> Expectation,
) -> std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> {
    for &mark in marks {
        if let Some(paths) = left_out.get_mut(&mark.message()) {
            let opened = expectation(mark).transformed;
            paths.retain(|path, _| opened.contains_key(path));
        }
    }
    left_out.retain(|_, paths| !paths.is_empty());
    left_out
}

/// What the work tree holds before the faulted command: the last record on the corpus's line. A
/// variant is recorded off it, on a side pallet, and every scenario switches back before the
/// faulted command, so a variant acknowledged last is not what the work tree holds.
fn before_of(acknowledged: &[Mark], corpus: &Corpus) -> crate::oracle::Tree {
    acknowledged.iter().rev().find(|mark| matches!(mark, Mark::Record(_)))
        .and_then(|mark| mark.snapshot(corpus)).map(|snapshot| corpus::expected(&snapshot, &corpus.modes)).unwrap_or_default()
}

/// What an export reported leaving out, per record message: path → the documented code of a read
/// of it. An export lists each file it left out with the parcels naming it, `withheld` content as
/// the warehouse's own `content_withheld` refusal, `redacted` as `content_redacted`; `described`
/// maps a parcel to its record's message.
fn left_out(export: &serde_json::Value, described: &[(String, String)]) -> std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> {
    let mut left_out: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> = Default::default();
    for (key, code) in [("withheld", "content_withheld"), ("redacted", "content_redacted")] {
        for entry in export["data"][key].as_array().into_iter().flatten() {
            let Some(path) = entry["path"].as_str() else { continue };
            for parcel in entry["parcels"].as_array().into_iter().flatten().filter_map(|parcel| parcel.as_str()) {
                for (description, _) in described.iter().filter(|(_, described)| described == parcel) {
                    left_out.entry(description.clone()).or_default().insert(path.to_string(), code.to_string());
                }
            }
        }
    }
    left_out
}

/// `expectation` restricted to what a bay scoped to `scope` holds.
fn within(expectation: Expectation, scope: &str) -> Expectation {
    let prefix = format!("{}/", scope);
    let inside = |path: &String| path.starts_with(&prefix);
    Expectation {
        trees: expectation.trees.into_iter().map(|tree| tree.into_iter().filter(|(path, _)| inside(path)).collect()).collect(),
        transformed: expectation.transformed.into_iter().filter(|(path, _)| inside(path)).collect(),
    }
}

/// What a read of `taken` may produce: for withheld content, what `reader` is documented to serve;
/// for redacted content, what the build found its warehouse reporting.
fn admissible(taken: &Taken, reader: oracle::WithholdReader) -> Vec<Admissible> {
    match &taken.by {
        TakenBy::Withheld => oracle::withheld_admissible(reader, &taken.digest),
        TakenBy::Redacted { admissible, .. } => admissible.clone(),
    }
}

/// The corpus record the faulted command writes: the next one after every record the history
/// made. A variant does not count, since it is off the corpus's line.
fn next_record(acknowledged: &[Mark]) -> usize {
    acknowledged.iter().filter_map(|mark| match mark { Mark::Record(index) => Some(index + 1), Mark::Variant(_) | Mark::Position(_) => None }).max().unwrap_or(0)
}

/// How many leading lines of a faulted command that runs its record and then `then` acknowledge
/// the record: the record's own lines, once they exit the tool has recorded it; read back through
/// the host (`via_host`), every line through the last of the last push, once it exits the host
/// holds it. A host-side readback of a command that pushes nothing has no line that acknowledges
/// the record, and is refused rather than graded against one.
pub(crate) fn acknowledging(tool: Tool, then: &[Op], via_host: bool) -> Result<usize, String> {
    let lines = |op: &Op| line_count(tool, op);
    let record = own_lines(tool)?;
    if !via_host {
        return Ok(record);
    }
    let mut through = None;
    let mut ran = record;
    for op in then {
        ran += lines(op)?;
        if matches!(op, Op::Push) {
            through = Some(ran);
        }
    }
    through.ok_or_else(|| "the history is read back through the host, and the faulted command pushes nothing to it".to_string())
}

/// How many lines the faulted command's record itself runs, before any of the scenario's after it.
fn own_lines(tool: Tool) -> Result<usize, String> {
    line_count(tool, &Op::Record { index: 0 })
}

/// How many lines `op` runs as `tool`, refusing an operation `tool` has no binding for: a count
/// of none would misplace every line after it.
fn line_count(tool: Tool, op: &Op) -> Result<usize, String> {
    bind(tool, op).map(|lines| lines.len()).ok_or_else(|| format!("{} has no binding for {:?}", tool.label(), op))
}

/// Per line of a faulted command that runs its record and then `then`, whether a settle that finds
/// the record served answers for it: the record's own lines, which the record being there says
/// ran, and a push's, which the record being on the host says ran. Any other line (a compaction)
/// says nothing of the record, and a refusal of it is its own answer.
fn vouched(tool: Tool, then: &[Op]) -> Result<Vec<bool>, String> {
    let mut vouched = vec![true; own_lines(tool)?];
    for op in then {
        vouched.extend(std::iter::repeat_n(matches!(op, Op::Push), line_count(tool, op)?));
    }
    Ok(vouched)
}

/// What the faulted command's attempts have established of its record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Made {
    /// The lines that acknowledge the record exited: it is owed from here on, so a later loss of
    /// it is a loss, not a fault the command never got past.
    pub(crate) acknowledged: bool,
    /// Every line ran: nothing is left for a retry to do.
    pub(crate) completed: bool,
}

impl Made {
    /// What a command of `lines` lines, its first `acknowledging` acknowledging its record,
    /// established by landing its first `landed`.
    pub(crate) fn through(landed: usize, acknowledging: usize, lines: usize) -> Made {
        Made { acknowledged: landed >= acknowledging, completed: landed >= lines }
    }

    /// What a first attempt that ran `ran`, one outcome per line from the first, established:
    /// the lines up to the first that did not succeed landed.
    pub(crate) fn of(ran: &[Outcome], acknowledging: usize, lines: usize) -> Made {
        Made::through(ran.iter().take_while(|outcome| outcome.succeeded()).count(), acknowledging, lines)
    }

    /// Settled as acknowledged by an answer asking nothing further, as `promoted`, the
    /// acknowledging lines landing (through the last push, read back through the host),
    /// establishes: completed too only when no line follows them.
    fn promote(&mut self, promoted: Made) {
        self.acknowledged = true;
        self.completed |= promoted.completed;
    }

    /// Where the rest of the lines resume when the attempt that stopped at `stopped` left the
    /// record, from `before`, acknowledged by a settle ([`Made::promote`]) rather than by landing:
    /// it stopped short of the acknowledging lines, at a line whose answer asked nothing further.
    /// A line the settle answers for (`vouched`: the record's own, or a push read back through
    /// the host, told there is nothing to push) is done, so the rest resume after it, and never
    /// inside the record's own lines. Any other line (a compaction refused for good) is not, and
    /// runs again, so a refusal for good is refused again and never completes. An attempt
    /// acknowledged by landing stopped at or past the acknowledging lines, at a line refused for
    /// good, and is not run again.
    fn rest(&self, before: Made, stopped: Resume, acknowledging: usize, vouched: &[bool], own: usize) -> Option<Resume> {
        let done = vouched.get(stopped.at).copied().unwrap_or(false);
        (!before.acknowledged && self.acknowledged && !self.completed && stopped.at < acknowledging)
            .then_some(Resume { at: if done { own.max(stopped.at + 1) } else { stopped.at }, may_have_stacked: false })
    }

    /// Adds what a retry established, which landed every line or stopped where `next` resumes:
    /// every line before that landed, in it or in an earlier attempt.
    fn retried(&mut self, landed: bool, next: Resume, acknowledging: usize, lines: usize) {
        let retried = if landed { Made::through(lines, acknowledging, lines) } else { Made::through(next.at, acknowledging, lines) };
        self.acknowledged |= retried.acknowledged;
        self.completed |= retried.completed;
    }
}

/// The expected trees of one faulted run.
pub(crate) struct Expected {
    acknowledged: Vec<Mark>,
    faulted: Mark,
    before: oracle::Tree,
    after: oracle::Tree,
    /// Content the history took away, by digest, with what a read of it may now produce.
    taken: Vec<(String, Vec<Admissible>)>,
    /// The parcels the history left parked, which must be listed again.
    parked: Vec<String>,
    /// The records the history pushed, which a fresh clone of its remote must serve.
    pushed: Vec<Mark>,
    /// Over a real history, what each sampled position holds, by [`Mark::Position`] index.
    positions: Vec<oracle::Tree>,
}

impl Expected {
    fn marks(&self) -> Vec<Mark> {
        self.acknowledged.iter().copied().chain([self.faulted]).collect()
    }

    /// What `mark` may hold, once the faulted record has (`acknowledged`) or has not been
    /// acknowledged.
    fn of(&self, corpus: &Corpus, mark: Mark, acknowledged: bool) -> Expectation {
        let mut expectation = self.untransformed(corpus, mark, acknowledged);
        // Every path whose recorded bytes are content taken away is open to what a read of it may
        // now produce, in whichever record holds it.
        for (digest, admissible) in &self.taken {
            for tree in &expectation.trees {
                for (path, _) in tree.iter().filter(|(_, recorded)| *recorded == digest) {
                    expectation.transformed.insert(path.clone(), admissible.clone());
                }
            }
        }
        expectation
    }

    fn untransformed(&self, corpus: &Corpus, mark: Mark, acknowledged: bool) -> Expectation {
        match mark {
            Mark::Position(index) => Expectation::acknowledged(self.positions.get(index).cloned().unwrap_or_default()),
            mark if mark != self.faulted => Expectation::acknowledged(mark.snapshot(corpus).map(|snapshot| corpus::expected(&snapshot, &corpus.modes)).unwrap_or_default()),
            _ if acknowledged => Expectation::acknowledged(self.after.clone()),
            _ => Expectation::faulted(self.before.clone(), self.after.clone()),
        }
    }

    /// The records that must be served: every acknowledged one, the faulted one among them once
    /// it is.
    fn required(&self, acknowledged: bool) -> Vec<Mark> {
        if acknowledged { self.marks() } else { self.acknowledged.clone() }
    }
}

/// What the steps of one faulted run have established so far.
#[derive(Default)]
struct Trail {
    steps: Vec<Outcome>,
    readbacks: Vec<HistoryReadback>,
    evidence: Evidence,
    recoverability: Recoverability,
    /// The harness's own reads that decided the own remedy set ([`Graded::probes`]).
    probes: Vec<Outcome>,
    /// The fresh clone the own remedy set read from in place of the work tree, once it made one.
    /// It owes only what the history pushed: a record kept local that it lacks is lost, not
    /// silent, while a pushed one it leaves out with exit 0 is still silent.
    recloned: Option<PathBuf>,
}

impl Trail {
    /// Reads the history back and records what it says; true when the faulted command completed
    /// and every record reads as expected, the faulted one held to its own tree once acknowledged.
    /// A served record counts as acknowledged from here on once the tool's latest answer asks
    /// nothing further (see [`answered`]) and settles it ([`settled_by`]): a command killed after
    /// its record was durable, retried, is told there is nothing left to stack. That is
    /// `promoted`, what the acknowledging lines landing establishes; it says nothing of any line
    /// after them, which completes only by landing.
    /// `retry` says the latest step re-ran the faulted command: only a retry's answer settles a
    /// served record, since the faulted command's own refusal of a write it committed is graded as
    /// a refusal, and heal is no re-run.
    fn settle(&mut self, context: &Context, expected: &Expected, made: &mut Made, promoted: Made, retry: bool) -> Result<bool, RunError> {
        let mut readback = self.read_back(context, expected, made.acknowledged)?;
        if !made.acknowledged && settles(retry, self.steps.last()) && readback.reads(&[expected.faulted]) {
            // Judged again as acknowledged: the record must now hold exactly its own tree.
            let settled = self.read_back(context, expected, true)?;
            if settled_by(self.steps.last(), &settled, expected.faulted) {
                self.evidence.any_silent |= readback.silent();
                self.readbacks.push(readback);
                readback = settled;
                made.promote(promoted);
            } else {
                // Kept for the record of what ran, its verdicts graded by nothing: the readback
                // this settle judges by is pushed after it, the last.
                self.readbacks.push(settled);
            }
        }
        let required = expected.required(made.acknowledged);
        self.evidence.any_silent |= readback.silent();
        let reads = made.completed && readback.reads(&required);
        self.evidence.acknowledged_readable = readback.reads(&expected.acknowledged);
        self.readbacks.push(readback);
        if reads && self.recoverability.commands_to_expected.is_none() {
            self.recoverability.commands_to_expected = Some(self.steps.len() as u32);
        }
        Ok(reads)
    }

    fn read_back(&self, context: &Context, expected: &Expected, acknowledged: bool) -> Result<HistoryReadback, RunError> {
        // A fresh clone owes what the history pushed, not what it kept local, nor its parks.
        let (bay, required) = match &self.recloned {
            Some(clone) => (clone.clone(), expected.pushed.clone()),
            None => (context.scratch.work(), expected.required(acknowledged)),
        };
        let mut readback = context.read_back(&bay, &expected.marks(), &required, |mark| expected.of(context.corpus, mark, acknowledged))?;
        readback.parked = context.read_back_parked(&bay, &expected.parked, self.recloned.is_none())?;
        Ok(readback)
    }

    /// Records `step`, when there is one, then, when it can change what reads back (`rescue`),
    /// settles; true once every acknowledged record reads back, recording `rescue` as what first
    /// got there.
    fn rescue(&mut self, context: &Context, expected: &Expected, made: &mut Made, promoted: Made, step: Option<Outcome>, rescue: Option<Rescue>) -> Result<bool, RunError> {
        self.steps.extend(step);
        let Some(rescue) = rescue else { return Ok(false) };
        self.settle(context, expected, made, promoted, false)?;
        if self.evidence.acknowledged_readable {
            self.recoverability.rescued_by = Some(rescue);
        }
        Ok(self.evidence.acknowledged_readable)
    }
}

/// Whether a served record counts as acknowledged at this settle: on a retry, after an
/// [`answered`] one. It is completed too only when no line follows the record's own
/// ([`Made::promote`]).
fn settles(retry: bool, last: Option<&Outcome>) -> bool {
    retry && answered(last)
}

/// Whether the answer `last`, the retry's last line, settles the faulted record as acknowledged,
/// from `settled`, the readback judged as acknowledged. A last line that succeeded landed every
/// line the retry ran, and a stack refused as nothing to stack claims the record stands: either
/// holds the record to its own tree. Any other refusal claims nothing of the record, so it
/// settles only one already holding its own tree: a damaged file refused for good on every retry
/// leaves a record still at the tree before it judged as never acknowledged, not as silent.
fn settled_by(last: Option<&Outcome>, settled: &HistoryReadback, faulted: Mark) -> bool {
    last.is_some_and(|outcome| outcome.succeeded() || already_stacked(outcome)) || settled.reads(&[faulted])
}

/// Whether `last`, the latest command run, answered and asked nothing further: it exited (a kill
/// is no answer), and either succeeded or refused with a failure document naming no remedy and
/// nothing unreadable as one, as a re-stack of a record already made is refused with nothing left
/// to stack. A crash with no failure document is no answer.
fn answered(last: Option<&Outcome>) -> bool {
    last.is_some_and(|outcome| outcome.succeeded() || (outcome.exit.is_some() && outcome.failure.is_some() && {
        // A failure document is forklift's: git writes none, so no git refusal is an answer.
        let extracted = remedy::extract(Tool::Forklift, &outcome.remedy_texts());
        extracted.remedies.is_empty() && extracted.unextractable.is_empty()
    }))
}

/// What a refused `outcome` said: its failure's message, else its stderr, cut at 400 characters
/// with the cut marked.
fn said(outcome: &Outcome) -> String {
    let text = outcome.failure.as_ref().map(|failure| failure.message.clone()).unwrap_or_else(|| String::from_utf8_lossy(&outcome.stderr).trim().to_string());
    match text.char_indices().nth(400) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

/// Why the unfaulted readback did not serve `mark`, from `reads`, the reads of that mark alone: a
/// walk that ended short of it, else the first of them that refused, as it exited and what it
/// said.
fn unserved(mark: Mark, reached: bool, reads: &[&Outcome]) -> String {
    match reads.iter().find(|outcome| !outcome.succeeded()) {
        _ if !reached => format!("the unfaulted readback did not serve {:?}: its walk ended short of it", mark),
        Some(refused) => format!("the unfaulted readback did not serve {:?}: {:?} exited {:?}: {}", mark, refused.arguments, refused.exit, said(refused)),
        None => format!("the unfaulted readback did not serve {:?}", mark),
    }
}

/// Each pallet's head in a `--json pallet list`, by name.
fn pallet_heads(stdout: &[u8]) -> BTreeMap<String, String> {
    let document = runner::document(stdout);
    document["data"]["pallets"].as_array().into_iter().flatten()
        .filter_map(|pallet| Some((pallet["name"].as_str()?.to_string(), pallet["head"].as_str()?.to_string())))
        .collect()
}

/// The verdict on one record of a real history: a record the subject did not reach (a walk that
/// exited 0 short of it) is silently lost whatever it was expected to hold, even an empty tree;
/// any other is judged as read.
fn judged_history(readback: &Readback, reached: bool, expectation: &Expectation) -> Verdict {
    if reached { oracle::judge(readback, expectation) } else { Verdict::Silent }
}

/// What `outcome` printed with every mention of `from` written as `to`, both as given and as the
/// platform resolves it (a temporary folder may be reached through a link): its stdout and
/// stderr, and the failure parsed from them, which is what remedies are read from. The failure's
/// text is already decoded, so a path it names matches however the JSON escaped it.
fn rebased(mut outcome: Outcome, from: &Path, to: &Path) -> Outcome {
    let mut pairs = vec![(from.display().to_string(), to.display().to_string())];
    if let (Ok(from), Ok(to)) = (std::fs::canonicalize(from), std::fs::canonicalize(to)) {
        pairs.push((from.display().to_string(), to.display().to_string()));
    }
    for text in [&mut outcome.stdout, &mut outcome.stderr] {
        let mut written = String::from_utf8_lossy(text).into_owned();
        for (from, to) in &pairs {
            written = written.replace(from.as_str(), to);
        }
        *text = written.into_bytes();
    }
    if let Some(failure) = outcome.failure.as_mut() {
        for text in std::iter::once(&mut failure.message).chain(failure.next_step.as_mut()) {
            for (from, to) in &pairs {
                *text = text.replace(from.as_str(), to);
            }
        }
    }
    outcome
}

/// The first `wanted` parcels of the first-parent chain from `head` through the parcels a
/// `--json history` lists, `head` first: fewer when it ends at a parcel with no parents, at one the
/// listing leaves out, or at one already walked. Without a head, from the listing's first entry.
fn first_parents(head: Option<&str>, stdout: &[u8], wanted: usize) -> Vec<String> {
    let document = runner::document(stdout);
    let entries = document["data"]["entries"].as_array().cloned().unwrap_or_default();
    let parents: BTreeMap<String, Option<String>> = entries.iter()
        .filter_map(|entry| Some((entry["parcel"].as_str()?.to_string(), entry["parents"][0].as_str().map(str::to_string))))
        .collect();
    let mut chain = Vec::new();
    let mut walked = std::collections::BTreeSet::new();
    let mut at = head.map(str::to_string).or_else(|| entries.first().and_then(|entry| entry["parcel"].as_str()).map(str::to_string));
    while let Some(parcel) = at {
        let Some(parent) = parents.get(&parcel) else { break };
        // A cycle in what a damaged store lists ends the walk rather than looping.
        if chain.len() == wanted || !walked.insert(parcel.clone()) {
            break;
        }
        at = parent.clone();
        chain.push(parcel);
    }
    chain
}

/// What the faulted command of a built scenario must leave readable: the record it writes, and
/// what every record may then hold. Fails when the corpus has no record after the history's.
pub(crate) fn expected(scenario: &Scenario, corpus: &Corpus, built: &runner::Built, warehouse: Warehouse) -> Result<(Expected, usize), RunError> {
    let faulted = next_record(&built.acknowledged);
    let snapshot = corpus.records.get(faulted)
        .ok_or_else(|| RunError::Io(format!("the faulted record {} is past the corpus's {} records", faulted, corpus.records.len())))?;
    let before = before_of(&built.acknowledged, corpus);
    let taken = built.taken.iter().map(|taken| (taken.digest.clone(), admissible(taken, scenario.withhold_reader(warehouse)))).collect();
    Ok((Expected { before, after: corpus::expected(snapshot, &corpus.modes), acknowledged: built.acknowledged.clone(), faulted: Mark::Record(faulted), taken,
        parked: built.parked.clone(), pushed: built.pushed.clone(), positions: Vec::new() }, faulted))
}

/// What the steps after a fault established, and the grade they earn.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Graded {
    pub grade: Grade,
    pub evidence: Evidence,
    pub recoverability: Recoverability,
    /// Every command from the fault on, in order: the faulted command, retries, remedies, the
    /// subject's own remedy set.
    pub steps: Vec<Outcome>,
    pub readbacks: Vec<HistoryReadback>,
    /// The harness's own reads that decided the subject's own remedy set, in order: for git, each
    /// ref's `rev-parse`, the `merge-base --is-ancestor` of a resolving ref that differs from its
    /// reflog's newest entry, and each candidate's `rev-list`. No step: the subject was not run for
    /// them.
    pub probes: Vec<Outcome>,
    /// Remedies named in a form the harness does not run.
    pub unextractable: Vec<String>,
}

/// Where a retry of the faulted command resumes: the line it starts at, and whether the earlier
/// attempt's failure there may have come after its parcel stacked, so that the same line refused
/// as nothing to stack means it landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Resume {
    at: usize,
    may_have_stacked: bool,
}

impl Resume {
    /// The start of the command, after an attempt that landed every line.
    const START: Resume = Resume { at: 0, may_have_stacked: false };

    /// Resuming at line `at`, which failed with `failed`: a stack refused as nothing to stack had
    /// nothing to stack, and the same refusal again is no proof that anything landed.
    fn at_failed(at: usize, failed: &Outcome) -> Resume {
        Resume { at, may_have_stacked: !already_stacked(failed) }
    }
}

/// Where the first retry resumes, after a first attempt that ran `ran`, one outcome per line, of
/// the command's `lines`: the first line that did not succeed, or the first it never reached, as a
/// user re-runs the step that failed rather than the ones that landed (a record already stacked is
/// refused as nothing to stack, and what follows it would never run again). A first attempt that
/// ran every line resumes at the start. Later retries say where the next one resumes themselves.
fn resume_after(ran: &[Outcome], lines: usize) -> Resume {
    match ran.iter().position(|outcome| !outcome.succeeded()) {
        Some(failed) => Resume::at_failed(failed, &ran[failed]),
        None if ran.len() < lines => Resume { at: ran.len(), may_have_stacked: false },
        None => Resume::START,
    }
}

/// Runs `lines` from `from` on through `run` until one does not land: says whether every line
/// landed, and where the next retry resumes (at the line that did not land, or the start).
fn resume_lines<L, E>(lines: L, from: Resume, steps: &mut Vec<Outcome>, mut run: impl FnMut(Vec<String>) -> Result<Outcome, E>) -> Result<(bool, Resume), E>
where L: IntoIterator<Item = Vec<String>> {
    for (at, arguments) in lines.into_iter().enumerate().skip(from.at) {
        let outcome = run(arguments)?;
        // The line the earlier attempt failed may have stacked its parcel before failing; its
        // refusal as nothing to stack then means it landed, and the user goes on to the next line.
        // Not when that earlier failure was the same refusal, and not on a later line, never run.
        let landed = outcome.succeeded() || (at == from.at && from.may_have_stacked && already_stacked(&outcome));
        if !landed {
            let next = Resume::at_failed(at, &outcome);
            steps.push(outcome);
            return Ok((false, next));
        }
        steps.push(outcome);
    }
    Ok((true, Resume::START))
}

/// How many rounds of named remedies a fault gets: each round runs what the refusals so far named
/// and nothing run before.
const REMEDY_ROUNDS: usize = 3;

/// Runs named remedies in rounds: each round the remedies `named` finds that no earlier round ran,
/// through `run`, which says whether the history then reads; until it reads, nothing new is named,
/// or [`REMEDY_ROUNDS`] have run. One remedy can uncover the next: a stale lock removed, the bay is
/// found behind its pallet.
fn follow_remedies<S, E>(state: &mut S, named: impl Fn(&mut S) -> Vec<Remedy>, mut run: impl FnMut(&mut S, &[Remedy]) -> Result<bool, E>) -> Result<bool, E> {
    let mut ran: Vec<Remedy> = Vec::new();
    for _ in 0..REMEDY_ROUNDS {
        let fresh: Vec<Remedy> = named(state).into_iter().filter(|remedy| !ran.contains(remedy)).collect();
        if fresh.is_empty() {
            return Ok(false);
        }
        let reads = run(state, &fresh)?;
        ran.extend(fresh);
        if reads {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Grades a fault: `first` is the faulted command's first attempt, pushing one outcome per line it
/// ran, from the first. What it established is read off those outcomes ([`Made::of`]), never
/// taken from the lane. Then the readback, one plain retry, the remedies every refusal named, and
/// the subject's own remedy set ([`own_remedies`]).
pub(crate) fn grade_after(
    context: &Context, expected: &Expected, faulted: usize,
    first: impl FnOnce(&Context, &mut Vec<Outcome>) -> Result<(), RunError>,
) -> Result<Graded, RunError> {
    let mut trail = Trail::default();
    let mut unextractable = Vec::new();

    // The faulted command, then one plain retry of it. Once the record is acknowledged it stays
    // owed, and once every line ran it stays completed, so a later attempt that refuses (a re-stack
    // with nothing new) unmakes neither.
    let lines = context.lines(faulted)?.len();
    let acknowledging = context.acknowledging()?;
    first(context, &mut trail.steps)?;
    let mut made = Made::of(&trail.steps, acknowledging, lines);
    let mut resume = resume_after(&trail.steps, lines);
    let promoted = Made::through(acknowledging, acknowledging, lines);
    let vouched = vouched(context.tool, context.then).map_err(RunError::Io)?;
    let own = own_lines(context.tool).map_err(RunError::Io)?;
    // A retry from where the last attempt stopped, then the readback. A retry whose answer settles
    // the record as acknowledged ran none of the lines after the record's own, or none that
    // landed, so they are run once more from the first of them: only landing completes them.
    let retry = |trail: &mut Trail, resume: &mut Resume, made: &mut Made| -> Result<bool, RunError> {
        let again = |trail: &mut Trail, resume: &mut Resume, made: &mut Made| -> Result<bool, RunError> {
            let (landed, next) = context.retry_from(faulted, *resume, &mut trail.steps)?;
            made.retried(landed, next, acknowledging, lines);
            *resume = next;
            trail.settle(context, expected, made, promoted, true)
        };
        let before = *made;
        let reads = again(trail, resume, made)?;
        match made.rest(before, *resume, acknowledging, &vouched, own) {
            Some(rest) => {
                *resume = rest;
                again(trail, resume, made)
            }
            None => Ok(reads),
        }
    };
    let mut reads = trail.settle(context, expected, &mut made, promoted, false)?;
    if !reads {
        reads = retry(&mut trail, &mut resume, &mut made)?;
    }
    trail.evidence.healed_by_retry = reads;

    // The remedies every refusal so far named, run in order, then the faulted command again; and
    // again for any remedy a refusal in that round named first, since one remedy can uncover the
    // next (a stale lock removed, the bay is found behind its pallet). Bounded, and never the same
    // remedy twice.
    if !reads {
        let named = |(trail, unextractable): &mut (Trail, Vec<String>)| {
            let texts: Vec<String> = trail.steps.iter().chain(trail.readbacks.iter().flat_map(|readback| std::iter::once(&readback.export).chain(&readback.reads)))
                .flat_map(|outcome| remedy::texts(context.tool, outcome)).collect();
            let extracted = remedy::extract(context.tool, &texts.iter().map(String::as_str).collect::<Vec<_>>());
            for text in extracted.unextractable {
                if !unextractable.contains(&text) {
                    unextractable.push(text);
                }
            }
            extracted.remedies
        };
        let run = |(trail, _): &mut (Trail, Vec<String>), fresh: &[Remedy]| -> Result<bool, RunError> {
            trail.evidence.remedy_named = true;
            for named in fresh {
                match named {
                    Remedy::Command { arguments } => {
                        let mut arguments = arguments.clone();
                        if context.tool == Tool::Forklift && arguments.first().map(String::as_str) != Some("--json") {
                            arguments.insert(0, "--json".into());
                        }
                        trail.steps.push(context.run(context.program(), context.subject(arguments), context.scratch.work())?);
                    }
                    Remedy::Remove { path } => trail.recoverability.outside_knowledge.push(remedy::remove(path, &context.scratch.roots()).0),
                }
            }
            let reads = retry(trail, &mut resume, &mut made)?;
            trail.evidence.remedy_worked = reads;
            trail.recoverability.named_right_remedy = reads;
            Ok(reads)
        };
        let mut state = (std::mem::take(&mut trail), std::mem::take(&mut unextractable));
        follow_remedies(&mut state, named, run)?;
        (trail, unextractable) = state;
    }

    // The subject's own remedy set last, when an acknowledged record still does not read back:
    // what a user of the tool reaches for unprompted, best first, each step settled where it can
    // change what reads back, and stopping at the first that rescues.
    if !trail.evidence.acknowledged_readable {
        own_remedies(context, expected, &mut trail, &mut made, promoted)?;
    }

    Ok(Graded {
        grade: oracle::grade(&trail.evidence), evidence: trail.evidence, recoverability: trail.recoverability,
        steps: trail.steps, readbacks: trail.readbacks, probes: trail.probes, unextractable,
    })
}

/// The subject's own remedy set, run in order until one rescues: for forklift `heal`, then
/// `audit` (a check: it reports and repairs nothing), then a fresh clone of the remote; for git
/// `fsck --full` (a check), then each ref restored with `update-ref` to its reflog's newest entry
/// ([`reflog_tips`]) when it no longer resolves or names a strict ancestor of that entry, and that
/// entry's objects are all present, then a fresh clone. A ref is only ever moved forward, to a
/// descendant of what it names, so a damaged reflog cannot rewind one; a loose ref lost over an
/// older packed one is restored.
/// A clone needs a remote, and is not made where the readback already reads through one, where
/// the work tree sits on a volume the clone would leave, or over parked work, which no clone holds.
/// What decides a restore (`rev-parse`, `merge-base`, `rev-list`) is the harness's reading, like a
/// readback, and is no step the subject was run for.
fn own_remedies(context: &Context, expected: &Expected, trail: &mut Trail, made: &mut Made, promoted: Made) -> Result<(), RunError> {
    let work = context.scratch.work();
    let run = |arguments: &[&str], directory: PathBuf| context.run(context.program(), context.subject(arguments.iter().map(|argument| argument.to_string()).collect()), directory);
    match context.tool {
        Tool::Forklift => {
            if trail.rescue(context, expected, made, promoted, Some(run(&["--json", "heal"], work.clone())?), Some(Rescue::LocalRepair))? {
                return Ok(());
            }
            trail.rescue(context, expected, made, promoted, Some(run(&["--json", "audit"], work.clone())?), None)?;
        }
        Tool::Git | Tool::GitTuned => {
            trail.rescue(context, expected, made, promoted, Some(run(&["fsck", "--full", "--no-progress"], work.clone())?), None)?;
            let mut restored = false;
            let state = work.join(runner::state_folder(context.tool));
            for (reference, hash) in reflog_tips(&state) {
                let resolved = run(&["rev-parse", "--verify", "-q", &reference], work.clone())?;
                let value = resolved.succeeded().then(|| String::from_utf8_lossy(&resolved.stdout).trim().to_string());
                trail.probes.push(resolved);
                if let Some(value) = value {
                    // A ref that resolves is moved only forward: when it names a strict ancestor of
                    // its reflog's newest entry (a loose ref lost over an older packed one), an
                    // update was lost. Every move git makes is logged, so a ref git moved back
                    // agrees with its newest entry, and an older entry never rewinds a ref. A ref
                    // update git refused after logging it (the reflog is appended before the ref's
                    // lock is renamed) reads the same way: the restore then lands, with git's own
                    // command, a write git reported failed, and the row says it was rescued so.
                    if value == hash {
                        continue;
                    }
                    let behind = run(&["merge-base", "--is-ancestor", &value, &hash], work.clone())?;
                    let lost = behind.succeeded();
                    trail.probes.push(behind);
                    if !lost {
                        continue;
                    }
                }
                // A candidate whose history is not all present would restore a ref the readback
                // then refuses: only one whose every object is there is restored. `fsck` cannot ask
                // this of one commit while a broken ref stands, since it checks every ref first.
                let listed = run(&["rev-list", "--objects", "--quiet", &hash], work.clone())?;
                let present = listed.succeeded();
                trail.probes.push(listed);
                if !present {
                    continue;
                }
                trail.recoverability.outside_knowledge.push(format!("read the newest entry of .git/logs/{}", reference));
                trail.steps.push(run(&["update-ref", &reference, &hash], work.clone())?);
                restored = true;
            }
            if restored && trail.rescue(context, expected, made, promoted, None, Some(Rescue::LocalRepair))? {
                return Ok(());
            }
        }
    }
    let Some(url) = context.url else { return Ok(()) };
    if context.via_host || context.scratch.work_moved() || !expected.parked.is_empty() {
        return Ok(());
    }
    let clone = context.fresh("reclone");
    let target = clone.display().to_string();
    let cloned = match (context.tool, context.scope) {
        (Tool::Forklift, Some(scope)) => run(&["--json", "clone", "--only", scope, url, &target], context.scratch.root().to_path_buf())?,
        (Tool::Forklift, None) => run(&["--json", "clone", url, &target], context.scratch.root().to_path_buf())?,
        // Through git's transport, as the build's clone is: a local clone would share the host's
        // object files.
        (Tool::Git | Tool::GitTuned, _) => run(&["clone", "-q", "--no-local", url, &target], context.scratch.root().to_path_buf())?,
    };
    if !cloned.succeeded() {
        trail.steps.push(cloned);
        return Ok(());
    }
    trail.recloned = Some(clone);
    trail.rescue(context, expected, made, promoted, Some(cloned), Some(Rescue::Reclone))?;
    Ok(())
}

/// Each ref under `refs/` with a reflog in the git folder `git`, and the commit its reflog's newest
/// entry moved it to, in path order. Read from the log files themselves: once a ref is broken, git
/// refuses every reflog query that names it. A log whose last line names no commit is skipped.
fn reflog_tips(git: &Path) -> Vec<(String, String)> {
    let logs = git.join("logs");
    let Ok(walked) = crate::census::walk(&logs.join("refs")) else { return Vec::new() };
    let mut tips: Vec<(String, String)> = walked.iter().filter(|path| !path.ends_with('/')).filter_map(|path| {
        let log = std::fs::read(logs.join("refs").join(path)).ok()?;
        let line = String::from_utf8_lossy(&log).lines().rev().find(|line| !line.trim().is_empty())?.to_string();
        let hash = line.split_whitespace().nth(1)?;
        (hash.len() >= 40 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) && hash.bytes().any(|byte| byte != b'0'))
            .then(|| (format!("refs/{}", path), hash.to_string()))
    }).collect();
    tips.sort();
    tips
}

/// Runs one faulted run of `scenario` as `tool`: damage `kind` to the path `name` resolves to
/// after the build (a census name for forklift, a git layout row for git), then the faulted
/// command, readback, a plain retry, the named remedies, and the subject's own remedy set.
#[allow(clippy::too_many_arguments)]
pub fn run(scenario: &Scenario, name: &str, warehouse: Warehouse, kind: Kind, tool: Tool, corpus: &Corpus, programs: &Programs, scratch: &Scratch, environment: &Environment, limit: Duration, seed: u64) -> Result<Faulted, RunError> {
    let built = runner::build(scenario, corpus, tool, programs.of(tool), programs.server.as_deref(), scratch, environment, limit, None)?;
    // What the build recorded: the corpus rooted under the scenario's scope, when it has one.
    let corpus = &scenario.corpus(corpus);
    let (walked, state) = match warehouse {
        Warehouse::Client => (&built.walked, scratch.work().join(runner::state_folder(tool))),
        Warehouse::Host if tool == Tool::Forklift && built.host.is_some() => (&built.host_walked, runner::host_root(scratch).join(".forklift")),
        // A bare repository is its own state folder.
        Warehouse::Host if tool != Tool::Forklift && built.bare.is_some() => (&built.host_walked, runner::host_root(scratch)),
        Warehouse::Host => return Err(RunError::Io(format!("{} damages a host, and {} hosts none for {:?}", name, scenario.id, tool))),
    };
    // A planted path is absent by definition, so it is named, not resolved against the walk.
    let resolved = match tool {
        Tool::Forklift => resolve(name, walked, seed),
        Tool::Git | Tool::GitTuned => crate::git_layout::resolve(name, walked, seed),
    };
    let target = if kind.plants() {
        damage::planted(name)
    } else {
        resolved.ok_or_else(|| RunError::Io(format!("{} resolves to nothing {} built", name, scenario.id)))?
    };
    damage::apply(&state.join(&target), kind, seed).map_err(|error| RunError::Io(format!("{:?}", error)))?;

    let (expected, faulted) = expected(scenario, corpus, &built, warehouse)?;
    let context = Context {
        tool, corpus, programs, scratch, environment, limit, then: &scenario.then, url: built.url(),
        scope: scenario.scope(), via_host: warehouse == Warehouse::Host, history: None,
    };
    let graded = grade_after(&context, &expected, faulted, |context, steps| context.record(faulted, steps).map(drop))?;
    Ok(Faulted {
        scenario: scenario.id.clone(), name: name.to_string(), warehouse, kind, target,
        grade: graded.grade, evidence: graded.evidence, recoverability: graded.recoverability,
        steps: graded.steps, readbacks: graded.readbacks, probes: graded.probes, unextractable: graded.unextractable, unreached: None,
    })
}

/// What a subject's own readback of an unfaulted run gave, the expectation of every damage row
/// over it: each sampled position's tree, and the probe ref's tree before and after the probe
/// record. Taken once per subject on a copy of its pristine subject, so what an import or export
/// lost is in it, identically, and never counts as damage.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Baseline {
    pub positions: Vec<oracle::Tree>,
    pub before: oracle::Tree,
    pub after: oracle::Tree,
}

/// One history subject a damage row works on: its tool, the pristine subject (a work tree with its
/// state folder, the probe ref current), the sample, and its unfaulted readback.
pub struct HistorySubject<'a> {
    pub tool: Tool,
    pub pristine: &'a Path,
    pub positions: &'a [Position],
    pub baseline: &'a Baseline,
}

/// The corpus a history's faulted command records: the probe file alone, its modes judged where
/// the platform keeps them, so a mode a fault changes reads back changed.
fn probe_corpus() -> Corpus {
    let record: corpus::Snapshot = [(PROBE_FILE.to_string(), PROBE_BYTES.to_vec())].into_iter().collect();
    Corpus {
        seed: 0, shape: corpus::Shape { records: 1, initial_files: 1, max_bytes: PROBE_BYTES.len() },
        records: vec![record], modes: corpus::Modes::Judged(Default::default()),
    }
}

/// Copies the pristine subject into `scratch`'s work tree, every entry with its state folder, so a
/// row never touches the pristine copy.
fn lay_out_subject(pristine: &Path, scratch: &Scratch) -> Result<(), RunError> {
    let work = scratch.work();
    let io = |path: &Path| { let path = path.to_path_buf(); move |error: std::io::Error| RunError::Io(format!("{}: {}", path.display(), error)) };
    std::fs::create_dir_all(&work).map_err(io(&work))?;
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(pristine).map_err(io(pristine))?.collect::<Result<_, _>>().map_err(io(pristine))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        crate::scrub::copy_tree(&entry.path(), &work.join(entry.file_name())).map_err(|error| RunError::Io(format!("{:?}", error)))?;
    }
    Ok(())
}

/// The context of a faulted run over a history subject in `scratch`.
fn history_context<'a>(tool: Tool, corpus: &'a Corpus, programs: &'a Programs, scratch: &'a Scratch, environment: &'a Environment, limit: Duration, positions: &'a [Position]) -> Context<'a> {
    Context { tool, corpus, programs, scratch, environment, limit, then: &[], url: None, scope: None, via_host: false, history: Some(positions) }
}

/// The unfaulted readback of `tool`'s pristine subject, copied into `scratch`: every position and
/// the probe ref read, then the probe recorded with the subject's own record binding and the
/// probe ref read again, then the subject's whole-store check run as every row's is. A record
/// that does not read back unfaulted (an empty tree is read back; a walk ending short of it is
/// not), a probe that does not land, or a check that finds damage or repairs anything, is the
/// subject's refusal, the inner `Err` naming what refused: that subject's rows are then not run,
/// since a check reporting the pristine subject would report it on every row. The outer `Err` is
/// the harness's own failure to take the readback.
#[allow(clippy::too_many_arguments)]
pub fn baseline(tool: Tool, pristine: &Path, positions: &[Position], programs: &Programs, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<Result<Baseline, String>, RunError> {
    lay_out_subject(pristine, scratch)?;
    let corpus = probe_corpus();
    let context = history_context(tool, &corpus, programs, scratch, environment, limit, positions);
    let marks: Vec<Mark> = (0..positions.len()).map(Mark::Position).chain([Mark::Record(0)]).collect();
    let trees = |marks: &[Mark]| -> Result<Result<Vec<oracle::Tree>, String>, RunError> {
        let (export, served, reads) = context.serve_history(&scratch.work(), positions, marks)?;
        if !export.succeeded() {
            return Ok(Err(format!("the unfaulted readback refused: {:?} exited {:?}: {}", export.arguments, export.exit, said(&export))));
        }
        let total = served.len();
        let mut trees = Vec::new();
        let mut refused = Vec::new();
        for (mark, readback, reached, evidence) in served {
            match readback {
                Readback::Served { tree, .. } if reached => trees.push(tree),
                _ => refused.push(unserved(mark, reached, &evidence.iter().filter_map(|&index| reads.get(index)).collect::<Vec<_>>())),
            }
        }
        Ok(if refused.is_empty() { Ok(trees) } else { Err(format!("{} of {} records: {}", refused.len(), total, refused.join("; "))) })
    };
    let mut read = match trees(&marks)? { Ok(read) => read, Err(refused) => return Ok(Err(refused)) };
    let before = read.pop().unwrap_or_default();
    let mut steps = Vec::new();
    if !context.record(0, &mut steps)? {
        let refused = steps.last().map(|step| format!("{:?} exited {:?}", step.arguments, step.exit)).unwrap_or_default();
        return Ok(Err(format!("the unfaulted probe record did not land: {}", refused)));
    }
    let after = match trees(&[Mark::Record(0)])? { Ok(mut after) => after.pop().unwrap_or_default(), Err(refused) => return Ok(Err(refused)) };
    let checked = store_check(&context)?;
    if !checked.succeeded() || (tool == Tool::Forklift && heal_repaired(&checked.stdout)) {
        return Ok(Err(format!("the unfaulted store check reports the pristine subject: {:?} exited {:?}: {}{}", checked.arguments, checked.exit,
            String::from_utf8_lossy(&checked.stdout).trim(), String::from_utf8_lossy(&checked.stderr).trim())));
    }
    Ok(Ok(Baseline { positions: read, before, after }))
}

/// Runs one damage row over a history subject: its pristine subject copied into `scratch`, damage
/// `kind` applied to the path `name` resolves to there, then the probe record as the faulted
/// command, graded by [`grade_after`] against the subject's [`Baseline`] over the sampled
/// positions. A row graded healed then runs the subject's own whole-store check
/// ([`whole_store_check`]); damage only that check sees is [`Faulted::unreached`].
#[allow(clippy::too_many_arguments)]
pub fn run_history(subject: &HistorySubject, name: &str, kind: Kind, programs: &Programs, scratch: &Scratch, environment: &Environment, limit: Duration, seed: u64) -> Result<Faulted, RunError> {
    lay_out_subject(subject.pristine, scratch)?;
    let state = scratch.work().join(runner::state_folder(subject.tool));
    let walked = crate::census::walk(&state).map_err(|error| RunError::Io(format!("{}: {}", state.display(), error)))?;
    let resolved = match subject.tool {
        Tool::Forklift => resolve(name, &walked, seed),
        Tool::Git | Tool::GitTuned => crate::git_layout::resolve(name, &walked, seed),
    };
    let target = if kind.plants() {
        damage::planted(name)
    } else {
        resolved.ok_or_else(|| RunError::Io(format!("{} resolves to nothing in the history subject", name)))?
    };
    damage::apply(&state.join(&target), kind, seed).map_err(|error| RunError::Io(format!("{:?}", error)))?;

    let corpus = probe_corpus();
    let positions = subject.positions;
    let expected = Expected {
        acknowledged: (0..positions.len()).map(Mark::Position).collect(), faulted: Mark::Record(0),
        before: subject.baseline.before.clone(), after: subject.baseline.after.clone(),
        taken: Vec::new(), parked: Vec::new(), pushed: Vec::new(), positions: subject.baseline.positions.clone(),
    };
    let context = history_context(subject.tool, &corpus, programs, scratch, environment, limit, positions);
    let mut graded = grade_after(&context, &expected, 0, |context, steps| context.record(0, steps).map(drop))?;
    let unreached = if graded.grade == Grade::Healed { whole_store_check(&context, &mut graded)? } else { None };
    Ok(Faulted {
        scenario: HISTORY_SCENARIO.to_string(), name: name.to_string(), warehouse: Warehouse::Client, kind, target,
        grade: graded.grade, evidence: graded.evidence, recoverability: graded.recoverability,
        steps: graded.steps, readbacks: graded.readbacks, probes: graded.probes, unextractable: graded.unextractable, unreached,
    })
}

/// The subject's own whole-store inspection, run in the work tree once every sampled record read
/// back as expected, and added to the steps: `git fsck --full`, or forklift's `heal` (`audit`
/// refuses an unsigned import, and heal is the step that names corrupt objects). Its outcome when
/// it reports damage, or, for a heal that exits 0, repaired some: the damage the sample did not
/// reach. Nothing when it found none. The row was already healed, so a repair here rescues nothing.
fn whole_store_check(context: &Context, graded: &mut Graded) -> Result<Option<Outcome>, RunError> {
    let checked = store_check(context)?;
    graded.steps.push(checked.clone());
    let found = !checked.succeeded() || (context.tool == Tool::Forklift && heal_repaired(&checked.stdout));
    Ok(found.then_some(checked))
}

/// The subject's whole-store check, run in the work tree: `git fsck --full`, or forklift's `heal`.
fn store_check(context: &Context) -> Result<Outcome, RunError> {
    let arguments: &[&str] = match context.tool {
        Tool::Forklift => &["--json", "heal"],
        Tool::Git | Tool::GitTuned => &["fsck", "--full", "--no-progress"],
    };
    context.run(context.program(), context.subject(arguments.iter().map(|argument| argument.to_string()).collect()), context.scratch.work())
}

/// Whether a `--json heal` that exited 0 repaired anything: it restaged a path, or found the
/// warehouse tainted.
fn heal_repaired(stdout: &[u8]) -> bool {
    let document = runner::document(stdout);
    document["data"]["restaged"].as_array().is_some_and(|restaged| !restaged.is_empty()) || document["data"]["was_tainted"].as_bool() == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// The forklift walk follows first parents from the listed head, stops at the depth it was
    /// asked for, at a parcel the listing leaves out, and at a cycle; the pallet listing gives
    /// each pallet's head by name.
    ///
    /// Revert: follow a merge's last parent → the walk takes the side parcel; drop the bound →
    /// the bounded walk runs to the root; drop the cycle stop → the cyclic walk never ends.
    #[test]
    fn the_forklift_walk_follows_first_parents() {
        let listing = |entries: &[(&str, &[&str])]| serde_json::json!({ "data": { "entries": entries.iter()
            .map(|(parcel, parents)| serde_json::json!({ "parcel": parcel, "parents": parents })).collect::<Vec<_>>() } }).to_string().into_bytes();
        let merged = listing(&[("m", &["b", "s"]), ("s", &["a"]), ("b", &["a"]), ("a", &[])]);
        assert_eq!(first_parents(Some("m"), &merged, 10), ["m", "b", "a"]);
        assert_eq!(first_parents(None, &merged, 10), ["m", "b", "a"]);
        assert_eq!(first_parents(Some("m"), &merged, 2), ["m", "b"]);
        assert_eq!(first_parents(Some("s"), &merged, 10), ["s", "a"]);
        assert_eq!(first_parents(Some("m"), &listing(&[("m", &["gone"])]), 10), ["m"]);
        assert_eq!(first_parents(Some("x"), &listing(&[("x", &["y"]), ("y", &["x"])]), 10), ["x", "y"]);
        let pallets = serde_json::json!({ "data": { "pallets": [{ "name": "main", "head": "h1" }, { "name": "side", "head": "h2" }] } }).to_string().into_bytes();
        assert_eq!(pallet_heads(&pallets), [("main".to_string(), "h1".to_string()), ("side".to_string(), "h2".to_string())].into_iter().collect());
    }

    /// What forklift printed in a readback copy names the graded warehouse instead, so a remedy
    /// named by path acts on it: the remedy texts, read from the parsed failure, name it too.
    ///
    /// Revert: leave the parsed failure as it was → the remedy texts still name the copy.
    #[test]
    fn a_copy_path_is_written_back_as_the_graded_one() {
        let failure = crate::tool::Failure { code: "locked".into(), message: "held at /s/readback-0/.forklift/lock".into(), next_step: Some("remove /s/readback-0/.forklift/lock".into()) };
        let outcome = Outcome { arguments: vec![], exit: Some(1), stdout: b"remove /s/readback-0/.forklift/lock".to_vec(), stderr: b"at /s/readback-0".to_vec(), failure: Some(failure) };
        let rebased = rebased(outcome, Path::new("/s/readback-0"), Path::new("/s/work"));
        assert_eq!((rebased.stdout.as_slice(), rebased.stderr.as_slice()), (&b"remove /s/work/.forklift/lock"[..], &b"at /s/work"[..]));
        assert_eq!(rebased.remedy_texts(), ["held at /s/work/.forklift/lock", "remove /s/work/.forklift/lock"]);
    }

    /// A record a forklift walk ended short of is silently lost even where the baseline expects an
    /// empty tree, which an unreached record reads back as; a reached one is judged as read.
    ///
    /// Revert: judge every record as read → the unreached empty record is expected.
    #[test]
    fn an_unreached_record_is_silent_even_when_empty_is_expected() {
        let empty = Readback::Served { tree: Default::default(), documented: Default::default() };
        let expected = Expectation::acknowledged(Default::default());
        assert_eq!(judged_history(&empty, false, &expected), Verdict::Silent);
        assert_eq!(judged_history(&empty, true, &expected), Verdict::Expected);
    }

    use crate::scrub::path_git;

    /// A tuned git build, where this git has an fsmonitor daemon, runs one over the work tree it
    /// ends in (the clone's, for a history that clones) before any step, and the daemon goes with
    /// the build; its walk is classified either way, and git at its defaults runs none.
    ///
    /// Revert: skip starting the daemon after a clone → the lifted build's walk has no socket;
    /// drop both the daemon's stop and its kill → `status` still says it watches after the build
    /// is dropped.
    #[test]
    fn a_tuned_build_watches_its_work_tree_until_it_is_dropped() {
        let git = path_git();
        let daemon = runner::has_fsmonitor_daemon(&git);
        let corpus = corpus::generate(7, corpus::Shape::SMALL);
        for (id, tool) in [("history", Tool::GitTuned), ("lifted", Tool::GitTuned), ("history", Tool::Git)] {
            let scenario = crate::scenario::catalog().into_iter().find(|scenario| scenario.id == id).unwrap();
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let root = std::env::temp_dir().join(format!("proof-load-git-daemon-{}-{}", std::process::id(), nanos));
            let scratch = Scratch::create(&root).unwrap();
            let search = crate::scrub::search_path(Some(&git), std::env::var_os("PATH"));
            let environment = Environment::scrubbed(&scratch, search, &BTreeMap::new()).unwrap();
            let built = runner::build(&scenario, &corpus, tool, &git, None, &scratch, &environment, Duration::from_secs(60), None).unwrap();
            assert_eq!(crate::git_layout::unclassified(&built.walked), Vec::<&str>::new(), "{} {:?}", id, tool);
            let watched = daemon && tool == Tool::GitTuned;
            assert_eq!(built.walked.iter().any(|path| path == "fsmonitor--daemon/cookies/"), watched, "{} {:?}: {:?}", id, tool, built.walked);
            let status = || crate::tool::run(&crate::tool::Invocation {
                program: git.clone(), arguments: vec!["fsmonitor--daemon".into(), "status".into()], directory: scratch.work(), limit: Duration::from_secs(30),
            }, &scratch, &environment).unwrap().succeeded();
            assert_eq!(status(), watched, "{} {:?}", id, tool);
            drop(built);
            assert!(!status(), "{} {:?}: the daemon outlived its build", id, tool);
        }
    }

    /// Git's readback serves every acknowledged record from history whether the repository's index
    /// is intact or damaged, and leaves that index byte for byte as it was; the repository was made
    /// with background maintenance kept in the foreground.
    ///
    /// Revert: drop the scratch `GIT_INDEX_FILE` → the checkout over the damaged index refuses,
    /// and over the intact one it rewrites the repository's index; drop the created config → the
    /// config assertion.
    #[test]
    fn git_reads_back_from_history_and_leaves_the_index_alone() {
        let git = path_git();
        let scenario = crate::scenario::catalog().into_iter().find(|scenario| scenario.id == "history").unwrap();
        let corpus = corpus::generate(7, corpus::Shape { records: corpus::Shape::SMALL.records + 1, ..corpus::Shape::SMALL });
        let programs = Programs { forklift: PathBuf::from("forklift-not-used"), git: git.clone(), server: None };
        for damaged in [false, true] {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let root = std::env::temp_dir().join(format!("proof-load-git-readback-{}-{}", std::process::id(), nanos));
            let scratch = Scratch::create(&root).unwrap();
            let search = crate::scrub::search_path(Some(&git), std::env::var_os("PATH"));
            let environment = Environment::scrubbed(&scratch, search, &BTreeMap::new()).unwrap();
            let built = runner::build(&scenario, &corpus, Tool::Git, &git, None, &scratch, &environment, Duration::from_secs(60), None).unwrap();
            let config = std::fs::read_to_string(scratch.work().join(".git/config")).unwrap();
            assert!(config.contains("[gc]\n\tautoDetach = false") && config.contains("[maintenance]\n\tautoDetach = false"), "{}", config);
            let index = scratch.work().join(".git/index");
            if damaged {
                damage::apply(&index, Kind::FlipByte, 3).unwrap();
            }
            let before = std::fs::read(&index).unwrap();

            let (expected, _) = expected(&scenario, &corpus, &built, Warehouse::Client).unwrap();
            let context = Context {
                tool: Tool::Git, corpus: &corpus, programs: &programs, scratch: &scratch, environment: &environment,
                limit: Duration::from_secs(60), then: &[], url: None, scope: None, via_host: false, history: None,
            };
            let readback = context.read_back(&scratch.work(), &expected.acknowledged, &expected.acknowledged, |mark| expected.of(&corpus, mark, false)).unwrap();
            assert!(readback.export.succeeded(), "damaged {}: the log ran", damaged);
            assert_eq!(readback.records.len(), 3);
            for record in &readback.records {
                assert_eq!(record.verdicts, [Verdict::Expected], "damaged {}: {:?}", damaged, record.mark);
            }
            assert!(std::fs::read(&index).unwrap() == before, "damaged {}: the index is left as it was", damaged);
        }
    }

    /// Git's own remedy set restores a deleted ref to its reflog's newest entry, git's own
    /// `update-ref` doing the write, so the stash it held is listed again; a zeroed one git itself
    /// refuses to rewrite, and nothing rescues it. Reading the log file is outside knowledge. Either
    /// way the listing first served the stash as missing with exit 0, which stays silent.
    ///
    /// Revert: skip the restore → the deleted ref is never rescued; restore without asking
    /// whether the ref resolves → the intact branch is rewritten too, and its steps show it.
    #[test]
    fn git_restores_a_lost_ref_from_its_reflog_and_only_with_its_own_command() {
        let git = path_git();
        let scenario = crate::scenario::catalog().into_iter().find(|scenario| scenario.id == "parked").unwrap();
        let corpus = corpus::generate(7, corpus::Shape { records: corpus::Shape::SMALL.records + 1, ..corpus::Shape::SMALL });
        let programs = Programs { forklift: PathBuf::from("forklift-not-used"), git: git.clone(), server: None };
        for (kind, rescued) in [(Kind::Delete, Some(Rescue::LocalRepair)), (Kind::Zero, None)] {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let root = std::env::temp_dir().join(format!("proof-load-git-restore-{}-{}", std::process::id(), nanos));
            let scratch = Scratch::create(&root).unwrap();
            let search = crate::scrub::search_path(Some(&git), std::env::var_os("PATH"));
            let environment = Environment::scrubbed(&scratch, search, &BTreeMap::new()).unwrap();
            let faulted = run(&scenario, "refs/stash", Warehouse::Client, kind, Tool::Git, &corpus, &programs, &scratch, &environment, Duration::from_secs(60), 0).unwrap();
            let restores: Vec<&Outcome> = faulted.steps.iter().filter(|step| step.arguments.first().map(String::as_str) == Some("update-ref")).collect();
            assert_eq!(restores.len(), 1, "{:?}: the stash restored, the intact branch left alone", kind);
            assert_eq!(restores[0].arguments[1], "refs/stash");
            assert_eq!(restores[0].succeeded(), rescued.is_some(), "{:?}", kind);
            assert_eq!(faulted.recoverability.outside_knowledge, ["read the newest entry of .git/logs/refs/stash"], "{:?}", kind);
            assert_eq!(faulted.recoverability.rescued_by, rescued, "{:?}: {:?}", kind, faulted.recoverability);
            assert_eq!(faulted.evidence.acknowledged_readable, rescued.is_some(), "{:?}", kind);
            assert_eq!(faulted.grade, Grade::Silent, "{:?}", kind);
        }
    }

    /// Git hosts a bare repository at the host path, which its URL is: the built history pushed
    /// every record there and tracks it, and the walk of the host lists only classified paths. A
    /// damaged host is read back through a fresh clone of it; a broken branch on the client, which
    /// git cannot rewrite, is rescued by cloning the host again.
    ///
    /// Revert: read a host-side git readback from the work tree → the host's lost object is not
    /// what is judged; leave git rows without a URL → no re-clone is tried; clone the host
    /// locally, for the readback or the re-clone → the clone's objects are the host's files (held
    /// on unix only, where a file's link count is read);
    /// check the readback clone out → it holds a work tree.
    #[test]
    fn git_hosts_a_bare_repository_and_clones_it_again() {
        let git = path_git();
        let scenario = crate::scenario::catalog().into_iter().find(|scenario| scenario.id == "lifted").unwrap();
        assert!(scenario.tools().contains(&Tool::Git));
        let corpus = corpus::generate(7, corpus::Shape { records: corpus::Shape::SMALL.records + 1, ..corpus::Shape::SMALL });
        let programs = Programs { forklift: PathBuf::from("forklift-not-used"), git: git.clone(), server: None };
        let area = |label: &str| {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let scratch = Scratch::create(&std::env::temp_dir().join(format!("proof-load-git-{}-{}-{}", label, std::process::id(), nanos))).unwrap();
            let environment = Environment::scrubbed(&scratch, crate::scrub::search_path(Some(&git), std::env::var_os("PATH")), &BTreeMap::new()).unwrap();
            (scratch, environment)
        };

        let (scratch, environment) = area("built");
        let built = runner::build(&scenario, &corpus, Tool::Git, &git, None, &scratch, &environment, Duration::from_secs(60), None).unwrap();
        assert_eq!(built.url(), Some(runner::host_root(&scratch).display().to_string().as_str()));
        assert_eq!(built.pushed, built.acknowledged);
        assert_eq!(crate::git_layout::unclassified(&built.host_walked), Vec::<&str>::new());
        assert!(!crate::git_layout::reached("refs/heads/**", &built.host_walked).is_empty(), "{:?}", built.host_walked);
        assert!(!crate::git_layout::reached("refs/remotes/**", &built.walked).is_empty(), "{:?}", built.walked);

        let (scratch, environment) = area("host");
        let faulted = run(&scenario, "objects/<xx>/<hash>", Warehouse::Host, Kind::Delete, Tool::Git, &corpus, &programs, &scratch, &environment, Duration::from_secs(60), 0).unwrap();
        // Which object is lost follows the commits' hashes, so it differs from run to run, and so
        // does whether the push is refused. Every host object is reachable from its branch, so
        // the clone of the host refuses: the records it acknowledged are lost.
        assert_eq!(faulted.grade, Grade::DataLost, "{:?}", faulted.recoverability);
        assert_eq!(faulted.recoverability.rescued_by, None);

        let (scratch, environment) = area("client");
        let faulted = run(&scenario, "refs/heads/**", Warehouse::Client, Kind::Zero, Tool::Git, &corpus, &programs, &scratch, &environment, Duration::from_secs(60), 0).unwrap();
        assert_eq!(faulted.recoverability.rescued_by, Some(Rescue::Reclone), "{:?}", faulted.recoverability);
        assert!(faulted.steps.iter().any(|step| step.arguments.first().map(String::as_str) == Some("clone") && step.succeeded()), "{:?}", faulted.steps);
        assert_owns_its_objects(&scratch.root().join("reclone-0"));

        // A host the readback can clone: the clone came through git's transport, so it shares
        // no object file with the host, and holds no work tree.
        let (scratch, environment) = area("readback");
        run(&scenario, "config", Warehouse::Host, Kind::Delete, Tool::Git, &corpus, &programs, &scratch, &environment, Duration::from_secs(60), 0).unwrap();
        let clone = scratch.root().join("clone-0");
        assert_owns_its_objects(&clone);
        let entries: Vec<_> = std::fs::read_dir(&clone).unwrap().map(|entry| entry.unwrap().file_name()).collect();
        assert_eq!(entries, vec![std::ffi::OsString::from(".git")]);
    }

    /// Every object file of the clone at `repository` is its own: a local clone hardlinks the
    /// host's.
    fn assert_owns_its_objects(repository: &Path) {
        let objects = repository.join(".git").join("objects");
        let mut files = 0;
        let mut pending = vec![objects.clone()];
        while let Some(folder) = pending.pop() {
            for entry in std::fs::read_dir(&folder).unwrap() {
                let entry = entry.unwrap();
                let metadata = std::fs::symlink_metadata(entry.path()).unwrap();
                if metadata.is_dir() {
                    pending.push(entry.path());
                } else {
                    files += 1;
                    #[cfg(unix)]
                    assert_eq!(std::os::unix::fs::MetadataExt::nlink(&metadata), 1, "{}", entry.path().display());
                }
            }
        }
        assert!(files > 0, "{} holds no object", objects.display());
    }

    /// Git tuned runs every command of its own with its configuration first, as its bindings do;
    /// the other subjects run theirs as given.
    ///
    /// Revert: run git tuned's own remedies bare → its restores and checks are plain git's.
    #[test]
    fn git_tuned_runs_its_own_remedies_tuned() {
        let (corpus, programs) = (corpus::generate(7, corpus::Shape::SMALL), Programs { forklift: PathBuf::new(), git: PathBuf::new(), server: None });
        let scratch = Scratch::create(&std::env::temp_dir().join(format!("proof-load-subject-{}", std::process::id()))).unwrap();
        let environment = Environment::scrubbed(&scratch, None, &BTreeMap::new()).unwrap();
        for (tool, prefix) in [(Tool::GitTuned, crate::scenario::GIT_TUNED), (Tool::Git, &[][..]), (Tool::Forklift, &[][..])] {
            let context = Context {
                tool, corpus: &corpus, programs: &programs, scratch: &scratch, environment: &environment,
                limit: Duration::from_secs(1), then: &[], url: None, scope: None, via_host: false, history: None,
            };
            let expected: Vec<String> = prefix.iter().map(|argument| argument.to_string()).chain(["fsck".to_string()]).collect();
            assert_eq!(context.subject(vec!["fsck".to_string()]), expected, "{:?}", tool);
        }
    }

    /// A ref only ever moves forward, to a strict descendant of what it names, so a reflog whose
    /// newest entry is older cannot rewind it; and the probes that decide a restore are no steps
    /// the subject was run for.
    ///
    /// Revert: restore every ref whose value differs from its reflog's tip → `main` is rewound to
    /// its first commit; push the probes as steps → `rev-parse` and `rev-list` are counted; run git
    /// tuned's own remedies bare → its steps lack its configuration.
    #[test]
    fn a_ref_only_moves_forward_and_probes_are_no_steps() {
        for tool in [Tool::Git, Tool::GitTuned] {
            resolving_ref_as(tool);
        }
    }

    fn resolving_ref_as(tool: Tool) {
        let git = path_git();
        let scenario = crate::scenario::catalog().into_iter().find(|scenario| scenario.id == "parked").unwrap();
        let corpus = corpus::generate(7, corpus::Shape { records: corpus::Shape::SMALL.records + 1, ..corpus::Shape::SMALL });
        let programs = Programs { forklift: PathBuf::from("forklift-not-used"), git: git.clone(), server: None };
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let scratch = Scratch::create(&std::env::temp_dir().join(format!("proof-load-git-rewind-{}-{}", std::process::id(), nanos))).unwrap();
        let search = crate::scrub::search_path(Some(&git), std::env::var_os("PATH"));
        let environment = Environment::scrubbed(&scratch, search, &BTreeMap::new()).unwrap();
        let built = runner::build(&scenario, &corpus, tool, &git, None, &scratch, &environment, Duration::from_secs(60), None).unwrap();
        let context = Context {
            tool, corpus: &corpus, programs: &programs, scratch: &scratch, environment: &environment,
            limit: Duration::from_secs(60), then: &[], url: None, scope: None, via_host: false, history: None,
        };
        let head = |reference: &str| String::from_utf8_lossy(&context.run(&git, vec!["rev-parse".into(), reference.into()], scratch.work()).unwrap().stdout).trim().to_string();
        let (main, first) = (head("refs/heads/main"), head("refs/heads/main~1"));
        let log = scratch.work().join(".git/logs/refs/heads/main");
        let mut lines = std::fs::read_to_string(&log).unwrap();
        lines.push_str(&format!("{} {} A <a@b> 1 +0000\treset\n", main, first));
        std::fs::write(&log, lines).unwrap();
        damage::apply(&scratch.work().join(".git/refs/stash"), Kind::Delete, 0).unwrap();

        let (expected, _) = expected(&scenario, &corpus, &built, Warehouse::Client).unwrap();
        let mut trail = Trail::default();
        let mut made = Made::default();
        own_remedies(&context, &expected, &mut trail, &mut made, Made::default()).unwrap();
        assert_eq!(head("refs/heads/main"), main, "the intact branch stays where it was");
        // Git tuned runs each of its own remedies with its configuration first.
        let prefix = if tool == Tool::GitTuned { crate::scenario::GIT_TUNED.len() } else { 0 };
        for step in &trail.steps {
            assert_eq!(&step.arguments[..prefix], crate::scenario::GIT_TUNED[..prefix].iter().map(|a| a.to_string()).collect::<Vec<_>>(), "{:?}", tool);
        }
        let ran: Vec<&str> = trail.steps.iter().map(|step| step.arguments[prefix].as_str()).collect();
        assert_eq!(ran, ["fsck", "update-ref"], "{:?}", tool);
        assert_eq!(trail.steps[1].arguments[prefix + 1], "refs/stash");
        // The probes are kept, apart from the steps: each ref's rev-parse, the stash's rev-list.
        let probed: Vec<&str> = trail.probes.iter().map(|probe| probe.arguments[prefix].as_str()).collect();
        assert!(probed.contains(&"rev-parse") && probed.contains(&"rev-list"), "{:?}: {:?}", tool, probed);
        assert_eq!(trail.recoverability.rescued_by, Some(Rescue::LocalRepair));
    }

    /// A branch whose loose file is gone while packed-refs still holds an older commit resolves,
    /// silently, to that commit: the update is lost, and its reflog's newest entry restores it.
    ///
    /// Revert: skip every ref that resolves → `main` stays on the packed, older commit.
    #[test]
    fn a_loose_ref_lost_over_an_older_packed_one_is_restored() {
        let git = path_git();
        let scenario = crate::scenario::catalog().into_iter().find(|scenario| scenario.id == "parked").unwrap();
        let corpus = corpus::generate(7, corpus::Shape { records: corpus::Shape::SMALL.records + 1, ..corpus::Shape::SMALL });
        let programs = Programs { forklift: PathBuf::from("forklift-not-used"), git: git.clone(), server: None };
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let scratch = Scratch::create(&std::env::temp_dir().join(format!("proof-load-git-packed-{}-{}", std::process::id(), nanos))).unwrap();
        let search = crate::scrub::search_path(Some(&git), std::env::var_os("PATH"));
        let environment = Environment::scrubbed(&scratch, search, &BTreeMap::new()).unwrap();
        let built = runner::build(&scenario, &corpus, Tool::Git, &git, None, &scratch, &environment, Duration::from_secs(60), None).unwrap();
        let context = Context {
            tool: Tool::Git, corpus: &corpus, programs: &programs, scratch: &scratch, environment: &environment,
            limit: Duration::from_secs(60), then: &[], url: None, scope: None, via_host: false, history: None,
        };
        let git_run = |arguments: &[&str]| context.run(&git, arguments.iter().map(|argument| argument.to_string()).collect(), scratch.work()).unwrap();
        let head = |reference: &str| String::from_utf8_lossy(&git_run(&["rev-parse", reference]).stdout).trim().to_string();
        let (main, first) = (head("refs/heads/main"), head("refs/heads/main~1"));
        assert!(git_run(&["update-ref", "refs/heads/main", &first]).succeeded());
        assert!(git_run(&["pack-refs", "--all"]).succeeded());
        assert!(git_run(&["update-ref", "refs/heads/main", &main]).succeeded());
        damage::apply(&scratch.work().join(".git/refs/heads/main"), Kind::Delete, 0).unwrap();
        assert_eq!(head("refs/heads/main"), first, "the branch fell back to its packed value");

        let (expected, _) = expected(&scenario, &corpus, &built, Warehouse::Client).unwrap();
        let (mut trail, mut made) = (Trail::default(), Made::default());
        own_remedies(&context, &expected, &mut trail, &mut made, Made::default()).unwrap();
        assert_eq!(head("refs/heads/main"), main, "the lost update is restored");
        assert!(trail.steps.iter().any(|step| step.arguments[..2] == ["update-ref".to_string(), "refs/heads/main".to_string()]));
    }

    /// The newest entry of each ref's reflog, in path order; a log whose last line names no commit
    /// (a ref's deletion, or garbage) names nothing, and HEAD's own log is not a ref's.
    ///
    /// Revert: take the first line → an older commit; keep the zero hash → a deleted ref is
    /// "restored" to nothing.
    #[test]
    fn a_reflog_tip_is_its_newest_entry() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let git = std::env::temp_dir().join(format!("proof-load-reflog-tips-{}-{}", std::process::id(), nanos));
        let (a, b, zero) = ("a".repeat(40), "b".repeat(40), "0".repeat(40));
        let line = |old: &str, new: &str| format!("{} {} A <a@b> 1 +0000\tcommit\n", old, new);
        std::fs::create_dir_all(git.join("logs/refs/heads/topic")).unwrap();
        std::fs::write(git.join("logs/HEAD"), line(&zero, &a)).unwrap();
        std::fs::write(git.join("logs/refs/heads/main"), line(&zero, &a) + &line(&a, &b) + "\n").unwrap();
        std::fs::write(git.join("logs/refs/heads/topic/x"), line(&zero, &a)).unwrap();
        std::fs::write(git.join("logs/refs/heads/gone"), line(&zero, &a) + &line(&a, &zero)).unwrap();
        std::fs::write(git.join("logs/refs/stash"), "not a log line\n").unwrap();
        assert_eq!(reflog_tips(&git), [("refs/heads/main".to_string(), b.clone()), ("refs/heads/topic/x".to_string(), a.clone())]);
        assert!(reflog_tips(&git.join("absent")).is_empty());
    }

    fn walked() -> Vec<String> {
        ["objects/", "objects/ab/", "objects/ab/11", "objects/cd/", "objects/cd/22", "pallets/", "pallets/main", "warehouse"]
            .iter().map(|path| path.to_string()).collect()
    }

    /// An unserved record names its own evidence: a walk that ended short of it, whatever else
    /// refused, or else the first of its reads that refused, with its failure's message or its
    /// stderr, a long one cut and marked.
    ///
    /// Revert: drop the refused read → the CI history run's refusal names no command; check the
    /// walk last → a short walk is blamed on a read that refused.
    #[test]
    fn an_unserved_record_names_the_read_that_refused() {
        let ran = |arguments: &[&str], exit: Option<i32>, stderr: &str, message: Option<&str>| Outcome {
            arguments: arguments.iter().map(|argument| argument.to_string()).collect(), exit, stdout: vec![], stderr: stderr.as_bytes().to_vec(),
            failure: message.map(|message| crate::tool::Failure { code: "x".into(), message: message.into(), next_step: None }),
        };
        let reads = [ran(&["walk"], Some(0), "", None), ran(&["lay", "out"], Some(2), "noise", Some("no parcel abc")), ran(&["open"], Some(1), "later", None)];
        let reads: Vec<&Outcome> = reads.iter().collect();
        assert_eq!(unserved(Mark::Position(0), true, &reads), r#"the unfaulted readback did not serve Position(0): ["lay", "out"] exited Some(2): no parcel abc"#);
        assert_eq!(unserved(Mark::Position(1), true, &reads[2..]), r#"the unfaulted readback did not serve Position(1): ["open"] exited Some(1): later"#);
        assert_eq!(unserved(Mark::Position(2), false, &reads[..1]), "the unfaulted readback did not serve Position(2): its walk ended short of it");
        assert_eq!(unserved(Mark::Position(3), false, &reads), "the unfaulted readback did not serve Position(3): its walk ended short of it");
        let long = ran(&["open"], Some(1), &"é".repeat(500), None);
        assert_eq!(said(&long), format!("{}…", "é".repeat(400)));
        assert_eq!(said(reads[1]), "no parcel abc");
    }

    /// A pattern resolves to one of the paths it matches, chosen by the seed, a folder without its
    /// trailing slash; a pattern nothing matches resolves to nothing.
    ///
    /// Revert: always take the first match → seed 1 damages `objects/ab/11` like seed 0.
    #[test]
    fn a_pattern_resolves_to_a_seeded_match() {
        assert_eq!(resolve("objects/<xx>/<hash>", &walked(), 0).as_deref(), Some("objects/ab/11"));
        assert_eq!(resolve("objects/<xx>/<hash>", &walked(), 1).as_deref(), Some("objects/cd/22"));
        assert_eq!(resolve("objects/<xx>/", &walked(), 0).as_deref(), Some("objects/ab"));
        assert_eq!(resolve("warehouse", &walked(), 5).as_deref(), Some("warehouse"));
        assert_eq!(resolve("trust", &walked(), 0), None);
    }

    /// A retry resumes at the line that failed or was cut, at the first never reached after an
    /// attempt that stopped early, and at the start after one that ran every line.
    ///
    /// Revert: always resume at the start → a repack refused after its record stacked is retried
    /// as a re-stack, which refuses as nothing to stack, and the repack never runs again.
    #[test]
    fn a_retry_resumes_at_the_line_that_did_not_land() {
        let exited = |exit: Option<i32>| Outcome { arguments: vec![], exit, stdout: vec![], stderr: vec![], failure: None };
        assert_eq!(resume_after(&[exited(Some(0)), exited(Some(0)), exited(Some(1))], 3).at, 2);
        assert_eq!(resume_after(&[exited(Some(0)), exited(None)], 3).at, 1);
        assert_eq!(resume_after(&[exited(Some(0))], 3).at, 1);
        assert_eq!(resume_after(&[exited(Some(0)), exited(Some(0)), exited(Some(0))], 3), Resume::START);
    }

    /// A stack refused as nothing to stack is a record that already stands; any other refusal, or
    /// that message from another command, is not.
    ///
    /// Revert: take every refused stack as stacked → a stack refused for a full disk reads as done.
    #[test]
    fn only_nothing_to_stack_is_already_stacked() {
        let refused = |arguments: &[&str], message: &str| Outcome {
            arguments: arguments.iter().map(|argument| argument.to_string()).collect(), exit: Some(1), stdout: vec![], stderr: vec![],
            failure: Some(tool::Failure { code: "error".into(), message: message.into(), next_step: None }),
        };
        assert!(already_stacked(&refused(&["--json", "stack", "-m", "r"], "Nothing to stack: the inventory matches the head of pallet \"main\".")));
        assert!(!already_stacked(&refused(&["--json", "stack", "-m", "r"], "Error while writing file: No space left on device (os error 28)")));
        assert!(!already_stacked(&refused(&["--json", "load", "."], "Nothing to stack")));
    }

    /// A retry counts nothing-to-stack as landed only on the line it resumes at, and only when the
    /// earlier attempt failed there for another reason; the same refusal from a line it reaches for
    /// the first time, or after the earlier attempt was refused the same way, stops it there.
    ///
    /// Revert: count it on any line, or drop the earlier failure's reason → a stack refused as
    /// nothing to stack reads as landed.
    #[test]
    fn a_retry_takes_nothing_to_stack_as_landed_only_where_it_resumes() {
        let stack = || vec!["--json".to_string(), "stack".into(), "-m".into(), "r".into()];
        let refused = |arguments: Vec<String>, message: &str| Outcome {
            arguments, exit: Some(1), stdout: vec![], stderr: vec![],
            failure: Some(tool::Failure { code: "error".into(), message: message.into(), next_step: None }),
        };
        let landed = |arguments: Vec<String>| Outcome { arguments, exit: Some(0), stdout: vec![], stderr: vec![], failure: None };
        let lines = vec![vec!["--json".to_string(), "load".into(), ".".into()], stack(), stack(), vec!["--json".to_string(), "store".into(), "repack".into()]];
        let nothing = |arguments| Ok::<_, ()>(refused(arguments, "Nothing to stack"));

        // The first stack failed for a full disk, possibly after its parcel landed: resumed there,
        // nothing to stack means it did, and the retry goes on, until the second stack, run for the
        // first time, is refused the same way and stops it.
        let full = resume_after(&[landed(lines[0].clone()), refused(stack(), "No space left on device (os error 28)")], 4);
        assert_eq!(full, Resume { at: 1, may_have_stacked: true });
        let mut steps = Vec::new();
        let ran = resume_lines(lines.clone(), full, &mut steps, nothing).unwrap();
        assert_eq!(ran, (false, Resume { at: 2, may_have_stacked: false }));
        assert_eq!(steps.len(), 2);

        // The first stack was itself refused as nothing to stack: nothing landed, and the same
        // refusal on the retry stops it at once.
        let empty = resume_after(&[landed(lines[0].clone()), refused(stack(), "Nothing to stack")], 4);
        assert_eq!(empty, Resume { at: 1, may_have_stacked: false });
        let mut steps = Vec::new();
        let ran = resume_lines(lines.clone(), empty, &mut steps, nothing).unwrap();
        assert_eq!(ran, (false, empty));
        assert_eq!(steps.len(), 1);

        // Every line from the resume point landed: the next retry starts over.
        let mut steps = Vec::new();
        let ran = resume_lines(lines, Resume { at: 3, may_have_stacked: true }, &mut steps, |arguments| Ok::<_, ()>(landed(arguments))).unwrap();
        assert_eq!(ran, (true, Resume::START));
        assert_eq!(steps.len(), 1);
    }

    fn served(subjects: &[(usize, Verdict)]) -> HistoryReadback {
        let export = Outcome { arguments: vec![], exit: Some(0), stdout: vec![], stderr: vec![], failure: None };
        let mut records: BTreeMap<Mark, Vec<Verdict>> = BTreeMap::new();
        for (index, verdict) in subjects {
            records.entry(Mark::Record(*index)).or_default().push(*verdict);
        }
        HistoryReadback { export, records: records.into_iter().map(|(mark, verdicts)| RecordReadback { mark, verdicts }).collect(), parked: None, reads: Vec::new() }
    }

    /// A readback's details keep a refused read whole, and a successful one without its stdout,
    /// whose length they keep; a silent readback keeps every read whole.
    ///
    /// Revert: serialize the reads as they are → the served bytes are written; drop every
    /// stdout → the refused read's own words are lost; drop the stdout of a silent readback's
    /// successful read → the bytes that show the wrong tree are lost.
    #[test]
    fn a_readback_writes_what_its_reads_refused_not_what_they_served() {
        let read = |exit, stdout: &[u8]| Outcome { arguments: vec!["show".into()], exit: Some(exit), stdout: stdout.to_vec(), stderr: b"said".to_vec(), failure: None };
        let readback = HistoryReadback { reads: vec![read(0, b"served-bytes"), read(1, b"refused-words")], ..served(&[(0, Verdict::Expected)]) };
        let written = serde_json::to_value(&readback).unwrap();
        let reads = written["reads"].as_array().unwrap();
        assert_eq!(reads[0].get("stdout"), None, "{}", written);
        assert_eq!(reads[0]["stdout_bytes"], 12);
        assert_eq!(reads[0]["stderr"], serde_json::json!(b"said".to_vec()));
        assert_eq!(reads[1]["stdout"], serde_json::json!(b"refused-words".to_vec()));
        assert_eq!(reads[1].get("stdout_bytes"), None);
        assert_eq!(reads[1]["exit"], 1);
        let silent = HistoryReadback { reads: vec![read(0, b"wrong-tree")], ..served(&[(0, Verdict::Silent)]) };
        let written = serde_json::to_value(&silent).unwrap();
        assert_eq!(written["reads"][0]["stdout"], serde_json::json!(b"wrong-tree".to_vec()), "{}", written);
        assert_eq!(written["reads"][0].get("stdout_bytes"), None);
        assert_eq!(written["records"][0]["verdicts"], serde_json::json!(["silent"]));
    }

    /// A history reads only when every wanted record was served and every copy of it expected: a
    /// record the export left out does not read, and neither does one with a silent copy.
    ///
    /// Revert: count a record with no exported commit as read → a deleted pallet's history passes.
    #[test]
    fn a_record_left_out_of_the_export_does_not_read() {
        let full = served(&[(0, Verdict::Expected), (1, Verdict::Expected)]);
        assert!(full.reads(&[Mark::Record(0), Mark::Record(1)]));
        let missing = HistoryReadback { records: vec![RecordReadback { mark: Mark::Record(0), verdicts: vec![Verdict::Expected] }, RecordReadback { mark: Mark::Record(1), verdicts: vec![] }], ..full.clone() };
        assert!(!missing.reads(&[Mark::Record(0), Mark::Record(1)]));
        assert!(missing.reads(&[Mark::Record(0)]));
        let doubled = served(&[(0, Verdict::Expected), (0, Verdict::Silent)]);
        assert!(!doubled.reads(&[Mark::Record(0)]));
        assert!(doubled.silent());
        let refused = HistoryReadback { export: Outcome { exit: Some(1), ..full.export.clone() }, ..full };
        assert!(!refused.reads(&[Mark::Record(0), Mark::Record(1)]));
    }

    /// Work the history left parked reads only while the listing still names it: one a listing
    /// that exited 0 left out is silent, unless the listing is a fresh clone's, which owes no park
    /// and so has lost it; and a listing that refused is a refusal.
    ///
    /// Revert: ignore the parked readback in `reads` → a heal that drops the park passes; ignore
    /// `owed` → a re-clone, which never holds a park, grades every parked scenario silent.
    #[test]
    fn parked_work_reads_only_while_it_is_listed() {
        let full = served(&[(0, Verdict::Expected)]);
        let listed = |exit: i32, missing: &[&str], owed: bool| HistoryReadback { parked: Some(ParkedReadback {
            listing: Outcome { exit: Some(exit), ..full.export.clone() }, missing: missing.iter().map(|parcel| parcel.to_string()).collect(), owed,
        }), ..full.clone() };
        let parked = |exit: i32, missing: &[&str]| listed(exit, missing, true);
        assert!(parked(0, &[]).reads(&[Mark::Record(0)]));
        let dropped = parked(0, &["p"]);
        assert!(!dropped.reads(&[Mark::Record(0)]));
        assert!(dropped.silent());
        let refused = parked(1, &["p"]);
        assert!(!refused.reads(&[Mark::Record(0)]));
        assert!(!refused.silent() && refused.refused());
        let recloned = listed(0, &["p"], false);
        assert!(!recloned.reads(&[Mark::Record(0)]) && !recloned.silent() && !recloned.refused());

        // A listing that names no parked list, or an entry without a parcel, is not read as none.
        assert_eq!(runner::parked_parcels(br#"{"data":{"parked":[{"parcel":"p"}]}}"#), Some(vec!["p".to_string()]));
        assert_eq!(runner::parked_parcels(br#"{"data":{}}"#), None);
        assert_eq!(runner::parked_parcels(br#"{"data":{"parked":[{"name":"n"}]}}"#), None);
    }

    /// An export's left-out files are documented per record by their parcels: withheld content as
    /// `content_withheld`, redacted as `content_redacted`, and only in the records whose parcels
    /// the export names. Only an export that left something out needs the history listing, and
    /// with no listing a left-out file is admitted as documented in every record whose expectation
    /// opens its path, and in no other.
    ///
    /// Revert: key every entry to every record → a record that never held the file expects a
    /// documented code for it, and an export serving it correctly reads silent; pair no record in
    /// `in_every_record` → a documented left-out file reads silent when the listing refuses; keep
    /// every path in `where_transformed` → a later record that never held the bytes reads silent.
    #[test]
    fn left_out_files_are_documented_in_the_records_naming_them() {
        let export = serde_json::json!({ "data": {
            "withheld": [{ "hash": "h1", "parcels": ["p1", "p2"], "path": "src/a", "statement": "s" }],
            "redacted": [{ "hash": "h2", "parcels": ["p2"], "path": "b" }],
        }});
        let described = vec![("r2".to_string(), "p2".to_string()), ("r1".to_string(), "p1".to_string()), ("r0".to_string(), "p0".to_string())];
        let left_out = left_out(&export, &described);
        assert_eq!(left_out.get("r1"), Some(&BTreeMap::from([("src/a".to_string(), "content_withheld".to_string())])));
        assert_eq!(left_out.get("r2"), Some(&BTreeMap::from([
            ("b".to_string(), "content_redacted".to_string()), ("src/a".to_string(), "content_withheld".to_string()),
        ])));
        assert_eq!(left_out.get("r0"), None);
        assert!(super::left_out(&serde_json::json!({ "data": {} }), &described).is_empty());
        assert!(leaves_out(&export));
        assert!(!leaves_out(&serde_json::json!({ "data": { "withheld": [], "redacted": [] } })));
        assert!(!leaves_out(&serde_json::json!({ "data": {} })));
        let marks = [Mark::Record(0), Mark::Record(1)];
        let everywhere = super::left_out(&export, &in_every_record(&export, &marks));
        for mark in marks {
            assert_eq!(everywhere.get(&mark.message()).map(|paths| paths.keys().cloned().collect::<Vec<_>>()), Some(vec!["b".to_string(), "src/a".to_string()]));
        }
        // Record 0 held the withheld bytes at src/a; record 1 holds nothing taken.
        let opened = |mark: Mark| Expectation {
            transformed: if mark == Mark::Record(0) { BTreeMap::from([("src/a".to_string(), vec![])]) } else { BTreeMap::new() },
            ..Expectation::acknowledged(BTreeMap::new())
        };
        // Judged: record 1 read back correctly (empty, as recorded) under the documented map it gets.
        let judged = |left_out: &BTreeMap<String, BTreeMap<String, String>>| oracle::judge(
            &Readback::Served { tree: BTreeMap::new(), documented: left_out.get(&Mark::Record(1).message()).cloned().unwrap_or_default() },
            &opened(Mark::Record(1)),
        );
        assert_eq!(judged(&everywhere), Verdict::Silent);
        let kept = where_transformed(everywhere, &marks, opened);
        assert_eq!(kept, BTreeMap::from([(Mark::Record(0).message(), BTreeMap::from([("src/a".to_string(), "content_withheld".to_string())]))]));
        assert_eq!(judged(&kept), Verdict::Expected);
    }

    /// Content taken away opens every path holding its bytes, in every record and in both trees
    /// of the faulted record, to what a read of it may produce; other paths stay fixed.
    ///
    /// Revert: open only the path the harness picked → the same bytes at another path, or in an
    /// earlier record, expect them served though the warehouse reports them taken away.
    #[test]
    fn taken_content_opens_every_path_holding_its_bytes() {
        let snapshot = |files: &[(&str, &str)]| -> crate::corpus::Snapshot { files.iter().map(|(path, bytes)| (path.to_string(), bytes.as_bytes().to_vec())).collect() };
        let corpus = Corpus { seed: 0, shape: crate::corpus::Shape::SMALL, records: vec![
            snapshot(&[("a", "x"), ("b", "y")]), snapshot(&[("a", "x"), ("c", "x"), ("b", "z")]), snapshot(&[("b", "w")]),
        ], modes: Default::default() };
        let documented = vec![Admissible::Documented { code: "content_withheld".into() }];
        let expected = Expected {
            acknowledged: vec![Mark::Record(0), Mark::Record(1)], faulted: Mark::Record(2),
            before: corpus::expected(&corpus.records[1], &corpus.modes), after: corpus::expected(&corpus.records[2], &corpus.modes),
            taken: vec![(oracle::digest(b"x"), documented.clone())], parked: Vec::new(), pushed: Vec::new(), positions: Vec::new(),
        };
        let first = expected.of(&corpus, Mark::Record(0), false);
        assert_eq!(first.transformed, BTreeMap::from([("a".to_string(), documented.clone())]));
        let second = expected.of(&corpus, Mark::Record(1), false);
        assert_eq!(second.transformed, BTreeMap::from([("a".to_string(), documented.clone()), ("c".to_string(), documented.clone())]));
        let faulted = expected.of(&corpus, Mark::Record(2), false);
        assert_eq!(faulted.trees.len(), 2);
        assert_eq!(faulted.transformed.keys().collect::<Vec<_>>(), ["a", "c"], "the before tree holds the bytes");
        assert!(expected.of(&corpus, Mark::Record(2), true).transformed.is_empty(), "the acknowledged record holds none of them");
    }

    /// The faulted command writes the record after the last corpus record, whatever variants the
    /// history recorded on a side branch.
    ///
    /// Revert: count a variant as its record → a history ending in variant 2 faults record 3, past
    /// the one the corpus line reached.
    #[test]
    fn the_faulted_record_follows_the_corpus_line_not_a_variant() {
        assert_eq!(next_record(&[]), 0);
        assert_eq!(next_record(&[Mark::Record(0), Mark::Variant(0), Mark::Record(1)]), 2);
        assert_eq!(next_record(&[Mark::Record(0), Mark::Variant(2)]), 1);
    }

    /// The tree before the faulted command is the last record's, even when the history ends in a
    /// variant on a side pallet (as `unvouched` does) and switched back.
    ///
    /// Revert: take the last mark → variant 0's tree, so a faulted command that serves the work
    /// tree as it was grades silent.
    #[test]
    fn the_tree_before_the_fault_is_the_last_records() {
        let corpus = crate::corpus::generate(1, crate::corpus::Shape::SMALL);
        let record = corpus::expected(&corpus.records[0], &corpus.modes);
        assert_ne!(record, corpus::expected(&crate::corpus::variant(&corpus.records[0]), &corpus.modes), "the variant differs");
        assert_eq!(before_of(&[Mark::Record(0), Mark::Variant(0)], &corpus), record);
        assert_eq!(before_of(&[Mark::Record(0), Mark::Variant(0), Mark::Record(1)], &corpus), corpus::expected(&corpus.records[1], &corpus.modes));
        assert!(before_of(&[], &corpus).is_empty());
    }

    /// A remedy that uncovers another refusal gets the next one run too, each remedy once, and the
    /// rounds stop as soon as the history reads or nothing new is named.
    ///
    /// Revert: one round → the shift the second refusal names never runs and the history never
    /// reads; drop the filter on remedies already run → the lock removal runs every round.
    #[test]
    fn a_remedy_that_uncovers_another_gets_it_run() {
        let lock = Remedy::Remove { path: "/s/work/.forklift/lock".into() };
        let shift = Remedy::Command { arguments: vec!["shift".into(), "main".into()] };
        // The state: what the refusals so far name, and every remedy run, in order.
        let mut state = (vec![lock.clone()], Vec::<Remedy>::new());
        let named = |(named, _): &mut (Vec<Remedy>, Vec<Remedy>)| named.clone();
        let run = |(named, ran): &mut (Vec<Remedy>, Vec<Remedy>), fresh: &[Remedy]| -> Result<bool, ()> {
            ran.extend(fresh.iter().cloned());
            // The lock gone, the retry finds the bay behind; once shifted, the history reads.
            if fresh.contains(&lock) {
                named.push(shift.clone());
            }
            Ok(ran.contains(&shift))
        };
        assert_eq!(follow_remedies(&mut state, named, run), Ok(true));
        assert_eq!(state.1, vec![lock.clone(), shift.clone()]);

        let mut stuck = (vec![lock.clone()], Vec::<Remedy>::new());
        let never = |(_, ran): &mut (Vec<Remedy>, Vec<Remedy>), fresh: &[Remedy]| -> Result<bool, ()> {
            ran.extend(fresh.iter().cloned());
            Ok(false)
        };
        assert_eq!(follow_remedies(&mut stuck, named, never), Ok(false));
        assert_eq!(stuck.1, vec![lock], "a remedy that did not help is not run again");
    }


    fn answer(exit: Option<i32>, message: &str, next_step: Option<&str>) -> Outcome {
        let failure = (exit != Some(0) && exit.is_some()).then(|| tool::Failure {
            code: "error".into(), message: message.into(), next_step: next_step.map(str::to_string),
        });
        Outcome { arguments: vec![], exit, stdout: vec![], stderr: vec![], failure }
    }

    /// A served record counts as acknowledged only after an answer that asks nothing further: success, or
    /// a refusal naming no remedy. A kill is no answer, and a refusal naming a remedy sends the user
    /// through it first.
    ///
    /// Revert: count a kill as an answer → a command killed after its record was durable heals
    /// with no retry; count any refusal → a stale lock's refusal heals before its remedy runs;
    /// drop the failure-document or unextractable conditions → a crash or an unreadable remedy
    /// settles the record; settle on the first attempt → the faulted command's refusal of a write
    /// it committed grades as acknowledged.
    #[test]
    fn only_an_answer_asking_nothing_further_settles_a_served_record() {
        assert!(answered(Some(&answer(Some(0), "", None))));
        assert!(answered(Some(&answer(Some(1), "Nothing to stack: the inventory matches the head of pallet \"main\".", None))));
        assert!(!answered(Some(&answer(None, "", None))), "a kill is no answer");
        assert!(!answered(Some(&answer(Some(26), "This bay is behind pallet \"main\".", Some("forklift shift main")))));
        assert!(!answered(Some(&answer(Some(6), "If that process is no longer running, remove \"/s/work/.forklift/lock\" and try again.", None))));
        assert!(!answered(None));
        let mut crashed = answer(Some(101), "", None);
        crashed.failure = None;
        assert!(!answered(Some(&crashed)), "a crash with no failure document is no answer");
        assert!(!answered(Some(&answer(Some(1), "Stuck.", Some("forklift load -- <path>")))), "an unreadable remedy is no answer");
        let nothing = answer(Some(1), "Nothing to stack.", None);
        assert!(settles(true, Some(&nothing)));
        assert!(!settles(false, Some(&nothing)), "the faulted command's own answer is no retry");
    }

    /// An answer settles the faulted record only by claiming it stands, a success or a stack
    /// refused as nothing to stack, which then must hold its own tree; or, as any other refusal,
    /// only once the record already holds it. A record still at the tree before it reads as
    /// silent when judged as acknowledged, which a refusal for good must not make it.
    ///
    /// Revert: settle on any answer → a damaged file refused on every retry, the record never
    /// stacked, grades silent; settle on a refusal only by the readback → a stack told there is
    /// nothing to stack, its record missing, is not held to it; drop the stack condition from
    /// already-stacked → a load told nothing to stack claims a record.
    #[test]
    fn a_refusal_for_good_settles_no_record_it_never_made() {
        let missing = served(&[(0, Verdict::Silent)]);
        let standing = served(&[(0, Verdict::Expected)]);
        let mut refused = answer(Some(1), "Error while reading configuration file \"warehouse.toml\": stream did not contain valid UTF-8", None);
        refused.arguments = vec!["--json".into(), "stack".into(), "-m".into(), "r".into()];
        assert!(answered(Some(&refused)), "a refusal naming no remedy is an answer");
        assert!(!settled_by(Some(&refused), &missing, Mark::Record(0)));
        assert!(settled_by(Some(&refused), &standing, Mark::Record(0)));
        let mut nothing = answer(Some(1), "Nothing to stack: the inventory matches the head of pallet \"main\".", None);
        nothing.arguments = refused.arguments.clone();
        assert!(settled_by(Some(&nothing), &missing, Mark::Record(0)), "told nothing to stack, the record is held to standing");
        assert!(settled_by(Some(&answer(Some(0), "", None)), &missing, Mark::Record(0)));
        nothing.arguments = vec!["--json".into(), "load".into(), ".".into()];
        assert!(!settled_by(Some(&nothing), &missing, Mark::Record(0)));
        assert!(!settled_by(None, &missing, Mark::Record(0)));
    }

    /// A record is acknowledged by its own lines, before any the scenario runs after them; read
    /// back through the host, by every line through the last of the last push; and a host-side
    /// readback of a command that pushes nothing has no acknowledging line.
    ///
    /// Revert: count every line → the store compaction after the record acknowledges it (3, not
    /// 2); count the record's own lines through the host too → the lift is not owed (2, not 3);
    /// stop at the first push → the second is not owed (3, not 5); default a host-side command
    /// with no push to its record's lines → it is graded, not refused.
    #[test]
    fn a_record_is_acknowledged_by_its_own_lines_or_through_its_last_push() {
        assert_eq!(acknowledging(Tool::Forklift, &[Op::Pack], false), Ok(2));
        assert_eq!(acknowledging(Tool::Forklift, &[Op::Push], false), Ok(2));
        assert_eq!(acknowledging(Tool::Forklift, &[], false), Ok(2));
        assert_eq!(acknowledging(Tool::Forklift, &[Op::Push], true), Ok(3));
        assert_eq!(acknowledging(Tool::Forklift, &[Op::Push, Op::Pack], true), Ok(3));
        assert_eq!(acknowledging(Tool::Forklift, &[Op::Push, Op::Pack, Op::Push, Op::Pack], true), Ok(5));
        assert!(acknowledging(Tool::Forklift, &[], true).unwrap_err().contains("pushes nothing"));
        assert!(acknowledging(Tool::Forklift, &[Op::Pack], true).is_err());
    }

    /// A first attempt acknowledges its record once the acknowledging lines landed, and completes
    /// once every line did; a line that did not land ends what the attempt established, a kill as
    /// much as a refusal.
    ///
    /// Revert: key completed on acknowledged → a run killed in its last line completes; key
    /// acknowledged on every line → a run that stacked and was refused its compaction owes nothing;
    /// count every success rather than the leading ones → a line after a refused one lands.
    #[test]
    fn an_attempt_acknowledges_through_its_own_lines_and_completes_through_every_line() {
        let (ok, killed, refused) = (answer(Some(0), "", None), answer(None, "", None), answer(Some(1), "no", None));
        let made = |acknowledged, completed| Made { acknowledged, completed };
        assert_eq!(Made::of(&[ok.clone(), ok.clone()], 2, 3), made(true, false));
        assert_eq!(Made::of(&[ok.clone(), ok.clone(), refused.clone()], 2, 3), made(true, false));
        assert_eq!(Made::of(&[ok.clone(), ok.clone(), killed.clone()], 2, 3), made(true, false));
        assert_eq!(Made::of(&[ok.clone(), ok.clone(), ok.clone()], 2, 3), made(true, true));
        assert_eq!(Made::of(&[ok.clone(), killed], 2, 3), made(false, false));
        assert_eq!(Made::of(&[refused, ok.clone(), ok.clone()], 2, 3), made(false, false));
        assert_eq!(Made::of(&[ok.clone(), ok], 2, 2), made(true, true));
    }

    /// A retry adds what it landed, every line before where the next resumes, and never takes back
    /// what an earlier attempt established.
    ///
    /// Revert: take a stopped retry as acknowledging nothing → a retry refused its compaction does
    /// not owe the record it stacked; assign instead of adding → a later refusal unmakes it.
    #[test]
    fn a_retry_adds_what_it_landed_and_unmakes_nothing() {
        let at = |at| Resume { at, may_have_stacked: false };
        let retried = |mut made: Made, landed, next| { made.retried(landed, next, 2, 3); made };
        let made = |acknowledged, completed| Made { acknowledged, completed };
        assert_eq!(retried(Made::default(), false, at(2)), made(true, false));
        assert_eq!(retried(Made::default(), false, at(1)), made(false, false));
        assert_eq!(retried(Made::default(), true, Resume::START), made(true, true));
        assert_eq!(retried(made(true, false), false, at(0)), made(true, false));
        assert_eq!(retried(made(true, true), false, at(1)), made(true, true));
    }

    /// A settle that finds the record served after an answer asking nothing further owes it, and
    /// completes the command only when no line follows the acknowledging ones; the rest then run
    /// once, after the line whose answer settled it, unless the attempt landed the record itself.
    ///
    /// Revert: complete on promotion → a compaction refused for good grades healed though it never
    /// ran; run the rest whenever the record is newly owed → a retry that stacked and was refused
    /// its compaction runs it twice; resume the rest at the acknowledging lines → read back
    /// through the host, the lines between an answered push and the last are skipped; resume it at
    /// the answered line → that line is re-run and refused again; resume it inside the record's
    /// own lines → the stack is re-run; skip any answered line → read back through the host, a
    /// compaction refused for good before the last push grades healed; vouch for a push only,
    /// not the record's own lines → the stack is re-run.
    #[test]
    fn a_settled_record_is_owed_and_the_lines_after_it_still_run() {
        let made = |acknowledged, completed| Made { acknowledged, completed };
        let promoted = |promoted: Made| { let mut made = Made::default(); made.promote(promoted); made };
        assert_eq!(promoted(Made::through(2, 2, 3)), made(true, false));
        assert_eq!(promoted(Made::through(2, 2, 2)), made(true, true));
        let at = |at| Resume { at, may_have_stacked: false };
        assert_eq!(own_lines(Tool::Forklift), Ok(2));
        // An operation a subject cannot run refuses to be counted, rather than counting as none.
        assert_eq!(vouched(Tool::Git, &[Op::Pack, Op::Repair]), Err("git has no binding for Repair".to_string()));
        assert!(acknowledging(Tool::Git, &[Op::Repair, Op::Push], true).is_err());
        let local = vouched(Tool::Forklift, &[Op::Pack]).unwrap();
        assert_eq!(local, [true, true, false]);
        assert_eq!(made(true, false).rest(Made::default(), at(1), 2, &local, 2), Some(at(2)));
        assert_eq!(made(true, false).rest(Made::default(), at(0), 2, &local, 2), Some(at(2)));
        assert_eq!(made(true, false).rest(Made::default(), at(2), 2, &local, 2), None, "acknowledged by landing: the compaction was refused for good");
        // Read back through the host, then [push, pack, push]: the acknowledging lines run through 5.
        let hosted = vouched(Tool::Forklift, &[Op::Push, Op::Pack, Op::Push]).unwrap();
        assert_eq!(hosted, [true, true, true, false, true]);
        assert_eq!(made(true, false).rest(Made::default(), at(2), 5, &hosted, 2), Some(at(3)), "the answered push is done, the pack and last push run");
        assert_eq!(made(true, false).rest(Made::default(), at(1), 5, &hosted, 2), Some(at(2)));
        assert_eq!(made(true, false).rest(Made::default(), at(3), 5, &hosted, 2), Some(at(3)), "a compaction refused for good runs again");
        assert_eq!(made(true, false).rest(made(true, false), at(1), 2, &local, 2), None, "owed before this settle");
        assert_eq!(made(true, true).rest(Made::default(), at(1), 2, &local, 2), None);
    }

}
