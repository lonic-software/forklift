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
    /// Start a forklift-server over a fresh host warehouse and make it this warehouse's remote.
    /// The server serves for the rest of the scenario, its faulted run included.
    Host,
    /// Move a partial warehouse's origin to the host it already has, which holds all it leans on.
    MoveOrigin,
    /// Have another warehouse enroll on the host: a clone of it elsewhere enrolls its operator and
    /// lifts, so the host serves a trust anchor this warehouse has not seen. The next contact is
    /// this warehouse's first, which judges its unsigned heads against that anchor.
    EnrollHost,
    /// Have the host's server build the bundle a full clone imports.
    Bundle,
    /// Delete pallet `name` on the remote too, which leaves the host a tombstone for it.
    DeleteRemote { name: String },
    /// Have the host's server withhold the content `taken` picks from corpus record `index`.
    Withhold { index: usize },
    /// Record a redaction of the content `taken` picks from corpus record `index`, which the
    /// recording warehouse applies at once.
    Redact { index: usize },
    /// Replace this warehouse with a clone of the host, of the given kind. What the history
    /// acknowledged before stays acknowledged: the clone holds it from the host.
    Clone { kind: CloneKind },
    Push,
    Fetch,
    /// Check the store's integrity.
    Verify,
    /// Repair the store from what it can still reach.
    Repair,
    /// Run one forklift command line, in the folder `at` under the scratch root, keeping what its
    /// document says for later lines: a coverage scenario's way to reach a command no other
    /// operation runs. A placeholder `{name}` stands for a kept value.
    Run { line: Vec<String>, at: String, keep: Vec<Keep> },
    /// Run one of the host's server commands over the host warehouse: `--root` is added.
    Serve { line: Vec<String> },
    /// Stop the host's server, for the server commands refused while it serves. The server is
    /// killed, and the lock it leaves behind cleared.
    StopHost,
    /// Delete every signing key the operator holds, so no one can extend the office chain.
    LoseKeys,
    /// Backdate every file of the host's object store by two days, past any grace period.
    Age,
    /// Write the variant of corpus record `index` into the work tree and record nothing.
    Scribble { index: usize },
    /// Write `text` to the work tree's `path`, recording nothing: a file whose name a line can
    /// give, which no seed moves.
    Write { path: String, text: String },
    /// Build a state only a failed write leaves: run `line` with its `when`-th fsync failing EIO,
    /// for `when` from 1 up, until `until` holds. Needs strace, and is skipped without it: the
    /// write-op check that needs the state runs under strace only.
    Inject { line: Vec<String>, at: String, until: Until },
}

/// A value kept from a command's document: the string at `pointer`, from the text after `after`
/// when given, split on whitespace, word `word`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Keep {
    pub name: String,
    pub pointer: String,
    pub after: Option<String>,
    pub word: usize,
}

/// When an injected state stands.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "until", rename_all = "kebab-case")]
pub enum Until {
    /// A folder under the line's folder holds an entry.
    Holds { folder: String },
    /// A forklift line's document holds `equals` at `pointer`.
    Says { line: Vec<String>, pointer: String, equals: String },
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

/// What a clone takes from its remote.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CloneKind {
    /// Everything: the bundle when the remote has one, then every object.
    Full,
    /// Only the paths under one folder.
    Only(String),
    /// Heads and trees, content fetched when read.
    Cache,
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

/// Stands in a binding's line for the host's URL, which is known only once its server listens.
pub const URL: &str = "{url}";
/// Stands in a binding's line for the work tree's absolute path.
pub const WORK: &str = "{work}";
/// Stands in a coverage line for the scratch area's root, where a line names another warehouse.
pub const SCRATCH: &str = "{scratch}";
/// Stands in a binding's line for `<parcel>:<path>`, the content a redaction takes away, which is
/// known only once its record is made.
pub const TARGET: &str = "{target}";

/// The message a redaction the harness records carries, and the statement a withhold gives.
pub const TAKEN_BECAUSE: &str = "proof-load takes this content away";

/// The content a withhold or a redaction of corpus record `index` takes away: the first path, in
/// order, whose bytes no later record holds anywhere, so no later record stores it again. None when
/// every file of the record lives on.
pub fn taken(corpus: &crate::corpus::Corpus, index: usize) -> Option<(String, Vec<u8>)> {
    let record = corpus.records.get(index)?;
    let later = &corpus.records[index + 1..];
    record.iter().find(|(_, bytes)| !later.iter().any(|snapshot| snapshot.values().any(|other| other == *bytes)))
        .map(|(path, bytes)| (path.clone(), bytes.clone()))
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
                Op::Host => vec![line(&["config", "set", "remote.url", URL])],
                Op::Clone { kind } => {
                    let mut clone = line(&["clone", URL, WORK]);
                    match kind {
                        CloneKind::Full => {}
                        CloneKind::Only(scope) => clone.extend(["--only".to_string(), scope.clone()]),
                        CloneKind::Cache => clone.push("--cache".to_string()),
                    }
                    vec![clone]
                }
                Op::MoveOrigin => vec![line(&["remote", "move", URL])],
                Op::DeleteRemote { name } => vec![line(&["pallet", "delete", name, "--remote"])],
                Op::Redact { .. } => vec![line(&["redact", "record", "-m", TAKEN_BECAUSE, TARGET])],
                // The server's own commands, which the build runs with the server binary.
                Op::Bundle | Op::Withhold { .. } => vec![],
                // Run in another warehouse, which the build lays out.
                Op::EnrollHost => vec![],
                Op::Push => vec![line(&["lift"])],
                Op::Fetch => vec![line(&["lower"])],
                Op::Verify => vec![line(&["audit", "--full"])],
                Op::Repair => vec![line(&["heal"])],
                Op::Run { line, .. } | Op::Inject { line, .. } => vec![line.clone()],
                // The harness's own steps, or the server's commands, which the build runs with
                // the server binary.
                Op::Serve { .. } | Op::StopHost | Op::LoseKeys | Op::Age | Op::Scribble { .. } | Op::Write { .. } => vec![],
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
                // The host is a forklift-server; git has no server of its own to clone from.
                Op::Host | Op::Bundle | Op::Clone { .. } | Op::MoveOrigin | Op::DeleteRemote { .. } | Op::Withhold { .. } | Op::EnrollHost => return None,
                // Git has no redaction: a rewrite of history is a different operation.
                Op::Redact { .. } => return None,
                // Forklift's own commands, and the harness's steps around them.
                Op::Run { .. } | Op::Serve { .. } | Op::StopHost | Op::LoseKeys | Op::Age | Op::Scribble { .. } | Op::Write { .. } | Op::Inject { .. } => return None,
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
    /// What a faulted run's command does after recording its record, so a fault in state only
    /// these operations read (a remote's, say) is reached by the command that grades it.
    pub then: Vec<Op>,
    pub damage: Option<Damage>,
    /// What its records hold: the seed's history, or files of the scenario's own.
    pub files: Files,
}

/// What a scenario's records hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Files {
    /// The seed's generated history.
    Seeded,
    /// The volume faults' own history ([`folded_corpus`]): an executable file and a case-only
    /// rename, the two things a work tree on a folding or no-exec volume can lose.
    Folded,
}

/// The executable file of the folded corpus, in every record.
pub const RUN_SH: &str = "run.sh";
/// The folded corpus's file under its first case, in record 0.
pub const README_BEFORE: &str = "docs/Readme.txt";
/// The same file under its new case, from record 1 on: a case-only rename.
pub const README_AFTER: &str = "docs/README.txt";

/// The corpus of a [`Files::Folded`] scenario, as many records as `corpus` has (at least two),
/// its seed and shape kept for the report. Record 0 holds an executable run.sh, docs/Readme.txt
/// and a.txt; record 1 renames docs/Readme.txt to docs/README.txt, a change of case alone with
/// the same bytes, and edits a.txt, leaving run.sh untouched; each later record edits a.txt again.
/// On a volume that folds case and keeps no executable bit, record 1 is what such a volume can
/// mis-record: the rename unseen, or run.sh recorded without its bit.
pub fn folded_corpus(corpus: &crate::corpus::Corpus) -> crate::corpus::Corpus {
    let record = |index: usize| -> crate::corpus::Snapshot {
        let readme = if index == 0 { README_BEFORE } else { README_AFTER };
        [
            (RUN_SH, b"#!/bin/sh\necho proof-load\n".to_vec()),
            (readme, b"proof-load readme\n".to_vec()),
            ("a.txt", format!("proof-load record {}\n", index).into_bytes()),
        ].into_iter().map(|(path, bytes)| (path.to_string(), bytes)).collect()
    };
    crate::corpus::Corpus {
        records: (0..corpus.records.len().max(2)).map(record).collect(),
        executable: [RUN_SH.to_string()].into(),
        ..corpus.clone()
    }
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

    /// The scope the work tree is left scoped to: that of the last clone, when it took one folder.
    pub fn scope(&self) -> Option<&str> {
        self.history.iter().rev().find_map(|op| match op {
            Op::Clone { kind: CloneKind::Only(scope) } => Some(Some(scope.as_str())),
            Op::Clone { .. } => Some(None),
            _ => None,
        }).flatten()
    }

    /// The record whose content the history withholds, if it does.
    pub fn withheld(&self) -> Option<usize> {
        self.history.iter().find_map(|op| match op { Op::Withhold { index } => Some(*index), _ => None })
    }

    /// The record whose content the history redacts, if it does.
    pub fn redacted(&self) -> Option<usize> {
        self.history.iter().find_map(|op| match op { Op::Redact { index } => Some(*index), _ => None })
    }

    /// Who serves a read of the withheld content when `warehouse` is read back: the host's is read
    /// through a fresh clone, which fetches after the withhold; the client's fetched after it when
    /// a clone follows the withhold in the history, and held the content before otherwise.
    pub fn withhold_reader(&self, warehouse: crate::damage::Warehouse) -> crate::oracle::WithholdReader {
        use crate::oracle::WithholdReader;
        let withheld_at = self.history.iter().position(|op| matches!(op, Op::Withhold { .. }));
        let cloned_after = withheld_at.is_some_and(|at| self.history[at..].iter().any(|op| matches!(op, Op::Clone { .. })));
        match warehouse {
            crate::damage::Warehouse::Host => WithholdReader::FetchedAfter,
            crate::damage::Warehouse::Client if cloned_after => WithholdReader::FetchedAfter,
            crate::damage::Warehouse::Client => WithholdReader::HeldBefore,
        }
    }

    /// The corpus this scenario records from `corpus`: its own files when it has them, rooted
    /// under its scope when it has one, so every record changes what the scoped bay holds
    /// (`corpus::rooted`).
    pub fn corpus(&self, corpus: &crate::corpus::Corpus) -> crate::corpus::Corpus {
        let corpus = match self.files {
            Files::Seeded => corpus.clone(),
            Files::Folded => folded_corpus(corpus),
        };
        match self.scope() {
            Some(scope) => crate::corpus::rooted(&corpus, scope),
            None => corpus,
        }
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
            Op::Park { index } | Op::Scribble { index } => Mark::Variant(*index),
            op => op.mark()?,
        };
        Some(mark.snapshot(corpus).ok_or_else(|| format!("{:?} is past the corpus's {} records", mark, corpus.records.len())))
    }
}

/// The scenarios this build of the harness runs. A damage binding names one of these as the
/// scenario that creates its path, and a run checks that it does.
pub fn catalog() -> Vec<Scenario> {
    let record = |index| Op::Record { index };
    let named = |id: &str, history: Vec<Op>| Scenario { id: id.to_string(), history, then: vec![], damage: None, files: Files::Seeded };
    vec![
        named("history", vec![record(0), record(1), record(2)]),
        named("branched", vec![
            record(0), Op::Branch { name: "side".into() }, Op::Switch { name: "side".into() }, record(1),
            Op::Switch { name: "main".into() }, Op::Merge { name: "side".into() },
        ]),
        // No Verify: `audit` refuses a warehouse with no office, and none of these enrolls one.
        named("repaired", vec![record(0), record(1), Op::Repair]),
        named("parked", vec![record(0), record(1), Op::Park { index: 1 }]),
        // The faulted command lifts its record too: the remote's state is read by lift.
        Scenario { then: vec![Op::Push], ..named("lifted", vec![Op::Host, record(0), record(1), Op::Push]) },
        Scenario { then: vec![Op::Push], ..named("bundled", vec![
            Op::Host, record(0), record(1), Op::Push, Op::Bundle, Op::Clone { kind: CloneKind::Full },
        ]) },
        Scenario { then: vec![Op::Push], ..named("cached", vec![
            Op::Host, record(0), record(1), Op::Push, Op::Clone { kind: CloneKind::Cache },
        ]) },
        Scenario { then: vec![Op::Push], ..named("moved", vec![
            Op::Host, record(0), record(1), Op::Push, Op::Clone { kind: CloneKind::Cache }, Op::MoveOrigin,
        ]) },
        // A sparse clone of one folder; the faulted command records and lifts from the scoped bay.
        Scenario { then: vec![Op::Push], ..named("sparse", vec![
            Op::Host, record(0), record(1), Op::Push, Op::Clone { kind: CloneKind::Only("src".into()) },
        ]) },
        // A host with a tombstone and withheld content, cloned after the withhold, so the clone
        // records the content unavailable; the faulted command records and lifts from the clone.
        Scenario { then: vec![Op::Push], ..named("withheld", vec![
            Op::Host, record(0), record(1), Op::Push, Op::Branch { name: "side".into() }, Op::Switch { name: "side".into() }, Op::Push,
            Op::Switch { name: "main".into() }, Op::DeleteRemote { name: "side".into() }, Op::Withhold { index: 1 },
            Op::Clone { kind: CloneKind::Full },
        ]) },
        // A host another warehouse enrolled after this one lifted record 0, and a side branch of
        // unsigned work that never left: the lower is first contact, which records the side head
        // as one the anchor does not vouch for. The faulted command records and lifts.
        Scenario { then: vec![Op::Push], ..named("unvouched", vec![
            Op::Host, record(0), Op::Push, Op::Branch { name: "side".into() }, Op::Switch { name: "side".into() }, Op::Variant { index: 0 },
            Op::Switch { name: "main".into() }, Op::EnrollHost, Op::Fetch,
        ]) },
        // Content of record 0 no later record holds, redacted once record 1 no longer names it.
        named("redacted", vec![record(0), record(1), Op::Redact { index: 0 }]),
        // Enrolled, so the history ends with a full audit.
        named("signed", vec![record(0), Op::Enroll, record(1), record(2), Op::Verify]),
        named("packed", vec![record(0), Op::Enroll, record(1), record(2), Op::Pack]),
        // The faulted command records and then repacks in full, taking the store and serve locks.
        Scenario { then: vec![run(&["store", "compact", "--all"])], ..named("compacted", vec![record(0), record(1), record(2)]) },
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
        // The volume faults' scenario: record 0, with an executable file, recorded and lifted on
        // the scratch volume, then cloned; the faulted command records the case-only rename and
        // lifts it. Run with a folding volume, the clone lands there (`Scratch::move_work`);
        // without one, at the ordinary work folder, so every other lane builds it as it is.
        Scenario { then: vec![Op::Push], files: Files::Folded, ..named("folded", vec![
            Op::Host, record(0), Op::Push, Op::Clone { kind: CloneKind::Full },
        ]) },
    ]
}

/// The coverage scenarios: histories built only for the write-op check, each reaching commands no
/// faulted scenario runs, with the precondition each needs built first. They are built and walked
/// like every scenario, and never faulted.
pub fn coverage() -> Vec<Scenario> {
    let record = |index| Op::Record { index };
    let named = |id: &str, history: Vec<Op>| Scenario { id: id.to_string(), history, then: vec![], damage: None, files: Files::Seeded };
    let write = |path: &str, text: &str| Op::Write { path: path.to_string(), text: text.to_string() };
    vec![
        // The work tree's own commands, and the read-only ones over a warehouse with history.
        named("staging", [
            // A record writes the whole work tree, so the fixed file follows the records.
            vec![record(0), record(1), run(&["undo"]), write("notes.txt", "notes\n"), run(&["load", "."]), run_keep(&["stack", "-m", "notes"], &[keep("parcel", "/data/parcel")])],
            runs(&[
                &["stocktake"], &["diff"], &["history"], &["blame", "--", "notes.txt"], &["show", "--", "main:notes.txt"],
                &["query"], &["store"], &["store", "status"], &["store", "peek", "--", "{parcel}"], &["pallet"], &["pallet", "list"],
                &["park", "list"], &["bay"], &["config"], &["config", "list"], &["config", "get", "--", "operator.identifier"],
                &["conflicts"], &["scope"], &["scope", "status"], &["remote"], &["tag"], &["tag", "list"], &["redact"],
                &["redact", "list"], &["office"], &["office", "list"], &["haul"], &["haul", "list"], &["manifest"],
                &["self", "alias"], &["self", "alias", "status"], &["profile"], &["profile", "list"],
            ]),
            vec![Op::Scribble { index: 1 }, run(&["restore", "--confirm", "--", "."]), Op::Scribble { index: 1 }],
            // A redaction of content only the staged work still holds waits, until a load takes
            // it out: the head moves on first, since one the head names is refused.
            vec![write("notes.txt", "later\n"), run(&["load", "."]), run(&["stack", "-m", "later"]), write("notes.txt", "notes\n"), run(&["load", "."])],
            vec![run(&["redact", "record", "-m", "redacted", "{parcel}:notes.txt"]), write("notes.txt", "later\n"), run(&["load", "."]), run(&["redact", "apply"])],
            runs(&[&["load", "."], &["unload", "--", "."], &["remove", "--", "notes.txt"]]),
            runs(&[&["config", "set", "--", "mirror.auto", "true"], &["config", "unset", "--", "mirror.auto"]]),
            runs(&[&["bay", "add", "--", "extra", "../extra"], &["bay", "remove", "--", "extra"]]),
        ].concat()),
        // Pallets: a local delete and its undo, a park popped, a delivery, a stopped merge
        // abandoned, and an export imported into another warehouse.
        named("pallets", [
            vec![record(0), Op::Branch { name: "side".into() }, Op::Switch { name: "side".into() }, Op::Variant { index: 0 }],
            runs(&[&["deliver", "-m", "delivered", "--", "main"], &["pallet", "delete", "--force", "--", "side"], &["undo"]]),
            vec![Op::Park { index: 1 }, run(&["park", "pop"]), run(&["load", "."]), run(&["stack", "-m", "popped"])],
            vec![Op::Switch { name: "side".into() }, Op::Variant { index: 1 }, Op::Switch { name: "main".into() }, record(1), Op::Conflict { name: "side".into() }],
            runs(&[&["abort", "--confirm"], &["git", "export", "--", "../exported"]]),
            vec![run_at("imported", &["prepare"]), run_at("imported", &["git", "import", "--", "../exported"])],
        ].concat()),
        // The office: signed facts and tags, operators admitted, keys authorized, linked, rotated
        // and retired, hauls through their life, then every key lost and a re-genesis.
        named("office", [
            vec![record(0), Op::Enroll],
            runs(&[
                &["manifest", "note", "-m", "noted", "--", "main"], &["manifest", "approve", "-m", "approved", "--", "main"],
                &["manifest", "provenance", "--model", "m", "--tool", "t", "-m", "made", "--", "main"],
                &["manifest", "set", "--", "main", "k", "v"], &["manifest", "get", "--", "main", "k"], &["manifest", "keys", "--", "main"],
                &["manifest", "log", "--", "main", "k"], &["manifest", "show", "--", "main"], &["manifest", "unset", "--", "main", "k"],
                &["tag", "create", "-m", "released", "--", "v1"], &["tag", "show", "--", "v1"],
                &["profile", "create", "--", "second"],
            ]),
            vec![
                run_keep(&["profile", "keygen", "--", "second"], &admitted("office admit ", &["who", "key", "pop"])),
                run_keep(&["office", "admit", "--", "{who}", "{key}", "{pop}"], &[Keep { name: "root".into(), pointer: "/data/message".into(), after: Some("identity root ".into()), word: 0 }]),
                run_keep(&["profile", "keygen", "--", "second"], &admitted("office admit ", &["who", "key", "pop"])),
                run(&["office", "authorize", "--", "{who}", "{key}", "{pop}"]), run(&["office", "role", "--", "{who}", "admin"]),
                run(&["office", "retire", "--", "{root}"]),
                run_keep(&["config", "get", "--", "operator.identifier"], &[keep("me", "/data/value")]),
                run(&["profile", "create", "--id", "{me}", "--", "device"]),
                run_keep(&["profile", "keygen", "--", "device"], &admitted("office link ", &["key", "pop"])),
                run(&["office", "link", "--", "{key}", "{pop}"]), run(&["office", "rotate"]),
            ],
            vec![Op::Branch { name: "side".into() }, Op::Switch { name: "side".into() }, record(1)],
            vec![run_keep(&["haul", "open", "--target", "main", "--source", "side", "--title", "t", "-m", "opened"], &[keep("haul", "/data/id")]), record(2)],
            runs(&[
                &["haul", "update", "--", "{haul}"], &["haul", "show", "--", "{haul}"], &["haul", "comment", "-m", "c", "--", "{haul}"],
                &["haul", "close", "--", "{haul}"], &["haul", "reopen", "--", "{haul}"],
            ]),
            vec![Op::Switch { name: "main".into() }, run(&["haul", "consolidate", "--", "{haul}"])],
            vec![Op::LoseKeys, run(&["office", "regenesis", "--confirm"]), run(&["profile", "use", "--", "second"])],
        ].concat()),
        // A host: a pallet deleted there and restored, sparse and cache clones whose scopes and
        // store change, a git mirror, a move of the origin stopped and abandoned, a re-genesis
        // another warehouse accepts, and the server's own commands, the ones refused while it
        // serves after it stops.
        named("hosted", [
            vec![Op::Host, record(0), Op::Enroll, Op::Push, record(1), write("left/a.txt", "a\n"), write("right/b.txt", "b\n")],
            vec![run(&["load", "."]), run(&["stack", "-m", "folders"]), Op::Push],
            vec![Op::Branch { name: "side".into() }, Op::Switch { name: "side".into() }, Op::Variant { index: 1 }, Op::Push, Op::Switch { name: "main".into() }],
            vec![Op::DeleteRemote { name: "side".into() }, run_at("host", &["pallet", "restore", "--", "side"]), run(&["pallet", "list", "--remote"])],
            vec![run_at(".", &["clone", "--", "{url}", "{scratch}/other"])],
            vec![run_at(".", &["clone", "--only", "left", "--only", "right", "--", "{url}", "{scratch}/sparse"])],
            vec![run_at("sparse", &["scope", "narrow", "--", "right"]), run_at("sparse", &["scope", "prune", "--confirm", "--", "right"]), run_at("sparse", &["scope", "fetch", "--", "right"])],
            vec![run_at(".", &["clone", "--cache", "--", "{url}", "{scratch}/cache"]), run_at("cache", &["store", "evict"])],
            vec![
                Op::Inject { line: line(&["remote", "move", "--", "{url}"]), at: "cache".into(), until: Until::Says { line: line(&["remote"]), pointer: "/data/blocked/code".into(), equals: "origin_move_pending".into() } },
                run_at("cache", &["remote", "abort"]),
            ],
            runs(&[&["git", "export", "--", "{scratch}/mirror"], &["config", "set", "--", "mirror.url", "{scratch}/mirror"], &["git", "mirror"]]),
            vec![
                Op::LoseKeys,
                run_keep(&["office", "regenesis", "--confirm"], &[Keep { name: "genesis".into(), pointer: "/data/message".into(), after: Some("New genesis parcel:".into()), word: 0 }]),
                Op::Push, run_at("other", &["office", "accept-regenesis", "--confirm", "{genesis}"]),
            ],
            // History made after the re-genesis, so no trust pin holds it: deleted on the host, its
            // tombstone dropped, and collected once it is old enough.
            vec![Op::Branch { name: "late".into() }, Op::Switch { name: "late".into() }, Op::Variant { index: 0 }, Op::Push, Op::Switch { name: "main".into() }],
            // The listings run with the host stopped, so their digests see no write of the server's.
            vec![Op::DeleteRemote { name: "late".into() }, Op::StopHost, serve(&["tombstone", "list"]), serve(&["withhold", "list"])],
            vec![serve(&["tombstone", "drop", "late"]), Op::Age, serve(&["gc"])],
        ].concat()),
        // A durability taint a failed directory sync leaves, which heal resolves.
        named("tainted", vec![
            record(0), Op::Scribble { index: 0 }, run(&["load", "."]),
            Op::Inject { line: line(&["stack", "-m", "tainted"]), at: "work".into(), until: Until::Holds { folder: ".forklift/taint".into() } },
            Op::Repair,
        ]),
    ]
}

/// A coverage line run in the work tree, keeping nothing.
fn run(arguments: &[&str]) -> Op {
    run_at("work", arguments)
}

fn run_at(at: &str, arguments: &[&str]) -> Op {
    Op::Run { line: line(arguments), at: at.to_string(), keep: vec![] }
}

fn run_keep(arguments: &[&str], keep: &[Keep]) -> Op {
    Op::Run { line: line(arguments), at: "work".to_string(), keep: keep.to_vec() }
}

fn runs(lines: &[&[&str]]) -> Vec<Op> {
    lines.iter().map(|arguments| run(arguments)).collect()
}

fn serve(arguments: &[&str]) -> Op {
    Op::Serve { line: line(arguments) }
}

/// The string at `pointer`, whole.
fn keep(name: &str, pointer: &str) -> Keep {
    Keep { name: name.to_string(), pointer: pointer.to_string(), after: None, word: 0 }
}

/// The words a keygen's message gives after `after`, one name each: the line it says to hand on.
fn admitted(after: &str, names: &[&str]) -> Vec<Keep> {
    names.iter().enumerate().map(|(word, name)| Keep { name: name.to_string(), pointer: "/data/message".into(), after: Some(after.to_string()), word }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_op() -> Vec<Op> {
        vec![
            Op::Record { index: 0 }, Op::Branch { name: "side".into() }, Op::Switch { name: "side".into() },
            Op::Merge { name: "main".into() }, Op::Push, Op::Fetch, Op::Verify, Op::Repair,
            Op::Variant { index: 0 }, Op::Conflict { name: "side".into() }, Op::Pick { name: "side".into() },
            Op::Park { index: 0 }, Op::Enroll, Op::Pack, Op::Host, Op::Clone { kind: CloneKind::Only("src".into()) },
            Op::Clone { kind: CloneKind::Cache }, Op::Bundle, Op::MoveOrigin, Op::DeleteRemote { name: "side".into() },
            Op::Withhold { index: 0 }, Op::Redact { index: 0 }, Op::EnrollHost,
            run(&["undo"]), serve(&["gc"]), Op::StopHost, Op::LoseKeys, Op::Age, Op::Scribble { index: 0 },
            Op::Write { path: "notes.txt".into(), text: "n".into() },
            Op::Inject { line: line(&["stack", "-m", "x"]), at: "work".into(), until: Until::Holds { folder: ".forklift/taint".into() } },
        ]
    }

    /// Forklift binds every operation, each command with `--json`; the record carries its message.
    ///
    /// Revert: drop the `--json` insertion → a refusal prints prose the extractor cannot read.
    #[test]
    fn forklift_binds_every_operation_with_json() {
        for op in every_op() {
            let lines = bind(Tool::Forklift, &op).unwrap_or_else(|| panic!("Tool::Forklift binds {:?}", op));
            // Bundle and withhold are the server's own commands, which the build runs with the
            // server binary; enrolling the host runs in another warehouse, which the build lays out.
            // The harness's own steps write files or stop the host, and run no forklift line.
            let none = matches!(op, Op::Bundle | Op::Withhold { .. } | Op::EnrollHost | Op::Serve { .. } | Op::StopHost | Op::LoseKeys | Op::Age | Op::Scribble { .. } | Op::Write { .. });
            assert_eq!(lines.is_empty(), none, "{:?}", op);
            assert!(lines.iter().all(|arguments| arguments[0] == "--json"), "{:?}", lines);
        }
        assert_eq!(bind(Tool::Forklift, &Op::Clone { kind: CloneKind::Cache }).unwrap(),
            vec![line(&["--json", "clone", URL, WORK, "--cache"])]);
        assert_eq!(bind(Tool::Forklift, &Op::Clone { kind: CloneKind::Only("src".into()) }).unwrap(),
            vec![line(&["--json", "clone", URL, WORK, "--only", "src"])]);
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
            let forklift_only = matches!(op, Op::Repair | Op::Conflict { .. } | Op::Pick { .. } | Op::Enroll | Op::Host | Op::Bundle | Op::Clone { .. } | Op::MoveOrigin
                | Op::DeleteRemote { .. } | Op::Withhold { .. } | Op::Redact { .. } | Op::EnrollHost | Op::Run { .. } | Op::Serve { .. } | Op::StopHost
                | Op::LoseKeys | Op::Age | Op::Scribble { .. } | Op::Write { .. } | Op::Inject { .. });
            assert_eq!(bind(Tool::Git, &op).is_some(), !forklift_only, "{:?}", op);
        }
        let tuned = bind(Tool::GitTuned, &Op::Verify).unwrap();
        assert_eq!(tuned[0][..2], ["-c", "core.fsync=all"]);
        assert_eq!(tuned[0].last().unwrap(), "--strict");
        assert_eq!(bind(Tool::Git, &Op::Verify).unwrap(), vec![line(&["fsck", "--full", "--strict"])]);

        let plain = Scenario { id: "plain".into(), history: vec![Op::Record { index: 0 }, Op::Verify], then: vec![], damage: None, files: Files::Seeded };
        assert_eq!(plain.tools(), vec![Tool::Forklift, Tool::Git, Tool::GitTuned]);
        assert!(!plain.forklift_only());
        let repaired = Scenario { id: "repaired".into(), history: vec![Op::Record { index: 0 }, Op::Repair], then: vec![], damage: None, files: Files::Seeded };
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
        assert_eq!(Op::Scribble { index: 1 }.writes(&corpus).unwrap().unwrap(), crate::corpus::variant(&corpus.records[1]));
        assert!(Op::Scribble { index: 1 }.mark().is_none(), "a scribble records nothing");
    }

    /// What a withhold or a redaction takes away is content of its record that no later record
    /// holds anywhere, so no later record stores it again.
    ///
    /// Revert: take the record's first path → content a later record still holds is taken away,
    /// and the faulted record that writes it again is refused.
    #[test]
    fn taken_content_is_held_by_no_later_record() {
        let snapshot = |files: &[(&str, &str)]| files.iter().map(|(path, bytes)| (path.to_string(), bytes.as_bytes().to_vec())).collect();
        let corpus = crate::corpus::Corpus {
            seed: 0, shape: crate::corpus::Shape::SMALL,
            records: vec![snapshot(&[("a", "1"), ("b", "2"), ("c", "3")]), snapshot(&[("a", "1"), ("b", "9"), ("d", "3")])],
            executable: Default::default(),
        };
        assert_eq!(taken(&corpus, 0), Some(("b".to_string(), b"2".to_vec())));
        assert_eq!(taken(&corpus, 1), Some(("a".to_string(), b"1".to_vec())), "nothing follows the last record");
        let kept = crate::corpus::Corpus { seed: 0, shape: crate::corpus::Shape::SMALL, records: vec![snapshot(&[("a", "1")]), snapshot(&[("z", "1")])], executable: Default::default() };
        assert_eq!(taken(&kept, 0), None, "bytes moved to another path still live on");
        assert_eq!(taken(&corpus, 2), None);
    }

    /// A read of withheld content through the host fetches after the withhold; through the client,
    /// only when a clone follows the withhold.
    ///
    /// Revert: read every client as holding the content before → the withheld scenario's clone
    /// expects bytes it was never sent.
    #[test]
    fn the_withhold_reader_follows_the_clone() {
        use crate::damage::Warehouse;
        use crate::oracle::WithholdReader;
        let withheld = catalog().into_iter().find(|scenario| scenario.id == "withheld").unwrap();
        assert_eq!(withheld.withheld(), Some(1));
        assert_eq!(withheld.withhold_reader(Warehouse::Client), WithholdReader::FetchedAfter);
        assert_eq!(withheld.withhold_reader(Warehouse::Host), WithholdReader::FetchedAfter);
        let held = Scenario { id: "h".into(), history: vec![Op::Host, Op::Clone { kind: CloneKind::Full }, Op::Withhold { index: 0 }], then: vec![], damage: None, files: Files::Seeded };
        assert_eq!(held.withhold_reader(Warehouse::Client), WithholdReader::HeldBefore);
        assert_eq!(held.withhold_reader(Warehouse::Host), WithholdReader::FetchedAfter);
    }

    /// A scenario is scoped by its last clone, and only when that clone took one folder; a scoped
    /// scenario records the corpus rooted under its scope, any other the corpus as it is.
    ///
    /// Revert: take the first clone's scope → a full clone after a sparse one stays scoped.
    #[test]
    fn the_last_clone_decides_the_scope() {
        let only = |scope: &str| Op::Clone { kind: CloneKind::Only(scope.into()) };
        let scenario = |history| Scenario { id: "s".into(), history, then: vec![], damage: None, files: Files::Seeded };
        assert_eq!(scenario(vec![Op::Host, only("src")]).scope(), Some("src"));
        assert_eq!(scenario(vec![only("src"), Op::Clone { kind: CloneKind::Full }]).scope(), None);
        assert_eq!(scenario(vec![Op::Clone { kind: CloneKind::Cache }, only("docs")]).scope(), Some("docs"));
        assert_eq!(scenario(vec![Op::Record { index: 0 }]).scope(), None);

        let corpus = crate::corpus::generate(1, crate::corpus::Shape::SMALL);
        let sparse = catalog().into_iter().find(|scenario| scenario.id == "sparse").unwrap();
        assert_eq!(sparse.corpus(&corpus), crate::corpus::rooted(&corpus, "src"));
        assert_eq!(scenario(vec![]).corpus(&corpus), corpus);
    }

    /// The folded corpus keeps its records' count, marks run.sh executable, and record 1 is a
    /// case-only rename of the readme, with the same bytes, plus an edit, run.sh untouched; the
    /// `folded` scenario records that corpus, lifts record 0 and clones before its faulted command.
    ///
    /// Revert: change the readme's bytes in record 1 → the rename is no longer case-only; drop the
    /// executable mark → no record can lose the bit.
    #[test]
    fn the_folded_corpus_carries_both_volume_faults() {
        let seeded = crate::corpus::generate(3, crate::corpus::Shape { records: 4, ..crate::corpus::Shape::SMALL });
        let folded = folded_corpus(&seeded);
        assert_eq!((folded.records.len(), folded.seed), (4, 3));
        assert_eq!(folded.executable, [RUN_SH.to_string()].into());
        let (first, second) = (&folded.records[0], &folded.records[1]);
        assert!(first.contains_key(README_BEFORE) && !first.contains_key(README_AFTER));
        assert!(second.contains_key(README_AFTER) && !second.contains_key(README_BEFORE));
        assert_eq!(README_BEFORE.to_lowercase(), README_AFTER.to_lowercase());
        assert_eq!(first[README_BEFORE], second[README_AFTER]);
        assert_eq!(first[RUN_SH], second[RUN_SH]);
        assert_ne!(first["a.txt"], second["a.txt"]);
        for pair in folded.records.windows(2) {
            assert_ne!(pair[0], pair[1]);
        }
        assert_eq!(folded_corpus(&crate::corpus::generate(3, crate::corpus::Shape { records: 1, ..crate::corpus::Shape::SMALL })).records.len(), 2);

        let scenario = catalog().into_iter().find(|scenario| scenario.id == "folded").unwrap();
        assert_eq!(scenario.corpus(&seeded), folded);
        assert_eq!(scenario.marks(), vec![Mark::Record(0)]);
        assert!(matches!(scenario.history.last(), Some(Op::Clone { kind: CloneKind::Full })), "the clone is what moves the work tree");
        assert_eq!(scenario.then, vec![Op::Push]);
        assert_eq!(scenario.scope(), None);
    }
}
