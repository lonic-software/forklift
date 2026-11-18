//! `proof-load perf`: Forklift timed side by side with git, on one generated history, one command
//! at a time with nothing else from the harness running.
//!
//! A perf run is not a graded run: it writes its own document, perf.json and perf.md, under its
//! own format number, [`PERF_FORMAT`], and never touches `run`'s report. Each row pairs one
//! operation across three subjects (forklift, git as every repository git makes is configured,
//! and git tuned as a careful user runs it), each timed over `--reps` repetitions and reported as
//! median, minimum and maximum. Before a pair is timed the harness checks that every subject sees
//! the same change; a pair that fails the check is reported as not comparable, never timed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::generate::{self, Generated, Shape};
use crate::report::Platform;
use crate::runner::{self, RunError as BuildError};
use crate::scenario::{Tool, GIT_TUNED};
use crate::scrub::{Environment, Output, Scratch};
use crate::tool::{self, Invocation, ToolError};

/// The perf document's format. A reader refuses any other number rather than guess at it.
pub const PERF_FORMAT: u32 = 1;

/// How long one timed or untimed command of the loop may run.
const COMMAND_LIMIT: Duration = Duration::from_secs(10 * 60);
/// How long one step of building a subject may run: an import of a large history is slow.
const BUILD_LIMIT: Duration = Duration::from_secs(30 * 60);
/// The CPU time below which a repetition's cores figure is not reported: rusage's CPU times do
/// not resolve a command much shorter than this.
const CORES_FLOOR: Duration = Duration::from_millis(20);
/// The most tracked files the diff pairs edit.
const EDITED: usize = 20;
const EDIT: &str = "\n// benchmark edit\n";

/// One perf run's document.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerfReport {
    pub format: u32,
    pub header: Header,
    pub rows: Vec<Row>,
}

/// What a perf run ran on: everything the graded report's header records, plus the hardware.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    pub seed: u64,
    pub shape: String,
    /// The shape's scale, spelled out.
    pub shape_detail: String,
    pub reps: usize,
    /// Each binary by path with the first line of its `--version`.
    pub tools: BTreeMap<String, String>,
    pub platform: Platform,
    pub os: String,
    /// The filesystem of the output directory, which every subject lives under.
    pub filesystem: String,
    /// Each git subject's configuration beyond what the binary defaults to.
    pub configurations: BTreeMap<String, String>,
    pub hardware: Hardware,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hardware {
    pub cpu: String,
    pub logical_cores: usize,
    /// Physical memory in bytes, where the platform's is read.
    pub ram_bytes: Option<u64>,
}

/// One operation, timed on every subject or not, with why not.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub operation: String,
    pub note: String,
    pub outcome: Outcome,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    /// Every subject ran, each in its own cell.
    Timed { cells: Vec<Cell> },
    /// The subjects did not see the same change before the pair, so their timings would compare
    /// different work.
    NotComparable { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cell {
    pub subject: Tool,
    /// The arguments after the binary, as every repetition ran them.
    pub command: Vec<String>,
    pub result: Measured,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Measured {
    Timed { wall: Spread, memory: Memory, cores: Cores },
    /// A run did not succeed, so the cell has no timing: its exit, or the signal it died of when
    /// it had no exit, and what it said on stderr.
    Voided { exit: Option<i32>, signal: Option<i32>, stderr: String },
    /// A run, the untimed one or a repetition, ran past its limit and was killed; what it said is
    /// not kept.
    KilledAtLimit { limit_seconds: u64 },
}

/// Microseconds over the repetitions: never the mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spread {
    pub median: u64,
    pub min: u64,
    pub max: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Memory {
    /// Peak resident set size in bytes over the repetitions.
    Measured { peak_bytes: Spread },
    NotMeasured { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Cores {
    /// The median over the repetitions of (user + system CPU time) / wall time, as a percentage
    /// of one core: 150 is one and a half cores busy throughout. A whole number, so a perf.json
    /// reads back as exactly what was written.
    Measured { median_percent: u64 },
    NotMeasured { reason: String },
}

impl PerfReport {
    /// Reads a perf.json, refusing a format other than [`PERF_FORMAT`] by its number before
    /// anything else, and any field this build does not know.
    pub fn from_json(text: &str) -> Result<PerfReport, String> {
        let value: serde_json::Value = serde_json::from_str(text).map_err(|error| format!("not a perf report: {}", error))?;
        match value.get("format") {
            Some(format) if format.as_u64() == Some(u64::from(PERF_FORMAT)) => {}
            Some(format) => return Err(format!("perf report format {} is not one this build reads (it reads {})", format, PERF_FORMAT)),
            None => return Err("not a perf report: it names no format".to_string()),
        }
        serde_json::from_value(value).map_err(|error| format!("not a perf report of format {}: {}", PERF_FORMAT, error))
    }

    pub fn json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|error| format!("{{\"error\":\"{}\"}}", error))
    }

    /// Whether the run failed to compare something it set out to: a row not comparable, or a cell
    /// with no timing.
    pub fn failed(&self) -> bool {
        self.rows.iter().any(|row| match &row.outcome {
            Outcome::NotComparable { .. } => true,
            Outcome::Timed { cells } => cells.iter().any(|cell| !matches!(cell.result, Measured::Timed { .. })),
        })
    }

    pub fn markdown(&self) -> String {
        let header = &self.header;
        let mut out = String::from("# Proof Load performance\n\n## Header\n\n");
        let _ = writeln!(out, "- format: {}", self.format);
        let _ = writeln!(out, "- shape: {} (seed {}): {}", header.shape, header.seed, header.shape_detail);
        let _ = writeln!(out, "- repetitions per cell: {}", header.reps);
        let _ = writeln!(out, "- platform: {} ({})\n- filesystem: {}", header.platform.label(), header.os, header.filesystem);
        let ram = header.hardware.ram_bytes.map_or_else(|| "not read".to_string(), bytes);
        let _ = writeln!(out, "- hardware: {}, {} logical cores, {} of memory", header.hardware.cpu, header.hardware.logical_cores, ram);
        for (tool, version) in &header.tools {
            let _ = writeln!(out, "- {}: {}", tool, version);
        }
        for (subject, configuration) in &header.configurations {
            let _ = writeln!(out, "- {} configuration: {}", subject, configuration);
        }
        out.push_str("\n## Rows\n");
        for row in &self.rows {
            let _ = writeln!(out, "\n### {}\n\n{}\n", row.operation, row.note);
            match &row.outcome {
                Outcome::NotComparable { reason } => { let _ = writeln!(out, "Not comparable: {}", reason); }
                Outcome::Timed { cells } => {
                    out.push_str("| subject | median | fastest | slowest | peak memory | cores | command |\n|---|---|---|---|---|---|---|\n");
                    for cell in cells {
                        let command = format!("`{}`", cell.command.join(" "));
                        match &cell.result {
                            Measured::Timed { wall, memory, cores } => {
                                let memory = match memory {
                                    Memory::Measured { peak_bytes } => format!("{} ({}–{})", bytes(peak_bytes.median), bytes(peak_bytes.min), bytes(peak_bytes.max)),
                                    Memory::NotMeasured { reason } => reason.clone(),
                                };
                                let cores = match cores {
                                    Cores::Measured { median_percent } => format!("{:.2}", *median_percent as f64 / 100.0),
                                    Cores::NotMeasured { reason } => reason.clone(),
                                };
                                let _ = writeln!(out, "| {} | {} | {} | {} | {} | {} | {} |", cell.subject.label(), micros(wall.median), micros(wall.min), micros(wall.max), memory, cores, command);
                            }
                            Measured::Voided { exit, signal, stderr } => {
                                let ended = match (exit, signal) {
                                    (Some(exit), _) => format!("exit {}", exit),
                                    (None, Some(signal)) => format!("signal {}", signal),
                                    (None, None) => "no exit".to_string(),
                                };
                                let _ = writeln!(out, "| {} | voided: {}: {} | | | | | {} |", cell.subject.label(), ended, stderr.trim().replace('\n', " ").replace('|', "\\|"), command);
                            }
                            Measured::KilledAtLimit { limit_seconds } => {
                                let _ = writeln!(out, "| {} | voided: killed at its {} s limit | | | | | {} |", cell.subject.label(), limit_seconds, command);
                            }
                        }
                    }
                }
            }
        }
        out
    }
}

fn micros(value: u64) -> String {
    match value {
        0..1_000 => format!("{} µs", value),
        1_000..1_000_000 => format!("{:.2} ms", value as f64 / 1e3),
        _ => format!("{:.2} s", value as f64 / 1e6),
    }
}

fn bytes(value: u64) -> String {
    format!("{:.1} MiB", value as f64 / (1024.0 * 1024.0))
}

/// What `proof-load perf` was asked to run.
#[derive(Clone, Debug)]
pub struct PerfOptions {
    pub out: PathBuf,
    pub forklift: PathBuf,
    pub git: PathBuf,
    pub shape: String,
    pub reps: usize,
}

/// One subject a pair runs on: its tool, the binary, the arguments every one of its command lines
/// starts with, and its work tree.
struct Subject {
    tool: Tool,
    program: PathBuf,
    prefix: Vec<String>,
    work: PathBuf,
}

impl Subject {
    fn line(&self, arguments: &[&str]) -> Vec<String> {
        self.prefix.iter().cloned().chain(arguments.iter().map(|argument| argument.to_string())).collect()
    }

    fn invocation(&self, arguments: Vec<String>) -> Invocation {
        Invocation { program: self.program.clone(), arguments, directory: self.work.clone(), limit: COMMAND_LIMIT }
    }
}

/// Runs a perf run and writes perf.md and then perf.json under `options.out`, perf.json last, so
/// a run that stopped leaves none. Refuses, writing no perf.json, when it cannot build what it
/// times.
pub fn run(options: &PerfOptions, progress: &dyn Fn(&str)) -> Result<PerfReport, String> {
    let shape = Shape::named(&options.shape).ok_or_else(|| {
        let names: Vec<&str> = Shape::ALL.iter().map(|shape| shape.name).collect();
        format!("--shape {}: not a shape this build has (it has {})", options.shape, names.join(", "))
    })?;
    if options.reps == 0 {
        return Err("--reps 0: a cell needs at least one repetition".to_string());
    }
    let absolute = |flag: &str, path: &Path| tool::absolute(path).map_err(|error| format!("{} {}: {}", flag, path.display(), error));
    let forklift = absolute("--forklift", &options.forklift)?;
    let git = absolute("--git", &options.git)?;
    let mut output = Output::claim(&options.out).map_err(|error| error.to_string())?;
    let scratch = output.scratch("perf").map_err(|error| error.to_string())?;
    let search_path = crate::scrub::search_path(Some(&git), std::env::var_os("PATH"));
    let environment = Environment::scrubbed(&scratch, search_path, &BTreeMap::new()).map_err(|error| error.to_string())?;

    let built = build(&forklift, &git, &shape, &scratch, &environment, progress).map_err(|error| format!("building the subjects failed: {}", error))?;
    let daemon = built.daemon.is_some();
    let rows = pairs(&built, options.reps, &scratch, &environment, progress).map_err(|error| error.to_string())?;
    drop(built);

    let mut tools = BTreeMap::new();
    tools.insert("forklift".to_string(), crate::run::described(&forklift));
    tools.insert("git".to_string(), crate::run::described(&git));
    let mut configurations = BTreeMap::new();
    configurations.insert(Tool::Git.label().to_string(), crate::run::created_configuration());
    configurations.insert(Tool::GitTuned.label().to_string(), format!("{}; a commit-graph written untimed after the build", crate::run::tuned_configuration(daemon)));
    let header = Header {
        seed: shape.seed, shape: shape.name.to_string(), shape_detail: shape.describe(), reps: options.reps, tools,
        platform: Platform::current(), os: crate::run::operating_system(), filesystem: crate::run::filesystem_of(output.root()),
        configurations, hardware: hardware(),
    };
    let report = PerfReport { format: PERF_FORMAT, header, rows };
    for (name, text) in [("perf.md", report.markdown()), ("perf.json", report.json())] {
        std::fs::write(output.root().join(name), text).map_err(|error| format!("cannot write {}: {}", name, error))?;
    }
    Ok(report)
}

/// The three subjects over one generated history, and the daemon kept running for tuned git.
struct Built {
    generated: Generated,
    subjects: Vec<Subject>,
    /// Stopped when dropped.
    daemon: Option<runner::FsmonitorDaemon>,
}

fn build(forklift: &Path, git: &Path, shape: &Shape, scratch: &Scratch, environment: &Environment, progress: &dyn Fn(&str)) -> Result<Built, BuildError> {
    let work = scratch.work();
    progress(&format!("generating the {} history", shape.name));
    let mirror = work.join("mirror.git");
    let generated = generate::mirror(git, shape, &mirror, scratch, environment, BUILD_LIMIT)?;
    let mut steps = Vec::new();
    progress("building git's subject");
    let branch = crate::history::build_git(git, &mirror, &work.join("git"), scratch, environment, BUILD_LIMIT, &mut steps)?;
    progress("building forklift's subject");
    crate::history::build_forklift(forklift, &mirror, &branch, &work.join("forklift"), scratch, environment, BUILD_LIMIT, &mut steps)?;
    progress("building tuned git's subject");
    let tuned = work.join("git-tuned");
    crate::history::build_git(git, &mirror, &tuned, scratch, environment, BUILD_LIMIT, &mut steps)?;
    let graph = Invocation { program: git.to_path_buf(), arguments: vec!["commit-graph".into(), "write".into(), "--reachable".into()], directory: tuned.clone(), limit: BUILD_LIMIT };
    let written = tool::run(&graph, scratch, environment).map_err(BuildError::Tool)?;
    if !written.succeeded() {
        return Err(BuildError::BuildFailed {
            arguments: written.arguments, exit: written.exit,
            stderr: String::from_utf8_lossy(&written.stderr).into_owned(), stdout: String::from_utf8_lossy(&written.stdout).into_owned(),
        });
    }
    let daemon = if runner::has_fsmonitor_daemon(git) {
        Some(runner::FsmonitorDaemon::start(git, &tuned, scratch, environment, BUILD_LIMIT)?.0)
    } else {
        None
    };
    let subjects = vec![
        Subject { tool: Tool::Forklift, program: forklift.to_path_buf(), prefix: vec![], work: work.join("forklift") },
        Subject { tool: Tool::Git, program: git.to_path_buf(), prefix: vec![], work: work.join("git") },
        Subject { tool: Tool::GitTuned, program: git.to_path_buf(), prefix: GIT_TUNED.iter().map(|option| option.to_string()).collect(), work: tuned },
    ];
    Ok(Built { generated, subjects, daemon })
}

/// Why a loop step could not go on: a tool that could not be run at all, or a file the harness
/// could not edit. A command that ran and failed is a voided cell or a not-comparable row instead.
#[derive(Debug)]
struct Stopped(String);

impl std::fmt::Display for Stopped {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<ToolError> for Stopped {
    fn from(error: ToolError) -> Stopped {
        Stopped(error.to_string())
    }
}

/// The read pairs, warm: status, log and blame on the clean tree, then diff and diff --staged
/// over one edit set.
fn pairs(built: &Built, reps: usize, scratch: &Scratch, environment: &Environment, progress: &dyn Fn(&str)) -> Result<Vec<Row>, Stopped> {
    let subjects = &built.subjects;
    let blame = built.generated.blame_file.as_str();
    let edited: Vec<&str> = built.generated.text_files.iter().take(EDITED).map(String::as_str).collect();
    let mut rows = Vec::new();

    // One untimed read after the build, so the first timing pays for no cache the build left cold.
    for subject in subjects {
        let read = if subject.tool == Tool::Forklift { subject.line(&["stocktake", "--summary"]) } else { subject.line(&["status"]) };
        tool::run(&subject.invocation(read), scratch, environment)?;
    }

    let edited_set: BTreeSet<String> = edited.iter().map(|file| file.to_string()).collect();
    let clean = same(subjects, &Changes::default(), scratch, environment)?;
    let reads: [(&str, String, [&[&str]; 2]); 3] = [
        ("status", "Forklift's stocktake --summary against git's status, on the clean tree".to_string(), [&["stocktake", "--summary"], &["status"]]),
        ("log", "Forklift's history against git's log, the whole history".to_string(), [&["history"], &["log"]]),
        ("blame", format!("{}, the file the history changes most, first-parent on both", blame), [&["blame", blame], &["blame", "--first-parent", blame]]),
    ];
    for (operation, note, [forklift, git]) in reads {
        progress(&format!("timing {}", operation));
        let outcome = match &clean {
            Ok(()) => timed_pair(subjects, forklift, git, reps, scratch, environment)?,
            Err(reason) => Outcome::NotComparable { reason: format!("before the clean-tree rows, {}", reason) },
        };
        rows.push(Row { operation: operation.to_string(), note, outcome });
    }

    for subject in subjects {
        for file in &edited {
            let path = subject.work.join(file);
            let mut text = std::fs::read(&path).map_err(|error| Stopped(format!("{}: {}", path.display(), error)))?;
            text.extend_from_slice(EDIT.as_bytes());
            std::fs::write(&path, text).map_err(|error| Stopped(format!("{}: {}", path.display(), error)))?;
        }
        // The untimed read after the edit, as after the build.
        let read = if subject.tool == Tool::Forklift { subject.line(&["stocktake", "--summary"]) } else { subject.line(&["status"]) };
        tool::run(&subject.invocation(read), scratch, environment)?;
    }
    let count = edited.len();
    let note = format!("{} tracked files edited, unstaged", count);
    progress("timing diff");
    let unstaged = Changes { staged: BTreeSet::new(), unstaged: edited_set.clone() };
    let outcome = match same(subjects, &unstaged, scratch, environment)? {
        Ok(()) => timed_pair(subjects, &["diff"], &["diff"], reps, scratch, environment)?,
        Err(reason) => Outcome::NotComparable { reason: format!("before diff, {}", reason) },
    };
    rows.push(Row { operation: "diff".to_string(), note, outcome });

    for subject in subjects {
        let stage = if subject.tool == Tool::Forklift { subject.line(&["load", "."]) } else { subject.line(&["add", "-A"]) };
        tool::run(&subject.invocation(stage), scratch, environment)?;
    }
    let note = format!("the same {} files staged, untimed, by Forklift's load . and git's add -A", count);
    progress("timing diff --staged");
    let staged = Changes { staged: edited_set, unstaged: BTreeSet::new() };
    let outcome = match same(subjects, &staged, scratch, environment)? {
        Ok(()) => timed_pair(subjects, &["diff", "--staged"], &["diff", "--staged"], reps, scratch, environment)?,
        Err(reason) => Outcome::NotComparable { reason: format!("before diff --staged, {}", reason) },
    };
    rows.push(Row { operation: "diff --staged".to_string(), note, outcome });
    Ok(rows)
}

/// The paths a subject's work tree differs from its last commit at: those staged, and those
/// changed but not staged, untracked ones included.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Changes {
    staged: BTreeSet<String>,
    unstaged: BTreeSet<String>,
}

impl Changes {
    /// How `self`, what a subject saw, differs from `expected`: each side's extra and missing
    /// paths, a few of each.
    fn against(&self, expected: &Changes) -> String {
        let side = |name: &str, seen: &BTreeSet<String>, wanted: &BTreeSet<String>| -> Option<String> {
            let few = |paths: Vec<&String>| paths.iter().take(3).map(|path| path.as_str()).collect::<Vec<_>>().join(", ");
            let extra: Vec<&String> = seen.difference(wanted).collect();
            let missing: Vec<&String> = wanted.difference(seen).collect();
            let mut parts = Vec::new();
            if !extra.is_empty() {
                parts.push(format!("{} more ({})", extra.len(), few(extra)));
            }
            if !missing.is_empty() {
                parts.push(format!("{} missing ({})", missing.len(), few(missing)));
            }
            (!parts.is_empty()).then(|| format!("{} {} of {} expected, {}", seen.len(), name, wanted.len(), parts.join(" and ")))
        };
        [side("staged", &self.staged, &expected.staged), side("unstaged", &self.unstaged, &expected.unstaged)].into_iter().flatten().collect::<Vec<_>>().join("; ")
    }
}

/// What `subject` reports as changed: Forklift by `--json stocktake`'s path lists, git by
/// `status --porcelain` with every untracked file listed. Gives why when the read failed.
fn changes(subject: &Subject, scratch: &Scratch, environment: &Environment) -> Result<Result<Changes, String>, Stopped> {
    let arguments: &[&str] = if subject.tool == Tool::Forklift { &["--json", "stocktake"] } else { &["status", "--porcelain", "--untracked-files=all"] };
    let outcome = tool::run(&subject.invocation(subject.line(arguments)), scratch, environment)?;
    if !outcome.succeeded() {
        return Ok(Err(format!("{} {} exited {:?}: {}", subject.tool.label(), arguments.join(" "), outcome.exit, String::from_utf8_lossy(&outcome.stderr).trim())));
    }
    if subject.tool == Tool::Forklift {
        let document = runner::document(&outcome.stdout);
        let paths = |field: &str| -> Option<BTreeSet<String>> {
            document["data"][field].as_array()?.iter().map(|change| change["path"].as_str().map(str::to_string)).collect()
        };
        return Ok(match (paths("staged"), paths("unstaged")) {
            (Some(staged), Some(unstaged)) => Ok(Changes { staged, unstaged }),
            _ => Err("Forklift's --json stocktake gave no staged and unstaged path lists".to_string()),
        });
    }
    Ok(Ok(porcelain(&String::from_utf8_lossy(&outcome.stdout))))
}

/// The changes in `git status --porcelain` lines `XY path`: X, the index's column, names a staged
/// change; Y, the work tree's, an unstaged one, and `??` an untracked file.
fn porcelain(text: &str) -> Changes {
    let mut changes = Changes::default();
    for line in text.lines().filter(|line| line.len() > 3) {
        let (columns, path) = line.split_at(3);
        let path = path.split(" -> ").last().unwrap_or(path).to_string();
        let mut columns = columns.chars();
        if columns.next().is_some_and(|index| index != ' ' && index != '?') {
            changes.staged.insert(path.clone());
        }
        if columns.next().is_some_and(|tree| tree != ' ') {
            changes.unstaged.insert(path);
        }
    }
    changes
}

/// Whether every subject reports exactly `expected` changed, or what each one that did not saw.
fn same(subjects: &[Subject], expected: &Changes, scratch: &Scratch, environment: &Environment) -> Result<Result<(), String>, Stopped> {
    let mut differ = Vec::new();
    for subject in subjects {
        match changes(subject, scratch, environment)? {
            Ok(seen) if seen == *expected => {}
            Ok(seen) => differ.push(format!("{} saw {}", subject.tool.label(), seen.against(expected))),
            Err(reason) => differ.push(reason),
        }
    }
    Ok(if differ.is_empty() { Ok(()) } else { Err(differ.join("; ")) })
}

/// Times forklift's `forklift` line and both gits' `git` line.
fn timed_pair(subjects: &[Subject], forklift: &[&str], git: &[&str], reps: usize, scratch: &Scratch, environment: &Environment) -> Result<Outcome, Stopped> {
    let mut cells = Vec::new();
    for subject in subjects {
        let command = subject.line(if subject.tool == Tool::Forklift { forklift } else { git });
        let result = measure(&subject.invocation(command.clone()), reps, scratch, environment)?;
        cells.push(Cell { subject: subject.tool, command, result });
    }
    Ok(Outcome::Timed { cells })
}

/// One cell: an untimed run first, whose exit is not judged, so the cache is warm; then `reps`
/// timed repetitions. The first repetition that does not succeed voids the cell, and so does any
/// run, the untimed one included, killed at its limit.
fn measure(invocation: &Invocation, reps: usize, scratch: &Scratch, environment: &Environment) -> Result<Measured, Stopped> {
    let killed = || Measured::KilledAtLimit { limit_seconds: invocation.limit.as_secs() };
    match tool::run(invocation, scratch, environment) {
        Ok(_) => {}
        Err(ToolError::TimedOut { .. }) => return Ok(killed()),
        Err(error) => return Err(error.into()),
    }
    let mut runs = Vec::with_capacity(reps);
    for _ in 0..reps {
        let timed = match tool::timed(invocation, scratch, environment) {
            Ok(timed) => timed,
            Err(ToolError::TimedOut { .. }) => return Ok(killed()),
            Err(error) => return Err(error.into()),
        };
        if !timed.outcome.succeeded() {
            return Ok(Measured::Voided { exit: timed.outcome.exit, signal: timed.signal, stderr: String::from_utf8_lossy(&timed.outcome.stderr).into_owned() });
        }
        runs.push(timed);
    }
    Ok(summarise(&runs))
}

/// The median, fastest and slowest of `values`; the median of an even count is the mean of the
/// middle two.
fn spread(mut values: Vec<u64>) -> Spread {
    values.sort_unstable();
    let middle = values.len() / 2;
    let median = if values.len().is_multiple_of(2) { (values[middle - 1] + values[middle]) / 2 } else { values[middle] };
    Spread { median, min: values[0], max: values[values.len() - 1] }
}

fn summarise(runs: &[tool::Timed]) -> Measured {
    let wall = spread(runs.iter().map(|run| u64::try_from(run.wall.as_micros()).unwrap_or(u64::MAX)).collect());
    let usages: Option<Vec<tool::Usage>> = runs.iter().map(|run| run.usage).collect();
    let (memory, cores) = match usages {
        None => {
            let reason = "not measured on Windows".to_string();
            (Memory::NotMeasured { reason: reason.clone() }, Cores::NotMeasured { reason })
        }
        Some(usages) => {
            let memory = Memory::Measured { peak_bytes: spread(usages.iter().map(|usage| usage.peak_bytes).collect()) };
            let cpu: Vec<Duration> = usages.iter().map(|usage| usage.user + usage.system).collect();
            let cores = if cpu.iter().all(|cpu| *cpu >= CORES_FLOOR) {
                let percents = cpu.iter().zip(runs).map(|(cpu, run)| {
                    let wall = run.wall.as_micros().max(1);
                    u64::try_from(cpu.as_micros() * 100 / wall).unwrap_or(u64::MAX)
                }).collect();
                Cores::Measured { median_percent: spread(percents).median }
            } else {
                Cores::NotMeasured { reason: format!("too short to measure: under {} ms of CPU time", CORES_FLOOR.as_millis()) }
            };
            (memory, cores)
        }
    };
    Measured::Timed { wall, memory, cores }
}

fn hardware() -> Hardware {
    let logical_cores = std::thread::available_parallelism().map_or(1, usize::from);
    Hardware { cpu: cpu_model(), logical_cores, ram_bytes: ram_bytes() }
}

#[cfg(target_os = "macos")]
fn cpu_model() -> String {
    let name = c"machdep.cpu.brand_string";
    let mut buffer = [0u8; 256];
    let mut length: libc::size_t = buffer.len();
    // SAFETY: sysctlbyname writes at most `length` bytes into the buffer and the length it wrote
    // back into `length`, and returns -1 on failure.
    let read = unsafe { libc::sysctlbyname(name.as_ptr(), buffer.as_mut_ptr().cast(), &mut length, std::ptr::null_mut(), 0) };
    if read != 0 {
        return format!("unknown: {}", std::io::Error::last_os_error());
    }
    String::from_utf8_lossy(&buffer[..length.min(buffer.len())]).trim_end_matches('\0').trim().to_string()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo").ok()
        .and_then(|info| info.lines().find_map(|line| line.strip_prefix("model name").and_then(|rest| rest.split_once(':')).map(|(_, model)| model.trim().to_string())))
        .unwrap_or_else(|| "unknown: /proc/cpuinfo names no model".to_string())
}

#[cfg(not(unix))]
fn cpu_model() -> String {
    std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "unknown: PROCESSOR_IDENTIFIER is not set".to_string())
}

#[cfg(unix)]
fn ram_bytes() -> Option<u64> {
    // SAFETY: sysconf takes no pointers and returns -1 for a name it does not know.
    let (pages, size) = unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES), libc::sysconf(libc::_SC_PAGESIZE)) };
    Some(u64::try_from(pages).ok()?.saturating_mul(u64::try_from(size).ok()?))
}

#[cfg(not(unix))]
fn ram_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn scratch(label: &str) -> (PathBuf, Scratch, Environment) {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("proof-load-perf-{}-{}-{}", label, std::process::id(), nanos));
        let scratch = Scratch::create(&root).unwrap();
        let environment = Environment::scrubbed(&scratch, std::env::var_os("PATH"), &BTreeMap::new()).unwrap();
        (root, scratch, environment)
    }

    fn report() -> PerfReport {
        let wall = Spread { median: 1500, min: 1200, max: 2100 };
        let header = Header {
            seed: 1, shape: "small".into(), shape_detail: "detail".into(), reps: 3, tools: BTreeMap::new(), platform: Platform::Linux,
            os: "Linux 6".into(), filesystem: "ext2/ext3/ext4".into(), configurations: BTreeMap::new(),
            hardware: Hardware { cpu: "a cpu".into(), logical_cores: 8, ram_bytes: Some(1 << 34) },
        };
        let cell = Cell { subject: Tool::Forklift, command: vec!["history".into()], result: Measured::Timed {
            wall, memory: Memory::Measured { peak_bytes: Spread { median: 3 << 20, min: 3 << 20, max: 4 << 20 } },
            cores: Cores::NotMeasured { reason: "too short to measure: under 20 ms of CPU time".into() },
        } };
        let rows = vec![Row { operation: "log".into(), note: "n".into(), outcome: Outcome::Timed { cells: vec![cell] } }];
        PerfReport { format: PERF_FORMAT, header, rows }
    }

    /// A perf.json reads back as written; one naming another format number, or none, is refused by
    /// that, before any field is read; one with a field this build does not know is refused too.
    #[test]
    fn a_perf_reader_refuses_a_format_it_does_not_know() {
        let report = report();
        assert_eq!(PerfReport::from_json(&report.json()).unwrap(), report);

        let mut value: serde_json::Value = serde_json::from_str(&report.json()).unwrap();
        value["format"] = serde_json::json!(2);
        assert_eq!(PerfReport::from_json(&value.to_string()).unwrap_err(), "perf report format 2 is not one this build reads (it reads 1)");
        value.as_object_mut().unwrap().remove("format");
        assert_eq!(PerfReport::from_json(&value.to_string()).unwrap_err(), "not a perf report: it names no format");

        let mut value: serde_json::Value = serde_json::from_str(&report.json()).unwrap();
        value["rows"][0]["outcome"]["cells"][0]["result"]["guessed"] = serde_json::json!(true);
        assert!(PerfReport::from_json(&value.to_string()).unwrap_err().starts_with("not a perf report of format 1: "));
    }

    /// A repetition that exits non-zero voids its cell, which carries the exit and the stderr and
    /// no timing; one that succeeds every time is timed.
    #[cfg(unix)]
    #[test]
    fn a_failing_repetition_voids_its_cell() {
        let (root, scratch, environment) = scratch("void");
        let shell = |script: &str| Invocation {
            program: PathBuf::from("/bin/sh"), arguments: vec!["-c".into(), script.into()], directory: scratch.work(), limit: Duration::from_secs(30),
        };
        assert_eq!(measure(&shell("echo boom >&2; exit 1"), 3, &scratch, &environment).unwrap(), Measured::Voided { exit: Some(1), signal: None, stderr: "boom\n".into() });
        assert!(matches!(measure(&shell("exit 0"), 3, &scratch, &environment).unwrap(), Measured::Timed { .. }));
        #[allow(clippy::disallowed_methods)] // a test's own temporary folder
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A command that dies of a signal voids its cell with that signal and no exit, and is not
    /// read as killed at its limit, even when the signal is SIGKILL; a run killed at its limit
    /// voids its cell as killed, the untimed run as well as a repetition.
    #[cfg(unix)]
    #[test]
    fn a_signal_is_not_a_timeout_and_a_warm_up_timeout_voids_its_cell() {
        let (root, scratch, environment) = scratch("signal");
        let shell = |script: &str, limit: Duration| Invocation {
            program: PathBuf::from("/bin/sh"), arguments: vec!["-c".into(), script.into()], directory: scratch.work(), limit,
        };
        let long = Duration::from_secs(30);
        assert_eq!(measure(&shell("kill -9 $$", long), 3, &scratch, &environment).unwrap(), Measured::Voided { exit: None, signal: Some(9), stderr: String::new() });
        assert_eq!(measure(&shell("kill -SEGV $$", long), 3, &scratch, &environment).unwrap(), Measured::Voided { exit: None, signal: Some(libc::SIGSEGV), stderr: String::new() });
        assert_eq!(measure(&shell("sleep 30", Duration::from_millis(300)), 3, &scratch, &environment).unwrap(), Measured::KilledAtLimit { limit_seconds: 0 });
        #[allow(clippy::disallowed_methods)] // a test's own temporary folder
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Porcelain lines are read by their two columns: the index's names a staged change, the work
    /// tree's an unstaged one, `??` an untracked file, and a rename counts at its new path.
    #[test]
    fn porcelain_columns_are_staged_and_unstaged() {
        let set = |paths: &[&str]| paths.iter().map(|path| path.to_string()).collect::<BTreeSet<String>>();
        let changes = porcelain(" M a/one\nM  b\n?? c\nR  old -> new\nMM d\n");
        assert_eq!(changes, Changes { staged: set(&["b", "new", "d"]), unstaged: set(&["a/one", "c", "d"]) });
    }

    /// The sameness check compares the paths each subject reports, not how many: a git subject with
    /// one file it should not have, even beside one it lacks, so that the count agrees, is told
    /// apart from what was expected, and a subject that sees exactly the expected set passes.
    #[cfg(unix)]
    #[test]
    fn sameness_compares_the_changed_paths_not_their_count() {
        let (root, scratch, environment) = scratch("same");
        let git = crate::scrub::path_git();
        let work = scratch.work().join("repo");
        let run = |arguments: &[&str]| {
            let invocation = Invocation { program: git.clone(), arguments: arguments.iter().map(|argument| argument.to_string()).collect(), directory: scratch.work(), limit: Duration::from_secs(30) };
            assert!(tool::run(&invocation, &scratch, &environment).unwrap().succeeded(), "{:?}", arguments);
        };
        let at = work.display().to_string();
        run(&["init", "-q", &at]);
        std::fs::write(work.join("a"), "a\n").unwrap();
        std::fs::write(work.join("b"), "b\n").unwrap();
        run(&["-C", &at, "add", "-A"]);
        run(&["-C", &at, "-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit", "-q", "-m", "one"]);
        let subjects = [Subject { tool: Tool::Git, program: git.clone(), prefix: vec![], work: work.clone() }];
        let set = |paths: &[&str]| paths.iter().map(|path| path.to_string()).collect::<BTreeSet<String>>();
        assert_eq!(same(&subjects, &Changes::default(), &scratch, &environment).unwrap(), Ok(()));

        std::fs::write(work.join("a"), "a\nedit\n").unwrap();
        std::fs::write(work.join("stray"), "s\n").unwrap();
        let expected = Changes { staged: BTreeSet::new(), unstaged: set(&["a", "b"]) };
        assert_eq!(same(&subjects, &expected, &scratch, &environment).unwrap(), Err("git saw 2 unstaged of 2 expected, 1 more (stray) and 1 missing (b)".to_string()));
        let expected = Changes { staged: BTreeSet::new(), unstaged: set(&["a", "stray"]) };
        assert_eq!(same(&subjects, &expected, &scratch, &environment).unwrap(), Ok(()));
        #[allow(clippy::disallowed_methods)] // a test's own temporary folder
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The median of an odd count is its middle value and of an even count the mean of the
    /// middle two, and the spread is the fastest and the slowest: never the mean of them all.
    #[test]
    fn a_spread_is_its_median_and_extremes() {
        assert_eq!(spread(vec![9, 1, 5, 100, 2]), Spread { median: 5, min: 1, max: 100 });
        assert_eq!(spread(vec![4, 1, 10, 2]), Spread { median: 3, min: 1, max: 10 });
    }

    /// A perf run over the small shape writes perf.json and perf.md, every read pair timed on all
    /// three subjects: the sameness checks pass on a generated history, and both subjects were
    /// built from one mirror (history.rs refuses an import whose current branch differs).
    ///
    /// Needs a forklift binary: `PROOF_LOAD_FORKLIFT=<path> cargo test -p proof-load -- --ignored`.
    #[test]
    #[ignore = "drives a forklift binary named by PROOF_LOAD_FORKLIFT"]
    fn a_perf_run_times_every_read_pair_on_every_subject() {
        let forklift = PathBuf::from(std::env::var_os("PROOF_LOAD_FORKLIFT").expect("PROOF_LOAD_FORKLIFT names the forklift binary under test"));
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let out = std::env::temp_dir().join(format!("proof-load-perf-run-{}-{}", std::process::id(), nanos));
        let options = PerfOptions { out: out.clone(), forklift, git: crate::scrub::path_git(), shape: "small".into(), reps: 1 };
        let report = run(&options, &|_| {}).unwrap();
        let operations: Vec<&str> = report.rows.iter().map(|row| row.operation.as_str()).collect();
        assert_eq!(operations, ["status", "log", "blame", "diff", "diff --staged"]);
        for row in &report.rows {
            let Outcome::Timed { cells } = &row.outcome else { panic!("{} was not timed: {:?}", row.operation, row.outcome) };
            let subjects: Vec<Tool> = cells.iter().map(|cell| cell.subject).collect();
            assert_eq!(subjects, [Tool::Forklift, Tool::Git, Tool::GitTuned], "{}", row.operation);
            for cell in cells {
                assert!(matches!(cell.result, Measured::Timed { .. }), "{} {:?}: {:?}", row.operation, cell.subject, cell.result);
            }
        }
        assert!(!report.failed());
        let written = std::fs::read_to_string(out.join("perf.json")).unwrap();
        assert_eq!(PerfReport::from_json(&written).unwrap(), report);
        assert!(std::fs::read_to_string(out.join("perf.md")).unwrap().contains("### diff --staged\n"));
        #[allow(clippy::disallowed_methods)] // a test's own temporary folder
        let _ = std::fs::remove_dir_all(&out);
    }
}
