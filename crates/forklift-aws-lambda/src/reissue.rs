//! The bounded re-issue of a transaction refused for a transient reason (FORK-95 design memo,
//! claim C14, arm two).
//!
//! A transaction refused without any input being shown to have moved — contention, throttling,
//! an attempt still in progress, or an attempt whose fate is unknown — is re-issued *alone*: the
//! identical transaction under the identical request token, never a re-audit. That is O(1) work
//! whatever the history size, which is what lets it run inside an invocation window that a
//! re-audit loop could not be trusted with.
//!
//! The rules, each of which the memo states and this module is the one carrier of:
//!
//! * **One budget per client request**: at most [`MAX_ATTEMPTS`] attempts in all, whatever mix of
//!   causes spends them, with the clock starting at the first attempt — not at the first
//!   refusal, and not afresh when the cause changes.
//! * **One token per request**, never per attempt. A same-token re-issue of an attempt that
//!   landed is a no-op success; of one that did not, it is the first application; of one that is
//!   still running, it is `TransactionInProgressException`. A fresh token would apply a landed
//!   attempt a second time and report the state it wrote as the pallet having moved.
//! * **Only the wait depends on the cause.** Contention and unknown fate wait steps of 50, 100
//!   and 200 ms, each jittered down to between half and all of the step (a chosen constant, not
//!   a measured one). An attempt in progress waits on the
//!   exponential schedule DynamoDB's model recommends for it, and its last re-issue is held back
//!   until five seconds after the first attempt. No re-issue starts that could not finish within
//!   [`ARM_LIMIT`] of the first attempt.
//! * **The window is checked before each re-issue.** When the invocation deadline cannot cover
//!   the wait plus one more [`TRANSACTION_ATTEMPT_TIMEOUT`] plus [`ANSWER_RESERVE`], the loop
//!   stops at once, without sleeping, and the caller answers `503` while there is still time to
//!   answer at all. A head
//!   with no deadline (the test path) keeps the attempt cap and skips the window check.
//!
//! What the loop returns is the last attempt's outcome. A transient one there means the budget
//! is spent or unaffordable, and the caller answers `503` — never `409`, which would claim an
//! input moved.

use std::time::{Duration, Instant, SystemTime};

use forklift_core::util::config_utils::mint_uuid_v4;

use crate::store::{TransientCause, TRANSACTION_ATTEMPT_TIMEOUT};

/// The most attempts one request makes at its transaction: the first plus three re-issues.
pub const MAX_ATTEMPTS: u32 = 4;

/// No re-issue starts unless it could finish this soon after the first attempt — the four-attempt
/// timeline DynamoDB's model works through for `TransactionInProgressException`.
pub const ARM_LIMIT: Duration = Duration::from_millis(7500);

/// What a re-issue leaves of the invocation for answering after its attempt runs to the timeout:
/// building the `503` and handing it back before the runtime stops the function. Without it, an
/// attempt that ran to its timeout could end exactly at the deadline and leave the gateway to
/// answer instead. A chosen margin, generous against a response that takes milliseconds.
pub const ANSWER_RESERVE: Duration = Duration::from_millis(500);

/// The first wait after contention or an unknown fate; each later one doubles it.
const CONTENDED_BASE: Duration = Duration::from_millis(50);

/// The first wait after `TransactionInProgressException`; each later one doubles it.
const IN_PROGRESS_BASE: Duration = Duration::from_millis(500);

/// DynamoDB's model asks for at least one re-issue after this much time since the first attempt,
/// so that the attempt still in progress has had time to finish.
const IN_PROGRESS_SETTLE: Duration = Duration::from_secs(5);

/// A fresh request token: a random UUID, 36 characters — DynamoDB's limit exactly. DynamoDB
/// scopes a token to the account and region for ten minutes, across every container the function
/// runs in, so it must be unique there, which is what drawing it from the OS's random source buys
/// over anything derived from a clock, a process id or a counter that every cold start resets.
pub fn request_token() -> String {
    mint_uuid_v4()
}

/// How long to wait before re-issue number `reissue` (1-based) after an attempt refused for
/// `cause`, `elapsed` after the first attempt started. `jitter` is a fraction in `[0, 1)`.
fn wait_before(reissue: u32, cause: TransientCause, elapsed: Duration, jitter: f64) -> Duration {
    let doubling = 1 << (reissue - 1);

    match cause {
        TransientCause::Contended | TransientCause::FateUnknown => {
            // Between half and all of the step: enough spread that two requests refused by the
            // same conflict do not re-issue into each other again.
            let step = CONTENDED_BASE * doubling;
            step / 2 + step.mul_f64(jitter / 2.0)
        }
        TransientCause::InProgress => {
            let backoff = IN_PROGRESS_BASE * doubling;

            if reissue == MAX_ATTEMPTS - 1 {
                backoff.max(IN_PROGRESS_SETTLE.saturating_sub(elapsed))
            } else {
                backoff
            }
        }
    }
}

/// Whether a re-issue that waits `wait` from now, `elapsed` after the first attempt, can still
/// finish both within the arm's own limit and before the invocation deadline, with
/// [`ANSWER_RESERVE`] still left for answering.
fn affordable(elapsed: Duration, wait: Duration, now: SystemTime, deadline: Option<SystemTime>) -> bool {
    let finish = wait + TRANSACTION_ATTEMPT_TIMEOUT;

    if elapsed + finish > ARM_LIMIT {
        return false;
    }

    match deadline {
        None => true,
        Some(deadline) => now + finish + ANSWER_RESERVE <= deadline,
    }
}

/// A jitter fraction in `[0, 1)` derived from the request token and the re-issue number — no
/// random-number dependency, and distinct across the requests a conflict refuses together.
fn jitter(token: &str, reissue: u32) -> f64 {
    let mut hasher = blake3::Hasher::new();

    hasher.update(token.as_bytes());
    hasher.update(&reissue.to_le_bytes());

    let bytes: [u8; 8] = hasher.finalize().as_bytes()[..8].try_into().expect("eight bytes");
    (u64::from_le_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64
}

/// Run `attempt` under one fresh request token until it returns an outcome `transient` does not
/// classify, the budget is spent, or the next re-issue is unaffordable — and return that last
/// outcome. A fault (`Err`) ends the loop at once: it is not a reason to re-issue.
///
/// Sleeps the calling thread between attempts, which is sound only because every `Head` method
/// runs on a blocking thread (see `blocking.rs`).
pub fn reissue<T>(
    deadline: Option<SystemTime>,
    mut attempt: impl FnMut(&str) -> Result<T, String>,
    transient: impl Fn(&T) -> Option<TransientCause>,
) -> Result<T, String> {
    let token = request_token();
    let first = Instant::now();
    let mut attempts = 1;

    loop {
        let outcome = attempt(&token)?;

        let Some(cause) = transient(&outcome) else {
            return Ok(outcome);
        };

        if attempts == MAX_ATTEMPTS {
            return Ok(outcome);
        }

        let elapsed = first.elapsed();
        let wait = wait_before(attempts, cause, elapsed, jitter(&token, attempts));

        if !affordable(elapsed, wait, SystemTime::now(), deadline) {
            return Ok(outcome);
        }

        std::thread::sleep(wait);
        attempts += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZERO: Duration = Duration::ZERO;

    /// The contended schedule doubles from 50 ms and stays inside each step's upper half, so the
    /// whole tail is at most 350 ms.
    #[test]
    fn contended_waits_double_from_fifty_milliseconds_within_the_jitter_band() {
        for (reissue, step) in [(1, 50), (2, 100), (3, 200)] {
            let step = Duration::from_millis(step);

            for cause in [TransientCause::Contended, TransientCause::FateUnknown] {
                assert_eq!(wait_before(reissue, cause, ZERO, 0.0), step / 2);
                assert!(wait_before(reissue, cause, ZERO, 0.999) < step);
            }
        }
    }

    /// In progress, the waits are 500, 1000 and 2000 ms, and the last re-issue is held back until
    /// five seconds after the first attempt when the attempts so far were fast.
    #[test]
    fn in_progress_waits_back_off_and_hold_the_last_reissue_past_five_seconds() {
        let cause = TransientCause::InProgress;

        assert_eq!(wait_before(1, cause, ZERO, 0.5), Duration::from_millis(500));
        assert_eq!(wait_before(2, cause, ZERO, 0.5), Duration::from_millis(1000));
        assert_eq!(
            wait_before(3, cause, Duration::from_millis(1600), 0.5),
            Duration::from_millis(3400)
        );
        assert_eq!(wait_before(3, cause, Duration::from_secs(4), 0.5), Duration::from_secs(2));
    }

    /// A re-issue is refused when it could not finish inside the arm's limit, or before the
    /// deadline with time left to answer — both counting the wait plus one whole attempt.
    #[test]
    fn a_reissue_is_affordable_only_if_it_can_finish_before_both_limits() {
        let now = SystemTime::now();
        let wait = Duration::from_millis(200);
        let enough = now + wait + TRANSACTION_ATTEMPT_TIMEOUT + ANSWER_RESERVE;

        assert!(affordable(ZERO, wait, now, None));
        assert!(affordable(ZERO, wait, now, Some(enough)));
        assert!(!affordable(ZERO, wait, now, Some(enough - Duration::from_millis(1))));
        assert!(!affordable(ARM_LIMIT - TRANSACTION_ATTEMPT_TIMEOUT, wait, now, None));
    }

    /// Request tokens are distinct across calls and fit DynamoDB's 1–36 character limit.
    #[test]
    fn request_tokens_are_distinct_and_fit_dynamodb() {
        let tokens: std::collections::HashSet<_> = (0..1000).map(|_| request_token()).collect();

        assert_eq!(tokens.len(), 1000);
        assert!(tokens.iter().all(|token| (1..=36).contains(&token.len())));
    }

    /// The jitter fraction is in range and differs across tokens, so requests refused by the
    /// same conflict do not re-issue in lockstep.
    #[test]
    fn jitter_is_a_fraction_that_varies_by_token() {
        let fractions: Vec<f64> = (0..64).map(|_| jitter(&request_token(), 1)).collect();

        assert!(fractions.iter().all(|f| (0.0..1.0).contains(f)));
        assert!(fractions.iter().any(|f| (f - fractions[0]).abs() > 1e-9));
    }

    /// Every attempt of one request carries the same token, and the loop stops after the cap
    /// with the last transient outcome.
    #[test]
    fn one_token_spans_every_attempt_and_the_cap_is_four() {
        let mut seen = Vec::new();
        let outcome = reissue(
            None,
            |token| {
                seen.push(token.to_string());
                Ok(())
            },
            |_| Some(TransientCause::Contended),
        );

        assert!(outcome.is_ok());
        assert_eq!(seen.len(), MAX_ATTEMPTS as usize);
        assert!(seen.iter().all(|token| *token == seen[0]));
    }

    /// The budget is shared across causes: switching cause mid-request does not start a new one.
    #[test]
    fn a_change_of_cause_spends_the_same_budget() {
        let mut calls = 0;
        let _ = reissue(
            None,
            |_| {
                calls += 1;
                Ok(calls)
            },
            |calls| Some(if calls % 2 == 0 { TransientCause::FateUnknown } else { TransientCause::Contended }),
        );

        assert_eq!(calls, MAX_ATTEMPTS);
    }

    /// A fault ends the loop on the attempt that raised it.
    #[test]
    fn a_fault_is_not_reissued() {
        let mut calls = 0;
        let outcome: Result<(), String> = reissue(
            None,
            |_| {
                calls += 1;
                Err("fault".to_string())
            },
            |_| Some(TransientCause::Contended),
        );

        assert!(outcome.is_err());
        assert_eq!(calls, 1);
    }

    /// With a deadline too close to cover a wait and one more attempt, the loop answers after the
    /// first attempt without sleeping.
    #[test]
    fn a_near_deadline_stops_the_loop_without_sleeping() {
        let mut calls = 0;
        let started = Instant::now();
        let _ = reissue(
            Some(SystemTime::now() + Duration::from_millis(500)),
            |_| {
                calls += 1;
                Ok(())
            },
            |_| Some(TransientCause::Contended),
        );

        assert_eq!(calls, 1);
        assert!(started.elapsed() < Duration::from_millis(20));
    }
}
