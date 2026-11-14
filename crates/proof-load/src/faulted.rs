//! The faulted run for damaged files: build a scenario's history, damage one path a binding's
//! census name resolves to, make the next record as the faulted command, then read every record
//! back through the tool into fresh folders and grade what came back.
//!
//! The readback is `forklift git export` into a fresh folder, then a git checkout of each commit
//! whose subject is a record's message into a folder of its own. Export is the tool reading its
//! own history whole; git only lays out the bytes forklift handed it, so a wrong byte in the
//! readback is one the tool served.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::corpus::{self, Corpus};
use crate::damage::{self, Kind, Warehouse};
use crate::oracle::{self, Admissible, Evidence, Expectation, Grade, Readback, Recoverability, Verdict};
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
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct HistoryReadback {
    /// The readback's own command: the export, or in a scoped bay the opening of the side bay it
    /// reads through. A refusal here refuses every record.
    pub export: Outcome,
    pub records: Vec<RecordReadback>,
}

impl HistoryReadback {
    fn refused(&self) -> bool {
        !self.export.succeeded()
    }

    /// Any record served wrong with exit 0.
    fn silent(&self) -> bool {
        self.records.iter().any(|record| record.verdicts.contains(&Verdict::Silent))
    }

    /// Every record in `wanted` served, and served as expected.
    fn reads(&self, wanted: &[Mark]) -> bool {
        !self.refused() && wanted.iter().all(|mark| self.records.iter().any(|record| record.mark == *mark
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
    /// Every command after the fault, in order: the faulted command, retries, remedies, heal.
    pub steps: Vec<Outcome>,
    pub readbacks: Vec<HistoryReadback>,
    /// Remedies named in a form the harness does not run.
    pub unextractable: Vec<String>,
}

/// The parts of a faulted run that stay fixed across its steps.
pub(crate) struct Context<'a> {
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
}

impl Context<'_> {
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
        resume_lines(lines, from, steps, |arguments| self.run(&self.programs.forklift, arguments, self.scratch.work()))
    }

    /// The faulted command's lines for corpus record `index`, run in order until one fails, its
    /// snapshot already written: an environment fault is set between the two.
    pub(crate) fn run_lines(&self, index: usize, steps: &mut Vec<Outcome>) -> Result<bool, RunError> {
        for arguments in self.lines(index)? {
            let outcome = self.run(&self.programs.forklift, arguments, self.scratch.work())?;
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
        let snapshot = &self.corpus.records[index];
        let snapshot = match self.scope { Some(scope) => corpus::within(snapshot, scope), None => snapshot.clone() };
        corpus::write(&snapshot, &self.corpus.modes, &self.scratch.work(), self.scratch.work_moved()).map_err(|error| RunError::Io(error.to_string()))
    }

    /// The command lines of the faulted command for corpus record `index`: the record operation's,
    /// then those of the scenario's `then`, in the order they run.
    pub(crate) fn lines(&self, index: usize) -> Result<Vec<Vec<String>>, RunError> {
        let mut lines = Vec::new();
        for op in std::iter::once(&Op::Record { index }).chain(self.then) {
            for arguments in bind(Tool::Forklift, op).expect("Tool::Forklift binds every operation") {
                lines.push(runner::fill(arguments, self.url, &self.scratch.work())
                    .map_err(|placeholder| RunError::Io(format!("{:?} needs {} and the scenario hosts nothing", op, placeholder)))?);
            }
        }
        Ok(lines)
    }

    /// How many of the faulted command's leading lines acknowledge its record, as
    /// [`acknowledging`] counts them for this run's `then` and readback.
    pub(crate) fn acknowledging(&self) -> Result<usize, RunError> {
        acknowledging(self.then, self.via_host).map_err(RunError::Io)
    }

    /// Reads every record in `marks` back through the tool. `expectation` gives what each may
    /// hold; a record in `required` that an export exiting 0 leaves out was served as missing,
    /// which is silent.
    fn read_back(&self, marks: &[Mark], required: &[Mark], expectation: impl Fn(Mark) -> Expectation) -> Result<HistoryReadback, RunError> {
        if let Some(scope) = self.scope {
            return self.read_back_scoped(scope, marks, required, |mark| within(expectation(mark), scope));
        }
        // The warehouse read: the work tree's, or a fresh clone of the host. A clone that fails
        // refuses every record, as a failed export does.
        let bay = if self.via_host {
            let url = self.url.ok_or_else(|| RunError::Io("a host-side readback needs a host".to_string()))?;
            let clone = self.fresh("clone");
            let cloned = self.run(&self.programs.forklift, vec!["--json".into(), "clone".into(), url.to_string(), clone.display().to_string()], self.scratch.root().to_path_buf())?;
            if !cloned.succeeded() {
                return Ok(HistoryReadback { export: cloned, records: Vec::new() });
            }
            clone
        } else {
            self.scratch.work()
        };
        let exported = self.fresh("readback");
        let export = self.run(&self.programs.forklift, vec!["--json".into(), "git".into(), "export".into(), exported.display().to_string()], bay.clone())?;
        if !export.succeeded() {
            return Ok(HistoryReadback { export, records: Vec::new() });
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
        Ok(HistoryReadback { export, records })
    }

    /// The readback of a scoped bay, where `git export` refuses and `show` serves no binary file's
    /// bytes: a side bay with the same scope, opened fresh, and in it a new pallet at each parcel
    /// whose description is a record's message, which materializes that record's scope there. The
    /// work tree under test, dirty or not, is left as it is. The parcels come from the history
    /// listing; a listing that cannot be read served nothing.
    fn read_back_scoped(&self, scope: &str, marks: &[Mark], required: &[Mark], expectation: impl Fn(Mark) -> Expectation) -> Result<HistoryReadback, RunError> {
        let work = self.scratch.work();
        let listing = self.run(&self.programs.forklift, vec!["--json".into(), "history".into()], work.clone())?;
        if !listing.succeeded() {
            return Ok(HistoryReadback { export: listing, records: Vec::new() });
        }
        let parcels = runner::described_parcels(&listing.stdout);

        let side = self.fresh("readback");
        let name = side.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        let opened = self.run(&self.programs.forklift, vec![
            "--json".into(), "bay".into(), "add".into(), name.clone(), side.display().to_string(), "--scope".into(), scope.to_string(),
        ], work)?;
        if !opened.succeeded() {
            return Ok(HistoryReadback { export: opened, records: Vec::new() });
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
        Ok(HistoryReadback { export: opened, records })
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
    acknowledged.iter().filter_map(|mark| match mark { Mark::Record(index) => Some(index + 1), Mark::Variant(_) => None }).max().unwrap_or(0)
}

/// How many leading lines of a faulted command that runs its record and then `then` acknowledge
/// the record: the record's own lines, once they exit the tool has recorded it; read back through
/// the host (`via_host`), every line through the last of the last push, once it exits the host
/// holds it. A host-side readback of a command that pushes nothing has no line that acknowledges
/// the record, and is refused rather than graded against one.
pub(crate) fn acknowledging(then: &[Op], via_host: bool) -> Result<usize, String> {
    let lines = |op: &Op| bind(Tool::Forklift, op).expect("Tool::Forklift binds every operation").len();
    let record = own_lines();
    if !via_host {
        return Ok(record);
    }
    let mut through = None;
    let mut ran = record;
    for op in then {
        ran += lines(op);
        if matches!(op, Op::Push) {
            through = Some(ran);
        }
    }
    through.ok_or_else(|| "the history is read back through the host, and the faulted command pushes nothing to it".to_string())
}

/// How many lines the faulted command's record itself runs, before any of the scenario's after it.
fn own_lines() -> usize {
    bind(Tool::Forklift, &Op::Record { index: 0 }).expect("Tool::Forklift binds every operation").len()
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

    /// Settled as acknowledged by an answer asking nothing further, as `promoted`, the record's
    /// own lines landing, establishes: completed too only when no line follows them.
    fn promote(&mut self, promoted: Made) {
        self.acknowledged = true;
        self.completed |= promoted.completed;
    }

    /// Where the lines after the record's own resume, when the attempt that stopped at `stopped`
    /// left it, from `before`, acknowledged only by a settle ([`Made::promote`]): it stopped in the
    /// record's own lines, so none after them ran, and they resume at the first. Keyed on the
    /// record's own lines, not the acknowledging ones: read back through the host, the lines up to
    /// the last push are after the record's own too, and an attempt that stopped short of them ran
    /// none of them either. An attempt that stopped at one of them already ran it, and resumes
    /// there on the next retry as any does.
    fn rest(&self, before: Made, stopped: Resume) -> Option<Resume> {
        let own = own_lines();
        (!before.acknowledged && self.acknowledged && !self.completed && stopped.at < own)
            .then_some(Resume { at: own, may_have_stacked: false })
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
}

impl Trail {
    /// Reads the history back and records what it says; true when the faulted command completed
    /// and every record reads as expected, the faulted one held to its own tree once acknowledged.
    /// A served record counts as acknowledged from here on once the tool's latest answer asks
    /// nothing further (see [`answered`]): a command killed after its record was durable, retried,
    /// is told there is nothing left to stack. That is `promoted`, what the record's lines landing
    /// establishes; it says nothing of any line after them, which completes only by landing.
    /// `retry` says the latest step re-ran the faulted command: only a retry's answer settles a
    /// served record, since the faulted command's own refusal of a write it committed is graded as
    /// a refusal, and heal is no re-run.
    fn settle(&mut self, context: &Context, expected: &Expected, made: &mut Made, promoted: Made, retry: bool) -> Result<bool, RunError> {
        let mut readback = self.read_back(context, expected, made.acknowledged)?;
        if !made.acknowledged && settles(retry, self.steps.last()) && readback.reads(&[expected.faulted]) {
            // Judged again as acknowledged: the record must now hold exactly its own tree.
            self.evidence.any_silent |= readback.silent();
            self.readbacks.push(readback);
            readback = self.read_back(context, expected, true)?;
            made.promote(promoted);
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
        context.read_back(&expected.marks(), &expected.required(acknowledged), |mark| expected.of(context.corpus, mark, acknowledged))
    }
}

/// Whether a served record counts as acknowledged at this settle: on a retry, after an
/// [`answered`] one. It is completed too only when no line follows the record's own
/// ([`Made::promote`]).
fn settles(retry: bool, last: Option<&Outcome>) -> bool {
    retry && answered(last)
}

/// Whether `last`, the latest command run, answered and asked nothing further: it exited (a kill
/// is no answer), and either succeeded or refused with a failure document naming no remedy and
/// nothing unreadable as one, as a re-stack of a record already made is refused with nothing left
/// to stack. A crash with no failure document is no answer.
fn answered(last: Option<&Outcome>) -> bool {
    last.is_some_and(|outcome| outcome.succeeded() || (outcome.exit.is_some() && outcome.failure.is_some() && {
        let extracted = remedy::extract(&outcome.remedy_texts());
        extracted.remedies.is_empty() && extracted.unextractable.is_empty()
    }))
}

/// What the faulted command of a built scenario must leave readable: the record it writes, and
/// what every record may then hold. Fails when the corpus has no record after the history's.
pub(crate) fn expected(scenario: &Scenario, corpus: &Corpus, built: &runner::Built, warehouse: Warehouse) -> Result<(Expected, usize), RunError> {
    let faulted = next_record(&built.acknowledged);
    let snapshot = corpus.records.get(faulted)
        .ok_or_else(|| RunError::Io(format!("the faulted record {} is past the corpus's {} records", faulted, corpus.records.len())))?;
    let before = before_of(&built.acknowledged, corpus);
    let taken = built.taken.iter().map(|taken| (taken.digest.clone(), admissible(taken, scenario.withhold_reader(warehouse)))).collect();
    Ok((Expected { before, after: corpus::expected(snapshot, &corpus.modes), acknowledged: built.acknowledged.clone(), faulted: Mark::Record(faulted), taken }, faulted))
}

/// What the steps after a fault established, and the grade they earn.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Graded {
    pub grade: Grade,
    pub evidence: Evidence,
    pub recoverability: Recoverability,
    /// Every command from the fault on, in order: the faulted command, retries, remedies, heal.
    pub steps: Vec<Outcome>,
    pub readbacks: Vec<HistoryReadback>,
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
/// heal.
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
        match made.rest(before, *resume) {
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
            let texts: Vec<String> = trail.steps.iter().chain(trail.readbacks.iter().map(|readback| &readback.export))
                .flat_map(|outcome| outcome.remedy_texts().into_iter().map(str::to_string)).collect();
            let extracted = remedy::extract(&texts.iter().map(String::as_str).collect::<Vec<_>>());
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
                        if arguments.first().map(String::as_str) != Some("--json") {
                            arguments.insert(0, "--json".into());
                        }
                        trail.steps.push(context.run(&context.programs.forklift, arguments, context.scratch.work())?);
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

    // Heal last, when an acknowledged record still does not read back.
    if !trail.evidence.acknowledged_readable {
        trail.steps.push(context.run(&context.programs.forklift, vec!["--json".into(), "heal".into()], context.scratch.work())?);
        trail.settle(context, expected, &mut made, promoted, false)?;
    }

    Ok(Graded {
        grade: oracle::grade(&trail.evidence), evidence: trail.evidence, recoverability: trail.recoverability,
        steps: trail.steps, readbacks: trail.readbacks, unextractable,
    })
}

/// Runs one faulted run of `scenario`: damage `kind` to the path `name` resolves to after the
/// build, then the faulted command, readback, a plain retry, the named remedies, and heal.
#[allow(clippy::too_many_arguments)]
pub fn run(scenario: &Scenario, name: &str, warehouse: Warehouse, kind: Kind, corpus: &Corpus, programs: &Programs, scratch: &Scratch, environment: &Environment, limit: Duration, seed: u64) -> Result<Faulted, RunError> {
    let built = runner::build(scenario, corpus, Tool::Forklift, &programs.forklift, programs.server.as_deref(), scratch, environment, limit, None)?;
    // What the build recorded: the corpus rooted under the scenario's scope, when it has one.
    let corpus = &scenario.corpus(corpus);
    let (walked, state) = match warehouse {
        Warehouse::Client => (&built.walked, scratch.work().join(".forklift")),
        Warehouse::Host if built.host.is_some() => (&built.host_walked, runner::host_root(scratch).join(".forklift")),
        Warehouse::Host => return Err(RunError::Io(format!("{} damages a host, and {} hosts none", name, scenario.id))),
    };
    // A planted path is absent by definition, so it is named, not resolved against the walk.
    let target = if kind.plants() {
        damage::planted(name)
    } else {
        resolve(name, walked, seed).ok_or_else(|| RunError::Io(format!("{} resolves to nothing {} built", name, scenario.id)))?
    };
    damage::apply(&state.join(&target), kind, seed).map_err(|error| RunError::Io(format!("{:?}", error)))?;

    let (expected, faulted) = expected(scenario, corpus, &built, warehouse)?;
    let context = Context {
        corpus, programs, scratch, environment, limit, then: &scenario.then, url: built.host.as_ref().map(|host| host.url.as_str()),
        scope: scenario.scope(), via_host: warehouse == Warehouse::Host,
    };
    let graded = grade_after(&context, &expected, faulted, |context, steps| context.record(faulted, steps).map(drop))?;
    Ok(Faulted {
        scenario: scenario.id.clone(), name: name.to_string(), warehouse, kind, target,
        grade: graded.grade, evidence: graded.evidence, recoverability: graded.recoverability,
        steps: graded.steps, readbacks: graded.readbacks, unextractable: graded.unextractable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn walked() -> Vec<String> {
        ["objects/", "objects/ab/", "objects/ab/11", "objects/cd/", "objects/cd/22", "pallets/", "pallets/main", "warehouse"]
            .iter().map(|path| path.to_string()).collect()
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
        HistoryReadback { export, records: records.into_iter().map(|(mark, verdicts)| RecordReadback { mark, verdicts }).collect() }
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
            taken: vec![(oracle::digest(b"x"), documented.clone())],
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
        assert_eq!(acknowledging(&[Op::Pack], false), Ok(2));
        assert_eq!(acknowledging(&[Op::Push], false), Ok(2));
        assert_eq!(acknowledging(&[], false), Ok(2));
        assert_eq!(acknowledging(&[Op::Push], true), Ok(3));
        assert_eq!(acknowledging(&[Op::Push, Op::Pack], true), Ok(3));
        assert_eq!(acknowledging(&[Op::Push, Op::Pack, Op::Push, Op::Pack], true), Ok(5));
        assert!(acknowledging(&[], true).unwrap_err().contains("pushes nothing"));
        assert!(acknowledging(&[Op::Pack], true).is_err());
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
    /// completes the command only when no line follows the record's own; the lines that do follow
    /// then run once from the first of them, unless the attempt already reached them.
    ///
    /// Revert: complete on promotion → a compaction refused for good grades healed though it never
    /// ran; run the rest whenever the record is newly owed → a retry that stacked and was refused
    /// its compaction runs it twice; resume the rest anywhere but its first line → it is skipped
    /// or the stack re-run; key the rest on the acknowledging lines → read back through the host,
    /// an attempt that stopped at the stack skips the lines before the last push.
    #[test]
    fn a_settled_record_is_owed_and_the_lines_after_it_still_run() {
        let made = |acknowledged, completed| Made { acknowledged, completed };
        let promoted = |promoted: Made| { let mut made = Made::default(); made.promote(promoted); made };
        assert_eq!(promoted(Made::through(2, 2, 3)), made(true, false));
        assert_eq!(promoted(Made::through(2, 2, 2)), made(true, true));
        let at = |at| Resume { at, may_have_stacked: false };
        assert_eq!(own_lines(), 2);
        assert_eq!(made(true, false).rest(Made::default(), at(1)), Some(at(2)));
        assert_eq!(made(true, false).rest(Made::default(), at(0)), Some(at(2)));
        assert_eq!(made(true, false).rest(Made::default(), at(2)), None, "the attempt ran the line after the record");
        assert_eq!(made(true, false).rest(Made::default(), at(4)), None, "the attempt ran up to the last push");
        assert_eq!(made(true, false).rest(made(true, false), at(1)), None, "owed before this settle");
        assert_eq!(made(true, true).rest(Made::default(), at(1)), None);
    }

}
