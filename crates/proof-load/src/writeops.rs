//! Write-op completeness: every command the version under test has is either seen writing to a
//! warehouse in some scenario's trace, or exempt by name with a reason.
//!
//! The universe comes from the binaries themselves, never from a list kept here: forklift's command
//! ids from its schema command (`self schema`, or `self-schema` in older versions), and
//! forklift-server's leaf subcommands from its `--help`, nested subcommands walked. A command id is
//! covered when a traced build step with that id made a successful mutating call on a warehouse
//! other than on a lock file: anywhere in the scratch area but the home and temp folders the
//! scrubbed environment points at. A step that ran and made none is "run, no write reached", which
//! covers nothing. A command read-only by design is exempt by name, and every run of it is checked
//! by a digest of the tree it ran in, before and after, to have changed nothing.

use std::cell::{Cell as Counter, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::host::Host;
use crate::proxy::Exchange;
use crate::report::{Cell, Completeness};
use crate::routewrites::Serving;
use crate::scrub::{Environment, Scratch};
use crate::tool::{self, Invocation, Outcome, ToolError};
use crate::trace::{self, Root};

/// One command of the schema: its id and the literal words of its command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub id: String,
    pub words: Vec<String>,
}

/// The commands a forklift binary says it has.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schema {
    pub commands: Vec<Command>,
}

/// The literal words a schema `run` template opens with: those before its first placeholder or
/// `--`.
pub fn words(run: &str) -> Vec<String> {
    run.split_whitespace().take_while(|word| !word.starts_with('{') && *word != "--").map(str::to_string).collect()
}

impl Schema {
    /// Reads a schema document: `{"commands": [{"id", "run"}, …]}`. A command with no id or no
    /// literal word refuses the schema, since it could never be told apart from another.
    pub fn parse(document: &[u8]) -> Result<Schema, String> {
        let document: serde_json::Value = serde_json::from_slice(document).map_err(|error| format!("the schema is not JSON: {}", error))?;
        let listed = document["commands"].as_array().ok_or("the schema lists no commands")?;
        let mut commands = Vec::new();
        for command in listed {
            let id = command["id"].as_str().ok_or_else(|| format!("a schema command has no id: {}", command))?;
            let run = command["run"].as_str().ok_or_else(|| format!("schema command {} has no run template", id))?;
            let words = words(run);
            if words.is_empty() {
                return Err(format!("schema command {}'s run template opens with no literal word: {:?}", id, run));
            }
            commands.push(Command { id: id.to_string(), words });
        }
        if commands.is_empty() {
            return Err("the schema lists no commands".to_string());
        }
        Ok(Schema { commands })
    }

    /// The id of the command a forklift line runs: the command whose words are the longest prefix
    /// of the line's arguments before any `--` that are not options. None when no command's words
    /// are.
    pub fn id_of(&self, arguments: &[String]) -> Option<String> {
        let positional: Vec<&str> = arguments.iter().map(String::as_str).take_while(|argument| *argument != "--")
            .filter(|argument| !argument.starts_with('-')).collect();
        self.commands.iter()
            .filter(|command| command.words.len() <= positional.len() && command.words.iter().zip(&positional).all(|(word, given)| word == given))
            .max_by_key(|command| command.words.len())
            .map(|command| command.id.clone())
    }
}

/// The two spellings of the schema command, newest first.
const SCHEMA_SPELLINGS: &[&[&str]] = &[&["self", "schema"], &["self-schema"]];

/// Asks `program` for its schema, under either spelling. Without one the harness refuses to run:
/// it keeps no list of commands of its own to fall back on.
pub fn read_schema(program: &Path, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<Schema, String> {
    let mut said = Vec::new();
    for spelling in SCHEMA_SPELLINGS {
        let invocation = Invocation { program: program.to_path_buf(), arguments: spelling.iter().map(|word| word.to_string()).collect(), directory: scratch.root().to_path_buf(), limit };
        let outcome = tool::run(&invocation, scratch, environment).map_err(|error| error.to_string())?;
        if outcome.succeeded() {
            return Schema::parse(&outcome.stdout).map_err(|error| format!("`{}`: {}", spelling.join(" "), error));
        }
        said.push(format!("`{}` exited {:?}", spelling.join(" "), outcome.exit));
    }
    Err(format!("{} has no schema command ({}): the write-op universe cannot be read", program.display(), said.join(", ")))
}

/// The subcommand names a clap `--help` lists under "Commands:", `help` left out.
pub fn listed_commands(help: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut inside = false;
    for line in help.lines() {
        if line.trim_end() == "Commands:" {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        if !line.starts_with("  ") {
            break;
        }
        // A description wrapped onto the next line is indented past the names.
        if line[2..].starts_with(' ') {
            continue;
        }
        if let Some(name) = line.split_whitespace().next() {
            if name != "help" {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// Every leaf subcommand under `prefix`, by walking `help`, which gives the `--help` of a prefix: a
/// subcommand that lists subcommands of its own is walked, one that lists none is a leaf.
pub fn leaves(prefix: &[String], help: &mut dyn FnMut(&[String]) -> Result<String, String>) -> Result<Vec<Vec<String>>, String> {
    let listed = listed_commands(&help(prefix)?);
    if listed.is_empty() {
        return Ok(if prefix.is_empty() { vec![] } else { vec![prefix.to_vec()] });
    }
    let mut found = Vec::new();
    for name in listed {
        let mut path = prefix.to_vec();
        path.push(name);
        found.extend(leaves(&path, help)?);
    }
    Ok(found)
}

/// The id a server leaf is reported under, apart from a client command of the same name.
pub fn server_id(leaf: &[String]) -> String {
    format!("forklift-server {}", leaf.join(" "))
}

/// forklift-server's leaf subcommands, from its own `--help`.
pub fn read_server_leaves(program: &Path, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<Vec<Vec<String>>, String> {
    let mut help = |prefix: &[String]| -> Result<String, String> {
        let mut arguments = prefix.to_vec();
        arguments.push("--help".to_string());
        let invocation = Invocation { program: program.to_path_buf(), arguments, directory: scratch.root().to_path_buf(), limit };
        let outcome = tool::run(&invocation, scratch, environment).map_err(|error| error.to_string())?;
        if !outcome.succeeded() {
            return Err(format!("`{} {} --help` exited {:?}", program.display(), prefix.join(" "), outcome.exit));
        }
        Ok(String::from_utf8_lossy(&outcome.stdout).into_owned())
    };
    let found = leaves(&[], &mut help)?;
    if found.is_empty() {
        return Err(format!("{} lists no subcommands", program.display()));
    }
    Ok(found)
}

/// The server leaf a server line runs: the longest leaf whose words open its arguments.
pub fn leaf_of(leaves: &[Vec<String>], arguments: &[String]) -> Option<String> {
    leaves.iter().filter(|leaf| leaf.len() <= arguments.len() && leaf.iter().zip(arguments).all(|(word, given)| word == given))
        .max_by_key(|leaf| leaf.len()).map(|leaf| server_id(leaf))
}

/// Whether a path is a lock file: named `lock`, or ending `.lock`. Taking and releasing a lock is
/// no write to a warehouse.
pub fn is_lock(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name == "lock" || name.ends_with(".lock")
}

/// What one traced or digested build step showed.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Evidence {
    pub scenario: String,
    pub id: String,
    /// The step ran under strace, so `writes` is what it did.
    pub traced: bool,
    /// The warehouse paths its successful mutating calls named, root-relative, locks left out.
    pub writes: Vec<String>,
    /// For a step whose id is exempt as read-only: the paths under the tree it ran in that its run
    /// changed, created or removed.
    pub changed: Option<Vec<String>>,
}

/// A tree's content: every path, root-relative, with what it holds. A lock file is left out.
pub fn digest(root: &Path) -> std::io::Result<BTreeMap<String, String>> {
    let mut held = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let metadata = std::fs::symlink_metadata(&path)?;
        let relative = path.strip_prefix(root).unwrap_or(&path).components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
        if is_lock(&relative) {
            continue;
        }
        let what = if metadata.is_dir() {
            for entry in std::fs::read_dir(&path)? {
                stack.push(entry?.path());
            }
            "folder".to_string()
        } else if metadata.file_type().is_symlink() {
            format!("link {}", std::fs::read_link(&path)?.to_string_lossy())
        } else {
            format!("file {}", crate::oracle::digest(&std::fs::read(&path)?))
        };
        held.insert(relative, what);
    }
    Ok(held)
}

/// The paths two digests disagree on.
pub fn changed(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> Vec<String> {
    let paths: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    paths.into_iter().filter(|path| before.get(*path) != after.get(*path)).cloned().collect()
}

/// What a build records about its steps for the write-op check.
pub struct Observer<'a> {
    /// strace, by absolute path, when steps are traced.
    pub strace: Option<&'a Path>,
    /// Where traces go while they are read: outside every scratch area.
    pub traces: &'a Path,
    pub schema: &'a Schema,
    pub leaves: &'a [Vec<String>],
    /// The ids digested around every run: those exempt as read-only.
    pub read_only: &'a BTreeSet<String>,
    /// The scenario being built.
    pub scenario: String,
    next: Counter<usize>,
    seen: RefCell<Vec<Evidence>>,
    servings: RefCell<Vec<Serving>>,
    untraced: RefCell<Vec<String>>,
}

impl<'a> Observer<'a> {
    pub fn new(strace: Option<&'a Path>, traces: &'a Path, schema: &'a Schema, leaves: &'a [Vec<String>], read_only: &'a BTreeSet<String>, scenario: &str) -> Observer<'a> {
        Observer { strace, traces, schema, leaves, read_only, scenario: scenario.to_string(), next: Counter::new(0), seen: RefCell::new(Vec::new()),
            servings: RefCell::new(Vec::new()), untraced: RefCell::new(Vec::new()) }
    }

    /// Every traced serving finished so far.
    pub fn servings(&self) -> Vec<Serving> {
        self.servings.borrow().clone()
    }

    /// Why each host that should have been traced was not: strace's own words.
    pub fn untraced(&self) -> Vec<String> {
        self.untraced.borrow().clone()
    }

    /// Serves the warehouse at `root` with `program`, traced when a tracer is given. When strace
    /// cannot attach, the host serves untraced and what strace said is kept for the report.
    pub fn host(&self, program: &Path, root: &Path, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<Host, ToolError> {
        let Some(strace) = self.strace else { return Host::start(program, root, scratch, environment, limit) };
        let base = self.trace_path("serve").with_extension("");
        let (host, failed) = Host::start_traced(program, root, scratch, environment, limit, strace, &base)?;
        if let Some(failed) = failed {
            self.untraced.borrow_mut().push(format!("{}: strace could not attach to the server: {}", self.scenario, failed));
        }
        Ok(host)
    }

    /// Kills `host`, serving the warehouse at `root`, and keeps what its traces show: the serving,
    /// for the server route write check, and the serve's evidence, for the write-op check. What
    /// its proxy logged is returned.
    pub fn finish_host(&self, host: Host, root: &Path, scratch: &Scratch) -> Result<Vec<Exchange>, ToolError> {
        let traced = host.is_traced();
        let (log, traces) = host.finish();
        if !traced {
            return Ok(log);
        }
        let spawn = |reason: String| ToolError::Spawn { program: root.to_path_buf(), reason };
        let start = scratch.root().to_string_lossy().into_owned();
        let mut writes = Vec::new();
        for text in traces {
            let text = text.map_err(|reason| spawn(format!("the server's trace: {}", reason)))?;
            let calls = trace::parse(&text).map_err(|reason| spawn(format!("the server's trace: {}", reason)))?;
            writes.extend(crate::kill::writes(&calls, &start));
        }
        let scratch_root = Root::of(scratch.root()).map_err(|error| spawn(error.to_string()))?;
        let paths: Vec<String> = writes.iter().map(|write| write.path.clone()).collect();
        self.seen.borrow_mut().push(Evidence { scenario: self.scenario.clone(), id: "forklift-server serve".to_string(), traced: true, writes: warehouse_writes(&paths, &scratch_root), changed: None });
        self.servings.borrow_mut().push(Serving { scenario: self.scenario.clone(), root: root.to_string_lossy().into_owned(), exchanges: log.clone(), writes });
        Ok(log)
    }

    /// A fresh trace file's path for this scenario, outside every scratch area.
    pub fn trace_path(&self, kind: &str) -> PathBuf {
        let at = self.next.get();
        self.next.set(at + 1);
        self.traces.join(format!("{}-{}-{}.trace", self.scenario, kind, at))
    }

    /// What the steps observed so far showed.
    pub fn evidence(&self) -> Vec<Evidence> {
        self.seen.borrow().clone()
    }

    /// Runs one build step, traced when a tracer is given and digested when its id is read-only.
    /// `server` says whether `program` is forklift-server; `tree` is the tree a read-only run
    /// must leave unchanged.
    pub fn run(&self, invocation: &Invocation, server: bool, tree: &Path, scratch: &Scratch, environment: &Environment) -> Result<Outcome, ToolError> {
        let id = if server { leaf_of(self.leaves, &invocation.arguments) } else { self.schema.id_of(&invocation.arguments) };
        // A line the universe has no name for would go unrecorded: refuse it rather than guess.
        let Some(id) = id else {
            return Err(ToolError::Spawn { program: invocation.program.clone(), reason: format!("no command of the version under test is `{}`", invocation.arguments.join(" ")) });
        };
        let digested = self.read_only.contains(&id);
        let before = if digested { Some(digest(tree).map_err(|error| ToolError::Spawn { program: tree.to_path_buf(), reason: error.to_string() })?) } else { None };
        let mut traced_at = None;
        let mut outcome = match self.strace {
            Some(strace) => {
                let path = self.trace_path("cover");
                let line = crate::kill::strace_line(&path, &[], &invocation.program, &invocation.arguments);
                let traced = Invocation { program: strace.to_path_buf(), arguments: line, directory: invocation.directory.clone(), limit: invocation.limit };
                traced_at = Some(path);
                tool::run(&traced, scratch, environment)?
            }
            None => tool::run(invocation, scratch, environment)?,
        };
        outcome.arguments = invocation.arguments.clone();
        let changed = match before {
            Some(before) => {
                let after = digest(tree).map_err(|error| ToolError::Spawn { program: tree.to_path_buf(), reason: error.to_string() })?;
                Some(changed(&before, &after))
            }
            None => None,
        };
        let mut writes = Vec::new();
        if let Some(path) = &traced_at {
            let text = std::fs::read_to_string(path).map_err(|error| ToolError::Spawn { program: path.clone(), reason: error.to_string() })?;
            #[allow(clippy::disallowed_methods, reason = "the harness's own trace, read in full above")]
            let _ = std::fs::remove_file(path);
            let calls = trace::parse(&text).map_err(|reason| ToolError::Spawn { program: path.clone(), reason })?;
            let root = Root::of(scratch.root()).map_err(|error| ToolError::Spawn { program: scratch.root().to_path_buf(), reason: error.to_string() })?;
            writes = warehouse_writes(&crate::kill::written(&calls, &invocation.directory.to_string_lossy()), &root);
        }
        // A failed step proves nothing either way; the build refuses it anyway.
        if outcome.succeeded() {
            self.seen.borrow_mut().push(Evidence { scenario: self.scenario.clone(), id, traced: traced_at.is_some(), writes, changed });
        }
        Ok(outcome)
    }
}

/// The paths of `written` that lie on a warehouse: under the scratch root, outside its home and
/// temp folders, and not a lock file. Root-relative, sorted, once each.
pub fn warehouse_writes(written: &[String], root: &Root) -> Vec<String> {
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for path in written {
        if !root.holds(path) {
            continue;
        }
        let relative = root.tokenized(path).trim_start_matches("<root>").trim_start_matches('/').to_string();
        let first = relative.split('/').next().unwrap_or_default();
        if relative.is_empty() || first == "home" || first == "tmp" || is_lock(&relative) {
            continue;
        }
        paths.insert(relative);
    }
    paths.into_iter().collect()
}

/// Why a command is exempt from the write-op check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exemption {
    /// It changes no warehouse by design. Some scenario must run it, and every run is digested.
    ReadOnly(&'static str),
    /// No corpus can run it meaningfully.
    Unrunnable(&'static str),
}

/// The write-op check's two entries: coverage, which needs traces, and the read-only digests,
/// which do not. `universe` is every command id and server leaf; `traced` says whether the
/// evidence comes from traced builds; `not_traced` is why not, when it does not.
pub fn check(universe: &[String], evidence: &[Evidence], exemptions: &[(&str, Exemption)], not_traced: Option<&str>) -> Vec<Completeness> {
    let mut coverage = Vec::new();
    let mut read_only = Vec::new();
    let exempt: BTreeMap<&str, Exemption> = exemptions.iter().copied().collect();
    for (name, _) in exemptions {
        if !universe.iter().any(|id| id == name) {
            coverage.push(format!("{}: exempt, but the version under test has no such command", name));
        }
    }
    for id in universe {
        let runs: Vec<&Evidence> = evidence.iter().filter(|seen| &seen.id == id).collect();
        match exempt.get(id.as_str()) {
            Some(Exemption::Unrunnable(_)) => {}
            Some(Exemption::ReadOnly(_)) => {
                if runs.is_empty() {
                    read_only.push(format!("{}: exempt as read-only, and no scenario runs it", id));
                }
                for seen in runs {
                    match &seen.changed {
                        Some(changed) if !changed.is_empty() => read_only.push(format!("{}: exempt as read-only, and in {} it changed {}", id, seen.scenario, changed.join(" "))),
                        Some(_) => {}
                        None => read_only.push(format!("{}: exempt as read-only, and its run in {} was not digested", id, seen.scenario)),
                    }
                }
            }
            None if not_traced.is_some() => {}
            None => {
                let traced: Vec<&&Evidence> = runs.iter().filter(|seen| seen.traced).collect();
                if traced.is_empty() {
                    coverage.push(format!("{}: no scenario runs it", id));
                } else if traced.iter().all(|seen| seen.writes.is_empty()) {
                    let scenarios: BTreeSet<&str> = traced.iter().map(|seen| seen.scenario.as_str()).collect();
                    coverage.push(format!("{}: run, no write reached (in {})", id, scenarios.into_iter().collect::<Vec<_>>().join(", ")));
                }
            }
        }
    }
    let coverage_cell = match not_traced { Some(reason) => Cell::not_run(reason), None => Cell::Run };
    vec![
        Completeness { check: "write-op coverage".to_string(), cell: coverage_cell, missing: coverage },
        Completeness { check: "read-only exemptions".to_string(), cell: Cell::Run, missing: read_only },
    ]
}

/// The universe of a run: every forklift command id, then every server leaf.
pub fn universe(schema: &Schema, leaves: &[Vec<String>]) -> Vec<String> {
    schema.commands.iter().map(|command| command.id.clone()).chain(leaves.iter().map(|leaf| server_id(leaf))).collect()
}

/// Where coverage traces go: beside the report, outside every scratch area.
pub fn traces_folder(output: &Path) -> PathBuf {
    output.join("traces")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(arguments: &[&str]) -> Vec<String> {
        arguments.iter().map(|argument| argument.to_string()).collect()
    }

    fn schema() -> Schema {
        let document = br#"{"commands": [
            {"id": "park", "run": "park"}, {"id": "park-pop", "run": "park pop"},
            {"id": "pallet-delete", "run": "pallet delete {force} {remote} -- {name}"},
            {"id": "pallet", "run": "pallet"}, {"id": "clone", "run": "clone {only} -- {url} {directory}"},
            {"id": "self-alias-status", "run": "self alias status"}
        ]}"#;
        Schema::parse(document).unwrap()
    }

    /// A line's id is the command whose literal words are the longest prefix of its non-option
    /// arguments, so a group's subcommand wins over the bare group, and options anywhere are
    /// skipped.
    ///
    /// Revert: take the first argument as the id (the S2a reading) → `park pop` reads as `park`,
    /// and `pallet delete` as `pallet`.
    #[test]
    fn a_line_maps_to_the_longest_matching_command() {
        let schema = schema();
        assert_eq!(schema.id_of(&strings(&["--json", "park", "pop"])).as_deref(), Some("park-pop"));
        assert_eq!(schema.id_of(&strings(&["--json", "park", "--", "pop"])).as_deref(), Some("park"), "an operand is no command word");
        assert_eq!(schema.id_of(&strings(&["--json", "park"])).as_deref(), Some("park"));
        assert_eq!(schema.id_of(&strings(&["--json", "pallet", "delete", "--remote", "--", "side"])).as_deref(), Some("pallet-delete"));
        assert_eq!(schema.id_of(&strings(&["--json", "pallet", "side"])).as_deref(), Some("pallet"));
        assert_eq!(schema.id_of(&strings(&["--json", "clone", "--only", "src", "http://h", "/w"])).as_deref(), Some("clone"));
        assert_eq!(schema.id_of(&strings(&["--json", "self", "alias", "status"])).as_deref(), Some("self-alias-status"));
        assert_eq!(schema.id_of(&strings(&["--json", "stack"])), None);
    }

    /// A schema with no commands, or a command whose template opens on a placeholder, refuses:
    /// either would let a line go unattributed without saying so.
    #[test]
    fn a_schema_that_cannot_attribute_a_line_refuses() {
        assert!(Schema::parse(br#"{"commands": []}"#).is_err());
        assert!(Schema::parse(br#"{"commands": [{"id": "x", "run": "{a} b"}]}"#).is_err());
        assert!(Schema::parse(br#"{"name": "forklift"}"#).is_err());
        assert_eq!(words("tag create {message} -- {name} {revision}"), strings(&["tag", "create"]));
    }

    const TOP: &str = "Forklift — the server.\n\nUsage: forklift-server <COMMAND>\n\nCommands:\n  serve      Serve a warehouse\n  tombstone  Tombstones\n  gc         Collect\n  help       Print this message or the help of the given subcommand(s)\n\nOptions:\n  -h, --help  Print help\n";
    const TOMBSTONE: &str = "Tombstones.\n\nUsage: forklift-server tombstone <COMMAND>\n\nCommands:\n  list  List them\n  drop  Drop one: the name is free for unrelated history, and the history it held\n        is collected by the next gc. Refused while the warehouse is being served\n  help  Print this message\n\nOptions:\n  -h, --help  Print help\n";
    const LEAF: &str = "Serve.\n\nUsage: forklift-server serve [OPTIONS] --root <ROOT>\n\nOptions:\n  --root <ROOT>  The warehouse\n";

    /// The server's leaves are every subcommand its `--help` lists, nested ones walked, `help`
    /// never one; a line maps to its longest leaf.
    ///
    /// Revert: stop at the top level → `tombstone` is a leaf and `tombstone drop` never one; read
    /// a wrapped description's words as names → `is` is a leaf.
    #[test]
    fn server_leaves_walk_nested_subcommands() {
        let mut help = |prefix: &[String]| -> Result<String, String> {
            Ok(match prefix.join(" ").as_str() { "" => TOP, "tombstone" => TOMBSTONE, _ => LEAF }.to_string())
        };
        let found = leaves(&[], &mut help).unwrap();
        assert_eq!(found, vec![strings(&["serve"]), strings(&["tombstone", "list"]), strings(&["tombstone", "drop"]), strings(&["gc"])]);
        assert_eq!(leaf_of(&found, &strings(&["tombstone", "drop", "side", "--root", "/h"])).as_deref(), Some("forklift-server tombstone drop"));
        assert_eq!(leaf_of(&found, &strings(&["gc", "--root", "/h"])).as_deref(), Some("forklift-server gc"));
        assert_eq!(leaf_of(&found, &strings(&["prepare"])), None);
    }

    /// A write counts only on a warehouse: under the scratch root, outside home and tmp, and not
    /// on a lock file.
    ///
    /// Revert: keep lock files → a read that only takes the lock counts as a write.
    #[test]
    fn only_warehouse_writes_count() {
        let root = Root { given: "/o/s/0001-x".into(), canonical: "/p/o/s/0001-x".into() };
        let written = strings(&[
            "/o/s/0001-x/work/.forklift/lock", "/o/s/0001-x/work/.forklift/refs.lock", "/o/s/0001-x/home/.forkliftconfig",
            "/o/s/0001-x/tmp/x", "/elsewhere/file", "/p/o/s/0001-x/work/.forklift/pallets/main", "/o/s/0001-x/host/.forklift/objects/ab/cd",
            "/o/s/0001-x/work/.forklift/pallets/main",
        ]);
        assert_eq!(warehouse_writes(&written, &root), strings(&["host/.forklift/objects/ab/cd", "work/.forklift/pallets/main"]));
    }

    fn seen(id: &str, traced: bool, writes: &[&str], changed: Option<&[&str]>) -> Evidence {
        Evidence { scenario: "s".into(), id: id.into(), traced, writes: strings(writes), changed: changed.map(strings) }
    }

    /// Covered needs a traced write; a traced run with none is "run, no write reached"; a command
    /// nothing runs is missing; a read-only exemption needs a digested run that changed nothing;
    /// an exemption for a command the version lacks is stale; untraced, coverage is not run but
    /// the digests still are.
    ///
    /// Revert: count an untraced run as covered → `heal` passes without a trace.
    #[test]
    fn the_check_tells_covered_from_reached_nothing_and_checks_exemptions() {
        let universe = strings(&["stack", "heal", "pick", "history", "stocktake", "version", "diff"]);
        let evidence = vec![
            seen("stack", true, &["work/.forklift/pallets/main"], None), seen("heal", true, &[], None), seen("heal", false, &["x"], None),
            seen("history", false, &[], Some(&[])), seen("stocktake", false, &[], Some(&["work/.forklift/inventory/metadata"])),
        ];
        let exemptions = [("history", Exemption::ReadOnly("reads")), ("stocktake", Exemption::ReadOnly("reads")), ("diff", Exemption::ReadOnly("reads")),
            ("version", Exemption::Unrunnable("prints")), ("mcp", Exemption::Unrunnable("gone"))];
        let checked = check(&universe, &evidence, &exemptions, None);
        assert_eq!(checked[0].missing, strings(&[
            "mcp: exempt, but the version under test has no such command", "heal: run, no write reached (in s)", "pick: no scenario runs it",
        ]));
        assert_eq!(checked[1].missing, strings(&[
            "stocktake: exempt as read-only, and in s it changed work/.forklift/inventory/metadata", "diff: exempt as read-only, and no scenario runs it",
        ]));
        let untraced = check(&universe, &evidence, &exemptions, Some("no strace given"));
        assert_eq!(untraced[0].cell, Cell::not_run("no strace given"));
        assert_eq!(untraced[0].missing, strings(&["mcp: exempt, but the version under test has no such command"]));
        assert_eq!(untraced[1].missing, checked[1].missing);
    }

    /// A digest sees content, not times: a rewrite of the same bytes is no change, a new byte or
    /// a new path is, and a lock file is never looked at.
    #[test]
    fn a_digest_sees_content_and_skips_locks() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("proof-load-digest-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("a/f"), b"one").unwrap();
        let before = digest(&root).unwrap();
        std::fs::write(root.join("a/f"), b"one").unwrap();
        std::fs::write(root.join("refs.lock"), b"held").unwrap();
        assert_eq!(changed(&before, &digest(&root).unwrap()), Vec::<String>::new());
        std::fs::write(root.join("a/f"), b"two").unwrap();
        std::fs::write(root.join("a/g"), b"").unwrap();
        assert_eq!(changed(&before, &digest(&root).unwrap()), strings(&["a/f", "a/g"]));
        #[allow(clippy::disallowed_methods, reason = "the test's own temp folder")]
        std::fs::remove_dir_all(&root).unwrap();
    }
}
