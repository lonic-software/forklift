//! Building a scenario's history with a real tool, and walking what it left behind.
//!
//! The build runs in a fresh scratch area under the scrubbed environment: the corpus writes each
//! record into the work tree, and the scenario's operations run through their binding. The walk
//! that follows lists every path in the warehouse's state folder; a path no census row matches
//! fails the run, and the census names some path matched are the ones this scenario creates.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::census::{self, Census};
use crate::corpus::{self, Corpus};
use crate::oracle::{Admissible, RedactionLedger, RedactionState};
use crate::scenario::{bind, Keep, Mark, Op, Scenario, Tool, Until, SCRATCH, TARGET, URL, WORK};
use crate::scrub::{Environment, Scratch};
use crate::host::Host;
use crate::proxy::Exchange;
use crate::tool::{self, Invocation, Outcome};
use crate::writeops::Observer;

/// What building a scenario's history produced.
#[derive(serde::Serialize)]
pub struct Built {
    /// Every command the build ran, in order, with what it said.
    pub outcomes: Vec<Outcome>,
    /// The paths under the state folder after the build, as `census::walk` lists them.
    pub walked: Vec<String>,
    /// The same of the host's state folder, when the history hosts a warehouse: for git, the bare
    /// repository itself, which is all state folder.
    pub host_walked: Vec<String>,
    /// Content the history took away from records it had acknowledged.
    pub taken: Vec<Taken>,
    /// Marks acknowledged: each record or variant whose binding exited 0 throughout.
    pub acknowledged: Vec<Mark>,
    /// Marks the history's remote surely holds: every mark acknowledged at its last push, claimed
    /// only while every op so far kept one pallet current and left the remote's content alone. Any
    /// other op (a branch, a switch, a merge, a deleted remote pallet, a moved origin, a withheld
    /// or redacted record…) drops the claim for the rest of the history, so this is never more
    /// than was pushed, though it may be less.
    pub pushed: Vec<Mark>,
    /// The parcels `park list` names after a Forklift history that parks, which every readback
    /// must find listed again.
    pub parked: Vec<String>,
    /// The server a `Host` operation started, serving until this is dropped.
    #[serde(skip)]
    pub host: Option<Host>,
    /// What the proxies of hosts the history stopped logged before they went.
    #[serde(skip)]
    pub stopped: Vec<Exchange>,
    /// The bare repository a git history's `Host` made, by the path that is its URL.
    #[serde(skip)]
    pub bare: Option<String>,
    /// The fsmonitor daemon of a tuned git work tree, watching until this is dropped.
    #[serde(skip)]
    pub daemon: Option<FsmonitorDaemon>,
}

impl Built {
    /// The URL the history's remote answers at: its forklift-server's, or its bare repository's
    /// path. None when it hosts nothing, or its server has stopped.
    pub fn url(&self) -> Option<&str> {
        self.host.as_ref().map(|host| host.url.as_str()).or(self.bare.as_deref())
    }

    /// Every exchange the history's hosts logged: those it stopped, then the one still serving.
    pub fn exchanges(&self) -> Vec<Exchange> {
        let mut exchanges = self.stopped.clone();
        exchanges.extend(self.host.iter().flat_map(|host| host.proxy.log()));
        exchanges
    }
}

impl std::fmt::Debug for Built {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Built").field("outcomes", &self.outcomes).field("walked", &self.walked)
            .field("acknowledged", &self.acknowledged).field("host_walked", &self.host_walked).field("taken", &self.taken).field("host", &self.host.as_ref().map(|host| &host.url)).field("bare", &self.bare).finish()
    }
}

/// Content a withhold or a redaction took away: the path the harness picked in its record, the
/// digest of its bytes (which any path of any record holding those bytes shares), and how.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Taken {
    pub path: String,
    pub digest: String,
    pub by: TakenBy,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "by", rename_all = "kebab-case")]
pub enum TakenBy {
    /// The host withheld it; what a read expects depends on who serves it.
    Withheld,
    /// The client redacted it; what a read of it may produce in that warehouse, from the state the
    /// warehouse reported.
    Redacted { hash: String, admissible: Vec<Admissible> },
}

#[derive(Debug)]
pub enum RunError {
    Tool(tool::ToolError),
    Io(String),
    /// A build step ran and did not exit 0, or exited 0 having built other than it was asked to:
    /// the history a scenario names, or the subject built from a real history, does not exist.
    /// The tool's own stderr, or what the harness found it built.
    BuildFailed { arguments: Vec<String>, exit: Option<i32>, stderr: String, stdout: String },
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Tool(error) => write!(f, "{}", error),
            RunError::Io(reason) => write!(f, "{}", reason),
            RunError::BuildFailed { arguments, exit, stderr, stdout } =>
                write!(f, "building the history failed at `{}` (exit {:?}): {}{}", arguments.join(" "), exit, stdout.trim(), stderr.trim()),
        }
    }
}

impl std::error::Error for RunError {}

/// What a history's remote surely holds, op by op ([`Built::pushed`]).
#[derive(Debug, Default)]
struct Pushed {
    marks: Vec<Mark>,
    /// Some op so far switched pallets or changed the remote's content: nothing is claimed again.
    diverged: bool,
}

impl Pushed {
    /// Takes in `op`, run with `acknowledged` the marks acknowledged through it.
    fn after(&mut self, op: &Op, acknowledged: &[Mark]) {
        match op {
            Op::Push if !self.diverged => self.marks = acknowledged.to_vec(),
            // A clone replaces the work tree with the remote's, so it keeps the claim only while
            // nothing was acknowledged since the last push: a later push would otherwise claim
            // records the clone threw away.
            Op::Clone { .. } if acknowledged.len() == self.marks.len() && !self.diverged => {}
            Op::Record { .. } | Op::Variant { .. } | Op::Push | Op::Fetch | Op::Park { .. }
                | Op::Enroll | Op::Pack | Op::Verify | Op::Repair => {}
            // The first host, before anything was pushed to one, is the remote itself.
            Op::Host if self.marks.is_empty() && !self.diverged => {}
            _ => {
                self.diverged = true;
                self.marks.clear();
            }
        }
    }
}

/// The state folder a tool keeps beside its work tree.
pub(crate) fn state_folder(tool: Tool) -> &'static str {
    match tool {
        Tool::Forklift => ".forklift",
        Tool::Git | Tool::GitTuned => ".git",
    }
}

/// The command lines that make an empty work tree a repository of `tool`.
fn prepare(tool: Tool) -> Vec<String> {
    match tool {
        Tool::Forklift => vec!["--json".into(), "prepare".into()],
        Tool::Git | Tool::GitTuned => vec!["init".into(), "-q".into(), "-b".into(), "main".into()],
    }
}

/// The configuration every repository git makes is given at creation, before any history: no
/// detached background gc or maintenance, so nothing runs on in the background after a command
/// exits and a grade never depends on what a leftover process did.
pub(crate) const GIT_CREATED_CONFIG: &[[&str; 2]] = &[["gc.autoDetach", "false"], ["maintenance.autoDetach", "false"]];

/// Git's fsmonitor daemon over one tuned work tree, run by the harness in the foreground rather
/// than started by the first command that queries it, so it is up before the build's first step,
/// outside the build's commands. A command that finds it down later (a fault stopped it) starts a
/// detached one of its own, from inside that command. Dropping this asks git to stop whatever
/// daemon watches the work tree (that detached one; or, where the git given is a launcher such as
/// Git for Windows' `cmd\git.exe`, the daemon the launcher runs as its own child), then kills
/// the child. A daemon git started on its own in
/// a repository a fault left unreadable, which no `stop` can name, outlives the run, and so does
/// every daemon when the harness itself is killed.
pub struct FsmonitorDaemon {
    child: std::process::Child,
    program: PathBuf,
    work: PathBuf,
    environment: Environment,
}

/// Asks any fsmonitor daemon watching `work` to stop, giving it at most [`DAEMON_PROBE_LIMIT`]:
/// one git started on its own as well as the harness's.
pub(crate) fn stop_daemon(program: &Path, work: &Path, environment: &Environment) {
    let mut command = std::process::Command::new(program);
    command.args(["fsmonitor--daemon", "stop"]).current_dir(work)
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    environment.apply(&mut command);
    if let Ok(mut stop) = command.spawn() {
        let started = std::time::Instant::now();
        while stop.try_wait().is_ok_and(|status| status.is_none()) && started.elapsed() < DAEMON_PROBE_LIMIT {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = stop.kill();
        let _ = stop.wait();
    }
}

/// Why a daemon never watched.
#[derive(Debug)]
pub(crate) enum Unwatched {
    /// It exited first, as git does over a work tree it cannot take for a repository.
    Exited(String),
    /// What failed was the harness's: a spawn, a probe, the wait.
    Harness(RunError),
}

/// Stops, when dropped, any fsmonitor daemon watching its work tree: one git started on its own.
pub(crate) struct StopsDaemon {
    pub(crate) program: PathBuf,
    pub(crate) work: PathBuf,
    pub(crate) environment: Environment,
}

impl Drop for StopsDaemon {
    fn drop(&mut self) {
        stop_daemon(&self.program, &self.work, &self.environment);
    }
}

impl Drop for FsmonitorDaemon {
    fn drop(&mut self) {
        stop_daemon(&self.program, &self.work, &self.environment);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// How long one `status` or `stop` of the fsmonitor daemon may take.
const DAEMON_PROBE_LIMIT: Duration = Duration::from_secs(10);

impl FsmonitorDaemon {
    /// Turns on the fsmonitor in the work tree at `work` and starts its daemon, then waits until
    /// `status` says it watches.
    pub(crate) fn start(program: &Path, work: &Path, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<(FsmonitorDaemon, Vec<Outcome>), RunError> {
        let mut outcomes = Vec::new();
        let configured = tool::run(&Invocation { program: program.to_path_buf(), arguments: vec!["config".into(), "core.fsmonitor".into(), "true".into()], directory: work.to_path_buf(), limit }, scratch, environment)
            .map_err(RunError::Tool)?;
        if !configured.succeeded() {
            return Err(RunError::Io(format!("turning on the fsmonitor exited {:?}: {}", configured.exit, String::from_utf8_lossy(&configured.stderr))));
        }
        outcomes.push(configured);
        let (daemon, status) = FsmonitorDaemon::watch(program, work, scratch, environment, limit)?;
        outcomes.push(status);
        Ok((daemon, outcomes))
    }

    /// Starts the daemon over the work tree at `work`, its fsmonitor already on, and waits until
    /// `status` says it watches: what that `status` said. Nothing in the work tree is configured,
    /// so a copy whose state is under test keeps its config as it was.
    pub(crate) fn watch(program: &Path, work: &Path, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<(FsmonitorDaemon, Outcome), RunError> {
        FsmonitorDaemon::watch_or_exit(program, work, scratch, environment, limit).map_err(|unwatched| match unwatched {
            Unwatched::Exited(why) => RunError::Io(why),
            Unwatched::Harness(error) => error,
        })
    }

    /// [`FsmonitorDaemon::watch`], telling a daemon that exited before it watched, which is git's
    /// answer to the work tree it was given, from what failed the harness.
    pub(crate) fn watch_or_exit(program: &Path, work: &Path, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<(FsmonitorDaemon, Outcome), Unwatched> {
        let harness = Unwatched::Harness;
        let mut command = std::process::Command::new(program);
        command.args(["fsmonitor--daemon", "run"]).current_dir(work)
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        environment.apply(&mut command);
        let child = command.spawn().map_err(|error| harness(RunError::Io(format!("starting the fsmonitor daemon: {}", error))))?;
        let mut daemon = FsmonitorDaemon { child, program: program.to_path_buf(), work: work.to_path_buf(), environment: environment.clone() };
        let started = std::time::Instant::now();
        loop {
            // Only an exit that failed with a code is git's answer to the work tree, its refusal; a
            // clean exit before watching, or a death by a signal, is the harness's.
            if let Some(status) = daemon.child.try_wait().map_err(|error| harness(RunError::Io(error.to_string())))? {
                return Err(match status.code() {
                    Some(code) if code != 0 => Unwatched::Exited(format!("the fsmonitor daemon exited with code {} before it watched", code)),
                    _ => harness(RunError::Io(format!("the fsmonitor daemon ended {} before it watched {}", status, work.display()))),
                });
            }
            // A probe that runs past its own bound has not seen the daemon watch yet; only `limit`
            // as a whole gives up.
            match tool::run(&Invocation { program: program.to_path_buf(), arguments: vec!["fsmonitor--daemon".into(), "status".into()], directory: work.to_path_buf(), limit: DAEMON_PROBE_LIMIT.min(limit) }, scratch, environment) {
                Ok(status) if status.succeeded() => return Ok((daemon, status)),
                Ok(_) | Err(tool::ToolError::TimedOut { .. }) => {}
                Err(error) => return Err(harness(RunError::Tool(error))),
            }
            if started.elapsed() > limit {
                return Err(harness(RunError::Io(format!("the fsmonitor daemon did not watch {} within {:?}", work.display(), limit))));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Whether the git at `program` was built with the fsmonitor daemon, as its build options say.
/// Asked once per binary; a git that cannot say reads as one without.
pub fn has_fsmonitor_daemon(program: &Path) -> bool {
    static ASKED: std::sync::Mutex<BTreeMap<PathBuf, bool>> = std::sync::Mutex::new(BTreeMap::new());
    let mut asked = ASKED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    *asked.entry(program.to_path_buf()).or_insert_with(|| {
        std::process::Command::new(program).args(["version", "--build-options"]).env_clear()
            .envs(std::env::var_os("PATH").map(|path| ("PATH", path)))
            .envs(crate::scrub::system_variables_when(cfg!(windows), |name| std::env::var_os(name)))
            .current_dir(std::env::temp_dir()).output()
            .is_ok_and(|output| builds_fsmonitor_daemon(&output.stdout))
    })
}

/// Whether `git version --build-options` printed the fsmonitor daemon's feature line.
fn builds_fsmonitor_daemon(stdout: &[u8]) -> bool {
    String::from_utf8_lossy(stdout).lines().any(|line| line.trim() == "feature: fsmonitor--daemon")
}

/// The listing of git's parked work: each stash commit, newest first.
pub(crate) const STASH_LIST: &[&str] = &["stash", "list", "--format=%H"];

/// The stash commits [`STASH_LIST`] printed, newest first; nothing when it printed something
/// other than one commit id per line.
pub fn stashed_commits(stdout: &[u8]) -> Option<Vec<String>> {
    let text = std::str::from_utf8(stdout).ok()?;
    text.lines().map(|line| (line.len() == 40 && line.bytes().all(|byte| byte.is_ascii_hexdigit())).then(|| line.to_string())).collect()
}

/// The host warehouse a `Host` operation serves, beside the work tree in the scratch area.
pub fn host_root(scratch: &Scratch) -> PathBuf {
    scratch.root().join("host")
}

/// Builds `scenario`'s history with `program` (the binary of `tool`) in `scratch`'s work tree,
/// with `server` the forklift-server a `Host` operation starts. Every build step must succeed: a
/// failed one means the scenario's precondition does not hold, which is reported, never graded.
/// With an `observer`, every step goes through it, for the write-op check.
#[allow(clippy::too_many_arguments)]
pub fn build(scenario: &Scenario, corpus: &Corpus, tool: Tool, program: &Path, server: Option<&Path>, scratch: &Scratch, environment: &Environment, limit: Duration, observer: Option<&Observer>) -> Result<Built, RunError> {
    build_logged(scenario, corpus, tool, program, server, scratch, environment, limit, observer, &mut Vec::new())
}

/// [`build`], and when it fails, every exchange its hosts logged up to the failure is added to
/// `lost`: what was answered before a later step failed was still answered.
#[allow(clippy::too_many_arguments)]
pub fn build_logged(scenario: &Scenario, corpus: &Corpus, tool: Tool, program: &Path, server: Option<&Path>, scratch: &Scratch, environment: &Environment, limit: Duration, observer: Option<&Observer>, lost: &mut Vec<Exchange>) -> Result<Built, RunError> {
    let mut host: Option<Host> = None;
    let mut stopped = Vec::new();
    let built = build_in(scenario, corpus, tool, program, server, scratch, environment, limit, observer, &mut host, &mut stopped);
    if built.is_err() {
        lost.append(&mut stopped);
        lost.extend(host.iter().flat_map(|host| host.proxy.log()));
    }
    built
}

#[allow(clippy::too_many_arguments)]
fn build_in(scenario: &Scenario, corpus: &Corpus, tool: Tool, program: &Path, server: Option<&Path>, scratch: &Scratch, environment: &Environment, limit: Duration, observer: Option<&Observer>, host: &mut Option<Host>, stopped: &mut Vec<Exchange>) -> Result<Built, RunError> {
    let corpus = &scenario.corpus(corpus);
    let run_in = |program: &Path, arguments: Vec<String>, directory: PathBuf| -> Result<Outcome, RunError> {
        let invocation = Invocation { program: PathBuf::from(program), arguments, directory, limit };
        let outcome = match observer {
            Some(observer) if tool == Tool::Forklift => {
                let served = server == Some(program);
                let tree = if served { host_root(scratch) } else { invocation.directory.clone() };
                observer.run(&invocation, served, &tree, scratch, environment)
            }
            _ => tool::run(&invocation, scratch, environment),
        }.map_err(RunError::Tool)?;
        if !outcome.succeeded() {
            return Err(RunError::BuildFailed {
                arguments: outcome.arguments.clone(), exit: outcome.exit,
                stderr: String::from_utf8_lossy(&outcome.stderr).into_owned(),
                stdout: String::from_utf8_lossy(&outcome.stdout).into_owned(),
            });
        }
        Ok(outcome)
    };

    let mut outcomes = vec![run_in(program, prepare(tool), scratch.work())?];
    if tool != Tool::Forklift {
        for [key, value] in GIT_CREATED_CONFIG {
            outcomes.push(run_in(program, vec!["config".into(), key.to_string(), value.to_string()], scratch.work())?);
        }
    }
    let watched = tool == Tool::GitTuned && has_fsmonitor_daemon(program);
    let mut daemon = None;
    if watched {
        let (started, said) = FsmonitorDaemon::start(program, &scratch.work(), scratch, environment, limit)?;
        daemon = Some(started);
        outcomes.extend(said);
    }
    let mut acknowledged = Vec::new();
    let mut pushed = Pushed::default();
    let mut taken = Vec::new();
    let mut ledger = RedactionLedger::default();
    // The folder a sparse clone scoped the work tree to, which bounds what a later write may hold.
    let mut scope: Option<&str> = None;
    // Values coverage lines kept from earlier documents, by name.
    let mut kept: BTreeMap<String, String> = BTreeMap::new();
    // A git history's host, by its path, once it has one.
    let mut bare: Option<String> = None;
    for op in &scenario.history {
        // Read at each step: a clone onto a folding volume moves the work tree.
        let mut work = scratch.work();
        if let Some(snapshot) = op.writes(corpus) {
            let snapshot = snapshot.map_err(|reason| RunError::Io(format!("scenario {}: {}", scenario.id, reason)))?;
            let snapshot = match scope { Some(scope) => corpus::within(&snapshot, scope), None => snapshot };
            corpus::write(&snapshot, &corpus.modes, &work, scratch.work_moved()).map_err(|error| RunError::Io(error.to_string()))?;
        }
        let mut directory = work.clone();
        match op {
            Op::Host if tool != Tool::Forklift => {
                // Git serves itself: the host is a bare repository, which a push and a pull reach
                // by its path. Made on the default branch the work tree's prepare names, with the
                // same configuration, so nothing it runs detaches.
                let root = host_root(scratch);
                let path = root.display().to_string();
                outcomes.push(run_in(program, vec!["init".into(), "--bare".into(), "-q".into(), "-b".into(), "main".into(), path.clone()], scratch.root().to_path_buf())?);
                for [key, value] in GIT_CREATED_CONFIG {
                    outcomes.push(run_in(program, vec!["config".into(), key.to_string(), value.to_string()], root.clone())?);
                }
                bare = Some(path);
            }
            Op::Host => {
                let server = server.ok_or_else(|| RunError::Io(format!("scenario {} hosts a warehouse, and no forklift-server binary was given", scenario.id)))?;
                let root = host_root(scratch);
                outcomes.push(run_in(server, vec!["prepare".into(), "--root".into(), root.display().to_string()], scratch.root().to_path_buf())?);
                // Taken out first, so a new host that fails to start leaves no old one behind to
                // log a second time.
                if let Some(old) = host.take() {
                    stopped.extend(finish(old, observer, scratch)?);
                }
                *host = Some(match observer {
                    Some(observer) => observer.host(server, &root, scratch, environment, limit),
                    None => Host::start(server, &root, scratch, environment, limit),
                }.map_err(RunError::Tool)?);
            }
            Op::Bundle => {
                let server = server.filter(|_| host.is_some())
                    .ok_or_else(|| RunError::Io(format!("scenario {} bundles before it hosts a warehouse", scenario.id)))?;
                let root = host_root(scratch);
                outcomes.push(run_in(server, vec!["bundle".into(), "--root".into(), root.display().to_string()], scratch.root().to_path_buf())?);
            }
            Op::Withhold { index } => {
                let server = server.filter(|_| host.is_some())
                    .ok_or_else(|| RunError::Io(format!("scenario {} withholds before it hosts a warehouse", scenario.id)))?;
                let (path, bytes) = crate::scenario::taken(corpus, *index)
                    .ok_or_else(|| RunError::Io(format!("scenario {}: record {} holds nothing no later record holds", scenario.id, index)))?;
                let parcel = parcel_of(Mark::Record(*index), &work, &mut |arguments, directory| run_in(program, arguments, directory))?;
                let hash = object_hash(&parcel, &path, &work, &mut |arguments, directory| run_in(program, arguments, directory))?;
                let root = host_root(scratch);
                outcomes.push(run_in(server, vec![
                    "withhold".into(), "add".into(), "--root".into(), root.display().to_string(),
                    "--statement".into(), crate::scenario::TAKEN_BECAUSE.into(), hash.clone(),
                ], scratch.root().to_path_buf())?);
                // The server deletes its own copy within seconds of a withhold; a fault before
                // that would grade the window, not the state a host is in from then on. Wait for
                // the loose object to go, as the census lays it out.
                let copy = root.join(".forklift").join("objects").join(&hash[..2]).join(&hash[2..]);
                let started = std::time::Instant::now();
                while std::fs::symlink_metadata(&copy).is_ok() {
                    if started.elapsed() > limit {
                        return Err(RunError::Io(format!("scenario {}: the host kept its copy of withheld {} past {:?}", scenario.id, hash, limit)));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let digest = crate::corpus::expected_entry(&path, &bytes, &corpus.modes);
                taken.push(Taken { path, digest, by: TakenBy::Withheld });
            }
            Op::Redact { index } => {
                let (path, bytes) = crate::scenario::taken(corpus, *index)
                    .ok_or_else(|| RunError::Io(format!("scenario {}: record {} holds nothing no later record holds", scenario.id, index)))?;
                let parcel = parcel_of(Mark::Record(*index), &work, &mut |arguments, directory| run_in(program, arguments, directory))?;
                let target = format!("{}:{}", parcel, path);
                let mut lines = Vec::new();
                for arguments in bind(tool, op).ok_or_else(|| RunError::Io(format!("{:?} has no binding for {:?}", tool, op)))? {
                    lines.push(arguments.into_iter().map(|argument| if argument == TARGET { target.clone() } else { argument }).collect::<Vec<_>>());
                }
                let mut recorded = None;
                for arguments in lines {
                    let outcome = run_in(program, arguments, work.clone())?;
                    recorded = Some(document(&outcome.stdout));
                    outcomes.push(outcome);
                }
                let recorded = recorded.unwrap_or_default();
                let hash = recorded["data"]["hash"].as_str()
                    .ok_or_else(|| RunError::Io(format!("scenario {}: the redaction reported no hash", scenario.id)))?.to_string();
                // The state the warehouse reports, not the exit status: `redact list` for applied,
                // and the recording's own deletion report for the copy deleted.
                let listed = run_in(program, vec!["--json".into(), "redact".into(), "list".into()], work.clone())?;
                if let Some(state) = listed_state(&document(&listed.stdout), &hash) {
                    ledger.observe("client", &hash, state);
                }
                outcomes.push(listed);
                let deleted = recorded["data"]["deletion"]["deleted"].as_array()
                    .is_some_and(|deleted| deleted.iter().any(|deleted| deleted.as_str() == Some(hash.as_str())));
                if deleted {
                    ledger.observe("client", &hash, RedactionState::Deleted);
                }
                let digest = crate::corpus::expected_entry(&path, &bytes, &corpus.modes);
                let admissible = ledger.admissible("client", &hash, &digest, "content_redacted");
                taken.push(Taken { path, digest, by: TakenBy::Redacted { hash, admissible } });
                // Its lines ran above, with the target filled.
                continue;
            }
            Op::EnrollHost => {
                let url = host.as_ref().map(|host| host.url.clone())
                    .ok_or_else(|| RunError::Io(format!("scenario {} enrolls a host before it hosts one", scenario.id)))?;
                let elsewhere = scratch.root().join("enroller");
                outcomes.push(run_in(program, vec!["--json".into(), "clone".into(), url, elsewhere.display().to_string()], scratch.root().to_path_buf())?);
                for command in [["office", "enroll"].as_slice(), ["lift"].as_slice()] {
                    let arguments = std::iter::once("--json").chain(command.iter().copied()).map(str::to_string).collect();
                    outcomes.push(run_in(program, arguments, elsewhere.clone())?);
                }
            }
            Op::Run { line, at, keep } => {
                let directory = scratch.root().join(at);
                std::fs::create_dir_all(&directory).map_err(|error| RunError::Io(error.to_string()))?;
                let arguments = filled(line, &kept, host.as_ref().map(|host| host.url.as_str()).or(bare.as_deref()), scratch.root(), &work)
                    .map_err(|reason| RunError::Io(format!("scenario {}: {}", scenario.id, reason)))?;
                let outcome = run_in(program, std::iter::once("--json".to_string()).chain(arguments).collect(), directory)?;
                for keep in keep {
                    let value = kept_value(&document(&outcome.stdout), keep)
                        .ok_or_else(|| RunError::Io(format!("scenario {}: `{}` gave no {} at {}", scenario.id, line.join(" "), keep.name, keep.pointer)))?;
                    kept.insert(keep.name.clone(), value);
                }
                outcomes.push(outcome);
                continue;
            }
            Op::Serve { line } => {
                let server = server.ok_or_else(|| RunError::Io(format!("scenario {} runs a server command, and no forklift-server binary was given", scenario.id)))?;
                let mut arguments = line.clone();
                arguments.extend(["--root".to_string(), host_root(scratch).display().to_string()]);
                outcomes.push(run_in(server, arguments, scratch.root().to_path_buf())?);
                continue;
            }
            Op::StopHost => {
                let gone = host.take().ok_or_else(|| RunError::Io(format!("scenario {} stops a host it does not serve", scenario.id)))?;
                stopped.extend(finish(gone, observer, scratch)?);
                // The kill leaves the server's lock behind, its holder gone: the harness killed
                // the one holder, so it clears it, as an operator would.
                let lock = host_root(scratch).join(".forklift").join("serve.lock");
                if std::fs::symlink_metadata(&lock).is_ok() {
                    #[allow(clippy::disallowed_methods)] // the lock of a server the harness just killed
                    std::fs::remove_file(&lock).map_err(|error| RunError::Io(format!("scenario {}: clearing the stopped host's lock: {}", scenario.id, error)))?;
                }
                continue;
            }
            Op::LoseKeys => {
                #[allow(clippy::disallowed_methods)] // the scratch home's keys, which this step exists to lose
                std::fs::remove_dir_all(scratch.home().join(".forklift-keys")).map_err(|error| RunError::Io(format!("scenario {}: losing the keys: {}", scenario.id, error)))?;
                continue;
            }
            Op::Age => {
                age(&host_root(scratch).join(".forklift").join("objects"), Duration::from_secs(2 * 24 * 60 * 60))
                    .map_err(|error| RunError::Io(format!("scenario {}: backdating the host's objects: {}", scenario.id, error)))?;
                continue;
            }
            Op::Write { path, text } => {
                let target = work.join(path);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| RunError::Io(error.to_string()))?;
                }
                std::fs::write(&target, text).map_err(|error| RunError::Io(error.to_string()))?;
                continue;
            }
            Op::Inject { line, at, until } => {
                // Without a tracer the state cannot be built, and nothing that needs it is checked.
                let Some((observer, strace)) = observer.and_then(|observer| Some((observer, observer.strace?))) else { continue };
                let directory = scratch.root().join(at);
                let arguments: Vec<String> = std::iter::once("--json".to_string())
                    .chain(filled(line, &kept, host.as_ref().map(|host| host.url.as_str()).or(bare.as_deref()), scratch.root(), &work).map_err(|reason| RunError::Io(format!("scenario {}: {}", scenario.id, reason)))?)
                    .collect();
                let mut stood = false;
                for when in 1..=INJECT_ATTEMPTS {
                    let trace = observer.trace_path("inject");
                    let filter = ["-e".to_string(), "trace=fsync".into(), "-e".into(), format!("inject=fsync:error=EIO:when={}", when)];
                    let traced = crate::kill::strace_line(&trace, &filter, program, &arguments);
                    // An attempt is meant to fail; what it left is what `until` judges.
                    tool::run(&Invocation { program: strace.to_path_buf(), arguments: traced, directory: directory.clone(), limit }, scratch, environment).map_err(RunError::Tool)?;
                    #[allow(clippy::disallowed_methods)] // the harness's own trace, which nothing reads
                    let _ = std::fs::remove_file(&trace);
                    stood = match until {
                        Until::Holds { folder } => std::fs::read_dir(directory.join(folder)).is_ok_and(|mut entries| entries.next().is_some()),
                        Until::Says { line, pointer, equals } => {
                            let asked = Invocation { program: program.to_path_buf(), arguments: std::iter::once("--json".to_string()).chain(line.iter().cloned()).collect(), directory: directory.clone(), limit };
                            let said = tool::run(&asked, scratch, environment).map_err(RunError::Tool)?;
                            document(&said.stdout).pointer(pointer).and_then(|value| value.as_str()) == Some(equals.as_str())
                        }
                    };
                    if stood {
                        break;
                    }
                }
                if !stood {
                    return Err(RunError::Io(format!("scenario {}: no failed fsync of `{}` up to #{} left {:?}", scenario.id, line.join(" "), INJECT_ATTEMPTS, until)));
                }
                continue;
            }
            Op::Clone { .. } => {
                // The work tree's daemon goes with it.
                daemon = None;
                // A clone makes its target; the work tree it replaces is the harness's own scratch.
                #[allow(clippy::disallowed_methods)] // a scratch work tree the harness made; no retry is wanted
                std::fs::remove_dir_all(&work).map_err(|error| RunError::Io(error.to_string()))?;
                directory = scratch.root().to_path_buf();
                // With a folding volume, the clone lands there instead, and the work tree stays
                // there for the rest of the run.
                if let Some(moved) = scratch.move_work().map_err(|error| RunError::Io(error.to_string()))? {
                    work = moved;
                }
            }
            _ => {}
        }
        let lines = bind(tool, op).ok_or_else(|| RunError::Io(format!("{:?} has no binding for {:?}", tool, op)))?;
        for arguments in lines {
            let arguments = fill(arguments, host.as_ref().map(|host| host.url.as_str()).or(bare.as_deref()), &work)
                .map_err(|placeholder| RunError::Io(format!("scenario {}: {:?} needs {} before a Host", scenario.id, op, placeholder)))?;
            outcomes.push(run_in(program, arguments, directory.clone())?);
        }
        if let Op::Clone { kind } = op {
            scope = match kind { crate::scenario::CloneKind::Only(scope) => Some(scope.as_str()), _ => None };
            // A clone's repository is new, and git's configuration is per repository.
            if tool != Tool::Forklift {
                for [key, value] in GIT_CREATED_CONFIG {
                    outcomes.push(run_in(program, vec!["config".into(), key.to_string(), value.to_string()], work.clone())?);
                }
            }
            if watched {
                let (started, said) = FsmonitorDaemon::start(program, &work, scratch, environment, limit)?;
                daemon = Some(started);
                outcomes.extend(said);
            }
        }
        if let Some(mark) = op.mark() {
            acknowledged.push(mark);
        }
        pushed.after(op, &acknowledged);
    }

    let parks = scenario.history.iter().any(|op| matches!(op, Op::Park { .. }));
    let parked = if tool != Tool::Forklift && parks {
        let listed = run_in(program, STASH_LIST.iter().map(|argument| argument.to_string()).collect(), scratch.work())?;
        // Refusing or unreadable fails the build, as forklift's listing does below.
        let parked = stashed_commits(&listed.stdout).filter(|commits| !commits.is_empty())
            .ok_or_else(|| RunError::Io(format!("scenario {}: the stash listing printed no stash commit: {}", scenario.id, String::from_utf8_lossy(&listed.stdout))));
        outcomes.push(listed);
        parked?
    } else if tool == Tool::Forklift && parks {
        let listed = run_in(program, vec!["--json".into(), "park".into(), "list".into()], scratch.work())?;
        // The history runs on an undamaged warehouse, so a listing that refuses, or exits 0 but
        // cannot be read, fails the build: an empty list here would let every readback pass
        // whatever happened to the parks.
        let parked = match listed.succeeded() {
            true => parked_parcels(&listed.stdout).ok_or_else(|| format!(
                "printed no parked list: {}", String::from_utf8_lossy(&listed.stdout)
            )),
            false => Err(format!(
                "exited {:?}: {}{}", listed.exit, String::from_utf8_lossy(&listed.stdout), String::from_utf8_lossy(&listed.stderr)
            )),
        };
        outcomes.push(listed);
        parked.map_err(|why| RunError::Io(format!("scenario {}: `forklift park list --json` {}", scenario.id, why)))?
    } else {
        Vec::new()
    };

    let walked = census::walk(&scratch.work().join(state_folder(tool))).map_err(|error| RunError::Io(error.to_string()))?;
    let host_walked = match (host.as_ref(), &bare) {
        (Some(_), _) => census::walk(&host_root(scratch).join(".forklift")).map_err(|error| RunError::Io(error.to_string()))?,
        (None, Some(_)) => census::walk(&host_root(scratch)).map_err(|error| RunError::Io(error.to_string()))?,
        (None, None) => Vec::new(),
    };
    // Observed, the build's host goes with its build, so its traces are read with the rest.
    if observer.is_some() {
        if let Some(last) = host.take() {
            stopped.extend(finish(last, observer, scratch)?);
        }
    }
    Ok(Built { outcomes, walked, host_walked, taken, acknowledged, pushed: pushed.marks, parked, host: host.take(), stopped: std::mem::take(stopped), bare, daemon })
}

/// Stops `host`: through `observer` when there is one, so its traces are read, and otherwise by
/// killing it. What its proxy logged is returned.
fn finish(host: Host, observer: Option<&Observer>, scratch: &Scratch) -> Result<Vec<Exchange>, RunError> {
    match observer {
        Some(observer) => observer.finish_host(host, &host_root(scratch), scratch).map_err(RunError::Tool),
        None => {
            let log = host.proxy.log();
            drop(host);
            Ok(log)
        }
    }
}

/// A tool's JSON document, or null when its stdout is not one.
pub fn document(stdout: &[u8]) -> serde_json::Value {
    serde_json::from_slice(stdout).unwrap_or_default()
}

/// The (description, parcel) of every entry a `history --json` listing holds, newest first; empty
/// when the document is not one.
pub fn described_parcels(stdout: &[u8]) -> Vec<(String, String)> {
    document(stdout)["data"]["entries"].as_array().map(|entries| entries.iter().filter_map(|entry| {
        Some((entry["description"].as_str()?.to_string(), entry["parcel"].as_str()?.to_string()))
    }).collect()).unwrap_or_default()
}

/// The parcels `park list --json` names, newest first; nothing when it printed no parked list, or
/// an entry without a parcel.
pub fn parked_parcels(stdout: &[u8]) -> Option<Vec<String>> {
    document(stdout)["data"]["parked"].as_array()?.iter()
        .map(|entry| Some(entry["parcel"].as_str()?.to_string())).collect()
}

/// The state `redact list --json` reports for redaction `hash`, if it lists it.
fn listed_state(listed: &serde_json::Value, hash: &str) -> Option<RedactionState> {
    let entry = listed["data"]["redactions"].as_array()?.iter().find(|entry| entry["hash"].as_str() == Some(hash))?;
    match entry["status"]["state"].as_str()? {
        "pending" => Some(RedactionState::Pending),
        "applied" => Some(RedactionState::Applied),
        _ => None,
    }
}

type Runner<'a> = dyn FnMut(Vec<String>, PathBuf) -> Result<Outcome, RunError> + 'a;

/// The parcel whose description is `mark`'s message on the current pallet, from `history --json`.
fn parcel_of(mark: Mark, work: &Path, run: &mut Runner) -> Result<String, RunError> {
    let listing = run(vec!["--json".into(), "history".into()], work.to_path_buf())?;
    described_parcels(&listing.stdout).into_iter().find(|(description, _)| *description == mark.message()).map(|(_, parcel)| parcel)
        .ok_or_else(|| RunError::Io(format!("no parcel of the history is described {:?}", mark.message())))
}

/// The object hash of the file at `path` in `parcel`, read by walking the parcel's trees with
/// `store peek`.
fn object_hash(parcel: &str, path: &str, work: &Path, run: &mut Runner) -> Result<String, RunError> {
    let peek = |run: &mut Runner, hash: &str| -> Result<serde_json::Value, RunError> {
        Ok(document(&run(vec!["--json".into(), "store".into(), "peek".into(), hash.to_string()], work.to_path_buf())?.stdout))
    };
    let mut hash = peek(run, parcel)?["data"]["tree"].as_str()
        .ok_or_else(|| RunError::Io(format!("parcel {} names no tree", parcel)))?.to_string();
    for name in path.split('/') {
        let tree = peek(run, &hash)?;
        hash = tree["data"]["entries"].as_array().and_then(|entries| entries.iter().find(|entry| entry["name"].as_str() == Some(name)))
            .and_then(|entry| entry["hash"].as_str()).map(str::to_string)
            .ok_or_else(|| RunError::Io(format!("{} is not in parcel {}'s tree", path, parcel)))?;
    }
    Ok(hash)
}

/// Fills a binding's placeholders: the host's URL, and the work tree's path. A URL placeholder
/// with no host yet is an error naming it.
pub fn fill(arguments: Vec<String>, url: Option<&str>, work: &Path) -> Result<Vec<String>, &'static str> {
    arguments.into_iter().map(|argument| match argument.as_str() {
        URL => url.map(str::to_string).ok_or(URL),
        WORK => Ok(work.display().to_string()),
        _ => Ok(argument),
    }).collect()
}

/// How many fsyncs an `Inject` fails in turn, one per attempt, before it gives up on its state.
const INJECT_ATTEMPTS: usize = 32;

/// Fills a coverage line's placeholders, anywhere in an argument: kept values by `{name}`, the
/// host's URL, the scratch root and the work tree. One left unfilled is an error naming it.
pub fn filled(line: &[String], kept: &BTreeMap<String, String>, url: Option<&str>, root: &Path, work: &Path) -> Result<Vec<String>, String> {
    let mut values: Vec<(String, String)> = kept.iter().map(|(name, value)| (format!("{{{}}}", name), value.clone())).collect();
    values.push((SCRATCH.to_string(), root.display().to_string()));
    values.push((WORK.to_string(), work.display().to_string()));
    if let Some(url) = url {
        values.push((URL.to_string(), url.to_string()));
    }
    line.iter().map(|argument| {
        let mut argument = argument.clone();
        for (placeholder, value) in &values {
            argument = argument.replace(placeholder.as_str(), value);
        }
        match (argument.find('{'), argument.find('}')) {
            (Some(open), Some(close)) if open < close => Err(format!("`{}` has no value for {}", line.join(" "), &argument[open..=close])),
            _ => Ok(argument),
        }
    }).collect()
}

/// The value `keep` names in a document: the string at its pointer, from the text after its
/// `after` when it has one, split on whitespace, word `word`.
pub fn kept_value(document: &serde_json::Value, keep: &Keep) -> Option<String> {
    let text = document.pointer(&keep.pointer)?.as_str()?;
    let text = match &keep.after {
        Some(after) => &text[text.find(after.as_str())? + after.len()..],
        None => return (keep.word == 0).then(|| text.to_string()),
    };
    text.split_whitespace().nth(keep.word).map(str::to_string)
}

#[cfg(windows)]
const FILE_WRITE_ATTRIBUTES: u32 = 0x100;

/// Sets every file under `root` back by `by`, so a grace period measured from a write has passed.
fn age(root: &Path, by: Duration) -> std::io::Result<()> {
    let then = std::time::SystemTime::now() - by;
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        if std::fs::symlink_metadata(&path)?.is_dir() {
            for entry in std::fs::read_dir(&path)? {
                stack.push(entry?.path());
            }
        } else {
            // Windows sets a time only through a handle opened to write attributes, which a
            // read-only file still grants; elsewhere any handle the owner opens will do.
            let mut options = std::fs::OpenOptions::new();
            #[cfg(windows)]
            std::os::windows::fs::OpenOptionsExt::access_mode(&mut options, FILE_WRITE_ATTRIBUTES);
            #[cfg(not(windows))]
            options.read(true);
            options.open(&path)?.set_modified(then)?;
        }
    }
    Ok(())
}

/// The census names some walked path matches: the names this build creates.
pub fn created_names(census: &Census, walked: &[String]) -> Vec<String> {
    census.damage_names().into_iter().map(|name| name.path)
        .filter(|pattern| walked.iter().any(|path| census::matches(pattern, path)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A remote is claimed to hold what a push sent only while the history kept one pallet
    /// current and left the remote's content alone; any other op drops the claim for good.
    ///
    /// Revert: set the marks at every push → a push from a side pallet claims main's records;
    /// keep them past a deleted remote pallet → its records are still owed; let any clone keep the
    /// claim → records the clone threw away are owed.
    #[test]
    fn a_remote_is_owed_only_what_a_linear_history_pushed() {
        let replay = |history: &[Op]| {
            let (mut pushed, mut acknowledged) = (Pushed::default(), Vec::new());
            for op in history {
                acknowledged.extend(op.mark());
                pushed.after(op, &acknowledged);
            }
            pushed.marks
        };
        let record = |index| Op::Record { index };
        assert_eq!(replay(&[Op::Host, record(0), record(1), Op::Push, record(2)]), [Mark::Record(0), Mark::Record(1)]);
        assert_eq!(replay(&[Op::Host, record(0), Op::Push, Op::Clone { kind: crate::scenario::CloneKind::Full }, record(1), Op::Push]),
            [Mark::Record(0), Mark::Record(1)]);
        assert!(replay(&[record(0)]).is_empty());
        assert!(replay(&[Op::Host, record(0), Op::Push, Op::Branch { name: "side".into() }, Op::Switch { name: "side".into() }, record(1), Op::Push]).is_empty());
        assert!(replay(&[Op::Host, record(0), Op::Push, Op::DeleteRemote { name: "main".into() }]).is_empty());
        assert!(replay(&[Op::Host, record(0), Op::Push, record(1), Op::Clone { kind: crate::scenario::CloneKind::Full }, Op::Push]).is_empty(),
            "a clone over unpushed work claims nothing more");
        assert!(replay(&[Op::Host, record(0), Op::Push, Op::Host, record(1), Op::Push]).is_empty());
    }
    use std::collections::BTreeMap;

    fn strings(arguments: &[&str]) -> Vec<String> {
        arguments.iter().map(|argument| argument.to_string()).collect()
    }

    /// A binding's placeholders are filled whole, the URL only once a host serves one, and an
    /// argument merely containing a placeholder's text is left as it is.
    ///
    /// Revert: fill a missing URL with an empty string → the clone line runs `clone "" <work>`.
    #[test]
    fn placeholders_fill_whole_arguments_and_a_missing_host_is_an_error() {
        let work = Path::new("/s/work");
        assert_eq!(fill(strings(&["clone", URL, WORK, "-m", "x{url}"]), Some("http://127.0.0.1:9"), work).unwrap(),
            strings(&["clone", "http://127.0.0.1:9", "/s/work", "-m", "x{url}"]));
        assert_eq!(fill(strings(&["clone", URL, WORK]), None, work), Err(URL));
        assert_eq!(fill(strings(&["stack", "-m", WORK]), None, work).unwrap(), strings(&["stack", "-m", "/s/work"]));
    }

    /// A coverage line fills its placeholders anywhere in an argument, kept values by name; one
    /// left unfilled is an error naming it, never passed on as text.
    ///
    /// Revert: fill whole arguments only, as `fill` does → `{parcel}:notes.txt` reaches the tool
    /// as written.
    #[test]
    fn coverage_lines_fill_within_arguments_and_refuse_a_missing_value() {
        let kept = BTreeMap::from([("parcel".to_string(), "ab12".to_string())]);
        let (root, work) = (Path::new("/s"), Path::new("/s/work"));
        assert_eq!(filled(&strings(&["redact", "record", "{parcel}:notes.txt", "{scratch}/mirror", "{url}"]), &kept, Some("http://h"), root, work).unwrap(),
            strings(&["redact", "record", "ab12:notes.txt", "/s/mirror", "http://h"]));
        assert_eq!(filled(&strings(&["echo", "--", "{root}"]), &kept, None, root, work), Err("`echo -- {root}` has no value for {root}".to_string()));
        assert!(filled(&strings(&["clone", "{url}"]), &kept, None, root, work).is_err(), "no host yet");
    }

    /// A kept value is the string at its pointer, or one word of the text after a marker in it.
    ///
    /// Revert: split the whole string when no marker is given → a kept parcel hash with a space
    /// in its message keeps only its first word.
    #[test]
    fn a_kept_value_is_the_string_or_a_word_after_its_marker() {
        let document = serde_json::json!({"data": {"parcel": "ab12", "message": "Run: office admit op pk pop. Then: office link pk2 pop2"}});
        let keep = |pointer: &str, after: Option<&str>, word| Keep { name: "k".into(), pointer: pointer.into(), after: after.map(str::to_string), word };
        assert_eq!(kept_value(&document, &keep("/data/parcel", None, 0)).as_deref(), Some("ab12"));
        assert_eq!(kept_value(&document, &keep("/data/message", None, 0)).as_deref(), Some("Run: office admit op pk pop. Then: office link pk2 pop2"));
        assert_eq!(kept_value(&document, &keep("/data/message", Some("office admit"), 2)).as_deref(), Some("pop."));
        assert_eq!(kept_value(&document, &keep("/data/message", Some("office link"), 0)).as_deref(), Some("pk2"));
        assert_eq!(kept_value(&document, &keep("/data/message", Some("absent"), 0)), None);
        assert_eq!(kept_value(&document, &keep("/data/none", None, 0)), None);
    }

    /// Dropping the daemon stops one git started on its own after the harness's went down, which
    /// killing the harness's child alone would leave watching.
    ///
    /// Revert: drop the `stop` from Drop → `status` still says a daemon watches.
    #[test]
    fn dropping_the_daemon_stops_one_git_started_on_its_own() {
        let name = if cfg!(windows) { "git.exe" } else { "git" };
        let git = std::env::split_paths(&std::env::var_os("PATH").expect("a PATH")).map(|folder| folder.join(name))
            .find(|path| path.is_file()).expect("a git on PATH");
        if !has_fsmonitor_daemon(&git) {
            return;
        }
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("proof-load-git-autostart-{}-{}", std::process::id(), nanos));
        let scratch = Scratch::create(&root).unwrap();
        let search = crate::scrub::search_path(Some(&git), std::env::var_os("PATH"));
        let environment = Environment::scrubbed(&scratch, search, &BTreeMap::new()).unwrap();
        let git_in = |arguments: &[&str]| tool::run(&Invocation {
            program: git.clone(), arguments: arguments.iter().map(|argument| argument.to_string()).collect(), directory: scratch.work(), limit: Duration::from_secs(30),
        }, &scratch, &environment).unwrap();
        assert!(git_in(&["init", "-q", "-b", "main"]).succeeded());
        let (mut daemon, _) = FsmonitorDaemon::start(&git, &scratch.work(), &scratch, &environment, Duration::from_secs(60)).unwrap();
        // The harness's daemon goes; the next command that queries starts git's own.
        let _ = daemon.child.kill();
        let _ = daemon.child.wait();
        let started = std::time::Instant::now();
        while !git_in(&["fsmonitor--daemon", "status"]).succeeded() {
            assert!(git_in(&["status", "--porcelain"]).succeeded());
            assert!(started.elapsed() < Duration::from_secs(30), "git started no daemon of its own");
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(daemon);
        assert!(!git_in(&["fsmonitor--daemon", "status"]).succeeded(), "git's own daemon outlived the drop");
    }

    /// Watching configures nothing, so a copy whose state is under test keeps its config; starting
    /// turns the fsmonitor on.
    ///
    /// Revert: configure in `watch` → the config gains `fsmonitor` and is no longer byte-identical.
    #[test]
    fn watching_leaves_the_config_as_it_was() {
        let name = if cfg!(windows) { "git.exe" } else { "git" };
        let git = std::env::split_paths(&std::env::var_os("PATH").expect("a PATH")).map(|folder| folder.join(name))
            .find(|path| path.is_file()).expect("a git on PATH");
        if !has_fsmonitor_daemon(&git) {
            return;
        }
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("proof-load-git-watch-{}-{}", std::process::id(), nanos));
        let scratch = Scratch::create(&root).unwrap();
        let search = crate::scrub::search_path(Some(&git), std::env::var_os("PATH"));
        let environment = Environment::scrubbed(&scratch, search, &BTreeMap::new()).unwrap();
        let git_in = |arguments: &[&str]| tool::run(&Invocation {
            program: git.clone(), arguments: arguments.iter().map(|argument| argument.to_string()).collect(), directory: scratch.work(), limit: Duration::from_secs(30),
        }, &scratch, &environment).unwrap();
        assert!(git_in(&["init", "-q", "-b", "main"]).succeeded());
        let config = scratch.work().join(".git").join("config");
        let before = std::fs::read(&config).unwrap();
        let (daemon, status) = FsmonitorDaemon::watch(&git, &scratch.work(), &scratch, &environment, Duration::from_secs(60)).unwrap();
        assert!(status.succeeded());
        assert_eq!(std::fs::read(&config).unwrap(), before, "watching rewrote the config");
        drop(daemon);
        let (daemon, _) = FsmonitorDaemon::start(&git, &scratch.work(), &scratch, &environment, Duration::from_secs(60)).unwrap();
        drop(daemon);
        assert!(String::from_utf8_lossy(&std::fs::read(&config).unwrap()).contains("fsmonitor = true"), "starting turned no fsmonitor on");
    }

    /// A daemon over a work tree git cannot take for a repository exits, which is told apart from
    /// a harness that could not start one; dropping a stop guard stops a daemon git started on its
    /// own.
    ///
    /// Revert: report the exit as the harness's → the torn HEAD reads as a harness failure; leave
    /// the guard's drop empty → git's own daemon still watches.
    #[test]
    fn an_exited_daemon_is_told_from_the_harness_and_a_guard_stops_git_s_own() {
        let name = if cfg!(windows) { "git.exe" } else { "git" };
        let git = std::env::split_paths(&std::env::var_os("PATH").expect("a PATH")).map(|folder| folder.join(name))
            .find(|path| path.is_file()).expect("a git on PATH");
        if !has_fsmonitor_daemon(&git) {
            return;
        }
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("proof-load-git-unwatched-{}-{}", std::process::id(), nanos));
        let scratch = Scratch::create(&root).unwrap();
        let search = crate::scrub::search_path(Some(&git), std::env::var_os("PATH"));
        let environment = Environment::scrubbed(&scratch, search, &BTreeMap::new()).unwrap();
        let git_in = |arguments: &[&str]| tool::run(&Invocation {
            program: git.clone(), arguments: arguments.iter().map(|argument| argument.to_string()).collect(), directory: scratch.work(), limit: Duration::from_secs(30),
        }, &scratch, &environment).unwrap();
        assert!(git_in(&["init", "-q", "-b", "main"]).succeeded());
        assert!(git_in(&["config", "core.fsmonitor", "true"]).succeeded());
        let missing = scratch.root().join("no-such-git");
        assert!(matches!(FsmonitorDaemon::watch_or_exit(&missing, &scratch.work(), &scratch, &environment, Duration::from_secs(10)), Err(Unwatched::Harness(_))));
        // Git's own daemon, which a command over a tuned tree starts as the harness's does not.
        assert!(git_in(&["fsmonitor--daemon", "start"]).succeeded());
        assert!(git_in(&["fsmonitor--daemon", "status"]).succeeded());
        drop(StopsDaemon { program: git.clone(), work: scratch.work(), environment: environment.clone() });
        assert!(!git_in(&["fsmonitor--daemon", "status"]).succeeded(), "the guard left git's own daemon watching");
        let head = scratch.work().join(".git").join("HEAD");
        std::fs::write(&head, b"").unwrap();
        assert!(matches!(FsmonitorDaemon::watch_or_exit(&git, &scratch.work(), &scratch, &environment, Duration::from_secs(30)), Err(Unwatched::Exited(_))));
        let _ = crate::power::remove_tree(scratch.root());
    }

    /// A daemon that failed with a code before it watched is git's refusal; one that ended clean
    /// before it watched is the harness's.
    ///
    /// Revert: take every coded exit for a refusal → the clean exit reads as git's answer.
    #[cfg(unix)]
    #[test]
    fn only_a_failed_exit_is_git_s_refusal() {
        use std::os::unix::fs::PermissionsExt;
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("proof-load-daemon-exit-{}-{}", std::process::id(), nanos));
        let scratch = Scratch::create(&root).unwrap();
        let environment = Environment::scrubbed(&scratch, std::env::var_os("PATH"), &BTreeMap::new()).unwrap();
        let unwatched = |code: i32| {
            // `run` ends with `code`; `status` never says it watches.
            let program = scratch.root().join(format!("daemon-{}", code));
            std::fs::write(&program, format!("#!/bin/sh\n[ \"$2\" = run ] && exit {}\nexit 1\n", code)).unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
            FsmonitorDaemon::watch_or_exit(&program, &scratch.work(), &scratch, &environment, Duration::from_secs(10)).err().expect("the daemon never watched")
        };
        assert!(matches!(unwatched(128), Unwatched::Exited(why) if why.contains("code 128")));
        assert!(matches!(unwatched(0), Unwatched::Harness(_)));
        let _ = crate::power::remove_tree(scratch.root());
    }

    /// Only the feature line itself says a git has the fsmonitor daemon.
    ///
    /// Revert: match by substring → the `no` line below reads as the feature.
    #[test]
    fn the_daemon_feature_is_read_from_its_own_line() {
        assert!(builds_fsmonitor_daemon(b"git version 2.55.0\ncpu: arm64\nfeature: fsmonitor--daemon\nlibcurl: 8.7.1\n"));
        assert!(!builds_fsmonitor_daemon(b"git version 2.43.0\ncpu: x86_64\nno commit associated with this build\n"));
        assert!(!builds_fsmonitor_daemon(b"git version 2.43.0\nno feature: fsmonitor--daemon here\n"));
    }

    /// The forklift binary to drive, given by path; the harness never builds or links it.
    fn forklift() -> Option<PathBuf> {
        std::env::var_os("PROOF_LOAD_FORKLIFT").map(PathBuf::from)
    }

    /// A built history leaves every record acknowledged, and its walk lists only paths the census
    /// classifies, with the store, the pallet and the inventory among the names it creates.
    ///
    /// Needs a forklift binary: `PROOF_LOAD_FORKLIFT=<path> cargo test -p proof-load -- --ignored`.
    #[test]
    #[ignore = "drives a forklift binary named by PROOF_LOAD_FORKLIFT"]
    fn a_built_history_is_acknowledged_and_its_walk_is_classified() {
        let program = forklift().expect("PROOF_LOAD_FORKLIFT names the forklift binary under test");
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("proof-load-runner-{}-{}", std::process::id(), nanos));
        let scratch = Scratch::create(&root).unwrap();
        let environment = Environment::scrubbed(&scratch, None, &BTreeMap::new()).unwrap();
        let corpus = corpus::generate(1, corpus::Shape::SMALL);
        let scenario = Scenario {
            id: "history".into(),
            history: vec![Op::Record { index: 0 }, Op::Record { index: 1 }, Op::Record { index: 2 }],
            then: vec![],
            damage: None,
            files: crate::scenario::Files::Seeded,
        };

        let built = build(&scenario, &corpus, Tool::Forklift, &program, None, &scratch, &environment, Duration::from_secs(60), None).unwrap();
        assert_eq!(built.acknowledged, vec![Mark::Record(0), Mark::Record(1), Mark::Record(2)]);

        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let census = Census::load(&source).unwrap().expect("this tree has a census");
        assert_eq!(census.unclassified(&built.walked), Vec::<&str>::new());
        let created = created_names(&census, &built.walked);
        for name in ["objects/<xx>/<hash>", "pallets/<name>", "{bay}inventory/metadata", "warehouse"] {
            assert!(created.iter().any(|created| created == name), "{} is created: {:?}", name, created);
        }
        assert_eq!(crate::oracle::read_tree(&scratch.work()).unwrap(), corpus::expected(&corpus.records[2], &corpus.modes));
    }
}
