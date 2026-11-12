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
struct Context<'a> {
    corpus: &'a Corpus,
    programs: &'a Programs,
    scratch: &'a Scratch,
    environment: &'a Environment,
    limit: Duration,
    /// What the faulted command runs after its record.
    then: &'a [Op],
    /// The host's URL, when the scenario serves one.
    url: Option<&'a str>,
    /// The folder the work tree is scoped to, when a sparse clone left it scoped.
    scope: Option<&'a str>,
    /// Read the history back through a fresh clone of the host rather than from the work tree:
    /// the damage is the host's.
    via_host: bool,
}

impl Context<'_> {
    fn run(&self, program: &Path, arguments: Vec<String>, directory: PathBuf) -> Result<Outcome, RunError> {
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
    fn record(&self, index: usize, steps: &mut Vec<Outcome>) -> Result<bool, RunError> {
        let snapshot = &self.corpus.records[index];
        let snapshot = match self.scope { Some(scope) => corpus::within(snapshot, scope), None => snapshot.clone() };
        corpus::write(&snapshot, &self.scratch.work()).map_err(|error| RunError::Io(error.to_string()))?;
        let mut lines = Vec::new();
        for op in std::iter::once(&Op::Record { index }).chain(self.then) {
            for arguments in bind(Tool::Forklift, op).expect("Tool::Forklift binds every operation") {
                lines.push(runner::fill(arguments, self.url, &self.scratch.work())
                    .map_err(|placeholder| RunError::Io(format!("{:?} needs {} and the scenario hosts nothing", op, placeholder)))?);
            }
        }
        for arguments in lines {
            let outcome = self.run(&self.programs.forklift, arguments, self.scratch.work())?;
            let succeeded = outcome.succeeded();
            steps.push(outcome);
            if !succeeded {
                return Ok(false);
            }
        }
        Ok(true)
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
        let listing = self.run(&self.programs.forklift, vec!["--json".into(), "history".into()], bay)?;
        let left_out = left_out(&runner::document(&export.stdout), &runner::described_parcels(&listing.stdout));

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
                    let tree = oracle::read_tree(&laid_out).map_err(|error| RunError::Io(error.to_string()))?;
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
                    Readback::Served { tree: oracle::read_tree(&side).map_err(|error| RunError::Io(error.to_string()))?, documented: Default::default() }
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

/// The expected trees of one faulted run.
struct Expected {
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

    /// What `mark` may hold, once the faulted command has (`made`) or has not been acknowledged.
    fn of(&self, corpus: &Corpus, mark: Mark, made: bool) -> Expectation {
        let mut expectation = self.untransformed(corpus, mark, made);
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

    fn untransformed(&self, corpus: &Corpus, mark: Mark, made: bool) -> Expectation {
        match mark {
            mark if mark != self.faulted => Expectation::acknowledged(mark.snapshot(corpus).map(|snapshot| corpus::expected(&snapshot)).unwrap_or_default()),
            _ if made => Expectation::acknowledged(self.after.clone()),
            _ => Expectation::faulted(self.before.clone(), self.after.clone()),
        }
    }

    /// The records that must be served: every acknowledged one, and the faulted one once made.
    fn required(&self, made: bool) -> Vec<Mark> {
        if made { self.marks() } else { self.acknowledged.clone() }
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
    /// Reads the history back and records what it says; true when the faulted record was made and
    /// every record reads as expected.
    fn settle(&mut self, context: &Context, expected: &Expected, made: bool) -> Result<bool, RunError> {
        let required = expected.required(made);
        let readback = context.read_back(&expected.marks(), &required, |mark| expected.of(context.corpus, mark, made))?;
        self.evidence.any_silent |= readback.silent();
        let reads = made && readback.reads(&required);
        self.evidence.acknowledged_readable = readback.reads(&expected.acknowledged);
        self.readbacks.push(readback);
        if reads && self.recoverability.commands_to_expected.is_none() {
            self.recoverability.commands_to_expected = Some(self.steps.len() as u32);
        }
        Ok(reads)
    }
}

/// Runs one faulted run of `scenario`: damage `kind` to the path `name` resolves to after the
/// build, then the faulted command, readback, a plain retry, the named remedies, and heal.
#[allow(clippy::too_many_arguments)]
pub fn run(scenario: &Scenario, name: &str, warehouse: Warehouse, kind: Kind, corpus: &Corpus, programs: &Programs, scratch: &Scratch, environment: &Environment, limit: Duration, seed: u64) -> Result<Faulted, RunError> {
    let built = runner::build(scenario, corpus, Tool::Forklift, &programs.forklift, programs.server.as_deref(), scratch, environment, limit)?;
    // What the build recorded: the corpus rooted under the scenario's scope, when it has one.
    let corpus = &scenario.corpus(corpus);
    let (walked, state) = match warehouse {
        Warehouse::Client => (&built.walked, scratch.work().join(".forklift")),
        Warehouse::Host if built.host.is_some() => (&built.host_walked, runner::host_root(scratch).join(".forklift")),
        Warehouse::Host => return Err(RunError::Io(format!("{} damages a host, and {} hosts none", name, scenario.id))),
    };
    let target = resolve(name, walked, seed)
        .ok_or_else(|| RunError::Io(format!("{} resolves to nothing {} built", name, scenario.id)))?;
    damage::apply(&state.join(&target), kind, seed).map_err(|error| RunError::Io(format!("{:?}", error)))?;

    let faulted = next_record(&built.acknowledged);
    let snapshot = corpus.records.get(faulted)
        .ok_or_else(|| RunError::Io(format!("the faulted record {} is past the corpus's {} records", faulted, corpus.records.len())))?;
    // Before the faulted command, the work tree holds what the history recorded last.
    let before = built.acknowledged.last().and_then(|last| last.snapshot(corpus)).map(|snapshot| corpus::expected(&snapshot)).unwrap_or_default();
    let taken = built.taken.iter().map(|taken| (taken.digest.clone(), admissible(taken, scenario.withhold_reader(warehouse)))).collect();
    let expected = Expected { before, after: corpus::expected(snapshot), acknowledged: built.acknowledged, faulted: Mark::Record(faulted), taken };
    let context = Context {
        corpus, programs, scratch, environment, limit, then: &scenario.then, url: built.host.as_ref().map(|host| host.url.as_str()),
        scope: scenario.scope(), via_host: warehouse == Warehouse::Host,
    };
    let mut trail = Trail::default();
    let mut unextractable = Vec::new();

    // The faulted command, then one plain retry of it.
    let mut made = context.record(faulted, &mut trail.steps)?;
    let mut reads = trail.settle(&context, &expected, made)?;
    if !reads {
        made = context.record(faulted, &mut trail.steps)?;
        reads = trail.settle(&context, &expected, made)?;
    }
    trail.evidence.healed_by_retry = reads;

    // The remedies every refusal so far named, run in order, then the faulted command again.
    if !reads {
        let texts: Vec<String> = trail.steps.iter().chain(trail.readbacks.iter().map(|readback| &readback.export))
            .flat_map(|outcome| outcome.remedy_texts().into_iter().map(str::to_string)).collect();
        let extracted = remedy::extract(&texts.iter().map(String::as_str).collect::<Vec<_>>());
        unextractable = extracted.unextractable;
        trail.evidence.remedy_named = !extracted.remedies.is_empty();
        for named in &extracted.remedies {
            match named {
                Remedy::Command { arguments } => {
                    let mut arguments = arguments.clone();
                    if arguments.first().map(String::as_str) != Some("--json") {
                        arguments.insert(0, "--json".into());
                    }
                    trail.steps.push(context.run(&programs.forklift, arguments, scratch.work())?);
                }
                Remedy::Remove { path } if remedy::removable(path, scratch.root()) => {
                    trail.recoverability.outside_knowledge.push(format!("remove {}", path.display()));
                    let _ = damage::apply(path, Kind::Delete, 0);
                }
                Remedy::Remove { path } => {
                    trail.recoverability.outside_knowledge.push(format!("remove {} (outside the run root; not run)", path.display()));
                }
            }
        }
        if trail.evidence.remedy_named {
            made = context.record(faulted, &mut trail.steps)?;
            reads = trail.settle(&context, &expected, made)?;
            trail.evidence.remedy_worked = reads;
            trail.recoverability.named_right_remedy = reads;
        }
    }

    // Heal last, when an acknowledged record still does not read back.
    if !trail.evidence.acknowledged_readable {
        trail.steps.push(context.run(&programs.forklift, vec!["--json".into(), "heal".into()], scratch.work())?);
        trail.settle(&context, &expected, made)?;
    }

    Ok(Faulted {
        scenario: scenario.id.clone(), name: name.to_string(), warehouse, kind, target,
        grade: oracle::grade(&trail.evidence), evidence: trail.evidence, recoverability: trail.recoverability,
        steps: trail.steps, readbacks: trail.readbacks, unextractable,
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
    /// the export names.
    ///
    /// Revert: key every entry to every record → a record that never held the file expects a
    /// documented code for it, and an export serving it correctly reads silent.
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
        ] };
        let documented = vec![Admissible::Documented { code: "content_withheld".into() }];
        let expected = Expected {
            acknowledged: vec![Mark::Record(0), Mark::Record(1)], faulted: Mark::Record(2),
            before: corpus::expected(&corpus.records[1]), after: corpus::expected(&corpus.records[2]),
            taken: vec![(oracle::digest(b"x"), documented.clone())],
        };
        let first = expected.of(&corpus, Mark::Record(0), false);
        assert_eq!(first.transformed, BTreeMap::from([("a".to_string(), documented.clone())]));
        let second = expected.of(&corpus, Mark::Record(1), false);
        assert_eq!(second.transformed, BTreeMap::from([("a".to_string(), documented.clone()), ("c".to_string(), documented.clone())]));
        let faulted = expected.of(&corpus, Mark::Record(2), false);
        assert_eq!(faulted.trees.len(), 2);
        assert_eq!(faulted.transformed.keys().collect::<Vec<_>>(), ["a", "c"], "the before tree holds the bytes");
        assert!(expected.of(&corpus, Mark::Record(2), true).transformed.is_empty(), "the made record holds none of them");
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
}
