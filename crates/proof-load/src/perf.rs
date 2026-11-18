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
//!
//! The rows are then judged ([`judge`]): the gate a run is given names which checks fail it, and
//! the verdict is written into the document beside the rows.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::faulted::PROBE_REF;
use crate::generate::{self, Generated, Shape, SIDE_BRANCH};
use crate::report::Platform;
use crate::runner::{self, RunError as BuildError, GIT_CREATED_CONFIG};
use crate::scenario::{Tool, GIT_TUNED, URL, WORK};
use crate::scrub::{Environment, Output, Scratch};
use crate::tool::{self, Invocation, ToolError};

/// The perf document's format. A reader refuses any other number rather than guess at it.
pub const PERF_FORMAT: u32 = 4;

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
/// How many commits the push pair's remote stands behind the subject's head, and the pull pair's
/// stands past it.
const DISTANCE: usize = 3;
/// How long a forklift-server may take to start listening, and to stop once asked.
const SERVE_LIMIT: Duration = Duration::from_secs(60);
/// Why the remote rows were not run when no forklift-server was given.
const NO_SERVER: &str = "no forklift-server given (--forklift-server)";
/// Why the remote rows were not run on Windows.
const NO_REMOTE_ON_WINDOWS: &str = "remote rows are not built on Windows in this harness, which stops a forklift-server gracefully only on unix";
/// Why switch and merge were not run on a real history whose side branch was not named.
const NO_SIDE: &str = "no side branch given (--side-branch)";
/// Why the one-core audit was not run.
const NO_ONE_CORE: &str = "pinning a command to one core is built only on Linux, with taskset on the PATH";
/// The gate subject's tracked files, and how many of them each parcel after the first changes.
const GATE_FILES: usize = 240;
const GATE_TOUCHED: usize = 5;
/// The gate subject's parcels: more than the count at which Forklift's audit verifies signatures
/// in parallel (audit_utils's PARALLEL_THRESHOLD, 256 when this was written), so the unrestricted
/// audit takes the parallel path. Its ratio to the one-core audit catches that path running much
/// slower than one core; an audit that lost its parallelism, and runs as fast as one core, passes.
const GATE_PARCELS: usize = 300;
/// The gate subject's rows, Forklift's alone, warm only.
pub const FRESH_COMPACT: &str = "fresh compact";
pub const REPACK: &str = "repack";
pub const AUDIT: &str = "audit main";
pub const AUDIT_ONE_CORE: &str = "audit main, one core";
/// The paired rows a gross regression fails under either gate.
pub const GROSS: [&str; 7] = ["status", "diff", "diff --staged", "switch", "commit", "verify", "pack"];
/// How many times tuned git's median Forklift's may be on a gross row.
const GROSS_TIMES: u64 = 10;

/// One perf run's document.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerfReport {
    pub format: u32,
    pub header: Header,
    pub rows: Vec<Row>,
    pub verdict: Verdict,
}

/// What a perf run ran on: everything the graded report's header records, plus the hardware.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    /// The generated shape's seed; none for a real history.
    pub seed: Option<u64>,
    /// The generated shape's name, or `history` for a real one.
    pub shape: String,
    /// The shape's scale, or the real history's source and what was mirrored of it, spelled out.
    pub shape_detail: String,
    /// The branch switch and merge time against; none when none was given for a real history.
    pub side_branch: Option<String>,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
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
    /// The arguments after the binary, as every repetition ran them. A clone's names its remote's
    /// URL and its new folder as `{url}` and `{work}`, since each repetition has its own.
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
    /// A transfer succeeded but did not carry the head across, so its time would be of no
    /// transfer: a push or a pull by the refs read before and after it, a clone by the head its new
    /// work tree landed on. What the reads said.
    NothingMoved { reason: String },
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

/// Which checks fail a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    /// The checks a run on a shared machine can hold: Forklift against its own ratios, a gross
    /// regression against tuned git, and a gated row that could not be run.
    #[serde(rename = "self")]
    SelfOnly,
    /// Every check: the self checks, any row that could not be run, and a clearly slower row no
    /// ticket tracks.
    All,
}

impl Gate {
    pub fn named(name: &str) -> Option<Gate> {
        match name {
            "self" => Some(Gate::SelfOnly),
            "all" => Some(Gate::All),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Gate::SelfOnly => "self",
            Gate::All => "all",
        }
    }
}

/// What the rows came to under the run's gate.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Verdict {
    pub gate: Gate,
    /// What fails the run under its gate; it passed when there is none.
    pub failures: Vec<Failure>,
    /// Every clearly slower row, gated or not, with the ticket that tracks it, if any.
    pub slower: Vec<Slower>,
    /// Tickets for a row this run timed and did not find clearly slower, which may be closable.
    pub closable: Vec<Ticket>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub operation: String,
    pub cache: Cache,
    pub check: Check,
    pub detail: String,
}

/// The check a failure failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    /// The row was not comparable, or a cell has no timing.
    CouldNotRun,
    /// Forklift's median is more than ten times tuned git's on a gross row.
    Gross,
    /// Forklift against its own other row: the repack against the fresh compact, or the audit
    /// against the one-core audit.
    SelfRatio,
    /// A clearly slower row no ticket tracks.
    Untracked,
}

/// A row where Forklift's median is more than 1.25 times tuned git's and its fastest repetition
/// slower than tuned git's slowest.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slower {
    pub operation: String,
    pub cache: Cache,
    pub forklift: Spread,
    pub git_tuned: Spread,
    pub ticket: Option<String>,
}

/// One entry of perf-tickets.json: the ticket tracking a clearly slower row, by the row's
/// operation and cache and the shape and platform it was slower on.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ticket {
    pub operation: String,
    pub shape: String,
    pub cache: Cache,
    pub os: Platform,
    pub ticket: String,
}

/// The tickets this build was shipped with, crates/proof-load/perf-tickets.json.
pub fn tickets() -> Result<Vec<Ticket>, String> {
    serde_json::from_str(include_str!("../perf-tickets.json")).map_err(|error| format!("perf-tickets.json: {}", error))
}

/// Forklift's and tuned git's timings in `cells`, where both were timed.
fn forklift_and_tuned(cells: &[Cell]) -> Option<(Spread, Spread)> {
    let wall = |tool: Tool| cells.iter().find(|cell| cell.subject == tool).and_then(|cell| match cell.result {
        Measured::Timed { wall, .. } => Some(wall),
        _ => None,
    });
    wall(Tool::Forklift).zip(wall(Tool::GitTuned))
}

/// Forklift's timing in the warm row `operation`, where it was timed.
fn forklift_warm(rows: &[Row], operation: &str) -> Option<Spread> {
    rows.iter().filter(|row| row.operation == operation && row.cache == Cache::Warm).find_map(|row| match &row.outcome {
        Outcome::Timed { cells } => cells.iter().find(|cell| cell.subject == Tool::Forklift).and_then(|cell| match cell.result {
            Measured::Timed { wall, .. } => Some(wall),
            _ => None,
        }),
        _ => None,
    })
}

/// Judges `rows`, timed on `shape` and `platform`, under `gate`, given the tickets that track
/// clearly slower rows. Under either gate: a gross row where Forklift's median is more than
/// [`GROSS_TIMES`] tuned git's; the repack's median above 1.5 times the fresh compact's, and the
/// audit's above twice the one-core audit's; and a gross or gate-subject row that could not be
/// run. Under `all` besides: any row that could not be run, and a clearly slower row with no
/// ticket. A row not run, for its stated reason, never fails, and only rows this run timed are
/// looked up in the tickets: a ticket for one that is no longer clearly slower is reported as
/// possibly closable, never as a failure.
pub fn judge(rows: &[Row], shape: &str, platform: Platform, gate: Gate, tickets: &[Ticket]) -> Verdict {
    let mut verdict = Verdict { gate, failures: Vec::new(), slower: Vec::new(), closable: Vec::new() };
    let gate_row = |operation: &str| [FRESH_COMPACT, REPACK, AUDIT, AUDIT_ONE_CORE].contains(&operation);
    let mut fail = |row: &Row, check: Check, detail: String| verdict.failures.push(Failure { operation: row.operation.clone(), cache: row.cache, check, detail });
    let mut slower = Vec::new();
    let mut closable = Vec::new();
    for row in rows {
        let gated = gate == Gate::All || GROSS.contains(&row.operation.as_str()) || gate_row(&row.operation);
        let unrun = match &row.outcome {
            Outcome::NotRun { .. } => None,
            Outcome::NotComparable { reason } => Some(format!("not comparable: {}", reason)),
            Outcome::Timed { cells } => cells.iter().find(|cell| !matches!(cell.result, Measured::Timed { .. })).map(|cell| format!("{}'s cell has no timing", cell.subject.label())),
        };
        let Outcome::Timed { cells } = &row.outcome else {
            if let Some(detail) = unrun.filter(|_| gated) {
                fail(row, Check::CouldNotRun, detail);
            }
            continue;
        };
        // A timed row compares Forklift with tuned git, and a gate-subject row is Forklift's alone:
        // a row missing a subject it needs was never compared, which is not a pass.
        let needs: &[Tool] = if gate_row(&row.operation) { &[Tool::Forklift] } else { &[Tool::Forklift, Tool::GitTuned] };
        let missing = needs.iter().find(|tool| !cells.iter().any(|cell| cell.subject == **tool)).map(|tool| format!("it has no {} cell", tool.label()));
        if let Some(detail) = unrun.or(missing).filter(|_| gated) {
            fail(row, Check::CouldNotRun, detail);
        }
        let Some((forklift, tuned)) = forklift_and_tuned(cells) else { continue };
        if GROSS.contains(&row.operation.as_str()) && forklift.median > tuned.median.saturating_mul(GROSS_TIMES) {
            fail(row, Check::Gross, format!("Forklift's median {} is more than {} times tuned git's {}", micros(forklift.median), GROSS_TIMES, micros(tuned.median)));
        }
        let ticket = tickets.iter().find(|ticket| ticket.operation == row.operation && ticket.shape == shape && ticket.cache == row.cache && ticket.os == platform);
        let clearly = forklift.median.saturating_mul(4) > tuned.median.saturating_mul(5) && forklift.min > tuned.max;
        if clearly {
            if ticket.is_none() && gate == Gate::All {
                fail(row, Check::Untracked, format!(
                    "clearly slower, Forklift's median {} against tuned git's {} and its fastest {} against tuned git's slowest {}, and perf-tickets.json names no ticket for it",
                    micros(forklift.median), micros(tuned.median), micros(forklift.min), micros(tuned.max),
                ));
            }
            slower.push(Slower { operation: row.operation.clone(), cache: row.cache, forklift, git_tuned: tuned, ticket: ticket.map(|ticket| ticket.ticket.clone()) });
        } else if let Some(ticket) = ticket {
            closable.push(ticket.clone());
        }
    }
    let ratios: [(&str, &str, u64, u64, &str); 2] = [(REPACK, FRESH_COMPACT, 3, 2, "1.5 times"), (AUDIT, AUDIT_ONE_CORE, 2, 1, "twice")];
    for (row, against, times, per, said) in ratios {
        if let Some((timed, bound)) = forklift_warm(rows, row).zip(forklift_warm(rows, against)) {
            if timed.median.saturating_mul(per) > bound.median.saturating_mul(times) {
                let at = rows.iter().find(|candidate| candidate.operation == row && candidate.cache == Cache::Warm).expect("a row timed is a row");
                fail(at, Check::SelfRatio, format!("its median {} is more than {} the {}'s {}", micros(timed.median), said, against, micros(bound.median)));
            }
        }
    }
    verdict.slower = slower;
    verdict.closable = closable;
    verdict
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

    /// Whether the verdict failed the run under its gate.
    pub fn failed(&self) -> bool {
        !self.verdict.failures.is_empty()
    }

    pub fn markdown(&self) -> String {
        let header = &self.header;
        let mut out = String::from("# Proof Load performance\n\n## Header\n\n");
        let _ = writeln!(out, "- format: {}", self.format);
        match header.seed {
            Some(seed) => { let _ = writeln!(out, "- shape: {} (seed {}): {}", header.shape, seed, header.shape_detail); }
            None => { let _ = writeln!(out, "- shape: {}: {}", header.shape, header.shape_detail); }
        }
        match &header.side_branch {
            Some(side) => { let _ = writeln!(out, "- side branch: {}", side); }
            None => out.push_str("- side branch: none given\n"),
        }
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
        let verdict = &self.verdict;
        let _ = writeln!(out, "\n## Verdict\n\n- gate: {}", verdict.gate.label());
        if verdict.failures.is_empty() {
            out.push_str("- passed\n");
        }
        for failure in &verdict.failures {
            let check = match failure.check {
                Check::CouldNotRun => "could not run",
                Check::Gross => "gross regression",
                Check::SelfRatio => "against its own ratio",
                Check::Untracked => "clearly slower, no ticket",
            };
            let _ = writeln!(out, "- failed: {}, {}: {}: {}", failure.operation, failure.cache.label(), check, failure.detail);
        }
        for slower in &verdict.slower {
            let ticket = slower.ticket.as_deref().map_or_else(|| "no ticket".to_string(), |ticket| format!("tracked by {}", ticket));
            let _ = writeln!(out, "- clearly slower: {}, {}: Forklift's median {} against tuned git's {}; {}", slower.operation, slower.cache.label(), micros(slower.forklift.median), micros(slower.git_tuned.median), ticket);
        }
        for ticket in &verdict.closable {
            let _ = writeln!(out, "- possibly closable: {} names {}, {}, which this run timed and did not find clearly slower", ticket.ticket, ticket.operation, ticket.cache.label());
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
                            Measured::NothingMoved { reason } => {
                                let _ = writeln!(out, "| {} | voided: did not move the head: {} | | | | | {} |", cell.subject.label(), reason.replace('|', "\\|"), command);
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
    /// The generated history to time on, `small` when neither it nor `history_from` is given.
    pub shape: Option<String>,
    /// A real history to time on instead of a generated one: a git repository, mirrored first.
    pub history_from: Option<PathBuf>,
    /// The branch of `history_from`'s history that switch and merge time against; without it
    /// they are not run on a real history.
    pub side_branch: Option<String>,
    pub reps: usize,
    /// Which checks fail the run: all on a generated shape and self on a real history when none
    /// is given.
    pub gate: Option<Gate>,
    /// The program each cold repetition runs first to drop the operating system's caches; without
    /// it the cold rows are not run.
    pub drop_caches: Option<PathBuf>,
    /// The forklift-server that serves Forklift's remotes; without it the remote rows are not run.
    pub forklift_server: Option<PathBuf>,
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
    let source = source(options)?;
    let tickets = tickets()?;
    if options.reps == 0 {
        return Err("--reps 0: a cell needs at least one repetition".to_string());
    }
    let absolute = |flag: &str, path: &Path| tool::absolute(path).map_err(|error| format!("{} {}: {}", flag, path.display(), error));
    let forklift = absolute("--forklift", &options.forklift)?;
    let git = absolute("--git", &options.git)?;
    let drop_caches = options.drop_caches.as_deref().map(|program| absolute("--drop-caches", program)).transpose()?;
    let cold = cold_when(cfg!(windows), drop_caches);
    let forklift_server = options.forklift_server.as_deref().map(|program| absolute("--forklift-server", program)).transpose()?;
    let remote = remote_when(cfg!(windows), forklift_server);
    let mut output = Output::claim(&options.out).map_err(|error| error.to_string())?;
    let scratch = output.scratch("perf").map_err(|error| error.to_string())?;
    let search_path = crate::scrub::search_path(Some(&git), std::env::var_os("PATH"));
    let environment = Environment::scrubbed(&scratch, search_path, &BTreeMap::new()).map_err(|error| error.to_string())?;

    let built = build(&forklift, &git, &source, &scratch, &environment, progress).map_err(|error| format!("building the subjects failed: {}", error))?;
    let daemon = built.daemon.is_some();
    let mut rows = pairs(&built, options.reps, &cold, &remote, &scratch, &environment, progress).map_err(|error| error.to_string())?;
    let (shape_detail, side_branch) = (built.detail.clone(), built.side.clone());
    drop(built);
    let one_core = one_core_when(cfg!(target_os = "linux"), std::env::var_os("PATH"));
    gate_rows(&forklift, options.reps, &one_core, &scratch, &environment, progress, &mut rows).map_err(|error| error.to_string())?;

    let mut tools = BTreeMap::new();
    tools.insert("forklift".to_string(), crate::run::described(&forklift));
    tools.insert("git".to_string(), crate::run::described(&git));
    if let Ok(server) = &remote {
        tools.insert("forklift-server".to_string(), crate::run::described(server));
    }
    let mut configurations = BTreeMap::new();
    configurations.insert(Tool::Git.label().to_string(), crate::run::created_configuration());
    configurations.insert(Tool::GitTuned.label().to_string(), format!("{}; a commit-graph written untimed after the build", crate::run::tuned_configuration(daemon)));
    let (seed, shape) = match &source {
        Source::Generated(shape) => (Some(shape.seed), shape.name.to_string()),
        Source::History(_) => (None, "history".to_string()),
    };
    let platform = Platform::current();
    let verdict = judge(&rows, &shape, platform, gate_of(options), &tickets);
    let header = Header {
        seed, shape, shape_detail, side_branch, reps: options.reps, tools,
        platform, os: crate::run::operating_system(), filesystem: crate::run::filesystem_of(output.root()),
        configurations, hardware: hardware(), drop_caches: cold.as_ref().ok().map(|drop| drop.program.display().to_string()),
    };
    let report = PerfReport { format: PERF_FORMAT, header, rows, verdict };
    for (name, text) in [("perf.md", report.markdown()), ("perf.json", report.json())] {
        std::fs::write(output.root().join(name), text).map_err(|error| format!("cannot write {}: {}", name, error))?;
    }
    Ok(report)
}

/// What a run times on: a generated shape, or a real history by path, with the branch switch and
/// merge time against, if one was named.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Source {
    Generated(Shape),
    History(History),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct History {
    from: PathBuf,
    side: Option<String>,
}

/// The history `options` name, refusing two at once, a side branch for a generated shape, which
/// has its own, a shape this build does not have, and a history that is no folder.
fn source(options: &PerfOptions) -> Result<Source, String> {
    match (&options.shape, &options.history_from) {
        (Some(_), Some(_)) => Err("--shape and --history-from each name the history to time on; give one".to_string()),
        (shape, None) => {
            if options.side_branch.is_some() {
                return Err(format!("--side-branch names a branch of --history-from's history; a generated shape has its own, {}", SIDE_BRANCH));
            }
            let name = shape.as_deref().unwrap_or("small");
            Shape::named(name).map(Source::Generated).ok_or_else(|| {
                let names: Vec<&str> = Shape::ALL.iter().map(|shape| shape.name).collect();
                format!("--shape {}: not a shape this build has (it has {})", name, names.join(", "))
            })
        }
        (None, Some(from)) => {
            let from = std::path::absolute(from).map_err(|error| format!("--history-from {}: {}", from.display(), error))?;
            if !from.is_dir() {
                return Err(format!("--history-from {}: not a folder", from.display()));
            }
            if options.gate == Some(Gate::All) {
                return Err("--gate all fails a clearly slower row no ticket tracks, and a ticket names a generated shape, so none can track a row of --history-from's history; judge it with --gate self".to_string());
            }
            Ok(Source::History(History { from, side: options.side_branch.clone() }))
        }
    }
}

/// The three subjects over one history, and the daemon kept running for tuned git.
struct Built {
    generated: Generated,
    /// The history's description for the header: the shape's scale, or the real history's source
    /// and what was mirrored of it.
    detail: String,
    /// The branch switch and merge time against, if any.
    side: Option<String>,
    /// The generated history's branch, which every subject also has [`PROBE_REF`] at the tip of.
    branch: String,
    subjects: Vec<Subject>,
    /// Stopped when dropped.
    daemon: Option<runner::FsmonitorDaemon>,
}

fn build(forklift: &Path, git: &Path, source: &Source, scratch: &Scratch, environment: &Environment, progress: &dyn Fn(&str)) -> Result<Built, BuildError> {
    let work = scratch.work();
    let (mirror, generated, detail, side) = match source {
        Source::Generated(shape) => {
            progress(&format!("generating the {} history", shape.name));
            let mirror = work.join("mirror.git");
            let generated = generate::mirror(git, shape, &mirror, scratch, environment, BUILD_LIMIT)?;
            (mirror, generated, shape.describe(), Some(SIDE_BRANCH.to_string()))
        }
        Source::History(history) => {
            progress(&format!("mirroring the history at {}", history.from.display()));
            let copy = crate::fidelity::take_history(git, &history.from, scratch, environment, BUILD_LIMIT);
            let (mirror, described) = copy.copied.map_err(|reason| BuildError::Io(format!("the history could not be mirrored: {}", reason)))?;
            if let Some(side) = &history.side {
                let reference = format!("refs/heads/{}", side);
                let found = tool::run(&Invocation { program: git.to_path_buf(), arguments: vec!["rev-parse".into(), "--verify".into(), "-q".into(), reference], directory: mirror.clone(), limit: BUILD_LIMIT }, scratch, environment).map_err(BuildError::Tool)?;
                if !found.succeeded() {
                    return Err(BuildError::Io(format!("--side-branch {}: the history at {} has no branch of that name", side, history.from.display())));
                }
            }
            let generated = generate::read_history(git, &mirror, scratch, environment, BUILD_LIMIT)?;
            let detail = format!("the history at {}, mirrored: {}; {} text files at HEAD", history.from.display(), described, generated.text_files.len());
            (mirror, generated, detail, history.side.clone())
        }
    };
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
    Ok(Built { generated, detail, side, branch, subjects, daemon })
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

/// Every pair, each as a warm row and then a cold one: status, log, blame and verify on the clean
/// tree; the write pairs, commit, switch, merge and pack, each repetition on a fresh copy of the
/// clean subject;
/// the remote pairs, clone, push and pull, each repetition against a fresh copy of its remote;
/// then diff and diff --staged over one edit set.
#[allow(clippy::too_many_arguments)]
fn pairs(built: &Built, reps: usize, cold: &Cold, remote: &Remote, scratch: &Scratch, environment: &Environment, progress: &dyn Fn(&str)) -> Result<Vec<Row>, Stopped> {
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
    let reads: [(&str, String, [&[&str]; 2]); 4] = [
        ("status", "Forklift's stocktake --summary against git's status, on the clean tree".to_string(), [&["stocktake", "--summary"], &["status"]]),
        ("log", "Forklift's history against git's log, the whole history".to_string(), [&["history"], &["log"]]),
        ("blame", format!("{}, the file the history changes most, first-parent on both", blame), [&["blame", blame], &["blame", "--first-parent", blame]]),
        (
            "verify",
            "Forklift's audit --full against git's fsck --full --strict, every record of the whole history; the imported parcels carry no signatures, which the audit counts as legacy".to_string(),
            [&["audit", "--full"], &["fsck", "--full", "--strict"]],
        ),
    ];
    for (operation, note, [forklift, git]) in reads {
        progress(&format!("timing {}", operation));
        let check = clean.clone().map_err(|reason| format!("before the clean-tree rows, {}", reason));
        both(&mut rows, operation, &note, check, cold, |cold| timed_pair(subjects, forklift, git, reps, cold, scratch, environment))?;
    }

    writes(subjects, built.side.as_deref(), &edited, reps, cold, scratch, environment, progress, &mut rows)?;
    remotes(subjects, &built.branch, &edited, reps, cold, remote, scratch, environment, progress, &mut rows)?;

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
    Pack,
}

/// A write pair's timed lines, Forklift's and then git's, before a subject's own prefix, given its
/// message or branch: the lines scenario::bind runs for its record, switch and merge, less the
/// `--json` it gives Forklift's. A test holds the two tables to each other.
fn write_lines(write: Write, argument: &str) -> [Vec<&str>; 2] {
    match write {
        Write::Commit => [vec!["stack", "-m", argument], vec!["commit", "-q", "-m", argument]],
        Write::Switch => [vec!["shift", argument], vec!["switch", "-q", argument]],
        Write::Merge => [vec!["consolidate", argument], vec!["merge", "-q", "--no-edit", argument]],
        Write::Pack => [vec!["store", "compact", "--all"], vec!["gc", "-q"]],
    }
}

/// The write pairs, warm and cold: commit over the edit set, staged untimed, then switch to the
/// side branch and merge it, not run when there is none, then pack the whole store. Switch is not
/// comparable unless the side branch is at another commit than the current branch in every
/// subject, and merge unless neither of the two contains the other, so that it joins two lines of
/// history. Each repetition runs on a fresh copy of the clean subject, which every write must
/// leave clean; the subjects themselves are not changed.
#[allow(clippy::too_many_arguments)]
fn writes(subjects: &[Subject], side: Option<&str>, edited: &[&str], reps: usize, cold: &Cold, scratch: &Scratch, environment: &Environment, progress: &dyn Fn(&str), rows: &mut Vec<Row>) -> Result<(), Stopped> {
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

    match side {
        None => {
            for operation in ["switch", "merge"] {
                for cache in [Cache::Warm, Cache::Cold] {
                    rows.push(Row { operation: operation.to_string(), cache, note: "against the side branch".to_string(), outcome: Outcome::NotRun { reason: NO_SIDE.to_string() } });
                }
            }
        }
        Some(side) => {
            let (switch, merge) = sides(subjects, side, scratch, environment)?;
            for (write, operation, note, check) in [
                (Write::Switch, "switch", format!("from the current branch to {}, on a fresh copy for each repetition", side), switch),
                (Write::Merge, "merge", format!("{} into the current branch, each with commits the other lacks, on a fresh copy for each repetition", side), merge),
            ] {
                progress(&format!("timing {}", operation));
                let fresh = Fresh { operation, edit: &[], before: &clean, copies: &copies };
                let [forklift, git] = write_lines(write, side);
                both(rows, operation, &note, check, cold, |cold| fresh_pair(subjects, &forklift, &git, reps, cold, &fresh, scratch, environment))?;
            }
        }
    }

    progress("timing pack");
    let note = "every object of the store repacked, on a fresh copy for each repetition: Forklift's store compact --all against git's gc; an import leaves Forklift's store packed already";
    let fresh = Fresh { operation: "pack", edit: &[], before: &clean, copies: &copies };
    let [forklift, git] = write_lines(Write::Pack, "");
    both(rows, "pack", note, Ok(()), cold, |cold| fresh_pair(subjects, &forklift, &git, reps, cold, &fresh, scratch, environment))?;
    Ok(())
}

/// The program, with the arguments before Forklift's own, that pins a command to one core, or
/// why the one-core row is not run.
type OneCore = Result<(PathBuf, Vec<String>), String>;

/// The one-core plan on a platform that is Linux or not, given the harness's PATH: taskset, the
/// first one on it, pinning to core 0.
fn one_core_when(linux: bool, path: Option<std::ffi::OsString>) -> OneCore {
    if !linux {
        return Err(NO_ONE_CORE.to_string());
    }
    let found = path.iter().flat_map(std::env::split_paths).map(|folder| folder.join("taskset")).find(|candidate| candidate.is_file());
    found.map(|taskset| (taskset, vec!["-c".to_string(), "0".to_string()])).ok_or_else(|| NO_ONE_CORE.to_string())
}

/// The gate subject's text file at `index`, and its first content, the same on every run.
fn gate_file(index: usize) -> (String, String) {
    let path = format!("src/mod{:02}/file{:03}.txt", index / 20, index);
    let mut text = String::new();
    for line in 0..8 {
        let _ = writeln!(text, "file {} line {}: value={}", index, line, (index * 8 + line).wrapping_mul(2_654_435_761) % 1_000_003);
    }
    (path, text)
}

/// The files the gate subject's parcel `parcel` changes after the first.
fn gate_touched(parcel: usize) -> impl Iterator<Item = usize> {
    (0..GATE_TOUCHED).map(move |touch| (parcel * 7 + touch * 31 + parcel * touch * 17) % GATE_FILES)
}

/// Builds Forklift's gate subject at `at`, which must not exist yet: a fresh warehouse whose
/// first parcel is stacked before it is enrolled and every later one after, [`GATE_PARCELS`] in
/// all, each after the first changing [`GATE_TOUCHED`] of its [`GATE_FILES`] files, its objects
/// left loose. Gives why it could not, as a command's own failure.
fn gate_subject(forklift: &Path, at: &Path, scratch: &Scratch, environment: &Environment) -> Result<Result<(), String>, Stopped> {
    std::fs::create_dir(at).map_err(|error| Stopped(format!("{}: {}", at.display(), error)))?;
    let run = |arguments: &[&str]| -> Result<Result<(), String>, Stopped> {
        let invocation = Invocation { program: forklift.to_path_buf(), arguments: arguments.iter().map(|argument| argument.to_string()).collect(), directory: at.to_path_buf(), limit: BUILD_LIMIT };
        let outcome = tool::run(&invocation, scratch, environment)?;
        Ok(if outcome.succeeded() { Ok(()) } else {
            Err(format!("Forklift's {} exited {:?}: {}", arguments.join(" "), outcome.exit, String::from_utf8_lossy(&outcome.stderr).trim()))
        })
    };
    let write = |path: &Path, text: &[u8]| -> Result<(), Stopped> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| Stopped(format!("{}: {}", parent.display(), error)))?;
        }
        std::fs::write(path, text).map_err(|error| Stopped(format!("{}: {}", path.display(), error)))
    };
    macro_rules! step {
        ($arguments:expr) => {
            if let Err(reason) = run($arguments)? {
                return Ok(Err(reason));
            }
        };
    }
    step!(&["--json", "prepare"]);
    for index in 0..GATE_FILES {
        let (path, text) = gate_file(index);
        write(&at.join(path), text.as_bytes())?;
    }
    step!(&["--json", "load", "."]);
    step!(&["--json", "stack", "-m", "gate parcel 0"]);
    step!(&["--json", "office", "enroll"]);
    for parcel in 1..GATE_PARCELS {
        for index in gate_touched(parcel) {
            let path = at.join(gate_file(index).0);
            let mut text = std::fs::read(&path).map_err(|error| Stopped(format!("{}: {}", path.display(), error)))?;
            text.extend_from_slice(format!("parcel {} update: value={}\n", parcel, parcel.wrapping_mul(40_503) % 65_521).as_bytes());
            write(&path, &text)?;
        }
        step!(&["--json", "load", "."]);
        let message = format!("gate parcel {}", parcel);
        step!(&["--json", "stack", "-m", &message]);
    }
    Ok(Ok(()))
}

/// Whether Forklift's audit of the gate subject's main pallet counts every parcel after the first
/// as verified, its signature checked, and the first alone as legacy, or why not: the audit rows
/// must time signature checks, not their absence.
fn gate_signed(forklift: &Path, at: &Path, scratch: &Scratch, environment: &Environment) -> Result<Result<(), String>, Stopped> {
    let invocation = Invocation { program: forklift.to_path_buf(), arguments: vec!["--json".into(), "audit".into(), "main".into()], directory: at.to_path_buf(), limit: BUILD_LIMIT };
    let outcome = tool::run(&invocation, scratch, environment)?;
    if !outcome.succeeded() {
        return Ok(Err(format!("Forklift's --json audit main exited {:?}: {}", outcome.exit, String::from_utf8_lossy(&outcome.stderr).trim())));
    }
    let document = runner::document(&outcome.stdout);
    let (verified, legacy) = (document["data"]["verified_parcels"].as_u64(), document["data"]["legacy_parcels"].as_u64());
    let expected = (u64::try_from(GATE_PARCELS - 1).ok(), Some(1));
    Ok(if (verified, legacy) == expected { Ok(()) } else {
        Err(format!("Forklift's audit of the gate subject counted {:?} parcels verified and {:?} legacy, not {} and 1", verified, legacy, GATE_PARCELS - 1))
    })
}

/// The gate subject's rows, Forklift's alone and warm only: the fresh compact of the loose objects
/// its build left, on a fresh copy each repetition; the repack of its whole store; its audit of main; and that audit pinned
/// to one core, when `one_core` allows. Every row is not comparable when the subject could not be
/// built or its audit verified other than every signed parcel, and the fresh compact when it
/// packed nothing.
#[allow(clippy::too_many_arguments)]
fn gate_rows(forklift: &Path, reps: usize, one_core: &OneCore, scratch: &Scratch, environment: &Environment, progress: &dyn Fn(&str), rows: &mut Vec<Row>) -> Result<(), Stopped> {
    progress(&format!("building the gate subject, {} parcels", GATE_PARCELS));
    let at = scratch.work().join("gate");
    let built = match gate_subject(forklift, &at, scratch, environment)? {
        Ok(()) => gate_signed(forklift, &at, scratch, environment)?,
        Err(reason) => Err(format!("the gate subject could not be built: {}", reason)),
    };
    let notes = [
        (FRESH_COMPACT, format!("Forklift's store compact over the loose objects of a warehouse of {} signed parcels, each repetition on a fresh copy, since it leaves nothing loose", GATE_PARCELS)),
        (REPACK, "Forklift's store compact --all over the same store, packed by the fresh compact".to_string()),
        (AUDIT, "Forklift's audit of main, which checks every parcel's signature".to_string()),
        (AUDIT_ONE_CORE, "the same audit pinned to one core with taskset -c 0".to_string()),
    ];
    let subject = Subject { tool: Tool::Forklift, program: forklift.to_path_buf(), prefix: vec![], work: at.clone(), watched: false };
    for (operation, note) in notes {
        let row = |outcome: Outcome| Row { operation: operation.to_string(), cache: Cache::Warm, note: note.clone(), outcome };
        if let Err(reason) = &built {
            rows.push(row(Outcome::NotComparable { reason: reason.clone() }));
            continue;
        }
        progress(&format!("timing {}", operation));
        let cell = |command: Vec<String>, result: Measured| Outcome::Timed { cells: vec![Cell { subject: Tool::Forklift, command, result }] };
        let outcome = match operation {
            FRESH_COMPACT => {
                let command = subject.line(&["--json", "store", "compact"]);
                match fresh_compact(&subject, &command, reps, scratch, environment)? {
                    Ok(Ok(runs)) => cell(command, summarise(&runs)),
                    Ok(Err(ended)) => cell(command, ended),
                    Err(reason) => Outcome::NotComparable { reason },
                }
            }
            REPACK => {
                let command = subject.line(&["store", "compact", "--all"]);
                cell(command.clone(), measure(&subject.invocation(command), reps, None, scratch, environment)?)
            }
            AUDIT => {
                let command = subject.line(&["audit", "main"]);
                cell(command.clone(), measure(&subject.invocation(command), reps, None, scratch, environment)?)
            }
            _ => match one_core {
                Err(reason) => Outcome::NotRun { reason: reason.clone() },
                Ok((program, prefix)) => {
                    let pinned = Subject { tool: Tool::Forklift, program: program.clone(), prefix: prefix.iter().cloned().chain([forklift.display().to_string()]).collect(), work: at.clone(), watched: false };
                    let command = pinned.line(&["audit", "main"]);
                    let shown: Vec<String> = std::iter::once(program.display().to_string()).chain(command.iter().cloned()).collect();
                    cell(shown, measure(&pinned.invocation(command), reps, None, scratch, environment)?)
                }
            },
        };
        rows.push(row(outcome));
    }
    Ok(())
}

/// The gate `options` judges by: the one it names, else all on a generated shape and self on a
/// real history, which no ticket can track.
fn gate_of(options: &PerfOptions) -> Gate {
    options.gate.unwrap_or(if options.history_from.is_some() { Gate::SelfOnly } else { Gate::All })
}

/// The fresh compact's `reps` timed runs, each on a fresh copy of the gate subject's loose store
/// after one untimed run on another, and then the subject's own compact, untimed, so the repack
/// after it runs over a packed store. Every run must pack something, since the subject is built
/// loose: gives why the row is not comparable when one packed nothing, and the cell's ending when
/// a run ended without a timing.
fn fresh_compact(subject: &Subject, command: &[String], reps: usize, scratch: &Scratch, environment: &Environment) -> Result<Result<Result<Vec<tool::Timed>, Measured>, String>, Stopped> {
    let copies = scratch.work().join("gate-copies");
    std::fs::create_dir_all(&copies).map_err(|error| Stopped(format!("{}: {}", copies.display(), error)))?;
    let mut runs = Vec::with_capacity(reps);
    for rep in 0..=reps {
        let copied = Copied(Some(copies.join(format!("fresh-compact-{}", rep))));
        let compacted = crate::scrub::copy_tree(&subject.work, copied.path())
            .map_err(|error| Stopped(format!("copying {} to {}: {:?}", subject.work.display(), copied.path().display(), error)))
            .and_then(|()| compact_once(&subject.at(copied.path().to_path_buf()), command, scratch, environment));
        let removed = copied.remove();
        let compacted = compacted?;
        removed?;
        match compacted {
            Ok(Ok(timed)) => if rep > 0 { runs.push(timed) },
            ended => return Ok(ended.map(|ended| ended.map(|_| Vec::new()))),
        }
    }
    Ok(match compact_once(subject, command, scratch, environment)? {
        Ok(Ok(_)) => Ok(Ok(runs)),
        Ok(Err(ended)) => Err(format!("the gate subject's own compact, untimed, ended without packing: {:?}", ended)),
        Err(reason) => Err(reason),
    })
}

/// One `store compact` of `subject`'s store, after its writes are settled, which must pack at least
/// one object.
fn compact_once(subject: &Subject, command: &[String], scratch: &Scratch, environment: &Environment) -> Result<Result<Result<tool::Timed, Measured>, String>, Stopped> {
    settle(&subject.work)?;
    Ok(match repetition(&subject.invocation(command.to_vec()), None, scratch, environment)? {
        Err(ended) => Ok(Err(ended)),
        Ok(timed) => match runner::document(&timed.outcome.stdout)["data"]["objects_packed"].as_u64() {
            Some(packed) if packed > 0 => Ok(Ok(timed)),
            packed => Err(format!("the fresh compact packed {:?} objects, so it timed no packing", packed)),
        },
    })
}

/// The forklift-server the remote rows serve Forklift's remotes with, or why they are not run.
type Remote = Result<PathBuf, String>;

/// The remote rows' plan on a platform that is Windows or not, given the forklift-server the
/// caller named, if any.
fn remote_when(windows: bool, server: Option<PathBuf>) -> Remote {
    match server {
        _ if windows => Err(NO_REMOTE_ON_WINDOWS.to_string()),
        Some(server) => Ok(server),
        None => Err(NO_SERVER.to_string()),
    }
}

/// The value of a step whose inner error says why it failed, or that reason returned as the
/// enclosing function's own inner error.
macro_rules! or_return {
    ($step:expr) => {
        match $step {
            Ok(value) => value,
            Err(reason) => return Ok(Err(reason)),
        }
    };
}

/// A remote pair: one that talks to a remote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transfer {
    Clone,
    Push,
    Pull,
}

impl Transfer {
    fn operation(self) -> &'static str {
        match self {
            Transfer::Clone => "clone",
            Transfer::Push => "push",
            Transfer::Pull => "pull",
        }
    }
}

/// A remote pair's timed lines, Forklift's and then git's, before a subject's own prefix, with the
/// remote as [`URL`] and a clone's new folder as [`WORK`]: the lines scenario::bind runs for its
/// full clone, push and fetch, less the `--json` it gives Forklift's. A test holds the two tables
/// to each other.
fn remote_lines(transfer: Transfer) -> [Vec<&'static str>; 2] {
    match transfer {
        Transfer::Clone => [vec!["clone", URL, WORK], vec!["clone", "-q", "--no-local", URL, WORK]],
        Transfer::Push => [vec!["lift"], vec!["push", "-q", "-u", "origin", "HEAD"]],
        Transfer::Pull => [vec!["lower"], vec!["pull", "-q", "--ff-only"]],
    }
}

/// What one subject's remote pairs start from, each copied afresh for every repetition. `level`
/// is a remote, a forklift-server root for Forklift's and a bare repository for git's, holding the
/// history's branch and [`PROBE_REF`], the subject's current branch, at the subject's head: the
/// clone's remote, and the push's. `ahead` is the subject with [`DISTANCE`] commits more on
/// [`PROBE_REF`], the push's subject; and `past` a remote holding that branch there, the pull's.
///
/// The push's subject goes ahead rather than its remote behind because Forklift's subject is
/// enrolled at its head: a remote behind it would lack the parcel trust begins at, and
/// forklift-server refuses an unsigned parcel it cannot place before that boundary.
#[derive(Clone, Debug)]
struct Hosts {
    level: PathBuf,
    ahead: PathBuf,
    past: PathBuf,
}

impl Hosts {
    /// The remote `transfer` runs against.
    fn remote(&self, transfer: Transfer) -> &Path {
        match transfer {
            Transfer::Clone | Transfer::Push => &self.level,
            Transfer::Pull => &self.past,
        }
    }

    /// The work tree a push or a pull runs in, copied for each repetition: the subject's own, but
    /// the push's ahead of it.
    fn work<'a>(&'a self, subject: &'a Subject, transfer: Transfer) -> &'a Path {
        match transfer {
            Transfer::Push => &self.ahead,
            Transfer::Clone | Transfer::Pull => &subject.work,
        }
    }
}

/// The remote pairs, warm and cold: clone, push and pull, each repetition against a fresh copy of
/// a remote built untimed from the clean subject, and push and pull in a fresh copy of a work
/// tree too. Not run without a forklift-server, and not comparable when the remotes could not be
/// built.
#[allow(clippy::too_many_arguments)]
fn remotes(subjects: &[Subject], branch: &str, edited: &[&str], reps: usize, cold: &Cold, remote: &Remote, scratch: &Scratch, environment: &Environment, progress: &dyn Fn(&str), rows: &mut Vec<Row>) -> Result<(), Stopped> {
    let transports = "Forklift's over HTTP on loopback to a forklift-server, git's to a bare repository by path";
    let transfers = [
        (Transfer::Clone, format!("the whole history of {}, cloned into a new folder from a fresh copy of the remote for each repetition; {}", branch, transports)),
        (Transfer::Push, format!(
            "the current branch with {} commits more than the remote's, pushed from a fresh copy of the subject so advanced to a fresh copy of the remote for each repetition, which must leave the remote's branch at the pushed head; {}",
            DISTANCE, transports,
        )),
        (Transfer::Pull, format!(
            "the remote's branch, {} commits past the current one, pulled as a fast-forward into a fresh copy of the subject from a fresh copy of the remote for each repetition, which must leave the subject's head at the remote's; {}",
            DISTANCE, transports,
        )),
    ];
    let server = match remote {
        Ok(server) => server,
        Err(reason) => {
            for (transfer, note) in transfers {
                for cache in [Cache::Warm, Cache::Cold] {
                    rows.push(Row { operation: transfer.operation().to_string(), cache, note: note.clone(), outcome: Outcome::NotRun { reason: reason.clone() } });
                }
            }
            return Ok(());
        }
    };
    let copies = scratch.work().join("copies");
    std::fs::create_dir_all(&copies).map_err(|error| Stopped(format!("{}: {}", copies.display(), error)))?;
    progress("building the remotes");
    let folder = Copied(Some(scratch.work().join("remotes")));
    let built = hosts(subjects, server, branch, edited, folder.path(), scratch, environment)?;
    let check = built.as_ref().map(|_| ()).map_err(|reason| format!("building the remotes failed: {}", reason));
    let hosts = built.as_deref().unwrap_or(&[]);
    for (transfer, note) in transfers {
        progress(&format!("timing {}", transfer.operation()));
        both(rows, transfer.operation(), &note, check.clone(), cold, |cold| remote_pair(subjects, hosts, transfer, branch, reps, cold, server, &copies, scratch, environment))?;
    }
    // A full tree for each subject, and its remotes: removed as the write pairs' copies are.
    folder.remove()
}

/// Builds every subject's remotes and its advanced work tree under `folder`, untimed, or gives
/// why one could not be built.
#[allow(clippy::too_many_arguments)]
fn hosts(subjects: &[Subject], server: &Path, branch: &str, edited: &[&str], folder: &Path, scratch: &Scratch, environment: &Environment) -> Result<Result<Vec<Hosts>, String>, Stopped> {
    std::fs::create_dir_all(folder).map_err(|error| Stopped(format!("{}: {}", folder.display(), error)))?;
    if edited.is_empty() {
        return Ok(Err("the history has no text file to make the advanced commits with".to_string()));
    }
    let mut all = Vec::new();
    for subject in subjects {
        let at = |name: &str| folder.join(format!("{}-{}", subject.tool.label(), name));
        let hosts = Hosts { level: at("level"), ahead: at("ahead"), past: at("past") };
        // A watched copy's untimed git commands start git's own fsmonitor daemon over it; these
        // stop it once the copy is done with, as the write pairs' daemons are stopped.
        let stops = |work: &Path| subject.watched.then(|| runner::StopsDaemon { program: subject.program.clone(), work: work.to_path_buf(), environment: environment.clone() });
        let source = Copied(Some(at("level-source")));
        crate::scrub::copy_tree(&subject.work, source.path()).map_err(|error| Stopped(format!("copying {} to {}: {:?}", subject.work.display(), source.path().display(), error)))?;
        let level = {
            let _stops = stops(source.path());
            host(&subject.at(source.path().to_path_buf()), &[branch, PROBE_REF], server, &hosts.level, scratch, environment)
        };
        let removed = source.remove();
        let level = level?;
        removed?;
        if let Err(reason) = level {
            return Ok(Err(format!("{}'s remote at its head: {}", named(subject.tool), reason)));
        }
        crate::scrub::copy_tree(&subject.work, &hosts.ahead).map_err(|error| Stopped(format!("copying {} to {}: {:?}", subject.work.display(), hosts.ahead.display(), error)))?;
        keep_times(&subject.work, &hosts.ahead)?;
        let _stops = stops(&hosts.ahead);
        let ahead = subject.at(hosts.ahead.clone());
        let advanced = advance(&ahead, edited, scratch, environment)?;
        if let Err(reason) = advanced {
            return Ok(Err(format!("{}'s commits past its head: {}", named(subject.tool), reason)));
        }
        if let Err(reason) = host(&ahead, &[PROBE_REF], server, &hosts.past, scratch, environment)? {
            return Ok(Err(format!("{}'s remote past its head: {}", named(subject.tool), reason)));
        }
        all.push(hosts);
    }
    Ok(Ok(all))
}

/// Makes [`DISTANCE`] commits on `copy`'s current branch, each an edit of one of `edited`.
fn advance(copy: &Subject, edited: &[&str], scratch: &Scratch, environment: &Environment) -> Result<Result<(), String>, Stopped> {
    for (index, file) in edited.iter().cycle().take(DISTANCE).enumerate() {
        edit(&copy.work, &[file])?;
        let message = format!("perf: commit {} of {} past the subject's head", index + 1, DISTANCE);
        let lines: [&[&str]; 2] = if copy.tool == Tool::Forklift {
            [&["load", "."], &["stack", "-m", &message]]
        } else {
            [&["add", "-A"], &["commit", "-q", "-m", &message]]
        };
        for line in lines {
            or_return!(output(copy, line, scratch, environment)?);
        }
    }
    Ok(Ok(()))
}

/// Runs `arguments` with `program` in `directory`, untimed, giving what it printed on stdout, or
/// why it did not succeed.
fn untimed(program: &Path, arguments: &[&str], directory: &Path, scratch: &Scratch, environment: &Environment) -> Result<Result<String, String>, Stopped> {
    let invocation = Invocation { program: program.to_path_buf(), arguments: arguments.iter().map(|argument| argument.to_string()).collect(), directory: directory.to_path_buf(), limit: COMMAND_LIMIT };
    let outcome = tool::run(&invocation, scratch, environment)?;
    if !outcome.succeeded() {
        return Ok(Err(format!("{} {} exited {:?}: {}", program.display(), arguments.join(" "), outcome.exit, String::from_utf8_lossy(&outcome.stderr).trim())));
    }
    Ok(Ok(String::from_utf8_lossy(&outcome.stdout).into_owned()))
}

/// Builds a remote at `root` holding `branches` where `source`, a copy of a subject on
/// [`PROBE_REF`], has them: a forklift-server root lifted to from the copy, which it shifts
/// between them, for Forklift's; a bare repository pushed to, its HEAD naming the first of
/// `branches`, for git's, so that git's clone checks out the branch Forklift's does.
fn host(source: &Subject, branches: &[&str], server: &Path, root: &Path, scratch: &Scratch, environment: &Environment) -> Result<Result<(), String>, Stopped> {
    let root_text = root.display().to_string();
    if source.tool != Tool::Forklift {
        or_return!(untimed(&source.program, &["init", "--bare", "-q", "-b", branches[0], &root_text], scratch.root(), scratch, environment)?);
        for [key, value] in GIT_CREATED_CONFIG {
            or_return!(untimed(&source.program, &["config", key, value], root, scratch, environment)?);
        }
        let mut push = vec!["push".to_string(), "-q".to_string(), root_text];
        push.extend(branches.iter().map(|branch| format!("refs/heads/{0}:refs/heads/{0}", branch)));
        or_return!(output(source, &push.iter().map(String::as_str).collect::<Vec<_>>(), scratch, environment)?);
        return Ok(Ok(()));
    }
    or_return!(untimed(server, &["prepare", "--root", &root_text], scratch.root(), scratch, environment)?);
    let served = match tool::serve(server, root, scratch, environment, SERVE_LIMIT) {
        Ok(served) => served,
        Err(error) => return Ok(Err(format!("forklift-server over {} did not start: {}", root.display(), error))),
    };
    let mut lifted = output(source, &["config", "set", "remote.url", &served.url], scratch, environment)?.map(|_| ());
    // Lifted from PROBE_REF last, so the copy ends on the branch it started on.
    let order = branches.iter().filter(|branch| **branch != PROBE_REF).chain(branches.iter().filter(|branch| **branch == PROBE_REF));
    'lifting: for branch in order {
        for line in [&["shift", branch][..], &["lift"]] {
            if lifted.is_err() {
                break 'lifting;
            }
            lifted = output(source, line, scratch, environment)?.map(|_| ());
        }
    }
    if let Err(reason) = served.stop(SERVE_LIMIT) {
        return Ok(Err(format!("forklift-server over {}: {}", root.display(), reason)));
    }
    Ok(lifted)
}

/// Times a remote pair: every repetition against a fresh copy of each subject's remote for
/// `transfer`, and for push and pull from a fresh copy of the subject. Not comparable when a copy
/// did not reach the state the pair starts from, or the transfer left it not clean.
#[allow(clippy::too_many_arguments)]
fn remote_pair(subjects: &[Subject], hosts: &[Hosts], transfer: Transfer, branch: &str, reps: usize, cold: Option<&DropCaches>, server: &Path, copies: &Path, scratch: &Scratch, environment: &Environment) -> Result<Outcome, Stopped> {
    let [forklift, git] = remote_lines(transfer);
    let mut cells = Vec::new();
    for (subject, hosts) in subjects.iter().zip(hosts) {
        let command = subject.line(if subject.tool == Tool::Forklift { &forklift } else { &git });
        let cloned = match transfer {
            Transfer::Clone => match head(subject, scratch, environment)? {
                Ok(at) => Some(Head { branch: branch.to_string(), commit: at.commit }),
                Err(reason) => return Ok(Outcome::NotComparable { reason }),
            },
            Transfer::Push | Transfer::Pull => None,
        };
        let remote = Against { transfer, template: hosts.remote(transfer), work: hosts.work(subject, transfer), cloned, server, copies };
        match remote_cell(subject, &command, reps, cold, &remote, scratch, environment)? {
            Ok(result) => cells.push(Cell { subject: subject.tool, command, result }),
            Err(reason) => return Ok(Outcome::NotComparable { reason }),
        }
    }
    Ok(Outcome::Timed { cells })
}

/// What each repetition of a remote cell runs against: a fresh copy of `template`, served by
/// `server` for Forklift's, from a fresh copy of `work` for a push or a pull, each copy and a
/// clone's folder under `copies`. A clone must land on `cloned`, which only a clone has.
struct Against<'a> {
    transfer: Transfer,
    template: &'a Path,
    work: &'a Path,
    cloned: Option<Head>,
    server: &'a Path,
    copies: &'a Path,
}

/// One remote cell, as a write cell is: a warm cell's untimed run first, on copies of its own,
/// and every copy removed after its repetition, however the repetition ended.
fn remote_cell(subject: &Subject, command: &[String], reps: usize, cold: Option<&DropCaches>, remote: &Against, scratch: &Scratch, environment: &Environment) -> Result<Result<Measured, String>, Stopped> {
    let cache = if cold.is_some() { Cache::Cold } else { Cache::Warm };
    let first = if cold.is_some() { 1 } else { 0 };
    let mut runs = Vec::with_capacity(reps);
    for rep in first..=reps {
        let name = format!("{}-{}-{}-{}", remote.transfer.operation(), cache.label(), subject.tool.label(), rep);
        let work = Copied(Some(remote.copies.join(&name)));
        let host = Copied(Some(remote.copies.join(format!("{}-remote", name))));
        let step = remote_rep(subject, command, rep, cold, remote, work.path(), host.path(), scratch, environment);
        let removed = [if remote.transfer == Transfer::Clone { work.remove_if_made() } else { work.remove() }, host.remove()];
        let step = step?;
        for removed in removed {
            removed?;
        }
        match step {
            Step::Ran(timed) => runs.extend(timed),
            Step::Ended(ended) => return Ok(ended),
        }
    }
    Ok(Ok(summarise(&runs)))
}

/// Repetition `rep` of a remote cell, against a fresh copy of the remote at `host`, served for
/// Forklift's, and with the subject's copy, or the clone, at `work`; repetition 0 is the untimed
/// run. A push or a pull is judged by the refs read before and after it as well as clean after; a
/// clone, by where its new work tree landed and its reading clean.
#[allow(clippy::too_many_arguments)]
fn remote_rep(subject: &Subject, command: &[String], rep: usize, cold: Option<&DropCaches>, remote: &Against, work: &Path, host: &Path, scratch: &Scratch, environment: &Environment) -> Result<Step, Stopped> {
    let before = if rep == 0 { "before the untimed run".to_string() } else { format!("before repetition {}", rep) };
    crate::scrub::copy_tree(remote.template, host).map_err(|error| Stopped(format!("copying {} to {}: {:?}", remote.template.display(), host.display(), error)))?;
    if subject.tool != Tool::Forklift {
        return transfer_rep(subject, command, rep, cold, remote, work, host, &host.display().to_string(), &before, scratch, environment);
    }
    let mut served = match tool::serve(remote.server, host, scratch, environment, SERVE_LIMIT) {
        Ok(served) => served,
        Err(error) => return Ok(Step::Ended(Err(format!("{}, forklift-server over the remote did not start: {}", before, error)))),
    };
    let url = served.url.clone();
    let step = transfer_rep(subject, command, rep, cold, remote, work, host, &url, &before, scratch, environment);
    // Stopped before the copies are removed; one that ended on its own served a repetition
    // nothing can vouch for.
    let stopped = match served.ended() {
        Some(status) => Err(format!("forklift-server over the remote ended on its own, {}", status)),
        None => served.stop(SERVE_LIMIT).map_err(|reason| format!("forklift-server over the remote did not stop: {}", reason)),
    };
    let step = step?;
    let Err(reason) = stopped else {
        return Ok(step);
    };
    let at = if rep == 0 { "after the untimed run".to_string() } else { format!("after repetition {}", rep) };
    // What the command itself gave is kept beside the server's failure, which may be its cause.
    let own = match step {
        Step::Ended(Ok(Measured::Voided { exit, signal, stderr })) => {
            let ended = exit.map(|code| format!("exited {}", code)).or_else(|| signal.map(|signal| format!("died of signal {}", signal))).unwrap_or_else(|| "ended".to_string());
            format!("; the command had {}: {}", ended, stderr.trim())
        }
        Step::Ended(Ok(Measured::NothingMoved { reason })) => format!("; the command had moved nothing: {}", reason),
        Step::Ended(Err(reason)) => format!("; the repetition was already not comparable: {}", reason),
        Step::Ended(Ok(_)) | Step::Ran(_) => String::new(),
    };
    Ok(Step::Ended(Err(format!("{}, {}{}", at, reason, own))))
}

/// The repetition itself, against the copy of the remote at `host`, reached at `url`.
#[allow(clippy::too_many_arguments)]
fn transfer_rep(subject: &Subject, command: &[String], rep: usize, cold: Option<&DropCaches>, remote: &Against, work: &Path, host: &Path, url: &str, before: &str, scratch: &Scratch, environment: &Environment) -> Result<Step, Stopped> {
    let untimed_run = |invocation: &Invocation| match tool::run(invocation, scratch, environment) {
        Ok(_) => Ok(Step::Ran(None)),
        Err(ToolError::TimedOut { .. }) => Ok(Step::Ended(Ok(killed(invocation.limit)))),
        Err(error) => Err(Stopped::from(error)),
    };

    if remote.transfer == Transfer::Clone {
        let arguments = runner::fill(command.to_vec(), Some(url), work).map_err(|placeholder| Stopped(format!("the clone's line left {} unfilled", placeholder)))?;
        let invocation = Invocation { program: subject.program.clone(), arguments, directory: remote.copies.to_path_buf(), limit: COMMAND_LIMIT };
        settle(host)?;
        if rep == 0 {
            return untimed_run(&invocation);
        }
        let timed = match repetition(&invocation, cold, scratch, environment)? {
            Ok(timed) => timed,
            Err(ended) => return Ok(Step::Ended(Ok(ended))),
        };
        let clone = Subject { watched: false, ..subject.at(work.to_path_buf()) };
        match head(&clone, scratch, environment)? {
            Err(reason) => return Ok(Step::Ended(Err(format!("after repetition {}, the clone: {}", rep, reason)))),
            Ok(landed) => {
                let expected = remote.cloned.as_ref().expect("a clone's cell knows where it must land");
                if let Err(reason) = landed_on(clone.tool, expected, &landed) {
                    return Ok(Step::Ended(Ok(Measured::NothingMoved { reason: format!("repetition {}: {}", rep, reason) })));
                }
            }
        }
        if let Err(reason) = same(std::slice::from_ref(&clone), &Changes::default(), scratch, environment)? {
            return Ok(Step::Ended(Err(format!("after repetition {}, the clone: {}", rep, reason))));
        }
        return Ok(Step::Ran(Some(timed)));
    }

    crate::scrub::copy_tree(remote.work, work).map_err(|error| Stopped(format!("copying {} to {}: {:?}", remote.work.display(), work.display(), error)))?;
    keep_times(remote.work, work)?;
    let copy = subject.at(work.to_path_buf());
    let _daemon = if copy.watched {
        Some(runner::FsmonitorDaemon::watch(&copy.program, &copy.work, scratch, environment, COMMAND_LIMIT).map_err(|error| Stopped(error.to_string()))?.0)
    } else {
        None
    };
    let mut configure: Vec<Vec<String>> = if copy.tool == Tool::Forklift {
        vec![vec!["config".into(), "set".into(), "remote.url".into(), url.to_string()]]
    } else {
        vec![
            vec!["config".into(), "remote.origin.url".into(), url.to_string()],
            vec!["config".into(), "remote.origin.fetch".into(), "+refs/heads/*:refs/remotes/origin/*".into()],
        ]
    };
    if copy.tool != Tool::Forklift && remote.transfer == Transfer::Pull {
        configure.push(vec!["config".into(), format!("branch.{}.remote", PROBE_REF), "origin".into()]);
        configure.push(vec!["config".into(), format!("branch.{}.merge", PROBE_REF), format!("refs/heads/{}", PROBE_REF)]);
    }
    for line in configure {
        if let Err(reason) = output(&copy, &line.iter().map(String::as_str).collect::<Vec<_>>(), scratch, environment)? {
            return Ok(Step::Ended(Err(format!("{}, setting the remote failed: {}", before, reason))));
        }
    }
    if let Err(reason) = refresh_untimed(&copy, scratch, environment)? {
        return Ok(Step::Ended(Err(format!("{}, the refresh failed: {}", before, reason))));
    }
    let invocation = copy.invocation(command.to_vec());
    if rep == 0 {
        settle(work)?;
        settle(host)?;
        return untimed_run(&invocation);
    }
    if let Err(reason) = same(std::slice::from_ref(&copy), &Changes::default(), scratch, environment)? {
        return Ok(Step::Ended(Err(format!("{}, {}", before, reason))));
    }
    let refs_before = match refs(&copy, scratch, environment)? {
        Ok(refs) => refs,
        Err(reason) => return Ok(Step::Ended(Err(format!("{}, {}", before, reason)))),
    };
    settle(work)?;
    settle(host)?;
    let timed = match repetition(&invocation, cold, scratch, environment)? {
        Ok(timed) => timed,
        Err(ended) => return Ok(Step::Ended(Ok(ended))),
    };
    let refs_after = match refs(&copy, scratch, environment)? {
        Ok(refs) => refs,
        Err(reason) => return Ok(Step::Ended(Err(format!("after repetition {}, {}", rep, reason)))),
    };
    if let Err(reason) = moved(remote.transfer, copy.tool, &refs_before, &refs_after) {
        return Ok(Step::Ended(Ok(Measured::NothingMoved { reason: format!("repetition {}: {}", rep, reason) })));
    }
    if let Err(reason) = same(std::slice::from_ref(&copy), &Changes::default(), scratch, environment)? {
        return Ok(Step::Ended(Err(format!("after repetition {}, {}", rep, reason))));
    }
    Ok(Step::Ran(Some(timed)))
}

/// A subject's name at the start of a reason: its label, but Forklift's capitalised.
fn named(tool: Tool) -> &'static str {
    if tool == Tool::Forklift { "Forklift" } else { tool.label() }
}

/// Where [`PROBE_REF`], a subject's current branch, stands in the subject and in its remote: its
/// head, and the remote's branch of that name, none when the remote has no such branch.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Refs {
    local: String,
    remote: Option<String>,
}

/// Where a subject's work tree stands: its current branch, and the commit at its head.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Head {
    branch: String,
    commit: String,
}

/// The head of `subject`: git's by `symbolic-ref` and `rev-parse HEAD`, Forklift's by its pallet
/// list. Gives why when it could not be read.
fn head(subject: &Subject, scratch: &Scratch, environment: &Environment) -> Result<Result<Head, String>, Stopped> {
    if subject.tool != Tool::Forklift {
        let branch = or_return!(output(subject, &["symbolic-ref", "--short", "HEAD"], scratch, environment)?).trim().to_string();
        let commit = or_return!(output(subject, &["rev-parse", "HEAD"], scratch, environment)?).trim().to_string();
        return Ok(Ok(Head { branch, commit }));
    }
    let listed = runner::document(or_return!(output(subject, &["--json", "pallet", "list"], scratch, environment)?).as_bytes());
    let Some(branch) = listed["data"]["current"].as_str().map(str::to_string) else {
        return Ok(Err(format!("Forklift's --json pallet list named {} current, not a pallet", listed["data"]["current"])));
    };
    let pallets = listed["data"]["pallets"].as_array();
    let Some(commit) = pallets.and_then(|pallets| pallets.iter().find(|pallet| pallet["name"] == branch.as_str())).and_then(|pallet| pallet["head"].as_str()).map(str::to_string) else {
        return Ok(Err(format!("Forklift's --json pallet list named no head for {}", branch)));
    };
    Ok(Ok(Head { branch, commit }))
}

/// Whether a clone landed on `expected`: the branch it names, at its commit. Gives why not.
fn landed_on(tool: Tool, expected: &Head, landed: &Head) -> Result<(), String> {
    if landed == expected {
        return Ok(());
    }
    Err(format!("{}'s clone checked out {} at {}, not {} at the subject's head {}", named(tool), landed.branch, landed.commit, expected.branch, expected.commit))
}

/// The refs of `subject` and its remote: its [`head`], and the remote's branch by `ls-remote` for
/// git's, by the remote's pallet list for Forklift's. Gives why when either could not be read, or
/// the subject is not on [`PROBE_REF`].
fn refs(subject: &Subject, scratch: &Scratch, environment: &Environment) -> Result<Result<Refs, String>, Stopped> {
    let at = or_return!(head(subject, scratch, environment)?);
    if at.branch != PROBE_REF {
        return Ok(Err(format!("{} is on {}, not {}", named(subject.tool), at.branch, PROBE_REF)));
    }
    let local = at.commit;
    if subject.tool != Tool::Forklift {
        let listed = or_return!(output(subject, &["ls-remote", "origin", &format!("refs/heads/{}", PROBE_REF)], scratch, environment)?);
        return Ok(Ok(Refs { local, remote: listed.split_whitespace().next().map(str::to_string) }));
    }
    let named = |document: &serde_json::Value, field: &str| -> Option<serde_json::Value> {
        document["data"]["pallets"].as_array()?.iter().find(|pallet| pallet["name"] == PROBE_REF).map(|pallet| pallet[field].clone())
    };
    let remote = runner::document(or_return!(output(subject, &["--json", "pallet", "list", "--remote"], scratch, environment)?).as_bytes());
    if !remote["data"]["pallets"].is_array() {
        return Ok(Err("Forklift's --json pallet list --remote gave no pallets".to_string()));
    }
    let remote = match named(&remote, "remote_head") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(head)) => Some(head),
        Some(other) => return Ok(Err(format!("Forklift's --json pallet list --remote gave {} as the remote head of {}", other, PROBE_REF))),
    };
    Ok(Ok(Refs { local, remote }))
}

/// Whether a push or a pull carried the head across, by the refs read before and after it: a push
/// must have changed the remote's branch, to the subject's head, and a pull the subject's head, to
/// the remote's branch. Gives why not.
fn moved(transfer: Transfer, tool: Tool, before: &Refs, after: &Refs) -> Result<(), String> {
    let label = named(tool);
    let shown = |head: &Option<String>| head.clone().unwrap_or_else(|| "no such branch".to_string());
    let across = after.remote.as_deref() == Some(after.local.as_str());
    match transfer {
        Transfer::Push if before.remote == after.remote => Err(format!("{}'s remote held {} before the push and after it, so the push moved nothing", label, shown(&after.remote))),
        Transfer::Push if !across => Err(format!("after the push, {}'s remote held {}, not the subject's head {}", label, shown(&after.remote), after.local)),
        Transfer::Pull if before.local == after.local => Err(format!("{}'s head was {} before the pull and after it, so the pull moved nothing", label, after.local)),
        Transfer::Pull if !across => Err(format!("after the pull, {}'s head was {}, not its remote's {}", label, after.local, shown(&after.remote))),
        _ => Ok(()),
    }
}

/// Where the side branch stands against the current branch in one subject.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Side {
    /// At the commit the current branch is at.
    Same,
    /// On one line of history with it: one of the two contains the other.
    OneLine,
    /// Each has commits the other lacks.
    Forked,
    /// At another commit, on what line of history could not be read, and why.
    Unread(String),
}

/// Whether a pair can be timed, or why not.
type Comparable = Result<(), String>;

/// Whether switching to the side branch `name` changes anything in every subject, and whether
/// merging it joins two lines of history in every subject, as each tool reads its own history, or
/// why not.
fn sides(subjects: &[Subject], name: &str, scratch: &Scratch, environment: &Environment) -> Result<(Comparable, Comparable), Stopped> {
    let (mut switch, mut merge) = (Vec::new(), Vec::new());
    for subject in subjects {
        let label = subject.tool.label();
        match side(subject, name, scratch, environment)? {
            Err(reason) => {
                switch.push(reason.clone());
                merge.push(reason);
            }
            Ok(Side::Same) => {
                let reason = format!("{} has {} at the commit its current branch is at, so switching to it or merging it would time a change of nothing", label, name);
                switch.push(reason.clone());
                merge.push(reason);
            }
            Ok(Side::Unread(reason)) => merge.push(reason),
            Ok(Side::OneLine) => merge.push(format!("{} has {} and its current branch on one line of history, one containing the other, so merging it would not join two lines", label, name)),
            Ok(Side::Forked) => {}
        }
    }
    let verdict = |not: Vec<String>| if not.is_empty() { Ok(()) } else { Err(not.join("; ")) };
    Ok((verdict(switch), verdict(merge)))
}

/// Where the side branch `name` stands in `subject`: git's by `rev-list --left-right --count`,
/// Forklift's by its pallets' heads and the history of each. Gives why when the heads could not be
/// read; a history that could not be read leaves the heads apart, [`Side::Unread`].
fn side(subject: &Subject, name: &str, scratch: &Scratch, environment: &Environment) -> Result<Result<Side, String>, Stopped> {
    let label = subject.tool.label();
    if subject.tool != Tool::Forklift {
        let range = format!("HEAD...refs/heads/{}", name);
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
    let Some((current, side)) = head(&|pallet| pallet["current"] == true).zip(head(&|pallet| pallet["name"] == name)) else {
        return Ok(Err(format!("{} named no head for the current branch or for {}", label, name)));
    };
    if current == side {
        return Ok(Ok(Side::Same));
    }
    for (from, to) in [(&side, &current), (&current, &side)] {
        match reaches(subject, from, to, scratch, environment)? {
            Ok(true) => return Ok(Ok(Side::OneLine)),
            Ok(false) => {}
            Err(reason) => return Ok(Ok(Side::Unread(reason))),
        }
    }
    Ok(Ok(Side::Forked))
}

/// Whether `target` is in Forklift's history of `revision`, itself included, read page by page
/// until it turns up. Gives why when a page could not be read, or a page's cursor came round again.
fn reaches(subject: &Subject, revision: &str, target: &str, scratch: &Scratch, environment: &Environment) -> Result<Result<bool, String>, Stopped> {
    let mut seen = BTreeSet::new();
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
            if parcel == target {
                return Ok(Ok(true));
            }
        }
        match document["data"]["next"].as_str() {
            Some(next) if !seen.insert(next.to_string()) => return Ok(Err(format!("Forklift's --json history {} gave the cursor {} twice", revision, next))),
            Some(next) => after = Some(next.to_string()),
            None => return Ok(Ok(false)),
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

/// A folder the harness copied a subject into. [`Copied::remove`] removes it and says when it
/// could not; one dropped without it, on a panic, is removed as far as it can be.
struct Copied(Option<PathBuf>);

impl Copied {
    fn path(&self) -> &Path {
        self.0.as_deref().expect("a copy is removed only by consuming it")
    }

    /// Removes the copy, or stops the run naming it: copies left behind would fill the disk with
    /// a full tree each.
    fn remove(self) -> Result<(), Stopped> {
        self.removed(false)
    }

    /// As [`Copied::remove`], for a folder a timed command makes, which it may not have: a clone
    /// that failed before making its folder leaves nothing to remove.
    fn remove_if_made(self) -> Result<(), Stopped> {
        self.removed(true)
    }

    fn removed(mut self, may_be_missing: bool) -> Result<(), Stopped> {
        let path = self.0.take().expect("a copy is removed only once");
        #[allow(clippy::disallowed_methods, reason = "a scratch copy the harness made itself, under its own output directory")]
        match std::fs::remove_dir_all(&path) {
            Err(error) if may_be_missing && error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Stopped(format!("removing the copy {}: {}", path.display(), error))),
            Ok(()) => Ok(()),
        }
    }
}

impl Drop for Copied {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            #[allow(clippy::disallowed_methods, reason = "a scratch copy the harness made itself, under its own output directory")]
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Gives every file in the copy `to` the modification time its original under `from` has, and
/// on unix every folder too, after its contents. A copy that is newer than the tool's last look at
/// its files reads as possibly edited: Forklift's stat cache does not trust a stat within its
/// anchor's two seconds, and git's index does not trust one as new as the index, so each would
/// hash files inside the timed write that the subject's own caches had settled. It walks the copy,
/// so a file the original gained after the copy (a running daemon's cookie) is not looked for; one
/// the original lost since is left with the copy's time.
fn keep_times(from: &Path, to: &Path) -> Result<(), Stopped> {
    fn failed(path: &Path) -> impl Fn(std::io::Error) -> Stopped + '_ {
        move |error| Stopped(format!("keeping the times of {}: {}", path.display(), error))
    }
    let metadata = std::fs::symlink_metadata(to).map_err(failed(to))?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in std::fs::read_dir(to).map_err(failed(to))? {
            let entry = entry.map_err(failed(to))?;
            keep_times(&from.join(entry.file_name()), &entry.path())?;
        }
    }
    let original = match std::fs::symlink_metadata(from) {
        Ok(original) => original,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(failed(from)(error)),
    };
    let time = original.modified().map_err(failed(from))?;
    if metadata.is_dir() {
        #[cfg(unix)]
        std::fs::File::open(to).and_then(|folder| folder.set_modified(time)).map_err(failed(to))?;
        return Ok(());
    }
    let mut options = std::fs::OpenOptions::new();
    // Windows sets a time only through a handle opened to write attributes, which a read-only
    // file still grants; elsewhere any handle the owner opens will do.
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::access_mode(&mut options, runner::FILE_WRITE_ATTRIBUTES);
    #[cfg(not(windows))]
    options.read(true);
    options.open(to).and_then(|file| file.set_modified(time)).map_err(failed(to))
}

/// The untimed run that brings a fresh copy's caches up to date with its copied files, as git's
/// status refreshes its index: Forklift's `load .`, since its stocktake keeps nothing it learns
/// about a file, and git's `status`. Gives why when it did not succeed, since a copy whose caches
/// were not brought up to date would still read clean.
fn refresh_untimed(copy: &Subject, scratch: &Scratch, environment: &Environment) -> Result<Result<(), String>, Stopped> {
    let refresh: &[&str] = if copy.tool == Tool::Forklift { &["load", "."] } else { &["status"] };
    Ok(output(copy, refresh, scratch, environment)?.map(|_| ()))
}

/// Writes the copy at `root` out to disk, untimed, so that a timed write which syncs pays for its
/// own writes and not for the copy's: sync(2) on unix, and on macOS, whose sync(2) may return
/// before the writes are done and does not flush the drive's cache, an fsync of every file and
/// then one full flush. On Windows it does nothing.
fn settle(root: &Path) -> Result<(), Stopped> {
    #[cfg(unix)]
    // SAFETY: sync(2) takes no arguments and has no failure to report.
    unsafe {
        libc::sync();
    }
    #[cfg(target_os = "macos")]
    {
        fn each(path: &Path, last: &mut Option<std::fs::File>) -> std::io::Result<()> {
            let metadata = std::fs::symlink_metadata(path)?;
            if metadata.is_dir() {
                for entry in std::fs::read_dir(path)? {
                    each(&entry?.path(), last)?;
                }
            } else if metadata.is_file() {
                let file = std::fs::File::open(path)?;
                // SAFETY: fsync(2) on a descriptor this function owns for the call.
                if unsafe { libc::fsync(std::os::fd::AsRawFd::as_raw_fd(&file)) } != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                *last = Some(file);
            }
            Ok(())
        }
        let failed = |error: std::io::Error| Stopped(format!("writing the copy {} out to disk: {}", root.display(), error));
        let mut last = None;
        each(root, &mut last).map_err(failed)?;
        if let Some(file) = last {
            // SAFETY: fcntl(2) F_FULLFSYNC on a descriptor this function owns; it flushes the
            // drive's cache, which holds every file fsync(2) wrote, not only this one's.
            if unsafe { libc::fcntl(std::os::fd::AsRawFd::as_raw_fd(&file), libc::F_FULLFSYNC) } == -1 {
                return Err(failed(std::io::Error::last_os_error()));
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = root;
    Ok(())
}

/// What one repetition of a write cell came to.
enum Step {
    /// It ran: timed, or the untimed run first.
    Ran(Option<tool::Timed>),
    /// The cell ends here: voided or killed, or not comparable and why.
    Ended(Result<Measured, String>),
}

/// One write cell: each repetition on a fresh copy of `subject` as `fresh` describes it, watched
/// by a daemon of its own where the subject is, and judged clean after the timed run. A warm cell
/// first runs the command once untimed and unjudged, on a copy of its own, as a warm read cell
/// does. Every copy is removed after its repetition, however the repetition ended. Gives why the
/// pair is not comparable when a copy's refresh or check failed.
fn fresh_cell(subject: &Subject, command: &[String], reps: usize, cold: Option<&DropCaches>, fresh: &Fresh, scratch: &Scratch, environment: &Environment) -> Result<Result<Measured, String>, Stopped> {
    let cache = if cold.is_some() { Cache::Cold } else { Cache::Warm };
    let first = if cold.is_some() { 1 } else { 0 };
    let mut runs = Vec::with_capacity(reps);
    for rep in first..=reps {
        let name = format!("{}-{}-{}-{}", fresh.operation.replace(' ', "-"), cache.label(), subject.tool.label(), rep);
        let copied = Copied(Some(fresh.copies.join(name)));
        let step = fresh_rep(subject, command, rep, cold, fresh, copied.path(), scratch, environment);
        let removed = copied.remove();
        let step = step?;
        removed?;
        match step {
            Step::Ran(timed) => runs.extend(timed),
            Step::Ended(ended) => return Ok(ended),
        }
    }
    Ok(Ok(summarise(&runs)))
}

/// Repetition `rep` of a write cell, on a fresh copy of `subject` at `at`; repetition 0 is the
/// untimed run.
#[allow(clippy::too_many_arguments)]
fn fresh_rep(subject: &Subject, command: &[String], rep: usize, cold: Option<&DropCaches>, fresh: &Fresh, at: &Path, scratch: &Scratch, environment: &Environment) -> Result<Step, Stopped> {
    let before = if rep == 0 { "before the untimed run".to_string() } else { format!("before repetition {}", rep) };
    crate::scrub::copy_tree(&subject.work, at).map_err(|error| Stopped(format!("copying {} to {}: {:?}", subject.work.display(), at.display(), error)))?;
    keep_times(&subject.work, at)?;
    let copy = subject.at(at.to_path_buf());
    // Dropped, when this returns, before the copy is removed. The copy's fsmonitor setting came
    // with it.
    let _daemon = if copy.watched {
        Some(runner::FsmonitorDaemon::watch(&copy.program, &copy.work, scratch, environment, COMMAND_LIMIT).map_err(|error| Stopped(error.to_string()))?.0)
    } else {
        None
    };
    if let Err(reason) = refresh_untimed(&copy, scratch, environment)? {
        return Ok(Step::Ended(Err(format!("{}, the refresh failed: {}", before, reason))));
    }
    if !fresh.edit.is_empty() {
        edit(&copy.work, fresh.edit)?;
        stage(&copy, scratch, environment)?;
    }
    let invocation = copy.invocation(command.to_vec());
    if rep == 0 {
        settle(&copy.work)?;
        return match tool::run(&invocation, scratch, environment) {
            Ok(_) => Ok(Step::Ran(None)),
            Err(ToolError::TimedOut { .. }) => Ok(Step::Ended(Ok(killed(invocation.limit)))),
            Err(error) => Err(error.into()),
        };
    }
    if let Err(reason) = same(std::slice::from_ref(&copy), fresh.before, scratch, environment)? {
        return Ok(Step::Ended(Err(format!("{}, {}", before, reason))));
    }
    settle(&copy.work)?;
    let timed = match repetition(&invocation, cold, scratch, environment)? {
        Ok(timed) => timed,
        Err(ended) => return Ok(Step::Ended(Ok(ended))),
    };
    if let Err(reason) = same(std::slice::from_ref(&copy), &Changes::default(), scratch, environment)? {
        return Ok(Step::Ended(Err(format!("after repetition {}, {}", rep, reason))));
    }
    Ok(Step::Ran(Some(timed)))
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
            seed: Some(1), shape: "small".into(), shape_detail: "detail".into(), side_branch: Some("perf-side".into()), reps: 3, tools: BTreeMap::new(), platform: Platform::Linux,
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
        let verdict = judge(&rows, "small", Platform::Linux, Gate::All, &[]);
        PerfReport { format: PERF_FORMAT, header, rows, verdict }
    }

    /// A warm row timed on Forklift and tuned git, each at its own median, fastest and slowest.
    fn paired(operation: &str, forklift: [u64; 3], tuned: [u64; 3]) -> Row {
        let cell = |subject: Tool, [median, min, max]: [u64; 3]| Cell { subject, command: vec![], result: Measured::Timed {
            wall: Spread { median, min, max }, memory: Memory::NotMeasured { reason: "r".into() }, cores: Cores::NotMeasured { reason: "r".into() },
        } };
        Row { operation: operation.into(), cache: Cache::Warm, note: "n".into(), outcome: Outcome::Timed { cells: vec![cell(Tool::Forklift, forklift), cell(Tool::GitTuned, tuned)] } }
    }

    /// A warm gate-subject row timed on Forklift alone.
    fn alone(operation: &str, median: u64) -> Row {
        Row { operation: operation.into(), cache: Cache::Warm, note: "n".into(), outcome: Outcome::Timed { cells: vec![Cell { subject: Tool::Forklift, command: vec![], result: Measured::Timed {
            wall: Spread { median, min: median, max: median }, memory: Memory::NotMeasured { reason: "r".into() }, cores: Cores::NotMeasured { reason: "r".into() },
        } }] } }
    }

    fn failures(verdict: &Verdict) -> Vec<(&str, Check)> {
        verdict.failures.iter().map(|failure| (failure.operation.as_str(), failure.check)).collect()
    }

    /// Under either gate a gross row fails at more than ten times tuned git's median and passes
    /// at nine, and names the row; Forklift's own ratios fail the repack past 1.5 times the fresh
    /// compact and the audit past twice the one-core audit; a gross or gate-subject row that could
    /// not run fails, a row not run never does.
    ///
    /// Revert: judge gross rows under all alone → 11× status passes self; a gross bound of 8 → 9×
    /// fails.
    #[test]
    fn the_self_gate_fails_a_gross_regression_and_a_broken_own_ratio() {
        let fast = [1_000, 900, 1_100];
        let judged = |rows: &[Row]| judge(rows, "small", Platform::Linux, Gate::SelfOnly, &[]);

        let verdict = judged(&[paired("status", [11_000, 10_500, 11_500], fast)]);
        assert_eq!(failures(&verdict), [("status", Check::Gross)]);
        assert_eq!(verdict.failures[0].detail, "Forklift's median 11.00 ms is more than 10 times tuned git's 1.00 ms");
        let verdict = judged(&[paired("status", [9_000, 8_500, 9_500], fast)]);
        assert_eq!(failures(&verdict), []);
        // Clearly slower, so reported, but that alone fails only the all gate.
        assert_eq!(verdict.slower.iter().map(|slower| slower.operation.as_str()).collect::<Vec<_>>(), ["status"]);
        // Log is no gross row: eleven times is reported, never failed, under self.
        assert_eq!(failures(&judged(&[paired("log", [11_000, 10_500, 11_500], fast)])), []);

        let own = |repack: u64, audit: u64| failures(&judged(&[alone(FRESH_COMPACT, 1_000), alone(REPACK, repack), alone(AUDIT, audit), alone(AUDIT_ONE_CORE, 1_000)])).into_iter().map(|(operation, check)| (operation.to_string(), check)).collect::<Vec<_>>();
        assert_eq!(own(1_400, 1_900), []);
        assert_eq!(own(1_600, 2_100), [(REPACK.to_string(), Check::SelfRatio), (AUDIT.to_string(), Check::SelfRatio)]);

        let unrun = |operation: &str, outcome: Outcome| Row { operation: operation.into(), cache: Cache::Warm, note: "n".into(), outcome };
        let not_comparable = Outcome::NotComparable { reason: "why".into() };
        let verdict = judged(&[unrun("diff", not_comparable.clone()), unrun(AUDIT, not_comparable.clone()), unrun("log", not_comparable), unrun("switch", Outcome::NotRun { reason: "r".into() })]);
        assert_eq!(failures(&verdict), [("diff", Check::CouldNotRun), (AUDIT, Check::CouldNotRun)]);
        assert_eq!(verdict.failures[0].detail, "not comparable: why");

        // A timed row missing a subject it needs was never compared: a gross row with no tuned git
        // cell, and a gate-subject row with no cell at all, fail; log, ungated under self, does not.
        let Outcome::Timed { cells } = paired("status", fast, fast).outcome else { unreachable!() };
        let forklift_only = Outcome::Timed { cells: cells[..1].to_vec() };
        let verdict = judged(&[unrun("status", forklift_only.clone()), unrun(REPACK, Outcome::Timed { cells: vec![] }), unrun("log", forklift_only)]);
        assert_eq!(failures(&verdict), [("status", Check::CouldNotRun), (REPACK, Check::CouldNotRun)]);
        assert_eq!((verdict.failures[0].detail.as_str(), verdict.failures[1].detail.as_str()), ("it has no git-tuned cell", "it has no forklift cell"));
    }

    /// Under all, a clearly slower row with no ticket fails and one with a ticket for its
    /// operation, shape, cache and platform does not; a ticket whose row was timed and is not
    /// clearly slower is reported as possibly closable and fails nothing, and a row not clearly
    /// slower by its spread (its fastest within tuned git's slowest) is neither. Any row that could
    /// not run fails under all; a row not run never does.
    ///
    /// Revert: never fail an untracked row → the missing ticket passes; take a ticket as clearly
    /// slower → the stale one is not reported closable.
    #[test]
    fn the_all_gate_fails_an_untracked_slower_row_and_reports_a_stale_ticket() {
        let fast = [1_000, 900, 1_100];
        let ticket = |operation: &str, os: Platform| Ticket { operation: operation.into(), shape: "small".into(), cache: Cache::Warm, os, ticket: "FORK-1".into() };
        let rows = [paired("log", [2_000, 1_900, 2_100], fast), paired("blame", [1_100, 950, 1_200], fast), paired("commit", [1_400, 1_000, 1_500], fast)];

        let verdict = judge(&rows, "small", Platform::Linux, Gate::All, &[]);
        assert_eq!(failures(&verdict), [("log", Check::Untracked)]);
        assert!(verdict.failures[0].detail.contains("perf-tickets.json names no ticket for it"), "{}", verdict.failures[0].detail);
        // Commit's median is 1.4 times, but its fastest is within tuned git's slowest.
        assert_eq!(verdict.slower.iter().map(|slower| slower.operation.as_str()).collect::<Vec<_>>(), ["log"]);

        let tracked = [ticket("log", Platform::Linux), ticket("blame", Platform::Linux)];
        let verdict = judge(&rows, "small", Platform::Linux, Gate::All, &tracked);
        assert_eq!(failures(&verdict), []);
        assert_eq!(verdict.slower[0].ticket.as_deref(), Some("FORK-1"));
        assert_eq!(verdict.closable, [ticket("blame", Platform::Linux)]);
        // A ticket for another platform or shape tracks nothing here.
        assert_eq!(failures(&judge(&rows, "small", Platform::Macos, Gate::All, &tracked)), [("log", Check::Untracked)]);
        assert_eq!(failures(&judge(&rows, "medium", Platform::Linux, Gate::All, &tracked)), [("log", Check::Untracked)]);
        // A ticket for a row this run did not time is not reported either way.
        assert_eq!(judge(&rows, "small", Platform::Linux, Gate::All, &[ticket("pull", Platform::Linux)]).closable, []);

        let log = Row { operation: "log".into(), cache: Cache::Cold, note: "n".into(), outcome: Outcome::NotComparable { reason: "why".into() } };
        let not_run = Row { operation: "pull".into(), cache: Cache::Warm, note: "n".into(), outcome: Outcome::NotRun { reason: "r".into() } };
        assert_eq!(failures(&judge(&[log, not_run], "small", Platform::Linux, Gate::All, &[])), [("log", Check::CouldNotRun)]);
    }

    /// The tickets this build ships parse, name a FORK ticket, a shape this build has and a row
    /// it times, and track each row at most once.
    #[test]
    fn the_shipped_tickets_name_real_rows_once() {
        let operations = [
            "status", "log", "blame", "verify", "commit", "switch", "merge", "pack", "clone", "push", "pull", "diff", "diff --staged",
            "fresh compact", "repack", "audit main", "audit main, one core",
        ];
        let tickets = tickets().unwrap();
        let mut seen = BTreeSet::new();
        for ticket in &tickets {
            assert!(ticket.ticket.strip_prefix("FORK-").is_some_and(|number| !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())), "{:?}", ticket);
            assert!(Shape::named(&ticket.shape).is_some(), "{:?}", ticket);
            assert!(operations.contains(&ticket.operation.as_str()), "{:?}", ticket);
            assert!(seen.insert((ticket.operation.clone(), ticket.shape.clone(), ticket.cache, ticket.os)), "tracked twice: {:?}", ticket);
        }
        let parsed: Result<Vec<Ticket>, _> = serde_json::from_str(r#"[{"operation":"log","shape":"small","cache":"warm","os":"linux","ticket":"FORK-1","note":"x"}]"#);
        assert!(parsed.is_err(), "an unknown field is refused");
    }

    /// The options name one history: two are refused, a side branch without a real history is,
    /// a shape this build lacks is, and a history that is no folder is; no shape and no history
    /// is the small shape.
    #[test]
    fn perf_options_name_one_history() {
        let options = |shape: Option<&str>, history: Option<&str>, side: Option<&str>| PerfOptions {
            out: PathBuf::from("out"), forklift: PathBuf::from("f"), git: PathBuf::from("g"), shape: shape.map(str::to_string),
            history_from: history.map(PathBuf::from), side_branch: side.map(str::to_string), reps: 1, gate: None, drop_caches: None, forklift_server: None,
        };
        assert_eq!(source(&options(None, None, None)), Ok(Source::Generated(Shape::SMALL)));
        assert_eq!(source(&options(Some("huge"), None, None)), Ok(Source::Generated(Shape::HUGE)));
        assert_eq!(source(&options(Some("vast"), None, None)), Err("--shape vast: not a shape this build has (it has small, medium, huge)".into()));
        assert_eq!(source(&options(Some("small"), Some("."), None)), Err("--shape and --history-from each name the history to time on; give one".into()));
        assert_eq!(source(&options(None, None, Some("s"))), Err("--side-branch names a branch of --history-from's history; a generated shape has its own, perf-side".into()));
        let here = std::env::current_dir().unwrap();
        let Ok(Source::History(history)) = source(&options(None, Some("."), Some("s"))) else { panic!("a real history") };
        assert_eq!((std::fs::canonicalize(&history.from).unwrap(), history.side.as_deref()), (std::fs::canonicalize(&here).unwrap(), Some("s")));
        let judged = |gate: Option<Gate>| source(&PerfOptions { gate, ..options(None, Some("."), None) }).map(|_| ());
        assert_eq!((judged(None), judged(Some(Gate::SelfOnly))), (Ok(()), Ok(())));
        assert_eq!((gate_of(&options(None, Some("."), None)), gate_of(&options(Some("small"), None, None)), gate_of(&options(None, None, None))), (Gate::SelfOnly, Gate::All, Gate::All));
        assert_eq!(gate_of(&PerfOptions { gate: Some(Gate::SelfOnly), ..options(None, None, None) }), Gate::SelfOnly);
        assert_eq!(judged(Some(Gate::All)), Err("--gate all fails a clearly slower row no ticket tracks, and a ticket names a generated shape, so none can track a row of --history-from's history; judge it with --gate self".into()));
        let missing = here.join("no-such-folder-here");
        assert_eq!(source(&options(None, Some(&missing.display().to_string()), None)), Err(format!("--history-from {}: not a folder", missing.display())));
    }

    /// The four files `perf` replaced are named, outside this file, only on the lines kept as
    /// dated records of them: every tracked line the sweep finds is one kept line, and every kept
    /// line is found once. Kept lines are keyed by their file and a fragment of their text, never
    /// by number, since DESIGN.html's changelog moves down with each new row. The sweep matches the
    /// names as they are spelled, so a pattern that escapes one, as a regex does, passes it.
    ///
    /// Revert: drop a kept entry → its line is named as a referrer; name a retired file on a new
    /// line → that line is.
    #[test]
    fn the_retired_benchmarks_are_named_only_where_kept() {
        const KEPT: [(&str, &str); 9] = [
            ("CHANGELOG.md", "**The benchmark scripts"),
            ("docs/DESIGN.html", "Performance is measured by <code>proof-load perf</code> alone"),
            ("docs/DESIGN.html", "§5.0 milestone D, item 5 shipped — a benchmark regression job in CI (T5)."),
            ("docs/PERFORMANCE_BACKLOG.md", "taken against the Rosetta git 2.33.0. Later the same day (2026-07-15)"),
            ("docs/PERFORMANCE_BACKLOG.md", "branch or expected;"),
            ("docs/PERFORMANCE_BACKLOG.md", "was re-run in full on 2026-10-17 (hyperfine"),
            ("docs/PERFORMANCE_BACKLOG.md", "**Baseline** ("),
            ("docs/PERFORMANCE_BACKLOG.md", "**After** ("),
            ("docs/PERFORMANCE_BACKLOG.md", "**Where the benchmark stands** ("),
        ];
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let output = std::process::Command::new("git").current_dir(&root)
            .args(["grep", "-n", "-I", "-E", r"bench\.s[h]|bin/benchmar[k]|BENCHMARK\.md|bench\.ym[l]", "--", ".", ":(exclude)crates/proof-load/src/perf.rs"])
            .output().unwrap();
        assert!(output.status.success() || (output.status.code() == Some(1) && output.stderr.is_empty()), "{}", String::from_utf8_lossy(&output.stderr));
        let hits = String::from_utf8(output.stdout).unwrap();
        let mut found = [0usize; KEPT.len()];
        let mut referrers = Vec::new();
        for hit in hits.lines() {
            let mut fields = hit.splitn(3, ':');
            let (file, text) = (fields.next().unwrap(), fields.nth(1).unwrap_or(""));
            let kept: Vec<usize> = (0..KEPT.len()).filter(|&at| KEPT[at].0 == file && text.contains(KEPT[at].1)).collect();
            match kept[..] {
                [at] => found[at] += 1,
                _ => referrers.push(hit.chars().take(160).collect::<String>()),
            }
        }
        assert_eq!(referrers, Vec::<String>::new(), "these lines name a file perf replaced; name proof-load perf or its guide instead");
        let missing: Vec<_> = KEPT.iter().zip(found).filter(|(_, count)| *count != 1).collect();
        assert_eq!(missing, [], "each kept line is found exactly once");
    }

    /// A perf.json reads back as written; one naming another format number, or none, is refused by
    /// that, before any field is read; one with a field this build does not know is refused too.
    #[test]
    fn a_perf_reader_refuses_a_format_it_does_not_know() {
        let report = report();
        assert_eq!(PerfReport::from_json(&report.json()).unwrap(), report);

        let mut value: serde_json::Value = serde_json::from_str(&report.json()).unwrap();
        value["format"] = serde_json::json!(1);
        assert_eq!(PerfReport::from_json(&value.to_string()).unwrap_err(), "perf report format 1 is not one this build reads (it reads 4)");
        value.as_object_mut().unwrap().remove("format");
        assert_eq!(PerfReport::from_json(&value.to_string()).unwrap_err(), "not a perf report: it names no format");

        let mut value: serde_json::Value = serde_json::from_str(&report.json()).unwrap();
        value["rows"][0]["outcome"]["cells"][0]["result"]["guessed"] = serde_json::json!(true);
        assert!(PerfReport::from_json(&value.to_string()).unwrap_err().starts_with("not a perf report of format 4: "));
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
        let warm = paired("log", [1_000, 900, 1_100], [1_000, 900, 1_100]).outcome;
        both(&mut rows, "log", "n", Ok(()), &Err("why".into()), |cold| { timed.push(cold.cloned()); Ok(warm.clone()) }).unwrap();
        assert_eq!(timed, [None]);
        assert_eq!(rows.iter().map(|row| (row.cache, &row.outcome)).collect::<Vec<_>>(), [
            (Cache::Warm, &warm), (Cache::Cold, &Outcome::NotRun { reason: "why".into() }),
        ]);
        let mut report = report();
        report.verdict = judge(&rows, "small", Platform::Linux, Gate::All, &[]);
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

    /// A copy gets back every file's and every folder's modification time, whatever time
    /// the copy itself left. On macOS a copy keeps them anyway, so the copy's times are moved here
    /// first; on Linux the write-pair test sees the wiring too, through a hook in the copy.
    #[cfg(unix)]
    #[test]
    fn a_copy_keeps_its_files_times() {
        let (root, scratch, _) = scratch("keep-times");
        let from = scratch.work().join("from");
        let to = scratch.work().join("to");
        std::fs::create_dir_all(from.join("folder")).unwrap();
        std::fs::write(from.join("folder").join("file"), "f\n").unwrap();
        let then = std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let set = |path: &Path, time| std::fs::File::options().write(path.is_file()).read(true).open(path).unwrap().set_modified(time).unwrap();
        set(&from.join("folder").join("file"), then);
        set(&from.join("folder"), then);
        crate::scrub::copy_tree(&from, &to).unwrap();
        // The original gains a file after the copy, as a daemon's cookie, and the copy has one the
        // original lacks: neither stops it.
        std::fs::write(from.join("folder").join("cookie"), "").unwrap();
        set(&from.join("folder"), then);
        std::fs::write(to.join("folder").join("only-here"), "").unwrap();
        let now = std::time::SystemTime::now();
        set(&to.join("folder").join("file"), now);
        set(&to.join("folder"), now);
        keep_times(&from, &to).unwrap();
        let modified = |path: PathBuf| std::fs::metadata(path).unwrap().modified().unwrap();
        assert_eq!(modified(to.join("folder").join("file")), then);
        assert_eq!(modified(to.join("folder")), then);
        #[allow(clippy::disallowed_methods)] // a test's own temporary folder
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Forklift's side is read from its pallets' heads and the history of each, page by page: one
    /// head in the other's history is one line, neither is forked, and a history that cannot be
    /// read, or whose cursor comes round again, refuses merge alone, since switch needs only the
    /// heads. Against a stand-in that prints what Forklift's --json pallet list and history would.
    #[cfg(unix)]
    #[test]
    fn forklifts_side_is_read_from_its_heads_and_paged_history() {
        let (root, scratch, environment) = scratch("forklift-side");
        let folder = scratch.work().to_path_buf();
        let page = folder.join("page");
        let program = folder.join("forklift");
        std::fs::write(&program, format!(r#"#!/bin/sh
case "$*" in
"--json pallet list") echo '{{"data":{{"pallets":[{{"name":"main","current":true,"head":"m2"}},{{"name":"perf-side","current":false,"head":"s1"}}]}}}}';;
"--json history s1") echo '{{"data":{{"entries":[{{"parcel":"s1"}},{{"parcel":"b"}}],"next":"c1"}}}}';;
"--json history s1 --after c1") echo '{{"data":{{"entries":[{{"parcel":"a"}}]}}}}';;
"--json history m2") echo '{{"data":{{"entries":[{{"parcel":"m2"}},{{"parcel":"m1"}}],"next":"c2"}}}}';;
"--json history m2 --after c2")
  n=$(cat '{0}/count' 2>/dev/null || echo 0); n=$((n + 1)); echo $n > '{0}/count'
  if [ -f '{0}/repeat' ] && [ $n -lt 5 ]; then echo '{{"data":{{"entries":[{{"parcel":"x"}}],"next":"c2"}}}}'; else cat '{0}/page'; fi;;
*) echo "no $*" >&2; exit 2;;
esac
"#, folder.display())).unwrap();
        std::fs::set_permissions(&program, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let subject = Subject { tool: Tool::Forklift, program, prefix: vec![], work: folder.clone(), watched: false };
        let read = |content: &str| {
            std::fs::write(&page, content).unwrap();
            let _ = std::fs::write(folder.join("count"), "0");
            side(&subject, SIDE_BRANCH, &scratch, &environment).unwrap()
        };

        assert_eq!(read(r#"{"data":{"entries":[{"parcel":"b"},{"parcel":"a"}]}}"#), Ok(Side::Forked));
        assert_eq!(read(r#"{"data":{"entries":[{"parcel":"s1"},{"parcel":"b"}]}}"#), Ok(Side::OneLine));
        assert_eq!(read("not json"), Ok(Side::Unread("Forklift's --json history m2 gave no entries".into())));
        std::fs::write(folder.join("repeat"), "").unwrap();
        assert_eq!(read(r#"{"data":{"entries":[]}}"#), Ok(Side::Unread("Forklift's --json history m2 gave the cursor c2 twice".into())));
        let (switch, merge) = sides(std::slice::from_ref(&subject), SIDE_BRANCH, &scratch, &environment).unwrap();
        assert_eq!(switch, Ok(()));
        assert_eq!(merge, Err("Forklift's --json history m2 gave the cursor c2 twice".into()));
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
            writes(&subjects, Some(SIDE_BRANCH), &["a", "b"], 1, &Err("r".into()), &scratch, &environment, &|_| {}, &mut rows).unwrap();
            rows.into_iter().map(|row| (row.operation, row.cache, row.outcome)).collect::<Vec<_>>()
        };
        let timed = |outcome: &Outcome| matches!(outcome, Outcome::Timed { cells } if cells.len() == 1 && matches!(cells[0].result, Measured::Timed { .. }));
        let not_run = Outcome::NotRun { reason: "r".into() };
        let outcomes = |rows: &[(String, Cache, Outcome)]| rows.iter().map(|(_, _, outcome)| outcome.clone()).collect::<Vec<_>>();

        let rows = writes_now();
        let same = Outcome::NotComparable { reason: "git has perf-side at the commit its current branch is at, so switching to it or merging it would time a change of nothing".into() };
        assert_eq!(rows.iter().map(|(operation, cache, _)| (operation.as_str(), *cache)).collect::<Vec<_>>(), [
            ("commit", Cache::Warm), ("commit", Cache::Cold), ("switch", Cache::Warm), ("switch", Cache::Cold), ("merge", Cache::Warm), ("merge", Cache::Cold),
            ("pack", Cache::Warm), ("pack", Cache::Cold),
        ]);
        assert!(timed(&rows[0].2), "{:?}", rows[0]);
        assert_eq!(rows[1].2, not_run);
        assert_eq!(outcomes(&rows[2..6]), [same.clone(), same.clone(), same.clone(), same]);
        assert!(timed(&rows[6].2), "{:?}", rows[6]);

        // With no side branch, switch and merge are not run, and say why; the other writes are
        // timed as before.
        let mut rows = Vec::new();
        writes(&subjects, None, &["a", "b"], 1, &Err("r".into()), &scratch, &environment, &|_| {}, &mut rows).unwrap();
        let no_side = Outcome::NotRun { reason: "no side branch given (--side-branch)".into() };
        assert_eq!(rows[2..6].iter().map(|row| row.outcome.clone()).collect::<Vec<_>>(), [no_side.clone(), no_side.clone(), no_side.clone(), no_side]);
        assert!(timed(&rows[0].outcome) && timed(&rows[6].outcome), "{:?}", rows);

        // The side branch ahead of the current one: a switch changes something, a merge would only
        // fast-forward.
        run(&["switch", "-q", SIDE_BRANCH]);
        commit_file("c");
        run(&["switch", "-q", "probe"]);
        let rows = writes_now();
        let one_line = Outcome::NotComparable { reason: "git has perf-side and its current branch on one line of history, one containing the other, so merging it would not join two lines".into() };
        assert!(timed(&rows[2].2), "{:?}", rows[2]);
        assert_eq!(outcomes(&rows[3..6]), [not_run.clone(), one_line.clone(), one_line]);

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
        // The hook also lists `d` if the copy's `d` is newer than a file made now: the subject's `d`
        // is set back years, and a copy keeps that time.
        let newer = scratch.work().join("newer");
        let now = scratch.work().join("now");
        std::fs::write(&now, "").unwrap();
        std::fs::File::options().write(true).open(work.join("d")).unwrap().set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_000)).unwrap();
        set_hook(&format!("echo ran >> '{}'; find d -newer '{}' >> '{}'", ran.display(), now.display(), newer.display()));
        assert!(writes_now().iter().all(|(_, cache, outcome)| *cache == Cache::Cold || timed(outcome)));
        assert_eq!(std::fs::read_to_string(&ran).unwrap(), "ran\nran\n");
        assert_eq!(std::fs::read_to_string(&newer).unwrap(), "");
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
        set_hook(":");

        // A copy that cannot be removed stops the run and says which, rather than leaving a tree
        // behind for every repetition, however its repetition ended: here after the untimed run,
        // and after a refresh that failed. Root removes it anyway, so not as root.
        let corrupt = || std::fs::write(work.join(".git").join("index"), "not an index").unwrap();
        // SAFETY: geteuid(2) takes no arguments and cannot fail.
        if unsafe { libc::geteuid() } != 0 {
            let locked = work.join("locked");
            std::fs::create_dir(&locked).unwrap();
            std::fs::write(locked.join("f"), "f\n").unwrap();
            run(&["add", "-A"]);
            run(&["commit", "-q", "-m", "locked"]);
            let mode = |folder: &Path, bits| std::fs::set_permissions(folder, std::os::unix::fs::PermissionsExt::from_mode(bits)).unwrap();
            mode(&locked, 0o555);
            let copy = scratch.work().join("copies").join("commit-warm-git-0");
            for broken in [false, true] {
                if broken {
                    corrupt();
                }
                let mut rows = Vec::new();
                let stopped = writes(&subjects, Some(SIDE_BRANCH), &["a", "b"], 1, &Err("r".into()), &scratch, &environment, &|_| {}, &mut rows).unwrap_err();
                assert_eq!(stopped.0, format!("removing the copy {}: Permission denied (os error 13)", copy.display()), "broken index: {}", broken);
                mode(&copy.join("locked"), 0o755);
                #[allow(clippy::disallowed_methods)] // a test's own temporary folder
                let _ = std::fs::remove_dir_all(&copy);
            }
            mode(&locked, 0o755);
        }

        // A copy whose refresh failed is not comparable, since it would still read clean.
        corrupt();
        let rows = writes_now();
        assert!(matches!(&rows[0].2, Outcome::NotComparable { reason } if reason.starts_with("before the untimed run, the refresh failed: git status exited Some(128): ")), "{:?}", rows[0]);
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

    /// The remote rows are not run on Windows, with or without a forklift-server, nor anywhere
    /// without one; elsewhere they are served by the one given.
    #[test]
    fn remote_rows_need_a_forklift_server_and_unix() {
        let server = PathBuf::from("/opt/bin/forklift-server");
        assert_eq!(remote_when(false, Some(server.clone())), Ok(server.clone()));
        assert_eq!(remote_when(false, None), Err("no forklift-server given (--forklift-server)".to_string()));
        let windows = Err("remote rows are not built on Windows in this harness, which stops a forklift-server gracefully only on unix".to_string());
        assert_eq!(remote_when(true, Some(server)), windows);
        assert_eq!(remote_when(true, None), windows);
    }

    /// A push is judged moved only when the remote's branch changed to the subject's head, and a
    /// pull only when the subject's head changed to the remote's branch.
    #[test]
    fn a_transfer_moved_something_only_when_the_head_crossed() {
        let refs = |local: &str, remote: Option<&str>| Refs { local: local.into(), remote: remote.map(str::to_string) };
        assert_eq!(moved(Transfer::Push, Tool::Git, &refs("h", Some("b")), &refs("h", Some("h"))), Ok(()));
        assert_eq!(moved(Transfer::Push, Tool::Forklift, &refs("h", None), &refs("h", Some("h"))), Ok(()));
        assert_eq!(moved(Transfer::Push, Tool::Git, &refs("h", Some("h")), &refs("h", Some("h"))),
            Err("git's remote held h before the push and after it, so the push moved nothing".to_string()));
        assert_eq!(moved(Transfer::Push, Tool::Forklift, &refs("h", None), &refs("h", None)),
            Err("Forklift's remote held no such branch before the push and after it, so the push moved nothing".to_string()));
        assert_eq!(moved(Transfer::Push, Tool::Git, &refs("h", Some("b")), &refs("h", Some("x"))),
            Err("after the push, git's remote held x, not the subject's head h".to_string()));
        assert_eq!(moved(Transfer::Pull, Tool::Git, &refs("b", Some("a")), &refs("a", Some("a"))), Ok(()));
        assert_eq!(moved(Transfer::Pull, Tool::GitTuned, &refs("b", Some("a")), &refs("b", Some("a"))),
            Err("git-tuned's head was b before the pull and after it, so the pull moved nothing".to_string()));
        assert_eq!(moved(Transfer::Pull, Tool::Forklift, &refs("b", Some("a")), &refs("c", Some("a"))),
            Err("after the pull, Forklift's head was c, not its remote's a".to_string()));
        let at = |branch: &str, commit: &str| Head { branch: branch.into(), commit: commit.into() };
        assert_eq!(landed_on(Tool::Forklift, &at("main", "h"), &at("main", "h")), Ok(()));
        assert_eq!(landed_on(Tool::Forklift, &at("main", "h"), &at("main", "x")), Err("Forklift's clone checked out main at x, not main at the subject's head h".to_string()));
        assert_eq!(landed_on(Tool::Git, &at("main", "h"), &at("probe", "h")), Err("git's clone checked out probe at h, not main at the subject's head h".to_string()));
    }

    /// A copy gone before it is removed stops the run; only a folder a timed command may not have
    /// made, a clone's, is let go missing.
    #[test]
    fn only_a_clones_folder_may_be_missing_when_removed() {
        let missing = std::env::temp_dir().join(format!("proof-load-never-made-{}", std::process::id()));
        assert!(matches!(Copied(Some(missing.clone())).remove(), Err(Stopped(reason)) if reason.starts_with("removing the copy ")));
        assert!(matches!(Copied(Some(missing)).remove_if_made(), Ok(())));
    }

    /// Forklift's refs are read from its pallet list, which must name the probe pallet current,
    /// and from the remote's, where a pallet the remote lacks, or one it lists with no head, has
    /// none; a list with no pallets, or a head of another kind, is not read as none.
    #[cfg(unix)]
    #[test]
    fn forklifts_refs_are_its_probe_pallet_and_the_remotes() {
        let (root, scratch, environment) = scratch("forklift-refs");
        let folder = scratch.work().to_path_buf();
        let program = folder.join("forklift");
        std::fs::write(&program, format!(r#"#!/bin/sh
case "$*" in
"--json pallet list") cat '{0}/local';;
"--json pallet list --remote") cat '{0}/remote';;
*) echo "no $*" >&2; exit 2;;
esac
"#, folder.display())).unwrap();
        std::fs::set_permissions(&program, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let subject = Subject { tool: Tool::Forklift, program, prefix: vec![], work: folder.clone(), watched: false };
        let read = |local: &str, remote: &str| {
            std::fs::write(folder.join("local"), local).unwrap();
            std::fs::write(folder.join("remote"), remote).unwrap();
            refs(&subject, &scratch, &environment).unwrap()
        };
        let local = r#"{"data":{"current":"proof-load-probe","pallets":[{"name":"main","current":false,"head":"m"},{"name":"proof-load-probe","current":true,"head":"p"}]}}"#;
        let refs = |local: &str, remote: Option<&str>| Refs { local: local.into(), remote: remote.map(str::to_string) };

        assert_eq!(read(local, r#"{"data":{"pallets":[{"name":"main","remote_head":"m"},{"name":"proof-load-probe","remote_head":"r"}]}}"#), Ok(refs("p", Some("r"))));
        assert_eq!(read(local, r#"{"data":{"pallets":[{"name":"main","remote_head":"m"}]}}"#), Ok(refs("p", None)));
        assert_eq!(read(local, r#"{"data":{"pallets":[{"name":"proof-load-probe","remote_head":null}]}}"#), Ok(refs("p", None)));
        assert_eq!(read(local, r#"{"data":{}}"#), Err("Forklift's --json pallet list --remote gave no pallets".to_string()));
        assert_eq!(read(local, r#"{"data":{"pallets":[{"name":"proof-load-probe","remote_head":7}]}}"#),
            Err("Forklift's --json pallet list --remote gave 7 as the remote head of proof-load-probe".to_string()));
        assert_eq!(read(&local.replace(r#""current":"proof-load-probe""#, r#""current":"main""#), "{}"),
            Err("Forklift is on main, not proof-load-probe".to_string()));
        assert_eq!(read(&local.replace(r#""current":"proof-load-probe""#, r#""current":null"#), "{}"), Err("Forklift's --json pallet list named null current, not a pallet".to_string()));
        assert_eq!(read(&local.replace(r#""head":"p""#, r#""head":null"#), "{}"), Err("Forklift's --json pallet list named no head for proof-load-probe".to_string()));
        #[allow(clippy::disallowed_methods)] // a test's own temporary folder
        let _ = std::fs::remove_dir_all(&root);
    }

    /// git's remote pairs are timed against the remotes the harness builds, each repetition on
    /// fresh copies that are gone afterwards; with the remote at the head being pushed or pulled
    /// to, a push and a pull succeed but move nothing, and are voided saying so.
    #[cfg(unix)]
    #[test]
    fn gits_remote_pairs_are_timed_and_a_transfer_of_nothing_is_voided() {
        let (root, scratch, environment) = scratch("remotes");
        let git = crate::scrub::path_git();
        let work = scratch.work().join("repo");
        let run = |directory: &Path, arguments: &[&str]| {
            let invocation = Invocation { program: git.clone(), arguments: arguments.iter().map(|argument| argument.to_string()).collect(), directory: directory.to_path_buf(), limit: Duration::from_secs(30) };
            let outcome = tool::run(&invocation, &scratch, &environment).unwrap();
            assert!(outcome.succeeded(), "{:?}: {}", arguments, String::from_utf8_lossy(&outcome.stderr));
            String::from_utf8_lossy(&outcome.stdout).trim().to_string()
        };
        std::fs::create_dir(&work).unwrap();
        run(&work, &["init", "-q", "-b", PROBE_REF]);
        for name in ["a", "b", "c", "d", "e"] {
            std::fs::write(work.join(name), format!("{}\n", name)).unwrap();
            run(&work, &["add", "-A"]);
            run(&work, &["commit", "-q", "-m", name]);
        }
        run(&work, &["branch", "main"]);
        let head = run(&work, &["rev-parse", "HEAD"]);
        let subjects = [Subject { tool: Tool::Git, program: git.clone(), prefix: vec![], work: work.clone(), watched: false }];
        let server = PathBuf::from("/nonexistent/forklift-server");

        let mut rows = Vec::new();
        remotes(&subjects, "main", &["a", "b"], 1, &Err("r".into()), &Ok(server.clone()), &scratch, &environment, &|_| {}, &mut rows).unwrap();
        assert_eq!(rows.iter().map(|row| (row.operation.as_str(), row.cache)).collect::<Vec<_>>(), [
            ("clone", Cache::Warm), ("clone", Cache::Cold), ("push", Cache::Warm), ("push", Cache::Cold), ("pull", Cache::Warm), ("pull", Cache::Cold),
        ]);
        for row in &rows {
            match row.cache {
                Cache::Warm => assert!(matches!(&row.outcome, Outcome::Timed { cells } if cells.len() == 1 && matches!(cells[0].result, Measured::Timed { .. })), "{} {:?}", row.operation, row.outcome),
                Cache::Cold => assert_eq!(row.outcome, Outcome::NotRun { reason: "r".into() }),
            }
        }
        let Outcome::Timed { cells } = &rows[0].outcome else { unreachable!() };
        assert_eq!(cells[0].command, ["clone", "-q", "--no-local", "{url}", "{work}"]);
        // The remotes were built from copies of the subject, which neither moved nor kept them.
        assert_eq!(run(&work, &["rev-parse", "HEAD"]), head);
        assert_eq!(run(&work, &["status", "--porcelain"]), "");
        let copies = scratch.work().join("copies");
        assert_eq!(std::fs::read_dir(&copies).unwrap().count(), 0, "copies left behind");
        assert!(!scratch.work().join("remotes").exists(), "remotes left behind");
        // The remotes, built again to be read: git's level one names main as its HEAD, as
        // Forklift's clone checks out main.
        let built = hosts(&subjects, &server, "main", &["a", "b"], &scratch.work().join("built"), &scratch, &environment).unwrap().unwrap();
        let built = &built[0];
        assert_eq!(run(&built.level, &["rev-parse", "main", PROBE_REF]), format!("{}\n{}", head, head));
        assert_eq!(run(&built.level, &["symbolic-ref", "HEAD"]), "refs/heads/main");
        let ahead = run(&built.ahead, &["rev-parse", "HEAD"]);
        assert_eq!(run(&built.ahead, &["rev-list", "--count", &format!("{}..{}", head, ahead)]), "3");
        assert_eq!(run(&built.ahead, &["status", "--porcelain"]), "");
        assert_eq!(run(&built.past, &["rev-parse", PROBE_REF]), ahead);
        let voided = |hosts: &[Hosts], transfer: Transfer| match remote_pair(&subjects, hosts, transfer, "main", 1, None, &server, &copies, &scratch, &environment).unwrap() {
            Outcome::Timed { cells } => cells.into_iter().map(|cell| cell.result).collect::<Vec<_>>(),
            other => panic!("{:?}", other),
        };
        // A clone that lands on another branch, at another commit, is voided saying where.
        let past = [Hosts { level: built.past.clone(), ahead: work.clone(), past: built.past.clone() }];
        assert_eq!(voided(&past, Transfer::Clone), [Measured::NothingMoved {
            reason: format!("repetition 1: git's clone checked out {} at {}, not main at the subject's head {}", PROBE_REF, ahead, head),
        }]);
        assert_eq!(std::fs::read_dir(&copies).unwrap().count(), 0, "copies left behind");
        // A watched subject's untimed commands start git's own fsmonitor daemon over its advanced
        // copy, which is stopped once the copy is built.
        if crate::runner::has_fsmonitor_daemon(&git) {
            let tuned = scratch.work().join("tuned");
            crate::scrub::copy_tree(&work, &tuned).unwrap();
            run(&tuned, &["config", "core.fsmonitor", "true"]);
            let tuned = [Subject { tool: Tool::GitTuned, program: git.clone(), prefix: vec![], work: tuned, watched: true }];
            let built = hosts(&tuned, &server, "main", &["a", "b"], &scratch.work().join("tuned-built"), &scratch, &environment).unwrap().unwrap();
            let status = Invocation { program: git.clone(), arguments: vec!["fsmonitor--daemon".into(), "status".into()], directory: built[0].ahead.clone(), limit: Duration::from_secs(30) };
            assert!(!tool::run(&status, &scratch, &environment).unwrap().succeeded(), "git's own daemon still watches the advanced copy");
        }

        let level = scratch.work().join("level");
        run(scratch.root(), &["init", "--bare", "-q", "-b", PROBE_REF, &level.display().to_string()]);
        run(&work, &["push", "-q", &level.display().to_string(), &format!("HEAD:refs/heads/{}", PROBE_REF)]);
        let hosts = [Hosts { level: level.clone(), ahead: work.clone(), past: level.clone() }];
        assert_eq!(voided(&hosts, Transfer::Push), [Measured::NothingMoved {
            reason: format!("repetition 1: git's remote held {} before the push and after it, so the push moved nothing", head),
        }]);
        assert_eq!(voided(&hosts, Transfer::Pull), [Measured::NothingMoved {
            reason: format!("repetition 1: git's head was {} before the pull and after it, so the pull moved nothing", head),
        }]);
        let cells = vec![Cell { subject: Tool::Git, command: vec!["pull".into(), "-q".into()], result: Measured::NothingMoved { reason: "a | b".into() } }];
        let rows = vec![Row { operation: "pull".into(), cache: Cache::Warm, note: "n".into(), outcome: Outcome::Timed { cells } }];
        let shown = PerfReport { verdict: judge(&rows, "small", Platform::Linux, Gate::All, &[]), rows, ..report() };
        assert!(shown.markdown().contains("| git | voided: did not move the head: a \\| b | | | | | `pull -q` |\n"), "{}", shown.markdown());
        assert!(shown.failed());
        assert_eq!(std::fs::read_dir(&copies).unwrap().count(), 0, "copies left behind");

        // A clone that fails leaves no folder, which voids its cell rather than stopping the run.
        let empty = scratch.work().join("empty");
        std::fs::create_dir(&empty).unwrap();
        let hosts = [Hosts { level: empty, ahead: work.clone(), past: level.clone() }];
        let Outcome::Timed { cells } = remote_pair(&subjects, &hosts, Transfer::Clone, "main", 1, None, &server, &copies, &scratch, &environment).unwrap() else { panic!() };
        assert!(matches!(cells[0].result, Measured::Voided { exit: Some(128), .. }), "{:?}", cells[0].result);
        assert_eq!(std::fs::read_dir(&copies).unwrap().count(), 0, "copies left behind");
        #[allow(clippy::disallowed_methods)] // a test's own temporary folder
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A perf run over the small shape writes perf.json and perf.md, every pair timed warm on all
    /// three subjects and its cold row not run without a cache-drop program: the sameness, differ,
    /// clean and moved-something checks pass on a generated history, both subjects were built from
    /// one mirror (history.rs refuses an import whose current branch differs), and Forklift's is
    /// enrolled.
    ///
    /// Needs a forklift binary: `PROOF_LOAD_FORKLIFT=<path> cargo test -p proof-load -- --ignored`;
    /// the forklift-server is `PROOF_LOAD_FORKLIFT_SERVER`, or the one beside that forklift.
    #[test]
    #[ignore = "drives a forklift binary named by PROOF_LOAD_FORKLIFT"]
    fn a_perf_run_times_every_pair_on_every_subject() {
        let forklift = PathBuf::from(std::env::var_os("PROOF_LOAD_FORKLIFT").expect("PROOF_LOAD_FORKLIFT names the forklift binary under test"));
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let out = std::env::temp_dir().join(format!("proof-load-perf-run-{}-{}", std::process::id(), nanos));
        let server = std::env::var_os("PROOF_LOAD_FORKLIFT_SERVER").map(PathBuf::from)
            .unwrap_or_else(|| forklift.with_file_name(format!("forklift-server{}", std::env::consts::EXE_SUFFIX)));
        let options = PerfOptions {
            out: out.clone(), forklift: forklift.clone(), git: crate::scrub::path_git(), shape: None, history_from: None, side_branch: None, reps: 1, gate: Some(Gate::All),
            drop_caches: None, forklift_server: Some(server),
        };
        let report = run(&options, &|_| {}).unwrap();
        let operations: Vec<&str> = report.rows.iter().filter(|row| row.cache == Cache::Warm).map(|row| row.operation.as_str()).collect();
        let paired = ["status", "log", "blame", "verify", "commit", "switch", "merge", "pack", "clone", "push", "pull", "diff", "diff --staged"];
        let gated = [FRESH_COMPACT, REPACK, AUDIT, AUDIT_ONE_CORE];
        assert_eq!(operations, paired.iter().chain(&gated).copied().collect::<Vec<_>>());
        assert_eq!(report.rows.len(), 2 * paired.len() + gated.len());
        assert_eq!(report.header.drop_caches, None);
        assert!(report.header.tools.contains_key("forklift-server"), "{:?}", report.header.tools);
        let (pairs, gate) = report.rows.split_at(2 * paired.len());
        // The gate subject's rows are timed on Forklift alone: its audit counted every parcel after
        // the first as signed, and its fresh compact packed something, or they would not be.
        for row in gate {
            let pinned = row.operation == AUDIT_ONE_CORE && one_core_when(cfg!(target_os = "linux"), std::env::var_os("PATH")).is_err();
            if pinned {
                assert_eq!(row.outcome, Outcome::NotRun { reason: NO_ONE_CORE.into() });
                continue;
            }
            let Outcome::Timed { cells } = &row.outcome else { panic!("{} was not timed: {:?}", row.operation, row.outcome) };
            assert_eq!(cells.iter().map(|cell| cell.subject).collect::<Vec<_>>(), [Tool::Forklift], "{}", row.operation);
            assert!(matches!(cells[0].result, Measured::Timed { .. }), "{}: {:?}", row.operation, cells[0].result);
        }
        for pair in pairs.chunks(2) {
            assert_eq!((pair[0].cache, pair[1].cache, &pair[0].operation), (Cache::Warm, Cache::Cold, &pair[1].operation));
            assert_eq!(pair[1].outcome, Outcome::NotRun { reason: "no cache-drop program given (--drop-caches)".into() });
        }
        for row in pairs.iter().filter(|row| row.cache == Cache::Warm) {
            let Outcome::Timed { cells } = &row.outcome else { panic!("{} was not timed: {:?}", row.operation, row.outcome) };
            let subjects: Vec<Tool> = cells.iter().map(|cell| cell.subject).collect();
            assert_eq!(subjects, [Tool::Forklift, Tool::Git, Tool::GitTuned], "{}", row.operation);
            for cell in cells {
                assert!(matches!(cell.result, Measured::Timed { .. }), "{} {:?}: {:?}", row.operation, cell.subject, cell.result);
            }
        }
        // A debug build may be slower than the gate allows; it must still have run every row.
        let unrun: Vec<&Failure> = report.verdict.failures.iter().filter(|failure| failure.check == Check::CouldNotRun).collect();
        assert_eq!(unrun, Vec::<&Failure>::new());
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
