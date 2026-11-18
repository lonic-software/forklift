//! Running a tool: one binary, found by an absolute path, run in a scrubbed environment with its
//! working directory inside a scratch area, with everything it said captured.
//!
//! The harness never looks a tool up on PATH: the binary under test is the one the run was given,
//! and a PATH lookup could quietly pick another. A forklift failure under `--json` is one document
//! whose `error` carries a code, a message and an optional next_step; the message and next_step
//! are the texts remedies are extracted from.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::scrub::{Environment, Scratch};

#[derive(Debug, PartialEq, Eq)]
pub enum ToolError {
    /// The binary was not given as an absolute path.
    NotAbsolute(PathBuf),
    /// The working directory is outside the scratch area the tool runs in.
    OutsideScratch(PathBuf),
    /// The tool could not be started.
    Spawn { program: PathBuf, reason: String },
    /// The tool ran past its wall-clock limit and was killed.
    TimedOut { program: PathBuf, limit: Duration },
    /// A server that exited, or had not bound its listener, within its limit.
    NotListening { program: PathBuf, limit: Duration, said: String },
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolError::NotAbsolute(path) => write!(f, "the tool {} is not an absolute path; tools are found by path, never on PATH", path.display()),
            ToolError::OutsideScratch(path) => write!(f, "the working directory {} is outside the run's scratch area", path.display()),
            ToolError::Spawn { program, reason } => write!(f, "could not start {}: {}", program.display(), reason),
            ToolError::TimedOut { program, limit } => write!(f, "{} ran past its {} s limit and was killed", program.display(), limit.as_secs()),
            ToolError::NotListening { program, limit, said } =>
                write!(f, "{} was not listening within {} s: {}", program.display(), limit.as_secs(), said.trim()),
        }
    }
}

impl std::error::Error for ToolError {}

/// One run of one tool.
#[derive(Clone, Debug)]
pub struct Invocation {
    pub program: PathBuf,
    pub arguments: Vec<String>,
    /// Must lie inside the scratch area the invocation runs in.
    pub directory: PathBuf,
    pub limit: Duration,
}

/// A forklift failure document's `error`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub code: String,
    pub message: String,
    pub next_step: Option<String>,
}

/// What a run produced. A row's details file holds these as its `steps`; `Row::steps` reads them.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outcome {
    pub arguments: Vec<String>,
    /// The exit code; None when the process ended on a signal.
    pub exit: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// The `error` of a failure document on stdout, when stdout is one.
    pub failure: Option<Failure>,
}

impl Outcome {
    pub fn succeeded(&self) -> bool {
        self.exit == Some(0)
    }

    /// The texts a refusal's remedies are extracted from: the JSON error's message and next_step.
    pub fn remedy_texts(&self) -> Vec<&str> {
        match &self.failure {
            Some(failure) => std::iter::once(failure.message.as_str()).chain(failure.next_step.as_deref()).collect(),
            None => Vec::new(),
        }
    }
}

/// The `error` of a lonic JSON failure document, or None when `stdout` is not one.
pub fn failure_of(stdout: &[u8]) -> Option<Failure> {
    let document: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    if document.get("ok")? != &serde_json::Value::Bool(false) {
        return None;
    }
    let error = document.get("error")?;
    Some(Failure {
        code: error.get("code")?.as_str()?.to_string(),
        message: error.get("message")?.as_str()?.to_string(),
        next_step: error.get("next_step").and_then(|step| step.as_str()).map(str::to_string),
    })
}

pub(crate) fn drain(mut source: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = source.read_to_end(&mut bytes);
        bytes
    })
}

/// The command `invocation` names, checked: its program by an absolute path, its working
/// directory inside `scratch`, its environment `environment` and nothing inherited, its output
/// piped.
fn prepared(invocation: &Invocation, scratch: &Scratch, environment: &Environment) -> Result<Command, ToolError> {
    if !invocation.program.is_absolute() {
        return Err(ToolError::NotAbsolute(invocation.program.clone()));
    }
    // `starts_with` compares components, so a `..` would climb out of a root it seems to be under.
    let climbs = invocation.directory.components().any(|component| matches!(component, std::path::Component::ParentDir));
    if climbs || !scratch.roots().iter().any(|root| invocation.directory.starts_with(root)) {
        return Err(ToolError::OutsideScratch(invocation.directory.clone()));
    }

    let mut command = Command::new(&invocation.program);
    command.args(&invocation.arguments).current_dir(&invocation.directory).stdout(Stdio::piped()).stderr(Stdio::piped());
    environment.apply(&mut command);
    Ok(command)
}

/// Runs `invocation` in `scratch` under `environment` and captures what it said. Its stdin is
/// closed, so a tool that would prompt sees end of input instead of waiting.
pub fn run(invocation: &Invocation, scratch: &Scratch, environment: &Environment) -> Result<Outcome, ToolError> {
    run_from(invocation, Stdio::null(), scratch, environment)
}

/// [`run`], with the file at `input` as the tool's stdin, as `git fast-import` reads its stream.
pub fn run_with_input(invocation: &Invocation, input: &Path, scratch: &Scratch, environment: &Environment) -> Result<Outcome, ToolError> {
    let file = std::fs::File::open(input).map_err(|error| ToolError::Spawn { program: invocation.program.clone(), reason: format!("{}: {}", input.display(), error) })?;
    run_from(invocation, Stdio::from(file), scratch, environment)
}

fn run_from(invocation: &Invocation, stdin: Stdio, scratch: &Scratch, environment: &Environment) -> Result<Outcome, ToolError> {
    let mut command = prepared(invocation, scratch, environment)?;
    command.stdin(stdin);

    let spawn = |error: std::io::Error| ToolError::Spawn { program: invocation.program.clone(), reason: error.to_string() };
    let mut child = command.spawn().map_err(spawn)?;
    let stdout = drain(child.stdout.take().expect("stdout is piped"));
    let stderr = drain(child.stderr.take().expect("stderr is piped"));

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(spawn)? {
            break status;
        }
        if started.elapsed() > invocation.limit {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ToolError::TimedOut { program: invocation.program.clone(), limit: invocation.limit });
        }
        std::thread::sleep(Duration::from_millis(5));
    };

    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    Ok(Outcome { arguments: invocation.arguments.clone(), exit: status.code(), failure: failure_of(&stdout), stdout, stderr })
}

/// What the operating system measured of one waited child: its CPU time, and the most memory it
/// held resident at once, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    pub user: Duration,
    pub system: Duration,
    pub peak_bytes: u64,
}

/// One timed run: what it said, how long it took from just before its spawn to its exit, and its
/// [`Usage`] where the platform reports one (unix; never Windows).
#[derive(Clone, Debug)]
pub struct Timed {
    pub outcome: Outcome,
    pub wall: Duration,
    pub usage: Option<Usage>,
}

/// Runs `invocation` as [`run`] does, timed for a measurement rather than polled. On unix the
/// harness blocks until the child exits, so the wall time is not rounded up to a poll's interval;
/// the child's own rusage gives its CPU time and peak memory. Its limit is still enforced, by a
/// watchdog that kills it. On Windows only the wall time is read, polled every millisecond.
pub fn timed(invocation: &Invocation, scratch: &Scratch, environment: &Environment) -> Result<Timed, ToolError> {
    let mut command = prepared(invocation, scratch, environment)?;
    command.stdin(Stdio::null());

    let spawn = |error: std::io::Error| ToolError::Spawn { program: invocation.program.clone(), reason: error.to_string() };
    let started = Instant::now();
    let mut child = command.spawn().map_err(spawn)?;
    let stdout = drain(child.stdout.take().expect("stdout is piped"));
    let stderr = drain(child.stderr.take().expect("stderr is piped"));

    let reaped = reap(&mut child, invocation.limit).map_err(spawn)?;
    let wall = reaped.ended.duration_since(started);
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if reaped.killed {
        return Err(ToolError::TimedOut { program: invocation.program.clone(), limit: invocation.limit });
    }
    let outcome = Outcome { arguments: invocation.arguments.clone(), exit: reaped.exit, failure: failure_of(&stdout), stdout, stderr };
    Ok(Timed { outcome, wall, usage: reaped.usage })
}

/// How a timed child ended.
struct Reaped {
    exit: Option<i32>,
    /// When the harness saw it exit.
    ended: Instant,
    usage: Option<Usage>,
    /// Whether the watchdog killed it at its limit.
    killed: bool,
}

/// Waits for `child` to exit, then reaps it with its rusage. The wait does not reap
/// (`WNOWAIT`), so until the watchdog is stopped the child's id is still its own and the
/// watchdog's kill can reach no other process.
#[cfg(unix)]
fn reap(child: &mut std::process::Child, limit: Duration) -> std::io::Result<Reaped> {
    let pid = libc::pid_t::try_from(child.id()).map_err(|_| std::io::Error::other(format!("process id {} is out of range", child.id())))?;
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || match stopped.recv_timeout(limit) {
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            // SAFETY: kill takes no pointers; the child is not reaped until this thread is joined.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            true
        }
        _ => false,
    });
    let waited = loop {
        // SAFETY: waitid fills the zeroed siginfo it is given and returns -1 on failure.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        #[allow(clippy::unnecessary_cast)]
        if unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT) } == 0 {
            break Ok(Instant::now());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            break Err(error);
        }
    };
    drop(stop);
    let killed = watchdog.join().unwrap_or(false);
    let ended = waited?;

    let mut status: libc::c_int = 0;
    // SAFETY: wait4 fills the status and the zeroed rusage it is given and returns -1 on failure.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        let reaped = unsafe { libc::wait4(pid, &mut status, 0, &mut usage) };
        if reaped == pid {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    let exit = if libc::WIFEXITED(status) { Some(libc::WEXITSTATUS(status)) } else { None };
    let time = |value: libc::timeval| Duration::from_secs(u64::try_from(value.tv_sec).unwrap_or(0)) + Duration::from_micros(u64::try_from(value.tv_usec).unwrap_or(0));
    #[allow(clippy::unnecessary_cast)]
    let peak_bytes = peak_bytes_when(!cfg!(target_os = "macos"), usage.ru_maxrss as i64);
    Ok(Reaped { exit, ended, usage: Some(Usage { user: time(usage.ru_utime), system: time(usage.ru_stime), peak_bytes }), killed })
}

#[cfg(not(unix))]
fn reap(child: &mut std::process::Child, limit: Duration) -> std::io::Result<Reaped> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Reaped { exit: status.code(), ended: Instant::now(), usage: None, killed: false });
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Reaped { exit: None, ended: Instant::now(), usage: None, killed: true });
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A peak resident set size as rusage's `ru_maxrss` gives it, in bytes: when `kibibytes`, as Linux
/// and the BSDs report it, in KiB; otherwise, as macOS reports it, already in bytes.
pub fn peak_bytes_when(kibibytes: bool, maxrss: i64) -> u64 {
    let maxrss = u64::try_from(maxrss).unwrap_or(0);
    if kibibytes { maxrss.saturating_mul(1024) } else { maxrss }
}

/// A forklift-server serving one warehouse for the length of a scenario, killed when dropped.
pub struct Served {
    child: std::process::Child,
    /// The base URL it printed once listening.
    pub url: String,
    /// Everything it said, drained so a full pipe never stalls it.
    output: Vec<std::thread::JoinHandle<Vec<u8>>>,
}

impl Served {
    /// Its process id, for a tracer to attach to.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// How its process ended, if it has: a kill the harness did not make, such as an injected one,
    /// or a crash.
    pub fn ended(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// Stops it as its operator would: asked to shut down (SIGTERM, which it drains gracefully,
    /// releasing its serve lock), then waited on for up to `limit`. One still running then is an
    /// error, and is killed hard only when dropped, as every `Served` is: a server killed hard
    /// leaves its lock behind, so the operator's remedy is never run after one.
    pub fn stop(mut self, limit: Duration) -> Result<(), String> {
        #[cfg(unix)]
        {
            let pid = libc::pid_t::try_from(self.child.id()).map_err(|_| format!("the server's process id {} is out of range", self.child.id()))?;
            // SAFETY: kill takes no pointers; the child is ours and not yet reaped, so its id is its own.
            if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
                return Err(format!("the server could not be asked to stop: {}", std::io::Error::last_os_error()));
            }
            let deadline = Instant::now() + limit;
            loop {
                match self.child.try_wait() {
                    Ok(Some(_)) => return Ok(()),
                    Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                    Ok(None) => return Err(format!("the server had not stopped within {:?} of being asked to", limit)),
                    Err(error) => return Err(format!("the server's state could not be read: {}", error)),
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = limit;
            Err("the server can be asked to stop only on Unix".to_string())
        }
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for handle in self.output.drain(..) {
            let _ = handle.join();
        }
    }
}

/// The line forklift-server prints once its listener is bound.
const LISTENING: &str = "forklift-server listening on ";

/// Starts `program serve` over the warehouse at `root`, open and on a port the system picks, and
/// waits up to `limit` for the line naming the address it bound.
pub fn serve(program: &Path, root: &Path, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<Served, ToolError> {
    use std::io::BufRead;
    if !program.is_absolute() {
        return Err(ToolError::NotAbsolute(program.to_path_buf()));
    }
    if !root.starts_with(scratch.root()) {
        return Err(ToolError::OutsideScratch(root.to_path_buf()));
    }
    let mut command = Command::new(program);
    command.arg("serve").arg("--root").arg(root).args(["--open", "--addr", "127.0.0.1:0"])
        .current_dir(scratch.root()).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    environment.apply(&mut command);
    // Under Yama's ptrace scope 1 only an ancestor may attach to a process, and the tracer the
    // harness attaches once serve is listening is its sibling: the server names any tracer as
    // allowed, before it runs. Where Yama is absent the call fails, and nothing needs allowing.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: prctl is async-signal-safe, and the closure allocates nothing.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PTRACER, libc::PR_SET_PTRACER_ANY, 0, 0, 0);
                Ok(())
            });
        }
    }

    let spawn = |error: std::io::Error| ToolError::Spawn { program: program.to_path_buf(), reason: error.to_string() };
    let mut child = command.spawn().map_err(spawn)?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let (sender, receiver) = std::sync::mpsc::channel();
    let lines = std::thread::spawn(move || {
        let mut said = Vec::new();
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(url) = line.strip_prefix(LISTENING) {
                let _ = sender.send(url.trim().to_string());
            }
            said.extend_from_slice(line.as_bytes());
            said.push(b'\n');
        }
        said
    });
    let stderr = drain(child.stderr.take().expect("stderr is piped"));
    let mut served = Served { child, url: String::new(), output: vec![lines, stderr] };
    match receiver.recv_timeout(limit) {
        Ok(url) => {
            served.url = url;
            Ok(served)
        }
        Err(_) => {
            let _ = served.child.kill();
            let _ = served.child.wait();
            let said: Vec<u8> = served.output.drain(..).flat_map(|handle| handle.join().unwrap_or_default()).collect();
            Err(ToolError::NotListening { program: program.to_path_buf(), limit, said: String::from_utf8_lossy(&said).into_owned() })
        }
    }
}

/// The absolute path of `program`, resolved against the harness's working directory once, when
/// the run starts; never looked up on PATH.
pub fn absolute(program: &Path) -> std::io::Result<PathBuf> {
    std::path::absolute(program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn scratch(name: &str) -> Scratch {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("proof-load-tool-{}-{}-{}", name, std::process::id(), nanos));
        Scratch::create(&root).unwrap()
    }

    /// A failure document's error is read whole; anything else is not a failure.
    ///
    /// Revert: drop the `ok == false` check → a success document that also carries an `error`
    /// object is read as a refusal.
    #[test]
    fn a_failure_document_is_read_and_nothing_else_is() {
        let failed = br#"{ "lonic_json": 1, "tool": "forklift", "ok": false,
            "error": { "code": "locked", "message": "the warehouse is locked", "next_step": "remove \"/s/wh/.forklift/lock\"" } }"#;
        let failure = failure_of(failed).unwrap();
        assert_eq!(failure.code, "locked");
        assert_eq!(failure.next_step.as_deref(), Some("remove \"/s/wh/.forklift/lock\""));

        let no_step = br#"{ "ok": false, "error": { "code": "error", "message": "no" } }"#;
        assert_eq!(failure_of(no_step).unwrap().next_step, None);

        let succeeded = br#"{ "ok": true, "data": {}, "error": { "code": "x", "message": "y" } }"#;
        assert_eq!(failure_of(succeeded), None);
        assert_eq!(failure_of(b"not json"), None);
    }

    /// A tool is never found on PATH, and never runs outside its scratch area.
    ///
    /// Revert: drop the absolute check → `env` resolves through PATH; drop the directory check →
    /// the tool runs in the harness's own directory.
    #[test]
    fn a_tool_runs_only_by_absolute_path_inside_its_scratch() {
        let scratch = scratch("refusals");
        let environment = Environment::scrubbed(&scratch, None, &BTreeMap::new()).unwrap();
        let limit = Duration::from_secs(10);

        let relative = Invocation { program: PathBuf::from("env"), arguments: vec![], directory: scratch.work(), limit };
        assert_eq!(run(&relative, &scratch, &environment).unwrap_err(), ToolError::NotAbsolute(PathBuf::from("env")));

        let outside = std::env::temp_dir();
        let program = std::env::current_exe().unwrap();
        let elsewhere = Invocation { program, arguments: vec![], directory: outside.clone(), limit };
        assert_eq!(run(&elsewhere, &scratch, &environment).unwrap_err(), ToolError::OutsideScratch(outside));
    }

    /// Once a clone moved the work tree onto a folding volume, a tool may run there, and still
    /// nowhere else on that volume, nor by a `..` out of either root.
    ///
    /// Revert: fence on the scratch root alone → the moved work tree is refused; fence on the
    /// folding volume → a sibling folder on it is admitted; admit a `..` → a path climbing out of
    /// either root is admitted.
    #[test]
    fn a_moved_work_tree_is_inside_the_fence() {
        let folding = std::env::temp_dir().join(format!("proof-load-tool-fold-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(folding.join("sibling")).unwrap();
        let scratch = scratch("moved").with_folding_volume(&folding);
        let moved = scratch.move_work().unwrap().unwrap();
        std::fs::create_dir(&moved).unwrap();
        let environment = Environment::scrubbed(&scratch, None, &BTreeMap::new()).unwrap();
        // A program that does not exist, at a path absolute on every OS: past the fence, the run
        // fails to start, never refused.
        let program = folding.join("no-such-program");
        let at = |directory: PathBuf| run(&Invocation { program: program.clone(), arguments: vec![], directory, limit: Duration::from_secs(10) }, &scratch, &environment);
        assert!(matches!(at(moved.clone()), Err(ToolError::Spawn { .. })), "{:?}", at(moved.clone()));
        assert!(matches!(at(scratch.root().to_path_buf()), Err(ToolError::Spawn { .. })));
        assert_eq!(at(folding.join("sibling")).unwrap_err(), ToolError::OutsideScratch(folding.join("sibling")));
        assert_eq!(at(folding.clone()).unwrap_err(), ToolError::OutsideScratch(folding.clone()));
        for climbing in [moved.join("..").join("sibling"), scratch.root().join("..")] {
            assert_eq!(at(climbing.clone()).unwrap_err(), ToolError::OutsideScratch(climbing));
        }
    }

    /// The run captures exit code, both streams and the working directory it was given, and stdin
    /// is closed.
    ///
    /// Revert: inherit stdin → `cat` waits for input until the limit kills it.
    #[cfg(unix)]
    #[test]
    fn a_run_captures_what_the_tool_said() {
        let scratch = scratch("capture");
        let environment = Environment::scrubbed(&scratch, None, &BTreeMap::new()).unwrap();
        let limit = Duration::from_secs(10);

        let shell = |script: &str| Invocation {
            program: PathBuf::from("/bin/sh"), arguments: vec!["-c".into(), script.into()], directory: scratch.work(), limit,
        };
        let outcome = run(&shell("pwd; echo oops >&2; cat; exit 3"), &scratch, &environment).unwrap();
        assert_eq!(outcome.exit, Some(3));
        assert!(!outcome.succeeded());
        let printed = String::from_utf8(outcome.stdout).unwrap();
        assert_eq!(std::fs::canonicalize(printed.trim()).unwrap(), std::fs::canonicalize(scratch.work()).unwrap());
        assert_eq!(outcome.stderr, b"oops\n");
        assert_eq!(outcome.failure, None);

        let refusal = r#"printf '{"ok":false,"error":{"code":"c","message":"m","next_step":"run `forklift heal`"}}'; exit 2"#;
        let outcome = run(&shell(refusal), &scratch, &environment).unwrap();
        assert_eq!(outcome.remedy_texts(), vec!["m", "run `forklift heal`"]);
    }

    /// A tool that runs past its limit is killed and reported, never waited on forever.
    ///
    /// Revert: drop the limit check → the test hangs on `sleep 30`.
    #[cfg(unix)]
    #[test]
    fn a_tool_past_its_limit_is_killed() {
        let scratch = scratch("limit");
        let environment = Environment::scrubbed(&scratch, None, &BTreeMap::new()).unwrap();
        let limit = Duration::from_millis(200);
        let slow = Invocation { program: PathBuf::from("/bin/sleep"), arguments: vec!["30".into()], directory: scratch.work(), limit };
        let started = Instant::now();
        assert_eq!(run(&slow, &scratch, &environment).unwrap_err(), ToolError::TimedOut { program: PathBuf::from("/bin/sleep"), limit });
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// A server is stopped as its operator stops it: asked, so its own shutdown runs (here, the
    /// trap that writes `stopped`), and waited on. One that ignores the request is an error once
    /// the limit passes, never reported stopped.
    ///
    /// Revert: send SIGKILL → the trap never runs; return Ok at the deadline → the ignoring child
    /// reads as stopped.
    #[cfg(unix)]
    #[test]
    fn a_server_is_asked_to_stop_and_an_ignoring_one_is_an_error() {
        let scratch = scratch("stop");
        let marker = scratch.root().join("stopped");
        let spawn = |script: String| Served {
            child: Command::new("/bin/sh").arg("-c").arg(script).stdin(Stdio::null()).spawn().unwrap(),
            url: String::new(),
            output: Vec::new(),
        };
        let polite = spawn(format!("trap 'touch \"{}\"; exit 0' TERM; while :; do sleep 0.05; done", marker.display()));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(polite.stop(Duration::from_secs(10)), Ok(()));
        assert!(marker.exists(), "the server's own shutdown ran");

        let stubborn = spawn("trap '' TERM; while :; do sleep 0.05; done".to_string());
        std::thread::sleep(Duration::from_millis(200));
        let error = stubborn.stop(Duration::from_millis(300)).unwrap_err();
        assert!(error.contains("had not stopped"), "{}", error);
    }

    /// The wall time `timed` reports is the child's, not a poll's: a child that exits at once
    /// takes well under the 5 ms `run` sleeps between polls, so a reap through a poll would read
    /// every run as at least that long. The median of ten runs, so one slow spawn on a loaded
    /// machine does not decide it.
    #[cfg(unix)]
    #[test]
    fn wall_time_is_not_quantised_by_a_poll() {
        let scratch = scratch("timed-wall");
        let environment = Environment::scrubbed(&scratch, None, &BTreeMap::new()).unwrap();
        let invocation = Invocation { program: PathBuf::from("/usr/bin/true"), arguments: vec![], directory: scratch.root().to_path_buf(), limit: Duration::from_secs(10) };
        let mut walls: Vec<Duration> = (0..10).map(|_| {
            let timed = timed(&invocation, &scratch, &environment).unwrap();
            assert_eq!(timed.outcome.exit, Some(0));
            timed.wall
        }).collect();
        walls.sort();
        assert!(walls[5] < Duration::from_millis(5), "median wall time {:?} of {:?}", walls[5], walls);
        #[allow(clippy::disallowed_methods)] // a test's own temporary folder
        let _ = std::fs::remove_dir_all(scratch.root());
    }

    /// The peak memory `timed` reports is in bytes on every unix: a child that fills a 64 MiB
    /// buffer reports at least that and less than four times it, whichever unit its platform's
    /// rusage counts in.
    #[cfg(unix)]
    #[test]
    fn peak_memory_is_in_bytes() {
        let scratch = scratch("timed-peak");
        let environment = Environment::scrubbed(&scratch, None, &BTreeMap::new()).unwrap();
        let arguments = ["if=/dev/zero", "of=/dev/null", "bs=67108864", "count=1"].map(String::from).to_vec();
        let invocation = Invocation { program: PathBuf::from("/bin/dd"), arguments, directory: scratch.root().to_path_buf(), limit: Duration::from_secs(30) };
        let timed = timed(&invocation, &scratch, &environment).unwrap();
        assert_eq!(timed.outcome.exit, Some(0), "{}", String::from_utf8_lossy(&timed.outcome.stderr));
        let peak = timed.usage.expect("unix reads rusage").peak_bytes;
        assert!((64 << 20..256 << 20).contains(&peak), "peak {} bytes", peak);
        #[allow(clippy::disallowed_methods)] // a test's own temporary folder
        let _ = std::fs::remove_dir_all(scratch.root());
    }

    /// `timed` still enforces the invocation's limit, and a child it killed is a timeout, not an
    /// exit; it gives what the child said and how it exited, as `run` does.
    #[cfg(unix)]
    #[test]
    fn timed_kills_at_the_limit_and_keeps_the_exit() {
        let scratch = scratch("timed-limit");
        let environment = Environment::scrubbed(&scratch, None, &BTreeMap::new()).unwrap();
        let shell = |script: &str, limit: Duration| Invocation {
            program: PathBuf::from("/bin/sh"), arguments: vec!["-c".into(), script.into()], directory: scratch.root().to_path_buf(), limit,
        };
        let started = Instant::now();
        assert!(matches!(timed(&shell("sleep 30", Duration::from_millis(300)), &scratch, &environment), Err(ToolError::TimedOut { .. })));
        assert!(started.elapsed() < Duration::from_secs(10));
        let exited = timed(&shell("echo out; echo err >&2; exit 3", Duration::from_secs(10)), &scratch, &environment).unwrap();
        assert_eq!((exited.outcome.exit, exited.outcome.stdout.as_slice(), exited.outcome.stderr.as_slice()), (Some(3), b"out\n".as_slice(), b"err\n".as_slice()));
        #[allow(clippy::disallowed_methods)] // a test's own temporary folder
        let _ = std::fs::remove_dir_all(scratch.root());
    }

    /// A KiB count is multiplied out and a byte count is kept.
    #[test]
    fn a_peak_is_normalised_to_bytes_by_its_unit() {
        assert_eq!(peak_bytes_when(true, 65_536), 64 << 20);
        assert_eq!(peak_bytes_when(false, 65_536), 65_536);
        assert_eq!(peak_bytes_when(true, -1), 0);
    }
}
