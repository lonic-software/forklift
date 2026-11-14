//! Server kill points (Linux): the host's forklift-server killed at each mutating or syncing call
//! it makes while a hosted scenario's faulted command runs, then the server restarted and the
//! fault graded through the host, as a network fault is.
//!
//! The server is never traced from its start. Each run builds the scenario's history on a fresh
//! scratch area with its own host, attaches strace to the listening server, and runs the faulted
//! command through the proxy, which forwards one request at a time. The counting run attaches a
//! plain trace before the faulted command; every mutating or syncing call in it is a kill point,
//! enumerated as a client's are, every thread the server already had counted as its own, and
//! positioned among the calls made since its unit began. Each point's unit is the request whose
//! window its call fell in, by the route the table resolves it to, or the server's background work
//! when it fell in none. An injected run attaches a trace that kills the server at the point, the
//! moment its unit begins: just before the proxy forwards the point's request, or before the
//! faulted command for a background point; the run counts only when the server was killed and its
//! own trace shows the kill on the point's target. The server is then restarted through
//! [`Host::restart`], whose remedies are the outside knowledge the grade records, and the record is
//! read back through a fresh clone of the host.
//!
//! A fresh build per run, not a copy: a copy's work tree names the URL of the server it was copied
//! beside. The server's handlers run on pooled threads, so a point another thread can reach first
//! is "sampled", and one it keeps reaching first stays "not reached" after its attempts. A hosted
//! scenario whose faulted command only reads from the server owes no point.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::corpus::Corpus;
use crate::damage::Warehouse;
use crate::faulted::{self, Context, Programs};
use crate::host::{Host, Restarted};
use crate::kill::{self, Aim, Killed, Landing, Point, ScenarioKills, Settings, ATTEMPTS};
use crate::proxy::{self, Exchange};
use crate::routes::{Mode, RouteTable};
use crate::runner::{self, RunError};
use crate::scenario::{Scenario, Tool};
use crate::scrub::Scratch;
use crate::trace::{self, Call, Root};

/// What an injected run says when the server outlived the faulted command and its settling time.
pub const NOT_KILLED: &str = "the server was never killed: no call the injection counts reached its position";

/// How long past a point's moment in the counting run an injected run waits for the server's
/// background work to reach it, at most.
const SETTLE: Duration = Duration::from_secs(2);
const SETTLE_CAP: Duration = Duration::from_secs(30);

/// A hosted scenario's faulted command counted once against a traced server.
pub struct Counted {
    corpus: Corpus,
    expected: faulted::Expected,
    faulted: usize,
    /// The counting run's scratch root, which absolute aims are rebased from.
    root: Root,
    /// Each point; the request, by its number among the faulted command's, whose window held its
    /// call, when one did; and how long after the faulted command's last answered request the
    /// call came.
    points: Vec<(Point, Option<usize>, Duration)>,
    children: BTreeSet<String>,
    /// What refused the counting trace: an unclassified call, a write outside the scratch area.
    refusals: Vec<String>,
}

/// What counting found: the counted run, or why the scenario's server points cannot run.
pub enum Counting {
    Counted(Box<Counted>),
    NotRun(String),
}

/// One request's window in the counting run.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Window {
    /// Its number among the faulted command's requests.
    at: usize,
    from: u64,
    to: u64,
    route: String,
}

/// The window whose span holds `call`'s moment, ends included.
fn window_of<'a>(call: &Call, windows: &'a [Window]) -> Option<&'a Window> {
    let when = call.when?;
    windows.iter().find(|window| (window.from..=window.to).contains(&when))
}

/// The unit a server call belongs to, named by the route `table` resolved its request to, or the
/// background work when no window holds it; and the moment its position is counted from: its
/// window's opening, or the trace's start for the background's.
fn unit_of(call: &Call, windows: &[Window]) -> (String, u64) {
    if call.when.is_none() {
        return ("serve unstamped".to_string(), 0);
    }
    window_of(call, windows).map_or_else(|| ("serve background".to_string(), 0), |window| (format!("serve {}", window.route), window.from))
}

/// Each exchange's window and what names it, numbered in order.
fn windows(exchanges: &[Exchange], table: Option<&RouteTable>) -> Vec<Window> {
    exchanges.iter().enumerate().filter_map(|(at, exchange)| {
        let from = exchange.forwarded?;
        let route = table.and_then(|table| table.resolve(Mode::Single, &exchange.method, &exchange.path).ok().flatten()).map(|route| route.label())
            .unwrap_or_else(|| format!("{} (no listed route)", exchange.method));
        Some(Window { at, from, to: exchange.answered.unwrap_or(u64::MAX), route })
    }).collect()
}

/// Writes a server trace back where the run's traces are kept: the host removes each it reads.
fn keep(settings: &Settings, name: &str, text: &str) {
    let _ = std::fs::write(settings.traces.join(format!("{}.trace", name)), text);
}

fn host_of<'a>(scenario: &Scenario, built: &'a runner::Built) -> Result<&'a Host, RunError> {
    built.host.as_ref().ok_or_else(|| RunError::Io(format!("scenario {} hosts no warehouse", scenario.id)))
}

/// Builds `scenario` in `template` and runs its faulted command once against its host with a plain
/// trace attached, every mutating or syncing call of the server a point.
pub fn count(scenario: &Scenario, corpus: &Corpus, programs: &Programs, settings: &Settings, template: &Scratch, table: Option<&RouteTable>) -> Result<Counting, RunError> {
    if !scenario.hosts() {
        return Ok(Counting::NotRun("hosts no warehouse".to_string()));
    }
    let io = |error: std::io::Error| RunError::Io(error.to_string());
    let environment = kill::environment_of(settings, template)?;
    let mut built = runner::build(scenario, corpus, Tool::Forklift, &programs.forklift, programs.server.as_deref(), template, &environment, settings.limit, None)?;
    let corpus = scenario.corpus(corpus);
    let (expected, faulted) = faulted::expected(scenario, &corpus, &built, Warehouse::Host)?;
    let root = Root::of(template.root()).map_err(io)?;
    let label = template.root().file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let host = host_of(scenario, &built)?;
    let base = settings.traces.join(format!("{}-serve-count", label));
    if let Err(said) = host.trace_once(settings.strace, &base, &[], settings.limit) {
        return Ok(Counting::NotRun(format!("strace could not attach to the server: {}", said)));
    }
    let pid = host.pid().ok_or_else(|| RunError::Io("the counted server is not running".to_string()))?;
    let since = proxy::now();
    let context = Context {
        corpus: &corpus, programs, scratch: template, environment: &environment, limit: settings.limit, then: &scenario.then,
        url: Some(host.url.as_str()), scope: scenario.scope(), via_host: true,
    };
    let mut steps = Vec::new();
    if !context.record(faulted, &mut steps)? {
        let refused = steps.last().map(|outcome| outcome.arguments.join(" ")).unwrap_or_default();
        return Err(RunError::Io(format!("the faulted command refused against a traced server: {}", refused)));
    }
    let host = built.host.take().expect("the host was found above");
    let (exchanges, traces) = host.finish();
    let exchanges: Vec<Exchange> = exchanges.into_iter().filter(|exchange| exchange.forwarded.is_some_and(|forwarded| forwarded >= since)).collect();
    let text = traces.into_iter().next().ok_or_else(|| RunError::Io("the counting server left no trace".to_string()))?.map_err(RunError::Io)?;
    keep(settings, &format!("{}-serve-count", label), &text);
    let calls = trace::parse(&text).map_err(RunError::Io)?;
    let windows = windows(&exchanges, table);
    let last = exchanges.iter().filter_map(|exchange| exchange.answered).max().unwrap_or(since);
    let start = template.root().to_string_lossy().into_owned();
    let (enumerated, refusals) = match kill::enumerate_served(&calls, pid, &|call| unit_of(call, &windows), &root, &start) {
        Ok(enumerated) => (enumerated, Vec::new()),
        Err(refusals) => (kill::Enumerated::default(), refusals),
    };
    let points = enumerated.points.into_iter().map(|point| {
        let call = &calls[point.call];
        let after = call.when.map_or(Duration::ZERO, |when| Duration::from_micros(when.saturating_sub(last)));
        (point, window_of(call, &windows).map(|window| window.at), after)
    }).collect();
    Ok(Counting::Counted(Box::new(Counted { corpus, expected, faulted, root, points, children: enumerated.children, refusals })))
}

/// Runs every server kill point `count` found, each injected run on a fresh build beside
/// `template`.
pub fn run(scenario: &Scenario, programs: &Programs, settings: &Settings, template: &Scratch, counting: &Counting) -> ScenarioKills {
    let mut kills = ScenarioKills { scenario: scenario.id.clone(), ..Default::default() };
    let counted = match counting {
        Counting::Counted(counted) => counted,
        Counting::NotRun(reason) => {
            kills.not_run = Some(reason.clone());
            return kills;
        }
    };
    kills.children = counted.children.clone();
    if !counted.refusals.is_empty() {
        kills.refusals = counted.refusals.clone();
        return kills;
    }
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, Result<Killed, String>)>> = Mutex::new(Vec::new());
    let workers = settings.jobs.clamp(1, counted.points.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let at = next.fetch_add(1, Ordering::SeqCst);
                let Some((point, window, after)) = counted.points.get(at) else { break };
                let result = attempt(scenario, programs, settings, template, counted, point, *window, *after, at)
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
    kills
}

#[allow(clippy::too_many_arguments)]
fn attempt(scenario: &Scenario, programs: &Programs, settings: &Settings, template: &Scratch, counted: &Counted, point: &Point, window: Option<usize>, after: Duration, at: usize) -> Result<Killed, RunError> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        let mut killed = inject(scenario, programs, settings, template, counted, point, window, after, &format!("{}-{}", at, attempts))?;
        killed.attempts = attempts;
        if killed.landing == Landing::Landed || attempts == ATTEMPTS {
            return Ok(killed);
        }
    }
}

/// strace's filter for `point` in a run whose scratch root is `root`: its aim, an absolute path
/// rebased from the counting run's root, and the kill.
pub fn filter(point: &Point, counting: &Root, root: &Root) -> Result<Vec<String>, String> {
    let mut filter = Vec::new();
    match &point.aim {
        Aim::Path { path } => {
            let aimed = if path.starts_with('/') {
                counting.rebase(path, root).ok_or_else(|| format!("the aim {} is outside the counting run's root", path))?
            } else {
                path.clone()
            };
            filter.extend(["-P".to_string(), aimed, "-e".to_string(), format!("trace={}", point.name)]);
        }
        Aim::Position => filter.extend(["-e".to_string(), format!("trace={},clone,clone3,fork,vfork", point.name)]),
    }
    filter.extend(["-e".to_string(), format!("inject={}:signal=KILL:when={}", point.name, point.when)]);
    Ok(filter)
}

/// Waits until the host's server ends, up to `limit`: whether it was killed. A server that ended
/// any other way crashed on its own, which is an error, never a kill that missed.
fn killed_within(host: &Host, limit: Duration) -> Result<bool, RunError> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = host.ended() {
            if !killed_by_signal(&status) {
                return Err(RunError::Io(format!("the server ended on its own ({}) while the faulted command ran", status)));
            }
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[allow(clippy::too_many_arguments)]
fn inject(scenario: &Scenario, programs: &Programs, settings: &Settings, template: &Scratch, counted: &Counted, point: &Point, window: Option<usize>, after: Duration, at: &str) -> Result<Killed, RunError> {
    let io = |error: std::io::Error| RunError::Io(error.to_string());
    let scratch = Scratch::create(Path::new(&format!("{}-kill-{}", template.root().display(), at))).map_err(|error| RunError::Io(error.to_string()))?;
    let environment = kill::environment_of(settings, &scratch)?;
    let mut built = runner::build(scenario, &counted.corpus, Tool::Forklift, &programs.forklift, programs.server.as_deref(), &scratch, &environment, settings.limit, None)?;
    let root = Root::of(scratch.root()).map_err(io)?;
    let label = scratch.root().file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let host = host_of(scenario, &built)?;
    let filter = filter(point, &counted.root, &root).map_err(RunError::Io)?;
    let pid = host.pid().ok_or_else(|| RunError::Io("the injected server is not running".to_string()))?;
    let base = settings.traces.join(&label);
    let attach = || host.trace_once(settings.strace, &base, &filter, settings.limit).map_err(RunError::Io);
    // A point in a request's window is counted from that request's start, so the trace is attached
    // just before the proxy forwards it; a background point's, before the faulted command starts.
    let (go, went) = std::sync::mpsc::channel::<()>();
    let (done, finished) = std::sync::mpsc::channel::<()>();
    match window {
        // Serial from the start, as the counting run was: requests are numbered in the order they
        // are forwarded, and the hook runs once the request before it is answered.
        Some(_) => host.proxy.set_serial(true),
        None => {}
    }
    match window {
        Some(at) => host.proxy.before(host.proxy.requests() + at, move || {
            if go.send(()).is_ok() {
                let _ = finished.recv();
            }
        }),
        None => {
            drop(go);
            attach()?;
        }
    }
    let context = Context {
        corpus: &counted.corpus, programs, scratch: &scratch, environment: &environment, limit: settings.limit, then: &scenario.then,
        url: Some(host.url.as_str()), scope: scenario.scope(), via_host: true,
    };
    let settle = (after + SETTLE).min(SETTLE_CAP);
    let mut restarted = None;
    let mut attached = Ok(());
    let graded = std::thread::scope(|scope| {
        // Attaches when the proxy reaches the point's request, then lets it go on; ends once the
        // hook is dropped unrun.
        let attach = &attach;
        let waiter = scope.spawn(move || {
            if went.recv().is_ok() {
                let attached = attach();
                let _ = done.send(());
                return attached;
            }
            Ok(())
        });
        let graded = faulted::grade_after(&context, &counted.expected, counted.faulted, |context, steps| {
            let made = context.record(counted.faulted, steps);
            host.proxy.clear_before();
            if !killed_within(host, settle)? {
                return Err(RunError::Io(NOT_KILLED.to_string()));
            }
            restarted = Some(host.restart(&scratch, &environment, settings.limit).map_err(RunError::Tool)?);
            made
        });
        host.proxy.clear_before();
        attached = waiter.join().unwrap_or_else(|_| Err(RunError::Io("the attaching thread panicked".to_string())));
        graded
    });
    attached?;
    let host = built.host.take().expect("the host was found above");
    let (_, traces) = host.finish();
    let graded = match graded {
        Err(RunError::Io(why)) if why == NOT_KILLED => {
            #[allow(clippy::disallowed_methods, reason = "a scratch area the harness made itself, under its own output directory")]
            let _ = std::fs::remove_dir_all(scratch.root());
            return Ok(Killed { point: point.clone(), landing: Landing::NotReached { why }, graded: None, attempts: 1 });
        }
        graded => graded?,
    };
    let text = traces.into_iter().next().ok_or_else(|| RunError::Io("the injected server left no trace".to_string()))?.map_err(RunError::Io)?;
    keep(settings, &label, &text);
    let calls = trace::parse(&text).map_err(RunError::Io)?;
    let landing = kill::landing_served(&calls, point, &root, pid);
    let mut graded = (landing == Landing::Landed).then_some(graded);
    if let (Some(graded), Some(Restarted { outside_knowledge, unextractable, refused })) = (graded.as_mut(), restarted) {
        for (at, step) in outside_knowledge.into_iter().enumerate() {
            graded.recoverability.outside_knowledge.insert(at, step);
        }
        for text in unextractable.into_iter().chain(refused.map(|said| format!("the restarted server refused: {}", said))) {
            if !graded.unextractable.contains(&text) {
                graded.unextractable.push(text);
            }
        }
    }
    // An area that graded well holds nothing to look at; one that did not is kept for diagnosis.
    if graded.as_ref().is_none_or(|graded| !graded.grade.is_failure()) {
        #[allow(clippy::disallowed_methods, reason = "a scratch area the harness made itself, under its own output directory")]
        let _ = std::fs::remove_dir_all(scratch.root());
    }
    Ok(Killed { point: point.clone(), landing, graded, attempts: 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(when: Option<u64>) -> Call {
        let mut call = trace::parse("7 1.000000 unlinkat(AT_FDCWD</s>, \"/s/x\", 0) = 0\n").expect("one call").remove(0);
        call.when = when;
        call
    }

    /// Only a SIGKILL is the injected kill; a server that exited, or died of another signal,
    /// crashed on its own.
    ///
    /// Revert: take any end for the kill → the panic's exit reads as killed.
    #[cfg(unix)]
    #[test]
    fn only_a_sigkill_is_the_injected_kill() {
        use std::os::unix::process::ExitStatusExt;
        assert!(killed_by_signal(&std::process::ExitStatus::from_raw(9)));
        assert!(!killed_by_signal(&std::process::ExitStatus::from_raw(101 << 8)));
        assert!(!killed_by_signal(&std::process::ExitStatus::from_raw(6)));
    }

    /// A call is the unit of the request whose window holds its moment, ends included, counted
    /// from that window's opening, and the background's outside every window, counted from the
    /// trace's start.
    ///
    /// Revert: an open-ended window → a write after the response is credited to the request;
    /// a strict end → a write at the response's moment falls to the background.
    #[test]
    fn calls_are_placed_in_their_request_window() {
        let windows = vec![
            Window { at: 0, from: 10, to: 20, route: "PUT /objects/{hash}".to_string() },
            Window { at: 1, from: 30, to: u64::MAX, route: "POST /pallets/{name}".to_string() },
        ];
        let unit = |when| unit_of(&call(when), &windows);
        assert_eq!(unit(Some(15)), ("serve PUT /objects/{hash}".to_string(), 10));
        assert_eq!(unit(Some(20)), ("serve PUT /objects/{hash}".to_string(), 10));
        assert_eq!(unit(Some(25)), ("serve background".to_string(), 0));
        assert_eq!(unit(Some(9)), ("serve background".to_string(), 0));
        assert_eq!(unit(Some(40)), ("serve POST /pallets/{name}".to_string(), 30));
        assert_eq!(unit(None), ("serve unstamped".to_string(), 0));
        assert_eq!(window_of(&call(Some(40)), &windows).map(|window| window.at), Some(1));
    }

    /// A path aim is rebased onto the injected run's root, and the kill is counted at the point's
    /// position; a positional aim follows the server's thread spawns.
    ///
    /// Revert: aim at the counting run's path → the injected server is never selected.
    #[test]
    fn the_filter_aims_at_the_injected_root() {
        let counting = Root { given: "/out/a-served".to_string(), canonical: "/out/a-served".to_string() };
        let root = Root { given: "/out/a-served-kill-0-1".to_string(), canonical: "/out/a-served-kill-0-1".to_string() };
        let point = Point {
            line: 0, call: 0, command: "serve background".to_string(), name: "renameat2".to_string(),
            aim: Aim::Path { path: "/out/a-served/host/.forklift/pallets/main".to_string() }, when: 2,
            target: kill::Identity { detail: String::new(), paths: Vec::new() }, base: String::new(), sampled: false,
        };
        assert_eq!(filter(&point, &counting, &root).expect("inside the root"), vec![
            "-P", "/out/a-served-kill-0-1/host/.forklift/pallets/main", "-e", "trace=renameat2", "-e", "inject=renameat2:signal=KILL:when=2",
        ]);
        let outside = Point { aim: Aim::Path { path: "/elsewhere/x".to_string() }, ..point.clone() };
        assert!(filter(&outside, &counting, &root).is_err());
        let positional = Point { aim: Aim::Position, ..point };
        assert_eq!(filter(&positional, &counting, &root).expect("no path")[1], "trace=renameat2,clone,clone3,fork,vfork");
    }
}

/// Whether `status` is a SIGKILL's: the injected kill's.
#[cfg(unix)]
fn killed_by_signal(status: &std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    status.signal() == Some(9)
}

#[cfg(not(unix))]
fn killed_by_signal(_: &std::process::ExitStatus) -> bool {
    false
}
