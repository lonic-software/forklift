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
use crate::oracle::{self, Admissible, RedactionLedger, RedactionState};
use crate::scenario::{bind, Mark, Op, Scenario, Tool, TARGET, URL, WORK};
use crate::scrub::{Environment, Scratch};
use crate::tool::{self, Invocation, Outcome, Served};

/// What building a scenario's history produced.
#[derive(serde::Serialize)]
pub struct Built {
    /// Every command the build ran, in order, with what it said.
    pub outcomes: Vec<Outcome>,
    /// The paths under the state folder after the build, as `census::walk` lists them.
    pub walked: Vec<String>,
    /// The same of the host's state folder, when the history hosts a warehouse.
    pub host_walked: Vec<String>,
    /// Content the history took away from records it had acknowledged.
    pub taken: Vec<Taken>,
    /// Marks acknowledged: each record or variant whose binding exited 0 throughout.
    pub acknowledged: Vec<Mark>,
    /// The server a `Host` operation started, serving until this is dropped.
    #[serde(skip)]
    pub host: Option<Served>,
}

impl std::fmt::Debug for Built {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Built").field("outcomes", &self.outcomes).field("walked", &self.walked)
            .field("acknowledged", &self.acknowledged).field("host_walked", &self.host_walked).field("taken", &self.taken).field("host", &self.host.as_ref().map(|host| &host.url)).finish()
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
    let corpus = &scenario.corpus(corpus);
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
    let mut taken = Vec::new();
    let mut ledger = RedactionLedger::default();
    // The folder a sparse clone scoped the work tree to, which bounds what a later write may hold.
    let mut scope: Option<&str> = None;
    for op in &scenario.history {
        if let Some(snapshot) = op.writes(corpus) {
            let snapshot = snapshot.map_err(|reason| RunError::Io(format!("scenario {}: {}", scenario.id, reason)))?;
            let snapshot = match scope { Some(scope) => corpus::within(&snapshot, scope), None => snapshot };
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
                    "--statement".into(), crate::scenario::TAKEN_BECAUSE.into(), hash,
                ], scratch.root().to_path_buf())?);
                taken.push(Taken { path, digest: oracle::digest(&bytes), by: TakenBy::Withheld });
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
                let digest = oracle::digest(&bytes);
                let admissible = ledger.admissible("client", &hash, &digest, "content_redacted");
                taken.push(Taken { path, digest, by: TakenBy::Redacted { hash, admissible } });
                // Its lines ran above, with the target filled.
                continue;
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
        if let Op::Clone { kind } = op {
            scope = match kind { crate::scenario::CloneKind::Only(scope) => Some(scope.as_str()), _ => None };
        }
        if let Some(mark) = op.mark() {
            acknowledged.push(mark);
        }
    }

    let walked = census::walk(&work.join(state_folder(tool))).map_err(|error| RunError::Io(error.to_string()))?;
    let host_walked = match host {
        Some(_) => census::walk(&host_root(scratch).join(".forklift")).map_err(|error| RunError::Io(error.to_string()))?,
        None => Vec::new(),
    };
    Ok(Built { outcomes, walked, host_walked, taken, acknowledged, host })
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
