//! Progress reporting out of long-running core operations (`compact`, `lift`, `lower`,
//! `git import`). Core never prints: an operation reports into a [`Progress`] handle, and the
//! head that created the handle decides where the reports go. The CLI wires it to the
//! `PULT_EVENTS` channel; every other caller passes [`Progress::NONE`], which costs one branch.
//!
//! A report is one of two shapes, mirroring the events vocabulary the CLI writes:
//!
//! * a **status** names the phase the operation has entered;
//! * a **progress** carries a percentage (or none, when the total is unknown) and per-item text.
//!
//! The handle enforces the one ordering rule a consumer relies on: within a phase the percentage
//! never goes down. Only a status — a new phase, named — may be followed by a lower value. A
//! lower value reported mid-phase (two parallel tasks finishing out of order, say) is raised to
//! the phase's high-water mark instead of being passed on as a reset nobody announced.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One report from an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event<'a> {
    /// The operation entered the named phase.
    Status(&'a str),

    /// How far the current phase is: a percentage in 0–100, or `None` when the total is not
    /// known up front, with per-item text (possibly empty).
    Progress { percent: Option<u8>, text: &'a str },
}

/// The sink a head supplies: called synchronously, from whichever thread the operation reports
/// on, and under the handle's lock (which is what keeps reports ordered), so it must be cheap,
/// must never block, and must not report back into the same handle.
pub type Sink = dyn Fn(Event<'_>) + Send + Sync;

/// A cheap, cloneable reporting handle threaded through an operation. Cloning shares the
/// phase state, so parallel tasks reporting into clones of one handle still never move the
/// percentage backwards within a phase.
#[derive(Clone)]
pub struct Progress(Option<Arc<Inner>>);

struct Inner {
    sink: Box<Sink>,

    /// How long a report that would change nothing a consumer can see (the same percentage,
    /// or another indeterminate tick) is held back before a fresh one is passed on.
    interval: Duration,

    state: Mutex<PhaseState>,
}

struct PhaseState {
    /// The highest percentage reported since the last status.
    high_water: Option<u8>,

    /// When a progress report was last passed to the sink, and what percentage it carried.
    last_sent: Option<(Instant, Option<u8>)>,

    /// The newest report held back since then, passed on before the next status (or when the
    /// last handle goes) so the phase's final count or item is never lost — only thinned. There
    /// is no timer: a held report waits for one of those, or is superseded by the next report
    /// passed on, so a consumer's text can lag through a quiet stretch.
    held: Option<(Option<u8>, String)>,
}

/// How long a repeated percentage is held back. Ten updates a second is as fast as anything
/// renders text, and keeps a per-item operation over a million items from sending a million
/// lines nobody reads.
const DEFAULT_INTERVAL: Duration = Duration::from_millis(100);

impl Progress {
    /// A handle that reports nowhere.
    pub const NONE: Progress = Progress(None);

    /// A handle reporting into `sink`.
    pub fn new(sink: impl Fn(Event<'_>) + Send + Sync + 'static) -> Progress {
        Progress::with_interval(sink, DEFAULT_INTERVAL)
    }

    /// [`Progress::new`] with an explicit hold-back interval (see [`DEFAULT_INTERVAL`]).
    pub fn with_interval(sink: impl Fn(Event<'_>) + Send + Sync + 'static,
                         interval: Duration) -> Progress {
        Progress(Some(Arc::new(Inner {
            sink: Box::new(sink),
            interval,
            state: Mutex::new(PhaseState { high_water: None, last_sent: None, held: None }),
        })))
    }

    /// Whether reports go anywhere — lets a caller skip work that only feeds a report.
    pub fn is_active(&self) -> bool {
        self.0.is_some()
    }

    /// Report entering a new phase. Starts the phase's percentage afresh, after passing on the
    /// last report of the phase ending if it was held back.
    pub fn status(&self, phase: &str) {
        let Some(inner) = &self.0 else { return };

        let mut state = lock(&inner.state);
        inner.flush_held(&mut state);
        state.high_water = None;
        state.last_sent = None;
        (inner.sink)(Event::Status(phase));
    }

    /// Report `done` of `total` items through the current phase. `text` is formatted only when
    /// reports go somewhere.
    pub fn fraction(&self, done: usize, total: usize, text: impl FnOnce() -> String) {
        let percent = if total == 0 {
            100
        } else {
            (done.min(total) as u128 * 100 / total as u128) as u8
        };

        self.report(Some(percent), text);
    }

    /// Report activity in a phase whose total is not known up front.
    pub fn indeterminate(&self, text: impl FnOnce() -> String) {
        self.report(None, text);
    }

    fn report(&self, percent: Option<u8>, text: impl FnOnce() -> String) {
        let Some(inner) = &self.0 else { return };

        // The sink is called with the state still locked: computing a value and passing it on
        // are one step, so two threads can never hand the sink their values in the opposite
        // order to the one the high-water mark saw them in.
        let mut state = lock(&inner.state);

        let percent = match (percent, state.high_water) {
            (Some(new), Some(high)) => Some(new.max(high)),
            (new, _) => new,
        };

        if let Some(value) = percent {
            state.high_water = Some(value);
        }

        let now = Instant::now();
        let unchanged = match state.last_sent {
            Some((at, sent)) => sent == percent && now.duration_since(at) < inner.interval,
            None => false,
        };

        let text = text();

        if unchanged {
            state.held = Some((percent, text));
            return;
        }

        state.last_sent = Some((now, percent));
        state.held = None;
        (inner.sink)(Event::Progress { percent, text: &text });
    }
}

impl Inner {
    /// Pass on the report held back since the last one sent, if there is one.
    fn flush_held(&self, state: &mut PhaseState) {
        if let Some((percent, text)) = state.held.take() {
            (self.sink)(Event::Progress { percent, text: &text });
        }
    }
}

impl Drop for Inner {
    /// The operation is over (every clone of the handle is gone): its last report must not stay
    /// held back.
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((percent, text)) = state.held.take() {
            (self.sink)(Event::Progress { percent, text: &text });
        }
    }
}

/// Lock a mutex, recovering the guard if a reporting thread panicked while holding it: the
/// state is two plain values, never left half-written.
fn lock(mutex: &Mutex<PhaseState>) -> std::sync::MutexGuard<'_, PhaseState> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a sink saw, owned.
    #[derive(Debug, PartialEq, Eq)]
    enum Seen {
        Status(String),
        Progress(Option<u8>, String),
    }

    fn recording(interval: Duration) -> (Progress, Arc<Mutex<Vec<Seen>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        let progress = Progress::with_interval(move |event| {
            // Tolerates poison: a failed assertion holding the lock must not turn the handle's
            // final flush into a second panic.
            sink_seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(match event {
                Event::Status(phase) => Seen::Status(phase.to_string()),
                Event::Progress { percent, text } => Seen::Progress(percent, text.to_string()),
            });
        }, interval);
        (progress, seen)
    }

    #[test]
    fn none_reports_nothing_and_never_formats() {
        let progress = Progress::NONE;
        assert!(!progress.is_active());
        progress.status("phase");
        progress.fraction(1, 2, || panic!("text formatted for an inactive handle"));
        progress.indeterminate(|| panic!("text formatted for an inactive handle"));
    }

    #[test]
    fn fraction_is_floored_and_bounded() {
        let (progress, seen) = recording(Duration::ZERO);
        progress.fraction(1, 3, String::new);
        progress.fraction(2, 3, String::new);
        progress.fraction(7, 3, String::new);
        assert_eq!(*seen.lock().unwrap(), vec![
            Seen::Progress(Some(33), String::new()),
            Seen::Progress(Some(66), String::new()),
            Seen::Progress(Some(100), String::new()),
        ]);
    }

    #[test]
    fn an_empty_total_is_complete() {
        let (progress, seen) = recording(Duration::ZERO);
        progress.fraction(0, 0, String::new);
        assert_eq!(*seen.lock().unwrap(), vec![Seen::Progress(Some(100), String::new())]);
    }

    #[test]
    fn a_lower_value_mid_phase_is_raised_to_the_high_water_mark() {
        let (progress, seen) = recording(Duration::ZERO);
        progress.fraction(50, 100, || "a".to_string());
        progress.fraction(40, 100, || "b".to_string());
        assert_eq!(*seen.lock().unwrap(), vec![
            Seen::Progress(Some(50), "a".to_string()),
            Seen::Progress(Some(50), "b".to_string()),
        ]);
    }

    #[test]
    fn a_status_allows_the_next_value_to_be_lower() {
        let (progress, seen) = recording(Duration::ZERO);
        progress.fraction(90, 100, String::new);
        progress.status("next");
        progress.fraction(10, 100, String::new);
        assert_eq!(*seen.lock().unwrap(), vec![
            Seen::Progress(Some(90), String::new()),
            Seen::Status("next".to_string()),
            Seen::Progress(Some(10), String::new()),
        ]);
    }

    #[test]
    fn a_repeated_percentage_inside_the_interval_is_held_back() {
        let (progress, seen) = recording(Duration::from_secs(3600));
        progress.fraction(1, 100, || "first".to_string());
        progress.fraction(1, 100, || "held".to_string());
        progress.fraction(2, 100, || "second".to_string());
        assert_eq!(*seen.lock().unwrap(), vec![
            Seen::Progress(Some(1), "first".to_string()),
            Seen::Progress(Some(2), "second".to_string()),
        ]);
    }

    #[test]
    fn a_repeated_percentage_past_the_interval_is_passed_on() {
        let (progress, seen) = recording(Duration::ZERO);
        progress.fraction(1, 100, || "first".to_string());
        progress.fraction(1, 100, || "again".to_string());
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn indeterminate_ticks_are_held_back_like_a_repeated_percentage() {
        let (progress, seen) = recording(Duration::from_secs(3600));
        progress.indeterminate(|| "one".to_string());
        progress.indeterminate(|| "two".to_string());
        assert_eq!(*seen.lock().unwrap(), vec![Seen::Progress(None, "one".to_string())]);
    }

    #[test]
    fn the_last_held_back_report_is_passed_on_before_the_next_status() {
        let (progress, seen) = recording(Duration::from_secs(3600));
        progress.indeterminate(|| "1 walked".to_string());
        progress.indeterminate(|| "2 walked".to_string());
        progress.indeterminate(|| "3 walked".to_string());
        progress.status("next");
        assert_eq!(*seen.lock().unwrap(), vec![
            Seen::Progress(None, "1 walked".to_string()),
            Seen::Progress(None, "3 walked".to_string()),
            Seen::Status("next".to_string()),
        ]);
    }

    #[test]
    fn the_last_held_back_report_is_passed_on_when_the_last_handle_goes() {
        let (progress, seen) = recording(Duration::from_secs(3600));
        let clone = progress.clone();
        progress.fraction(50, 100, || "a".to_string());
        clone.fraction(50, 100, || "b".to_string());

        drop(progress);
        assert_eq!(seen.lock().unwrap().len(), 1, "flushed while a clone still reports");

        drop(clone);
        assert_eq!(seen.lock().unwrap()[1], Seen::Progress(Some(50), "b".to_string()));
    }

    #[test]
    fn a_report_passed_on_supersedes_the_held_one() {
        let (progress, seen) = recording(Duration::from_secs(3600));
        progress.fraction(1, 100, || "first".to_string());
        progress.fraction(1, 100, || "held".to_string());
        progress.fraction(2, 100, || "second".to_string());
        progress.status("next");
        assert_eq!(seen.lock().unwrap().len(), 3, "{:?}", seen.lock().unwrap());
    }

    #[test]
    fn a_status_is_never_held_back_and_releases_the_next_report() {
        let (progress, seen) = recording(Duration::from_secs(3600));
        progress.fraction(5, 100, String::new);
        progress.status("a");
        progress.status("a");
        progress.fraction(5, 100, String::new);
        assert_eq!(*seen.lock().unwrap(), vec![
            Seen::Progress(Some(5), String::new()),
            Seen::Status("a".to_string()),
            Seen::Status("a".to_string()),
            Seen::Progress(Some(5), String::new()),
        ]);
    }

    #[test]
    fn clones_share_one_phase() {
        let (progress, seen) = recording(Duration::ZERO);
        let clone = progress.clone();
        progress.fraction(60, 100, String::new);
        clone.fraction(30, 100, String::new);
        assert_eq!(seen.lock().unwrap()[1], Seen::Progress(Some(60), String::new()));
    }
}
