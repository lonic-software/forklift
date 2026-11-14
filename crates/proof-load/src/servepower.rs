//! Server power loss (Linux): what the host's warehouse could hold after its machine lost power at
//! any moment while a hosted scenario's faulted command ran, rebuilt from the server's own trace and
//! graded through the host, as a server kill is.
//!
//! The server never exits, so every response it writes is a crash point. Each run builds the
//! scenario's history on a fresh scratch area with its own host, scans the host's warehouse as the
//! idle server holds it, attaches a trace that dumps every write to the listening server, and runs
//! the faulted command through the proxy, one request at a time. One run goes to its end, then waits
//! for the server's background work before the server is killed: it counts the faulted command's
//! requests. Every other run is cut at one of them: the server is killed once its answer to that
//! request is read, none of the answer delivered. A run that never reaches its cut, because the
//! faulted command made fewer requests this time, is retried, and stays "not reached" after its
//! attempts.
//!
//! A run's trace is replayed over the warehouse it scanned, by [`power`]'s model, every thread the
//! server already had counted as its own. Its crash states lie between the answer to the request
//! before its cut, or the trace's start for the first, and the server's death: before the server's
//! first call past that answer, before every sync it made after it, and as it died. Together the
//! runs reach every moment the server passes through. Each state is graded with the steps its own
//! run took, acknowledged when the faulted command reported its record made. A run whose replay
//! does not end at the warehouse the dead server left, path by path, refuses the scenario, as does
//! a call the model does not understand or a write outside the warehouse, which no state holds.
//!
//! Each state is graded on its run's own area: the area as the run left it, restored at its own
//! path so every URL and path it records still holds, the warehouse replaced by the state, then
//! the server restarted through [`Host::restart`], whose remedies are the outside knowledge the
//! grade records. The model's caveats hold here as they do for a client: every row says so. As a
//! client's fixture is, the warehouse the build left is taken as written and synced whole: a loss
//! of what the history's own requests left unsynced is not this lane's to find.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use crate::corpus::Corpus;
use crate::damage::Warehouse;
use crate::faulted::{self, Context, Expected, Graded, Programs};
use crate::host::Host;
use crate::kill::{self, Settings, ATTEMPTS};
use crate::power::{self, Collector, Crash, GradedCrash, Image, Model, Moment, Replay, Requested, ScenarioPower};
use crate::proxy::{Exchange, Fault};
use crate::routes::RouteTable;
use crate::runner::{self, RunError};
use crate::scenario::{Scenario, Tool};
use crate::scrub::{Environment, Scratch};
use crate::servekill;
use crate::tool::Outcome;
use crate::trace::{self, Call, Returned, Root};

/// How long the run to its end waits past its last answer for the server's background work.
const SETTLE: Duration = Duration::from_secs(2);

/// One run, traced and ended, its server dead and its area as the run left it.
struct Traced {
    scratch: Scratch,
    environment: Environment,
    host: Host,
    expected: Expected,
    faulted: usize,
    /// What the faulted command ran, and whether it reported its record made.
    steps: Vec<Outcome>,
    made: bool,
    /// The faulted command's requests, in the order the proxy forwarded them.
    exchanges: Vec<Exchange>,
    calls: Vec<Call>,
    pid: u32,
    /// The warehouse as the idle server held it, before the trace was attached.
    fixture: Image,
    area: Root,
    /// The warehouse as the dead server left it.
    left: Image,
}

/// What a run came to.
enum Ran {
    Traced(Box<Traced>),
    /// The faulted command made fewer requests than the cut: the run proves nothing.
    NotReached(String),
}

/// The run's area, removed: one that graded well holds nothing to look at.
fn discard(scratch: &Scratch) {
    let _ = power::remove_tree(scratch.root());
}

/// Builds `scenario` beside `template` and runs its faulted command once against its host with a
/// dumping trace attached; cut, when given, at the request of that number among the faulted
/// command's, counted from 0.
#[allow(clippy::too_many_arguments)]
fn traced(scenario: &Scenario, corpus: &Corpus, programs: &Programs, settings: &Settings, template: &Scratch, cut: Option<usize>, label: &str) -> Result<Ran, RunError> {
    let io = |error: std::io::Error| RunError::Io(error.to_string());
    let scratch = Scratch::create(Path::new(&format!("{}-{}", template.root().display(), label))).map_err(|error| RunError::Io(error.to_string()))?;
    let environment = kill::environment_of(settings, &scratch)?;
    let mut built = runner::build(scenario, corpus, Tool::Forklift, &programs.forklift, programs.server.as_deref(), &scratch, &environment, settings.limit, None)?;
    let (expected, faulted) = faulted::expected(scenario, corpus, &built, Warehouse::Host)?;
    let host = built.host.take().ok_or_else(|| RunError::Io(format!("scenario {} hosts no warehouse", scenario.id)))?;
    // The server is idle between its build and the faulted command, so this is what it holds.
    let fixture = power::scan(host.root()).map_err(io)?;
    let area = Root::of(host.root()).map_err(io)?;
    let name = scratch.root().file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let dumps = ["-e".to_string(), "write=all".to_string()];
    host.trace_once(settings.strace, &settings.traces.join(&name), &dumps, settings.limit)
        .map_err(|said| RunError::Io(format!("strace could not attach to the server: {}", said)))?;
    let pid = host.pid().ok_or_else(|| RunError::Io("the traced server is not running".to_string()))?;
    let first = host.proxy.requests();
    if let Some(at) = cut {
        host.proxy.arm(Fault::Crash, at);
    }
    let context = Context {
        corpus, programs, scratch: &scratch, environment: &environment, limit: settings.limit, then: &scenario.then,
        url: Some(host.url.as_str()), scope: scenario.scope(), via_host: true,
    };
    let mut steps = Vec::new();
    let made = context.record(faulted, &mut steps)?;
    if cut.is_none() {
        if !made {
            let refused = steps.last().map(|outcome| outcome.arguments.join(" ")).unwrap_or_default();
            return Err(RunError::Io(format!("the faulted command refused against a traced server: {}", refused)));
        }
        std::thread::sleep(SETTLE);
    }
    if let Some(status) = host.ended() {
        return Err(RunError::Io(format!("the server ended on its own ({}) while the faulted command ran", status)));
    }
    let unreached = cut.is_some() && host.proxy.disarm();
    let traces = host.end_traces();
    let exchanges: Vec<Exchange> = host.proxy.log().into_iter().filter(|exchange| exchange.index >= first).collect();
    if let (true, Some(at)) = (unreached, cut) {
        discard(&scratch);
        return Ok(Ran::NotReached(format!("the faulted command made {} requests this time, never request {}", exchanges.len(), at + 1)));
    }
    let text = traces.into_iter().next().ok_or_else(|| RunError::Io("the traced server left no trace".to_string()))?.map_err(RunError::Io)?;
    servekill::keep(settings, &name, &text);
    let calls = trace::parse(&text).map_err(RunError::Io)?;
    let left = power::scan(host.root()).map_err(io)?;
    Ok(Ran::Traced(Box::new(Traced { scratch, environment, host, expected, faulted, steps, made, exchanges, calls, pid, fixture, area, left })))
}

/// The moment a run's crash states start from: the answer to the request before its cut, or to
/// the last request for the run to its end; the trace's start when there is none.
fn opening(exchanges: &[Exchange], cut: Option<usize>) -> Result<u64, String> {
    let before = match cut {
        Some(0) => return Ok(0),
        Some(at) => at - 1,
        None => match exchanges.len().checked_sub(1) {
            Some(last) => last,
            None => return Ok(0),
        },
    };
    exchanges.get(before).and_then(|exchange| exchange.answered).ok_or_else(|| format!("request {} was never answered before the run's end", before + 1))
}

/// The calls a run's crash states are taken before: the server's first past `from`, and every
/// sync it made past it that returned.
fn marked(calls: &[Call], from: u64) -> std::collections::BTreeSet<usize> {
    let past = |call: &Call| call.when.is_some_and(|when| when > from);
    let first = calls.iter().position(past);
    calls.iter().enumerate()
        .filter(|(_, call)| past(call) && power::SYNCS.contains(&call.name.as_str()) && matches!(call.returned, Returned::Value(_)))
        .map(|(index, _)| index).chain(first).collect()
}

/// Every distinct state a power loss can leave in a run cut at `cut`, `variants` seeded ones drawn
/// among them, and how many of each kind between were asked for; or what refuses the run.
fn states(run: &Traced, cut: Option<usize>, route: &str, umask: u32, variants: usize, seed: u64) -> Result<(Vec<Crash>, Requested), Vec<String>> {
    let start = run.scratch.root().to_string_lossy().into_owned();
    // Any call the table cannot read, or any write outside the warehouse, refuses: the replay
    // would not see it.
    kill::enumerate_served(&run.calls, run.pid, &|_| ("serve".to_string(), 0), &run.area, &start)?;
    let from = opening(&run.exchanges, cut).map_err(|why| vec![why])?;
    let marked = marked(&run.calls, from);
    let mut collector = Collector::new(marked.len() + 1, variants, seed);
    let moment = |when: String| Moment::Served { request: cut, route: route.to_string(), when };
    let mut replay = Replay::new(Model::of(&run.fixture).map_err(|why| vec![why])?, run.area.clone(), umask);
    replay.attached(&run.calls, run.pid, &start, |index, model| {
        if marked.contains(&index) {
            collector.record(moment(format!("before {} at traced call {}", run.calls[index].name, index + 1)), model, run.made);
        }
    }).map_err(|why| vec![why])?;
    collector.record(moment("as the server died".to_string()), &replay.model, run.made);
    if let Some(why) = power::difference(&replay.model.volatile_image(), &run.left) {
        return Err(vec![format!("the replay does not reproduce the server's run: {}", why)]);
    }
    Ok(collector.finish())
}

/// Grades each of `crashes` on `run`'s own area, restored before each, its warehouse the state.
fn grade(run: Traced, crashes: Vec<Crash>, corpus: &Corpus, scenario: &Scenario, programs: &Programs, settings: &Settings) -> (Vec<GradedCrash>, Vec<String>) {
    let (mut graded, mut errors) = (Vec::new(), Vec::new());
    let saved = match run.scratch.copy_to(Path::new(&format!("{}-saved", run.scratch.root().display()))) {
        Ok(saved) => saved,
        Err(error) => return (graded, vec![format!("the run's area could not be saved: {}", error)]),
    };
    for (index, mut crash) in crashes.into_iter().enumerate() {
        let result = grade_one(&run, &saved, &crash, index, corpus, scenario, programs, settings);
        crash.image = std::sync::Arc::default();
        match result {
            Ok(one) => graded.push(GradedCrash { crash, graded: one }),
            Err(error) => errors.push(format!("{}: {}", crash.moment.label(), error)),
        }
    }
    discard(&saved);
    discard(&run.scratch);
    (graded, errors)
}

#[allow(clippy::too_many_arguments)]
fn grade_one(run: &Traced, saved: &Scratch, crash: &Crash, index: usize, corpus: &Corpus, scenario: &Scenario, programs: &Programs, settings: &Settings) -> Result<Graded, RunError> {
    let io = |error: std::io::Error| RunError::Io(error.to_string());
    power::remove_tree(run.scratch.root()).map_err(io)?;
    let restored = saved.copy_to(run.scratch.root()).map_err(|error| RunError::Io(error.to_string()))?;
    power::materialize(&crash.image, run.host.root()).map_err(io)?;
    let context = Context {
        corpus, programs, scratch: &restored, environment: &run.environment, limit: settings.limit, then: &scenario.then,
        url: Some(run.host.url.as_str()), scope: scenario.scope(), via_host: true,
    };
    let mut restarted = None;
    let graded = faulted::grade_after(&context, &run.expected, run.faulted, |_, first| {
        first.extend(run.steps.iter().cloned());
        restarted = Some(run.host.restart(&restored, &run.environment, settings.limit).map_err(RunError::Tool)?);
        Ok(crash.acknowledged)
    });
    run.host.kill();
    let mut graded = graded?;
    if let Some(restarted) = restarted {
        servekill::merge(&mut graded, restarted);
    }
    // A state that did not grade well is kept for diagnosis, beside the run's area.
    if graded.grade.is_failure() {
        let _ = restored.copy_to(Path::new(&format!("{}-state-{}", run.scratch.root().display(), index)));
    }
    Ok(graded)
}

/// How many of `variants` seeded states each of `runs` runs draws: as evenly as they divide, the
/// earlier runs taking what is left over.
fn shares(variants: usize, runs: usize) -> Vec<usize> {
    (0..runs).map(|run| variants / runs + usize::from(run < variants % runs)).collect()
}

/// Grades every state a power loss of the host's machine can leave while `scenario`'s faulted
/// command runs, `variants` seeded ones among them, each run on a fresh build beside `template`.
#[allow(clippy::too_many_arguments)]
pub fn run(scenario: &Scenario, corpus: &Corpus, programs: &Programs, settings: &Settings, template: &Scratch, table: Option<&RouteTable>, variants: usize, seed: u64) -> ScenarioPower {
    let mut power = ScenarioPower { scenario: scenario.id.clone(), ..Default::default() };
    if !scenario.hosts() {
        power.not_run = Some("hosts no warehouse".to_string());
        return power;
    }
    let corpus = scenario.corpus(corpus);
    let umask = match power::harness_umask() {
        Ok(umask) => umask,
        Err(why) => {
            power.errors.push(why);
            return power;
        }
    };
    // Each scenario draws its own variants, not the same corners of every scenario's server.
    let drawn = seed ^ u64::from_le_bytes(blake3::hash(format!("server {}", scenario.id).as_bytes()).as_bytes()[..8].try_into().expect("eight bytes"));
    // The run to its end first: it says how many requests there are to cut at.
    let end = match traced(scenario, &corpus, programs, settings, template, None, "end") {
        Ok(Ran::Traced(end)) => end,
        Ok(Ran::NotReached(why)) => {
            power.errors.push(why);
            return power;
        }
        Err(error) => {
            power.errors.push(format!("the run to its end: {}", error));
            return power;
        }
    };
    let requests = end.exchanges.len();
    let shares = shares(variants, requests + 1);
    let results: Mutex<Vec<(usize, Share)>> = Mutex::new(Vec::new());
    let one = |cut: Option<usize>, run: Box<Traced>| -> Share {
        let at = cut.unwrap_or(requests);
        // The route this run's own request took: a fresh build may order its requests otherwise.
        let route = cut.and_then(|cut| run.exchanges.get(cut)).map(|exchange| servekill::route_of(exchange, table)).unwrap_or_default();
        match states(&run, cut, &route, umask, shares[at], drawn ^ at as u64) {
            Ok((crashes, requested)) => {
                let (graded, errors) = grade(*run, crashes, &corpus, scenario, programs, settings);
                Share::Graded(graded, errors, requested)
            }
            Err(refusals) => Share::Refused(refusals),
        }
    };
    // The run to its end is graded in the pool beside the cuts, as the job past the last cut.
    let end = Mutex::new(Some(end));
    let next = AtomicUsize::new(0);
    let workers = settings.jobs.clamp(1, requests + 1);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let cut = next.fetch_add(1, Ordering::SeqCst);
                if cut > requests {
                    break;
                }
                if cut == requests {
                    let end = end.lock().expect("no worker panics holding the lock").take().expect("the run to its end is graded once");
                    let outcome = one(None, end);
                    results.lock().expect("no worker panics holding the lock").push((cut, outcome));
                    continue;
                }
                let mut outcome = Share::NotReached(String::new());
                for attempt in 1..=ATTEMPTS {
                    outcome = match traced(scenario, &corpus, programs, settings, template, Some(cut), &format!("cut-{}-{}", cut, attempt)) {
                        Ok(Ran::Traced(run)) => one(Some(cut), run),
                        Ok(Ran::NotReached(why)) => Share::NotReached(format!("{} after {} attempts", why, attempt)),
                        Err(error) => Share::Failed(error.to_string()),
                    };
                    if !matches!(outcome, Share::NotReached(_)) {
                        break;
                    }
                }
                results.lock().expect("no worker panics holding the lock").push((cut, outcome));
            });
        }
    });
    let mut results = results.into_inner().expect("no worker panics holding the lock");
    results.sort_by_key(|(at, _)| *at);
    for (at, outcome) in results {
        let run = if at == requests { "the run to its end".to_string() } else { format!("the run cut at request {}", at + 1) };
        match outcome {
            Share::Graded(graded, errors, requested) => {
                power.requested.add(requested);
                power.crashes.extend(graded);
                power.errors.extend(errors);
            }
            Share::Refused(refusals) => power.refusals.extend(refusals.into_iter().map(|refusal| format!("{}: {}", run, refusal))),
            Share::NotReached(why) => power.not_reached.push(format!("{}: {}", run, why)),
            Share::Failed(error) => power.errors.push(format!("{}: {}", run, error)),
        }
    }
    power
}

/// What one run's share of the lane came to.
enum Share {
    Graded(Vec<GradedCrash>, Vec<String>, Requested),
    Refused(Vec<String>),
    NotReached(String),
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(index: usize, answered: Option<u64>) -> Exchange {
        Exchange { index, method: "POST".to_string(), path: "/v1/x".to_string(), status: Some(200), fault: None, forwarded: Some(1), answered }
    }

    /// A cut run's states start at the answer to the request before its cut, the first's at the
    /// trace's start, and the run to its end's at its last answer.
    ///
    /// Revert: start at the cut request's own answer → the work it did before answering is never
    /// a state; start every run at the trace's start → each run repeats the states before it.
    #[test]
    fn a_run_s_states_open_at_the_answer_before_its_cut() {
        let exchanges = vec![exchange(4, Some(10)), exchange(5, Some(20)), exchange(6, None)];
        assert_eq!(opening(&exchanges, Some(0)), Ok(0));
        assert_eq!(opening(&exchanges, Some(1)), Ok(10));
        assert_eq!(opening(&exchanges, Some(2)), Ok(20));
        assert!(opening(&exchanges, Some(3)).is_err(), "request 3 was never answered");
        assert_eq!(opening(&exchanges[..2], None), Ok(20));
        assert_eq!(opening(&[], None), Ok(0));
    }

    /// The states are taken before the first call past the opening and before every sync past it
    /// that returned; a sync before the opening, or one that failed, marks nothing.
    ///
    /// Revert: mark syncs from the trace's start → the sync at 1 is marked; drop the first call →
    /// the state as the opening left it is lost.
    #[test]
    fn states_are_taken_past_the_opening() {
        let text = "7 1.000001 openat(AT_FDCWD</s>, \"/s/a\", O_WRONLY|O_CREAT, 0644) = 3</s/a>\n\
                    7 1.000002 fsync(3</s/a>) = 0\n\
                    7 1.000010 write(3</s/a>, \"x\", 1) = 1\n\
                    7 1.000011 fsync(3</s/a>) = -1 EIO (Input/output error)\n\
                    7 1.000012 fdatasync(3</s/a>) = 0\n";
        let calls = trace::parse(text).expect("five calls");
        assert_eq!(marked(&calls, 1_000_005).into_iter().collect::<Vec<_>>(), vec![2, 4]);
        assert_eq!(marked(&calls, 0).into_iter().collect::<Vec<_>>(), vec![0, 1, 4]);
        assert!(marked(&calls, 2_000_000).is_empty());
    }

    /// Seeded variants are spread across every run, none lost and none drawn twice.
    ///
    /// Revert: give each run every variant → seven runs draw 28.
    #[test]
    fn variants_are_shared_across_the_runs() {
        assert_eq!(shares(4, 7), vec![1, 1, 1, 1, 0, 0, 0]);
        assert_eq!(shares(9, 4), vec![3, 2, 2, 2]);
        assert_eq!(shares(0, 3), vec![0, 0, 0]);
    }
}
