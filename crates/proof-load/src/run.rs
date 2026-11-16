//! A whole run as a library call: the completeness checks, every lane the options ask for, and the
//! report, written to the output directory beside each graded fault's details. The CLI and the
//! desktop head are both thin callers of `run`; neither has run logic of its own, and this module
//! never prints: what a caller should show arrives through `Progress`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::census::Census;
use crate::corpus;
use crate::damage::Warehouse;
use crate::faulted::{self, Programs};
use crate::kill;
use crate::report::{Category, Cell, Completeness, CoverageTable, Determinism, HistoryDirection, Pairing, Platform, Report, Row, Target, REPORT_FORMAT};
use crate::runner;
use crate::scenario::{self, Tool};
use crate::scrub::{Environment, Output};
use crate::tool;
use crate::writeops;

/// What one run is asked to do. The CLI's `run` flags, one field each; see `proof-load run --help`
/// for what each means.
#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    pub source: PathBuf,
    pub out: PathBuf,
    pub seed: u64,
    pub only: Option<String>,
    pub forklift: Option<PathBuf>,
    pub git: Option<PathBuf>,
    pub forklift_server: Option<PathBuf>,
    pub budget: Option<u64>,
    pub strace: Option<PathBuf>,
    pub kill: Vec<String>,
    pub power: Vec<String>,
    pub power_variants: usize,
    pub jobs: Option<usize>,
    pub capped_volume: Option<PathBuf>,
    pub disk_full: Vec<String>,
    pub read_only: Vec<String>,
    pub folding_volume: Option<PathBuf>,
    pub network: Vec<String>,
    pub history_from: Option<PathBuf>,
}

/// What a run tells its caller while it works. A block is one graded unit a lane runs on a fresh
/// build: one damage binding's kind, one disk-full point, one read-only or volume fault, one network
/// point, or one scenario's kill points or power loss. Every method has a do-nothing default.
pub trait Progress: Sync {
    /// A lane is starting, with how many blocks it will run where that is known before it starts.
    fn lane(&self, _category: Category, _blocks: Option<usize>) {}
    fn block_started(&self, _category: Category, _block: &str) {}
    /// `failed` is whether anything the block graded is a failing grade, or the block itself
    /// could not run.
    fn block_finished(&self, _category: Category, _block: &str, _failed: bool) {}
    /// A line for the person running it, such as which names a scenario's build created.
    fn message(&self, _text: &str) {}
    /// Polled before each block and each build a lane counts on: once it answers true the run
    /// stops there, writes no report and returns `RunError::Cancelled`. A block already started
    /// runs to its end.
    fn cancelled(&self) -> bool {
        false
    }
}

/// A `Progress` that shows nothing and never cancels.
pub struct Quiet;

impl Progress for Quiet {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunError {
    /// The run could not start or could not write its output; nothing was graded as failing.
    Refused(String),
    /// `Progress::cancelled` answered true before a block.
    Cancelled,
}

impl std::fmt::Display for RunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Refused(reason) => formatter.write_str(reason),
            RunError::Cancelled => formatter.write_str("the run was cancelled"),
        }
    }
}

/// Stops the run once the caller has cancelled.
fn stop_if_cancelled(progress: &dyn Progress) -> Result<(), RunError> {
    if progress.cancelled() { Err(RunError::Cancelled) } else { Ok(()) }
}

/// Stops the run before a block once the caller has cancelled, and says the block is starting.
fn start_block(progress: &dyn Progress, category: Category, block: &str) -> Result<(), RunError> {
    stop_if_cancelled(progress)?;
    progress.block_started(category, block);
    Ok(())
}

/// Writes a row's details file, `name` under the output directory. A details file a row points at
/// that cannot be written refuses the run, as the report itself would.
fn write_detail(root: &std::path::Path, name: &str, value: &impl serde::Serialize) -> Result<(), RunError> {
    let refused = |error: String| RunError::Refused(format!("cannot write {}: {}", name, error));
    let text = serde_json::to_string(value).map_err(|error| refused(error.to_string()))?;
    std::fs::write(root.join(name), text).map_err(|error| refused(error.to_string()))
}

/// A tool as the determinism record names it: its path and the first line of its `--version`.
/// Asked with nothing inherited but PATH, from the temp directory, so nothing of the user's is read.
fn described(program: &std::path::Path) -> String {
    let version = std::process::Command::new(program).arg("--version").env_clear()
        .envs(std::env::var_os("PATH").map(|path| ("PATH", path))).current_dir(std::env::temp_dir()).output().ok()
        .and_then(|output| String::from_utf8_lossy(&output.stdout).lines().next().map(|line| line.trim().to_string()))
        .filter(|line| !line.is_empty()).unwrap_or_else(|| "no --version output".to_string());
    format!("{} ({})", program.display(), version)
}

/// The operating system the run was on, as the plate's header names it: its name and release.
#[cfg(unix)]
fn operating_system() -> String {
    // SAFETY: uname fills the zeroed struct it is given and returns -1 on failure, writing nothing.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut name) } != 0 {
        return format!("{} (uname failed)", Platform::current().label());
    }
    let field = |chars: &[libc::c_char]| -> String {
        let bytes: Vec<u8> = chars.iter().take_while(|c| **c != 0).map(|c| c.to_ne_bytes()[0]).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    };
    format!("{} {}", field(&name.sysname), field(&name.release))
}

#[cfg(not(unix))]
fn operating_system() -> String {
    format!("{} (release not read in this harness)", Platform::current().label())
}

/// The type of the filesystem `path` is on, as the plate's header names it.
#[cfg(target_os = "macos")]
fn filesystem_of(path: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return "unknown: the path holds a NUL".to_string() };
    // SAFETY: statfs fills the zeroed struct from a NUL-terminated path and returns -1 on failure.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut stat) } != 0 {
        return format!("unknown: {}", std::io::Error::last_os_error());
    }
    let bytes: Vec<u8> = stat.f_fstypename.iter().take_while(|c| **c != 0).map(|c| c.to_ne_bytes()[0]).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The type of the filesystem `path` is on, by its magic number, as the plate's header names it.
#[cfg(all(unix, not(target_os = "macos")))]
fn filesystem_of(path: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return "unknown: the path holds a NUL".to_string() };
    // SAFETY: statfs fills the zeroed struct from a NUL-terminated path and returns -1 on failure.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut stat) } != 0 {
        return format!("unknown: {}", std::io::Error::last_os_error());
    }
    #[allow(clippy::unnecessary_cast)]
    let magic = stat.f_type as u64 & 0xffff_ffff;
    let name = match magic {
        0xEF53 => "ext2/ext3/ext4",
        0x0102_1994 => "tmpfs",
        0x9123_683E => "btrfs",
        0x5846_5342 => "xfs",
        0x794C_7630 => "overlayfs",
        0x2FC1_2FC1 => "zfs",
        0x6969 => "nfs",
        0xF2F5_2010 => "f2fs",
        0x6573_5546 => "fuse",
        _ => return format!("unknown (magic {:#x})", magic),
    };
    name.to_string()
}

#[cfg(not(unix))]
fn filesystem_of(_path: &std::path::Path) -> String {
    "not read on Windows in this harness".to_string()
}

/// What every repository the harness creates for git holds in its own config, as the plate's
/// header names it.
fn created_configuration() -> String {
    runner::GIT_CREATED_CONFIG.iter().map(|[key, value]| format!("{}={}", key, value)).collect::<Vec<_>>().join(" ")
}

/// What git tuned runs with beyond git at its defaults, as the report names it: its options, and
/// the fsmonitor where this git has a daemon for it.
fn tuned_configuration(daemon: bool) -> String {
    let options: Vec<&str> = crate::scenario::GIT_TUNED.iter().copied().filter(|option| *option != "-c").collect();
    let fsmonitor = if daemon { "core.fsmonitor=true (its daemon run by the harness)" } else { "core.fsmonitor: unsupported here" };
    format!("{}; {}", options.join(" "), fsmonitor)
}

/// How long each command of a round trip over `--history-from` may run. Forklift's own history,
/// some 3400 commits, exports in about 200 s from a debug build.
const HISTORY_LIMIT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// How long the damage rows over `--history-from` may start, at most: none starts past the
/// run's own `--budget` either. A row the budget does not reach is named in its subject's
/// completeness, never silently absent.
const HISTORY_DAMAGE_BUDGET: std::time::Duration = std::time::Duration::from_secs(2 * 60 * 60);

/// Why a history damage row may not start `now`, if it may not: the run, given a `deadline`, is
/// past it, or the rows have run more than `budget` since `rows_started`.
fn row_cut(now: std::time::Instant, rows_started: std::time::Instant, budget: std::time::Duration, deadline: Option<std::time::Instant>) -> Option<&'static str> {
    if deadline.is_some_and(|deadline| now > deadline) {
        Some("the run's --budget was spent")
    } else if now.duration_since(rows_started) > budget {
        Some("the lane's time budget was spent")
    } else {
        None
    }
}

/// Whose a git step building git's subject over forklift's export failed is: forklift's where git
/// refused it, since what git refuses there is forklift's output; the run's own where it ran past
/// its limit or could not be run at all.
fn export_charged_to(error: &crate::runner::RunError) -> Option<Tool> {
    matches!(error, crate::runner::RunError::BuildFailed { .. }).then_some(Tool::Forklift)
}

/// When a run `started` and given `budget` seconds must start its last history row: never, for
/// a run given none or a budget past what the clock can count.
fn run_deadline(started: std::time::Instant, budget: Option<u64>) -> Option<std::time::Instant> {
    budget.and_then(|budget| started.checked_add(std::time::Duration::from_secs(budget)))
}

/// Whose a history build step `tool` failed is: forklift's where forklift ran it and refused it
/// or misbuilt it, or ran past its limit. Git refusing one over the given history is the run's
/// own, git being the oracle the sample is drawn from, as is a step the harness could not run at
/// all. Git's steps over forklift's export are charged by [`export_charged_to`] instead.
fn build_charged_to(tool: Tool, error: &crate::runner::RunError) -> Option<Tool> {
    use crate::runner::RunError::{BuildFailed, Tool as Ran};
    let own = tool == Tool::Forklift && matches!(error, BuildFailed { .. } | Ran(crate::tool::ToolError::TimedOut { .. }));
    own.then_some(tool)
}

/// Whether a history `subject` refusing its own unfaulted readback is a gap in its measure:
/// forklift's, git tuned's, and git's over forklift's export are, while git at its defaults over
/// the given history is the oracle the sample was drawn from, so its refusal is the run's own.
fn refused_readback_is_a_gap(subject: Built) -> bool {
    subject != (Tool::Git, Some(HistoryDirection::FromGit))
}

/// Why a history's sample holds no position, and whose it is: forklift's, where git holds a ref
/// with commits and forklift's import made a pallet of none of them; the run's own otherwise.
fn empty_sample(lengths: &BTreeMap<String, usize>, present: &std::collections::BTreeSet<String>) -> (Option<Tool>, String) {
    let counted: Vec<&String> = lengths.iter().filter(|(_, length)| **length > 0).map(|(name, _)| name).collect();
    if !counted.is_empty() && !counted.iter().any(|name| present.contains(*name)) {
        (Some(Tool::Forklift), "forklift: the import made no pallet of a ref git holds".to_string())
    } else {
        (None, "the sample holds no position: no ref both subjects hold has a commit".to_string())
    }
}

/// A history subject: its tool and, for a git subject, the direction it was built in. Forklift's
/// one subject, the import, is both directions', so it names none.
type Built = (Tool, Option<HistoryDirection>);

/// Every history subject the lane builds, in the order it takes their readbacks.
const HISTORY_SUBJECTS: [Built; 5] = [
    (Tool::Forklift, None),
    (Tool::Git, Some(HistoryDirection::FromGit)), (Tool::GitTuned, Some(HistoryDirection::FromGit)),
    (Tool::Git, Some(HistoryDirection::FromForklift)), (Tool::GitTuned, Some(HistoryDirection::FromForklift)),
];

/// The directions whose plate lines a history subject's rows stand on: both for forklift's.
fn stands_on((_, direction): Built) -> Vec<HistoryDirection> {
    match direction {
        Some(direction) => vec![direction],
        None => vec![HistoryDirection::FromGit, HistoryDirection::FromForklift],
    }
}

/// A history completeness check's name for `subject`: with its direction, for a git subject.
fn history_check(check: &str, (_, direction): Built) -> String {
    match direction {
        Some(direction) => direction.name(check),
        None => check.to_string(),
    }
}

/// The label of `subject`'s detail and scratch for `label`: a git subject over forklift's export
/// is told from the one over the given history by its prefix.
fn history_label((_, direction): Built, label: String) -> String {
    match direction {
        Some(HistoryDirection::FromForklift) => format!("export-{}", label),
        _ => label,
    }
}

/// The row a damage run over a real history grades for `subject`, its detail at `label`, paired
/// under `scenario` by `role` on each line of [`stands_on`]; or, where the whole-store check found
/// damage the sample did not reach, the completeness entry naming it instead. Such damage is no
/// grade: never healed, never paired, so never in "git does better".
fn history_row(subject: Built, label: &str, run: &faulted::Faulted, scenario: &str, role: crate::git_layout::Role) -> Result<Row, String> {
    let (tool, direction) = subject;
    if let Some(check) = &run.unreached {
        return Err(format!("{} {:?}: {:?} exited {:?} (details {}.json)", run.target, run.kind, check.arguments, check.exit, label));
    }
    let mut notes: Vec<String> = Vec::new();
    if tool == Tool::Forklift {
        notes.push("each position is read through a bay opened at the current head first, so damage the head holds is met at every position".to_string());
    }
    if !run.unextractable.is_empty() {
        notes.push(format!("unextractable remedies: {}", run.unextractable.join("; ")));
    }
    let named = direction.map_or(scenario.to_string(), |direction| direction.name(scenario));
    Ok(Row {
        subject: tool,
        scenario: format!("{} {} {:?}", named, run.target, run.kind),
        category: if run.kind.plants() { Category::DeadPidLock } else { Category::DamagedFiles },
        binding: tool.label().to_string(),
        grade: run.grade,
        recoverability: run.recoverability.clone(),
        note: (!notes.is_empty()).then(|| notes.join("; ")),
        detail: Some(format!("{}.json#", label)),
        pairing: Some(Pairing { scenario: scenario.to_string(), target: Target::File { side: Warehouse::Client, role, kind: run.kind }, directions: stands_on(subject) }),
    })
}

/// The damage rows over a real history, from `mirror`, the copy the fidelity lane took, in both
/// directions: git's subject and forklift's built once from it, the sample drawn over the refs
/// both hold, and a second git subject built from forklift's git export of its own, so git's rows
/// run over the given history (from git) and over forklift's export of it (from Forklift) while
/// forklift's run once and stand on both. Each subject's history bindings are checked against its
/// walk and its unfaulted readback taken on a copy, then one row per (role, kind) per subject,
/// each on its own copy, in seed order while the rows have run no more than `budget` and the run
/// is not past its `deadline`. With the rows, every check the lane answers: a row the budget did
/// not reach, forklift refusing its unfaulted readback, and damage the sample did not reach are
/// each named in the subject's own completeness, never as a grade, as is git tuned refusing its
/// readback and git refusing its over forklift's export; git at its defaults refusing its over
/// the given history, the oracle the sample was drawn from, is the run's own failure. Forklift's
/// export failing leaves only the from-Forklift direction unrun.
/// `damage_blocks` is how many blocks the fixture damage lane announced, which the history's are
/// added to.
#[allow(clippy::too_many_arguments)]
fn history_damage(
    mirror: &std::path::Path, programs: &Programs, census: &Census, seed: u64, budget: std::time::Duration, deadline: Option<std::time::Instant>,
    damage_blocks: usize,
    output: &mut Output, search_path: &Option<std::ffi::OsString>, progress: &dyn Progress,
) -> Result<(Vec<Row>, Vec<Completeness>), RunError> {
    let mut rows = Vec::new();
    let mut history_checks = Vec::new();
    // What each subject's rows found missing, the run's own errors running them, and the
    // damage the sample did not reach.
    let mut gaps: BTreeMap<Built, Vec<String>> = BTreeMap::new();
    let mut errors: BTreeMap<Built, Vec<String>> = BTreeMap::new();
    let mut unreached: BTreeMap<Built, Vec<String>> = BTreeMap::new();
    stop_if_cancelled(progress)?;
    const BLOCK: &str = "damage-history";
    // Every step the subjects were built with and what the sample was drawn from, written whether
    // or not the build finished, so a failure the report names has its evidence beside it.
    let mut steps = Vec::new();
    let mut drawn = serde_json::Value::Null;
    type Refusal = (Option<Tool>, String);
    let built = (|| -> Result<(_, _, _, Result<std::path::PathBuf, Refusal>), Refusal> {
        let harness = |error: String| (None, error);
        let scratch = output.scratch(BLOCK).map_err(|error| harness(error.to_string()))?;
        let environment = Environment::scrubbed(&scratch, search_path.clone(), &BTreeMap::new()).map_err(|error| harness(error.to_string()))?;
        let (git_root, forklift_root) = (scratch.root().join("git"), scratch.root().join("forklift"));
        let refused = |tool: Tool| move |error: crate::runner::RunError| (build_charged_to(tool, &error), format!("{}: {}", tool.label(), error));
        let branch = crate::history::build_git(&programs.git, mirror, &git_root, &scratch, &environment, HISTORY_LIMIT, &mut steps).map_err(refused(Tool::Git))?;
        crate::history::build_forklift(&programs.forklift, mirror, &branch, &forklift_root, &scratch, &environment, HISTORY_LIMIT, &mut steps)
            .map_err(refused(Tool::Forklift))?;
        let lengths = crate::history::git_lengths(&programs.git, &git_root, &scratch, &environment, HISTORY_LIMIT).map_err(refused(Tool::Git))?;
        let present = crate::history::pallets(&programs.forklift, &forklift_root, &scratch, &environment, HISTORY_LIMIT).map_err(refused(Tool::Forklift))?;
        let positions = crate::history::sample(&lengths, &present, crate::history::SAMPLED, seed);
        drawn = serde_json::json!({ "lengths": lengths, "present": present, "positions": positions });
        if positions.is_empty() {
            return Err(empty_sample(&lengths, &present));
        }
        // Git's subject over forklift's export, from a copy of forklift's subject. Its failing
        // fails only its direction: the subjects above are built and the sample drawn.
        let root = scratch.root();
        // Git refusing to take the export as a repository is forklift's, as the export's own
        // refusal is: what git refuses is forklift's output, laid out by steps it takes over the
        // given history too. A git step running past its limit, or one the harness could not
        // run, is the run's own. Once built, git refusing to read a sampled position back is a
        // gap in git's measure, as over the given history git tuned refusing its is.
        let sampled: std::collections::BTreeSet<&str> = std::iter::once(branch.as_str()).chain(positions.iter().map(|position| position.reference.as_str())).collect();
        let over_export = |error: crate::runner::RunError| (export_charged_to(&error), format!("git, over forklift's export: {}", error));
        let exported = crate::history::export_forklift(&programs.forklift, &forklift_root, &sampled, &root.join("export-warehouse"), &root.join("export"), &scratch, &environment, HISTORY_LIMIT, &mut steps)
            .map_err(refused(Tool::Forklift))
            .and_then(|()| crate::history::build_git_export(&programs.git, &root.join("export"), &branch, &root.join("export-mirror"), &root.join("git-export"), &scratch, &environment, HISTORY_LIMIT, &mut steps)
                .map_err(over_export))
            .map(|()| root.join("git-export"));
        Ok((git_root, forklift_root, positions, exported))
    })();
    write_detail(output.root(), &format!("{}.json", BLOCK), &serde_json::json!({ "steps": steps, "drawn": drawn }))?;
    let (git_root, forklift_root, positions, exported) = match built {
        Ok(built) => built,
        Err((subject, error)) => {
            history_checks.push(Completeness { subject, check: "history subjects built".to_string(), cell: Cell::Run, missing: vec![error] });
            (Default::default(), Default::default(), Vec::new(), Err((None, String::new())))
        }
    };
    let export_root = match exported {
        Ok(root) => Some(root),
        Err((subject, error)) => {
            if !positions.is_empty() {
                let check = history_check("history subjects built", (Tool::Git, Some(HistoryDirection::FromForklift)));
                history_checks.push(Completeness { subject, check, cell: Cell::Run, missing: vec![error] });
            }
            None
        }
    };
    let mut subjects = Vec::new();
    let mut refused_baselines = Vec::new();
    if !positions.is_empty() {
        for subject in HISTORY_SUBJECTS {
            let (tool, direction) = subject;
            let pristine = match direction {
                None => forklift_root.clone(),
                Some(HistoryDirection::FromGit) => git_root.clone(),
                Some(HistoryDirection::FromForklift) => match &export_root { Some(root) => root.clone(), None => continue },
            };
            let state = pristine.join(runner::state_folder(tool));
            match crate::census::walk(&state) {
                Ok(walked) => history_checks.push(Completeness {
                    subject: Some(tool), check: history_check("history damage bindings", subject), cell: Cell::Run,
                    missing: crate::bindings::check_history_subject(tool, census, &walked),
                }),
                Err(error) => errors.entry(subject).or_default().push(format!("{}: {}", state.display(), error)),
            }
            stop_if_cancelled(progress)?;
            let label = history_label(subject, format!("{}-baseline-{}", BLOCK, tool.label()));
            let base = output.scratch(&label).map_err(|error| error.to_string()).and_then(|scratch| {
                let environment = Environment::scrubbed(&scratch, search_path.clone(), &BTreeMap::new()).map_err(|error| error.to_string())?;
                faulted::baseline(tool, &pristine, &positions, programs, &scratch, &environment, HISTORY_LIMIT).map_err(|error| error.to_string())
            });
            // Forklift, or git tuned, refusing its own unfaulted readback leaves a gap in its
            // measure, as git does refusing its over forklift's export. Git at its defaults over
            // the given history is the oracle the sample was drawn from, so git refusing a
            // position it counted there is the run's own failure, as a readback the harness could
            // not take is.
            match base {
                Ok(Ok(base)) => {
                    write_detail(output.root(), &format!("{}.json", label), &base)?;
                    subjects.push((subject, pristine, base));
                }
                Ok(Err(refused)) => {
                    refused_baselines.push(subject);
                    let blamed = if refused_readback_is_a_gap(subject) { &mut gaps } else { &mut errors };
                    blamed.entry(subject).or_default().push(format!("its rows: not run: the unfaulted readback failed: {}", refused));
                }
                Err(error) => errors.entry(subject).or_default().push(format!("its unfaulted readback: {}", error)),
            }
        }
    }
    // Every subject's rows, shuffled by the seed, so a spent budget cuts rows of every subject
    // rather than only those of the subject planned last. The shuffle draws from its own stream,
    // apart from the one the rows and the sample were picked with.
    let mut planned: Vec<_> = subjects.iter()
        .flat_map(|(subject, pristine, base)| crate::bindings::history_rows(subject.0, seed).into_iter().map(move |(binding, kind, role)| (*subject, pristine, base, binding, kind, role)))
        .collect();
    let mut state = seed ^ 0x5348_5546_464c_4521;
    for index in (1..planned.len()).rev() {
        planned.swap(index, (crate::history::splitmix(&mut state) % (index as u64 + 1)) as usize);
    }
    progress.lane(Category::DamagedFiles, Some(damage_blocks + planned.len()));
    let scenario = format!("{}, over a sample of {} positions", faulted::HISTORY_SCENARIO, positions.len());
    let lane_started = std::time::Instant::now();
    for (subject, pristine, base, binding, kind, role) in planned {
        let tool = subject.0;
        let label = history_label(subject, damage_label(tool, &binding, kind));
        if let Some(cut) = row_cut(std::time::Instant::now(), lane_started, budget, deadline) {
            // Started and finished unrun, so the lane's announced count is still reached.
            progress.block_started(Category::DamagedFiles, &label);
            progress.block_finished(Category::DamagedFiles, &label, tool == Tool::Forklift);
            gaps.entry(subject).or_default().push(format!("{}: not run: {}", label, cut));
            continue;
        }
        let history = faulted::HistorySubject { tool, pristine, positions: &positions, baseline: base };
        start_block(progress, Category::DamagedFiles, &label)?;
        let result = output.scratch(&label).map_err(|error| error.to_string()).and_then(|scratch| {
            let environment = Environment::scrubbed(&scratch, search_path.clone(), &BTreeMap::new()).map_err(|error| error.to_string())?;
            faulted::run_history(&history, binding.name, kind, programs, &scratch, &environment, HISTORY_LIMIT, seed).map_err(|error| error.to_string())
        });
        progress.block_finished(Category::DamagedFiles, &label, result.as_ref().map_or(true, |run| tool == Tool::Forklift && (run.grade.is_failure() || run.unreached.is_some())));
        match result {
            Ok(run) => {
                write_detail(output.root(), &format!("{}.json", label), &run)?;
                match history_row(subject, &label, &run, &scenario, role) {
                    Ok(row) => rows.push(row),
                    Err(entry) => unreached.entry(subject).or_default().push(entry),
                }
            }
            Err(error) => errors.entry(subject).or_default().push(format!("{} {:?}: {}", binding.name, kind, error)),
        }
    }
    for subject in HISTORY_SUBJECTS {
        let (tool, direction) = subject;
        let cell = if drawn.is_null() {
            Cell::not_run("the history subjects were not built")
        } else if positions.is_empty() {
            Cell::not_run("no position was sampled")
        } else if direction == Some(HistoryDirection::FromForklift) && export_root.is_none() {
            Cell::not_run("git's subject over forklift's export was not built")
        } else if refused_baselines.contains(&subject) {
            Cell::not_run("its unfaulted readback refused")
        } else if !subjects.iter().any(|(built, _, _)| *built == subject) {
            Cell::not_run("its unfaulted readback was not taken")
        } else {
            Cell::Run
        };
        history_checks.push(Completeness { subject: Some(tool), check: history_check("history damage rows ran", subject), cell: cell.clone(), missing: gaps.remove(&subject).unwrap_or_default() });
        // A subject whose rows did not run reached no damage and ran no fault, so its two other
        // checks say so too, rather than reading as run and clean.
        history_checks.push(Completeness {
            subject: Some(tool), check: history_check("damage not reached by the sample", subject), cell: cell.clone(),
            missing: unreached.remove(&subject).unwrap_or_default(),
        });
        // What kept a row from running, the harness's failures with it. Forklift's check stands
        // under forklift, as the fixture lane's does; a git subject's is the run's own. Either
        // way an entry fails the run.
        let check = match tool {
            Tool::Forklift => "history damage faults ran".to_string(),
            other => history_check(&format!("history damage faults ran under {}", other.label()), subject),
        };
        let owner = (tool == Tool::Forklift).then_some(tool);
        // A check with a failure to name ran and found it, whether or not the rows did.
        let missing = errors.remove(&subject).unwrap_or_default();
        let cell = if missing.is_empty() { cell } else { Cell::Run };
        history_checks.push(Completeness { subject: owner, check, cell, missing });
    }
    Ok((rows, history_checks))
}

/// Runs every lane `options` asks for into `options.out`, which must be empty or new, and returns
/// the report it wrote there as `report.json` and `report.md`. A run that is refused or cancelled
/// leaves whatever it wrote before it stopped, but no `report.json`; a run again needs another
/// empty directory.
pub fn run(options: &RunOptions, progress: &dyn Progress) -> Result<Report, RunError> {
    let RunOptions {
        source, out, seed, only, forklift, git, forklift_server, budget, strace, kill, power, power_variants, jobs,
        capped_volume, disk_full, read_only: read_only_lane, folding_volume, network: network_lane, history_from,
    } = options.clone();
    let started = std::time::Instant::now();

    // Ids are checked first, so a mistyped one is refused before anything is written or read.
    for (flag, ids) in [("--kill", &kill), ("--power", &power), ("--disk-full", &disk_full), ("--read-only", &read_only_lane), ("--network", &network_lane)] {
        if let Some(unknown) = ids.iter().find(|id| !scenario::catalog().iter().any(|scenario| &scenario.id == *id)) {
            return Err(RunError::Refused(format!("{} {}: no such scenario", flag, unknown)));
        }
    }
    if let Some(unknown) = only.as_ref().filter(|only| !scenario::known(only)) {
        return Err(RunError::Refused(format!("--only {}: no such scenario", unknown)));
    }
    // The history is read from wherever the trips run, so it is named absolutely; a path that is
    // no folder is refused before anything is written, as is a history with `--only`, which runs
    // no round trip to take it.
    let history_from = match history_from.as_deref().map(std::path::absolute).transpose() {
        Ok(Some(path)) if !path.is_dir() => return Err(RunError::Refused(format!("--history-from {}: not a folder", path.display()))),
        Ok(path) => path,
        Err(error) => return Err(RunError::Refused(format!("--history-from: {}", error))),
    };
    if history_from.is_some() && only.is_some() {
        return Err(RunError::Refused("--history-from feeds the fidelity round trips, which --only leaves out".to_string()));
    }
    let mut output = match Output::claim(&out) {
        Ok(output) => output,
        Err(error) => {
            return Err(RunError::Refused(error.to_string()));
        }
    };

    let census = match Census::load(&source) {
        Ok(census) => census,
        Err(error) => {
            return Err(RunError::Refused(error.to_string()));
        }
    };

    let routes = match crate::routes::RouteTable::load(&source) {
        Ok(routes) => routes,
        Err(error) => {
            return Err(RunError::Refused(error.to_string()));
        }
    };

    let scenarios: Vec<_> = scenario::catalog().into_iter().filter(|scenario| only.as_ref().is_none_or(|only| *only == scenario.id)).collect();
    let corpus = corpus::generate(seed, corpus::Shape::SMALL);
    let mut tools = BTreeMap::new();
    let search_path = crate::scrub::search_path(git.as_deref().and_then(|git| tool::absolute(git).ok()).as_deref(), std::env::var_os("PATH"));
    let mut environment_seen = BTreeMap::new();
    let server = match forklift_server.as_deref().map(tool::absolute).transpose() {
        Ok(server) => server,
        Err(error) => {
            return Err(RunError::Refused(format!("the forklift-server path: {}", error)));
        }
    };
    if let Some(server) = &server {
        tools.insert("forklift-server".to_string(), described(server));
    }

    // The version under test's write-op universe: its schema's command ids and the server's
    // leaves. Without a schema command the harness refuses to run; it keeps no list of its own.
    let program = match forklift.as_deref().map(tool::absolute).transpose() {
        Ok(program) => program,
        Err(error) => {
            return Err(RunError::Refused(format!("the client binary's path: {}", error)));
        }
    };
    let mut asked = |label: &str| -> Result<(crate::scrub::Scratch, Environment), String> {
        let scratch = output.scratch(label).map_err(|error| error.to_string())?;
        let environment = Environment::scrubbed(&scratch, search_path.clone(), &BTreeMap::new()).map_err(|error| error.to_string())?;
        Ok((scratch, environment))
    };
    let limit = std::time::Duration::from_secs(120);
    let schema = match &program {
        None => None,
        Some(program) => match asked("schema").and_then(|(scratch, environment)| writeops::read_schema(program, &scratch, &environment, limit)) {
            Ok(schema) => Some(schema),
            Err(error) => {
                return Err(RunError::Refused(error.to_string()));
            }
        },
    };
    let leaves = match &server {
        None => vec![],
        Some(server) => match asked("server-help").and_then(|(scratch, environment)| writeops::read_server_leaves(server, &scratch, &environment, limit)) {
            Ok(leaves) => leaves,
            Err(error) => {
                return Err(RunError::Refused(error.to_string()));
            }
        },
    };
    let traced = match &strace {
        _ if Platform::current() != Platform::Linux => Err("write-op traces run on Linux only"),
        None => Err("no strace given"),
        Some(strace) => tool::absolute(strace).map_err(|_| "cannot resolve the strace path"),
    };
    let traces = writeops::traces_folder(output.root());
    if let Err(error) = std::fs::create_dir_all(&traces) {
        return Err(RunError::Refused(format!("{}: {}", traces.display(), error)));
    }
    let read_only: std::collections::BTreeSet<String> = crate::bindings::WRITE_EXEMPTIONS.iter()
        .filter(|(_, exemption)| matches!(exemption, writeops::Exemption::ReadOnly(_))).map(|(id, _)| id.to_string()).collect();

    // Each scenario's history built with the forklift under test: what it created, what its walk
    // found that no census row classifies, and what each step wrote. The coverage scenarios are
    // built only for what their steps write.
    let built_scenarios: Vec<_> = scenarios.iter().cloned()
        .chain(scenario::coverage().into_iter().filter(|scenario| only.as_ref().is_none_or(|only| *only == scenario.id))).collect();
    let mut created: BTreeMap<(String, Warehouse), Vec<String>> = BTreeMap::new();
    let mut walk_missing = Vec::new();
    let mut evidence = Vec::new();
    // Every exchange the scenarios' hosts logged while they were built, a failed build's up to its
    // failure, for the route check.
    let mut exchanges = Vec::new();
    // What each traced host showed, and why any host that should have been traced was not.
    let mut servings = Vec::new();
    let mut untraced = Vec::new();
    if let (Some(program), Some(schema)) = (&program, &schema) {
        tools.insert("forklift".to_string(), described(program));
        if let Ok(strace) = &traced {
            tools.insert("strace".to_string(), described(strace));
        }
        for scenario in &built_scenarios {
            if progress.cancelled() {
                return Err(RunError::Cancelled);
            }
            let observer = writeops::Observer::new(traced.as_ref().ok().map(PathBuf::as_path), &traces, schema, &leaves, &read_only, &scenario.id);
            let built = output.scratch(&scenario.id).map_err(|error| error.to_string())
                .and_then(|scratch| {
                    let environment = Environment::scrubbed(&scratch, search_path.clone(), &BTreeMap::new()).map_err(|error| error.to_string())?;
                    environment_seen = environment.variables.clone();
                    runner::build_logged(scenario, &corpus, Tool::Forklift, program, server.as_deref(), &scratch, &environment, limit, Some(&observer), &mut exchanges)
                        .map_err(|error| error.to_string())
                });
            evidence.extend(observer.evidence());
            servings.extend(observer.servings());
            untraced.extend(observer.untraced());
            if let Ok(built) = &built {
                exchanges.extend(built.exchanges());
            }
            match (built, &census) {
                (Ok(built), Some(census)) => {
                    walk_missing.extend(census.unclassified(&built.walked).into_iter().map(|path| format!("{}: {}", scenario.id, path)));
                    walk_missing.extend(census.unclassified(&built.host_walked).into_iter().map(|path| format!("{} host: {}", scenario.id, path)));
                    created.insert((scenario.id.clone(), Warehouse::Client), runner::created_names(census, &built.walked));
                    created.insert((scenario.id.clone(), Warehouse::Host), runner::created_names(census, &built.host_walked));
                }
                (Ok(_), None) => {}
                (Err(error), _) => walk_missing.push(format!("{}: not built: {}", scenario.id, error)),
            }
        }
    }
    let written = serde_json::to_string(&evidence).map_err(|error| error.to_string())
        .and_then(|text| std::fs::write(output.root().join("write-ops.json"), text).map_err(|error| error.to_string()));
    if let Err(error) = written {
        return Err(RunError::Refused(format!("cannot write write-ops.json: {}", error)));
    }
    let written = serde_json::to_string(&exchanges).map_err(|error| error.to_string())
        .and_then(|text| std::fs::write(output.root().join("exchanges.json"), text).map_err(|error| error.to_string()));
    if let Err(error) = written {
        return Err(RunError::Refused(format!("cannot write exchanges.json: {}", error)));
    }
    let written = serde_json::to_string(&servings).map_err(|error| error.to_string())
        .and_then(|text| std::fs::write(output.root().join("server-writes.json"), text).map_err(|error| error.to_string()));
    if let Err(error) = written {
        return Err(RunError::Refused(format!("cannot write server-writes.json: {}", error)));
    }
    let not_run = |reason: &str| ["write-op coverage", "read-only exemptions"].map(|check| Completeness { subject: Some(Tool::Forklift), check: check.to_string(), cell: Cell::not_run(reason), missing: vec![] }).to_vec();
    let write_checks = match (&schema, &server) {
        (None, _) => not_run("no forklift binary given"),
        (Some(_), None) => not_run("no forklift-server binary given"),
        // Run in full only: a filtered run builds too few scenarios to cover the universe.
        (Some(_), Some(_)) if only.is_some() => not_run("--only builds one scenario"),
        (Some(schema), Some(_)) => {
            let mut checks = writeops::check(&writeops::universe(schema, &leaves), &evidence, crate::bindings::WRITE_EXEMPTIONS, traced.as_ref().err().copied());
            // A serve strace could not attach to has no evidence, and the coverage says why.
            if let Some(coverage) = checks.iter_mut().find(|check| check.check == "write-op coverage" && check.cell == Cell::Run) {
                coverage.missing.extend(untraced.iter().map(|reason| format!("forklift-server serve: untraced in {}", reason)));
            }
            checks
        }
    };
    let walk_cell = match (&program, &census) {
        (None, _) => Cell::not_run("no forklift binary given"),
        (Some(_), None) => Cell::not_run("no census for this version"),
        (Some(_), Some(_)) => Cell::Run,
    };
    let walk_check = Completeness { subject: Some(Tool::Forklift), check: "census walk".to_string(), cell: walk_cell, missing: walk_missing };
    // Every scenario hosts in single mode, so the routes are matched under the single prefix.
    let (route_cell, route_missing) = match (&program, &server, &routes) {
        (None, _, _) => (Cell::not_run("no forklift binary given"), vec![]),
        (_, None, _) => (Cell::not_run("no forklift-server binary given"), vec![]),
        (_, _, None) => (Cell::not_run("no route table for this version"), vec![]),
        // Run in full only: a filtered run builds too few scenarios to answer every route.
        _ if only.is_some() => (Cell::not_run("--only builds one scenario"), vec![]),
        (Some(_), Some(_), Some(routes)) => (Cell::Run, crate::routes::check(routes, crate::routes::Mode::Single, &exchanges, crate::bindings::ROUTE_EXEMPTIONS)),
    };
    let route_check = Completeness { subject: Some(Tool::Forklift), check: "server routes answered".to_string(), cell: route_cell, missing: route_missing };
    let (route_write_cell, route_write_missing) = match (&program, &server, &routes, &traced) {
        (None, _, _, _) => (Cell::not_run("no forklift binary given"), vec![]),
        (_, None, _, _) => (Cell::not_run("no forklift-server binary given"), vec![]),
        (_, _, None, _) => (Cell::not_run("no route table for this version"), vec![]),
        (_, _, _, Err(reason)) => (Cell::not_run(reason), vec![]),
        _ if only.is_some() => (Cell::not_run("--only builds one scenario"), vec![]),
        _ if !untraced.is_empty() => (Cell::not_run(&untraced.join("; ")), vec![]),
        (Some(_), Some(_), Some(routes), Ok(_)) => (Cell::Run, crate::routewrites::check(routes, crate::routes::Mode::Single, &servings,
            crate::bindings::ROUTE_WRITES, crate::bindings::BACKGROUND_WRITES, crate::bindings::ADMISSION_WRITES)),
    };
    let route_write_check = Completeness { subject: Some(Tool::Forklift), check: "server route writes".to_string(), cell: route_write_cell, missing: route_write_missing };

    let damage_check = match &census {
        None => Completeness { subject: Some(Tool::Forklift), check: "damage bindings".to_string(), cell: Cell::not_run("no census for this version"), missing: vec![] },
        Some(census) => {
            let missing = crate::bindings::check(census, crate::bindings::BINDINGS, crate::bindings::EXEMPTIONS, &scenario::catalog(), &created);
            Completeness { subject: Some(Tool::Forklift), check: "damage bindings".to_string(), cell: Cell::Run, missing }
        }
    };

    // Git's own layout, walked after each scenario git can run is built with it, at its defaults
    // and tuned, and so is the bare repository of one that hosts: every path no layout row
    // classifies, what each build created for that subject's damage bindings, and the role map's
    // gaps against the census.
    let git_runnable: Vec<String> = scenario::catalog().into_iter().filter(|scenario| scenario.tools().contains(&Tool::Git)).map(|scenario| scenario.id).collect();
    let git_program = git.as_deref().map(tool::absolute).transpose().map_err(|error| RunError::Refused(format!("the git path: {}", error)))?;
    let daemon = git_program.as_deref().is_some_and(runner::has_fsmonitor_daemon);
    let mut configurations = BTreeMap::new();
    if let Some(git_program) = &git_program {
        tools.insert(Tool::Git.label().to_string(), described(git_program));
        configurations.insert(Tool::Git.label().to_string(), created_configuration());
        configurations.insert(Tool::GitTuned.label().to_string(), format!("{}; {}", created_configuration(), tuned_configuration(daemon)));
    }
    let mut git_checks = Vec::new();
    for subject in [Tool::Git, Tool::GitTuned] {
        let mut walks: BTreeMap<(String, Warehouse), Vec<String>> = BTreeMap::new();
        let mut walk_missing = Vec::new();
        if let Some(git_program) = &git_program {
            for scenario in scenarios.iter().filter(|scenario| git_runnable.contains(&scenario.id)) {
                let label = format!("{}-{}", subject.label(), scenario.id);
                let built = in_area(&mut output, &label, &search_path, |scratch, environment| {
                    runner::build(scenario, &corpus, subject, git_program, None, scratch, environment, limit, None).map_err(|error| error.to_string())
                });
                match built {
                    Ok(built) => {
                        walk_missing.extend(crate::git_layout::unclassified(&built.walked).into_iter().map(|path| format!("{}: {}", scenario.id, path)));
                        // The bare repository a hosting history pushed to, laid out as a `.git` is.
                        if built.bare.is_some() {
                            walk_missing.extend(crate::git_layout::unclassified(&built.host_walked).into_iter().map(|path| format!("{} host: {}", scenario.id, path)));
                            walks.insert((scenario.id.clone(), Warehouse::Host), built.host_walked);
                        }
                        walks.insert((scenario.id.clone(), Warehouse::Client), built.walked);
                    }
                    Err(error) => walk_missing.push(format!("{}: not built: {}", scenario.id, error)),
                }
            }
        }
        let git_cell = match &git_program { None => Cell::not_run("no git binary given"), Some(_) => Cell::Run };
        git_checks.push(Completeness { subject: Some(subject), check: "git layout walk".to_string(), cell: git_cell.clone(), missing: walk_missing });
        // Run in full only: a filtered run walks too few scenarios to answer every binding.
        let (damage_cell, damage_missing) = match &git_program {
            None => (git_cell, vec![]),
            Some(_) if only.is_some() => (Cell::not_run("--only builds one scenario"), vec![]),
            Some(_) => {
                let (bindings, exemptions) = crate::bindings::git_subject(subject, daemon);
                (git_cell, crate::bindings::check_git(&bindings, &exemptions, &git_runnable, &walks))
            }
        };
        git_checks.push(Completeness { subject: Some(subject), check: "git damage bindings".to_string(), cell: damage_cell, missing: damage_missing });
    }
    let role_check = match &census {
        None => Completeness { subject: None, check: "role map".to_string(), cell: Cell::not_run("no census for this version"), missing: vec![] },
        Some(census) => Completeness { subject: None, check: "role map".to_string(), cell: Cell::Run, missing: crate::git_layout::role_gaps(census, crate::git_layout::CENSUS_ROLES) },
    };

    // Every damage binding, every kind it names, each on a fresh build of its scenario: forklift's
    // over its census, then git at its defaults and tuned over the git layout.
    let mut rows = Vec::new();
    let mut fault_errors: BTreeMap<Tool, Vec<String>> = BTreeMap::new();
    let mut damage_blocks = 0;
    let damaged_cell = match (&forklift, &git, &census) {
        (None, _, _) => Cell::not_run("no forklift binary given"),
        (_, None, _) => Cell::not_run("no git binary given to lay out the readback"),
        (_, _, None) => Cell::not_run("no census for this version"),
        (Some(forklift), Some(git), Some(_)) => {
            let (Ok(forklift), Ok(git)) = (tool::absolute(forklift), tool::absolute(git)) else {
                return Err(RunError::Refused("cannot resolve the tool paths".to_string()));
            };
            let programs = Programs { forklift, git, server: server.clone() };
            let faulted_corpus = faulted_corpus(seed);
            let subjects = [
                (Tool::Forklift, crate::bindings::BINDINGS.to_vec()),
                (Tool::Git, crate::bindings::git_subject(Tool::Git, daemon).0),
                (Tool::GitTuned, crate::bindings::git_subject(Tool::GitTuned, daemon).0),
            ];
            let picked_bindings = || subjects.iter().flat_map(|(subject, bindings)| bindings.iter().map(move |binding| (*subject, binding)))
                .filter_map(|(subject, binding)| scenarios.iter().find(|scenario| scenario.id == binding.scenario).map(|scenario| (subject, binding, scenario)));
            for (subject, binding, _) in picked_bindings() {
                let errors = fault_errors.entry(subject).or_default();
                errors.extend(role_or_refusal(subject, binding.name).err());
            }
            let roled = || picked_bindings().filter_map(|(subject, binding, scenario)| {
                role_or_refusal(subject, binding.name).ok().map(|role| (subject, binding, scenario, role))
            });
            damage_blocks = roled().map(|(_, binding, _, _)| binding.kinds.len()).sum();
            progress.lane(Category::DamagedFiles, Some(damage_blocks));
            for (subject, binding, scenario, role) in roled() {
                for kind in binding.kinds {
                    let label = damage_label(subject, binding, *kind);
                    start_block(progress, Category::DamagedFiles, &label)?;
                    let result = output.scratch(&label).map_err(|error| error.to_string()).and_then(|scratch| {
                        let environment = Environment::scrubbed(&scratch, search_path.clone(), &BTreeMap::new()).map_err(|error| error.to_string())?;
                        faulted::run(scenario, binding.name, binding.warehouse, *kind, subject, &faulted_corpus, &programs, &scratch, &environment, std::time::Duration::from_secs(120), seed)
                            .map_err(|error| error.to_string())
                    });
                    // A git subject's grade is a comparison, never a failure of the run.
                    progress.block_finished(Category::DamagedFiles, &label, result.as_ref().map_or(true, |run| subject == Tool::Forklift && run.grade.is_failure()));
                    match result {
                        Ok(run) => {
                            write_detail(output.root(), &format!("{}.json", label), &run)?;
                            rows.push(Row {
                                subject,
                                scenario: format!("{} {}{} {:?}", run.scenario, if run.warehouse == Warehouse::Host { "host:" } else { "" }, run.target, run.kind),
                                category: if run.kind.plants() { Category::DeadPidLock } else { Category::DamagedFiles },
                                binding: subject.label().to_string(),
                                grade: run.grade,
                                recoverability: run.recoverability,
                                note: (!run.unextractable.is_empty()).then(|| format!("unextractable remedies: {}", run.unextractable.join("; "))),
                                detail: Some(format!("{}.json#", label)),
                                pairing: Some(Pairing { scenario: run.scenario.clone(), target: Target::File { side: run.warehouse, role, kind: run.kind }, directions: vec![] }),
                            });
                        }
                        Err(error) => fault_errors.entry(subject).or_default().push(format!("{} {:?}: {}", binding.name, kind, error)),
                    }
                }
            }
            Cell::Run
        }
    };
    // One check per subject that ran; forklift's even when it did not. A fault that could not run
    // under a git subject is a fault of the harness, not a gap in git's measure, so its check is
    // the run's own and fails the run.
    fault_errors.entry(Tool::Forklift).or_default();
    let fault_checks: Vec<Completeness> = fault_errors.into_iter().map(|(subject, missing)| match subject {
        Tool::Forklift => Completeness { subject: Some(subject), check: "damaged-file faults ran".to_string(), cell: damaged_cell.clone(), missing },
        Tool::Git | Tool::GitTuned => Completeness {
            subject: None, check: format!("damaged-file faults ran under {}", subject.label()), cell: damaged_cell.clone(), missing,
        },
    }).collect();

    // Disk full on the capped volume and a read-only work folder, each under the faulted command of
    // the scenarios named for it, lifted before the grading readback.
    let picked = |named: &[String]| -> Vec<&scenario::Scenario> { scenarios.iter().filter(|scenario| named.contains(&scenario.id)).collect() };
    let resolved = match (&forklift, &git) {
        (None, _) => Err("no forklift binary given"),
        (_, None) => Err("no git binary given to lay out the readback"),
        (Some(forklift), Some(git)) => match (tool::absolute(forklift), tool::absolute(git)) {
            (Ok(forklift), Ok(git)) => Ok(Programs { forklift, git, server: server.clone() }),
            _ => {
                return Err(RunError::Refused("cannot resolve the tool paths".to_string()));
            }
        },
    };
    let mut condition_errors: BTreeMap<Category, Vec<String>> = BTreeMap::new();
    let mut lifted = Vec::new();
    let disk_full_cell = match (&resolved, &capped_volume) {
        (Err(reason), _) => Cell::not_run(reason),
        _ if Platform::current() == Platform::Windows => Cell::not_run("no capped volume is built on Windows in this harness"),
        (_, None) => Cell::not_run("no capped volume given"),
        _ if picked(&disk_full).is_empty() => Cell::not_run("no scenario this run picked was named for it"),
        (Ok(programs), Some(volume)) => {
            let mut capped = match Output::claim(volume) {
                Ok(capped) => capped,
                Err(error) => {
                    return Err(RunError::Refused(format!("the capped volume: {}", error)));
                }
            };
            let ballast = capped.root().join("ballast");
            progress.lane(Category::DiskFull, None);
            let errors = condition_errors.entry(Category::DiskFull).or_default();
            for scenario in picked(&disk_full) {
                stop_if_cancelled(progress)?;
                let consumed = discarded_area(&mut capped, &format!("{}-counted", scenario.id), &search_path, |scratch, environment|
                    crate::conditions::consumed(scenario, &faulted_corpus(seed), programs, scratch, environment, limit, &ballast));
                let consumed = match consumed {
                    Ok(consumed) => consumed,
                    Err(error) => {
                        errors.push(format!("{} disk full: {}", scenario.id, error));
                        continue;
                    }
                };
                for free in crate::conditions::points(consumed) {
                    let block = format!("{}-free-{}", scenario.id, free);
                    start_block(progress, Category::DiskFull, &block)?;
                    let result = discarded_area(&mut capped, &block, &search_path, |scratch, environment|
                        crate::conditions::disk_full(scenario, &faulted_corpus(seed), programs, scratch, environment, limit, &ballast, free)
                            .map_err(|error| error.to_string()));
                    // A fault that failed part-way may have left its ballast; the next point fills afresh.
                    let _ = crate::damage::apply(&ballast, crate::damage::Kind::Delete, 0);
                    progress.block_finished(Category::DiskFull, &block, result.as_ref().map_or(true, |run| run.graded.grade.is_failure()));
                    match result {
                        Ok(run) => lifted.push((Category::DiskFull, format!("of {} B the faulted command consumes", consumed), run)),
                        Err(error) => errors.push(format!("{} free {} B: {}", scenario.id, free, error)),
                    }
                }
            }
            // A few points of a continuum of free space, so sampled; none graded is none run.
            if lifted.iter().any(|(category, _, _)| *category == Category::DiskFull) { Cell::Sampled } else { Cell::not_run("no disk-full fault graded") }
        }
    };
    let mut writable = None;
    let read_only_cell = match &resolved {
        Err(reason) => Cell::not_run(reason),
        _ if picked(&read_only_lane).is_empty() => Cell::not_run("no scenario this run picked was named for it"),
        Ok(programs) => {
            progress.lane(Category::ReadOnly, Some(picked(&read_only_lane).len()));
            let errors = condition_errors.entry(Category::ReadOnly).or_default();
            for scenario in picked(&read_only_lane) {
                let block = format!("{}-read-only", scenario.id);
                start_block(progress, Category::ReadOnly, &block)?;
                let result = output.scratch(&format!("{}-read-only", scenario.id)).map_err(|error| error.to_string()).and_then(|scratch| {
                    let environment = Environment::scrubbed(&scratch, search_path.clone(), &BTreeMap::new()).map_err(|error| error.to_string())?;
                    crate::conditions::read_only(scenario, &faulted_corpus(seed), programs, &scratch, &environment, limit).map_err(|error| error.to_string())
                });
                progress.block_finished(Category::ReadOnly, &block, match &result {
                    Ok(run) => run.as_ref().is_some_and(|run| run.graded.grade.is_failure()),
                    Err(_) => true,
                });
                match result {
                    Ok(Some(run)) => lifted.push((Category::ReadOnly, "write bits cleared on the work folder, no unprivileged remount".to_string(), run)),
                    Ok(None) => writable = Some(crate::conditions::STAYS_WRITABLE),
                    Err(error) => errors.push(format!("{} read-only: {}", scenario.id, error)),
                }
            }
            match writable {
                _ if lifted.iter().any(|(category, _, _)| *category == Category::ReadOnly) => Cell::Run,
                Some(reason) => Cell::not_run(reason),
                None => Cell::not_run("no read-only fault graded"),
            }
        }
    };
    // One fault of each kind the proxy injects, at a seeded request of the faulted command of each
    // scenario named for it, each on a fresh build.
    let mut networked = Vec::new();
    let network_cell = match &resolved {
        Err(reason) => Cell::not_run(reason),
        Ok(Programs { server: None, .. }) => Cell::not_run("no forklift-server binary given"),
        _ if picked(&network_lane).is_empty() => Cell::not_run("no scenario this run picked was named for it"),
        Ok(programs) => {
            progress.lane(Category::Network, None);
            let errors = condition_errors.entry(Category::Network).or_default();
            for scenario in picked(&network_lane) {
                stop_if_cancelled(progress)?;
                let counted = in_area(&mut output, &format!("{}-network-counted", scenario.id), &search_path, |scratch, environment|
                    crate::network::count(scenario, &faulted_corpus(seed), programs, scratch, environment, limit).map_err(|error| error.to_string()));
                let requests = match counted {
                    Ok(requests) => requests,
                    Err(error) => {
                        errors.push(format!("{} network: {}", scenario.id, error));
                        continue;
                    }
                };
                for point in crate::network::points(requests, seed, &scenario.id) {
                    let label = format!("{}-network-{:?}-{}", scenario.id, point.fault, point.at);
                    start_block(progress, Category::Network, &label)?;
                    let result = in_area(&mut output, &label, &search_path, |scratch, environment|
                        crate::network::network(scenario, &faulted_corpus(seed), programs, scratch, environment, limit, point).map_err(|error| error.to_string()));
                    progress.block_finished(Category::Network, &label, result.as_ref().map_or(true, |run| run.graded.grade.is_failure()));
                    match result {
                        Ok(run) => networked.push(run),
                        Err(error) => errors.push(format!("{} {} at request {}: {}", scenario.id, point.fault.label(), point.at, error)),
                    }
                }
            }
            // A few requests of each faulted command, so sampled; none graded is none run.
            if networked.is_empty() { Cell::not_run("no network fault graded") } else { Cell::Sampled }
        }
    };
    for run in networked {
        let label = format!("{}-network-{:?}-{}", run.scenario, run.point.fault, run.point.at);
        write_detail(output.root(), &format!("{}.json", label), &run)?;
        let mut note = "through the harness's proxy, one request at a time".to_string();
        match &run.restart {
            Some(crate::network::Restart { refused: None }) => note.push_str("; the killed server restarted before the readback"),
            Some(crate::network::Restart { refused: Some(said) }) => note.push_str(&format!("; the killed server would not start again: {}", said.trim())),
            None => {}
        }
        if run.point.fault == crate::proxy::Fault::Reset && Platform::current() == Platform::Windows {
            note.push_str("; a plain close on Windows, where no linger is set");
        }
        if !run.graded.unextractable.is_empty() {
            note.push_str(&format!("; unextractable remedies: {}", run.graded.unextractable.join("; ")));
        }
        rows.push(Row { subject: Tool::Forklift,
            scenario: run.label(), category: Category::Network, binding: "forklift".to_string(),
            grade: run.graded.grade, recoverability: run.graded.recoverability, note: Some(note),
            detail: Some(format!("{}.json#/graded", label)),
            pairing: None,
        });
    }

    for (category, note, run) in lifted {
        let label = format!("{}-{}", run.scenario, run.condition).replace(' ', "-");
        write_detail(output.root(), &format!("{}.json", label), &run)?;
        let mut note = note;
        if !run.graded.unextractable.is_empty() {
            note.push_str(&format!("; unextractable remedies: {}", run.graded.unextractable.join("; ")));
        }
        rows.push(Row { subject: Tool::Forklift,
            scenario: format!("{} {}", run.scenario, run.condition), category, binding: "forklift".to_string(),
            grade: run.graded.grade, recoverability: run.graded.recoverability, note: Some(note),
            detail: Some(format!("{}.json#/graded", label)),
            pairing: None,
        });
    }

    // A work tree on a folding volume, standing throughout: each volume fault runs its own
    // scenario, whose files hold only that fault, so a cell's grade answers that fault alone; each
    // runs only where the probe found its property.
    let volume_faults = [(Category::CaseFolding, scenario::Fold::Case), (Category::NoExec, scenario::Fold::Exec)];
    let folding = match (&resolved, &folding_volume) {
        (Ok(_), Some(volume)) if Platform::current() != Platform::Windows => {
            let folding = match Output::claim(volume) {
                Ok(folding) => folding,
                Err(error) => {
                    return Err(RunError::Refused(format!("the folding volume: {}", error)));
                }
            };
            match crate::volume::probe(folding.root()) {
                Ok(probed) => Some((folding, probed)),
                Err(error) => {
                    return Err(RunError::Refused(format!("the folding volume: {}", error)));
                }
            }
        }
        _ => None,
    };
    stop_if_cancelled(progress)?;
    let mut volume_lane = |(category, fold): (Category, scenario::Fold)| -> Result<Cell, RunError> {
        let scenario = scenarios.iter().find(|scenario| scenario.files == scenario::Files::Folded(fold));
        let (programs, (folding, probed), scenario) = match (&resolved, &folding, scenario) {
            (Err(reason), _, _) => return Ok(Cell::not_run(reason)),
            _ if Platform::current() == Platform::Windows => return Ok(Cell::not_run("no folding volume is built on Windows in this harness")),
            (_, None, _) => return Ok(Cell::not_run("no folding volume given")),
            (_, _, None) => return Ok(Cell::not_run("no scenario this run picked holds this volume fault's files")),
            (Ok(programs), Some(folding), Some(scenario)) => (programs, folding, scenario),
        };
        if let Some(reason) = crate::volume::not_run_because(probed, category) {
            return Ok(Cell::not_run(reason));
        }
        let block = format!("{}-folding", scenario.id);
        progress.lane(category, Some(1));
        start_block(progress, category, &block)?;
        let result = output.scratch(&block).map_err(|error| error.to_string()).and_then(|scratch| {
            let scratch = scratch.with_folding_volume(folding.root());
            let environment = Environment::scrubbed(&scratch, search_path.clone(), &BTreeMap::new()).map_err(|error| error.to_string())?;
            crate::volume::folded(scenario, &faulted_corpus(seed), programs, &scratch, &environment, limit).map_err(|error| error.to_string())
        });
        progress.block_finished(category, &block, result.as_ref().map_or(true, |graded| graded.grade.is_failure()));
        match result {
            Ok(graded) => {
                write_detail(output.root(), &format!("{}-folding-volume.json", scenario.id), &graded)?;
                let mut note = format!("work tree cloned onto the folding volume ({}, {}), standing through the retry, remedies and heal",
                    if probed.folds_case { "folds case" } else { "keeps case" },
                    if probed.keeps_executable_bit { "keeps the executable bit" } else { "no executable bit" });
                if !graded.unextractable.is_empty() {
                    note.push_str(&format!("; unextractable remedies: {}", graded.unextractable.join("; ")));
                }
                rows.push(Row { subject: Tool::Forklift,
                    scenario: format!("{} on a {}", scenario.id, category.label()), category, binding: "forklift".to_string(),
                    grade: graded.grade, recoverability: graded.recoverability, note: Some(note),
                    detail: Some(format!("{}-folding-volume.json#", scenario.id)),
                    pairing: None,
                });
                Ok(Cell::Run)
            }
            Err(error) => {
                condition_errors.entry(category).or_default().push(format!("{} on the folding volume: {}", scenario.id, error));
                Ok(Cell::not_run("no volume fault graded"))
            }
        }
    };
    // One after the other, so a cancel between them stops the second.
    let case_folding_cell = volume_lane(volume_faults[0])?;
    let no_exec_cell = volume_lane(volume_faults[1])?;

    // Each fixture history round-tripped, git to Forklift to git and Forklift to git to Forklift,
    // and the history `--history-from` names both ways too: what a trip changed or dropped is
    // findings, never grades. A round trip that cannot finish is a gap in the run.
    let mut fidelity = Vec::new();
    let mut fidelity_missing = Vec::new();
    let mut history_record = None;
    let mut history_mirror = None;
    let fidelity_cell = match (&program, &git_program) {
        (None, _) => Cell::not_run("no forklift binary given"),
        (_, None) => Cell::not_run("no git binary given"),
        _ if only.is_some() => Cell::not_run("--only picks one scenario, and the fidelity round trips are none"),
        (Some(program), Some(git_program)) => {
            use crate::fidelity::{forklift_round_trip, git_round_trip, Direction, Trip};
            type RoundTrip = fn(&std::path::Path, &std::path::Path, Option<&std::path::Path>, &crate::scrub::Scratch, &Environment, std::time::Duration) -> Trip;
            let mut trips: Vec<(&str, Direction, RoundTrip, Option<&std::path::Path>)> = vec![
                ("fidelity-git-forklift-git", Direction::GitForkliftGit, git_round_trip, None),
                ("fidelity-forklift-git-forklift", Direction::ForkliftGitForklift, forklift_round_trip, None),
            ];
            // The history is mirrored once, in a block of its own, and both its trips copy that
            // mirror into their own scratch areas, so the record names what they compared; one
            // the copy refuses is a gap, and its trips are not run. The lane's count is announced
            // again once that is known.
            progress.lane(Category::Fidelity, Some(trips.len() + if history_from.is_some() { 3 } else { 0 }));
            if let Some(history) = history_from.as_deref() {
                const BLOCK: &str = "fidelity-history";
                start_block(progress, Category::Fidelity, BLOCK)?;
                let copy = output.scratch(BLOCK).map_err(|error| error.to_string()).and_then(|scratch| {
                    let environment = Environment::scrubbed(&scratch, search_path.clone(), &BTreeMap::new()).map_err(|error| error.to_string())?;
                    Ok(crate::fidelity::take_history(git_program, history, &scratch, &environment, HISTORY_LIMIT))
                });
                let copied = match copy {
                    Ok(copy) => {
                        write_detail(output.root(), &format!("{}.json", BLOCK), &copy)?;
                        copy.copied
                    }
                    Err(error) => Err(error),
                };
                progress.block_finished(Category::Fidelity, BLOCK, copied.is_err());
                match copied {
                    Ok((mirror, described)) => {
                        history_record = Some(format!("{} ({})", history.display(), described));
                        history_mirror = Some(mirror);
                    }
                    Err(error) => fidelity_missing.push(format!("{}: {}", crate::fidelity::Input::History.label(), error)),
                }
            }
            if let Some(mirror) = history_mirror.as_deref() {
                trips.push(("fidelity-history-git-forklift-git", Direction::GitForkliftGit, git_round_trip, Some(mirror)));
                trips.push(("fidelity-history-forklift-git-forklift", Direction::ForkliftGitForklift, forklift_round_trip, Some(mirror)));
            }
            if history_from.is_some() {
                progress.lane(Category::Fidelity, Some(trips.len() + 1));
            }
            let mut finished = 0;
            for (block, direction, round_trip, history) in trips.iter().copied() {
                start_block(progress, Category::Fidelity, block)?;
                let trip = output.scratch(block).map_err(|error| error.to_string()).and_then(|scratch| {
                    let environment = Environment::scrubbed(&scratch, search_path.clone(), &BTreeMap::new()).map_err(|error| error.to_string())?;
                    // A real history's import and export take minutes, not seconds: its commands
                    // get `HISTORY_LIMIT` each, the fixtures' the run's own limit.
                    Ok(round_trip(program, git_program, history, &scratch, &environment, if history.is_some() { HISTORY_LIMIT } else { limit }))
                });
                let trip = match trip {
                    Ok(trip) => {
                        write_detail(output.root(), &format!("{}.json", block), &trip)?;
                        trip.findings
                    }
                    Err(error) => Err(error),
                };
                progress.block_finished(Category::Fidelity, block, trip.is_err());
                match trip {
                    Ok(findings) => {
                        finished += 1;
                        fidelity.extend(findings);
                    }
                    Err(error) => fidelity_missing.push(format!("{} from {}: {}", direction.label(), crate::fidelity::Input::of(history).label(), error)),
                }
            }
            // A trip that did not finish leaves the others' findings standing: the lane is
            // partial, and `fidelity_missing` names each trip it lacks.
            match (fidelity_missing.len(), finished) {
                (0, _) => Cell::Run,
                (_, 0) => Cell::not_run("no round trip finished"),
                _ => Cell::Sampled,
            }
        }
    };

    // Damage over the real history, from the mirror the fidelity lane took: git's subject and
    // forklift's built once from it, the sample drawn over the refs both hold, each subject's
    // unfaulted readback taken on a copy, then one row per (role, kind) per subject, every row on
    // its own copy, in seed order until the lane's wall budget is spent.
    let mut history_checks = Vec::new();
    let history_cell = match (&history_mirror, &resolved, &census) {
        _ if history_from.is_none() => Cell::not_run("no --history-from given"),
        (_, Err(reason), _) => Cell::not_run(reason),
        (None, _, _) => Cell::not_run("the history was not copied"),
        (_, _, None) => Cell::not_run("no census for this version"),
        (Some(mirror), Ok(programs), Some(census)) => {
            // No row starts past the run's own budget, so a run given less cuts rows, each named;
            // the row running when it is spent, and the lane's build, may still take it over.
            let deadline = run_deadline(started, budget);
            let (history_rows, checks) = history_damage(mirror, programs, census, seed, HISTORY_DAMAGE_BUDGET, deadline, damage_blocks, &mut output, &search_path, progress)?;
            rows.extend(history_rows);
            history_checks.extend(checks);
            Cell::Run
        }
    };
    if !matches!(history_cell, Cell::Run) {
        history_checks.push(Completeness { subject: None, check: "history damage rows ran".to_string(), cell: history_cell, missing: vec![] });
    }

    let condition_checks: Vec<Completeness> = [
        (Category::DiskFull, "disk-full faults ran", &disk_full_cell), (Category::ReadOnly, "read-only faults ran", &read_only_cell),
        (Category::CaseFolding, "case-folding faults ran", &case_folding_cell), (Category::NoExec, "no-exec faults ran", &no_exec_cell),
        (Category::Network, "network faults ran", &network_cell),
    ].into_iter().map(|(category, check, cell)| Completeness {
        subject: Some(Tool::Forklift),
            check: check.to_string(), cell: cell.clone(), missing: condition_errors.remove(&category).unwrap_or_default(),
        }).chain(std::iter::once(Completeness {
            // Git's host is a bare repository reached by its path, with no server between: the
            // proxy has nothing of git's to fault.
            subject: Some(Tool::Git), check: "network faults ran".to_string(), missing: vec![],
            cell: match &git_program {
                None => Cell::not_run("no git binary given"),
                Some(_) => Cell::not_run("the harness's proxy faults an HTTP server, and serving git over HTTP needs a CGI backend the harness does not run"),
            },
        })).collect();

    // Every scenario's faulted command killed at each call that changes or syncs a file, and its
    // power loss at each of those moments, Linux only.
    let mut kill_completeness = Vec::new();
    let mut server_kill_completeness = Vec::new();
    let mut server_not_reached = 0;
    let mut power_completeness = Vec::new();
    let mut server_power_completeness = Vec::new();
    let mut server_power_not_reached = 0;
    let mut not_reached = 0;
    let traced_lanes = match (&forklift, &git, &strace) {
        _ if Platform::current() != Platform::Linux => Err(("kill points run on Linux only", "power loss runs on Linux only")),
        (None, _, _) => Err(("no forklift binary given", "no forklift binary given")),
        (_, None, _) => Err(("no git binary given to lay out the readback", "no git binary given to lay out the readback")),
        (_, _, None) => Err(("no strace given", "no strace given")),
        (Some(forklift), Some(git), Some(strace)) => {
            let (Ok(forklift), Ok(git), Ok(strace)) = (tool::absolute(forklift), tool::absolute(git), tool::absolute(strace)) else {
                return Err(RunError::Refused("cannot resolve the tool paths".to_string()));
            };
            tools.insert("strace".to_string(), described(&strace));
            let programs = Programs { forklift, git, server: server.clone() };
            let faulted_corpus = faulted_corpus(seed);
            let settings = kill::Settings {
                strace: &strace, search_path: search_path.clone(), limit: std::time::Duration::from_secs(120),
                jobs: jobs.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from)), traces: &traces,
                schema: schema.as_ref().expect("a forklift binary was given, so its schema was read"),
            };
            // One template and one counting run per scenario feed both lanes; the write dumps are
            // taken only when the power loss will read them.
            let (mut all_kills, mut all_power, mut all_server_kills, mut all_server_power) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            // Each lane's total is the scenarios it will run, one block per scenario.
            let hosted = |scenario: &&scenario::Scenario| server.is_some() && scenario.hosts();
            let killed = |scenario: &&scenario::Scenario| kill::lanes(&kill, &power, &scenario.id).0;
            let powered = |scenario: &&scenario::Scenario| kill::lanes(&kill, &power, &scenario.id).1;
            // Each git subject a scenario binds is a block of its own in each lane, beside
            // forklift's.
            let git_subjects = |scenario: &scenario::Scenario| scenario.tools().into_iter().filter(|tool| *tool != Tool::Forklift).collect::<Vec<_>>();
            progress.lane(Category::KillPointsClient, Some(scenarios.iter().filter(killed).map(|scenario| 1 + git_subjects(scenario).len()).sum()));
            progress.lane(Category::PowerLossClient, Some(scenarios.iter().filter(powered).map(|scenario| 1 + git_subjects(scenario).len()).sum()));
            progress.lane(Category::KillPointsServer, Some(scenarios.iter().filter(killed).filter(hosted).count()));
            progress.lane(Category::PowerLossServer, Some(scenarios.iter().filter(powered).filter(hosted).count()));
            let kills_failed = |kills: &kill::ScenarioKills| !kills.errors.is_empty() || !kills.refusals.is_empty()
                || kills.killed.iter().any(|killed| killed.graded.as_ref().is_some_and(|graded| graded.grade.is_failure()));
            let power_failed = |power: &crate::power::ScenarioPower| !power.errors.is_empty() || !power.refusals.is_empty()
                || power.crashes.iter().any(|crash| crash.graded.grade.is_failure());
            for scenario in &scenarios {
                let (killing, powering) = kill::lanes(&kill, &power, &scenario.id);
                if !killing && !powering {
                    continue;
                }
                // The host's server killed while the faulted command runs, on fresh builds of its own.
                if killing && server.is_some() && scenario.hosts() {
                    start_block(progress, Category::KillPointsServer, &scenario.id)?;
                    let served = output.scratch(&format!("{}-served", scenario.id)).map_err(|error| error.to_string())
                        .and_then(|template| crate::servekill::count(scenario, &faulted_corpus, &programs, &settings, &template, routes.as_ref()).map(|counting| (template, counting)).map_err(|error| error.to_string()));
                    let kills = match served {
                        Ok((template, counting)) => crate::servekill::run(scenario, &programs, &settings, &template, &counting),
                        Err(error) => kill::ScenarioKills { scenario: scenario.id.clone(), errors: vec![error], ..Default::default() },
                    };
                    write_detail(output.root(), &kill::details_file(kill::Side::Server, &scenario.id), &kills)?;
                    progress.block_finished(Category::KillPointsServer, &scenario.id, kills_failed(&kills));
                    all_server_kills.push(kills);
                }
                // The host's machine losing power while the faulted command runs, on fresh builds too.
                if powering && server.is_some() && scenario.hosts() {
                    start_block(progress, Category::PowerLossServer, &scenario.id)?;
                    let lost = match output.scratch(&format!("{}-spowered", scenario.id)) {
                        Ok(template) => crate::servepower::run(scenario, &faulted_corpus, &programs, &settings, &template, routes.as_ref(), power_variants, seed),
                        Err(error) => crate::power::ScenarioPower { scenario: scenario.id.clone(), errors: vec![error.to_string()], ..Default::default() },
                    };
                    write_detail(output.root(), &crate::power::details_file(kill::Side::Server, &scenario.id), &lost)?;
                    progress.block_finished(Category::PowerLossServer, &scenario.id, power_failed(&lost));
                    all_server_power.push(lost);
                }
                // The same faulted command run as each git subject, killed at its own points and
                // losing power in its own run. A count that fails is the run's own failure, as
                // forklift's is.
                for subject in git_subjects(scenario) {
                    let block = format!("{}-{}", subject.label(), scenario.id);
                    // One poll for both blocks, so a cancel never leaves one started alone.
                    stop_if_cancelled(progress)?;
                    if killing {
                        progress.block_started(Category::KillPointsClient, &block);
                    }
                    if powering {
                        progress.block_started(Category::PowerLossClient, &block);
                    }
                    let counted = output.scratch(&format!("{}-traced", block)).map_err(|error| error.to_string())
                        .and_then(|template| kill::count(scenario, &faulted_corpus, &programs, &settings, &template, powering, subject).map(|counting| (template, counting)).map_err(|error| error.to_string()));
                    let kill_error = |error: String| kill::ScenarioKills { scenario: scenario.id.clone(), subject, errors: vec![error], ..Default::default() };
                    let power_error = |error: String| crate::power::ScenarioPower { scenario: scenario.id.clone(), subject, errors: vec![error], ..Default::default() };
                    let (kills, lost) = match &counted {
                        Err(error) => (killing.then(|| kill_error(error.clone())), powering.then(|| power_error(error.clone()))),
                        Ok((template, counting)) => (
                            killing.then(|| kill::run(scenario, &programs, &settings, template, counting, subject).unwrap_or_else(|error| kill_error(error.to_string()))),
                            powering.then(|| crate::power::run(scenario, &programs, &settings, template, counting, power_variants, seed, subject).unwrap_or_else(|error| power_error(error.to_string()))),
                        ),
                    };
                    // A git grade never fails the run; only what failed the harness's own look at
                    // git does.
                    // Each started block is finished before a details file that failed refuses the
                    // run, so none is left running.
                    let mut written = Ok(());
                    if let Some(kills) = kills {
                        let detail = write_detail(output.root(), &kill::details_file_of(kill::Side::Client, subject, &scenario.id), &kills);
                        progress.block_finished(Category::KillPointsClient, &block, detail.is_err() || !kills.errors.is_empty() || !kills.refusals.is_empty());
                        written = written.and(detail);
                        all_kills.push(kills);
                    }
                    if let Some(lost) = lost {
                        let detail = write_detail(output.root(), &crate::power::details_file_of(kill::Side::Client, subject, &scenario.id), &lost);
                        progress.block_finished(Category::PowerLossClient, &block, detail.is_err() || !lost.errors.is_empty() || !lost.refusals.is_empty());
                        written = written.and(detail);
                        all_power.push(lost);
                    }
                    written?;
                }
                // One poll for both client blocks, so a cancel never leaves one started alone.
                stop_if_cancelled(progress)?;
                if killing {
                    progress.block_started(Category::KillPointsClient, &scenario.id);
                }
                if powering {
                    progress.block_started(Category::PowerLossClient, &scenario.id);
                }
                let counted = output.scratch(&format!("{}-traced", scenario.id)).map_err(|error| error.to_string())
                    .and_then(|template| kill::count(scenario, &faulted_corpus, &programs, &settings, &template, powering, Tool::Forklift).map(|counting| (template, counting)).map_err(|error| error.to_string()));
                let (template, counting) = match counted {
                    Ok(counted) => counted,
                    Err(error) => {
                        if killing {
                            progress.block_finished(Category::KillPointsClient, &scenario.id, true);
                            all_kills.push(kill::ScenarioKills { scenario: scenario.id.clone(), errors: vec![error.clone()], ..Default::default() });
                        }
                        if powering {
                            progress.block_finished(Category::PowerLossClient, &scenario.id, true);
                            all_power.push(crate::power::ScenarioPower { scenario: scenario.id.clone(), errors: vec![error], ..Default::default() });
                        }
                        continue;
                    }
                };
                // Each started block is finished before a details file that failed refuses the run,
                // so none is left running.
                let mut written = Ok(());
                if killing {
                    let (kills, detail) = match kill::run(scenario, &programs, &settings, &template, &counting, Tool::Forklift) {
                        Ok(kills) => {
                            let detail = write_detail(output.root(), &kill::details_file(kill::Side::Client, &scenario.id), &kills);
                            (kills, detail)
                        }
                        Err(error) => (kill::ScenarioKills { scenario: scenario.id.clone(), errors: vec![error.to_string()], ..Default::default() }, Ok(())),
                    };
                    progress.block_finished(Category::KillPointsClient, &scenario.id, detail.is_err() || kills_failed(&kills));
                    written = written.and(detail);
                    all_kills.push(kills);
                }
                if powering {
                    let (lost, detail) = match crate::power::run(scenario, &programs, &settings, &template, &counting, power_variants, seed, Tool::Forklift) {
                        Ok(power) => {
                            let detail = write_detail(output.root(), &crate::power::details_file(kill::Side::Client, &scenario.id), &power);
                            (power, detail)
                        }
                        Err(error) => (crate::power::ScenarioPower { scenario: scenario.id.clone(), errors: vec![error.to_string()], ..Default::default() }, Ok(())),
                    };
                    progress.block_finished(Category::PowerLossClient, &scenario.id, detail.is_err() || power_failed(&lost));
                    written = written.and(detail);
                    all_power.push(lost);
                }
                written?;
            }
            // A lane no scenario was picked for is not run, which fails nothing: it was not asked
            // for, or what it named is not among the scenarios this run picked.
            let unasked = || Cell::not_run("no scenario this run picked was named for it");
            let kill_cell = if all_kills.is_empty() {
                unasked()
            } else {
                let summary = kill::summarize(&all_kills);
                rows.extend(summary.rows);
                kill_completeness = summary.completeness;
                not_reached = summary.not_reached;
                summary.cell
            };
            let server_kill_cell = if server.is_none() {
                Cell::not_run("no forklift-server binary given")
            } else if all_server_kills.is_empty() {
                Cell::not_run("no hosted scenario this run picked was named for it")
            } else {
                let summary = kill::summarize_as(&all_server_kills, kill::Side::Server);
                rows.extend(summary.rows);
                server_kill_completeness = summary.completeness;
                server_not_reached = summary.not_reached;
                summary.cell
            };
            let power_cell = if all_power.is_empty() {
                unasked()
            } else {
                let summary = crate::power::summarize(&all_power);
                rows.extend(summary.rows);
                power_completeness = summary.completeness;
                summary.cell
            };
            let server_power_cell = if server.is_none() {
                Cell::not_run("no forklift-server binary given")
            } else if all_server_power.is_empty() {
                Cell::not_run("no hosted scenario this run picked was named for it")
            } else {
                let summary = crate::power::summarize_as(&all_server_power, kill::Side::Server);
                rows.extend(summary.rows);
                server_power_completeness = summary.completeness;
                server_power_not_reached = all_server_power.iter().map(|scenario| scenario.not_reached.len() as u64).sum();
                summary.cell
            };
            Ok((kill_cell, power_cell, server_kill_cell, server_power_cell))
        }
    };
    let (kill_cell, power_cell, server_kill_cell, server_power_cell) = match traced_lanes {
        Ok(cells) => cells,
        Err((kill, power)) => (Cell::not_run(kill), Cell::not_run(power), Cell::not_run(kill), Cell::not_run(power)),
    };
    if kill_completeness.is_empty() {
        kill_completeness.push(Completeness { subject: Some(Tool::Forklift), check: "kill-point units".to_string(), cell: kill_cell.clone(), missing: vec![] });
    }
    if server_kill_completeness.is_empty() {
        server_kill_completeness.push(Completeness { subject: Some(Tool::Forklift), check: "server kill-point units".to_string(), cell: server_kill_cell.clone(), missing: vec![] });
    }
    if power_completeness.is_empty() {
        power_completeness.push(Completeness { subject: Some(Tool::Forklift), check: "power-loss states".to_string(), cell: power_cell.clone(), missing: vec![] });
    }
    if server_power_completeness.is_empty() {
        server_power_completeness.push(Completeness { subject: Some(Tool::Forklift), check: "server power-loss states".to_string(), cell: server_power_cell.clone(), missing: vec![] });
    }

    let mut coverage = CoverageTable::new(Platform::current(), |category: Category| match category {
        // The dead-holder locks are planted by the damage lane, binding by binding.
        Category::DamagedFiles | Category::DeadPidLock => damaged_cell.clone(),
        Category::KillPointsClient => kill_cell.clone(),
        Category::KillPointsServer => server_kill_cell.clone(),
        Category::PowerLossClient => power_cell.clone(),
        Category::PowerLossServer => server_power_cell.clone(),
        Category::DiskFull => disk_full_cell.clone(),
        Category::ReadOnly => read_only_cell.clone(),
        Category::CaseFolding => case_folding_cell.clone(),
        Category::NoExec => no_exec_cell.clone(),
        Category::Network => network_cell.clone(),
        Category::Fidelity => fidelity_cell.clone(),
        _ => Cell::not_run("no runner for this fault in this build of the harness"),
    });
    if matches!(kill_cell, Cell::Run | Cell::Sampled) || not_reached > 0 {
        coverage.not_reached.insert(Category::KillPointsClient, not_reached);
    }
    if matches!(server_kill_cell, Cell::Run | Cell::Sampled) || server_not_reached > 0 {
        coverage.not_reached.insert(Category::KillPointsServer, server_not_reached);
    }
    if matches!(server_power_cell, Cell::Run | Cell::Sampled) || server_power_not_reached > 0 {
        coverage.not_reached.insert(Category::PowerLossServer, server_power_not_reached);
    }
    let mut completeness = vec![damage_check, walk_check];
    completeness.extend(git_checks);
    completeness.extend([role_check, route_check, route_write_check]);
    completeness.extend(fault_checks);
    completeness.extend(condition_checks);
    completeness.push(Completeness { subject: None, check: "fidelity round trips ran".to_string(), cell: fidelity_cell, missing: fidelity_missing });
    completeness.extend(history_checks);
    completeness.extend(write_checks);
    completeness.extend(kill_completeness);
    completeness.extend(server_kill_completeness);
    completeness.extend(power_completeness);
    completeness.extend(server_power_completeness);
    completeness.push(Completeness::budget(started.elapsed(), budget.map(std::time::Duration::from_secs)));
    let report = Report {
        format: REPORT_FORMAT,
        coverage,
        determinism: Determinism {
            seed,
            tools,
            scenarios: scenarios.iter().map(|scenario| scenario.id.clone()).collect(),
            platform: Platform::current(),
            os: operating_system(),
            filesystem: filesystem_of(output.root()),
            configurations,
            history: history_record,
            environment: environment_seen,
        },
        completeness,
        rows,
        forklift_only: vec![],
        fidelity,
    };

    for ((scenario, warehouse), names) in &created {
        if !names.is_empty() {
            progress.message(&format!("{} creates{} {}", scenario, if *warehouse == Warehouse::Host { " on its host" } else { "" }, names.join(" ")));
        }
    }
    // report.json last: it is what says the run finished, so none is left by a run refused here.
    for (name, text) in [("report.md", report.markdown()), ("report.json", report.json())] {
        if let Err(error) = std::fs::write(output.root().join(name), text) {
            return Err(RunError::Refused(format!("cannot write {}: {}", name, error)));
        }
    }

    Ok(report)
}

/// Runs `run` in a new scratch area `label` of `output`, with a scrubbed environment; the area is
/// kept for its details.
fn in_area<T>(
    output: &mut Output, label: &str, search_path: &Option<std::ffi::OsString>,
    run: impl FnOnce(&crate::scrub::Scratch, &Environment) -> Result<T, String>,
) -> Result<T, String> {
    let scratch = output.scratch(label).map_err(|error| error.to_string())?;
    let environment = Environment::scrubbed(&scratch, search_path.clone(), &BTreeMap::new()).map_err(|error| error.to_string())?;
    run(&scratch, &environment)
}

/// Runs `run` in a new scratch area `label` of `volume`, then removes the area, failed or not: what
/// one leaves on a small capped volume would take the free space of the next, and the details of
/// each graded fault are written to the output.
fn discarded_area<T>(
    volume: &mut Output, label: &str, search_path: &Option<std::ffi::OsString>,
    run: impl FnOnce(&crate::scrub::Scratch, &Environment) -> Result<T, String>,
) -> Result<T, String> {
    let scratch = volume.scratch(label).map_err(|error| error.to_string())?;
    let root = scratch.root().to_path_buf();
    let result = Environment::scrubbed(&scratch, search_path.clone(), &BTreeMap::new()).map_err(|error| error.to_string())
        .and_then(|environment| run(&scratch, &environment));
    drop(scratch);
    #[allow(clippy::disallowed_methods, reason = "a scratch area the harness made itself on the capped volume; no retry is wanted")]
    let _ = std::fs::remove_dir_all(&root);
    result
}

/// A damage binding's role, or its refusal where the role map does not know it. Such a binding is
/// the harness's gap, never a row with no counterpart: that would drop it from every comparison
/// unsaid. The tables are static, so it is refused once, before any of its faults runs.
fn role_or_refusal(subject: Tool, name: &str) -> Result<crate::git_layout::Role, String> {
    crate::git_layout::role_of(subject, name).ok_or_else(|| format!("{}: the role map names no role for it", name))
}

/// A damage block's label, which names its scratch area and detail file: the subject, the
/// scenario, the side, the binding's name and the kind. A host's path is named apart from the
/// client's of the same name.
fn damage_label(subject: Tool, binding: &crate::damage::Binding, kind: crate::damage::Kind) -> String {
    let prefix = match subject { Tool::Forklift => String::new(), other => format!("{}-", other.label()) };
    let side = match binding.warehouse { Warehouse::Client => "", Warehouse::Host => "host-" };
    format!("{}{}-{}{}-{:?}", prefix, binding.scenario, side, binding.name, kind).replace(['/', '<', '>', '{', '}', '*'], "_")
}

/// The corpus a faulted command runs over, shared by every lane: the small shape with one record
/// past the history, the one the faulted command writes.
fn faulted_corpus(seed: u64) -> corpus::Corpus {
    corpus::generate(seed, corpus::Shape { records: corpus::Shape::SMALL.records + 1, ..corpus::Shape::SMALL })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cancels at its `polls`-th question, and counts the questions.
    struct CancelAt {
        polls: usize,
        asked: std::sync::atomic::AtomicUsize,
    }

    impl Progress for CancelAt {
        fn cancelled(&self) -> bool {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 >= self.polls
        }
    }

    /// A history build step forklift ran and refused is forklift's; one the harness could not run
    /// at all is the run's own. Either way every step run so far is written beside the failure,
    /// the refused one too, and no subject's rows are marked as run.
    ///
    /// Revert: charge every forklift error to forklift → the binary that cannot be spawned is
    /// forklift's; write the detail only once built → no damage-history.json; leave the rows' cell
    /// `Run` → a subject with no readback reads as run.
    #[test]
    #[cfg(unix)]
    fn a_history_build_is_forklifts_only_where_forklift_refused_it() {
        use std::os::unix::fs::PermissionsExt;
        let (shared, environment) = crate::history::tests::scratch("charged");
        let mirror = crate::history::tests::mirrored(&shared, &environment);
        let census = Census::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap().unwrap();
        let search_path = crate::scrub::search_path(Some(&crate::scrub::path_git()), std::env::var_os("PATH"));
        let refuses = shared.root().join("refuses");
        std::fs::write(&refuses, "#!/bin/sh\nexit 3\n").unwrap();
        std::fs::set_permissions(&refuses, std::fs::Permissions::from_mode(0o755)).unwrap();
        for (name, forklift, charged, last) in [("absent", shared.root().join("absent"), None, Some(0)), ("refuses", refuses, Some(Tool::Forklift), Some(3))] {
            let programs = Programs { forklift, git: crate::scrub::path_git(), server: None };
            let out = shared.root().join(format!("out-{}", name));
            let mut output = Output::claim(&out).unwrap();
            let (rows, checks) = history_damage(&mirror, &programs, &census, 3, std::time::Duration::from_secs(3600), None, 0, &mut output, &search_path, &Quiet).unwrap();
            assert!(rows.is_empty(), "{}: {:?}", name, rows);
            let built: Vec<&Completeness> = checks.iter().filter(|entry| entry.check == "history subjects built").collect();
            assert!(built.len() == 1 && built[0].subject == charged && built[0].missing[0].starts_with("forklift: "), "{}: {:?}", name, built);
            let detail: serde_json::Value = serde_json::from_slice(&std::fs::read(out.join("damage-history.json")).unwrap()).unwrap();
            let steps = detail["steps"].as_array().unwrap();
            assert!(steps.iter().any(|step| step["arguments"] == serde_json::json!(["switch", "-q", "-c", crate::faulted::PROBE_REF])), "{}: {:?}", name, steps);
            assert_eq!(steps.last().unwrap()["exit"].as_i64().map(|exit| exit as i32), last, "{}", name);
            for subject in HISTORY_SUBJECTS {
                let check = history_check("history damage rows ran", subject);
                let ran = checks.iter().find(|entry| entry.subject == Some(subject.0) && entry.check == check).unwrap();
                assert_eq!(ran.cell, Cell::not_run("the history subjects were not built"), "{}: {:?}", name, ran);
            }
        }
    }

    /// Forklift refusing its git export, or git refusing what it exported, the subjects given the
    /// history built, is forklift's and leaves only the from-Forklift direction unrun: forklift's
    /// rows and git's over the given history still run, while every check of git's over the
    /// export names why it did not run, none reading as run and clean.
    ///
    /// Revert: fail the whole build on the export → no subject's rows run; charge the export, or
    /// git's clone of it, to git → the entry stands under no subject; give the sibling checks
    /// `Cell::Run` → they read as run.
    #[test]
    #[cfg(unix)]
    #[ignore = "needs PROOF_LOAD_FORKLIFT"]
    fn a_refused_history_export_leaves_only_its_direction_unrun() {
        use std::os::unix::fs::PermissionsExt;
        let real = std::env::var("PROOF_LOAD_FORKLIFT").expect("PROOF_LOAD_FORKLIFT");
        let (shared, environment) = crate::history::tests::scratch("export-refused");
        let mirror = crate::history::tests::mirrored(&shared, &environment);
        let census = Census::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap().unwrap();
        let search_path = crate::scrub::search_path(Some(&crate::scrub::path_git()), std::env::var_os("PATH"));
        // The export's arguments are `--json git export -- <dir>`: one wrapper refuses it, the
        // other runs it, then takes the repository's objects away, so git's clone refuses it.
        let cases = [
            ("refused", "[ \"$2$3\" = gitexport ] && exit 3\nexec '{}' \"$@\"", "forklift: "),
            ("unreadable", "'{}' \"$@\" || exit\n[ \"$2$3\" = gitexport ] && rm -rf \"$5/.git/objects\"\nexit 0", "git, over forklift's export: "),
        ];
        for (name, script, blamed) in cases {
            let forklift = shared.root().join(name);
            std::fs::write(&forklift, format!("#!/bin/sh\n{}\n", script.replace("{}", &real))).unwrap();
            std::fs::set_permissions(&forklift, std::fs::Permissions::from_mode(0o755)).unwrap();
            let programs = Programs { forklift, git: crate::scrub::path_git(), server: None };
            let mut output = Output::claim(&shared.root().join(format!("out-{}", name))).unwrap();
            let (_, checks) = history_damage(&mirror, &programs, &census, 3, std::time::Duration::ZERO, None, 0, &mut output, &search_path, &Quiet).unwrap();
            let built: Vec<&Completeness> = checks.iter().filter(|entry| entry.check.starts_with("history subjects built")).collect();
            assert!(built.len() == 1 && built[0].check == "history subjects built, from Forklift" && built[0].subject == Some(Tool::Forklift)
                && built[0].missing[0].starts_with(blamed), "{}: {:?}", name, built);
            let unbuilt = Cell::not_run("git's subject over forklift's export was not built");
            for subject in HISTORY_SUBJECTS {
                let check = history_check("history damage rows ran", subject);
                let ran = checks.iter().find(|entry| entry.subject == Some(subject.0) && entry.check == check).unwrap();
                let expected = if subject.1 == Some(HistoryDirection::FromForklift) { unbuilt.clone() } else { Cell::Run };
                assert_eq!(ran.cell, expected, "{}: {:?}", name, subject);
            }
            let over_export: Vec<&Completeness> = checks.iter().filter(|entry| entry.check.ends_with(", from Forklift") && !entry.check.starts_with("history subjects built")).collect();
            assert!(over_export.len() == 6 && over_export.iter().all(|entry| entry.cell == unbuilt), "{}: {:?}", name, over_export);
        }
    }

    /// Forklift running past its limit is forklift's, as its refusal is; git's, and a step the
    /// harness could not run, are the run's own. Git refusing forklift's export is forklift's, its
    /// running long over it is not.
    ///
    /// Revert: drop the timeout from forklift's → a hung import is the harness's; charge every
    /// error over the export to forklift → git's slow clone is forklift's.
    #[test]
    fn a_history_build_past_its_limit_is_forklifts() {
        use crate::runner::RunError;
        use crate::tool::ToolError;
        let timed_out = || RunError::Tool(ToolError::TimedOut { program: PathBuf::from("/forklift"), limit: HISTORY_LIMIT });
        let refused = || RunError::BuildFailed { arguments: vec![], exit: Some(1), stderr: String::new(), stdout: String::new() };
        assert_eq!(build_charged_to(Tool::Forklift, &timed_out()), Some(Tool::Forklift));
        // Git's steps over forklift's export: its refusal is forklift's, its running long is not.
        assert_eq!(export_charged_to(&refused()), Some(Tool::Forklift));
        assert_eq!(export_charged_to(&timed_out()), None);
        assert_eq!(build_charged_to(Tool::Forklift, &refused()), Some(Tool::Forklift));
        assert_eq!(build_charged_to(Tool::Forklift, &RunError::Io("scratch".into())), None);
        assert_eq!(build_charged_to(Tool::Git, &timed_out()), None);
        assert_eq!(build_charged_to(Tool::Git, &refused()), None);
    }

    /// A history row starts only while the rows' own budget lasts and the run's deadline, the
    /// clock the run is graded on, has not passed, however long the build before the rows took;
    /// each cut names which ran out. A budget past what the clock counts is no deadline.
    ///
    /// Revert: ignore the deadline → a row starts past the run's budget.
    #[test]
    fn the_history_lane_starts_rows_only_within_the_runs_budget() {
        let minute = std::time::Duration::from_secs(60);
        let started = std::time::Instant::now();
        let (rows, now) = (started + minute * 10, started + minute * 20);
        assert_eq!(row_cut(now, rows, minute * 10, None), None);
        assert_eq!(row_cut(now, rows, minute * 9, None), Some("the lane's time budget was spent"));
        assert_eq!(row_cut(now, rows, minute * 120, Some(started + minute * 20)), None);
        assert_eq!(row_cut(now, rows, minute * 120, Some(started + minute * 19)), Some("the run's --budget was spent"));
        assert_eq!(run_deadline(started, Some(60)), Some(started + minute));
        assert_eq!(run_deadline(started, None), None);
        assert_eq!(run_deadline(started, Some(u64::MAX)), None);
    }

    /// Git tuned refusing its unfaulted readback leaves a gap in git tuned's measure, as forklift
    /// refusing its does in forklift's, and as git refusing its over forklift's export does in
    /// git's; git at its defaults over the given history, the oracle, refusing its is the run's.
    ///
    /// Revert: judge by the tool alone (`subject.0 != Tool::Git`) → git refusing its readback
    /// over forklift's export fails the run as the harness's.
    #[test]
    fn only_the_oracle_refusing_its_readback_is_the_runs_own() {
        use HistoryDirection::*;
        assert!(refused_readback_is_a_gap((Tool::Forklift, None)));
        assert!(refused_readback_is_a_gap((Tool::GitTuned, Some(FromGit))));
        assert!(refused_readback_is_a_gap((Tool::GitTuned, Some(FromForklift))));
        assert!(refused_readback_is_a_gap((Tool::Git, Some(FromForklift))));
        assert!(!refused_readback_is_a_gap((Tool::Git, Some(FromGit))));
    }

    /// An empty sample is forklift's when git holds a ref with commits and the import made a
    /// pallet of none of them, though it made others; the run's own when git holds none.
    ///
    /// Revert: blame forklift only when it made no pallet at all → the renamed import is the run's.
    #[test]
    fn an_import_missing_every_git_ref_is_forklifts() {
        let lengths: BTreeMap<String, usize> = [("main".to_string(), 3), ("empty".to_string(), 0)].into_iter().collect();
        let renamed = ["trunk".to_string(), "empty".to_string()].into_iter().collect();
        assert_eq!(empty_sample(&lengths, &renamed).0, Some(Tool::Forklift));
        assert_eq!(empty_sample(&lengths, &Default::default()).0, Some(Tool::Forklift));
        let none: BTreeMap<String, usize> = [("empty".to_string(), 0)].into_iter().collect();
        assert_eq!(empty_sample(&none, &renamed).0, None);
    }

    /// A damage run over a real history becomes a row paired under the history's own scenario,
    /// which no fixture scenario shares, so its line never folds with a fixture row's; one whose
    /// whole-store check found damage the sample did not reach becomes a completeness entry naming
    /// the check, never a row.
    ///
    /// Revert: ignore `unreached` → the healed row stands; name the history `history` → it is a
    /// fixture scenario.
    #[test]
    fn damage_the_sample_did_not_reach_is_named_never_graded() {
        use crate::damage::Kind;
        use crate::oracle::Grade;
        assert!(!scenario::known(faulted::HISTORY_SCENARIO));
        let check = tool::Outcome { arguments: vec!["fsck".into(), "--full".into()], exit: Some(4), stdout: vec![], stderr: vec![], failure: None };
        let run = faulted::Faulted {
            scenario: faulted::HISTORY_SCENARIO.to_string(), name: "objects/pack/<name>.pack".into(), warehouse: Warehouse::Client, kind: Kind::Zero,
            target: "objects/pack/pack-1.pack".into(), grade: Grade::Healed, evidence: Default::default(), recoverability: Default::default(),
            steps: vec![], readbacks: vec![], probes: vec![], unextractable: vec![], unreached: Some(check),
        };
        let scenario = "real-history, over a sample of 8 positions";
        let subject = (Tool::Git, Some(HistoryDirection::FromForklift));
        let entry = history_row(subject, "git-real-history-pack-Zero", &run, scenario, crate::git_layout::Role::Pack).unwrap_err();
        assert!(entry.contains("\"fsck\"") && entry.contains("Some(4)") && entry.contains("git-real-history-pack-Zero.json"), "{}", entry);
        let row = history_row(subject, "git-real-history-pack-Zero", &faulted::Faulted { unreached: None, ..run.clone() }, scenario, crate::git_layout::Role::Pack).unwrap();
        assert_eq!(row.grade, Grade::Healed);
        assert_eq!(row.scenario, format!("{}, from Forklift objects/pack/pack-1.pack Zero", scenario));
        let pairing = row.pairing.expect("paired");
        assert_eq!((pairing.scenario.as_str(), pairing.directions.as_slice()), (scenario, [HistoryDirection::FromForklift].as_slice()));
        // Forklift's one subject is both directions': its row stands on both lines.
        let forklift = history_row((Tool::Forklift, None), "real-history-pack-Zero", &faulted::Faulted { unreached: None, ..run }, scenario, crate::git_layout::Role::Pack).unwrap();
        assert_eq!(forklift.pairing.expect("paired").directions, [HistoryDirection::FromGit, HistoryDirection::FromForklift]);
    }

    /// The history damage lane over a small mirrored history: every subject built, git's over the
    /// given history and over forklift's export of it too, their binding sets clean against their
    /// walks and their unfaulted readbacks taken. With no budget, every
    /// planned row of every subject is named in its completeness as not run, as with the run's
    /// deadline already passed, each naming which ran out; with budget enough,
    /// every planned row ran to a row or an unreached entry, paired under the history's scenario,
    /// with no harness error, and no forklift row refused every stack for good judges its faulted
    /// record silent.
    ///
    /// Revert: drop the budget check → rows run with no budget; pass no deadline on → rows run
    /// past the run's; settle a record on any answer → the zeroed config's faulted record is
    /// judged silent; push the unsettled readback last → the same.
    #[test]
    #[ignore = "needs PROOF_LOAD_FORKLIFT"]
    fn the_history_lane_names_what_its_budget_did_not_reach() {
        let forklift = PathBuf::from(std::env::var("PROOF_LOAD_FORKLIFT").expect("PROOF_LOAD_FORKLIFT"));
        let (shared, environment) = crate::history::tests::scratch("lane");
        let mirror = crate::history::tests::mirrored(&shared, &environment);
        let programs = Programs { forklift, git: crate::scrub::path_git(), server: None };
        let census = Census::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap().unwrap();
        let search_path = crate::scrub::search_path(Some(&crate::scrub::path_git()), std::env::var_os("PATH"));
        let seed = 3;
        let planned = |tool| crate::bindings::history_rows(tool, seed).len();
        let named = |checks: &[Completeness], subject: Built, check: &str| {
            let check = history_check(check, subject);
            checks.iter().find(|entry| entry.subject == Some(subject.0) && entry.check == check)
                .unwrap_or_else(|| panic!("{:?} {}: {:?}", subject, check, checks)).missing.clone()
        };
        // The subject a row was graded over: a git row's one direction names it.
        let graded_over = |row: &Row| (row.subject, (row.subject != Tool::Forklift).then(|| row.pairing.as_ref().expect("paired").directions[0]));
        let hour = std::time::Duration::from_secs(3600);
        let cases = [(std::time::Duration::ZERO, None, "the lane's time budget was spent"), (hour, Some(std::time::Instant::now()), "the run's --budget was spent"), (hour, None, "")];
        for (index, (budget, deadline, reason)) in cases.into_iter().enumerate() {
            let mut output = Output::claim(&shared.root().join(format!("out-{}", index))).unwrap();
            let (rows, checks) = history_damage(&mirror, &programs, &census, seed, budget, deadline, 0, &mut output, &search_path, &Quiet).unwrap();
            assert!(!checks.iter().any(|entry| entry.check.starts_with("history subjects built")), "{:?}", checks);
            for subject in HISTORY_SUBJECTS {
                let tool = subject.0;
                assert_eq!(named(&checks, subject, "history damage bindings"), Vec::<String>::new(), "{:?}", subject);
                let cut = named(&checks, subject, "history damage rows ran");
                let unreached = named(&checks, subject, "damage not reached by the sample");
                let graded = rows.iter().filter(|row| graded_over(row) == subject).count();
                if !reason.is_empty() {
                    assert_eq!(cut.len(), planned(tool), "{:?}: {:?}", subject, cut);
                    assert!(cut.iter().all(|entry| entry.ends_with(&format!(": not run: {}", reason))), "{:?}: {:?}", subject, cut);
                    assert_eq!(graded + unreached.len(), 0, "{:?}", subject);
                } else {
                    assert_eq!(cut, Vec::<String>::new(), "{:?}", subject);
                    assert_eq!(graded + unreached.len(), planned(tool), "{:?}: {:?}", subject, checks);
                }
            }
            let faults: Vec<&Completeness> = checks.iter().filter(|entry| entry.check.starts_with("history damage faults ran")).collect();
            assert!(faults.len() == HISTORY_SUBJECTS.len() && faults.iter().all(|entry| entry.missing.is_empty()), "{:?}", faults);
            assert!(rows.iter().all(|row| row.pairing.as_ref().is_some_and(|pairing| pairing.scenario.starts_with(faulted::HISTORY_SCENARIO))), "{:?}", rows);
            if reason.is_empty() {
                refused_for_good_is_never_silent(output.root());
            }
        }
    }

    /// Over the forklift rows a lane wrote to `root`, one whose every stack was refused, none as
    /// nothing to stack, made no record, so the readback its last settle judged by reads no
    /// silent copy of the faulted record; and some such row judged the record at all. A sampled
    /// position may still be silent there, which is forklift's own.
    fn refused_for_good_is_never_silent(root: &std::path::Path) {
        let mut refused_for_good = Vec::new();
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !name.starts_with(&format!("{}-", faulted::HISTORY_SCENARIO)) || !name.ends_with(".json") {
                continue;
            }
            let details: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let stacks: Vec<&serde_json::Value> = details["steps"].as_array().map(Vec::as_slice).unwrap_or_default().iter()
                .filter(|step| step["arguments"].as_array().is_some_and(|arguments| arguments.iter().any(|argument| argument == "stack"))).collect();
            let for_good = !stacks.is_empty() && stacks.iter().all(|step| step["exit"] != 0
                && !step["failure"]["message"].as_str().is_some_and(|message| message.starts_with("Nothing to stack")));
            if for_good {
                let judged = details["readbacks"].as_array().and_then(|readbacks| readbacks.last());
                let faulted = judged.and_then(|readback| readback["records"].as_array())
                    .and_then(|records| records.iter().find(|record| record["mark"] == serde_json::json!({"mark": "record", "index": 0})));
                let verdicts: &[serde_json::Value] = faulted.and_then(|record| record["verdicts"].as_array()).map(Vec::as_slice).unwrap_or_default();
                assert!(!verdicts.iter().any(|verdict| verdict == "silent"), "{}: the faulted record judged {:?}", name, verdicts);
                if !verdicts.is_empty() {
                    refused_for_good.push(name);
                }
            }
        }
        assert!(!refused_for_good.is_empty(), "of the rows in {}, none of forklift's was refused every stack and judged its record", root.display());
    }

    /// A binding the role map names no role for is refused, under either subject; one it knows
    /// takes its role.
    ///
    /// Revert: return any role for an unknown name → the unknown binding passes unsaid.
    #[test]
    fn a_binding_with_no_role_is_refused() {
        use crate::git_layout::Role;
        assert_eq!(role_or_refusal(Tool::Forklift, "no-such-path"), Err("no-such-path: the role map names no role for it".to_string()));
        assert!(role_or_refusal(Tool::GitTuned, "no-such-path").is_err());
        assert_eq!(role_or_refusal(Tool::Forklift, "trust"), Ok(Role::Signature));
        assert_eq!(role_or_refusal(Tool::Git, "index.lock"), Ok(Role::Lock));
    }

    /// Revert: drop the side → a client and a host binding of one name share a label, so one
    /// row's scratch and detail file is the other's.
    #[test]
    fn a_host_damage_block_is_labelled_apart_from_the_client() {
        use crate::damage::{Binding, Kind};
        let client = Binding { name: "HEAD", scenario: "lifted", warehouse: Warehouse::Client, kinds: &[] };
        let host = Binding { warehouse: Warehouse::Host, ..client.clone() };
        assert_eq!(damage_label(Tool::Git, &client, Kind::Delete), "git-lifted-HEAD-Delete");
        assert_eq!(damage_label(Tool::Git, &host, Kind::Delete), "git-lifted-host-HEAD-Delete");
        assert_eq!(damage_label(Tool::Forklift, &host, Kind::Zero), "lifted-host-HEAD-Zero");
    }

    fn fresh(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("proof-load-run-{}-{}-{}", name, std::process::id(), nanos))
    }

    fn bare(out: PathBuf) -> RunOptions {
        let source = fresh("source");
        std::fs::create_dir_all(&source).unwrap();
        RunOptions { source, out, seed: 1, ..RunOptions::default() }
    }

    /// A run given no tools still completes in-process: it returns its report and writes the
    /// same report, which reads back.
    #[test]
    fn a_run_returns_the_report_it_writes() {
        let out = fresh("out");
        let report = run(&bare(out.clone()), &Quiet).expect("a run with no tools completes");
        let written = Report::from_json(&std::fs::read_to_string(out.join("report.json")).unwrap()).expect("the written report reads");
        assert_eq!(written.json(), report.json());
        assert_eq!(report.format, REPORT_FORMAT);
    }

    /// An `--only` that names no scenario is refused, not run as an empty run; one that names a
    /// coverage scenario is not.
    ///
    /// Revert: drop the check → the unknown id runs nothing and reports success.
    #[test]
    fn an_unknown_only_is_refused() {
        let out = fresh("out");
        let options = RunOptions { only: Some("signd".to_string()), ..bare(out.clone()) };
        assert_eq!(run(&options, &Quiet).unwrap_err(), RunError::Refused("--only signd: no such scenario".to_string()));
        assert!(!out.exists(), "a mistyped id is refused before the report folder is made");
        let coverage = scenario::coverage().into_iter().next().expect("there are coverage scenarios").id;
        assert!(scenario::known(&coverage) && scenario::known("signed"));
    }

    /// A `--history-from` that names no folder is refused before the report folder is made, as is
    /// one given with `--only`, which runs no round trip.
    ///
    /// Revert: drop either check → the run goes on and makes the report folder.
    #[test]
    fn a_history_from_that_is_no_folder_is_refused() {
        let out = fresh("out");
        let missing = fresh("missing-history");
        let options = RunOptions { history_from: Some(missing.clone()), ..bare(out.clone()) };
        assert_eq!(run(&options, &Quiet).unwrap_err(), RunError::Refused(format!("--history-from {}: not a folder", missing.display())));
        assert!(!out.exists());
        let folder = fresh("history");
        std::fs::create_dir_all(&folder).unwrap();
        let options = RunOptions { history_from: Some(folder), only: Some("signed".to_string()), ..bare(out.clone()) };
        assert_eq!(run(&options, &Quiet).unwrap_err(), RunError::Refused("--history-from feeds the fidelity round trips, which --only leaves out".to_string()));
        assert!(!out.exists());
    }

    /// A run asked to stop stops with `Cancelled` and writes no report.
    ///
    /// Revert: drop the poll before the volume faults → the run completes.
    #[test]
    fn a_cancelled_run_stops_and_writes_no_report() {
        let out = fresh("out");
        let progress = CancelAt { polls: 1, asked: Default::default() };
        assert!(matches!(run(&bare(out.clone()), &progress), Err(RunError::Cancelled)));
        assert!(!out.join("report.json").exists());
    }
}
