//! Scenarios and bindings.
//!
//! A scenario is tool-independent: a history to build, written in an abstract operation
//! vocabulary, and a fault. A binding maps each operation to concrete command lines for one tool.
//! Forklift binds every operation; git binds the ones it has, twice, at its defaults and tuned. An
//! operation git has no binding for makes the scenario Forklift-only, and the report shows those
//! rows in their own table instead of counting them as git losses.

use crate::damage::Kind;

/// The abstract operation vocabulary.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Op {
    /// Write corpus record `index` into the work tree and record it.
    Record { index: usize },
    /// Write the variant of corpus record `index` (`corpus::variant`) and record it: content off
    /// the corpus's own line, so two branches can change one file two ways.
    Variant { index: usize },
    /// Create a branch at the current head.
    Branch { name: String },
    /// Make `name` the current branch.
    Switch { name: String },
    /// Merge `name` into the current branch.
    Merge { name: String },
    /// Merge `name` into the current branch where the two changed one file two ways, leaving the
    /// merge stopped on its conflicts.
    Conflict { name: String },
    /// Apply the head change of `name` onto the current branch where it conflicts, leaving the
    /// pick stopped on its conflicts.
    Pick { name: String },
    /// Write the variant of corpus record `index` into the work tree and set it aside unrecorded,
    /// resetting the work tree to the head.
    Park { index: usize },
    /// Enroll the operator, so every record from here on is signed.
    Enroll,
    /// Pack the store's loose objects.
    Pack,
    Push,
    Fetch,
    /// Check the store's integrity.
    Verify,
    /// Repair the store from what it can still reach.
    Repair,
}

/// The tools a scenario can be bound to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tool {
    Forklift,
    /// Git at its defaults.
    Git,
    /// Git tuned for durability and integrity: every fsync on, fsck on transfer.
    GitTuned,
}

/// A recorded snapshot a readback grades: a corpus record, or the variant of one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(tag = "mark", content = "index", rename_all = "kebab-case")]
pub enum Mark {
    Record(usize),
    Variant(usize),
}

impl Mark {
    /// The message this mark's record carries, so a readback can find it by its subject.
    pub fn message(self) -> String {
        match self {
            Mark::Record(index) => format!("proof-load record {}", index),
            Mark::Variant(index) => format!("proof-load variant {}", index),
        }
    }

    /// The snapshot this mark records.
    pub fn snapshot(self, corpus: &crate::corpus::Corpus) -> Option<crate::corpus::Snapshot> {
        match self {
            Mark::Record(index) => corpus.records.get(index).cloned(),
            Mark::Variant(index) => corpus.records.get(index).map(crate::corpus::variant),
        }
    }
}

/// The message a recorded corpus record carries, so a readback can find each record by it.
pub fn record_message(index: usize) -> String {
    Mark::Record(index).message()
}

fn line(arguments: &[&str]) -> Vec<String> {
    arguments.iter().map(|argument| argument.to_string()).collect()
}

/// Git's options for the tuned binding, given before the subcommand.
const GIT_TUNED: &[&str] = &["-c", "core.fsync=all", "-c", "core.fsyncMethod=fsync", "-c", "transfer.fsckObjects=true", "-c", "fetch.fsckObjects=true", "-c", "receive.fsckObjects=true"];

/// The command lines (arguments after the binary) `op` runs for `tool`, in order, or None when the
/// tool has no binding for it. Forklift runs every command with `--json`, so a refusal is one
/// document the remedy extractor can read.
pub fn bind(tool: Tool, op: &Op) -> Option<Vec<Vec<String>>> {
    match tool {
        Tool::Forklift => {
            let lines = match op {
                Op::Record { index } => vec![line(&["load", "."]), vec!["stack".into(), "-m".into(), record_message(*index)]],
                Op::Variant { index } => vec![line(&["load", "."]), vec!["stack".into(), "-m".into(), Mark::Variant(*index).message()]],
                Op::Branch { name } => vec![line(&["pallet", "create", name])],
                Op::Switch { name } => vec![line(&["shift", name])],
                // A consolidation or pick that stops on its conflicts exits 0, naming them.
                Op::Merge { name } | Op::Conflict { name } => vec![line(&["consolidate", name])],
                Op::Pick { name } => vec![line(&["pick", name])],
                Op::Park { .. } => vec![line(&["load", "."]), line(&["park"])],
                Op::Enroll => vec![line(&["office", "enroll"])],
                Op::Pack => vec![line(&["store", "compact"])],
                Op::Push => vec![line(&["lift"])],
                Op::Fetch => vec![line(&["lower"])],
                Op::Verify => vec![line(&["audit", "--full"])],
                Op::Repair => vec![line(&["heal"])],
            };
            Some(lines.into_iter().map(|mut arguments| { arguments.insert(0, "--json".into()); arguments }).collect())
        }
        Tool::Git | Tool::GitTuned => {
            let lines = match op {
                Op::Record { index } => vec![line(&["add", "-A"]), vec!["commit".into(), "-q".into(), "-m".into(), record_message(*index)]],
                Op::Variant { index } => vec![line(&["add", "-A"]), vec!["commit".into(), "-q".into(), "-m".into(), Mark::Variant(*index).message()]],
                Op::Park { .. } => vec![line(&["stash", "push", "-q", "--include-untracked"])],
                Op::Pack => vec![line(&["gc", "-q"])],
                Op::Branch { name } => vec![line(&["branch", name])],
                Op::Switch { name } => vec![line(&["switch", "-q", name])],
                Op::Merge { name } => vec![line(&["merge", "-q", "--no-edit", name])],
                Op::Push => vec![line(&["push", "-q"])],
                Op::Fetch => vec![line(&["pull", "-q", "--ff-only"])],
                Op::Verify => vec![line(&["fsck", "--full", "--strict"])],
                // Git has no command that rebuilds a store from what it can still reach.
                Op::Repair => return None,
                // Git's merge and cherry-pick exit 1 on a conflict, which a build reads as a
                // failed step; and git signs nothing by enrollment.
                Op::Conflict { .. } | Op::Pick { .. } | Op::Enroll => return None,
            };
            let prefix: &[&str] = if tool == Tool::GitTuned { GIT_TUNED } else { &[] };
            Some(lines.into_iter().map(|arguments| line(prefix).into_iter().chain(arguments).collect()).collect())
        }
    }
}

/// A damaged-file fault: which census name it targets and what it does to the file.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Damage {
    pub name: String,
    pub kind: Kind,
}

/// A scenario: a history to build and, for a faulted one, the damage applied after it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Scenario {
    pub id: String,
    pub history: Vec<Op>,
    pub damage: Option<Damage>,
}

impl Scenario {
    /// The tools this scenario runs against: Forklift always; git, both ways, only when git binds
    /// every operation of the history.
    pub fn tools(&self) -> Vec<Tool> {
        let git_binds_all = self.history.iter().all(|op| bind(Tool::Git, op).is_some());
        if git_binds_all { vec![Tool::Forklift, Tool::Git, Tool::GitTuned] } else { vec![Tool::Forklift] }
    }

    /// Whether the report shows this scenario in the Forklift-only table.
    pub fn forklift_only(&self) -> bool {
        self.tools() == [Tool::Forklift]
    }

    /// The records the history acknowledges, in order.
    pub fn records(&self) -> Vec<usize> {
        self.history.iter().filter_map(|op| match op { Op::Record { index } => Some(*index), _ => None }).collect()
    }

    /// Every mark the history records, in order: corpus records and variants.
    pub fn marks(&self) -> Vec<Mark> {
        self.history.iter().filter_map(Op::mark).collect()
    }
}

impl Op {
    /// The mark this operation records, if it records one.
    pub fn mark(&self) -> Option<Mark> {
        match self {
            Op::Record { index } => Some(Mark::Record(*index)),
            Op::Variant { index } => Some(Mark::Variant(*index)),
            _ => None,
        }
    }

    /// The snapshot this operation writes into the work tree before its lines run, if any.
    pub fn writes(&self, corpus: &crate::corpus::Corpus) -> Option<Result<crate::corpus::Snapshot, String>> {
        let mark = match self {
            Op::Park { index } => Mark::Variant(*index),
            op => op.mark()?,
        };
        Some(mark.snapshot(corpus).ok_or_else(|| format!("{:?} is past the corpus's {} records", mark, corpus.records.len())))
    }
}

/// The scenarios this build of the harness runs. A damage binding names one of these as the
/// scenario that creates its path, and a run checks that it does.
pub fn catalog() -> Vec<Scenario> {
    let record = |index| Op::Record { index };
    let named = |id: &str, history: Vec<Op>| Scenario { id: id.to_string(), history, damage: None };
    vec![
        named("history", vec![record(0), record(1), record(2)]),
        named("branched", vec![
            record(0), Op::Branch { name: "side".into() }, Op::Switch { name: "side".into() }, record(1),
            Op::Switch { name: "main".into() }, Op::Merge { name: "side".into() },
        ]),
        // No Verify: `audit` refuses a warehouse with no office, and none of these enrolls one.
        named("repaired", vec![record(0), record(1), Op::Repair]),
        named("parked", vec![record(0), record(1), Op::Park { index: 1 }]),
        named("signed", vec![record(0), Op::Enroll, record(1), record(2)]),
        named("packed", vec![record(0), Op::Enroll, record(1), record(2), Op::Pack]),
        // The variant changes every file of record 0 on the side branch, and record 1 changes at
        // least one of them again on main, so the merge and the pick stop on a conflict.
        named("conflicted", vec![
            record(0), Op::Branch { name: "side".into() }, Op::Switch { name: "side".into() }, Op::Variant { index: 0 },
            Op::Switch { name: "main".into() }, record(1), Op::Conflict { name: "side".into() },
        ]),
        named("picking", vec![
            record(0), Op::Branch { name: "side".into() }, Op::Switch { name: "side".into() }, Op::Variant { index: 0 },
            Op::Switch { name: "main".into() }, record(1), Op::Pick { name: "side".into() },
        ]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_op() -> Vec<Op> {
        vec![
            Op::Record { index: 0 }, Op::Branch { name: "side".into() }, Op::Switch { name: "side".into() },
            Op::Merge { name: "main".into() }, Op::Push, Op::Fetch, Op::Verify, Op::Repair,
            Op::Variant { index: 0 }, Op::Conflict { name: "side".into() }, Op::Pick { name: "side".into() },
            Op::Park { index: 0 }, Op::Enroll, Op::Pack,
        ]
    }

    /// Forklift binds every operation, each command with `--json`; the record carries its message.
    ///
    /// Revert: drop the `--json` insertion → a refusal prints prose the extractor cannot read.
    #[test]
    fn forklift_binds_every_operation_with_json() {
        for op in every_op() {
            let lines = bind(Tool::Forklift, &op).unwrap_or_else(|| panic!("forklift binds {:?}", op));
            assert!(!lines.is_empty());
            assert!(lines.iter().all(|arguments| arguments[0] == "--json"), "{:?}", lines);
        }
        assert_eq!(bind(Tool::Forklift, &Op::Record { index: 2 }).unwrap(),
            vec![line(&["--json", "load", "."]), line(&["--json", "stack", "-m", "proof-load record 2"])]);
    }

    /// Git binds what it has, the tuned binding with its durability and fsck options before the
    /// subcommand; repair is Forklift-only, and a scenario using it runs against Forklift alone.
    ///
    /// Revert: bind git's repair to anything → the repair scenario counts git rows it cannot earn.
    #[test]
    fn git_binds_what_it_has_and_the_rest_is_forklift_only() {
        for op in every_op() {
            let forklift_only = matches!(op, Op::Repair | Op::Conflict { .. } | Op::Pick { .. } | Op::Enroll);
            assert_eq!(bind(Tool::Git, &op).is_some(), !forklift_only, "{:?}", op);
        }
        let tuned = bind(Tool::GitTuned, &Op::Verify).unwrap();
        assert_eq!(tuned[0][..2], ["-c", "core.fsync=all"]);
        assert_eq!(tuned[0].last().unwrap(), "--strict");
        assert_eq!(bind(Tool::Git, &Op::Verify).unwrap(), vec![line(&["fsck", "--full", "--strict"])]);

        let plain = Scenario { id: "plain".into(), history: vec![Op::Record { index: 0 }, Op::Verify], damage: None };
        assert_eq!(plain.tools(), vec![Tool::Forklift, Tool::Git, Tool::GitTuned]);
        assert!(!plain.forklift_only());
        let repaired = Scenario { id: "repaired".into(), history: vec![Op::Record { index: 0 }, Op::Repair], damage: None };
        assert_eq!(repaired.tools(), vec![Tool::Forklift]);
        assert!(repaired.forklift_only());
        assert_eq!(repaired.records(), vec![0]);
    }

    /// A history's marks are its records and variants in order, and a park writes the variant of
    /// its record, so what it sets aside differs from the head it resets to.
    ///
    /// Revert: have a park write its record → the parked tree equals the head and nothing is set
    /// aside.
    #[test]
    fn marks_follow_the_history_and_a_park_writes_a_variant() {
        let conflicted = catalog().into_iter().find(|scenario| scenario.id == "conflicted").unwrap();
        assert_eq!(conflicted.marks(), vec![Mark::Record(0), Mark::Variant(0), Mark::Record(1)]);
        assert_eq!(conflicted.records(), vec![0, 1]);
        assert_eq!(Mark::Variant(3).message(), "proof-load variant 3");

        let corpus = crate::corpus::generate(1, crate::corpus::Shape { records: 2, initial_files: 3, max_bytes: 16 });
        assert_eq!(Op::Park { index: 1 }.writes(&corpus).unwrap().unwrap(), crate::corpus::variant(&corpus.records[1]));
        assert_eq!(Op::Variant { index: 0 }.writes(&corpus).unwrap().unwrap(), crate::corpus::variant(&corpus.records[0]));
        assert!(Op::Record { index: 2 }.writes(&corpus).unwrap().is_err(), "past the corpus is an error, not a guess");
        assert!(Op::Enroll.writes(&corpus).is_none());
    }
}

