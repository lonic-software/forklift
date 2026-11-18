//! `proof-load perf`: Forklift timed side by side with git, on one generated history, one command
//! at a time with nothing else from the harness running.
//!
//! A perf run is not a graded run: it writes its own document, perf.json and perf.md, under its
//! own format number, [`PERF_FORMAT`], and never touches `run`'s report. Each row pairs one
//! operation across three subjects (forklift, git as every repository git makes is configured,
//! and git tuned as a careful user runs it), each timed over `--reps` repetitions and reported as
//! median, minimum and maximum, once warm and once cold. Before a pair is timed the harness checks
//! that every subject sees the same change; a pair that fails the check is reported as not
//! comparable, never timed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::generate::{self, Generated, Shape, SIDE_BRANCH};
use crate::report::Platform;
use crate::runner::{self, RunError as BuildError};
use crate::scenario::{Tool, GIT_TUNED};
use crate::scrub::{Environment, Output, Scratch};
use crate::tool::{self, Invocation, ToolError};

/// The perf document's format. A reader refuses any other number rather than guess at it.
pub const PERF_FORMAT: u32 = 2;

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
/// The message of the commit pair's commits.
const COMMIT_MESSAGE: &str = "perf: a timed commit";
/// Why a cold row was not run when no cache-drop program was given.
const NO_DROP: &str = "no cache-drop program given (--drop-caches)";
/// Why a cold row was not run on Windows.
const NO_COLD_ON_WINDOWS: &str = "cold runs are not built on Windows in this harness";

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
    /// The cache-drop program each cold repetition ran first, by path; none when the cold rows
    /// were not run.
    pub drop_caches: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hardware {
    pub cpu: String,
    pub logical_cores: usize,
    /// Physical memory in bytes, where the platform's is read.
    pub ram_bytes: Option<u64>,
}

/// One operation on a warm or a cold cache, timed on every subject or not, with why not.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub operation: String,
    pub cache: Cache,
    pub note: String,
    pub outcome: Outcome,
}

/// What a row's repetitions start from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cache {
    /// The page cache as the untimed run before the repetitions left it.
    Warm,
    /// The page cache dropped, by the caller's cache-drop program, just before each repetition.
    Cold,
}

impl Cache {
    pub fn label(self) -> &'static str {
        match self {
            Cache::Warm => "warm",
            Cache::Cold => "cold",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    /// Every subject ran, each in its own cell.
    Timed { cells: Vec<Cell> },
    /// The subjects did not see the same change before the pair, or a write left them in different
    /// states, so their timings would compare different work.
    NotComparable { reason: String },
    /// The row was not run, for the reason given: no subject ran its command.
    NotRun { reason: String },
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
    /// A run, the untimed one, a repetition or the cache drop before it, ran past its limit and
    /// was killed; what it said is not kept.
    KilledAtLimit { limit_ms: u64 },
    /// The cache-drop program did not succeed before a cold repetition, so the repetition would
    /// not have been cold and was not run: its exit, or the signal it died of when it had no exit,
    /// and what it said on stderr.
    DropFailed { exit: Option<i32>, signal: Option<i32>, stderr: String },
    /// The cache-drop program ran past its limit before a cold repetition and was killed, so the
    /// repetition was not run.
    DropKilledAtLimit { limit_ms: u64 },
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
    /// with no timing. A row not run for its stated reason is not a failure.
    pub fn failed(&self) -> bool {
        self.rows.iter().any(|row| match &row.outcome {
            Outcome::NotComparable { .. } => true,
            Outcome::NotRun { .. } => false,
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
        match &header.drop_caches {
            Some(program) => { let _ = writeln!(out, "- cold rows: the caches dropped by `{}` before each repetition", program); }
            None => out.push_str("- cold rows: not run\n"),
        }
        out.push_str("\n## Rows\n");
        for row in &self.rows {
            let _ = writeln!(out, "\n### {}, {}\n\n{}\n", row.operation, row.cache.label(), row.note);
            match &row.outcome {
                Outcome::NotComparable { reason } => { let _ = writeln!(out, "Not comparable: {}", reason); }
                Outcome::NotRun { reason } => { let _ = writeln!(out, "Not run: {}", reason); }
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
                            Measured::KilledAtLimit { limit_ms } => {
                                let _ = writeln!(out, "| {} | voided: killed at its {} limit | | | | | {} |", cell.subject.label(), micros(limit_ms.saturating_mul(1000)), command);
                            }
                            Measured::DropFailed { exit, signal, stderr } => {
                                let ended = match (exit, signal) {
                                    (Some(exit), _) => format!("exited {}", exit),
                                    (None, Some(signal)) => format!("died of signal {}", signal),
                                    (None, None) => "ended with no exit".to_string(),
                                };
                                let _ = writeln!(out, "| {} | voided: the cache-drop program {}: {} | | | | | {} |", cell.subject.label(), ended, stderr.trim().replace('\n', " ").replace('|', "\\|"), command);
                            }
                            Measured::DropKilledAtLimit { limit_ms } => {
                                let _ = writeln!(out, "| {} | voided: the cache-drop program was killed at its {} limit | | | | | {} |", cell.subject.label(), micros(limit_ms.saturating_mul(1000)), command);
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
    /// The program each cold repetition runs first to drop the operating system's caches; without
    /// it the cold rows are not run.
    pub drop_caches: Option<PathBuf>,
}

/// One subject a pair runs on: its tool, the binary, the arguments every one of its command lines
/// starts with, its work tree, and whether an fsmonitor daemon watches it.
struct Subject {
    tool: Tool,
    program: PathBuf,
    prefix: Vec<String>,
    work: PathBuf,
    watched: bool,
}

impl Subject {
    fn line(&self, arguments: &[&str]) -> Vec<String> {
        self.prefix.iter().cloned().chain(arguments.iter().map(|argument| argument.to_string())).collect()
    }

    fn invocation(&self, arguments: Vec<String>) -> Invocation {
        Invocation { program: self.program.clone(), arguments, directory: self.work.clone(), limit: COMMAND_LIMIT }
    }

    /// This subject's tool over another work tree: a copy of this one's.
    fn at(&self, work: PathBuf) -> Subject {
        Subject { tool: self.tool, program: self.program.clone(), prefix: self.prefix.clone(), work, watched: self.watched }
    }
}

/// The cache-drop program each cold repetition runs first, with no arguments, and how long it may
/// take.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DropCaches {
    program: PathBuf,
    limit: Duration,
}

/// How the cold rows run: the cache drop each repetition runs first, or why they are not run at
/// all.
type Cold = Result<DropCaches, String>;

/// The cold rows' plan on a platform that is Windows or not, given the cache-drop program the
/// caller named, if any.
fn cold_when(windows: bool, drop_caches: Option<PathBuf>) -> Cold {
    match drop_caches {
        _ if windows => Err(NO_COLD_ON_WINDOWS.to_string()),
        Some(program) => Ok(DropCaches { program, limit: COMMAND_LIMIT }),
        None => Err(NO_DROP.to_string()),
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
    let drop_caches = options.drop_caches.as_deref().map(|program| absolute("--drop-caches", program)).transpose()?;
    let cold = cold_when(cfg!(windows), drop_caches);
    let mut output = Output::claim(&options.out).map_err(|error| error.to_string())?;
    let scratch = output.scratch("perf").map_err(|error| error.to_string())?;
    let search_path = crate::scrub::search_path(Some(&git), std::env::var_os("PATH"));
    let environment = Environment::scrubbed(&scratch, search_path, &BTreeMap::new()).map_err(|error| error.to_string())?;

    let built = build(&forklift, &git, &shape, &scratch, &environment, progress).map_err(|error| format!("building the subjects failed: {}", error))?;
    let daemon = built.daemon.is_some();
    let rows = pairs(&built, options.reps, &cold, &scratch, &environment, progress).map_err(|error| error.to_string())?;
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
        configurations, hardware: hardware(), drop_caches: cold.as_ref().ok().map(|drop| drop.program.display().to_string()),
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
    // Enrolled after its import, which refuses to run once trust exists, so every stack the
    // commit pair times is signed, as a Forklift user's is.
    let enroll = Invocation { program: forklift.to_path_buf(), arguments: vec!["--json".into(), "office".into(), "enroll".into()], directory: work.join("forklift"), limit: BUILD_LIMIT };
    let enrolled = tool::run(&enroll, scratch, environment).map_err(BuildError::Tool)?;
    if !enrolled.succeeded() {
        return Err(BuildError::BuildFailed {
            arguments: enrolled.arguments, exit: enrolled.exit,
            stderr: String::from_utf8_lossy(&enrolled.stderr).into_owned(), stdout: String::from_utf8_lossy(&enrolled.stdout).into_owned(),
        });
    }
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
        Subject { tool: Tool::Forklift, program: forklift.to_path_buf(), prefix: vec![], work: work.join("forklift"), watched: false },
        Subject { tool: Tool::Git, program: git.to_path_buf(), prefix: vec![], work: work.join("git"), watched: false },
        Subject {
            tool: Tool::GitTuned, program: git.to_path_buf(), prefix: GIT_TUNED.iter().map(|option| option.to_string()).collect(), work: tuned,
            watched: daemon.is_some(),
        },
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

/// Every pair, each as a warm row and then a cold one: status, log and blame on the clean tree; the
/// write pairs, commit, switch and merge, each repetition on a fresh copy of the clean subject;
/// then diff and diff --staged over one edit set.
fn pairs(built: &Built, reps: usize, cold: &Cold, scratch: &Scratch, environment: &Environment, progress: &dyn Fn(&str)) -> Result<Vec<Row>, Stopped> {
    let subjects = &built.subjects;
    let blame = built.generated.blame_file.as_str();
    let edited: Vec<&str> = built.generated.text_files.iter().take(EDITED).map(String::as_str).collect();
    let mut rows = Vec::new();

    // One untimed read after the build, so the first timing pays for no cache the build left cold.
    for subject in subjects {
        read_untimed(subject, scratch, environment)?;
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
        let check = clean.clone().map_err(|reason| format!("before the clean-tree rows, {}", reason));
        both(&mut rows, operation, &note, check, cold, |cold| timed_pair(subjects, forklift, git, reps, cold, scratch, environment))?;
    }

    writes(subjects, &edited, reps, cold, scratch, environment, progress, &mut rows)?;

    for subject in subjects {
        edit(&subject.work, &edited)?;
        // The untimed read after the edit, as after the build.
        read_untimed(subject, scratch, environment)?;
    }
    let count = edited.len();
    let note = format!("{} tracked files edited, unstaged", count);
    progress("timing diff");
    let unstaged = Changes { staged: BTreeSet::new(), unstaged: edited_set.clone() };
    let check = same(subjects, &unstaged, scratch, environment)?.map_err(|reason| format!("before diff, {}", reason));
    both(&mut rows, "diff", &note, check, cold, |cold| timed_pair(subjects, &["diff"], &["diff"], reps, cold, scratch, environment))?;

    for subject in subjects {
        stage(subject, scratch, environment)?;
    }
    let note = format!("the same {} files staged, untimed, by Forklift's load . and git's add -A", count);
    progress("timing diff --staged");
    let staged = Changes { staged: edited_set, unstaged: BTreeSet::new() };
    let check = same(subjects, &staged, scratch, environment)?.map_err(|reason| format!("before diff --staged, {}", reason));
    both(&mut rows, "diff --staged", &note, check, cold, |cold| timed_pair(subjects, &["diff", "--staged"], &["diff", "--staged"], reps, cold, scratch, environment))?;
    Ok(rows)
}

/// Pushes `operation`'s warm row and then its cold one: both not comparable when `check` failed,
/// the cold one not run when the plan says so, and otherwise each as `time` measures it, given the
/// cache-drop program for the cold row.
fn both(rows: &mut Vec<Row>, operation: &str, note: &str, check: Result<(), String>, cold: &Cold, mut time: impl FnMut(Option<&DropCaches>) -> Result<Outcome, Stopped>) -> Result<(), Stopped> {
    for cache in [Cache::Warm, Cache::Cold] {
        let outcome = match (&check, cache, cold) {
            (Err(reason), _, _) => Outcome::NotComparable { reason: reason.clone() },
            (Ok(()), Cache::Warm, _) => time(None)?,
            (Ok(()), Cache::Cold, Ok(drop)) => time(Some(drop))?,
            (Ok(()), Cache::Cold, Err(reason)) => Outcome::NotRun { reason: reason.clone() },
        };
        rows.push(Row { operation: operation.to_string(), cache, note: note.to_string(), outcome });
    }
    Ok(())
}

/// The run that warms a subject's caches after its work tree was built or edited, untimed
/// and unjudged: Forklift's stocktake --summary or git's status.
fn read_untimed(subject: &Subject, scratch: &Scratch, environment: &Environment) -> Result<(), Stopped> {
    let read = if subject.tool == Tool::Forklift { subject.line(&["stocktake", "--summary"]) } else { subject.line(&["status"]) };
    tool::run(&subject.invocation(read), scratch, environment)?;
    Ok(())
}

/// Appends the benchmark edit to each of `files` in the work tree at `work`.
fn edit(work: &Path, files: &[&str]) -> Result<(), Stopped> {
    for file in files {
        let path = work.join(file);
        let mut text = std::fs::read(&path).map_err(|error| Stopped(format!("{}: {}", path.display(), error)))?;
        text.extend_from_slice(EDIT.as_bytes());
        std::fs::write(&path, text).map_err(|error| Stopped(format!("{}: {}", path.display(), error)))?;
    }
    Ok(())
}

/// Stages every change in `subject`, untimed: Forklift's `load .`, git's `add -A`. Whether it
/// staged what was meant is the sameness check's to judge.
fn stage(subject: &Subject, scratch: &Scratch, environment: &Environment) -> Result<(), Stopped> {
    let stage = if subject.tool == Tool::Forklift { subject.line(&["load", "."]) } else { subject.line(&["add", "-A"]) };
    tool::run(&subject.invocation(stage), scratch, environment)?;
    Ok(())
}

/// A write pair: one that changes its subject.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Write {
    Commit,
    Switch,
    Merge,
}

/// A write pair's timed lines, Forklift's and then git's, before a subject's own prefix, given its
/// message or branch: the lines scenario::bind runs for its record, switch and merge, less the
/// `--json` it gives Forklift's. A test holds the two tables to each other.
fn write_lines(write: Write, argument: &str) -> [Vec<&str>; 2] {
    match write {
        Write::Commit => [vec!["stack", "-m", argument], vec!["commit", "-q", "-m", argument]],
        Write::Switch => [vec!["shift", argument], vec!["switch", "-q", argument]],
        Write::Merge => [vec!["consolidate", argument], vec!["merge", "-q", "--no-edit", argument]],
    }
}

/// The write pairs, warm and cold: commit over the edit set, staged untimed, then switch to
/// [`SIDE_BRANCH`] and merge it. Switch is not comparable unless the side branch is at another
/// commit than the current branch in every subject, and merge unless neither of the two contains
/// the other, so that it joins two lines of history. Each repetition runs on a fresh copy of the
/// clean subject, which every write must leave clean; the subjects themselves are not changed.
#[allow(clippy::too_many_arguments)]
fn writes(subjects: &[Subject], edited: &[&str], reps: usize, cold: &Cold, scratch: &Scratch, environment: &Environment, progress: &dyn Fn(&str), rows: &mut Vec<Row>) -> Result<(), Stopped> {
    let copies = scratch.work().join("copies");
    std::fs::create_dir_all(&copies).map_err(|error| Stopped(format!("{}: {}", copies.display(), error)))?;
    let staged = Changes { staged: edited.iter().map(|file| file.to_string()).collect(), unstaged: BTreeSet::new() };
    let clean = Changes::default();

    progress("timing commit");
    let note = format!(
        "the same {} tracked files edited and staged, untimed, on a fresh copy for each repetition; Forklift's subject is enrolled, so its stack is signed, and git's commit is not",
        edited.len(),
    );
    let fresh = Fresh { operation: "commit", edit: edited, before: &staged, copies: &copies };
    let [forklift, git] = write_lines(Write::Commit, COMMIT_MESSAGE);
    both(rows, "commit", &note, Ok(()), cold, |cold| fresh_pair(subjects, &forklift, &git, reps, cold, &fresh, scratch, environment))?;

    let (switch, merge) = sides(subjects, scratch, environment)?;
    for (write, operation, note, check) in [
        (Write::Switch, "switch", format!("from the current branch to {}, which leaves it a few commits before its tip, on a fresh copy for each repetition", SIDE_BRANCH), switch),
        (Write::Merge, "merge", format!("{} into the current branch, each with commits the other lacks, a merge with no conflict, on a fresh copy for each repetition", SIDE_BRANCH), merge),
    ] {
        progress(&format!("timing {}", operation));
        let fresh = Fresh { operation, edit: &[], before: &clean, copies: &copies };
        let [forklift, git] = write_lines(write, SIDE_BRANCH);
        both(rows, operation, &note, check, cold, |cold| fresh_pair(subjects, &forklift, &git, reps, cold, &fresh, scratch, environment))?;
    }
    Ok(())
}

/// Where [`SIDE_BRANCH`] stands against the current branch in one subject.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    /// At the commit the current branch is at.
    Same,
    /// On one line of history with it: one of the two contains the other.
    OneLine,
    /// Each has commits the other lacks.
    Forked,
}

/// Whether a pair can be timed, or why not.
type Comparable = Result<(), String>;

/// Whether switching to [`SIDE_BRANCH`] changes anything in every subject, and whether merging it
/// joins two lines of history in every subject, as each tool reads its own history, or why not.
fn sides(subjects: &[Subject], scratch: &Scratch, environment: &Environment) -> Result<(Comparable, Comparable), Stopped> {
    let (mut switch, mut merge) = (Vec::new(), Vec::new());
    for subject in subjects {
        let label = subject.tool.label();
        match side(subject, scratch, environment)? {
            Err(reason) => {
                switch.push(reason.clone());
                merge.push(reason);
            }
            Ok(Side::Same) => {
                let reason = format!("{} has {} at the commit its current branch is at, so switching to it or merging it would time a change of nothing", label, SIDE_BRANCH);
                switch.push(reason.clone());
                merge.push(reason);
            }
            Ok(Side::OneLine) => merge.push(format!("{} has {} and its current branch on one line of history, one containing the other, so merging it would not join two lines", label, SIDE_BRANCH)),
            Ok(Side::Forked) => {}
        }
    }
    let verdict = |not: Vec<String>| if not.is_empty() { Ok(()) } else { Err(not.join("; ")) };
    Ok((verdict(switch), verdict(merge)))
}

/// Where [`SIDE_BRANCH`] stands in `subject`: git's by `rev-list --left-right --count`, Forklift's
/// by its pallets' heads and the history of each. Gives why when a read failed.
fn side(subject: &Subject, scratch: &Scratch, environment: &Environment) -> Result<Result<Side, String>, Stopped> {
    let label = subject.tool.label();
    if subject.tool != Tool::Forklift {
        let range = format!("HEAD...{}", SIDE_BRANCH);
        let text = match output(subject, &["rev-list", "--left-right", "--count", &range], scratch, environment)? {
            Ok(text) => text,
            Err(reason) => return Ok(Err(reason)),
        };
        let counts: Vec<u64> = text.split_whitespace().map_while(|count| count.parse().ok()).collect();
        return Ok(match counts[..] {
            [0, 0] => Ok(Side::Same),
            [ahead, behind] if ahead == 0 || behind == 0 => Ok(Side::OneLine),
            [_, _] => Ok(Side::Forked),
            _ => Err(format!("{} rev-list gave no two counts: {}", label, text.trim())),
        });
    }
    let text = match output(subject, &["--json", "pallet", "list"], scratch, environment)? {
        Ok(text) => text,
        Err(reason) => return Ok(Err(reason)),
    };
    let document = runner::document(text.as_bytes());
    let pallets = document["data"]["pallets"].as_array().cloned().unwrap_or_default();
    let head = |found: &dyn Fn(&serde_json::Value) -> bool| pallets.iter().find(|pallet| found(pallet)).and_then(|pallet| pallet["head"].as_str()).map(str::to_string);
    let Some((current, side)) = head(&|pallet| pallet["current"] == true).zip(head(&|pallet| pallet["name"] == SIDE_BRANCH)) else {
        return Ok(Err(format!("{} named no head for the current branch or for {}", label, SIDE_BRANCH)));
    };
    if current == side {
        return Ok(Ok(Side::Same));
    }
    let behind = match parcels(subject, &side, scratch, environment)? {
        Ok(parcels) => parcels.contains(&current),
        Err(reason) => return Ok(Err(reason)),
    };
    let ahead = match parcels(subject, &current, scratch, environment)? {
        Ok(parcels) => parcels.contains(&side),
        Err(reason) => return Ok(Err(reason)),
    };
    Ok(Ok(if behind || ahead { Side::OneLine } else { Side::Forked }))
}

/// Every parcel in Forklift's history of `revision`, itself included, read page by page.
fn parcels(subject: &Subject, revision: &str, scratch: &Scratch, environment: &Environment) -> Result<Result<BTreeSet<String>, String>, Stopped> {
    let mut parcels = BTreeSet::new();
    let mut after: Option<String> = None;
    loop {
        let mut arguments = vec!["--json".to_string(), "history".to_string(), revision.to_string()];
        if let Some(cursor) = after.take() {
            arguments.extend(["--after".to_string(), cursor]);
        }
        let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let text = match output(subject, &arguments, scratch, environment)? {
            Ok(text) => text,
            Err(reason) => return Ok(Err(reason)),
        };
        let document = runner::document(text.as_bytes());
        let Some(entries) = document["data"]["entries"].as_array() else {
            return Ok(Err(format!("Forklift's --json history {} gave no entries", revision)));
        };
        for entry in entries {
            let Some(parcel) = entry["parcel"].as_str() else {
                return Ok(Err(format!("Forklift's --json history {} gave an entry with no parcel", revision)));
            };
            parcels.insert(parcel.to_string());
        }
        match document["data"]["next"].as_str() {
            Some(next) => after = Some(next.to_string()),
            None => return Ok(Ok(parcels)),
        }
    }
}

/// What `subject` printed on stdout for `arguments`, after its prefix, or why it did not succeed.
fn output(subject: &Subject, arguments: &[&str], scratch: &Scratch, environment: &Environment) -> Result<Result<String, String>, Stopped> {
    let outcome = tool::run(&subject.invocation(subject.line(arguments)), scratch, environment)?;
    if !outcome.succeeded() {
        return Ok(Err(format!("{} {} exited {:?}: {}", subject.tool.label(), arguments.join(" "), outcome.exit, String::from_utf8_lossy(&outcome.stderr).trim())));
    }
    Ok(Ok(String::from_utf8_lossy(&outcome.stdout).into_owned()))
}

/// What each repetition of a write pair starts from: a fresh copy of the subject under `copies`,
/// named for `operation`, refreshed once untimed, then `edit`'s files edited and staged, untimed,
/// after which the copy must report exactly `before` changed.
struct Fresh<'a> {
    operation: &'a str,
    edit: &'a [&'a str],
    before: &'a Changes,
    copies: &'a Path,
}

/// Times a write pair: forklift's `forklift` line and both gits' `git` line, every repetition on a
/// fresh copy. Not comparable when a copy did not reach the state the pair starts from, or a write
/// left one not clean.
#[allow(clippy::too_many_arguments)]
fn fresh_pair(subjects: &[Subject], forklift: &[&str], git: &[&str], reps: usize, cold: Option<&DropCaches>, fresh: &Fresh, scratch: &Scratch, environment: &Environment) -> Result<Outcome, Stopped> {
    let mut cells = Vec::new();
    for subject in subjects {
        let command = subject.line(if subject.tool == Tool::Forklift { forklift } else { git });
        match fresh_cell(subject, &command, reps, cold, fresh, scratch, environment)? {
            Ok(result) => cells.push(Cell { subject: subject.tool, command, result }),
            Err(reason) => return Ok(Outcome::NotComparable { reason }),
        }
    }
    Ok(Outcome::Timed { cells })
}

/// A folder the harness copied a subject into, removed when dropped. A copy that cannot be removed
/// is left where it is: every copy has a name of its own, so a leftover is in no later one's way.
struct Copied(PathBuf);

impl Drop for Copied {
    fn drop(&mut self) {
        #[allow(clippy::disallowed_methods, reason = "a scratch copy the harness made itself, under its own output directory")]
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The untimed run that brings a fresh copy's caches up to date with its copied files, as git's
/// status refreshes its index: Forklift's `load .`, since its stocktake keeps nothing it learns
/// about a file, and git's `status`.
fn refresh_untimed(copy: &Subject, scratch: &Scratch, environment: &Environment) -> Result<(), Stopped> {
    let refresh = if copy.tool == Tool::Forklift { copy.line(&["load", "."]) } else { copy.line(&["status"]) };
    tool::run(&copy.invocation(refresh), scratch, environment)?;
    Ok(())
}

/// Writes every dirty page out, untimed, so that a timed write which syncs pays for its own writes
/// and not for the copy's. Only on unix, which has sync(2); on Windows it does nothing.
fn settle() {
    #[cfg(unix)]
    // SAFETY: sync(2) takes no arguments and has no failure to report.
    unsafe {
        libc::sync();
    }
}

/// One write cell: each repetition on a fresh copy of `subject` as `fresh` describes it, watched
/// by a daemon of its own where the subject is, and judged clean after the timed run. A warm cell
/// first runs the command once untimed and unjudged, on a copy of its own, as a warm read cell
/// does. Gives why the pair is not comparable when a copy's check failed.
fn fresh_cell(subject: &Subject, command: &[String], reps: usize, cold: Option<&DropCaches>, fresh: &Fresh, scratch: &Scratch, environment: &Environment) -> Result<Result<Measured, String>, Stopped> {
    let cache = if cold.is_some() { Cache::Cold } else { Cache::Warm };
    let first = if cold.is_some() { 1 } else { 0 };
    let mut runs = Vec::with_capacity(reps);
    for rep in first..=reps {
        let name = format!("{}-{}-{}-{}", fresh.operation.replace(' ', "-"), cache.label(), subject.tool.label(), rep);
        let copied = Copied(fresh.copies.join(name));
        crate::scrub::copy_tree(&subject.work, &copied.0).map_err(|error| Stopped(format!("copying {} to {}: {:?}", subject.work.display(), copied.0.display(), error)))?;
        let copy = subject.at(copied.0.clone());
        // Dropped before the copy is removed. The copy's fsmonitor setting came with it.
        let _daemon = if copy.watched {
            Some(runner::FsmonitorDaemon::watch(&copy.program, &copy.work, scratch, environment, COMMAND_LIMIT).map_err(|error| Stopped(error.to_string()))?.0)
        } else {
            None
        };
        refresh_untimed(&copy, scratch, environment)?;
        if !fresh.edit.is_empty() {
            edit(&copy.work, fresh.edit)?;
            stage(&copy, scratch, environment)?;
        }
        let invocation = copy.invocation(command.to_vec());
        if rep == 0 {
            settle();
            match tool::run(&invocation, scratch, environment) {
                Ok(_) => continue,
                Err(ToolError::TimedOut { .. }) => return Ok(Ok(killed(invocation.limit))),
                Err(error) => return Err(error.into()),
            }
        }
        if let Err(reason) = same(std::slice::from_ref(&copy), fresh.before, scratch, environment)? {
            return Ok(Err(format!("before repetition {}, {}", rep, reason)));
        }
        settle();
        match repetition(&invocation, cold, scratch, environment)? {
            Ok(timed) => runs.push(timed),
            Err(ended) => return Ok(Ok(ended)),
        }
        if let Err(reason) = same(std::slice::from_ref(&copy), &Changes::default(), scratch, environment)? {
            return Ok(Err(format!("after repetition {}, {}", rep, reason)));
        }
    }
    Ok(Ok(summarise(&runs)))
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
    let arguments: &[&str] = if subject.tool == Tool::Forklift { &["--json", "stocktake"] } else { &["status", "--porcelain", "-z", "--untracked-files=all"] };
    let outcome = tool::run(&subject.invocation(subject.line(arguments)), scratch, environment)?;
    if !outcome.succeeded() {
        return Ok(Err(format!("{} {} exited {:?}: {}", subject.tool.label(), arguments.join(" "), outcome.exit, String::from_utf8_lossy(&outcome.stderr).trim())));
    }
    if subject.tool == Tool::Forklift {
        return Ok(stocktake_changes(&runner::document(&outcome.stdout)).ok_or_else(|| "Forklift's --json stocktake gave no staged and unstaged paths that agree with its counts".to_string()));
    }
    Ok(Ok(porcelain(&String::from_utf8_lossy(&outcome.stdout))))
}

/// The changes in a `--json stocktake` document, or None when it holds none that can be trusted. A
/// list is left out when it is empty, so each is read only beside its count, and must hold as many
/// distinct paths as the count says.
fn stocktake_changes(document: &serde_json::Value) -> Option<Changes> {
    let paths = |field: &str| -> Option<BTreeSet<String>> {
        let count = document["data"][format!("{}_count", field)].as_u64()?;
        let list = match &document["data"][field] {
            serde_json::Value::Null => Vec::new(),
            list => list.as_array()?.iter().map(|change| change["path"].as_str().map(str::to_string)).collect::<Option<Vec<String>>>()?,
        };
        let paths: BTreeSet<String> = list.into_iter().collect();
        (u64::try_from(paths.len()).ok() == Some(count)).then_some(paths)
    };
    Some(Changes { staged: paths("staged")?, unstaged: paths("unstaged")? })
}

/// The changes in `git status --porcelain -z` entries `XY path`, each ended by a NUL, the path
/// unquoted: X, the index's column, names a staged change; Y, the work tree's, an unstaged one, and
/// `??` an untracked file. A rename or copy names its new path, then its old one as an entry of its
/// own, which is skipped.
fn porcelain(text: &str) -> Changes {
    let mut changes = Changes::default();
    let mut entries = text.split('\0').filter(|entry| !entry.is_empty());
    while let Some(entry) = entries.next() {
        let Some((columns, path)) = entry.split_at_checked(3) else { continue };
        let path = path.to_string();
        if columns.starts_with(['R', 'C']) {
            entries.next();
        }
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

/// Times forklift's `forklift` line and both gits' `git` line, cold when given the cache-drop
/// program.
fn timed_pair(subjects: &[Subject], forklift: &[&str], git: &[&str], reps: usize, cold: Option<&DropCaches>, scratch: &Scratch, environment: &Environment) -> Result<Outcome, Stopped> {
    let mut cells = Vec::new();
    for subject in subjects {
        let command = subject.line(if subject.tool == Tool::Forklift { forklift } else { git });
        let result = measure(&subject.invocation(command.clone()), reps, cold, scratch, environment)?;
        cells.push(Cell { subject: subject.tool, command, result });
    }
    Ok(Outcome::Timed { cells })
}

/// One cell: `reps` timed repetitions. A warm cell has an untimed run first, whose exit is not
/// judged, so the cache is warm; a cold cell drops the cache before each repetition instead. The
/// first repetition that does not end in a timing ends the cell, and so does an untimed run
/// killed at its limit.
fn measure(invocation: &Invocation, reps: usize, cold: Option<&DropCaches>, scratch: &Scratch, environment: &Environment) -> Result<Measured, Stopped> {
    if cold.is_none() {
        match tool::run(invocation, scratch, environment) {
            Ok(_) => {}
            Err(ToolError::TimedOut { .. }) => return Ok(killed(invocation.limit)),
            Err(error) => return Err(error.into()),
        }
    }
    let mut runs = Vec::with_capacity(reps);
    for _ in 0..reps {
        match repetition(invocation, cold, scratch, environment)? {
            Ok(timed) => runs.push(timed),
            Err(ended) => return Ok(ended),
        }
    }
    Ok(summarise(&runs))
}

fn killed(limit: Duration) -> Measured {
    Measured::KilledAtLimit { limit_ms: millis(limit) }
}

fn millis(limit: Duration) -> u64 {
    u64::try_from(limit.as_millis()).unwrap_or(u64::MAX)
}

/// One timed repetition, after the cache-drop program when it is cold: its timing when it
/// succeeded, or what its cell is instead, voided by the command's exit or signal, by the drop's
/// failure, or killed at a limit.
fn repetition(invocation: &Invocation, cold: Option<&DropCaches>, scratch: &Scratch, environment: &Environment) -> Result<Result<tool::Timed, Measured>, Stopped> {
    if let Some(caches) = cold {
        let drop = Invocation { program: caches.program.clone(), arguments: vec![], directory: invocation.directory.clone(), limit: caches.limit };
        match tool::timed(&drop, scratch, environment) {
            Ok(dropped) if dropped.outcome.succeeded() => {}
            Ok(dropped) => return Ok(Err(Measured::DropFailed { exit: dropped.outcome.exit, signal: dropped.signal, stderr: String::from_utf8_lossy(&dropped.outcome.stderr).into_owned() })),
            Err(ToolError::TimedOut { .. }) => return Ok(Err(Measured::DropKilledAtLimit { limit_ms: millis(caches.limit) })),
            Err(error) => return Err(error.into()),
        }
    }
    let timed = match tool::timed(invocation, scratch, environment) {
        Ok(timed) => timed,
        Err(ToolError::TimedOut { .. }) => return Ok(Err(killed(invocation.limit))),
        Err(error) => return Err(error.into()),
    };
    if !timed.outcome.succeeded() {
        return Ok(Err(Measured::Voided { exit: timed.outcome.exit, signal: timed.signal, stderr: String::from_utf8_lossy(&timed.outcome.stderr).into_owned() }));
    }
    Ok(Ok(timed))
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
    use crate::scenario::{bind, record_message, Op};

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
            hardware: Hardware { cpu: "a cpu".into(), logical_cores: 8, ram_bytes: Some(1 << 34) }, drop_caches: Some("/usr/local/bin/purge-caches".into()),
        };
        let cell = Cell { subject: Tool::Forklift, command: vec!["history".into()], result: Measured::Timed {
            wall, memory: Memory::Measured { peak_bytes: Spread { median: 3 << 20, min: 3 << 20, max: 4 << 20 } },
            cores: Cores::NotMeasured { reason: "too short to measure: under 20 ms of CPU time".into() },
        } };
        let rows = vec![
            Row { operation: "log".into(), cache: Cache::Warm, note: "n".into(), outcome: Outcome::Timed { cells: vec![cell] } },
            Row { operation: "log".into(), cache: Cache::Cold, note: "n".into(), outcome: Outcome::NotRun { reason: "r".into() } },
        ];
        PerfReport { format: PERF_FORMAT, header, rows }
    }

    /// A perf.json reads back as written; one naming another format number, or none, is refused by
    /// that, before any field is read; one with a field this build does not know is refused too.
    #[test]
    fn a_perf_reader_refuses_a_format_it_does_not_know() {
        let report = report();
        assert_eq!(PerfReport::from_json(&report.json()).unwrap(), report);

        let mut value: serde_json::Value = serde_json::from_str(&report.json()).unwrap();
        value["format"] = serde_json::json!(1);
        assert_eq!(PerfReport::from_json(&value.to_string()).unwrap_err(), "perf report format 1 is not one this build reads (it reads 2)");
        value.as_object_mut().unwrap().remove("format");
        assert_eq!(PerfReport::from_json(&value.to_string()).unwrap_err(), "not a perf report: it names no format");

        let mut value: serde_json::Value = serde_json::from_str(&report.json()).unwrap();
        value["rows"][0]["outcome"]["cells"][0]["result"]["guessed"] = serde_json::json!(true);
        assert!(PerfReport::from_json(&value.to_string()).unwrap_err().starts_with("not a perf report of format 2: "));
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
        assert_eq!(measure(&shell("echo boom >&2; exit 1"), 3, None, &scratch, &environment).unwrap(), Measured::Voided { exit: Some(1), signal: None, stderr: "boom\n".into() });
        assert!(matches!(measure(&shell("exit 0"), 3, None, &scratch, &environment).unwrap(), Measured::Timed { .. }));
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
        assert_eq!(measure(&shell("kill -9 $$", long), 3, None, &scratch, &environment).unwrap(), Measured::Voided { exit: None, signal: Some(9), stderr: String::new() });
        assert_eq!(measure(&shell("kill -SEGV $$", long), 3, None, &scratch, &environment).unwrap(), Measured::Voided { exit: None, signal: Some(libc::SIGSEGV), stderr: String::new() });
        assert_eq!(measure(&shell("sleep 30", Duration::from_millis(300)), 3, None, &scratch, &environment).unwrap(), Measured::KilledAtLimit { limit_ms: 300 });
        #[allow(clippy::disallowed_methods)] // a test's own temporary folder
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Porcelain lines are read by their two columns: the index's names a staged change, the work
    /// tree's an unstaged one, `??` an untracked file, and a rename counts at its new path; a path
    /// with a space, an arrow or a non-ASCII letter is read as it is, unquoted.
    #[test]
    fn porcelain_columns_are_staged_and_unstaged() {
        let set = |paths: &[&str]| paths.iter().map(|path| path.to_string()).collect::<BTreeSet<String>>();
        let changes = porcelain(" M a/one\0M  b\0?? c d\0R  new\0old\0MM d -> e\0?? ü\0");
        assert_eq!(changes, Changes { staged: set(&["b", "new", "d -> e"]), unstaged: set(&["a/one", "c d", "d -> e", "ü"]) });
    }

    /// A stocktake document's paths are read beside their counts: a list left out reads as empty
    /// only when its count is 0, and a list that holds fewer paths than its count, or one whose
    /// entry names no path, is not trusted.
    #[test]
    fn stocktake_paths_are_read_beside_their_counts() {
        let set = |paths: &[&str]| paths.iter().map(|path| path.to_string()).collect::<BTreeSet<String>>();
        let read = |data: serde_json::Value| stocktake_changes(&serde_json::json!({ "ok": true, "data": data }));
        assert_eq!(read(serde_json::json!({ "staged_count": 0, "unstaged_count": 1, "unstaged": [{ "kind": "untracked", "path": "a" }] })), Some(Changes { staged: BTreeSet::new(), unstaged: set(&["a"]) }));
        assert_eq!(read(serde_json::json!({ "staged_count": 1, "unstaged_count": 0 })), None);
        assert_eq!(read(serde_json::json!({ "staged_count": 0, "unstaged_count": 2, "unstaged": [{ "path": "a" }] })), None);
        assert_eq!(read(serde_json::json!({ "staged_count": 0, "unstaged_count": 1, "unstaged": [{ "kind": "added" }] })), None);
        assert_eq!(read(serde_json::json!({ "staged_count": 0 })), None);
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
        let subjects = [Subject { tool: Tool::Git, program: git.clone(), prefix: vec![], work: work.clone(), watched: false }];
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

    /// The write pairs time what the scenarios run for the same operation: each of perf's lines is
    /// the last line scenario::bind gives the record, switch and merge, less the `--json` bind
    /// puts first on Forklift's, and after the options bind puts first on tuned git's.
    ///
    /// Revert: time `switch name` for git, or `stack` with a `--json` → the line differs.
    #[test]
    fn perfs_write_lines_are_the_scenarios_less_json() {
        let message = record_message(0);
        let name = "a-branch".to_string();
        for (write, op, argument) in [
            (Write::Commit, Op::Record { index: 0 }, message.as_str()),
            (Write::Switch, Op::Switch { name: name.clone() }, name.as_str()),
            (Write::Merge, Op::Merge { name: name.clone() }, name.as_str()),
        ] {
            let [forklift, git] = write_lines(write, argument);
            let last = |tool: Tool| bind(tool, &op).and_then(|lines| lines.last().cloned()).unwrap();
            let mut scenario = last(Tool::Forklift);
            assert_eq!(scenario.remove(0), "--json", "{:?}", write);
            assert_eq!(scenario, forklift, "{:?}", write);
            assert_eq!(last(Tool::Git), git, "{:?}", write);
            // Tuned git's subject puts the same options first.
            let tuned: Vec<&str> = GIT_TUNED.iter().copied().chain(git.iter().copied()).collect();
            assert_eq!(last(Tool::GitTuned), tuned, "{:?}", write);
        }
    }

    /// The cold rows' plan: run with the cache-drop program given, and otherwise not run, with the
    /// reason; on Windows never run, given a program or not. A row not run is not a failure, and
    /// its subjects run nothing.
    #[test]
    fn cold_rows_are_not_run_without_a_cache_drop_program_or_on_windows() {
        let program = PathBuf::from("/usr/local/bin/purge-caches");
        assert_eq!(cold_when(false, Some(program.clone())), Ok(DropCaches { program: program.clone(), limit: Duration::from_secs(600) }));
        assert_eq!(cold_when(false, None), Err("no cache-drop program given (--drop-caches)".to_string()));
        assert_eq!(cold_when(true, Some(program)), Err("cold runs are not built on Windows in this harness".to_string()));
        assert_eq!(cold_when(true, None), Err("cold runs are not built on Windows in this harness".to_string()));

        let mut rows = Vec::new();
        let mut timed = Vec::new();
        both(&mut rows, "log", "n", Ok(()), &Err("why".into()), |cold| { timed.push(cold.cloned()); Ok(Outcome::Timed { cells: vec![] }) }).unwrap();
        assert_eq!(timed, [None]);
        assert_eq!(rows.iter().map(|row| (row.cache, &row.outcome)).collect::<Vec<_>>(), [
            (Cache::Warm, &Outcome::Timed { cells: vec![] }), (Cache::Cold, &Outcome::NotRun { reason: "why".into() }),
        ]);
        let mut report = report();
        report.rows = rows;
        assert!(!report.failed());
    }

    /// A cold cell runs the cache-drop program before each repetition, and has no untimed run
    /// first; a drop program that fails voids the cell with its exit and stderr, before the
    /// command runs.
    #[cfg(unix)]
    #[test]
    fn a_cold_cell_drops_before_each_repetition_and_a_failed_drop_voids_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let (root, scratch, environment) = scratch("cold");
        let script = |name: &str, body: &str| {
            let path = scratch.work().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{}\n", body)).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let command = |log: &Path| Invocation {
            program: PathBuf::from("/bin/sh"), arguments: vec!["-c".into(), format!("echo run >> '{}'", log.display())], directory: scratch.work(), limit: Duration::from_secs(30),
        };
        let caches = |program: PathBuf, limit: Duration| DropCaches { program, limit };
        let long = Duration::from_secs(30);
        let log = scratch.work().join("log");
        let drop = caches(script("drop", &format!("echo drop >> '{}'", log.display())), long);
        assert!(matches!(measure(&command(&log), 3, Some(&drop), &scratch, &environment).unwrap(), Measured::Timed { .. }));
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "drop\nrun\ndrop\nrun\ndrop\nrun\n");
        let unrun = scratch.work().join("unrun");
        let failing = caches(script("failing", "echo no caches here >&2; exit 3"), long);
        assert_eq!(measure(&command(&unrun), 3, Some(&failing), &scratch, &environment).unwrap(), Measured::DropFailed { exit: Some(3), signal: None, stderr: "no caches here\n".into() });
        // A drop that dies of a signal keeps it; one that hangs is killed at its own limit, and is
        // told apart from the command being killed.
        let signalled = caches(script("signalled", "kill -9 $$"), long);
        assert_eq!(measure(&command(&unrun), 3, Some(&signalled), &scratch, &environment).unwrap(), Measured::DropFailed { exit: None, signal: Some(9), stderr: String::new() });
        let hanging = caches(script("hanging", "sleep 30"), Duration::from_millis(300));
        assert_eq!(measure(&command(&unrun), 3, Some(&hanging), &scratch, &environment).unwrap(), Measured::DropKilledAtLimit { limit_ms: 300 });
        assert!(!unrun.exists());
        #[allow(clippy::disallowed_methods)] // a test's own temporary folder
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Switch and merge are reported not comparable, with the reason, on a subject whose side
    /// branch is at the commit its current branch is at; merge is too while one of the two
    /// contains the other, and both are timed once each has commits the other lacks. Every write
    /// is timed on a fresh copy each time, after one untimed run when warm, and the subject itself
    /// is never changed; a copy that does not start or end as it should is not comparable.
    ///
    /// Revert: skip the differ check → switch and merge are timed over a change of nothing.
    #[cfg(unix)]
    #[test]
    fn switch_and_merge_are_not_comparable_when_the_side_branch_is_the_current_one() {
        let (root, scratch, environment) = scratch("differ");
        let git = crate::scrub::path_git();
        let work = scratch.work().join("repo");
        let run = |arguments: &[&str]| {
            let invocation = Invocation { program: git.clone(), arguments: arguments.iter().map(|argument| argument.to_string()).collect(), directory: work.clone(), limit: Duration::from_secs(30) };
            let outcome = tool::run(&invocation, &scratch, &environment).unwrap();
            assert!(outcome.succeeded(), "{:?}: {}", arguments, String::from_utf8_lossy(&outcome.stderr));
            String::from_utf8_lossy(&outcome.stdout).trim().to_string()
        };
        let commit_file = |name: &str| {
            std::fs::write(work.join(name), format!("{}\n", name)).unwrap();
            run(&["add", "-A"]);
            run(&["commit", "-q", "-m", name]);
        };
        std::fs::create_dir(&work).unwrap();
        run(&["init", "-q", "-b", "probe"]);
        std::fs::write(work.join("a"), "a\n").unwrap();
        commit_file("b");
        run(&["branch", SIDE_BRANCH]);
        let subjects = [Subject { tool: Tool::Git, program: git.clone(), prefix: vec![], work: work.clone(), watched: false }];
        let writes_now = || {
            let mut rows = Vec::new();
            writes(&subjects, &["a", "b"], 1, &Err("r".into()), &scratch, &environment, &|_| {}, &mut rows).unwrap();
            rows.into_iter().map(|row| (row.operation, row.cache, row.outcome)).collect::<Vec<_>>()
        };
        let timed = |outcome: &Outcome| matches!(outcome, Outcome::Timed { cells } if cells.len() == 1 && matches!(cells[0].result, Measured::Timed { .. }));
        let not_run = Outcome::NotRun { reason: "r".into() };
        let outcomes = |rows: &[(String, Cache, Outcome)]| rows.iter().map(|(_, _, outcome)| outcome.clone()).collect::<Vec<_>>();

        let rows = writes_now();
        let same = Outcome::NotComparable { reason: "git has perf-side at the commit its current branch is at, so switching to it or merging it would time a change of nothing".into() };
        assert_eq!(rows.iter().map(|(operation, cache, _)| (operation.as_str(), *cache)).collect::<Vec<_>>(), [
            ("commit", Cache::Warm), ("commit", Cache::Cold), ("switch", Cache::Warm), ("switch", Cache::Cold), ("merge", Cache::Warm), ("merge", Cache::Cold),
        ]);
        assert!(timed(&rows[0].2), "{:?}", rows[0]);
        assert_eq!(rows[1].2, not_run);
        assert_eq!(outcomes(&rows[2..]), [same.clone(), same.clone(), same.clone(), same]);

        // The side branch ahead of the current one: a switch changes something, a merge would only
        // fast-forward.
        run(&["switch", "-q", SIDE_BRANCH]);
        commit_file("c");
        run(&["switch", "-q", "probe"]);
        let rows = writes_now();
        let one_line = Outcome::NotComparable { reason: "git has perf-side and its current branch on one line of history, one containing the other, so merging it would not join two lines".into() };
        assert!(timed(&rows[2].2), "{:?}", rows[2]);
        assert_eq!(outcomes(&rows[3..]), [not_run.clone(), one_line.clone(), one_line]);

        commit_file("d");
        let head = run(&["rev-parse", "HEAD"]);
        let rows = writes_now();
        for (operation, cache, outcome) in &rows {
            match cache {
                Cache::Warm => assert!(timed(outcome), "{} {:?}", operation, outcome),
                Cache::Cold => assert_eq!(outcome, &not_run),
            }
        }
        assert_eq!(run(&["rev-parse", "HEAD"]), head);
        assert_eq!(run(&["status", "--porcelain"]), "");
        assert_eq!(std::fs::read_dir(scratch.work().join("copies")).unwrap().count(), 0);

        // A warm write cell runs its command once untimed first, on a copy of its own: a
        // post-commit hook, copied with the subject, logs two commits for one repetition.
        let hook = work.join(".git").join("hooks").join("post-commit");
        let ran = scratch.work().join("ran");
        let set_hook = |body: &str| {
            std::fs::write(&hook, format!("#!/bin/sh\n{}\n", body)).unwrap();
            std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        };
        set_hook(&format!("echo ran >> '{}'", ran.display()));
        assert!(writes_now().iter().all(|(_, cache, outcome)| *cache == Cache::Cold || timed(outcome)));
        assert_eq!(std::fs::read_to_string(&ran).unwrap(), "ran\nran\n");
        set_hook(":");

        // A write whose copy does not start exactly as the pair expects, or does not end clean, is
        // not comparable, and says which.
        std::fs::write(work.join("stray"), "s\n").unwrap();
        let rows = writes_now();
        assert_eq!(rows[0].2, Outcome::NotComparable { reason: "before repetition 1, git saw 3 staged of 2 expected, 1 more (stray)".into() });
        let stray = Outcome::NotComparable { reason: "before repetition 1, git saw 1 unstaged of 0 expected, 1 more (stray)".into() };
        assert_eq!([&rows[2].2, &rows[4].2], [&stray, &stray]);
        commit_file("stray");
        set_hook("echo left > left-behind");
        assert_eq!(writes_now()[0].2, Outcome::NotComparable { reason: "after repetition 1, git saw 1 unstaged of 0 expected, 1 more (left-behind)".into() });
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

    /// A perf run over the small shape writes perf.json and perf.md, every pair timed warm on all
    /// three subjects and its cold row not run without a cache-drop program: the sameness, differ
    /// and clean checks pass on a generated history, both subjects were built from one mirror
    /// (history.rs refuses an import whose current branch differs), and Forklift's is enrolled.
    ///
    /// Needs a forklift binary: `PROOF_LOAD_FORKLIFT=<path> cargo test -p proof-load -- --ignored`.
    #[test]
    #[ignore = "drives a forklift binary named by PROOF_LOAD_FORKLIFT"]
    fn a_perf_run_times_every_pair_on_every_subject() {
        let forklift = PathBuf::from(std::env::var_os("PROOF_LOAD_FORKLIFT").expect("PROOF_LOAD_FORKLIFT names the forklift binary under test"));
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let out = std::env::temp_dir().join(format!("proof-load-perf-run-{}-{}", std::process::id(), nanos));
        let options = PerfOptions { out: out.clone(), forklift: forklift.clone(), git: crate::scrub::path_git(), shape: "small".into(), reps: 1, drop_caches: None };
        let report = run(&options, &|_| {}).unwrap();
        let operations: Vec<&str> = report.rows.iter().filter(|row| row.cache == Cache::Warm).map(|row| row.operation.as_str()).collect();
        assert_eq!(operations, ["status", "log", "blame", "commit", "switch", "merge", "diff", "diff --staged"]);
        assert_eq!(report.rows.len(), 2 * operations.len());
        assert_eq!(report.header.drop_caches, None);
        for pair in report.rows.chunks(2) {
            assert_eq!((pair[0].cache, pair[1].cache, &pair[0].operation), (Cache::Warm, Cache::Cold, &pair[1].operation));
            assert_eq!(pair[1].outcome, Outcome::NotRun { reason: "no cache-drop program given (--drop-caches)".into() });
        }
        for row in report.rows.iter().filter(|row| row.cache == Cache::Warm) {
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
        assert!(std::fs::read_to_string(out.join("perf.md")).unwrap().contains("### diff --staged, warm\n"));

        // Forklift's subject is enrolled: its audit names a genesis, which an import alone has not.
        let area = out.join("scratch").join("0000-perf");
        let audit = std::process::Command::new(&forklift).args(["--json", "audit"]).current_dir(area.join("work").join("forklift"))
            .env_clear().env("PATH", std::env::var_os("PATH").unwrap_or_default()).env("HOME", area.join("home"))
            .env("XDG_CONFIG_HOME", area.join("home").join(".config")).env("TMPDIR", area.join("tmp")).output().unwrap();
        let audit = runner::document(&audit.stdout);
        assert_eq!(audit["ok"], true, "{}", audit);
        assert!(audit["data"]["genesis"].as_str().is_some_and(|genesis| !genesis.is_empty()), "{}", audit);
        #[allow(clippy::disallowed_methods)] // a test's own temporary folder
        let _ = std::fs::remove_dir_all(&out);
    }
}
