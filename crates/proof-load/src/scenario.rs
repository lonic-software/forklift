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
    /// Create a branch at the current head.
    Branch { name: String },
    /// Make `name` the current branch.
    Switch { name: String },
    /// Merge `name` into the current branch.
    Merge { name: String },
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

/// The message a recorded corpus record carries, so a readback can find each record by it.
pub fn record_message(index: usize) -> String {
    format!("proof-load record {}", index)
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
                Op::Branch { name } => vec![line(&["pallet", "create", name])],
                Op::Switch { name } => vec![line(&["shift", name])],
                Op::Merge { name } => vec![line(&["consolidate", name])],
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
                Op::Branch { name } => vec![line(&["branch", name])],
                Op::Switch { name } => vec![line(&["switch", "-q", name])],
                Op::Merge { name } => vec![line(&["merge", "-q", "--no-edit", name])],
                Op::Push => vec![line(&["push", "-q"])],
                Op::Fetch => vec![line(&["pull", "-q", "--ff-only"])],
                Op::Verify => vec![line(&["fsck", "--full", "--strict"])],
                // Git has no command that rebuilds a store from what it can still reach.
                Op::Repair => return None,
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
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_op() -> Vec<Op> {
        vec![
            Op::Record { index: 0 }, Op::Branch { name: "side".into() }, Op::Switch { name: "side".into() },
            Op::Merge { name: "main".into() }, Op::Push, Op::Fetch, Op::Verify, Op::Repair,
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
            assert_eq!(bind(Tool::Git, &op).is_some(), op != Op::Repair, "{:?}", op);
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
}
