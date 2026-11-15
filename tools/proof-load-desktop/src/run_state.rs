//! A run on a worker thread, and what the Run screen knows of it. The worker calls the library's
//! `run::run` with a `Progress` that sends each event over a channel and asks for a repaint; the
//! window drains the channel into a `Meter` every frame. Stop sets the flag `cancelled` answers.
//! Nothing here draws, so the reducer is tested without egui.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;

use proof_load::report::{Category, Report};
use proof_load::run::{self, Progress, RunError, RunOptions};

use crate::widgets::Led;

/// What the run said, in the order it said it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Lane(Category, Option<usize>),
    Started(Category, String),
    Finished(Category, String, bool),
    Message(String),
    /// The run returned, or the harness failed under it; nothing follows.
    Done(Result<Report, Ended>),
}

/// How a run ended without a report: as the run itself said, or on a fault of the harness that
/// is no verdict on the tool under test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ended {
    Run(RunError),
    Internal(String),
}

/// One lane of the meter: its category, its block count where the run knew it before starting,
/// and each block it started, in order, with how it went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lane {
    pub category: Category,
    pub blocks: Option<usize>,
    pub started: Vec<(String, Led)>,
}

impl Lane {
    /// One LED per block: each started block as it stands, then one queued LED per block the
    /// lane announced and has not started.
    pub fn leds(&self) -> Vec<Led> {
        let queued = self.blocks.map_or(0, |blocks| blocks.saturating_sub(self.started.len()));
        self.started.iter().map(|(_, led)| *led).chain(std::iter::repeat_n(Led::Queued, queued)).collect()
    }
}

/// The run as far as its events have told: the lanes in the order they started, the run's
/// messages, and, once it returned, how it ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Meter {
    pub lanes: Vec<Lane>,
    pub messages: Vec<String>,
    pub ended: Option<Result<Report, Ended>>,
}

impl Meter {
    /// Takes in one event. A block finishing that never started, or in a lane that never
    /// started, is shown as it finished rather than dropped.
    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Lane(category, blocks) => match self.lanes.iter_mut().find(|lane| lane.category == category) {
                Some(lane) => lane.blocks = blocks,
                None => self.lanes.push(Lane { category, blocks, started: Vec::new() }),
            },
            Event::Started(category, block) => self.lane(category).started.push((block, Led::Running)),
            Event::Finished(category, block, failed) => {
                let led = if failed { Led::Failed } else { Led::Passed };
                let lane = self.lane(category);
                match lane.started.iter_mut().rev().find(|(name, state)| *name == block && *state == Led::Running) {
                    Some(started) => started.1 = led,
                    None => lane.started.push((block, led)),
                }
            }
            Event::Message(text) => self.messages.push(text),
            Event::Done(result) => self.ended = Some(result),
        }
    }

    fn lane(&mut self, category: Category) -> &mut Lane {
        let index = match self.lanes.iter().position(|lane| lane.category == category) {
            Some(index) => index,
            None => {
                self.lanes.push(Lane { category, blocks: None, started: Vec::new() });
                self.lanes.len() - 1
            }
        };
        &mut self.lanes[index]
    }
}

/// The `Progress` the worker runs with: every call becomes an event on the channel, followed by
/// `repaint`, and `cancelled` reads the Stop flag.
pub struct Channel {
    events: Sender<Event>,
    stop: Arc<AtomicBool>,
    repaint: Box<dyn Fn() + Send + Sync>,
}

impl Channel {
    fn send(&self, event: Event) {
        // A window that has gone away has nothing left to show; the run goes on to its end.
        let _ = self.events.send(event);
        (self.repaint)();
    }
}

impl Progress for Channel {
    fn lane(&self, category: Category, blocks: Option<usize>) {
        self.send(Event::Lane(category, blocks));
    }

    fn block_started(&self, category: Category, block: &str) {
        self.send(Event::Started(category, block.to_string()));
    }

    fn block_finished(&self, category: Category, block: &str, failed: bool) {
        self.send(Event::Finished(category, block.to_string(), failed));
    }

    fn message(&self, text: &str) {
        self.send(Event::Message(text.to_string()));
    }

    fn cancelled(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

/// A run in progress: its events, and the flag Stop sets.
pub struct Worker {
    events: Receiver<Event>,
    stop: Arc<AtomicBool>,
}

impl Worker {
    /// Starts `options` on a thread of its own. `repaint` is called after every event, so the
    /// window draws it.
    pub fn start(options: RunOptions, repaint: impl Fn() + Send + Sync + 'static) -> Worker {
        Worker::start_with(move |progress| run::run(&options, progress), repaint)
    }

    /// `start` with the run itself given, so tests can drive the channel without running lanes.
    pub fn start_with(
        body: impl FnOnce(&dyn Progress) -> Result<Report, RunError> + Send + 'static,
        repaint: impl Fn() + Send + Sync + 'static,
    ) -> Worker {
        let (sender, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let channel = Channel { events: sender, stop: stop.clone(), repaint: Box::new(repaint) };
        std::thread::spawn(move || {
            // A run that panics still ends: the window is told it was refused, and why, rather
            // than left waiting for a Done that never comes.
            let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(&channel))) {
                Ok(result) => result.map_err(Ended::Run),
                Err(panic) => Err(Ended::Internal(panicked(&*panic))),
            };
            channel.send(Event::Done(result));
        });
        Worker { events, stop }
    }

    /// Every event sent since the last call, in order, without waiting; a worker that has gone
    /// without sending `Done` ends as refused, so the window never waits on it.
    pub fn drain(&self) -> Vec<Event> {
        let mut events = Vec::new();
        loop {
            match self.events.try_recv() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !events.iter().any(|event| matches!(event, Event::Done(_))) {
                        events.push(Event::Done(Err(Ended::Internal("the run ended without a result".to_string()))));
                    }
                    break;
                }
            }
        }
        events
    }

    /// Asks the run to stop before its next block.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// The next event, waiting for it; `None` once the worker has gone.
    pub fn next(&self) -> Option<Event> {
        self.events.recv().ok()
    }
}

/// What a panic said, where it said it in words.
fn panicked(panic: &(dyn std::any::Any + Send)) -> String {
    match (panic.downcast_ref::<&str>(), panic.downcast_ref::<String>()) {
        (Some(text), _) => text.to_string(),
        (_, Some(text)) => text.clone(),
        _ => "no message".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finished(meter: &Meter, category: Category) -> Vec<Led> {
        meter.lanes.iter().find(|lane| lane.category == category).map(Lane::leds).unwrap_or_default()
    }

    /// The meter shows each lane's blocks as they start and finish, the rest of an announced
    /// lane queued, and a failure lit red.
    ///
    /// Revert: light every finished block green → the failed block reads Passed.
    #[test]
    fn the_meter_follows_the_runs_events() {
        let mut meter = Meter::default();
        meter.apply(Event::Lane(Category::DamagedFiles, Some(4)));
        meter.apply(Event::Started(Category::DamagedFiles, "a".into()));
        assert_eq!(finished(&meter, Category::DamagedFiles), [Led::Running, Led::Queued, Led::Queued, Led::Queued]);
        meter.apply(Event::Finished(Category::DamagedFiles, "a".into(), false));
        meter.apply(Event::Started(Category::DamagedFiles, "b".into()));
        meter.apply(Event::Finished(Category::DamagedFiles, "b".into(), true));
        meter.apply(Event::Started(Category::DamagedFiles, "c".into()));
        assert_eq!(finished(&meter, Category::DamagedFiles), [Led::Passed, Led::Failed, Led::Running, Led::Queued]);
        // A lane that did not say how many blocks it runs shows only the ones it started.
        meter.apply(Event::Lane(Category::Network, None));
        meter.apply(Event::Started(Category::Network, "n".into()));
        assert_eq!(finished(&meter, Category::Network), [Led::Running]);
        assert_eq!(meter.lanes.iter().map(|lane| lane.category).collect::<Vec<_>>(), [Category::DamagedFiles, Category::Network]);
    }

    /// A block's finish lights the block of that name that is running, not an earlier one of the
    /// same name, and a finish nothing announced is still shown.
    ///
    /// Revert: light the first block of that name → the earlier, already-passed block turns red.
    #[test]
    fn a_finish_lights_the_running_block_of_its_name() {
        let mut meter = Meter::default();
        for (failed, block) in [(false, "same"), (true, "same")] {
            meter.apply(Event::Started(Category::DiskFull, block.into()));
            meter.apply(Event::Finished(Category::DiskFull, block.into(), failed));
        }
        assert_eq!(finished(&meter, Category::DiskFull), [Led::Passed, Led::Failed]);
        meter.apply(Event::Finished(Category::ReadOnly, "unannounced".into(), true));
        assert_eq!(finished(&meter, Category::ReadOnly), [Led::Failed]);
    }

    /// The worker sends what the run reports, in order, ends with the run's result, and Stop
    /// reaches the run's `cancelled`.
    ///
    /// Revert: have `cancelled` answer false → the run never sees Stop and ends Ok.
    #[test]
    fn the_worker_carries_events_and_stop() {
        let (go, wait) = mpsc::channel::<()>();
        let worker = Worker::start_with(
            move |progress| {
                progress.lane(Category::DamagedFiles, Some(1));
                progress.message("built");
                wait.recv().expect("the test lets the run go on");
                if progress.cancelled() { Err(RunError::Cancelled) } else { Err(RunError::Refused("not stopped".into())) }
            },
            || {},
        );
        assert_eq!(worker.next(), Some(Event::Lane(Category::DamagedFiles, Some(1))));
        assert_eq!(worker.next(), Some(Event::Message("built".into())));
        assert!(!worker.stopping());
        worker.stop();
        go.send(()).expect("the run is waiting");
        assert_eq!(worker.next(), Some(Event::Done(Err(Ended::Run(RunError::Cancelled)))));
        assert!(worker.stopping());
    }

    /// A run that panics ends on an internal error, with the panic's message, not as a refusal,
    /// and not left running.
    ///
    /// Revert: send Done only when the body returns → the worker goes with no Done.
    #[test]
    fn a_panicking_run_ends_refused() {
        let worker = Worker::start_with(|_| panic!("index out of bounds"), || {});
        assert_eq!(worker.next(), Some(Event::Done(Err(Ended::Internal("index out of bounds".into())))));
        assert_eq!(worker.next(), None);
        let events = worker.drain();
        assert_eq!(events, [Event::Done(Err(Ended::Internal("the run ended without a result".into())))], "a gone worker reads as ended");
    }
}
