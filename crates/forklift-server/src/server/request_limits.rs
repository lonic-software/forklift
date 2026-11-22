//! Per-caller request limits: a token bucket per resolved principal and cost class, so one caller
//! spending its budget never spends another's, and costly requests never spend the budget of cheap
//! ones. The authentication layer ([`super::authenticate`]) charges each request once its
//! principal is known and before its body is read; an empty bucket is a `429` with `Retry-After`.
//! A request its handler then refuses for now (a `503` with `Retry-After`, sent before anything
//! changed) is refunded, so a caller's budget is spent by what it sent and not by other callers'
//! load. The limits bound how fast a caller sends, not how many of its requests run at once.
//!
//! A class with no limit configured is never charged. The buckets are kept per serving process,
//! across every warehouse it serves: a principal is the server's identity for its caller, not one
//! warehouse's.

use std::collections::HashMap;
use std::sync::Mutex;

use forklift_core::util::authority_utils::Principal;
use tokio::time::Instant;

use super::Cost;

/// The fewest requests per minute a limit may allow. An empty bucket's caller is told to wait the
/// time one request takes to refill (60 s over the limit), and the client gives up on a run of
/// `429`s after 30 s (`FETCH_THROTTLE_PATIENCE` in `forklift-core`'s `remote_utils`, private
/// there, so this is stated, not asserted). At 4 a minute the wait is 15 s, half that patience,
/// so one refused request can still be admitted; a lower limit would fail it outright. It is only
/// that floor: the client sends many requests at once, each refused one is told the same wait,
/// and a limit far under what a caller sends fails its requests at any value of this.
pub(super) const MIN_REQUESTS_PER_MINUTE: u32 = 4;

/// How many buckets are held before full ones are let go. A full bucket admits exactly what a
/// missing one would, so letting it go changes no answer.
const PRUNE_AT: usize = 4096;

/// Who a bucket belongs to: the principal the request was authenticated as. Every caller of the
/// static token shares its bucket, and so does every caller of an open server.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Caller {
    Open,
    Static,
    Operator(String),
}

impl Caller {
    fn of(principal: &Principal) -> Caller {
        match principal {
            Principal::Open => Caller::Open,
            Principal::Static => Caller::Static,
            Principal::Operator(identifier) => Caller::Operator(identifier.clone()),
        }
    }
}

/// One caller's budget in one cost class: the requests it may still send now, as of `at`.
struct Bucket {
    tokens: f64,
    at: Instant,

    /// When this bucket's last refusal was logged, so a caller refused many times a minute is
    /// logged once a minute rather than once a request.
    logged: Option<Instant>,
}

/// A refusal of [`RequestLimits::charge`]: how long the caller should wait before its next
/// request is admitted, in whole seconds, at least 1.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Throttled {
    pub(super) retry_after_secs: u64,

    /// Whether this refusal is the first of its bucket within a minute, the one to log.
    pub(super) log: bool,
}

/// The buckets of every caller, and the limit of each class (requests per minute; `None` = that
/// class is not limited). Each bucket holds one minute of requests and refills continuously.
pub(super) struct RequestLimits {
    light: Option<u32>,
    heavy: Option<u32>,
    buckets: Mutex<Buckets>,
}

struct Buckets {
    by_caller: HashMap<(Caller, Cost), Bucket>,

    /// The size at which the next prune runs: [`PRUNE_AT`], or twice what the last prune kept, so
    /// many callers active at once cost one prune per doubling rather than one per request.
    prune_at: usize,
}

impl RequestLimits {
    /// Limits of `light` and `heavy` requests per minute per caller. A limit under
    /// [`MIN_REQUESTS_PER_MINUTE`] is refused, naming its flag and config key.
    pub(super) fn new(light: Option<u32>, heavy: Option<u32>) -> Result<RequestLimits, String> {
        for (limit, name) in [(light, "light_requests_per_minute"), (heavy, "heavy_requests_per_minute")] {
            if let Some(limit) = limit {
                if limit < MIN_REQUESTS_PER_MINUTE {
                    return Err(format!(
                        "--{} (\"{}\" in the config file) is {}; the lowest limit is {} a minute, \
                        since under it a refused caller is told to wait longer than its client \
                        waits. Pick a higher limit, or leave it unset for no limit.",
                        name.replace('_', "-"), name, limit, MIN_REQUESTS_PER_MINUTE
                    ));
                }
            }
        }

        Ok(RequestLimits {
            light,
            heavy,
            buckets: Mutex::new(Buckets { by_caller: HashMap::new(), prune_at: PRUNE_AT }),
        })
    }

    /// No class limited: every request is admitted.
    #[cfg(test)]
    pub(super) fn unlimited() -> RequestLimits {
        RequestLimits::new(None, None).expect("no limit is always valid")
    }

    fn limit_of(&self, cost: Cost) -> Option<u32> {
        match cost {
            Cost::Light => self.light,
            Cost::Heavy => self.heavy,
        }
    }

    /// Charge one request of class `cost` to `principal`'s bucket for that class, now.
    pub(super) fn charge(&self, principal: &Principal, cost: Cost) -> Result<(), Throttled> {
        self.charge_at(principal, cost, Instant::now())
    }

    fn charge_at(&self, principal: &Principal, cost: Cost, now: Instant) -> Result<(), Throttled> {
        let Some(limit) = self.limit_of(cost) else { return Ok(()) };
        // A poisoned lock still holds consistent buckets: every update below is a plain store.
        let mut buckets = self.buckets.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = (Caller::of(principal), cost);

        if !buckets.by_caller.contains_key(&key) && buckets.by_caller.len() >= buckets.prune_at {
            buckets.by_caller.retain(|(_, class), bucket| {
                self.limit_of(*class).is_some_and(|limit| refilled(bucket, limit, now) < f64::from(limit))
            });
            buckets.prune_at = PRUNE_AT.max(buckets.by_caller.len() * 2);
        }

        let bucket = buckets.by_caller.entry(key)
            .or_insert_with(|| Bucket { tokens: f64::from(limit), at: now, logged: None });
        // `now` was read before the lock, so a later charge may hold an earlier time: it refills
        // nothing and never moves the bucket back, or the interval between would be refilled twice.
        let now = now.max(bucket.at);
        bucket.tokens = refilled(bucket, limit, now);
        bucket.at = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;

            return Ok(());
        }

        let per_second = f64::from(limit) / 60.0;
        let retry_after_secs = ((1.0 - bucket.tokens) / per_second).ceil().max(1.0) as u64;
        let log = bucket.logged.is_none_or(|logged| now.duration_since(logged).as_secs() >= 60);

        if log {
            bucket.logged = Some(now);
        }

        Err(Throttled { retry_after_secs, log })
    }

    /// Give back the request [`Self::charge`] took from `principal`'s bucket of class `cost`, for a
    /// request refused for now before it changed anything. A bucket refunded past full is held to
    /// its limit by the next charge's refill ([`refilled`]), like any other.
    pub(super) fn refund(&self, principal: &Principal, cost: Cost) {
        if self.limit_of(cost).is_none() {
            return;
        }

        let mut buckets = self.buckets.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(bucket) = buckets.by_caller.get_mut(&(Caller::of(principal), cost)) {
            bucket.tokens += 1.0;
        }
    }
}

/// `bucket`'s tokens at `now`, refilled at `limit` a minute up to `limit`.
fn refilled(bucket: &Bucket, limit: u32, now: Instant) -> f64 {
    let per_second = f64::from(limit) / 60.0;
    let elapsed = now.saturating_duration_since(bucket.at).as_secs_f64();

    (bucket.tokens + elapsed * per_second).min(f64::from(limit))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    fn operator(id: &str) -> Principal {
        Principal::Operator(id.to_string())
    }

    /// Charge `count` requests at `at`, and return how many were admitted.
    fn admitted(limits: &RequestLimits, principal: &Principal, cost: Cost, count: usize, at: Instant) -> usize {
        (0..count).filter(|_| limits.charge_at(principal, cost, at).is_ok()).count()
    }

    /// A caller gets its limit at once, then is refused with the wait one request takes to refill,
    /// and is admitted again once that time has passed. Revert: start a bucket empty → red on the
    /// first count; refill by elapsed seconds without the rate → red on the refill; a wait of the
    /// whole minute → red on `retry_after_secs`.
    #[test]
    fn a_bucket_admits_its_limit_then_refills_at_its_rate() {
        let limits = RequestLimits::new(Some(6), None).unwrap();
        let alice = operator("alice");
        let start = Instant::now();

        assert_eq!(admitted(&limits, &alice, Cost::Light, 9, start), 6, "a minute's worth at once");
        assert_eq!(
            limits.charge_at(&alice, Cost::Light, start),
            Err(Throttled { retry_after_secs: 10, log: false }),
            "six a minute is one every 10 s; the first refusal was the one logged"
        );
        assert!(limits.charge_at(&alice, Cost::Light, start + Duration::from_secs(9)).is_err(), "not refilled yet");
        assert!(limits.charge_at(&alice, Cost::Light, start + Duration::from_secs(11)).is_ok(), "one refilled");
        assert_eq!(admitted(&limits, &alice, Cost::Light, 9, start + Duration::from_secs(3600)), 6,
                   "a bucket refills to its limit, never past it");
    }

    /// One caller emptying its bucket leaves another's untouched, and emptying the Heavy bucket
    /// leaves the same caller's Light bucket untouched. The static token and an open server are
    /// one caller each. Revert: key the bucket on the class alone → red on bob; on the caller
    /// alone → red on Light.
    #[test]
    fn callers_and_classes_have_buckets_of_their_own() {
        let limits = RequestLimits::new(Some(4), Some(4)).unwrap();
        let now = Instant::now();

        assert_eq!(admitted(&limits, &operator("alice"), Cost::Heavy, 10, now), 4);
        assert_eq!(admitted(&limits, &operator("bob"), Cost::Heavy, 10, now), 4, "bob's own bucket");
        assert_eq!(admitted(&limits, &operator("alice"), Cost::Light, 10, now), 4, "alice's Light bucket");
        assert_eq!(admitted(&limits, &Principal::Static, Cost::Heavy, 10, now), 4);
        assert_eq!(admitted(&limits, &Principal::Open, Cost::Heavy, 10, now), 4);
        assert_eq!(admitted(&limits, &Principal::Static, Cost::Heavy, 1, now), 0, "every static-token caller shares one");
    }

    /// A class with no limit is never charged, and limiting one class leaves the other free.
    #[test]
    fn an_unset_limit_never_refuses() {
        let now = Instant::now();
        let heavy_only = RequestLimits::new(None, Some(4)).unwrap();

        assert_eq!(admitted(&heavy_only, &operator("alice"), Cost::Light, 10_000, now), 10_000);
        assert_eq!(admitted(&heavy_only, &operator("alice"), Cost::Heavy, 10, now), 4);
        assert_eq!(admitted(&RequestLimits::unlimited(), &Principal::Open, Cost::Heavy, 10_000, now), 10_000);
    }

    /// A limit under the minimum is refused with its flag and key named, and the minimum itself is
    /// accepted. Revert: compare with `<=` → red on the minimum.
    #[test]
    fn a_limit_below_the_minimum_is_refused() {
        let error = RequestLimits::new(Some(3), None).err().unwrap();
        assert!(error.contains("--light-requests-per-minute") && error.contains("\"light_requests_per_minute\""), "{}", error);
        let error = RequestLimits::new(None, Some(0)).err().unwrap();
        assert!(error.contains("--heavy-requests-per-minute") && error.contains("\"heavy_requests_per_minute\""), "{}", error);
        assert!(RequestLimits::new(Some(4), Some(4)).is_ok());
    }

    /// A refused caller is logged once a minute, not once a request.
    #[test]
    fn a_refused_caller_is_logged_once_a_minute() {
        let limits = RequestLimits::new(None, Some(4)).unwrap();
        let alice = operator("alice");
        let start = Instant::now();
        admitted(&limits, &alice, Cost::Heavy, 4, start);

        let logged = |at: Instant| limits.charge_at(&alice, Cost::Heavy, at).err().unwrap().log;
        assert!(logged(start));
        assert!(!logged(start + Duration::from_millis(100)));
        // 59 s refills about 3.9 tokens: the bucket is still short of the fourth.
        assert_eq!(admitted(&limits, &alice, Cost::Heavy, 3, start + Duration::from_secs(59)), 3);
        assert!(!logged(start + Duration::from_secs(59)));
        assert_eq!(admitted(&limits, &alice, Cost::Heavy, 1, start + Duration::from_secs(75)), 1);
        assert!(logged(start + Duration::from_secs(75)), "a minute after the last logged refusal");
    }

    /// Past [`PRUNE_AT`] buckets, the full ones are let go and the ones still spending are kept,
    /// so a caller cannot refill its budget by crowding the map. Revert: drop every bucket at the
    /// prune → red on the kept caller.
    #[test]
    fn a_prune_lets_go_only_of_full_buckets() {
        let limits = RequestLimits::new(Some(4), None).unwrap();
        let start = Instant::now();
        let spender = operator("spender");
        assert_eq!(admitted(&limits, &spender, Cost::Light, 4, start), 4);

        // Each of these takes one token and is full again 15 s later.
        // With the spender, the map then holds PRUNE_AT buckets, so the next new caller prunes.
        for index in 0..PRUNE_AT - 1 {
            assert!(limits.charge_at(&operator(&format!("caller-{}", index)), Cost::Light, start).is_ok());
        }

        let later = start + Duration::from_secs(16);
        assert!(limits.charge_at(&operator("newcomer"), Cost::Light, later).is_ok(), "the prune runs here");
        let held = limits.buckets.lock().unwrap().by_caller.len();
        assert_eq!(held, 2, "the spender (refilled one, short of four) and the newcomer");
        assert_eq!(admitted(&limits, &spender, Cost::Light, 4, later), 1, "the spender kept its spent bucket");
    }

    /// A charge holding an earlier time than the bucket's (it read the clock before another took
    /// the lock) refills nothing and leaves the bucket's time where it was.
    ///
    /// Revert: drop the `now.max(bucket.at)` → the stale charge moves the bucket back 5 s, which
    /// the next charge refills again → 13 admitted, not 8 → red.
    #[test]
    fn a_stale_charge_refills_nothing_twice() {
        let limits = RequestLimits::new(Some(60), None).unwrap();
        let start = Instant::now();
        let alice = operator("alice");

        assert_eq!(admitted(&limits, &alice, Cost::Light, 60, start), 60);
        assert_eq!(admitted(&limits, &alice, Cost::Light, 1, start + Duration::from_secs(10)), 1);
        assert_eq!(admitted(&limits, &alice, Cost::Light, 1, start + Duration::from_secs(5)), 1);
        assert_eq!(
            admitted(&limits, &alice, Cost::Light, 60, start + Duration::from_secs(10)), 8,
            "ten seconds at one a second, less the two already admitted"
        );
    }

    /// A refund gives back one request, and never fills a bucket past its limit.
    ///
    /// Reverts: refund nothing → red on the first; drop the cap in [`refilled`] → red on the second.
    #[test]
    fn a_refund_gives_back_one_request_up_to_full() {
        let limits = RequestLimits::new(Some(6), Some(6)).unwrap();
        let start = Instant::now();
        let alice = operator("alice");

        assert_eq!(admitted(&limits, &alice, Cost::Heavy, 6, start), 6);
        limits.refund(&alice, Cost::Heavy);
        assert_eq!(admitted(&limits, &alice, Cost::Heavy, 6, start), 1, "the refunded request");

        let bob = operator("bob");
        assert_eq!(admitted(&limits, &bob, Cost::Heavy, 1, start), 1);
        limits.refund(&bob, Cost::Heavy);
        limits.refund(&bob, Cost::Heavy);
        assert_eq!(admitted(&limits, &bob, Cost::Heavy, 10, start), 6, "full is the limit, not more");
    }
}
