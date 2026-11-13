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
use crate::oracle::{self, Admissible, RedactionLedger, RedactionState};
use crate::scenario::{bind, Keep, Mark, Op, Scenario, Tool, Until, SCRATCH, TARGET, URL, WORK};
use crate::scrub::{Environment, Scratch};
use crate::tool::{self, Invocation, Outcome, Served};
use crate::writeops::Observer;

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
/// With an `observer`, every step goes through it, for the write-op check.
#[allow(clippy::too_many_arguments)]
pub fn build(scenario: &Scenario, corpus: &Corpus, tool: Tool, program: &Path, server: Option<&Path>, scratch: &Scratch, environment: &Environment, limit: Duration, observer: Option<&Observer>) -> Result<Built, RunError> {
    let work = scratch.work();
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

    let mut outcomes = vec![run_in(program, prepare(tool), work.clone())?];
    let mut acknowledged = Vec::new();
    let mut host: Option<Served> = None;
    let mut taken = Vec::new();
    let mut ledger = RedactionLedger::default();
    // The folder a sparse clone scoped the work tree to, which bounds what a later write may hold.
    let mut scope: Option<&str> = None;
    // Values coverage lines kept from earlier documents, by name.
    let mut kept: BTreeMap<String, String> = BTreeMap::new();
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
                let arguments = filled(line, &kept, host.as_ref().map(|host| host.url.as_str()), scratch)
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
                host.take().ok_or_else(|| RunError::Io(format!("scenario {} stops a host it does not serve", scenario.id)))?;
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
                let Some(strace) = observer.and_then(|observer| observer.strace) else { continue };
                let directory = scratch.root().join(at);
                let arguments: Vec<String> = std::iter::once("--json".to_string())
                    .chain(filled(line, &kept, host.as_ref().map(|host| host.url.as_str()), scratch).map_err(|reason| RunError::Io(format!("scenario {}: {}", scenario.id, reason)))?)
                    .collect();
                let mut stood = false;
                for when in 1..=INJECT_ATTEMPTS {
                    let mut traced = vec!["-f".to_string(), "-qq".into(), "-e".into(), "trace=fsync".into(), "-e".into(), format!("inject=fsync:error=EIO:when={}", when), "--".into(), program.display().to_string()];
                    traced.extend(arguments.iter().cloned());
                    // An attempt is meant to fail; what it left is what `until` judges.
                    tool::run(&Invocation { program: strace.to_path_buf(), arguments: traced, directory: directory.clone(), limit }, scratch, environment).map_err(RunError::Tool)?;
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

/// How many fsyncs an `Inject` fails in turn, one per attempt, before it gives up on its state.
const INJECT_ATTEMPTS: usize = 32;

/// Fills a coverage line's placeholders, anywhere in an argument: kept values by `{name}`, the
/// host's URL, the scratch root and the work tree. One left unfilled is an error naming it.
pub fn filled(line: &[String], kept: &BTreeMap<String, String>, url: Option<&str>, scratch: &Scratch) -> Result<Vec<String>, String> {
    let mut values: Vec<(String, String)> = kept.iter().map(|(name, value)| (format!("{{{}}}", name), value.clone())).collect();
    values.push((SCRATCH.to_string(), scratch.root().display().to_string()));
    values.push((WORK.to_string(), scratch.work().display().to_string()));
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
            std::fs::File::open(&path)?.set_modified(then)?;
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

        let built = build(&scenario, &corpus, Tool::Forklift, &program, None, &scratch, &environment, Duration::from_secs(60), None).unwrap();
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
