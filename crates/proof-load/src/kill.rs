//! Kill points (Linux): the faulted command killed at each mutating or syncing call it makes, on a
//! fresh copy of the scenario's scratch area, then graded like any other fault.
//!
//! A counting run traces the faulted command once, unfaulted, on its own copy, and every mutating
//! or syncing call the tool's own process makes there is a kill point. A point on a path fixed in
//! advance is aimed with `-P` at the path exactly as the trace shows it, an absolute one rebased
//! onto the injected run's root, and counted among the calls `-P` selects in its thread. A point on
//! a temp name, whose pid and counter differ per run, is counted among every call of its name in
//! its thread. strace's `when=` counter runs per thread, so another thread can reach its own count
//! first: every injected run is traced too, and a kill that did not land on its target is "not
//! reached" and grades nothing. A point some other thread or process could have stolen, though it
//! landed, is "sampled".

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use crate::corpus::Corpus;
use crate::damage::Warehouse;
use crate::faulted::{self, Context, Graded, Programs};
use crate::report::{Category, Cell, Completeness, Row};
use crate::runner::{self, RunError};
use crate::scenario::{Op, Scenario, Tool};
use crate::scrub::{Environment, Scratch};
use crate::tool::Outcome;
use crate::trace::{self, Argument, Call, Class, Operand, Returned, Root};

/// How a kill point is addressed.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "by", rename_all = "kebab-case")]
pub enum Aim {
    /// `-P` with this path, in the counting run's coordinates: as traced when relative, under the
    /// counting run's root when absolute.
    Path { path: String },
    /// No filter: counted among every call of the name in its thread.
    Position,
}

/// What a call is, in terms that survive a move to another root and another process: its flags,
/// and each path it names with the root replaced by `<root>` and a temp name's pid and counter
/// normalised away. A relative path stays relative, since both runs start in the same folder.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct Identity {
    pub detail: String,
    pub paths: Vec<String>,
}

/// One kill point.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Point {
    /// The index of the faulted command's line the point is in.
    pub line: usize,
    /// The command id: the line's subcommand.
    pub command: String,
    pub name: String,
    pub aim: Aim,
    /// strace's `when=`: the call's 1-based position among those its counter counts in its thread.
    pub when: usize,
    pub target: Identity,
    /// The unit's path base: the first path, resolved, root-relative, with temps and hashes
    /// normalised.
    pub base: String,
    /// Another thread or process makes a call this point's counter counts.
    pub sampled: bool,
}

impl Point {
    pub fn unit(&self) -> (String, String, String) {
        (self.command.clone(), self.name.clone(), self.base.clone())
    }
}

/// The kill points of one traced command line, and what its children did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Enumerated {
    pub points: Vec<Point>,
    /// Mutating or syncing calls of child processes, by name and path base: classified, listed,
    /// never targeted.
    pub children: BTreeSet<String>,
}

/// The working directory each process is in as the trace goes, for calls relative to it.
struct Directories {
    groups: BTreeMap<u32, u32>,
    cwd: BTreeMap<u32, String>,
    start: String,
}

impl Directories {
    fn new(groups: BTreeMap<u32, u32>, start: &str) -> Directories {
        Directories { groups, cwd: BTreeMap::new(), start: start.to_string() }
    }

    fn group(&self, tid: u32) -> u32 {
        self.groups.get(&tid).copied().unwrap_or(tid)
    }

    fn of(&self, tid: u32) -> String {
        self.cwd.get(&self.group(tid)).cloned().unwrap_or_else(|| self.start.clone())
    }

    /// Follows a call that changes a working directory or starts a process.
    fn follow(&mut self, call: &Call) {
        let group = self.group(call.tid);
        match (call.name.as_str(), &call.returned) {
            ("chdir", Returned::Value(_)) => {
                if let Some(Argument::Text(path)) = call.arguments.first().map(|text| trace::argument(text)) {
                    let next = trace::resolve(&path, &self.of(call.tid));
                    self.cwd.insert(group, next);
                }
            }
            ("fchdir", Returned::Value(_)) => {
                if let Some(Argument::Descriptor { target, .. }) = call.arguments.first().map(|text| trace::argument(text)) {
                    self.cwd.insert(group, target);
                }
            }
            ("clone" | "clone3" | "fork" | "vfork", Returned::Value(child)) => {
                if let Ok(child) = child.parse::<u32>() {
                    let child_group = self.group(child);
                    if child_group != group && !self.cwd.contains_key(&child_group) {
                        let inherited = self.of(call.tid);
                        self.cwd.insert(child_group, inherited);
                    }
                }
            }
            _ => {}
        }
    }

    /// Where an operand points, absolute.
    fn resolved(&self, operand: &Operand, tid: u32) -> String {
        match operand {
            Operand::Named { traced, base } => trace::resolve(traced, base.as_deref().unwrap_or(&self.of(tid))),
            Operand::Held { target } => target.clone(),
        }
    }
}

/// Every path a successful mutating call in `calls` named, absolute, from every thread and process
/// the trace holds: the write-op check's evidence. `start` is the folder the line ran in. A call
/// that failed, never returned, or the table cannot classify names nothing here.
pub fn written(calls: &[Call], start: &str) -> Vec<String> {
    let mut directories = Directories::new(trace::thread_groups(calls), start);
    let mut paths = Vec::new();
    for call in calls {
        if let Ok(touched) = trace::touch(call) {
            if touched.class == Class::Mutating && matches!(call.returned, Returned::Value(_)) {
                paths.extend(touched.operands.iter().map(|operand| directories.resolved(operand, call.tid)));
            }
        }
        directories.follow(call);
    }
    paths
}

/// `call`'s identity: see [`Identity`].
pub fn identity(operands: &[Operand], detail: &str, root: &Root, pid: u32) -> Identity {
    let paths = operands.iter().map(|operand| {
        let path = match operand {
            Operand::Named { traced, .. } => traced.clone(),
            Operand::Held { target } => target.clone(),
        };
        trace::without_temp(&root.tokenized(&path), pid).0
    }).collect();
    Identity { detail: detail.to_string(), paths }
}

/// Whether strace's `-P` with the paths in `aims` selects `call`: a path argument passed as one of
/// them, or a descriptor or directory descriptor `-y` resolves to one.
fn selected(call: &Call, aims: &BTreeSet<String>) -> bool {
    call.arguments.iter().any(|text| match trace::argument(text) {
        Argument::Text(path) => aims.contains(&path),
        Argument::Descriptor { target, .. } => aims.contains(&target),
        _ => false,
    })
}

/// The kill points of one command line's counting trace. `root` is the counting run's, `start` the
/// folder the line ran in, and `existed` says whether an absolute path (in the counting run's
/// coordinates) existed when the line started, since strace's `-P` adds the resolved form of a path
/// that exists then. A call the table cannot classify, or a write outside the root, refuses.
pub fn enumerate(calls: &[Call], line: usize, command: &str, root: &Root, start: &str, existed: impl Fn(&str) -> bool) -> Result<Enumerated, Vec<String>> {
    let groups = trace::thread_groups(calls);
    let tool = calls.first().map(|call| call.tid).unwrap_or_default();
    let mut directories = Directories::new(groups, start);
    let mut refusals = Vec::new();
    let mut enumerated = Enumerated::default();
    // Each candidate: (index into calls, aim set in counting coordinates, aim, identity, base).
    let mut candidates = Vec::new();

    for (index, call) in calls.iter().enumerate() {
        let touched = match trace::touch(call) {
            Ok(touched) => touched,
            Err(unsound) => {
                refusals.push(format!("{}: {}", command, unsound));
                continue;
            }
        };
        let resolved: Vec<String> = touched.operands.iter().map(|operand| directories.resolved(operand, call.tid)).collect();
        directories.follow(call);
        if touched.class == Class::Inert || call.returned == Returned::Restarted {
            continue;
        }
        // A descriptor that is no file (a pipe, a socket, anonymous memory), or a device, is not state.
        let files: Vec<usize> = (0..resolved.len())
            .filter(|at| resolved[*at].starts_with('/') && !resolved[*at].starts_with("/dev/") && !resolved[*at].starts_with("/memfd:"))
            .collect();
        if files.is_empty() && !touched.operands.is_empty() {
            continue;
        }
        if let Some(outside) = files.iter().map(|at| &resolved[*at]).find(|path| !root.holds(path)) {
            refusals.push(format!("{}: {} writes outside its scratch area: {}", command, call.name, outside));
            continue;
        }
        let group = directories.group(call.tid);
        let base = resolved.first().map(|path| trace::base(&trace::without_temp(&root.tokenized(path), group).0)).unwrap_or_default();
        if group != tool {
            enumerated.children.insert(format!("{} {}", call.name, base));
            continue;
        }
        let target = identity(&touched.operands, &touched.detail, root, group);
        // Judged below the root, so digits in the scratch area's own name are never taken for a temp.
        let temp = resolved.iter().any(|path| trace::without_temp(&root.tokenized(path), group).1);
        let (aim, aims) = match touched.operands.first() {
            Some(first) if !temp => {
                let path = match first { Operand::Named { traced, .. } => traced.clone(), Operand::Held { target } => target.clone() };
                let mut aims = BTreeSet::from([path.clone()]);
                // strace adds realpath() of the -P string when it starts: resolved from strace's
                // own working directory, the line's start folder, whatever the call's dirfd.
                let literal = lexical_join(start, &path);
                if let Some(canonical) = root.rebase(&literal, &Root { given: root.canonical.clone(), canonical: root.canonical.clone() }) {
                    if existed(&literal) {
                        aims.insert(canonical);
                    }
                }
                (Aim::Path { path }, Some(aims))
            }
            _ => (Aim::Position, None),
        };
        candidates.push((index, aims, aim, target, base));
    }
    if !refusals.is_empty() {
        return Err(refusals);
    }

    for (index, aims, aim, target, base) in candidates {
        let call = &calls[index];
        let counts = |other: &Call| other.name == call.name && aims.as_ref().is_none_or(|aims| selected(other, aims));
        let when = calls[..=index].iter().filter(|other| other.tid == call.tid && counts(other)).count();
        let sampled = calls.iter().any(|other| other.tid != call.tid && counts(other));
        enumerated.points.push(Point { line, command: command.to_string(), name: call.name.clone(), aim, when, target, base, sampled });
    }
    Ok(enumerated)
}

/// `path` resolved against the folder `start` as a path, without the file system: `.` and `..`
/// folded, an absolute `path` kept.
fn lexical_join(start: &str, path: &str) -> String {
    let mut parts: Vec<&str> = if path.starts_with('/') { Vec::new() } else { start.split('/').filter(|part| !part.is_empty()).collect() };
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

/// Whether an injected run's kill landed on `point`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum Landing {
    Landed,
    NotReached { why: String },
}

/// Judges an injected run's trace, traced with `-e trace=<name>` (plus the process calls, for a
/// positional point): the kill landed when the call that never returned is, in its thread, the
/// `when`-th call the counter counts, and is the point's target. `root` is the injected run's.
pub fn landing(calls: &[Call], point: &Point, root: &Root, exit: Option<i32>) -> Landing {
    if exit == Some(0) {
        return Landing::NotReached { why: "the injection never fired: the command finished".to_string() };
    }
    let groups = trace::thread_groups(calls);
    // Under -P the trace holds only selected calls and no execve; the tool's process is the first.
    let tool = calls.iter().find(|call| call.name == "execve").or(calls.first()).map(|call| call.tid).unwrap_or_default();
    let mut seen = Vec::new();
    for (index, call) in calls.iter().enumerate() {
        if call.name != point.name || call.returned != Returned::Unknown {
            continue;
        }
        let position = calls[..=index].iter().filter(|other| other.tid == call.tid && other.name == point.name).count();
        let Ok(touched) = trace::touch(call) else { continue };
        let group = groups.get(&call.tid).copied().unwrap_or(call.tid);
        let found = identity(&touched.operands, &touched.detail, root, group);
        if position == point.when && stands_for(&found, &point.target, point.aim == Aim::Position) && (point.aim != Aim::Position || group == tool) {
            return Landing::Landed;
        }
        seen.push(format!("{}#{} {}", call.name, position, found.paths.join(" ")));
    }
    Landing::NotReached {
        why: if seen.is_empty() { "no call of the name was killed".to_string() } else { format!("the kill landed on {}", seen.join("; ")) },
    }
}

/// Whether a killed call stands for the targeted one: the same flags and paths, or, for a temp,
/// whose name differs per run, the same flags and the same path bases, since where several threads
/// stage beside one target, which of them stages which object is the scheduler's choice.
fn stands_for(found: &Identity, target: &Identity, temp: bool) -> bool {
    let bases = |identity: &Identity| identity.paths.iter().map(|path| trace::base(path)).collect::<Vec<_>>();
    found == target || (temp && found.detail == target.detail && bases(found) == bases(target))
}

/// Whether any file under `root` holds one of `needles` at a path-component boundary: a recorded
/// path that would tie a copy to the area it was copied from. The first such file, root-relative.
pub fn recorded_root(root: &Path, needles: &[&str]) -> std::io::Result<Option<String>> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            for entry in std::fs::read_dir(&path)? {
                stack.push(entry?.path());
            }
            continue;
        }
        let bytes = if metadata.file_type().is_symlink() {
            std::fs::read_link(&path)?.to_string_lossy().into_owned().into_bytes()
        } else {
            std::fs::read(&path)?
        };
        if needles.iter().any(|needle| holds_root(&bytes, needle.as_bytes())) {
            let relative = path.strip_prefix(root).unwrap_or(&path);
            let parts: Vec<String> = relative.components().map(|part| part.as_os_str().to_string_lossy().into_owned()).collect();
            return Ok(Some(parts.join("/")));
        }
    }
    Ok(None)
}

/// `needle` in `bytes`, not followed by a byte that would continue its last component.
fn holds_root(bytes: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || bytes.len() < needle.len() {
        return false;
    }
    (0..=bytes.len() - needle.len()).any(|at| {
        bytes[at..].starts_with(needle)
            && bytes.get(at + needle.len()).is_none_or(|next| !(next.is_ascii_alphanumeric() || matches!(next, b'-' | b'_' | b'.')))
    })
}

/// The paths under `root`, absolute, as they stand.
fn listing(root: &Path) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        paths.insert(path.to_string_lossy().into_owned());
        if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
            for entry in std::fs::read_dir(&path).into_iter().flatten().flatten() {
                stack.push(entry.path());
            }
        }
    }
    paths
}

/// What one kill point's injected run did.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Killed {
    pub point: Point,
    pub landing: Landing,
    /// The grade, when the kill landed.
    pub graded: Option<Graded>,
    /// The injected runs it took, each on a fresh copy: a kill another thread stole is tried again,
    /// up to [`ATTEMPTS`].
    pub attempts: usize,
}

/// Every kill point of one scenario's faulted command.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ScenarioKills {
    pub scenario: String,
    /// Why the scenario's kill points were not run, when they were not.
    pub not_run: Option<String>,
    /// What refused the scenario's traces: unclassified calls, unseen writes, writes outside the
    /// scratch area.
    pub refusals: Vec<String>,
    /// Points whose run failed to happen at all, with why.
    pub errors: Vec<String>,
    pub killed: Vec<Killed>,
    pub children: BTreeSet<String>,
}

/// What the kill-point runner needs beyond a faulted run's programs.
pub struct Settings<'a> {
    /// strace, by absolute path.
    pub strace: &'a Path,
    pub search_path: Option<OsString>,
    pub limit: Duration,
    pub jobs: usize,
    /// Where traces go: outside every scratch area, so no scan reads them.
    pub traces: &'a Path,
    /// The version under test's schema, which names the command a faulted line runs.
    pub schema: &'a crate::writeops::Schema,
}

/// The command id of a forklift line, as the schema names it; its first argument after `--json`
/// when the schema has no command for it.
pub fn command_id(schema: &crate::writeops::Schema, arguments: &[String]) -> String {
    schema.id_of(arguments).unwrap_or_else(|| arguments.iter().find(|argument| *argument != "--json").cloned().unwrap_or_default())
}

/// Why a scenario's kill points cannot run in this build of the harness, if they cannot.
pub fn not_run(scenario: &Scenario) -> Option<String> {
    if scenario.history.iter().any(|op| matches!(op, Op::Host)) {
        return Some("hosted: every copy would share one server and its warehouse; copies of the host arrive with server kill points".to_string());
    }
    None
}

/// strace's command line tracing `program` with `arguments`, its trace written to `trace`.
pub fn strace_line(trace: &Path, filter: &[String], program: &Path, arguments: &[String]) -> Vec<String> {
    let mut line: Vec<String> = vec!["-f".into(), "-qq".into(), "-y".into(), "-o".into(), trace.display().to_string()];
    line.extend(filter.iter().cloned());
    line.push("--".into());
    line.push(program.display().to_string());
    line.extend(arguments.iter().cloned());
    line
}

/// Runs every kill point of `scenario`'s faulted command. `template` is a fresh scratch area the
/// history is built in; every counting and injected run works on a copy of it beside it.
pub fn run(scenario: &Scenario, corpus: &Corpus, programs: &Programs, settings: &Settings, template: &Scratch) -> Result<ScenarioKills, RunError> {
    let mut kills = ScenarioKills { scenario: scenario.id.clone(), ..Default::default() };
    if let Some(reason) = not_run(scenario) {
        kills.not_run = Some(reason);
        return Ok(kills);
    }
    let io = |error: std::io::Error| RunError::Io(error.to_string());
    let environment = environment_of(settings, template)?;
    let built = runner::build(scenario, corpus, Tool::Forklift, &programs.forklift, programs.server.as_deref(), template, &environment, settings.limit, None)?;
    let corpus = &scenario.corpus(corpus);
    let (expected, faulted) = faulted::expected(scenario, corpus, &built, Warehouse::Client)?;
    let original = Root::of(template.root()).map_err(io)?;
    // Each copy starts byte-identical to the template, so scanning the template once scans every
    // copy before it is used.
    if let Some(file) = recorded_root(template.root(), &[&original.given, &original.canonical]).map_err(io)? {
        kills.not_run = Some(format!("path-bound: {} records the scratch area's root", file));
        return Ok(kills);
    }

    // The counting run, on its own copy.
    let label = template.root().file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let counting = template.copy_to(Path::new(&format!("{}-count", template.root().display()))).map_err(|error| RunError::Io(error.to_string()))?;
    let counting_environment = environment_of(settings, &counting)?;
    let counting_context = context_of(scenario, corpus, programs, settings, &counting, &counting_environment);
    let counting_root = Root::of(counting.root()).map_err(io)?;
    counting_context.write_snapshot(faulted)?;
    let lines = counting_context.lines(faulted)?;
    let mut points = Vec::new();
    for (index, arguments) in lines.iter().enumerate() {
        let before = listing(counting.root());
        let trace_path = settings.traces.join(format!("{}-count-{}.trace", label, index));
        let line = strace_line(&trace_path, &[], &programs.forklift, arguments);
        let outcome = counting_context.run(settings.strace, line, counting.work())?;
        if !outcome.succeeded() {
            return Err(RunError::BuildFailed {
                arguments: arguments.clone(), exit: outcome.exit,
                stderr: String::from_utf8_lossy(&outcome.stderr).into_owned(), stdout: String::from_utf8_lossy(&outcome.stdout).into_owned(),
            });
        }
        let text = std::fs::read_to_string(&trace_path).map_err(io)?;
        let calls = trace::parse(&text).map_err(RunError::Io)?;
        let start = counting.work().to_string_lossy().into_owned();
        let existed = |path: &str| before.contains(path) || counting_root.rebase(path, &Root { given: counting_root.given.clone(), canonical: counting_root.given.clone() }).is_some_and(|given| before.contains(&given));
        match enumerate(&calls, index, &command_id(settings.schema, arguments), &counting_root, &start, existed) {
            Ok(enumerated) => {
                points.extend(enumerated.points);
                kills.children.extend(enumerated.children);
            }
            Err(refusals) => kills.refusals.extend(refusals),
        }
    }
    if !kills.refusals.is_empty() {
        return Ok(kills);
    }
    if let Some(file) = recorded_root(counting.root(), &[&original.given, &original.canonical, &counting_root.given, &counting_root.canonical]).map_err(io)? {
        kills.not_run = Some(format!("path-bound: the faulted command recorded the scratch area's root in {}", file));
        return Ok(kills);
    }

    // The injected runs, in parallel, each on its own copy.
    let shared = Shared { scenario, corpus, programs, settings, template, label: &label, lines: &lines, expected: &expected, faulted, counting_root: &counting_root };
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, Result<Killed, String>)>> = Mutex::new(Vec::new());
    let workers = settings.jobs.clamp(1, points.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let at = next.fetch_add(1, Ordering::SeqCst);
                let Some(point) = points.get(at) else { break };
                let result = attempt(&shared, point, at)
                    .map_err(|error| format!("{} {}#{}: {}", point.command, point.name, point.when, error));
                results.lock().expect("no worker panics holding the lock").push((at, result));
            });
        }
    });
    let mut results = results.into_inner().expect("no worker panics holding the lock");
    results.sort_by_key(|(at, _)| *at);
    for (_, result) in results {
        match result {
            Ok(killed) => kills.killed.push(killed),
            Err(error) => kills.errors.push(error),
        }
    }
    Ok(kills)
}

/// What every injected run of one scenario shares.
#[derive(Clone, Copy)]
struct Shared<'a> {
    scenario: &'a Scenario,
    corpus: &'a Corpus,
    programs: &'a Programs,
    settings: &'a Settings<'a>,
    template: &'a Scratch,
    label: &'a str,
    lines: &'a [Vec<String>],
    expected: &'a faulted::Expected,
    faulted: usize,
    counting_root: &'a Root,
}

fn environment_of(settings: &Settings, scratch: &Scratch) -> Result<Environment, RunError> {
    Environment::scrubbed(scratch, settings.search_path.clone(), &BTreeMap::new()).map_err(|error| RunError::Io(error.to_string()))
}

fn context_of<'a>(scenario: &'a Scenario, corpus: &'a Corpus, programs: &'a Programs, settings: &Settings, scratch: &'a Scratch, environment: &'a Environment) -> Context<'a> {
    Context { corpus, programs, scratch, environment, limit: settings.limit, then: &scenario.then, url: None, scope: scenario.scope(), via_host: false }
}

/// One injected run: a fresh copy, the lines before the point's run plainly, the point's line run
/// under strace with the kill injected, the landing judged, and a landed kill graded.
/// How many injected runs a kill point gets before it is reported not reached.
pub const ATTEMPTS: usize = 3;

/// A kill point's injected runs: the first that lands, or the last that did not.
fn attempt(shared: &Shared, point: &Point, at: usize) -> Result<Killed, RunError> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        let mut killed = inject(shared, point, &format!("{}-{}", at, attempts))?;
        killed.attempts = attempts;
        if killed.landing == Landing::Landed || attempts == ATTEMPTS {
            return Ok(killed);
        }
    }
}

fn inject(shared: &Shared, point: &Point, at: &str) -> Result<Killed, RunError> {
    let Shared { scenario, corpus, programs, settings, template, label, lines, expected, faulted, counting_root } = *shared;
    let io = |error: std::io::Error| RunError::Io(error.to_string());
    let copy = template.copy_to(Path::new(&format!("{}-kill-{}", template.root().display(), at))).map_err(|error| RunError::Io(error.to_string()))?;
    let environment = environment_of(settings, &copy)?;
    let context = context_of(scenario, corpus, programs, settings, &copy, &environment);
    let root = Root::of(copy.root()).map_err(io)?;

    let mut steps: Vec<Outcome> = Vec::new();
    context.write_snapshot(faulted)?;
    for arguments in &lines[..point.line] {
        let outcome = context.run(&programs.forklift, arguments.clone(), copy.work())?;
        let succeeded = outcome.succeeded();
        steps.push(outcome);
        if !succeeded {
            return Err(RunError::Io(format!("`{}` refused on a fresh copy, though it succeeded when counted", arguments.join(" "))));
        }
    }
    let mut filter = Vec::new();
    match &point.aim {
        Aim::Path { path } => {
            let aimed = if path.starts_with('/') {
                counting_root.rebase(path, &root).ok_or_else(|| RunError::Io(format!("the aim {} is outside the counting run's root", path)))?
            } else {
                path.clone()
            };
            filter.extend(["-P".to_string(), aimed, "-e".to_string(), format!("trace={}", point.name)]);
        }
        Aim::Position => filter.extend(["-e".to_string(), format!("trace={},execve,clone,clone3,fork,vfork", point.name)]),
    }
    filter.extend(["-e".to_string(), format!("inject={}:signal=KILL:when={}", point.name, point.when)]);
    let trace_path = settings.traces.join(format!("{}-kill-{}.trace", label, at));
    let arguments = &lines[point.line];
    let mut outcome = context.run(settings.strace, strace_line(&trace_path, &filter, &programs.forklift, arguments), copy.work())?;
    outcome.arguments = arguments.clone();
    let text = std::fs::read_to_string(&trace_path).map_err(io)?;
    let calls = trace::parse(&text).map_err(RunError::Io)?;
    let landing = landing(&calls, point, &root, outcome.exit);
    steps.push(outcome);

    let graded = match landing {
        Landing::Landed => Some(faulted::grade_after(&context, expected, faulted, |_, first| {
            first.append(&mut steps);
            Ok(false)
        })?),
        Landing::NotReached { .. } => None,
    };
    // A copy that graded well holds nothing to look at; one that did not is kept for diagnosis.
    if graded.as_ref().is_none_or(|graded| !graded.grade.is_failure()) {
        #[allow(clippy::disallowed_methods, reason = "a scratch copy the harness made itself, under its own output directory")]
        let _ = std::fs::remove_dir_all(copy.root());
    }
    Ok(Killed { point: point.clone(), landing, graded, attempts: 1 })
}

/// What the kill points of a run add to its report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    /// Run, or sampled once any landed point was.
    pub cell: Cell,
    pub rows: Vec<Row>,
    pub completeness: Vec<Completeness>,
    pub not_reached: u64,
}

/// The report's share of every scenario's kill points. A unit, a command's calls of one name on
/// one path base, is covered when any of its points landed; one with none landed is missing, as is
/// a scenario whose faulted command made no call to kill. A scenario reported not run is listed,
/// and fails nothing.
pub fn summarize(kills: &[ScenarioKills]) -> Summary {
    let mut rows = Vec::new();
    let mut units: BTreeMap<(String, String, String, String), bool> = BTreeMap::new();
    let (mut refusals, mut errors, mut empty, mut not_run) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut not_reached = 0;
    let mut sampled = false;
    for scenario in kills {
        if let Some(reason) = &scenario.not_run {
            not_run.push(Completeness { check: format!("kill points of {}", scenario.scenario), cell: Cell::not_run(reason), missing: vec![] });
            continue;
        }
        refusals.extend(scenario.refusals.iter().map(|refusal| format!("{}: {}", scenario.scenario, refusal)));
        errors.extend(scenario.errors.iter().map(|error| format!("{}: {}", scenario.scenario, error)));
        if scenario.refusals.is_empty() && scenario.killed.is_empty() && scenario.errors.is_empty() {
            empty.push(format!("{}: the faulted command made no call to kill", scenario.scenario));
        }
        for killed in &scenario.killed {
            let (command, name, base) = killed.point.unit();
            let covered = units.entry((scenario.scenario.clone(), command, name, base)).or_default();
            let Some(graded) = &killed.graded else {
                not_reached += 1;
                continue;
            };
            *covered = true;
            sampled |= killed.point.sampled;
            let mut notes = Vec::new();
            if killed.point.sampled {
                notes.push("sampled".to_string());
            }
            if !graded.unextractable.is_empty() {
                notes.push(format!("unextractable remedies: {}", graded.unextractable.join("; ")));
            }
            let point = &killed.point;
            rows.push(Row {
                scenario: format!("{} {} {}#{} {}", scenario.scenario, point.command, point.name, point.when, point.base),
                category: Category::KillPointsClient,
                binding: "forklift".to_string(),
                grade: graded.grade,
                recoverability: graded.recoverability.clone(),
                note: (!notes.is_empty()).then(|| notes.join("; ")),
            });
        }
    }
    let mut missing: Vec<String> = units.into_iter().filter(|(_, covered)| !covered)
        .map(|((scenario, command, name, base), _)| format!("{}: {} {} {}: no kill landed", scenario, command, name, base)).collect();
    missing.extend(empty);
    if kills.iter().all(|scenario| scenario.not_run.is_some()) {
        missing.push("no scenario's kill points ran".to_string());
    }
    let mut completeness = vec![
        Completeness { check: "kill-point units".to_string(), cell: Cell::Run, missing },
        Completeness { check: "kill-point soundness".to_string(), cell: Cell::Run, missing: refusals },
        Completeness { check: "kill points ran".to_string(), cell: Cell::Run, missing: errors },
    ];
    completeness.extend(not_run);
    let cell = if rows.is_empty() {
        Cell::not_run("no kill point was graded")
    } else if sampled {
        Cell::Sampled
    } else {
        Cell::Run
    };
    Summary { cell, rows, completeness, not_reached }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> Root {
        Root { given: "/o/s/0001-h-count".into(), canonical: "/o/s/0001-h-count".into() }
    }

    /// A counting trace yields one kill point per mutating or syncing call of the tool's process:
    /// a fixed path aimed with -P as traced and counted among the calls -P selects in its thread,
    /// a temp counted among every call of its name in its thread, a pipe write and a read-only
    /// open none, a child's write listed and not targeted, and a point other threads could steal
    /// sampled.
    ///
    /// Revert: count positions across threads → the worker's temp open is #3, not #1; aim the temp
    /// with -P → its pid-bearing path never fires; target the child → git's write is a point.
    #[test]
    fn a_counting_trace_yields_its_kill_points() {
        let text = "\
50 execve(\"/b/forklift\", [\"forklift\", \"--json\"], 0x1 /* 3 vars */) = 0
50 chdir(\"/o/s/0001-h-count/work\") = 0
50 clone(child_stack=0x1, flags=CLONE_VM|CLONE_THREAD) = 51
51 openat(AT_FDCWD</o/s/0001-h-count/work>, \".forklift/pallets/main\", O_RDONLY|O_CLOEXEC) = 3</o/s/0001-h-count/work/.forklift/pallets/main>
51 openat(AT_FDCWD</o/s/0001-h-count/work>, \".forklift/pallets/main.tmp~50-0\", O_WRONLY|O_CREAT|O_TRUNC|O_CLOEXEC, 0666) = 4</o/s/0001-h-count/work/.forklift/pallets/main.tmp~50-0>
51 write(4</o/s/0001-h-count/work/.forklift/pallets/main.tmp~50-0>, \"abc\", 3) = 3
51 write(1<pipe:[9]>, \"{}\", 2) = 2
51 fsync(4</o/s/0001-h-count/work/.forklift/pallets/main.tmp~50-0>) = 0
51 renameat(AT_FDCWD</o/s/0001-h-count/work>, \".forklift/pallets/main.tmp~50-0\", AT_FDCWD</o/s/0001-h-count/work>, \".forklift/pallets/main\") = 0
51 openat(AT_FDCWD</o/s/0001-h-count/work>, \".forklift/pallets\", O_RDONLY|O_DIRECTORY|O_CLOEXEC) = 5</o/s/0001-h-count/work/.forklift/pallets>
51 fsync(5</o/s/0001-h-count/work/.forklift/pallets>) = 0
50 clone(child_stack=0x1, flags=CLONE_VM|CLONE_THREAD) = 52
52 openat(AT_FDCWD</o/s/0001-h-count/work>, \".forklift/objects/ab/0123456789abcdef0123.tmp~50-1\", O_WRONLY|O_CREAT, 0666) = 6</o/s/0001-h-count/work/.forklift/objects/ab/0123456789abcdef0123.tmp~50-1>
50 clone(child_stack=NULL, flags=SIGCHLD) = 60
60 write(3</o/s/0001-h-count/tmp/x>, \"abc\", 3) = 3
";
        let calls = trace::parse(text).unwrap();
        let enumerated = enumerate(&calls, 1, "stack", &root(), "/o/s/0001-h-count/work", |_| false).unwrap();
        let points: Vec<(&str, &Aim, usize, &str, bool)> = enumerated.points.iter().map(|point| (point.name.as_str(), &point.aim, point.when, point.base.as_str(), point.sampled)).collect();
        let pallets = Aim::Path { path: "/o/s/0001-h-count/work/.forklift/pallets".into() };
        assert_eq!(points, vec![
            ("openat", &Aim::Position, 2, "<root>/work/.forklift/pallets/main.tmp~<pid>-<n>", true),
            ("write", &Aim::Position, 1, "<root>/work/.forklift/pallets/main.tmp~<pid>-<n>", true),
            ("fsync", &Aim::Position, 1, "<root>/work/.forklift/pallets/main.tmp~<pid>-<n>", false),
            ("renameat", &Aim::Position, 1, "<root>/work/.forklift/pallets/main.tmp~<pid>-<n>", false),
            ("fsync", &pallets, 1, "<root>/work/.forklift/pallets", false),
            ("openat", &Aim::Position, 1, "<root>/work/.forklift/objects/<hex>/<hex>.tmp~<pid>-<n>", true),
        ]);
        assert_eq!(enumerated.points[0].target, Identity {
            detail: "O_WRONLY|O_CREAT|O_TRUNC|O_CLOEXEC".into(), paths: vec![".forklift/pallets/main.tmp~<pid>-<n>".into()],
        });
        assert_eq!(enumerated.children, BTreeSet::from(["write <root>/tmp/x".to_string()]));
        assert!(enumerated.points.iter().all(|point| point.line == 1 && point.command == "stack"));
    }

    /// A fixed path counts only the calls -P would select, including a descriptor on the path's
    /// resolved form once the path existed when the line started; a write outside the root and an
    /// unclassified call refuse.
    ///
    /// Revert: drop the resolved form → the fsync on the existing file is counted #1 though -P
    /// counts the earlier open of it as well; resolve against the dirfd → the mkdir of "d" under
    /// "src" counts the earlier call under src/d, which -P never selects.
    #[test]
    fn a_fixed_path_counts_what_its_filter_selects() {
        let text = "\
50 execve(\"/b/forklift\", [], 0x1) = 0
50 openat(AT_FDCWD</o/s/0001-h-count/work>, \"a\", O_WRONLY|O_TRUNC) = 3</o/s/0001-h-count/work/a>
50 fsync(3</o/s/0001-h-count/work/a>) = 0
50 unlinkat(AT_FDCWD</o/s/0001-h-count/work>, \"b\", 0) = -1 ENOENT (No such file or directory)
50 unlinkat(AT_FDCWD</o/s/0001-h-count/work>, \"b\", 0) = 0
";
        let calls = trace::parse(text).unwrap();
        let existed = |path: &str| path == "/o/s/0001-h-count/work/a";
        let enumerated = enumerate(&calls, 0, "load", &root(), "/o/s/0001-h-count/work", existed).unwrap();
        let whens: Vec<(&str, usize)> = enumerated.points.iter().map(|point| (point.name.as_str(), point.when)).collect();
        assert_eq!(whens, vec![("openat", 1), ("fsync", 1), ("unlinkat", 1), ("unlinkat", 2)]);
        // A directory that existed: -P on "d" also selects a call whose directory descriptor is it.
        let under = trace::parse("\
50 execve(\"/b\", [], 0x1) = 0
50 mkdirat(4</o/s/0001-h-count/work/d>, \"e\", 0777) = 0
50 mkdirat(AT_FDCWD</o/s/0001-h-count/work>, \"d\", 0777) = -1 EEXIST (File exists)
").unwrap();
        let existed = |path: &str| path == "/o/s/0001-h-count/work/d";
        let points = enumerate(&under, 0, "load", &root(), "/o/s/0001-h-count/work", existed).unwrap().points;
        assert_eq!((&points[1].aim, points[1].when), (&Aim::Path { path: "d".into() }, 2));
        // strace resolves the -P string from its own folder, not the call's dirfd: "d" under the
        // descriptor of "src" adds the start folder's "d", so the call under src/d is not selected.
        let dirfd = trace::parse("\
50 execve(\"/b\", [], 0x1) = 0
50 mkdirat(4</o/s/0001-h-count/work/src/d>, \"e\", 0777) = 0
50 mkdirat(4</o/s/0001-h-count/work/src>, \"d\", 0777) = -1 EEXIST (File exists)
").unwrap();
        let existed = |path: &str| path == "/o/s/0001-h-count/work/src/d";
        let points = enumerate(&dirfd, 0, "load", &root(), "/o/s/0001-h-count/work", existed).unwrap().points;
        assert_eq!((&points[1].aim, points[1].when), (&Aim::Path { path: "d".into() }, 1));
        assert_eq!(lexical_join("/w/x", "../y/./z"), "/w/y/z");
        assert_eq!(lexical_join("/w", "/abs"), "/abs");

        let outside = trace::parse("50 execve(\"/b\", [], 0x1) = 0\n50 mkdirat(AT_FDCWD</o/s/0001-h-count/work>, \"/etc/x\", 0777) = 0\n").unwrap();
        let refused = enumerate(&outside, 0, "load", &root(), "/o/s/0001-h-count/work", |_| false).unwrap_err();
        assert!(refused[0].contains("outside its scratch area: /etc/x"), "{:?}", refused);
        let unknown = trace::parse("50 execve(\"/b\", [], 0x1) = 0\n51 frobnicate(1) = 0\n").unwrap();
        assert!(enumerate(&unknown, 0, "load", &root(), "/w", |_| false).unwrap_err()[0].contains("unclassified syscall frobnicate"));
        // A legacy call relative to the working directory resolves through chdir.
        let legacy = trace::parse("50 execve(\"/b\", [], 0x1) = 0\n50 chdir(\"work\") = 0\n50 rename(\"x.tmp~50-0\", \"x\") = 0\n").unwrap();
        let points = enumerate(&legacy, 0, "stack", &root(), "/o/s/0001-h-count", |_| false).unwrap().points;
        assert_eq!(points[0].base, "<root>/work/x.tmp~<pid>-<n>");
    }

    fn point(name: &str, aim: Aim, when: usize, paths: &[&str], detail: &str) -> Point {
        Point {
            line: 0, command: "stack".into(), name: name.into(), aim, when, base: String::new(), sampled: false,
            target: Identity { detail: detail.into(), paths: paths.iter().map(|path| path.to_string()).collect() },
        }
    }

    /// A kill lands only on the call that is, in its thread, the counted position and the target,
    /// with the root rebased and the temp's pid normalised; a kill on another thread's call, on a
    /// child, or none at all is not reached; a sibling temp of the same base stands for a temp target,
    /// and never for a fixed path.
    ///
    /// Revert: accept any killed call of the name → the worker's object open, killed in place of
    /// the head's temp, counts as landed; compare temps exactly → the sibling object is not reached;
    /// compare fixed paths by base → the other object lands on a fixed target.
    #[test]
    fn a_kill_lands_only_on_its_target() {
        let injected = Root { given: "/o/s/0001-h-kill-4".into(), canonical: "/o/s/0001-h-kill-4".into() };
        let target = point("openat", Aim::Position, 2, &[".forklift/pallets/main.tmp~<pid>-<n>"], "O_WRONLY|O_CREAT");
        let landed = trace::parse("\
70 execve(\"/b/forklift\", [], 0x1) = 0
70 clone(child_stack=0x1, flags=CLONE_VM|CLONE_THREAD) = 71
71 openat(AT_FDCWD</o/s/0001-h-kill-4/work>, \"a\", O_RDONLY) = 3</o/s/0001-h-kill-4/work/a>
71 openat(AT_FDCWD</o/s/0001-h-kill-4/work>, \".forklift/pallets/main.tmp~70-0\", O_WRONLY|O_CREAT <unfinished ...>
71 <... openat resumed>) = ?
").unwrap();
        assert_eq!(landing(&landed, &target, &injected, None), Landing::Landed);
        let stolen = trace::parse("\
70 execve(\"/b/forklift\", [], 0x1) = 0
70 clone(child_stack=0x1, flags=CLONE_VM|CLONE_THREAD) = 72
72 openat(AT_FDCWD</o/s/0001-h-kill-4/work>, \"b\", O_RDONLY) = 3</o/s/0001-h-kill-4/work/b>
72 openat(AT_FDCWD</o/s/0001-h-kill-4/work>, \".forklift/objects/ab/cd.tmp~70-3\", O_WRONLY|O_CREAT) = ?
").unwrap();
        assert!(matches!(landing(&stolen, &target, &injected, None), Landing::NotReached { why } if why.contains("objects/ab/cd.tmp~<pid>-<n>")));
        assert!(matches!(landing(&landed, &target, &injected, Some(0)), Landing::NotReached { .. }));
        let child = trace::parse("\
70 execve(\"/b/forklift\", [], 0x1) = 0
70 clone(child_stack=NULL, flags=SIGCHLD) = 80
80 openat(AT_FDCWD</o/s/0001-h-kill-4/work>, \"x\", O_RDONLY) = 3</o/s/0001-h-kill-4/work/x>
80 openat(AT_FDCWD</o/s/0001-h-kill-4/work>, \".forklift/pallets/main.tmp~70-0\", O_WRONLY|O_CREAT) = ?
").unwrap();
        assert!(matches!(landing(&child, &target, &injected, None), Landing::NotReached { .. }), "a child's call is never the target");

        // Another worker staging a sibling object of the same base stands for the targeted temp.
        let object = point("openat", Aim::Position, 1, &[".forklift/objects/ab/0123456789abcdef0123.tmp~<pid>-<n>"], "O_WRONLY|O_CREAT");
        let sibling = trace::parse("\
70 execve(\"/b/forklift\", [], 0x1) = 0
70 clone(child_stack=0x1, flags=CLONE_VM|CLONE_THREAD) = 73
73 openat(AT_FDCWD</o/s/0001-h-kill-4/work>, \".forklift/objects/cd/fedcba9876543210fedc.tmp~70-5\", O_WRONLY|O_CREAT) = ?
").unwrap();
        assert_eq!(landing(&sibling, &object, &injected, None), Landing::Landed);
        let fixed = point("openat", Aim::Path { path: ".forklift/objects/ab/0123456789abcdef0123".into() }, 1, &[".forklift/objects/ab/0123456789abcdef0123"], "O_WRONLY|O_CREAT");
        let other = trace::parse("73 openat(AT_FDCWD</o/s/0001-h-kill-4/work>, \".forklift/objects/cd/fedcba9876543210fedc\", O_WRONLY|O_CREAT) = ?\n").unwrap();
        assert!(matches!(landing(&other, &fixed, &injected, None), Landing::NotReached { .. }), "a fixed path is its own target");

        let rebased = point("fsync", Aim::Path { path: "/o/s/0001-h-count/work/.forklift/pallets".into() }, 1, &["<root>/work/.forklift/pallets"], "");
        let aimed = trace::parse("71 fsync(5</o/s/0001-h-kill-4/work/.forklift/pallets>) = ?\n").unwrap();
        assert_eq!(landing(&aimed, &rebased, &injected, Some(137)), Landing::Landed);
        assert!(matches!(landing(&[], &rebased, &injected, Some(137)), Landing::NotReached { why } if why == "no call of the name was killed"));
    }

    /// A file names the root only where the root ends a path component, so a sibling copy whose
    /// name extends the root is no hit.
    ///
    /// Revert: match a plain substring → every copy's own `…-kill-3` name reads as path-bound.
    #[test]
    fn a_recorded_root_is_found_at_component_boundaries() {
        assert!(holds_root(b"path = \"/o/s/0001-h/work\"", b"/o/s/0001-h"));
        assert!(holds_root(b"/o/s/0001-h", b"/o/s/0001-h"));
        assert!(!holds_root(b"/o/s/0001-h-kill-3/work", b"/o/s/0001-h"));
        assert!(!holds_root(b"/o/s/0001-hx", b"/o/s/0001-h"));
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("proof-load-kill-scan-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/bay"), b"root=/o/s/0001-h/work\n").unwrap();
        std::fs::write(dir.join("a/other"), b"/o/s/0001-h-count").unwrap();
        assert_eq!(recorded_root(&dir, &["/o/s/0001-h"]).unwrap().as_deref(), Some("a/b/bay"));
        assert_eq!(recorded_root(&dir, &["/nowhere"]).unwrap(), None);
    }

    fn killed(name: &str, base: &str, landed: bool, sampled: bool) -> Killed {
        let mut point = point(name, Aim::Position, 1, &[], "");
        point.base = base.into();
        point.sampled = sampled;
        let graded = Graded {
            grade: crate::oracle::Grade::Healed, evidence: Default::default(), recoverability: Default::default(),
            steps: vec![], readbacks: vec![], unextractable: vec![],
        };
        Killed {
            point,
            landing: if landed { Landing::Landed } else { Landing::NotReached { why: "stolen".into() } },
            graded: landed.then_some(graded), attempts: 1,
        }
    }

    /// A unit is covered by any landed point of it; one whose every point went unreached is
    /// missing, a scenario with no point at all is missing, a not-run scenario fails nothing, and a
    /// landed sampled point makes the cell sampled and notes its row.
    ///
    /// Revert: cover a unit by any point, landed or not → the unreached fsync unit goes unreported.
    #[test]
    fn a_unit_no_kill_landed_on_is_missing() {
        let kills = vec![
            ScenarioKills { scenario: "history".into(), killed: vec![
                killed("openat", "<root>/a", false, false), killed("openat", "<root>/a", true, true), killed("fsync", "<root>/b", false, false),
            ], ..Default::default() },
            ScenarioKills { scenario: "quiet".into(), ..Default::default() },
            ScenarioKills { scenario: "lifted".into(), not_run: Some("hosted".into()), ..Default::default() },
        ];
        let summary = summarize(&kills);
        assert_eq!(summary.cell, Cell::Sampled);
        assert_eq!(summary.not_reached, 2);
        assert_eq!(summary.rows.len(), 1);
        assert_eq!(summary.rows[0].note.as_deref(), Some("sampled"));
        let units = &summary.completeness[0];
        assert_eq!(units.missing, vec![
            "history: stack fsync <root>/b: no kill landed".to_string(), "quiet: the faulted command made no call to kill".to_string(),
        ]);
        let lifted = summary.completeness.iter().find(|check| check.check == "kill points of lifted").unwrap();
        assert!(lifted.missing.is_empty() && matches!(lifted.cell, Cell::NotRun { .. }));
    }

    /// A write to anonymous memory is no state and refuses nothing, and a scratch area whose own
    /// name looks like a temp's pid and count leaves a fixed path aimed by path.
    ///
    /// Revert: count /memfd: as a file → the scenario refuses as writing outside its area; judge
    /// temps on the full path → the fixed path is aimed by position.
    #[test]
    fn neither_anonymous_memory_nor_the_root_name_misleads() {
        let memfd = trace::parse("50 execve(\"/b\", [], 0x1) = 0\n50 write(3</memfd:x>, \"a\", 1) = 1\n").unwrap();
        assert!(enumerate(&memfd, 0, "load", &root(), "/o/s/0001-h-count/work", |_| false).unwrap().points.is_empty());
        let named = Root { given: "/o/run-50-1".into(), canonical: "/o/run-50-1".into() };
        let fixed = trace::parse("50 execve(\"/b\", [], 0x1) = 0\n50 openat(AT_FDCWD</o/run-50-1/work>, \"/o/run-50-1/work/a\", O_WRONLY) = 3</o/run-50-1/work/a>\n").unwrap();
        let points = enumerate(&fixed, 0, "load", &named, "/o/run-50-1/work", |_| false).unwrap().points;
        assert!(matches!(points[0].aim, Aim::Path { .. }), "{:?}", points[0].aim);
    }

    /// A summary with no graded kill never reads as run: no scenario, every scenario not run, or
    /// every point unreached leaves the cell not run, and with nothing run the units check misses.
    ///
    /// Revert: keep the cell Run whatever ran → each case below reads as run.
    #[test]
    fn a_summary_with_no_graded_kill_is_not_run() {
        for kills in [
            vec![],
            vec![ScenarioKills { scenario: "lifted".into(), not_run: Some("hosted".into()), ..Default::default() }],
            vec![ScenarioKills { scenario: "history".into(), killed: vec![killed("fsync", "<root>/b", false, false)], ..Default::default() }],
        ] {
            let summary = summarize(&kills);
            assert!(matches!(summary.cell, Cell::NotRun { .. }), "{:?}", summary.cell);
            assert!(!summary.completeness[0].missing.is_empty());
        }
    }
}
