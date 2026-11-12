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
use crate::damage::{self, Kind};
use crate::oracle::{self, Evidence, Expectation, Grade, Readback, Recoverability, Verdict};
use crate::remedy::{self, Remedy};
use crate::runner::{self, RunError};
use crate::scenario::{bind, record_message, Op, Scenario, Tool};
use crate::scrub::{Environment, Scratch};
use crate::tool::{self, Invocation, Outcome};

/// The programs a faulted run drives, each by absolute path.
#[derive(Clone, Debug)]
pub struct Programs {
    pub forklift: PathBuf,
    /// The git the harness lays out exported commits with.
    pub git: PathBuf,
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
    pub index: usize,
    /// One verdict per exported commit carrying the record's message; empty when none did.
    pub verdicts: Vec<Verdict>,
}

/// One readback of the whole history.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct HistoryReadback {
    /// The export's own outcome; a refusal here refuses every record.
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
    fn reads(&self, wanted: &[usize]) -> bool {
        !self.refused() && wanted.iter().all(|index| self.records.iter().any(|record| record.index == *index
            && !record.verdicts.is_empty() && record.verdicts.iter().all(|verdict| *verdict == Verdict::Expected)))
    }
}

/// What a faulted run did and the grade it earned.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Faulted {
    pub scenario: String,
    pub name: String,
    pub kind: Kind,
    /// The damaged path, relative to the state folder.
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
    /// operation's lines run in order until one fails.
    fn record(&self, index: usize, steps: &mut Vec<Outcome>) -> Result<bool, RunError> {
        corpus::write(&self.corpus.records[index], &self.scratch.work()).map_err(|error| RunError::Io(error.to_string()))?;
        let lines = bind(Tool::Forklift, &Op::Record { index }).expect("forklift binds every operation");
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

    /// Reads every record in `indices` back through the tool. `expectation` gives what each may
    /// hold; a record in `required` that an export exiting 0 leaves out was served as missing,
    /// which is silent.
    fn read_back(&self, indices: &[usize], required: &[usize], expectation: impl Fn(usize) -> Expectation) -> Result<HistoryReadback, RunError> {
        let exported = self.fresh("readback");
        let export = self.run(&self.programs.forklift, vec!["--json".into(), "git".into(), "export".into(), exported.display().to_string()], self.scratch.work())?;
        if !export.succeeded() {
            return Ok(HistoryReadback { export, records: Vec::new() });
        }

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
        for &index in indices {
            let message = record_message(index);
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
                    Readback::Served { tree, documented: Default::default() }
                } else {
                    // The export wrote a commit git cannot lay out: what the tool served is broken.
                    Readback::Served { tree: Default::default(), documented: Default::default() }
                };
                verdicts.push(oracle::judge(&readback, &expectation(index)));
            }
            if verdicts.is_empty() && required.contains(&index) {
                verdicts.push(Verdict::Silent);
            }
            records.push(RecordReadback { index, verdicts });
        }
        Ok(HistoryReadback { export, records })
    }
}

/// The expected trees of one faulted run.
struct Expected {
    acknowledged: Vec<usize>,
    faulted: usize,
    before: oracle::Tree,
    after: oracle::Tree,
}

impl Expected {
    fn indices(&self) -> Vec<usize> {
        self.acknowledged.iter().copied().chain([self.faulted]).collect()
    }

    /// What record `index` may hold, once the faulted command has (`made`) or has not been
    /// acknowledged.
    fn of(&self, corpus: &Corpus, index: usize, made: bool) -> Expectation {
        match index {
            index if index != self.faulted => Expectation::acknowledged(corpus::expected(&corpus.records[index])),
            _ if made => Expectation::acknowledged(self.after.clone()),
            _ => Expectation::faulted(self.before.clone(), self.after.clone()),
        }
    }

    /// The records that must be served: every acknowledged one, and the faulted one once made.
    fn required(&self, made: bool) -> Vec<usize> {
        if made { self.indices() } else { self.acknowledged.clone() }
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
        let readback = context.read_back(&expected.indices(), &required, |index| expected.of(context.corpus, index, made))?;
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
pub fn run(scenario: &Scenario, name: &str, kind: Kind, corpus: &Corpus, programs: &Programs, scratch: &Scratch, environment: &Environment, limit: Duration, seed: u64) -> Result<Faulted, RunError> {
    let built = runner::build(scenario, corpus, Tool::Forklift, &programs.forklift, scratch, environment, limit)?;
    let target = resolve(name, &built.walked, seed)
        .ok_or_else(|| RunError::Io(format!("{} resolves to nothing {} built", name, scenario.id)))?;
    let state = scratch.work().join(".forklift");
    damage::apply(&state.join(&target), kind, seed).map_err(|error| RunError::Io(format!("{:?}", error)))?;

    let faulted = built.acknowledged.iter().max().map_or(0, |last| last + 1);
    let snapshot = corpus.records.get(faulted)
        .ok_or_else(|| RunError::Io(format!("the faulted record {} is past the corpus's {} records", faulted, corpus.records.len())))?;
    let expected = Expected {
        before: built.acknowledged.last().map(|last| corpus::expected(&corpus.records[*last])).unwrap_or_default(),
        after: corpus::expected(snapshot),
        acknowledged: built.acknowledged,
        faulted,
    };
    let context = Context { corpus, programs, scratch, environment, limit };
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
        scenario: scenario.id.clone(), name: name.to_string(), kind, target,
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
        let mut records: BTreeMap<usize, Vec<Verdict>> = BTreeMap::new();
        for (index, verdict) in subjects {
            records.entry(*index).or_default().push(*verdict);
        }
        HistoryReadback { export, records: records.into_iter().map(|(index, verdicts)| RecordReadback { index, verdicts }).collect() }
    }

    /// A history reads only when every wanted record was served and every copy of it expected: a
    /// record the export left out does not read, and neither does one with a silent copy.
    ///
    /// Revert: count a record with no exported commit as read → a deleted pallet's history passes.
    #[test]
    fn a_record_left_out_of_the_export_does_not_read() {
        let full = served(&[(0, Verdict::Expected), (1, Verdict::Expected)]);
        assert!(full.reads(&[0, 1]));
        let missing = HistoryReadback { records: vec![RecordReadback { index: 0, verdicts: vec![Verdict::Expected] }, RecordReadback { index: 1, verdicts: vec![] }], ..full.clone() };
        assert!(!missing.reads(&[0, 1]));
        assert!(missing.reads(&[0]));
        let doubled = served(&[(0, Verdict::Expected), (0, Verdict::Silent)]);
        assert!(!doubled.reads(&[0]));
        assert!(doubled.silent());
        let refused = HistoryReadback { export: Outcome { exit: Some(1), ..full.export.clone() }, ..full };
        assert!(!refused.reads(&[0, 1]));
    }
}
