//! Building a scenario's history with a real tool, and walking what it left behind.
//!
//! The build runs in a fresh scratch area under the scrubbed environment: the corpus writes each
//! record into the work tree, and the scenario's operations run through their binding. The walk
//! that follows lists every path in the warehouse's state folder; a path no census row matches
//! fails the run, and the census names some path matched are the ones this scenario creates.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::census::{self, Census};
use crate::corpus::{self, Corpus};
use crate::scenario::{bind, Mark, Op, Scenario, Tool, URL, WORK};
use crate::scrub::{Environment, Scratch};
use crate::tool::{self, Invocation, Outcome, Served};

/// What building a scenario's history produced.
#[derive(serde::Serialize)]
pub struct Built {
    /// Every command the build ran, in order, with what it said.
    pub outcomes: Vec<Outcome>,
    /// The paths under the state folder after the build, as `census::walk` lists them.
    pub walked: Vec<String>,
    /// Marks acknowledged: each record or variant whose binding exited 0 throughout.
    pub acknowledged: Vec<Mark>,
    /// The server a `Host` operation started, serving until this is dropped.
    #[serde(skip)]
    pub host: Option<Served>,
}

impl std::fmt::Debug for Built {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Built").field("outcomes", &self.outcomes).field("walked", &self.walked)
            .field("acknowledged", &self.acknowledged).field("host", &self.host.as_ref().map(|host| &host.url)).finish()
    }
}

#[derive(Debug)]
pub enum RunError {
    Tool(tool::ToolError),
    Io(String),
    /// A build step the history depends on failed: the history the scenario names does not exist.
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

/// The state folder a tool keeps beside its work tree.
fn state_folder(tool: Tool) -> &'static str {
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

/// The host warehouse a `Host` operation serves, beside the work tree in the scratch area.
pub fn host_root(scratch: &Scratch) -> PathBuf {
    scratch.root().join("host")
}

/// Builds `scenario`'s history with `program` (the binary of `tool`) in `scratch`'s work tree,
/// with `server` the forklift-server a `Host` operation starts. Every build step must succeed: a
/// failed one means the scenario's precondition does not hold, which is reported, never graded.
#[allow(clippy::too_many_arguments)]
pub fn build(scenario: &Scenario, corpus: &Corpus, tool: Tool, program: &Path, server: Option<&Path>, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<Built, RunError> {
    let work = scratch.work();
    let run_in = |program: &Path, arguments: Vec<String>, directory: PathBuf| -> Result<Outcome, RunError> {
        let invocation = Invocation { program: PathBuf::from(program), arguments, directory, limit };
        let outcome = tool::run(&invocation, scratch, environment).map_err(RunError::Tool)?;
        if !outcome.succeeded() {
            return Err(RunError::BuildFailed {
                arguments: outcome.arguments.clone(), exit: outcome.exit,
                stderr: String::from_utf8_lossy(&outcome.stderr).into_owned(),
                stdout: String::from_utf8_lossy(&outcome.stdout).into_owned(),
            });
        }
        Ok(outcome)
    };

    let mut outcomes = vec![run_in(program, prepare(tool), work.clone())?];
    let mut acknowledged = Vec::new();
    let mut host: Option<Served> = None;
    for op in &scenario.history {
        if let Some(snapshot) = op.writes(corpus) {
            let snapshot = snapshot.map_err(|reason| RunError::Io(format!("scenario {}: {}", scenario.id, reason)))?;
            corpus::write(&snapshot, &work).map_err(|error| RunError::Io(error.to_string()))?;
        }
        let mut directory = work.clone();
        match op {
            Op::Host => {
                let server = server.ok_or_else(|| RunError::Io(format!("scenario {} hosts a warehouse, and no forklift-server binary was given", scenario.id)))?;
                let root = host_root(scratch);
                outcomes.push(run_in(server, vec!["prepare".into(), "--root".into(), root.display().to_string()], scratch.root().to_path_buf())?);
                host = Some(tool::serve(server, &root, scratch, environment, limit).map_err(RunError::Tool)?);
            }
            Op::Bundle => {
                let server = server.filter(|_| host.is_some())
                    .ok_or_else(|| RunError::Io(format!("scenario {} bundles before it hosts a warehouse", scenario.id)))?;
                let root = host_root(scratch);
                outcomes.push(run_in(server, vec!["bundle".into(), "--root".into(), root.display().to_string()], scratch.root().to_path_buf())?);
            }
            Op::Clone { .. } => {
                // A clone makes its target; the work tree it replaces is the harness's own scratch.
                #[allow(clippy::disallowed_methods)] // a scratch work tree the harness made; no retry is wanted
                std::fs::remove_dir_all(&work).map_err(|error| RunError::Io(error.to_string()))?;
                directory = scratch.root().to_path_buf();
            }
            _ => {}
        }
        let lines = bind(tool, op).ok_or_else(|| RunError::Io(format!("{:?} has no binding for {:?}", tool, op)))?;
        for arguments in lines {
            let arguments = fill(arguments, host.as_ref().map(|host| host.url.as_str()), &work)
                .map_err(|placeholder| RunError::Io(format!("scenario {}: {:?} needs {} before a Host", scenario.id, op, placeholder)))?;
            outcomes.push(run_in(program, arguments, directory.clone())?);
        }
        if let Some(mark) = op.mark() {
            acknowledged.push(mark);
        }
    }

    let walked = census::walk(&work.join(state_folder(tool))).map_err(|error| RunError::Io(error.to_string()))?;
    Ok(Built { outcomes, walked, acknowledged, host })
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

/// The census names some walked path matches: the names this build creates.
pub fn created_names(census: &Census, walked: &[String]) -> Vec<String> {
    census.damage_names().into_iter().map(|name| name.path)
        .filter(|pattern| walked.iter().any(|path| census::matches(pattern, path)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
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
        };

        let built = build(&scenario, &corpus, Tool::Forklift, &program, None, &scratch, &environment, Duration::from_secs(60)).unwrap();
        assert_eq!(built.acknowledged, vec![Mark::Record(0), Mark::Record(1), Mark::Record(2)]);

        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let census = Census::load(&source).unwrap().expect("this tree has a census");
        assert_eq!(census.unclassified(&built.walked), Vec::<&str>::new());
        let created = created_names(&census, &built.walked);
        for name in ["objects/<xx>/<hash>", "pallets/<name>", "{bay}inventory/metadata", "warehouse"] {
            assert!(created.iter().any(|created| created == name), "{} is created: {:?}", name, created);
        }
        assert_eq!(crate::oracle::read_tree(&scratch.work()).unwrap(), corpus::expected(&corpus.records[2]));
    }
}
