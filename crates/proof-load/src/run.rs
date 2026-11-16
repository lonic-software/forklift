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
use crate::report::{Category, Cell, Completeness, CoverageTable, Determinism, Pairing, Platform, Report, Row, REPORT_FORMAT};
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

/// Runs every lane `options` asks for into `options.out`, which must be empty or new, and returns
/// the report it wrote there as `report.json` and `report.md`. A run that is refused or cancelled
/// leaves whatever it wrote before it stopped, but no `report.json`; a run again needs another
/// empty directory.
pub fn run(options: &RunOptions, progress: &dyn Progress) -> Result<Report, RunError> {
    let RunOptions {
        source, out, seed, only, forklift, git, forklift_server, budget, strace, kill, power, power_variants, jobs,
        capped_volume, disk_full, read_only: read_only_lane, folding_volume, network: network_lane,
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
            progress.lane(Category::DamagedFiles, Some(roled().map(|(_, binding, _, _)| binding.kinds.len()).sum()));
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
                                pairing: Some(Pairing { scenario: run.scenario.clone(), side: run.warehouse, role, kind: run.kind }),
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

    // The fixture history round-tripped, git to forklift to git: what it changed or dropped is
    // findings, never grades. A round trip that cannot finish is a gap in the run.
    let mut fidelity = Vec::new();
    let mut fidelity_missing = Vec::new();
    let fidelity_cell = match (&program, &git_program) {
        (None, _) => Cell::not_run("no forklift binary given"),
        (_, None) => Cell::not_run("no git binary given"),
        _ if only.is_some() => Cell::not_run("--only picks one scenario, and the fidelity fixture is none"),
        (Some(program), Some(git_program)) => {
            let block = "fidelity-git-forklift-git";
            progress.lane(Category::Fidelity, Some(1));
            start_block(progress, Category::Fidelity, block)?;
            let trip = output.scratch(block).map_err(|error| error.to_string()).and_then(|scratch| {
                let environment = Environment::scrubbed(&scratch, search_path.clone(), &BTreeMap::new()).map_err(|error| error.to_string())?;
                Ok(crate::fidelity::git_round_trip(program, git_program, &scratch, &environment, limit))
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
                    fidelity.extend(findings);
                    Cell::Run
                }
                Err(error) => {
                    fidelity_missing.push(format!("git to Forklift to git: {}", error));
                    Cell::not_run("no round trip finished")
                }
            }
        }
    };

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
            progress.lane(Category::KillPointsClient, Some(scenarios.iter().filter(killed).count()));
            progress.lane(Category::PowerLossClient, Some(scenarios.iter().filter(powered).count()));
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
                // One poll for both client blocks, so a cancel never leaves one started alone.
                stop_if_cancelled(progress)?;
                if killing {
                    progress.block_started(Category::KillPointsClient, &scenario.id);
                }
                if powering {
                    progress.block_started(Category::PowerLossClient, &scenario.id);
                }
                let counted = output.scratch(&format!("{}-traced", scenario.id)).map_err(|error| error.to_string())
                    .and_then(|template| kill::count(scenario, &faulted_corpus, &programs, &settings, &template, powering).map(|counting| (template, counting)).map_err(|error| error.to_string()));
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
                if killing {
                    let kills = match kill::run(scenario, &programs, &settings, &template, &counting) {
                        Ok(kills) => {
                            write_detail(output.root(), &kill::details_file(kill::Side::Client, &scenario.id), &kills)?;
                            kills
                        }
                        Err(error) => kill::ScenarioKills { scenario: scenario.id.clone(), errors: vec![error.to_string()], ..Default::default() },
                    };
                    progress.block_finished(Category::KillPointsClient, &scenario.id, kills_failed(&kills));
                    all_kills.push(kills);
                }
                if powering {
                    let lost = match crate::power::run(scenario, &programs, &settings, &template, &counting, power_variants, seed) {
                        Ok(power) => {
                            write_detail(output.root(), &crate::power::details_file(kill::Side::Client, &scenario.id), &power)?;
                            power
                        }
                        Err(error) => crate::power::ScenarioPower { scenario: scenario.id.clone(), errors: vec![error.to_string()], ..Default::default() },
                    };
                    progress.block_finished(Category::PowerLossClient, &scenario.id, power_failed(&lost));
                    all_power.push(lost);
                }
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
