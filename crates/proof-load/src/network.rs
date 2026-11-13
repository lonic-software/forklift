//! Network faults: one fault of each kind the proxy injects, at a seeded one of the requests the
//! faulted command of a hosted scenario makes, each on a fresh build. The faulted command's
//! requests are counted first by running it once with nothing armed.
//!
//! The readback is through a fresh clone of the host: what a network fault can lose is what the
//! client was told the host holds. A server the fault killed is restarted before anything is read
//! back, as an operator restarts a crashed server; what the restart had to remove first is
//! outside knowledge, because the restarted server named it and the user would have had to act on it.

use std::time::Duration;

use crate::corpus::Corpus;
use crate::damage::Warehouse;
use crate::faulted::{self, Context, Graded, Programs};
use crate::host::{Host, Restarted};
use crate::proxy::{Exchange, Fault};
use crate::runner::{self, RunError};
use crate::scenario::{Scenario, Tool};
use crate::scrub::{Environment, Scratch};

/// Why a point was refused as not run: the faulted command made fewer requests than counted, so
/// the fault was never reached.
pub const NOT_REACHED: &str = "the faulted command made fewer requests than it did when counted, and the armed fault was never reached: it did not take";

/// One fault at one of the faulted command's requests: the `at`-th, from 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Point {
    pub fault: Fault,
    pub at: usize,
}

/// The points a scenario whose faulted command makes `requests` requests runs at: one per fault
/// kind, at a request the seed picks, the same for the same seed, scenario and count.
pub fn points(requests: usize, seed: u64, scenario: &str) -> Vec<Point> {
    if requests == 0 {
        return Vec::new();
    }
    Fault::ALL.iter().map(|&fault| {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&seed.to_le_bytes());
        hasher.update(scenario.as_bytes());
        hasher.update(fault.label().as_bytes());
        let drawn = u64::from_le_bytes(hasher.finalize().as_bytes()[..8].try_into().expect("eight bytes"));
        Point { fault, at: (drawn % requests as u64) as usize }
    }).collect()
}

/// One network fault and what grading it established.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Networked {
    pub scenario: String,
    pub point: Point,
    /// The exchange the fault fired on.
    pub exchange: Exchange,
    /// Whether the server was restarted after the fault killed it.
    pub restarted: bool,
    pub graded: Graded,
}

impl Networked {
    /// The row label: the scenario, the fault and the request it fired on.
    pub fn label(&self) -> String {
        format!("{} {} at request {} ({} {})", self.scenario, self.point.fault.label(), self.point.at, self.exchange.method, self.exchange.path)
    }
}

fn host_of<'a>(scenario: &Scenario, built: &'a runner::Built) -> Result<&'a Host, RunError> {
    built.host.as_ref().ok_or_else(|| RunError::Io(format!("scenario {} hosts no warehouse, so its faulted command crosses no network", scenario.id)))
}

/// Builds `scenario` and runs its faulted command once with nothing armed, counting the requests
/// it makes through the proxy. A faulted command that refuses, or makes none, is an error: there
/// is no fault to place in it.
pub fn count(scenario: &Scenario, corpus: &Corpus, programs: &Programs, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<usize, RunError> {
    let built = runner::build(scenario, corpus, Tool::Forklift, &programs.forklift, programs.server.as_deref(), scratch, environment, limit, None)?;
    let host = host_of(scenario, &built)?;
    let corpus = &scenario.corpus(corpus);
    let (_, index) = faulted::expected(scenario, corpus, &built, Warehouse::Host)?;
    let context = Context {
        corpus, programs, scratch, environment, limit, then: &scenario.then, url: Some(host.url.as_str()),
        scope: scenario.scope(), via_host: true,
    };
    let before = host.proxy.requests();
    let mut steps = Vec::new();
    if !context.record(index, &mut steps)? {
        let refused = steps.last().map(|outcome| outcome.arguments.join(" ")).unwrap_or_default();
        return Err(RunError::Io(format!("the faulted command refused with no fault armed: {}", refused)));
    }
    let requests = host.proxy.requests() - before;
    if requests == 0 {
        return Err(RunError::Io(format!("the faulted command of {} made no request through the host", scenario.id)));
    }
    Ok(requests)
}

/// Builds `scenario` and grades its faulted command with `point` armed: the fault fires once, on
/// the request it names, and a server it killed is restarted before the readback.
pub fn network(scenario: &Scenario, corpus: &Corpus, programs: &Programs, scratch: &Scratch, environment: &Environment, limit: Duration, point: Point) -> Result<Networked, RunError> {
    let built = runner::build(scenario, corpus, Tool::Forklift, &programs.forklift, programs.server.as_deref(), scratch, environment, limit, None)?;
    let host = host_of(scenario, &built)?;
    let corpus = &scenario.corpus(corpus);
    let (expected, index) = faulted::expected(scenario, corpus, &built, Warehouse::Host)?;
    let context = Context {
        corpus, programs, scratch, environment, limit, then: &scenario.then, url: Some(host.url.as_str()),
        scope: scenario.scope(), via_host: true,
    };
    let mut fired = None;
    let mut restarted = None;
    let mut graded = faulted::grade_after(&context, &expected, index, |context, steps| {
        host.proxy.arm(point.fault, point.at);
        let made = context.record(index, steps);
        let unreached = host.proxy.disarm();
        // A killed server is restarted whatever the command made of it, before anything is read.
        if host.killed() {
            restarted = Some(host.restart(scratch, environment, limit).map_err(RunError::Tool)?);
        }
        let made = made?;
        if unreached {
            return Err(RunError::Io(NOT_REACHED.to_string()));
        }
        fired = host.proxy.fired();
        Ok(made)
    })?;
    let exchange = fired.ok_or_else(|| RunError::Io("the armed fault was disarmed and never fired".to_string()))?;
    let restarted_any = restarted.is_some();
    if let Some(Restarted { outside_knowledge, unextractable }) = restarted {
        for (at, step) in outside_knowledge.into_iter().enumerate() {
            graded.recoverability.outside_knowledge.insert(at, step);
        }
        for text in unextractable {
            if !graded.unextractable.contains(&text) {
                graded.unextractable.push(text);
            }
        }
    }
    Ok(Networked { scenario: scenario.id.clone(), point, exchange, restarted: restarted_any, graded })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One point per fault kind, each inside the requests counted, the same for the same inputs
    /// and drawn again for another seed; none when nothing was counted.
    ///
    /// Revert: draw without the seed → two seeds give the same points; draw past the count → an
    /// index the fault can never reach.
    #[test]
    fn points_are_seeded_inside_the_count() {
        let drawn = points(5, 1, "lifted");
        assert_eq!(drawn.iter().map(|point| point.fault).collect::<Vec<_>>(), Fault::ALL.to_vec());
        assert!(drawn.iter().all(|point| point.at < 5));
        assert_eq!(drawn, points(5, 1, "lifted"));
        assert!((2..20).any(|seed| points(5, seed, "lifted") != drawn));
        assert!(points(0, 1, "lifted").is_empty());
        assert!(points(1, 1, "lifted").iter().all(|point| point.at == 0));
    }
}
