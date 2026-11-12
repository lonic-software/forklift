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
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolError::NotAbsolute(path) => write!(f, "the tool {} is not an absolute path; tools are found by path, never on PATH", path.display()),
            ToolError::OutsideScratch(path) => write!(f, "the working directory {} is outside the run's scratch area", path.display()),
            ToolError::Spawn { program, reason } => write!(f, "could not start {}: {}", program.display(), reason),
            ToolError::TimedOut { program, limit } => write!(f, "{} ran past its {} s limit and was killed", program.display(), limit.as_secs()),
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
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Failure {
    pub code: String,
    pub message: String,
    pub next_step: Option<String>,
}

/// What a run produced.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
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

fn drain(mut source: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = source.read_to_end(&mut bytes);
        bytes
    })
}

/// Runs `invocation` in `scratch` under `environment` and captures what it said. Its stdin is
/// closed, so a tool that would prompt sees end of input instead of waiting.
pub fn run(invocation: &Invocation, scratch: &Scratch, environment: &Environment) -> Result<Outcome, ToolError> {
    if !invocation.program.is_absolute() {
        return Err(ToolError::NotAbsolute(invocation.program.clone()));
    }
    if !invocation.directory.starts_with(scratch.root()) {
        return Err(ToolError::OutsideScratch(invocation.directory.clone()));
    }

    let mut command = Command::new(&invocation.program);
    command.args(&invocation.arguments).current_dir(&invocation.directory)
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    environment.apply(&mut command);

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
}
