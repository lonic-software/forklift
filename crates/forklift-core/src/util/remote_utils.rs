//! The client side of the remote protocol (`docs/format/REMOTE_PROTOCOL.md`): the HTTP
//! client and the sync engines behind `lift`, `lower` and `franchise`. Everything here
//! returns data — the commands own the words.
//!
//! Transfers are parallel by design (DESIGN.html §4.1): object fetches and uploads fan
//! out over concurrent connections, bounded by [`CONCURRENT_TRANSFERS`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use crate::model::remote::{
    CommitLiftRequest, ErrorResponse, MissingObjectsRequest, MissingObjectsResponse,
    RefUpdateRequest, ResolveRequest, ResolveResponse, TrustAnchorDto, UploadTargetsRequest,
    UploadTargetsResponse, WarehouseInfo, LIFT_SESSION_BLOB_NOT_READY, MAX_MISSING_BATCH,
    MAX_UPLOAD_TARGETS_BATCH, PROTOCOL_VERSION,
};
use crate::enums::config_scope::ConfigScope;
use crate::error::{CoreError, RefusalCode};
use crate::globals::{self, StorageRootScope};
use crate::util::office_utils::OFFICE_PALLET_NAME;
use crate::util::scope_utils::{self, MaterializationScope, ScopeClass};
use crate::util::{
    bundle_utils, config_utils, file_utils, merge_utils, object_utils, office_utils,
    pack_utils, pallet_utils, sign_utils,
};

/// How many object transfers run concurrently.
pub const CONCURRENT_TRANSFERS: usize = 24;

/// The characters a warehouse path SEGMENT must be percent-encoded against before it is spliced
/// into a URL. Everything but RFC 3986 unreserved characters (ASCII alphanumerics, `-`, `_`,
/// `.`, `~`) is encoded — so a segment holding a space, `#`, `?`, `%`, or any other character
/// that is reserved or unsafe in the URL grammar round-trips instead of producing an invalid or
/// misrouted request. Non-ASCII UTF-8 bytes are always percent-encoded by `utf8_percent_encode`
/// regardless of this set, since an `AsciiSet` only classifies the ASCII range.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// Percent-encode a warehouse path's SEGMENTS for use in a URL, preserving the `/` separators
/// between them (so a multi-segment path still round-trips as multiple segments on the wire,
/// never one opaque `%2F`-joined blob). Each segment — including an empty one, e.g. from a
/// leading, trailing, or doubled `/` — is encoded independently against [`PATH_SEGMENT`].
fn encode_path_segments(path: &str) -> String {
    path.split('/')
        .map(|segment| utf8_percent_encode(segment, PATH_SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

/// How many objects one batch-fetch request asks for: bounds the response the server
/// builds in memory while still amortizing the round trip over many objects.
const BATCH_FETCH_CHUNK: usize = 512;

/// How many times a staged lift retries its session commit while the staging verifier catches
/// up, and the backoff between attempts. A storage-backed head promotes a blob within seconds
/// of its staging `PUT` in the hosted deployment; the schedule (~0.2s doubling to a 3s cap)
/// spans about 24s of sleep (0.2+0.4+0.8+1.6+3×7), so a slow verifier still commits, while a
/// genuinely stuck one surfaces as an error rather than hanging the lift forever. Only the transient
/// blob-not-ready case is retried — a corrupt or missing object fails at once.
const MAX_COMMIT_ATTEMPTS: usize = 12;
const COMMIT_BACKOFF_START: std::time::Duration = std::time::Duration::from_millis(200);
const COMMIT_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(3);

/// The outcome of one lift-session commit attempt.
enum CommitOutcome {
    /// The session's objects are verified and promoted; the ref update may proceed.
    Committed,

    /// A blob is still being promoted out of band by the staging verifier — retry with backoff.
    BlobNotReady,
}

/// Whether a status means the remote does not implement an endpoint at all (an older build):
/// a `404` (no such route) or `405` (the path exists for other methods only). The caller falls
/// back to the legacy path. Any other non-success status is a real error, not an absence.
fn endpoint_absent(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::METHOD_NOT_ALLOWED
}

/// Whether a failed commit response is the one *transient* case — a blob the staging verifier
/// has not promoted yet — versus a terminal one (a corrupt staged object, a control-plane
/// object never uploaded, an over-cap request). The retriable signal is the shared
/// [`LIFT_SESSION_BLOB_NOT_READY`] marker the head embeds, matched on a `422`; keeping the
/// decision here (pure) is what makes it unit-testable and keeps the retry policy in one place.
fn is_transient_commit_failure(status: reqwest::StatusCode, message: &str) -> bool {
    status == reqwest::StatusCode::UNPROCESSABLE_ENTITY
        && message.contains(LIFT_SESSION_BLOB_NOT_READY)
}

/// A fresh client-side lift session id — a random v4 UUID (the same in-tree generator that
/// mints pseudonymous operator ids, so no new dependency). It scopes one pallet lift's staging
/// keys (`staging/{session}/{hash}`) on a storage-backed head and is a safe single path
/// component; a direct head ignores it.
fn new_lift_session() -> String {
    config_utils::mint_uuid_v4()
}

/// What a fetch pass actually transferred (objects already present are skipped).
#[derive(Default)]
pub struct FetchStats {
    pub fetched_objects: usize,
    pub fetched_signatures: usize,

    /// How many parcels the walk actually descended into. The bound makes this the size of
    /// the gap between the remote head and what is already complete locally, not the length
    /// of history — the property `fetch_history` exists to keep.
    pub walked_parcels: usize,
}

/// What a lift actually transferred.
pub struct LiftStats {
    pub new_parcels: usize,
    pub uploaded_objects: usize,
    pub uploaded_signatures: usize,
    pub old_head: Option<String>,
}

/// The outcome of lifting one pallet.
pub enum LiftResult {
    /// The remote already has the local head.
    UpToDate,

    /// The pallet was lifted.
    Lifted(LiftStats),
}

/// The default Tor SOCKS proxy: the address a stock local `tor` daemon listens on. The
/// `socks5h` scheme (not `socks5`) hands the hostname to the proxy to resolve, which is
/// mandatory for an onion address — it has no DNS record and only resolves inside the Tor
/// network, so resolving it locally would always fail.
pub const DEFAULT_TOR_PROXY: &str = "socks5h://127.0.0.1:9050";

/// How the client reaches a remote through Tor — the peer-to-peer transport (DESIGN.html §4.7).
/// A peer publishes its warehouse as a Tor onion service (no fixed IP, no port-forwarding, no
/// NAT configuration — just a shareable `.onion`), and this decides whether a given remote is
/// dialed through the local Tor SOCKS proxy.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TorMode {
    /// Route through Tor only when the remote is an onion service (its host ends in `.onion`).
    /// The default: a plain `http(s)` remote is dialed directly, an onion one through Tor — so
    /// nothing changes for existing remotes and an onion URL Just Works.
    Auto,

    /// Always route through Tor, even a clearnet remote — reach it anonymously.
    On,

    /// Never route through Tor, even an onion host (which then fails to resolve locally). The
    /// escape hatch for a caller that reaches `.onion` through some other transport.
    Off,
}

impl TorMode {
    /// Parse a `remote.tor` value. `auto` (or anything unrecognized) → [`TorMode::Auto`];
    /// `on`/`true`/`yes`/`1` → [`TorMode::On`]; `off`/`false`/`no`/`0` → [`TorMode::Off`].
    /// Case- and surrounding-whitespace-insensitive. An unrecognized value falls back to the
    /// safe default (`Auto`): it never forces traffic through a proxy the user did not ask for,
    /// and never blocks an onion remote.
    fn parse(value: &str) -> TorMode {
        match value.trim().to_ascii_lowercase().as_str() {
            "on" | "true" | "yes" | "1" => TorMode::On,
            "off" | "false" | "no" | "0" => TorMode::Off,
            _ => TorMode::Auto,
        }
    }
}

/// The client's Tor transport settings, resolved from configuration.
#[derive(Clone)]
pub struct TorSettings {
    pub mode: TorMode,
    pub proxy: String,
}

impl Default for TorSettings {
    fn default() -> TorSettings {
        TorSettings { mode: TorMode::Auto, proxy: DEFAULT_TOR_PROXY.to_string() }
    }
}

impl TorSettings {
    /// Read the Tor settings from configuration (`remote.tor`, `remote.torProxy`), falling back
    /// to the defaults for anything unset — and, deliberately, for anything *unreadable* too: a
    /// missing or malformed configuration file must never make constructing a client fail (a
    /// client is built on hot paths, and in contexts with no warehouse at all), so a read error
    /// degrades to the defaults, which route only onion remotes through the stock local proxy.
    ///
    /// The warehouse configuration is consulted first (see [`config_utils::get_effective_value`]):
    /// correct for every caller that already has "a warehouse" of its own to have an opinion —
    /// which is every caller except one. See [`Self::from_global_config`] for that exception.
    pub fn from_config() -> TorSettings {
        let mode = config_utils::get_effective_value(config_utils::KEY_REMOTE_TOR)
            .ok()
            .flatten()
            .map(|(value, _)| TorMode::parse(&value))
            .unwrap_or(TorMode::Auto);

        let proxy = config_utils::get_effective_value(config_utils::KEY_REMOTE_TOR_PROXY)
            .ok()
            .flatten()
            .map(|(value, _)| value)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_TOR_PROXY.to_string());

        TorSettings { mode, proxy }
    }

    /// Like [`Self::from_config`], but *global* configuration only — never the warehouse scope.
    ///
    /// The one caller this exists for is `franchise`'s initial handshake, which now (deliberately)
    /// runs before the target warehouse exists, so there is no "this warehouse" to have an
    /// opinion about Tor — only the ambient working directory, which may happen to sit inside a
    /// *different*, unrelated warehouse. `from_config`'s effective (warehouse-then-global) lookup
    /// would silently pick up that unrelated warehouse's `remote.tor`/`remote.torProxy` — a
    /// cwd-dependent, security-relevant surprise for a brand-new clone elsewhere. Before franchise
    /// moved its handshake earlier, this could never happen: the handshake ran *inside* the fresh
    /// (still-empty) target, whose warehouse scope had never had anything to set — so global was,
    /// in effect, the only scope that could ever apply. This preserves that.
    pub fn from_global_config() -> TorSettings {
        let mode = config_utils::get_scoped_value(config_utils::KEY_REMOTE_TOR, ConfigScope::Global)
            .ok()
            .flatten()
            .map(|value| TorMode::parse(&value))
            .unwrap_or(TorMode::Auto);

        let proxy = config_utils::get_scoped_value(config_utils::KEY_REMOTE_TOR_PROXY, ConfigScope::Global)
            .ok()
            .flatten()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_TOR_PROXY.to_string());

        TorSettings { mode, proxy }
    }
}

/// Whether a remote URL names a Tor onion service — its host is a `.onion` address. Tolerant by
/// design: an unparseable URL, or one with no host, is simply "not onion", so the decision
/// degrades to a direct dial rather than erroring here — a genuinely broken URL fails later at
/// the actual request, with a clearer message than a proxy-parse error would give.
fn is_onion_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(|host| host.to_ascii_lowercase()))
        // Tolerate a trailing FQDN dot (`x.onion.`) — still an onion, must route through Tor,
        // not get dialed directly and fail local resolution with a confusing error.
        .is_some_and(|host| host.trim_end_matches('.').ends_with(".onion"))
}

/// Whether to dial this remote through the Tor SOCKS proxy, given the mode. Pure and total, so
/// the policy is unit-testable without a socket: `On` always, `Off` never, `Auto` iff the
/// remote is an onion address.
fn should_route_through_tor(mode: &TorMode, url: &str) -> bool {
    match mode {
        TorMode::On => true,
        TorMode::Off => false,
        TorMode::Auto => is_onion_url(url),
    }
}

/// How long the *connect* phase of any request against this remote — including `update_ref` —
/// may take before the client gives up, when the remote is dialed directly (not through Tor; see
/// [`REMOTE_CONNECT_TIMEOUT_TOR`]). Safe to apply unconditionally, unlike [`REMOTE_READ_TIMEOUT`]
/// below: connecting is not what the settled audit-walk contract needs to run long (that walk
/// only starts once the connection is already open and the request already sent), so bounding the
/// dial never touches it.
const REMOTE_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The connect-phase bound for a remote dialed *through Tor* (`should_route_through_tor`).
/// `connect_timeout` wraps the whole connector, including the SOCKS handshake and onion circuit
/// build — not just the TCP leg — and circuit build alone can legitimately take tens of seconds.
/// [`REMOTE_CONNECT_TIMEOUT`]'s 5s would silently break every working onion remote, so a Tor dial
/// gets a separate, much longer allowance instead of one shared value that has to compromise
/// between "fail fast on a normal remote" and "don't kill a live onion circuit build".
const REMOTE_CONNECT_TIMEOUT_TOR: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a read/metadata request whose server side does **O(constant) work before its first
/// response byte** may go without receiving any bytes before the client gives up, *on top of*
/// whatever connect budget already applies (see [`bounded_read_timeout`] — this constant alone is
/// not the value configured on the client). This is the FORK-49 fix: a remote that completes the
/// TCP connect and then never writes anything (or writes a `Content-Length` it never delivers)
/// used to hang the calling command forever, with no output and no way out short of killing the
/// process.
///
/// This bounds *silence*, not duration: it is `ClientBuilder::read_timeout`, which applies
/// per-read and resets on any progress *once headers have arrived*, so a transfer that is moving
/// bytes — however slowly — is never killed by it, only one that has gone genuinely quiet for the
/// full window. A per-request *total* deadline (`RequestBuilder::timeout`) was tried first and
/// rejected: it cannot tell a stalled transfer from a slow-but-healthy one, and a large response
/// body would be killed on any link slower than the window can cover, identically on every retry,
/// which is worse than the hang this fix exists to remove (the settled contract: "a transfer that
/// is moving bytes… is never silent"; only silence may fail).
///
/// **Before headers arrive, `read_timeout` is a fixed, non-resetting deadline on the server's own
/// pre-first-byte work** — it does not get the resets-on-progress behavior at all, because there
/// is nothing yet to make progress on, **and it is armed and checked from the moment the request
/// is constructed, before the connector is even polled** — so it covers DNS/TCP/TLS/SOCKS too, not
/// just the body (measured empirically against this exact reqwest version — a client built with
/// `connect_timeout(60s)` + `read_timeout(3s)` against a black-holed address failed at 3.002s,
/// not 60s). That is why this is carried by only three of the module's read/metadata calls
/// — [`RemoteClient::fetch_info`], [`RemoteClient::fetch_signature`],
/// [`RemoteClient::fetch_bundle_to`] — each of whose server side does O(constant) work (a single
/// lookup, or serving an already-built file) before writing anything, so a flat 10s pre-first-byte
/// budget is honest. `fetch_object` needs a much looser budget of its own
/// ([`FETCH_OBJECT_READ_TIMEOUT`]) since its server side is size-dependent, not O(constant).
/// `missing_objects`, `fetch_batch`, and `fetch_subtree` are deliberately **not** bounded at all:
/// their server sides build a bundle (or consult up to `MAX_MISSING_BATCH` hashes) *before* the
/// first byte, work whose cost depends on object sizes the client cannot know in advance — no flat
/// budget for that is honest, so they stay on the unbounded client until they have their own
/// scaled/measured budget or an abandon-and-fall-back lane (see the comment at each of those three
/// call sites).
///
/// `read_timeout` is a `ClientBuilder`-level setting with no per-request override — it cannot be
/// switched off for one specific request — so it is carried only by
/// [`RemoteClient::bounded_reads`]/[`RemoteClient::bounded_object_reads`], never by `http`.
/// `update_ref` shares `http`, and its server side legitimately runs a parcel-closure audit walk —
/// scoped by the history segment being pushed, which on a first lift into an empty pallet is the
/// whole history — before its first response byte; that can take minutes with *no* bytes moving at
/// all, which this constant would (correctly) call silence if it applied there. Giving the bounded
/// reads their own clients means `update_ref` needs no exemption: it was simply never wired to a
/// client that carries this setting.
///
/// 10s of silence, on top of whichever connect budget applies, is generous for any of the three
/// tight calls above: a healthy connection carrying real progress, however slow the link,
/// essentially never goes a full 10s without delivering a byte, and each of their pre-first-byte
/// server costs is a single lookup or an already-built file.
const REMOTE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The read/metadata silence budget for [`RemoteClient::fetch_object`] alone — see
/// [`RemoteClient::bounded_object_reads`]. Deliberately loose, not tuned to feel responsive:
/// `server.rs`'s `get_object` handler documents that it "buffers the whole object in memory" via
/// `retrieve_object_by_hash`, which content-verifies before returning and, for a packed/delta
/// object, decompresses and reconstructs it in memory too — all inside `blocking(...)`, entirely
/// before `bytes.into_response()` writes a single byte. That pre-first-byte phase is
/// size-dependent, structurally the same shape as `objects/batch` (which stays fully unbounded for
/// exactly this reason) — the difference is that a single object's size is capped
/// (`object_utils::MAX_OBJECT_BYTES`, 64MiB), so unlike a batch, a flat budget over that ceiling
/// can be honest, if it is loose enough. 60s is sized to comfortably cover reading, reconstructing,
/// and hashing any object up to that ceiling — not to feel snappy.
///
/// Accepted residual, recorded rather than silently absorbed: `server.rs` also documents
/// grandfathered pre-ceiling blobs (from before `MAX_OBJECT_BYTES` existed) as served "whole and
/// genuinely unbounded" — for a multi-gigabyte one, even 60s can be too tight, making it
/// permanently, deterministically unfetchable through this bound. Knowingly accepted for this
/// slice; the root fix (streaming these handlers instead of buffering, removing the size-dependent
/// pre-first-byte phase entirely) is FORK-85.
const FETCH_OBJECT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The base allowance for [`RemoteClient::error_of`]'s own inline error-body read, once a
/// non-success status line and headers have already arrived. Needed because `error_of`'s own
/// bound (if any) doesn't cover this: it serves responses from every client this type builds,
/// including the three deliberately-unbounded ones
/// (`http`/`no_redirect`/`upload_targets`'s negotiation calls — see those calls' own docs), so a
/// remote that answers a `5xx` with full headers and then wedges before writing the JSON body can
/// otherwise hang the caller forever even though the status line already told it the call failed.
///
/// Flat, not scaled like [`FETCH_OBJECT_READ_TIMEOUT`] or the negotiation calls' own read costs:
/// after a non-success status line, the error body this module parses (an [`ErrorResponse`]) is
/// small and O(constant) regardless of which call produced it or why *that* call's own success
/// path is unbounded — the scaling reason a success body might be large or slow never applies to
/// the error one. On elapse, falls back to `status.canonical_reason()` — the exact fallback this
/// code already uses when the body fails to parse as JSON at all, so a timeout and a malformed
/// body are indistinguishable to the caller, which is right: both mean "no usable error body
/// arrived."
///
/// Not the actual timeout duration on its own (review round 5, finding 2) — see
/// [`error_body_read_budget`] for why `self.connect_timeout` is folded in on top of this base,
/// the same way [`bounded_read_timeout`] and [`RemoteClient::send_with_watchdog`]'s own phase
/// budgets already do.
const ERROR_BODY_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The actual bound to arm around an error-body read, given the connect budget this client
/// instance actually carries. `ERROR_BODY_READ_TIMEOUT` alone is a flat 10s regardless of which
/// client sent the request — but by the time a non-success response is in hand, the connection is
/// already established, so it is *not* protecting a connect phase the way
/// [`bounded_read_timeout`] protects a client's `read_timeout` sleep from preempting a legitimately
/// slow dial. It plays the other role this module already gives `connect_timeout` elsewhere
/// (`send_with_watchdog`'s `phase1_budget`/`phase2_budget`): a stand-in for "how slow is normal on
/// this link at all," which for a Tor-routed remote is 60s, not 5s. Without it, a refusal body
/// that takes, say, 11s to arrive over a healthy but slow Tor circuit gets killed at the flat 10s
/// mark — discarding a typed [`RefusalCode`] and its `next_step` for no reason but an unfairly
/// tight bound, degrading a machine caller to the wrong exit code.
///
/// A free function taking `connect_timeout` directly, not a method, so the arithmetic itself is
/// unit-testable without constructing a live client or waiting out a real budget.
fn error_body_read_budget(connect_timeout: std::time::Duration) -> std::time::Duration {
    connect_timeout + ERROR_BODY_READ_TIMEOUT
}

/// The `read_timeout` to configure on a bounded-reads client, given the `connect_timeout` this
/// client instance actually uses and the post-connect silence budget intended for it
/// ([`REMOTE_READ_TIMEOUT`] or [`FETCH_OBJECT_READ_TIMEOUT`]).
///
/// Simply using `silence_budget` as the client's `read_timeout` is a bug: that sleep is armed and
/// checked before the connector is even polled (see [`REMOTE_READ_TIMEOUT`]'s doc),
/// so it would preempt a legitimately slow connect — most sharply for a Tor dial, whose circuit
/// build can take tens of seconds under [`REMOTE_CONNECT_TIMEOUT_TOR`], far longer than a bare 10s
/// or even 60s silence budget would tolerate. Adding the connect budget in first guarantees the
/// connect phase always gets its own full allowance — whichever of [`REMOTE_CONNECT_TIMEOUT`] or
/// [`REMOTE_CONNECT_TIMEOUT_TOR`] this client was built with — before the silence clock can matter
/// at all.
fn bounded_read_timeout(connect_timeout: std::time::Duration,
                        silence_budget: std::time::Duration) -> std::time::Duration {
    connect_timeout + silence_budget
}

/// How large each chunk handed to `reqwest::Body::wrap_stream` is for an upload's
/// watchdog-guarded body stream (see [`RemoteClient::send_with_watchdog`]). Small enough that the
/// watchdog's "last chunk pulled" timestamp advances often during a healthy transfer — so a
/// genuine stall is caught close to the configured budget, not masked by one giant chunk that
/// takes long to hand off — large enough not to spend a large upload's CPU mostly on per-chunk
/// bookkeeping.
///
/// This size sets a **per-connection throughput floor** (review round S2-F4): hyper pulls the
/// next chunk from this stream only after handing the previous one off, so `progress`'s timestamp
/// advances at most once per `UPLOAD_CHUNK_SIZE` bytes — a connection that cannot move at least
/// one chunk within [`UPLOAD_SILENCE_BUDGET`] plus connect looks silent to the watchdog even if
/// it is genuinely, if slowly, progressing. At the old 64 KiB this floor was
/// `65536 / 15s ≈ 4.4 kB/s` per connection — with [`CONCURRENT_TRANSFERS`] (24) sharing one
/// uplink, an aggregate floor of `≈105 kB/s` (`≈840 kbit/s`), which a real ADSL-class 1 Mbit/s
/// uplink sits uncomfortably close to. At `4 KiB` the same arithmetic gives
/// `4096 / 15s ≈ 273 B/s` per connection, `≈6.6 kB/s` (`≈52 kbit/s`) aggregate — comfortably below
/// any uplink this tool needs to keep working on, and within reach of the read path's own
/// resolution (`read_timeout` resets per socket read, ordinarily an MSS-sized ~1.5 KB — about 44×
/// finer than the old 64 KiB chunk, about 2.7× finer than this one). Kept a fixed constant rather
/// than something smaller still because the chunking is zero-copy (`Bytes::slice`, see
/// [`UploadChunks`]'s doc) — a smaller chunk costs a few more `Arc`/mutex touches, not another
/// allocation, so there is no real tension between "small enough to stay off the throughput
/// floor" and "large enough not to waste CPU."
const UPLOAD_CHUNK_SIZE: usize = 4 * 1024;

/// How long an upload's body-send stream may go without the client pulling a single chunk from it
/// before [`RemoteClient::send_with_watchdog`] gives up and abandons the request, *on top of*
/// whichever connect budget applies — same reasoning as [`bounded_read_timeout`]'s: a per-request
/// bound starts before connect too, so the connect allowance is added in rather than layered on
/// blind.
///
/// This is a **different mechanism** from [`REMOTE_READ_TIMEOUT`], deliberately not reused (see
/// this module's "why the read-path tool does not work here" note): `read_timeout` is a flat,
/// non-resetting deadline that covers connect *and* the entire pre-headers send phase, which is
/// exactly wrong for an upload — arming it here would cap the whole upload's total duration and
/// kill a healthy transfer on any link slower than the window it allows. This budget instead
/// governs a hand-rolled watchdog ([`RemoteClient::send_with_watchdog`]) driven by a timestamp
/// updated every time the body's stream actually yields a chunk to hyper — which only happens
/// while hyper is still pulling data to write, i.e. only while the connection is making progress.
/// Like `read_timeout`, it resets on every chunk moved and only fires on genuine silence, never on
/// total elapsed time — proven by the slow-but-steady upload test below, whose total duration
/// deliberately exceeds this budget while every individual gap stays well under it.
///
/// 10s of post-connect silence is the same order of magnitude as [`REMOTE_READ_TIMEOUT`] for the
/// same reason: a healthy connection, however slow, essentially never goes this long without
/// hyper wanting more bytes to write — genuine silence this long means the peer has stopped
/// reading, not that the link is merely slow.
const UPLOAD_SILENCE_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// How often [`RemoteClient::send_with_watchdog`] wakes up to re-check its shared progress
/// timestamp against [`UPLOAD_SILENCE_BUDGET`]. Small relative to that budget so a genuine stall
/// is caught close to the configured deadline rather than delayed by a coarse poll; large enough
/// not to spin the executor pointlessly during a normal transfer.
const UPLOAD_WATCHDOG_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Fixed part of [`post_send_verify_budget`]'s bound on the *post-send* phase — the wait for a
/// response once the whole body has already been handed off (see [`UploadProgress`]'s doc on why
/// that phase used to be left unbounded, and why leaving it unbounded was actually the FORK-49
/// defect surviving inside its own fix: a remote that reads the entire body and then wedges during
/// verification is a completely ordinary failure — a crash or a stuck disk mid-write — and it hangs
/// exactly like the original bug on this exact path). This covers dispatch/scheduling overhead
/// so a tiny object's total budget isn't unrealistically close to zero.
const POST_SEND_VERIFY_BASE: std::time::Duration = std::time::Duration::from_secs(2);

/// The conservative inline-verification throughput [`post_send_verify_budget`] assumes for the
/// size-scaled part of its bound. Deliberately far below real-world `blake3` throughput (typically
/// hundreds of MB/s to several GB/s single-threaded on modern hardware) to absorb a busy or loaded
/// server without risking a false stall on a healthy one, while still keeping the bound
/// meaningfully tighter than [`UPLOAD_SILENCE_BUDGET`]'s flat send-phase allowance for anything
/// short of a maximum-size object.
const POST_SEND_VERIFY_RATE_BYTES_PER_SEC: u64 = 8 * 1024 * 1024;

/// The **server-side verification** component of the post-send phase's budget, for an upload of
/// `body_len` bytes — see [`POST_SEND_VERIFY_BASE`]/[`POST_SEND_VERIFY_RATE_BYTES_PER_SEC`]'s
/// docs for the constants. This alone is *not* [`RemoteClient::send_with_watchdog`]'s actual
/// post-send budget (see that method's doc, review round S2-F2, for why it also folds in the
/// send-phase allowance as a flush-time margin) — it names only the part of that budget scaled to
/// this specific quantity.
///
/// Scaled to the body size because, unlike `update_ref`'s audit walk (deliberately left unbounded
/// — its server side is scoped by the *pushed history segment*, which on a first lift is the whole
/// history and can legitimately run minutes, a quantity the client has no way to bound honestly),
/// the work this phase waits on is entirely **client-known**: `upload_object`'s server side does
/// nothing after receiving the body but hash-verify those same bytes
/// (`forklift-server`'s `put_object` → `object_utils::store_object_bytes`, capped at
/// `object_utils::MAX_OBJECT_BYTES` — 64 MiB — by the framework's body limit before the handler
/// ever runs), and `put_presigned`'s storage backend does nothing but persist them. Either way the
/// upper bound on that work is `body_len`, which the client already has in hand before the request
/// is even built — precisely the property that makes bounding this phase honest where `update_ref`
/// is not.
fn post_send_verify_budget(body_len: usize) -> std::time::Duration {
    POST_SEND_VERIFY_BASE + std::time::Duration::from_secs_f64(
        body_len as f64 / POST_SEND_VERIFY_RATE_BYTES_PER_SEC as f64
    )
}

/// Shared state between an upload's body-send stream ([`UploadChunks`]) and
/// [`RemoteClient::send_with_watchdog`]'s polling loop: a timestamp updated every time the stream
/// actually yields a chunk (the only signal available that the transfer is still moving — see
/// [`UPLOAD_SILENCE_BUDGET`]'s doc), and a flag set once the stream is exhausted.
///
/// That second half does **not** mean "stop bounding the wait" (an earlier version of this fix
/// did exactly that, on the theory that it matched `update_ref`'s own unbounded response wait —
/// review round S2-F2 found that theory didn't hold: `update_ref`'s wait is unbounded because its
/// server-side work is the *pushed history segment*, unknowable in advance, while
/// `upload_object`'s post-receive work is inline hash verification of the exact bytes just sent,
/// bounded by a quantity — `body_len` — the client already has. A remote that reads the whole
/// body and then wedges is an ordinary failure, and it hung exactly like the original FORK-49 bug
/// on this exact path when this phase was left unbounded). What exhaustion changes is *which*
/// budget [`RemoteClient::send_with_watchdog`] checks `silent_for()` against, not whether it
/// checks at all — see that method's doc for the two-budget shape and why phase 2's budget still
/// has to be generous: `is_exhausted()` means every chunk was handed to hyper, not that every byte
/// reached the peer, so the post-exhaustion silence can legitimately include time the OS kernel
/// (or hyper's own internal buffer) is still spending flushing an in-flight tail — S2-F2's own
/// numbers: a 64 MiB object on a 2 Mbit/s uplink can have several seconds of genuinely-still-
/// moving tail queued the instant this flag flips.
struct UploadProgress {
    last_chunk: std::sync::Mutex<std::time::Instant>,
    exhausted: std::sync::atomic::AtomicBool,
}

impl UploadProgress {
    fn new() -> Arc<UploadProgress> {
        Arc::new(UploadProgress {
            last_chunk: std::sync::Mutex::new(std::time::Instant::now()),
            exhausted: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn touch(&self) {
        *self.last_chunk.lock().unwrap() = std::time::Instant::now();
    }

    fn mark_exhausted(&self) {
        self.exhausted.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn is_exhausted(&self) -> bool {
        self.exhausted.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// How long it has been since the stream last yielded a chunk — the value
    /// [`RemoteClient::send_with_watchdog`] compares against the budget.
    fn silent_for(&self) -> std::time::Duration {
        self.last_chunk.lock().unwrap().elapsed()
    }
}

/// A `Stream` over an upload's bytes, sliced into [`UPLOAD_CHUNK_SIZE`] windows of one shared
/// `bytes::Bytes` rather than copied into a `Vec<Vec<u8>>` up front (review round S2-F6):
/// `Bytes::from(Vec<u8>)` takes ownership of the caller's existing allocation with no copy, and
/// `Bytes::slice` shares that same backing buffer via a refcount bump — so a 64 MiB upload no
/// longer peaks at 128 MiB (the original bytes plus a second, fully-copied chunk list), and
/// [`CONCURRENT_TRANSFERS`] of them no longer peaks at roughly double the aggregate memory this
/// module otherwise needs.
///
/// Touches a shared [`UploadProgress`] every time it is polled and has a chunk to hand back — i.e.
/// exactly when hyper actually pulls more data to write — and marks it exhausted **the moment the
/// last chunk is handed back**, not on some later poll that returns `None`. That distinction is
/// load-bearing, not stylistic: with an explicit `Content-Length` set (see
/// [`watched_upload_body`]'s doc), hyper already knows the exact byte count to expect and can
/// consider the body fully sent once that many bytes have been produced — it has no *need* to
/// poll again just to observe an explicit end-of-stream `None`, and a real run against a small,
/// fully-draining body confirmed it does not: an earlier version of this stream that only called
/// `mark_exhausted` from the `None` arm left `progress.is_exhausted()` false forever on a body
/// small enough to fit in one chunk, so `send_with_watchdog`'s send-phase silence check fired at
/// its own budget instead of ever reaching the post-send phase at all — wrong phase, wrong (and
/// much larger) budget, wrong message. Checking the *remaining* length on the same poll that
/// yields the last item avoids depending on a poll that may never come.
///
/// Implemented by hand, rather than via a combinator, because `tokio_stream::StreamExt` (this
/// crate's only stream-utilities dependency) carries no side-effecting `map`-style adapter, and
/// pulling in a full stream-combinators crate for one adapter would be a heavier dependency than
/// this needs. `Self` holds nothing self-referential (a `Bytes`, a cursor, and an `Arc`), so it is
/// automatically `Unpin` and needs no manual pin-projection.
///
/// **`is_exhausted()` means "every chunk was handed to hyper," not "every byte reached the
/// peer."** Hyper may still hold buffered-but-unwritten bytes, and the OS kernel's own send
/// buffer may hold more on top of that — see [`RemoteClient::send_with_watchdog`]'s doc (review
/// round S2-F2) for why the post-send phase's budget has to account for that gap rather than
/// treating "exhausted" as "delivered."
struct UploadChunks {
    body: bytes::Bytes,
    offset: usize,
    progress: Arc<UploadProgress>,
}

impl tokio_stream::Stream for UploadChunks {
    type Item = Result<bytes::Bytes, std::io::Error>;

    fn poll_next(mut self: std::pin::Pin<&mut Self>,
                 _cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        if self.offset >= self.body.len() {
            // Reachable only for an already-exhausted stream hyper polls again anyway (harmless —
            // `mark_exhausted` is idempotent); the *first* time exhaustion is observed is always
            // on the branch below, on the same poll that hands back the last chunk.
            self.progress.mark_exhausted();
            return std::task::Poll::Ready(None);
        }

        let end = std::cmp::min(self.offset + UPLOAD_CHUNK_SIZE, self.body.len());
        let chunk = self.body.slice(self.offset..end);
        self.offset = end;
        self.progress.touch();

        // On the same poll that hands back the last chunk — see this struct's own doc for why
        // that matters — not a later poll that may never come.
        if self.offset >= self.body.len() {
            self.progress.mark_exhausted();
        }

        std::task::Poll::Ready(Some(Ok(chunk)))
    }
}

/// Build a watchdog-instrumented streaming body for an upload: `bytes` wrapped (zero-copy, see
/// [`UploadChunks`]'s doc) in a `bytes::Bytes` and sliced into [`UPLOAD_CHUNK_SIZE`] windows,
/// handed to `reqwest::Body::wrap_stream` over an [`UploadChunks`] that touches `progress` as each
/// one is pulled. Returns the body alongside the exact byte length, which the caller must set as
/// an explicit `Content-Length` header: `wrap_stream`'s body always reports an unknown
/// `size_hint`, and without that header reqwest/hyper fall back to `Transfer-Encoding: chunked` —
/// which a presigned S3 `PUT` rejects outright (see [`RemoteClient::put_presigned`]'s doc). An
/// empty body marks `progress` exhausted immediately, up front: there would otherwise be no chunk
/// whose yielding ever sets the flag.
fn watched_upload_body(bytes: Vec<u8>, progress: Arc<UploadProgress>) -> (reqwest::Body, usize) {
    let len = bytes.len();
    let body = bytes::Bytes::from(bytes);

    if body.is_empty() {
        progress.mark_exhausted();
    }

    let stream = UploadChunks { body, offset: 0, progress };

    (reqwest::Body::wrap_stream(stream), len)
}

/// The remote endpoint: base URL, optional bearer token, and the HTTP clients.
///
/// Four clients, not one, because three independent axes each need to vary per call: *redirect
/// policy* (`fetch_batch`'s initial `POST`, `upload_object`, and `put_presigned` must not
/// auto-follow; everything else may), *whether a
/// read/metadata silence bound applies at all* (only `fetch_info`, `fetch_object`,
/// `fetch_signature`, `fetch_bundle_to`; never `update_ref`, `missing_objects`, `fetch_batch`,
/// `fetch_subtree`, or any upload path — see [`REMOTE_READ_TIMEOUT`]'s doc), and *how loose that
/// bound is* (`fetch_object` alone needs [`FETCH_OBJECT_READ_TIMEOUT`] instead of
/// [`REMOTE_READ_TIMEOUT`] — see [`bounded_object_reads`](Self::bounded_object_reads)'s doc). All
/// four otherwise share the same proxy/connect-timeout configuration built once in
/// [`RemoteClient::new_with_tor`], which is also where each bounded client's actual `read_timeout`
/// is computed via [`bounded_read_timeout`] rather than being the raw silence-budget constant.
#[derive(Clone)]
pub struct RemoteClient {
    http: reqwest::Client,
    /// Same endpoint, automatic redirect-following disabled. `fetch_batch`'s initial `POST` uses
    /// this one: reqwest's default policy replays a `307`/`308` redirect with the original
    /// method *and body*, which would re-`POST` that call's signed JSON at a URL presigned
    /// for `GET` only — failing signature verification on a real S3-backed head (LocalStack
    /// answers `500`, AWS `403 SignatureDoesNotMatch`). Redirects off this client are instead
    /// inspected and followed by hand with a fresh `GET` (see `fetch_batch`). Unbounded, like
    /// `http`: `fetch_batch` is one of the three calls [`REMOTE_READ_TIMEOUT`]'s doc explains are
    /// deliberately never bounded at all.
    ///
    /// Also used by [`Self::upload_object`] and [`Self::put_presigned`] (review round S2 fix hole):
    /// a streamed upload body is one-shot and cannot be replayed at a redirect target, so any `3xx`
    /// — not only the `307`/`308` this doc's other reasoning names — must come back to
    /// [`Self::describe_upload_redirect`] raw rather than being auto-followed. See that function's
    /// doc for why a `303` specifically forced the move off `self.http`.
    no_redirect: reqwest::Client,
    /// Same endpoint as [`Self::http`], plus a `read_timeout` of
    /// [`bounded_read_timeout`]`(connect_timeout, `[`REMOTE_READ_TIMEOUT`]`)`. Used only by the
    /// three O(constant)-pre-first-byte calls (`fetch_info`, `fetch_signature`,
    /// `fetch_bundle_to`) — never by `fetch_object` (which needs the much looser
    /// [`Self::bounded_object_reads`]), `update_ref`, `missing_objects`, `fetch_batch`,
    /// `fetch_subtree`, or any upload path (see [`REMOTE_READ_TIMEOUT`]'s doc for why).
    bounded_reads: reqwest::Client,
    /// Same endpoint as [`Self::http`], plus a `read_timeout` of
    /// [`bounded_read_timeout`]`(connect_timeout, `[`FETCH_OBJECT_READ_TIMEOUT`]`)` — the same
    /// shape as [`Self::bounded_reads`], just with the looser silence budget `fetch_object`'s
    /// size-dependent server work needs (see [`FETCH_OBJECT_READ_TIMEOUT`]'s doc). Used only by
    /// `fetch_object`.
    bounded_object_reads: reqwest::Client,
    /// The connect-phase bound this instance's clients were actually built with —
    /// [`REMOTE_CONNECT_TIMEOUT`] or [`REMOTE_CONNECT_TIMEOUT_TOR`], whichever
    /// `should_route_through_tor` selected at construction. Kept so a connect-phase transport
    /// failure can be reported against the bound that actually applied, rather than a
    /// hardcoded guess (see `describe_transport_error`).
    connect_timeout: std::time::Duration,
    base: String,
    token: Option<String>,
}

/// Compose the client-facing error for a non-success response, threading a server-side refusal
/// code (§7.4) through the taxonomy. When the body tagged the failure with a stable refusal `code`
/// this build recognizes, the error is re-framed (via the [`crate::error`] bridge shim) so a
/// server-side refusal classifies with the *same* code and exit code as a local one; an
/// unrecognized code (a newer peer) or none at all (an older head, or a plain error) degrades to
/// the wrapped message — but when the wire still carried a `next_step` (the newer-peer case), it
/// is folded into that message rather than dropped, so recovery guidance survives even when the
/// code itself does not. Either way the message is wrapped with the action and status for context.
///
/// A free function (not a method) so the round-trip can be unit-tested without a live socket.
fn classify_remote_error(status: u16, action: &str, message: String,
                         code: Option<String>, next_step: Option<String>) -> String {
    let wrapped = format!("The remote refused {} ({}): {}", action, status, message);

    match code.as_deref().and_then(RefusalCode::from_code) {
        Some(code) => String::from(CoreError::refusal(code, wrapped, next_step.unwrap_or_default())),
        None => match next_step {
            Some(next_step) if !next_step.is_empty() => format!("{} {}", wrapped, next_step),
            _ => wrapped,
        },
    }
}

/// How a transport-level failure (one that never got as far as an HTTP status) classifies, given
/// only `reqwest::Error::is_connect()` and `reqwest::Error::is_timeout()` — the two booleans the
/// crate exposes for exactly this decision. A free enum plus a pure function over those two
/// `bool`s, rather than a method taking `reqwest::Error` directly, so all four combinations are
/// directly unit-testable: one of them (a genuine multi-minute kernel `ETIMEDOUT` on an
/// already-established socket, once TCP retransmissions are exhausted — see
/// [`TransportFailure::ReadTimedOut`]) is not practically constructible in a test at all, and
/// pinning the *decision* rather than the error construction is what makes that tractable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransportFailure {
    /// `is_connect() && is_timeout()`. In hyper-util, every connector-layer error — refused,
    /// no-such-host, TLS failure, *and* a genuine connect timeout — is tagged `ErrorKind::Connect`
    /// regardless of cause, so `is_connect()` alone cannot tell a timeout from any other connect
    /// failure. Pairing it with `is_timeout()` can: a kernel-level SYN-retry exhaustion takes far
    /// longer (~127s+ on common defaults) than any budget this client configures (5s direct, 60s
    /// Tor), so whenever both are true, this client's own `connect_timeout` is what fired — never
    /// a slower kernel-level connect timeout racing it.
    ConnectTimedOut,

    /// `is_timeout() && !is_connect()`. Ambiguous, and deliberately reported without naming a
    /// specific bound: this can be either this client's own configured `read_timeout` (exactly
    /// the effective connect+silence budget), or a genuine kernel `ETIMEDOUT` on a connection that
    /// was already established — the OS returns that once TCP retransmissions are exhausted,
    /// roughly 15 minutes on common Linux/macOS defaults — and `reqwest::Error::is_timeout()`
    /// cannot tell the two apart: it matches any `io::Error` with `kind() == TimedOut` anywhere in
    /// the source chain, not only its own synthetic marker for a client-configured timeout.
    /// Naming the configured budget here would be right most of the time and wrong by two orders
    /// of magnitude the rest, which is worse than not naming a number at all.
    ReadTimedOut,

    /// Neither of the above: a connection reset, a DNS failure, a refused connect, a TLS failure,
    /// or anything else that is not a timeout. Checking `is_connect()` without also requiring
    /// `is_timeout()` would wrongly route all of these into [`Self::ConnectTimedOut`] — every one
    /// of them is `is_connect() == true`, none of them timed out.
    Other,
}

/// Classify a transport failure from the two booleans `reqwest::Error` exposes for it. Pure and
/// total, so every one of the four combinations is directly assertable without constructing a
/// real `reqwest::Error` of each underlying kind.
fn classify(is_connect: bool, is_timeout: bool) -> TransportFailure {
    if is_connect && is_timeout {
        TransportFailure::ConnectTimedOut
    } else if is_timeout {
        TransportFailure::ReadTimedOut
    } else {
        TransportFailure::Other
    }
}

impl RemoteClient {
    /// Create a client for a remote.
    ///
    /// # Arguments
    /// * `url`   - The base URL of the remote (e.g. `http://127.0.0.1:9418`).
    /// * `token` - The bearer token, when the remote requires one.
    ///
    /// # Returns
    /// * `Ok(RemoteClient)` - The client.
    /// * `Err(String)`      - If the HTTP client could not be built.
    pub fn new(url: &str, token: Option<String>) -> Result<RemoteClient, String> {
        RemoteClient::new_with_tor(url, token, TorSettings::from_config())
    }

    /// Like [`RemoteClient::new`], but with explicit Tor settings rather than reading them from
    /// configuration — the seam the tests use to exercise onion routing without a config file,
    /// and the constructor a caller with settings already in hand can use.
    ///
    /// When the settings route this remote through Tor (see [`should_route_through_tor`]), all
    /// four underlying clients (see [`RemoteClient`]'s own doc) dial through the Tor SOCKS proxy
    /// and use [`REMOTE_CONNECT_TIMEOUT_TOR`] as their connect budget instead of
    /// [`REMOTE_CONNECT_TIMEOUT`]; the two bounded clients' `read_timeout` is computed from
    /// whichever of those applies (see [`bounded_read_timeout`]), so a Tor dial's much longer
    /// connect allowance is never undercut by the read-silence budget. Every non-onion remote gets
    /// the shorter, direct connect budget, as before — but every remote, onion or not, now carries
    /// a connect bound it did not before this diff, so "unchanged" no longer describes it.
    ///
    /// # Arguments
    /// * `url`   - The base URL of the remote (`http://…`, or `http://<onion>.onion`).
    /// * `token` - The bearer token, when the remote requires one.
    /// * `tor`   - The Tor transport settings.
    ///
    /// # Returns
    /// * `Ok(RemoteClient)` - The client.
    /// * `Err(String)`      - If the HTTP client, or the configured proxy, could not be built.
    pub fn new_with_tor(url: &str,
                        token: Option<String>,
                        tor: TorSettings) -> Result<RemoteClient, String> {
        let routes_through_tor = should_route_through_tor(&tor.mode, url);
        let proxy = if routes_through_tor {
            Some(reqwest::Proxy::all(&tor.proxy).map_err(|e| format!(
                "Error while configuring the Tor proxy \"{}\": {}", tor.proxy, e
            ))?)
        } else {
            None
        };

        // A Tor dial's connect phase covers the whole SOCKS handshake and onion circuit build,
        // which can legitimately take tens of seconds — far past what a direct dial should ever
        // need (see `REMOTE_CONNECT_TIMEOUT_TOR`'s doc).
        let connect_timeout = if routes_through_tor {
            REMOTE_CONNECT_TIMEOUT_TOR
        } else {
            REMOTE_CONNECT_TIMEOUT
        };

        let mut http = reqwest::Client::builder()
            .connect_timeout(connect_timeout);
        let mut no_redirect = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .redirect(reqwest::redirect::Policy::none());
        // `read_timeout` is armed and checked before the connector is even polled, so using the
        // raw silence budget here would let it preempt this exact `connect_timeout` —
        // `bounded_read_timeout` adds it in first so the connect phase always gets its full
        // allowance regardless of which budget (direct or Tor) applies to this instance.
        let mut bounded_reads = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .read_timeout(bounded_read_timeout(connect_timeout, REMOTE_READ_TIMEOUT));
        let mut bounded_object_reads = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .read_timeout(bounded_read_timeout(connect_timeout, FETCH_OBJECT_READ_TIMEOUT));

        // The same proxy governs all four clients: the redirect-following ones and the
        // hand-following one alike route through Tor when the remote does.
        if let Some(proxy) = &proxy {
            http = http.proxy(proxy.clone());
            no_redirect = no_redirect.proxy(proxy.clone());
            bounded_reads = bounded_reads.proxy(proxy.clone());
            bounded_object_reads = bounded_object_reads.proxy(proxy.clone());
        }

        let http = http.build()
            .map_err(|e| format!("Error while creating the HTTP client: {}", e))?;

        let no_redirect = no_redirect.build()
            .map_err(|e| format!("Error while creating the HTTP client: {}", e))?;

        let bounded_reads = bounded_reads.build()
            .map_err(|e| format!("Error while creating the HTTP client: {}", e))?;

        let bounded_object_reads = bounded_object_reads.build()
            .map_err(|e| format!("Error while creating the HTTP client: {}", e))?;

        Ok(RemoteClient {
            http,
            no_redirect,
            bounded_reads,
            bounded_object_reads,
            connect_timeout,
            base: url.trim_end_matches('/').to_string(),
            token,
        })
    }

    /// Create the client for the configured remote of the current warehouse
    /// (`remote.url`, plus `remote.token` when set).
    ///
    /// # Returns
    /// * `Ok(RemoteClient)` - The client.
    /// * `Err(String)`      - No remote is configured, *or* a remote is configured but the
    ///                        client could not be built from it (its config could not be read,
    ///                        or its URL is unusable) — [`is_configured`] tells the two apart
    ///                        for a caller that needs to, without inspecting this message.
    pub fn from_config() -> Result<RemoteClient, String> {
        let url = config_utils::get_effective_value(config_utils::KEY_REMOTE_URL)?
            .map(|(value, _)| value)
            .ok_or(format!(
                "No remote is configured for this warehouse. Set one with \
                \"config {} <url>\".",
                config_utils::KEY_REMOTE_URL
            ))?;

        let token = config_utils::get_effective_value(config_utils::KEY_REMOTE_TOKEN)?
            .map(|(value, _)| value);

        RemoteClient::new(&url, token)
    }

    /// Whether a remote URL is configured at all for this warehouse — independent of whether
    /// [`RemoteClient::from_config`] could actually *build* a client from it. A malformed
    /// `remote.url`/`remote.token` config read, or a URL [`RemoteClient::new`] rejects, still
    /// counts as "configured" here: something is set, it is just broken.
    ///
    /// Exists so a caller that gets `Err` from [`from_config`](RemoteClient::from_config) can
    /// tell "nothing is configured" apart from "something is configured but could not be
    /// consulted" without parsing that `Err`'s message — `forklift heal`'s own remote-driven
    /// refetch (`recovery_utils::attempt_heal_driven_refetch`) is the caller this exists for: the
    /// two cases call for different remedies (a genuinely absent remote has no in-tool fetch to
    /// retry; a broken-but-real one might still be recoverable once it is fixed), and reporting
    /// the second as the first sends a user toward heavier remedies — franchise, reproduce, or
    /// accepting the loss — for an object the remote may still actually have. A read failure here
    /// (the same class `from_config` itself would also hit) is treated as "configured": it is
    /// certainly not "nothing is set," and erring toward the less alarming classification is the
    /// right default for what is, either way, only wording.
    pub fn is_configured() -> bool {
        !matches!(config_utils::get_effective_value(config_utils::KEY_REMOTE_URL), Ok(None))
    }

    /// The base URL of the remote.
    pub fn url(&self) -> &str {
        &self.base
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.request_on(&self.http, method, path)
    }

    /// Build a request against this remote using a specific underlying `reqwest::Client` — the
    /// seam `fetch_info`/`fetch_signature`/`fetch_bundle_to` use to go out on
    /// [`RemoteClient::bounded_reads`], `fetch_object` uses to go out on
    /// [`RemoteClient::bounded_object_reads`], and `fetch_batch`'s initial `POST` uses to go out
    /// on [`RemoteClient::no_redirect`] — all instead of the unbounded default.
    fn request_on(&self,
                   http: &reqwest::Client,
                   method: reqwest::Method,
                   path: &str) -> reqwest::RequestBuilder {
        let mut builder = http.request(method, format!("{}{}", self.base, path));

        if let Some(token) = &self.token {
            builder = builder.bearer_auth(token);
        }

        builder
    }

    /// Walk a `std::error::Error` source chain to its root and return that root's own `Display`
    /// text. `reqwest::Error`'s own `Display` prints only its `Kind` plus the URL ("error sending
    /// request for url (...)") — the actual cause (connection refused, no such host, a TLS
    /// failure) lives in the `source()` chain and is otherwise silently dropped on the floor.
    /// `reqwest::Error::Debug` does include the chain, but as Rust struct-debug syntax, not
    /// something worth showing a CLI operator; this instead surfaces just the innermost cause's
    /// own message, which for an I/O failure is normally the OS's own wording (e.g. "Connection
    /// refused (os error 61)").
    fn root_cause(e: &(dyn std::error::Error + 'static)) -> String {
        let mut current = e;
        while let Some(source) = current.source() {
            current = source;
        }
        current.to_string()
    }

    /// Compose the client-facing message for a *transport* failure (one that never got as far as
    /// an HTTP status) on one of the four bounded read/metadata calls. Thin: all it does is read
    /// `is_connect()`/`is_timeout()` off `e` and hand them to [`classify`], then render the
    /// resulting [`TransportFailure`] — the actual case analysis lives there, pure and
    /// unit-tested over all four boolean combinations, because the combination this function
    /// cares least about (a genuine multi-minute kernel timeout on an already-established socket)
    /// is not practically constructible in a test at all; see [`TransportFailure::ReadTimedOut`]'s
    /// doc.
    fn describe_transport_error(&self,
                                action: &str,
                                silence_budget: std::time::Duration,
                                e: reqwest::Error) -> String {
        match classify(e.is_connect(), e.is_timeout()) {
            TransportFailure::ConnectTimedOut => format!(
                "Timed out while {}: could not connect to the remote within {:?}.",
                action, self.connect_timeout
            ),
            TransportFailure::ReadTimedOut => {
                let effective_budget = self.connect_timeout + silence_budget;
                format!(
                    "Timed out while {}: the remote did not respond within at least {:?}.",
                    action, effective_budget
                )
            }
            TransportFailure::Other => format!("Error while {}: {}", action, Self::root_cause(&e)),
        }
    }

    /// The mutation counterpart of [`Self::describe_transport_error`], for the six calls that ride
    /// the unbounded `http`/`no_redirect` clients (`update_ref`, `upload_object`, `put_presigned`,
    /// `upload_signature`, `put_trust`, `commit_lift`) — `upload_object` and `put_presigned` ride
    /// `no_redirect` specifically (moved off `http` in the fix for the `303` redirect hole — see
    /// [`Self::no_redirect`]'s doc), and also layer [`Self::send_with_watchdog`] on top for their
    /// body-send phase, but `no_redirect` is just as unbounded as `http` itself, so a transport
    /// failure on either still lands here for anything `classify` can actually see (a connect
    /// failure, or a `reqwest::Error`-bearing timeout on the response side). Same [`classify`]
    /// dispatch, but the [`TransportFailure::ReadTimedOut`] wording differs from the read path's:
    /// on these clients that case can only be a timeout on an *established* connection — after
    /// the request bytes were already sent — so the settled contract requires the uncertainty be
    /// carried in the message rather than asserted away: it may have completed on the remote, and
    /// the caller must decide whether to check before retrying, never be told nothing happened.
    fn describe_mutation_transport_error(&self, action: &str, e: reqwest::Error) -> String {
        match classify(e.is_connect(), e.is_timeout()) {
            TransportFailure::ConnectTimedOut => format!(
                "Timed out while {}: could not connect to the remote within {:?}. Nothing was \
                sent — safe to retry.",
                action, self.connect_timeout
            ),
            TransportFailure::ReadTimedOut => Self::mutation_read_timeout_message(action),
            TransportFailure::Other => format!("Error while {}: {}", action, Self::root_cause(&e)),
        }
    }

    /// The message for a mutation transport failure that can only be a timeout on an
    /// *already-established* connection — after the request bytes may already have been sent.
    /// Factored out of [`Self::describe_mutation_transport_error`]'s own `ReadTimedOut` arm so
    /// [`Self::send_with_watchdog`]'s upload watchdog can produce the identical wording without a
    /// `reqwest::Error` to hand `classify`: a stalled body-send stream it kills produces no
    /// `reqwest::Error` at all — the in-flight `send()` future is simply dropped, never polled to
    /// completion or failure — so there is nothing for `classify` to inspect. The watchdog names
    /// this case directly instead, since a body-send stall **is** this shape: the connection is
    /// established, and by the time silence trips the watchdog, request bytes may already be
    /// underway to the remote — the mutation-uncertainty wording applies exactly as it does to a
    /// real `reqwest::Error` of the same shape.
    ///
    /// "may or may not have fully reached" (review round S2-F7), not "was sent": `silent_for()`
    /// is measured from when [`UploadProgress`] is constructed, *before* `send()` is even called,
    /// so this can fire with zero chunks ever having been pulled — claiming the request "was
    /// sent" would overstate what is actually known in that case.
    fn mutation_read_timeout_message(action: &str) -> String {
        format!(
            "Timed out while {}: the request may or may not have fully reached the remote, and \
            no response arrived. It may have already completed there — re-running converges, so \
            retrying is safe.",
            action
        )
    }

    /// The message for a *post-send* mutation stall — the watchdog's second phase (see
    /// [`Self::send_with_watchdog`]): the stream reported itself exhausted (every chunk was
    /// handed to hyper), and then `budget` elapsed with no response. Deliberately distinct
    /// wording from [`Self::mutation_read_timeout_message`] (the mid-body composer) so an operator
    /// can tell the two failures apart, and because they are not equally uncertain: here every
    /// chunk really was handed off, not merely possibly so, so a message that failed to say the
    /// remote may already have verified and stored the bytes would be a false claim, not just an
    /// imprecise one. `re-uploading the same content is a no-op if it already landed` names the
    /// concrete reason retrying is safe: every upload in this module is content-addressed, so a
    /// repeat `PUT` of bytes the remote already has changes nothing.
    ///
    /// "the client finished streaming the request body" (review round S2-F7), not "the request
    /// finished sending": exhaustion means hyper has every chunk, not that the peer has received
    /// every byte — hyper's own buffer, and the OS kernel's send buffer beneath it, can still hold
    /// an unflushed tail at the instant this fires (see [`Self::send_with_watchdog`]'s doc, S2-F2)
    /// — so this states only the locally-observable fact, not a claim about what the remote has.
    fn mutation_post_send_timeout_message(action: &str, budget: std::time::Duration) -> String {
        format!(
            "Timed out while {}: the client finished streaming the request body, but no \
            response arrived within {:?}. The remote may already have received, verified, and \
            stored the bytes — so retrying is safe; re-uploading the same content is a no-op if \
            it already landed.",
            action, budget
        )
    }

    /// Send `builder` (with a [`watched_upload_body`]-built body of `body_len` bytes already
    /// attached, sharing `progress`) through two phases, both bounded, both watched through the
    /// same `progress.silent_for()` signal — just against a different threshold once the stream is
    /// exhausted:
    ///
    /// 1. **Send**: `silent_for()` is checked against `phase1_budget` (this client's connect
    ///    budget plus [`UPLOAD_SILENCE_BUDGET`]) while the body is still streaming. A stall here
    ///    means the peer stopped reading mid-transfer.
    /// 2. **Post-send response wait**: once the stream reports itself exhausted, `silent_for()` —
    ///    which stops advancing the instant the last chunk is handed off, since there is nothing
    ///    left to touch it — is checked against a *larger* `phase2_budget` instead:
    ///    `phase1_budget + post_send_verify_budget(body_len)`.
    ///
    /// An earlier version of this fix (review round S2-F2) treated exhaustion as "stop bounding
    /// the wait at all" — restoring the original FORK-49 hang on exactly this path, since a remote
    /// that reads the whole body and then wedges during verification is an ordinary failure. A
    /// second attempt then bounded phase 2 with `post_send_verify_budget(body_len)` alone — too
    /// tight: `is_exhausted()` means every chunk was handed to *hyper*, not that every byte
    /// reached the *peer* (see [`UploadProgress`]'s doc) — hyper's own buffer, and the OS kernel's
    /// send buffer beneath it, can still hold an in-flight tail the instant this flag flips, and
    /// under Linux/Windows autotuning that tail can be megabytes. A verify-only budget would kill
    /// a transfer that is still genuinely, if slowly, moving those bytes — the same "must never
    /// kill a transfer that is still moving" property [`UPLOAD_SILENCE_BUDGET`]'s own doc requires
    /// of phase 1. `phase2_budget` folds phase 1's *entire* allowance back in as the tail's flush
    /// budget: it is the same figure this client already treats as generous for a single healthy
    /// gap to resolve (see [`UPLOAD_SILENCE_BUDGET`]'s doc), and flushing an already-in-flight
    /// tail is exactly that — a healthy link finishing outstanding work, not new work starting
    /// from nothing — on top of which [`post_send_verify_budget`]`(body_len)` adds the
    /// server-side, client-known-bounded verification time. This also folds in this client's own
    /// connect budget (review round S2-F3), the same way [`bounded_read_timeout`] and phase 1
    /// itself already do — a bound that ignores the link's own latency preempts healthy work on a
    /// slow-but-legitimate connection (a Tor circuit most sharply: `REMOTE_CONNECT_TIMEOUT_TOR` is
    /// 60s precisely because multi-second round trips are normal there).
    ///
    /// **Accepted residual**, in the same spirit as [`FETCH_OBJECT_READ_TIMEOUT`]'s documented
    /// gap: an autotuned buffer that grew large while the link was fast and then degraded to a
    /// much slower rate mid-transfer could in principle still exceed `phase2_budget`'s flush
    /// allowance. TCP autotuning targets a buffer near the bandwidth-delay product, so the ordinary
    /// case (buffer sized for the link's *own* rate) drains in on the order of a few round trips,
    /// not the link's raw throughput — comfortably inside [`UPLOAD_SILENCE_BUDGET`]'s allowance —
    /// but a link that changes character mid-flight is not covered by that reasoning. Not
    /// separately fixed here: it would need either OS-level control of the send buffer (no public
    /// `reqwest`/hyper hook exists for it) or a budget scaled to a worst-case buffer size no
    /// portable assumption can name honestly.
    ///
    /// Both phases can end in a watchdog kill, and both produce no `reqwest::Error` when they do
    /// (the in-flight future is simply dropped, and reqwest/hyper tear down the underlying socket
    /// as part of that drop, so no connection is leaked) — so both compose their message directly
    /// ([`Self::mutation_read_timeout_message`] for phase 1,
    /// [`Self::mutation_post_send_timeout_message`] for phase 2) rather than through `classify`,
    /// which needs an actual `reqwest::Error` neither kill ever produces. A genuine
    /// `reqwest::Error` (a real transport failure, not a watchdog kill) still flows through
    /// `classify` via [`Self::describe_mutation_transport_error`] as before, from either phase.
    async fn send_with_watchdog(&self,
                                builder: reqwest::RequestBuilder,
                                progress: Arc<UploadProgress>,
                                action: &str,
                                body_len: usize) -> Result<reqwest::Response, String> {
        let phase1_budget = self.connect_timeout + UPLOAD_SILENCE_BUDGET;
        let phase2_budget = phase1_budget + post_send_verify_budget(body_len);
        let send_fut = builder.send();
        tokio::pin!(send_fut);

        loop {
            tokio::select! {
                result = &mut send_fut => {
                    return result.map_err(|e| self.describe_mutation_transport_error(action, e));
                }
                _ = tokio::time::sleep(UPLOAD_WATCHDOG_POLL_INTERVAL) => {
                    let exhausted = progress.is_exhausted();
                    let budget = if exhausted { phase2_budget } else { phase1_budget };

                    if progress.silent_for() >= budget {
                        return Err(if exhausted {
                            Self::mutation_post_send_timeout_message(action, phase2_budget)
                        } else {
                            Self::mutation_read_timeout_message(action)
                        });
                    }
                }
            }
        }
    }

    /// Compose a loud, specific error for a `3xx` response to a streamed upload `PUT` (review
    /// round S2-F5). Both call sites that reach this ([`Self::upload_object`],
    /// [`Self::put_presigned`]) go out on [`Self::no_redirect`], which never auto-follows *any*
    /// redirect status — so this is a local, unconditional invariant of this client, not a claim
    /// about how any particular `3xx` happens to behave under a dependency's redirect matrix (a
    /// `303` once slipped through here on the auto-following client precisely because that matrix
    /// has more than one case; see the fixed-hole review round for the history). A silent redirect
    /// must never look like success: an operator seeing a bare "refused (307)" would have no idea
    /// a redirect was even involved. Names the status and, when present, the `Location` header the
    /// remote pointed at, so the caller can see exactly what happened and ask the remote for a
    /// fresh target rather than guess.
    fn describe_upload_redirect(action: &str, response: &reqwest::Response) -> String {
        let status = response.status();
        let location = response.headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("(no Location header)");

        format!(
            "The remote redirected {} ({}) to {} — a streamed upload cannot follow a redirect \
            (its body cannot be replayed to retry elsewhere), so this failed instead of silently \
            retrying at the new location. Ask the remote for a fresh upload target and retry.",
            action, status.as_u16(), location
        )
    }

    /// Turn a non-success response into the client-facing error, threading the server's refusal
    /// code (§7.4) through the taxonomy when the body carries one. The body read is bounded by
    /// [`error_body_read_budget`] (`&self`, review round 5 finding 2, so this instance's own
    /// `connect_timeout` is folded in) — see that function's doc for why this call in particular
    /// needs its own bound rather than inheriting one from whichever client sent the request, and
    /// why the flat [`ERROR_BODY_READ_TIMEOUT`] alone is not it.
    async fn error_of(&self, response: reqwest::Response, action: &str) -> String {
        let status = response.status();
        let budget = error_body_read_budget(self.connect_timeout);

        let (message, code, next_step) = match tokio::time::timeout(
            budget, response.json::<ErrorResponse>()
        ).await {
            Ok(Ok(body)) => (body.error, body.code, body.next_step),
            Ok(Err(_)) | Err(_) => (status.canonical_reason().unwrap_or("unknown error").to_string(), None, None),
        };

        classify_remote_error(status.as_u16(), action, message, code, next_step)
    }

    /// Fetch the warehouse handshake and check the protocol version.
    pub async fn fetch_info(&self) -> Result<WarehouseInfo, String> {
        let response = self.request_on(&self.bounded_reads, reqwest::Method::GET, "/v1/warehouse")
            .send()
            .await
            .map_err(|e| self.describe_transport_error(
                &format!("reaching the remote {}", self.base), REMOTE_READ_TIMEOUT, e
            ))?;

        if !response.status().is_success() {
            return Err(self.error_of(response, "the handshake").await);
        }

        let info: WarehouseInfo = response.json()
            .await
            .map_err(|e| if e.is_timeout() {
                self.describe_transport_error(
                    &format!("reaching the remote {}", self.base), REMOTE_READ_TIMEOUT, e
                )
            } else {
                format!("The remote's handshake is not valid JSON: {}", e)
            })?;

        if info.protocol != PROTOCOL_VERSION {
            return Err(format!(
                "The remote speaks protocol version \"{}\", this build speaks \"{}\". \
                Update the older side.",
                info.protocol, PROTOCOL_VERSION
            ));
        }

        Ok(info)
    }

    /// Ask which of the given objects the remote lacks (batched).
    ///
    /// Deliberately **not** on [`Self::bounded_reads`]: the server side consults up to
    /// `MAX_MISSING_BATCH` (10,000) hashes before its first response byte, work that scales with
    /// the batch rather than being O(constant) — the settled contract puts that in the
    /// scaled/measured-budget category, not the flat one `REMOTE_READ_TIMEOUT` is honest for.
    /// Likely fast in practice, but "probably fine" is exactly what the contract exists to stop
    /// us asserting; a real budget for this is a later slice.
    pub async fn missing_objects(&self, hashes: &[String]) -> Result<Vec<String>, String> {
        let mut missing: Vec<String> = Vec::new();

        for batch in hashes.chunks(MAX_MISSING_BATCH) {
            let response = self.request(reqwest::Method::POST, "/v1/objects/missing")
                .json(&MissingObjectsRequest { hashes: batch.to_vec() })
                .send()
                .await
                .map_err(|e| format!("Error while negotiating with the remote: {}", e))?;

            if !response.status().is_success() {
                return Err(self.error_of(response, "the negotiation").await);
            }

            let body: MissingObjectsResponse = response.json()
                .await
                .map_err(|e| format!("The remote's negotiation response is not valid JSON: {}", e))?;

            missing.extend(body.missing);
        }

        Ok(missing)
    }

    /// Resolve operator identifiers to display names through the server
    /// (`POST /v1/resolve`). Best-effort by the resolution failure policy: a server
    /// without a resolution hook (or that predates the endpoint, a `404`), an
    /// unreachable remote, or a malformed answer all resolve to an empty map — the
    /// caller shows the pseudonymous identifiers. The *server* decides which names
    /// this caller may see (§8.12); the client only asks.
    pub async fn resolve(&self, identifiers: Vec<String>) -> BTreeMap<String, String> {
        if identifiers.is_empty() {
            return BTreeMap::new();
        }

        let response = self.request(reqwest::Method::POST, "/v1/resolve")
            // A slow or black-holed remote must never hang a display command; the
            // fallback is pseudonyms anyway.
            .timeout(std::time::Duration::from_secs(5))
            .json(&ResolveRequest { identifiers })
            .send()
            .await;

        let Ok(response) = response else {
            return BTreeMap::new();
        };

        if !response.status().is_success() {
            return BTreeMap::new();
        }

        match response.json::<ResolveResponse>().await {
            Ok(body) => body.names,
            Err(_) => BTreeMap::new(),
        }
    }

    /// Fetch many objects in one round trip as a bundle-format stream
    /// (`POST /v1/objects/batch`). `None` when the remote predates the endpoint
    /// (a `404`) — the caller falls back to loose fetches.
    ///
    /// An offloading (storage-backed) head cannot stream a large bundle back through its own
    /// control plane, so it answers this `POST` with a redirect to a presigned `GET` of the
    /// bundle bytes under an ephemeral response key (`303 See Other` from a fixed head; a
    /// `307`/`308` from an older one is followed identically). The redirect is followed **by
    /// hand**, never by reqwest's automatic policy (this call goes out on [`Self::no_redirect`]
    /// for exactly that reason): a `307`/`308` replays the original request verbatim — method
    /// and JSON body — which would re-`POST` this call's body at a URL SigV4-signed for `GET`
    /// only, failing signature verification (`500` on LocalStack, `403 SignatureDoesNotMatch`
    /// on real AWS) rather than fetching anything. The follow-up `GET` also deliberately omits
    /// this remote's `Authorization` header: the presigned URL is self-authorizing, and
    /// forwarding a bearer token meant for the control plane to a storage host it was never
    /// issued for would be a needless credential leak.
    ///
    /// Deliberately **not** on [`Self::bounded_reads`]: the server builds the whole requested
    /// bundle — every object fully into memory — before its first response byte
    /// (`forklift-server/src/server.rs`'s `objects/batch` handler), and that cost depends on the
    /// byte sizes of objects this client doesn't have yet, which it cannot know in advance. No
    /// flat budget is honest here; this must stay unbounded until it has its own scaled budget or
    /// an abandon-and-fall-back lane, so a cold-cache multi-MB batch that works today keeps
    /// working rather than hard-failing identically on every retry.
    pub async fn fetch_batch(&self, hashes: &[String]) -> Result<Option<Vec<u8>>, String> {
        let response = self.request_on(&self.no_redirect, reqwest::Method::POST, "/v1/objects/batch")
            .json(&MissingObjectsRequest { hashes: hashes.to_vec() })
            .send()
            .await
            .map_err(|e| format!("Error while batch-fetching from the remote: {}", e))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let response = match response.status() {
            reqwest::StatusCode::SEE_OTHER
            | reqwest::StatusCode::TEMPORARY_REDIRECT
            | reqwest::StatusCode::PERMANENT_REDIRECT => {
                let location = response.headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| {
                        "The remote's batch redirect carried no usable Location header.".to_string()
                    })?
                    .to_string();

                // A bare GET: no Authorization header (the URL is self-authorizing) and no
                // body — the request the redirect target is actually presigned for.
                self.http.get(&location)
                    .send()
                    .await
                    .map_err(|e| format!("Error while following the batch redirect: {}", e))?
            }
            _ => response,
        };

        if !response.status().is_success() {
            return Err(self.error_of(response, "the batch fetch").await);
        }

        response.bytes()
            .await
            .map(|bytes| Some(bytes.to_vec()))
            .map_err(|e| format!("Error while reading the batch response: {}", e))
    }

    /// Fetch the object closure of a subtree at a path of a parcel, as a bundle-format stream
    /// (`GET /v1/parcels/{parcel}/subtree/{path}`). This is the **path-addressed** fetch: the
    /// remote resolves the path to a subtree itself, so it can authorize the request by path —
    /// the wire surface file-level path enforcement (FORK-10) is designed to gate, which a
    /// hash-addressed `GET /v1/objects/{hash}` cannot, being path-blind. `Ok(None)` when the
    /// remote predates the endpoint (a `404`/`405`) or refused because the resolved subtree
    /// exceeds the remote's per-response object cap (`422`, the same cap `objects/batch`
    /// enforces) — both cases share one fallback: the caller walks the shipped hash-addressed
    /// scoped fetch instead, which has no such single-response limit. That fallback is why
    /// shipping this endpoint needs no protocol bump.
    ///
    /// # Arguments
    /// * `parcel` - The parcel whose tree the path is resolved in.
    /// * `path`   - The warehouse path key of the subtree (`/`-separated, e.g. `src/api`).
    ///
    /// Deliberately **not** on [`Self::bounded_reads`]: the server side walks and buffers the
    /// whole resolved subtree closure into memory before its first response byte
    /// (`forklift-server/src/server.rs`'s `get_subtree` handler notes an uncapped closure "would
    /// buffer an arbitrarily large bundle in memory"), cost the client cannot bound in advance.
    /// Same reasoning as `fetch_batch` — this stays unbounded until it has its own scaled budget
    /// or an abandon-and-fall-back lane.
    pub async fn fetch_subtree(&self, parcel: &str, path: &str) -> Result<Option<Vec<u8>>, String> {
        let response = self.request(reqwest::Method::GET, &format!(
            "/v1/parcels/{}/subtree/{}", parcel, encode_path_segments(path)
        )).send()
            .await
            .map_err(|e| format!("Error while fetching subtree \"{}\" from the remote: {}", path, e))?;

        if endpoint_absent(response.status()) || response.status() == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
            return Ok(None);
        }

        if !response.status().is_success() {
            return Err(self.error_of(response, &format!("the subtree fetch for \"{}\"", path)).await);
        }

        response.bytes()
            .await
            .map(|bytes| Some(bytes.to_vec()))
            .map_err(|e| format!("Error while reading the subtree response: {}", e))
    }

    /// Fetch one object's raw bytes. On [`Self::bounded_object_reads`], not [`Self::bounded_reads`]
    /// — see [`FETCH_OBJECT_READ_TIMEOUT`]'s doc for why this call alone needs the looser budget.
    pub async fn fetch_object(&self, hash: &str) -> Result<Vec<u8>, String> {
        let response = self.request_on(&self.bounded_object_reads, reqwest::Method::GET, &format!("/v1/objects/{}", hash))
            .send()
            .await
            .map_err(|e| self.describe_transport_error(
                &format!("fetching object {}", hash), FETCH_OBJECT_READ_TIMEOUT, e
            ))?;

        if !response.status().is_success() {
            return Err(self.error_of(response, &format!("object {}", hash)).await);
        }

        response.bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|e| self.describe_transport_error(
                &format!("reading object {}", hash), FETCH_OBJECT_READ_TIMEOUT, e
            ))
    }

    /// Upload one object's raw bytes to the control plane (`PUT /v1/objects/{hash}`), where the
    /// remote verifies the hash inline before the object becomes fetchable. This is the direct
    /// path — for the objects `upload-targets` returns in `direct`, and the whole missing set on
    /// the legacy fallback.
    ///
    /// The body streams through [`watched_upload_body`]/[`Self::send_with_watchdog`] (FORK-49
    /// slice 2): a remote that accepts the connection and then never reads the body must not hang
    /// this call forever, but a per-request *total* deadline would kill a healthy large upload on
    /// a slow link — the same reasoning [`REMOTE_READ_TIMEOUT`]'s doc gives for the read path,
    /// applied to the send side instead of the receive side. A remote that reads the whole body
    /// and then wedges (during the inline hash-verify this endpoint's own doc names above) is
    /// bounded too — see [`Self::send_with_watchdog`]'s doc for that phase's own, size-scaled
    /// bound instead of the unbounded wait an earlier version of this fix left it with.
    pub async fn upload_object(&self, hash: &str, bytes: Vec<u8>) -> Result<(), String> {
        let action = format!("uploading object {}", hash);
        let progress = UploadProgress::new();
        let (body, len) = watched_upload_body(bytes, progress.clone());
        let builder = self.request_on(&self.no_redirect, reqwest::Method::PUT, &format!("/v1/objects/{}", hash))
            .header(reqwest::header::CONTENT_LENGTH, len)
            .body(body);

        let response = self.send_with_watchdog(builder, progress, &action, len).await?;

        if response.status().is_redirection() {
            return Err(Self::describe_upload_redirect(&action, &response));
        }

        if !response.status().is_success() {
            return Err(self.error_of(response, &format!("object {}", hash)).await);
        }

        Ok(())
    }

    /// Negotiate where to upload the given objects (`POST /v1/objects/upload-targets`, batched
    /// at the protocol cap). `Ok(None)` when the remote predates the endpoint (a `404`/`405`) —
    /// the caller falls back to `missing` + a per-object control-plane `PUT`.
    ///
    /// A storage-backed head answers `targets` (presigned staging `PUT` URLs) for what it wants
    /// staged and `direct` for what it verifies inline; a direct head answers every missing hash
    /// in `direct` with empty `targets`, so one client code path serves both heads. `present`
    /// (the complement of `missing`) is skipped.
    ///
    /// Deliberately **not** on [`Self::bounded_reads`], the same reasoning as
    /// [`Self::missing_objects`]: the server side (`forklift-server/src/server.rs`'s
    /// `post_upload_targets`) walks up to `MAX_UPLOAD_TARGETS_BATCH` (1,000) hashes, checking each
    /// against on-disk object presence, before its first response byte — work that scales with the
    /// batch rather than being O(constant). Same later-slice caveat as `missing_objects`: likely
    /// fast in practice, but not asserted bounded here.
    pub async fn upload_targets(&self,
                                session: &str,
                                hashes: &[String]) -> Result<Option<UploadTargetsResponse>, String> {
        let mut merged = UploadTargetsResponse {
            present: Vec::new(),
            targets: BTreeMap::new(),
            direct: Vec::new(),
        };

        for batch in hashes.chunks(MAX_UPLOAD_TARGETS_BATCH) {
            let response = self.request(reqwest::Method::POST, "/v1/objects/upload-targets")
                .json(&UploadTargetsRequest { session: session.to_string(), hashes: batch.to_vec() })
                .send()
                .await
                .map_err(|e| format!("Error while negotiating upload targets: {}", e))?;

            if endpoint_absent(response.status()) {
                return Ok(None);
            }

            if !response.status().is_success() {
                return Err(self.error_of(response, "the upload negotiation").await);
            }

            let body: UploadTargetsResponse = response.json()
                .await
                .map_err(|e| format!("The remote's upload-targets response is not valid JSON: {}", e))?;

            merged.present.extend(body.present);
            merged.targets.extend(body.targets);
            merged.direct.extend(body.direct);
        }

        Ok(Some(merged))
    }

    /// Upload one object's bytes straight to a presigned storage URL (a staging `PUT`). The
    /// URL's own signature is the authorization, so this deliberately carries **no** bearer
    /// token — and because the bearer is attached per request (in `request`/`request_on`, never
    /// as a client default header), `self.no_redirect.put(url)` (moved off `self.http` in the fix
    /// for the `303` redirect hole — see [`Self::no_redirect`]'s doc) cannot leak it to the
    /// storage host either, even were the storage host the remote itself: `no_redirect` is built
    /// (in [`Self::new_with_tor`]) with no default headers of its own, exactly like `http`.
    ///
    /// Same watchdog-guarded body as [`Self::upload_object`] (FORK-49 slice 2) — see that call's
    /// doc. This site is the higher-risk of the two: it dials a different host (object storage,
    /// not the control plane) and the explicit `Content-Length` [`watched_upload_body`] requires
    /// the caller to set matters especially here — a presigned S3 `PUT` is signed for a specific
    /// framing, and `Transfer-Encoding: chunked` (what `reqwest`/hyper fall back to without an
    /// explicit length on a streamed body) is not it; S3 rejects a chunked presigned `PUT` outright.
    async fn put_presigned(&self, url: &str, bytes: Vec<u8>) -> Result<(), String> {
        let progress = UploadProgress::new();
        let (body, len) = watched_upload_body(bytes, progress.clone());
        let builder = self.no_redirect.put(url)
            .header(reqwest::header::CONTENT_LENGTH, len)
            .body(body);

        let response = self.send_with_watchdog(
            builder, progress, "uploading to a staging URL", len
        ).await?;

        if response.status().is_redirection() {
            return Err(Self::describe_upload_redirect("uploading to a staging URL", &response));
        }

        if !response.status().is_success() {
            return Err(format!(
                "A staged upload was refused by object storage ({}).",
                response.status().as_u16()
            ));
        }

        Ok(())
    }

    /// One `POST /v1/lift/{session}/commit` attempt: ask a storage-backed head to verify and
    /// promote the session's staged control-plane objects and presence-check its blobs, before
    /// the ref update. `Ok(Committed)` when the session is ready; `Ok(BlobNotReady)` for the one
    /// transient case — a blob the staging verifier has not promoted yet, which the caller
    /// retries with backoff; `Err` for a terminal failure (a corrupt staged object, a
    /// control-plane object never uploaded, or a transport error).
    async fn commit_lift(&self,
                         session: &str,
                         control_plane: &[String],
                         blobs: &[String],
                         more: bool) -> Result<CommitOutcome, String> {
        let body = CommitLiftRequest {
            control_plane: control_plane.to_vec(),
            blobs: blobs.to_vec(),
            more,
        };

        let response = self.request(reqwest::Method::POST, &format!("/v1/lift/{}/commit", session))
            .json(&body)
            .send()
            .await
            .map_err(|e| self.describe_mutation_transport_error("committing the lift session", e))?;

        if response.status().is_success() {
            return Ok(CommitOutcome::Committed);
        }

        let status = response.status();
        let message = match response.json::<ErrorResponse>().await {
            Ok(body) => body.error,
            Err(_) => status.canonical_reason().unwrap_or("unknown error").to_string(),
        };

        if is_transient_commit_failure(status, &message) {
            return Ok(CommitOutcome::BlobNotReady);
        }

        Err(format!("The remote refused the lift commit ({}): {}", status.as_u16(), message))
    }

    /// Fetch a parcel's signature sidecar (`None` for unsigned parcels).
    pub async fn fetch_signature(&self, parcel_hash: &str) -> Result<Option<Vec<u8>>, String> {
        let response = self.request_on(&self.bounded_reads, reqwest::Method::GET, &format!("/v1/signatures/{}", parcel_hash))
            .send()
            .await
            .map_err(|e| self.describe_transport_error(
                &format!("fetching the signature of {}", parcel_hash), REMOTE_READ_TIMEOUT, e
            ))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !response.status().is_success() {
            return Err(self.error_of(response, &format!("the signature of {}", parcel_hash)).await);
        }

        response.bytes()
            .await
            .map(|bytes| Some(bytes.to_vec()))
            .map_err(|e| self.describe_transport_error(
                &format!("reading the signature of {}", parcel_hash), REMOTE_READ_TIMEOUT, e
            ))
    }

    /// Upload a parcel's signature sidecar.
    pub async fn upload_signature(&self, parcel_hash: &str, bytes: Vec<u8>) -> Result<(), String> {
        let response = self.request(reqwest::Method::PUT, &format!("/v1/signatures/{}", parcel_hash))
            .body(bytes)
            .send()
            .await
            .map_err(|e| self.describe_mutation_transport_error(
                &format!("uploading the signature of {}", parcel_hash), e
            ))?;

        if !response.status().is_success() {
            return Err(self.error_of(response, &format!("the signature of {}", parcel_hash)).await);
        }

        Ok(())
    }

    /// Establish the trust anchor on the remote (idempotent for an identical anchor).
    pub async fn put_trust(&self, anchor: &TrustAnchorDto) -> Result<(), String> {
        let response = self.request(reqwest::Method::PUT, "/v1/trust")
            .json(anchor)
            .send()
            .await
            .map_err(|e| self.describe_mutation_transport_error("uploading the trust anchor", e))?;

        if !response.status().is_success() {
            return Err(self.error_of(response, "the trust anchor").await);
        }

        Ok(())
    }

    /// Commit a ref update (the CAS of a lift).
    pub async fn update_ref(&self,
                            pallet: &str,
                            old_head: Option<&str>,
                            new_head: &str) -> Result<(), String> {
        let body = RefUpdateRequest {
            old_head: old_head.map(|hash| hash.to_string()),
            new_head: new_head.to_string(),
        };

        let response = self.request(reqwest::Method::POST, &format!("/v1/pallets/{}", pallet))
            .json(&body)
            .send()
            .await
            .map_err(|e| self.describe_mutation_transport_error(
                &format!("moving the remote pallet \"{}\"", pallet), e
            ))?;

        if !response.status().is_success() {
            return Err(self.error_of(response, &format!("moving pallet \"{}\"", pallet)).await);
        }

        Ok(())
    }

    /// Download the remote's latest bundle into a file. On [`Self::bounded_reads`]: the bundle
    /// is a **pre-built** file (whatever `forklift compact`/the equivalent server-side job last
    /// produced), so the server's pre-first-byte work is O(constant) — serving an already-built
    /// file — exactly the category [`REMOTE_READ_TIMEOUT`]'s flat budget is honest for. The
    /// chunked `response.chunk()` loop below is precisely where the per-read, resets-on-progress
    /// half of `read_timeout` matters: a bundle can be large, and a slow-but-steady download must
    /// never be treated as a stall.
    ///
    /// Downloads to a fresh temp file beside `path` and only renames it into place once every
    /// chunk has landed: `path` is this warehouse's *own* latest bundle, which
    /// `forklift serve` would otherwise hand out as-is, and any failure along the way — the new
    /// timeout very much included — must never leave a truncated (or otherwise stray) file
    /// sitting at the real name. [`RemoveTempOnDrop`] guards every early return between creating
    /// the temp file and the rename succeeding, not just the download loop, so `sync_all` or the
    /// rename itself failing can never leave the temp copy behind either — a manual
    /// `if let Err(e) = ... { remove_file; return Err(e) }` around only the download loop, which
    /// an earlier version of this function used, does not cover those.
    ///
    /// The directory sync after the rename goes through [`file_utils::sync_dir_or_taint`], not
    /// the plain [`file_utils::sync_dir`] every other call in this file uses for a `bounded_reads`
    /// response: unlike those, this rename publishes a file at a durable, reused name inside
    /// `forklift_root()` exactly like every other rename-into-place in the store
    /// (`write_file_atomically`, pack publication) — the file is visible at `path` the instant
    /// the rename returns, so a directory-sync failure right after leaves a *visible-but-not-
    /// durability-proven* bundle with no record of that fact unless a taint is recorded, same as
    /// any other object-store rename. `franchise` propagates a bare `Err` from this function
    /// straight out of the whole command (its `?` bypasses the loose-object fallback walk
    /// entirely — that fallback is only reached on `Ok(false)`/an unsupported-format import
    /// error, never on a transport-level `Err` from this call) — so treating a lost bundle here
    /// as "only an optimization" would be wrong for exactly the failure this fix introduces.
    ///
    /// # Returns
    /// * `Ok(true)`    - The bundle was downloaded.
    /// * `Ok(false)`   - The remote has no bundle.
    /// * `Err(String)` - On any other failure.
    pub async fn fetch_bundle_to(&self, path: &std::path::Path) -> Result<bool, String> {
        let mut response = self.request_on(&self.bounded_reads, reqwest::Method::GET, "/v1/bundles/latest")
            .send()
            .await
            .map_err(|e| self.describe_transport_error("fetching the bundle", REMOTE_READ_TIMEOUT, e))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }

        if !response.status().is_success() {
            return Err(self.error_of(response, "the bundle").await);
        }

        // The same temp-path naming `write_file_atomically` uses elsewhere in the store — unique
        // per write, in the same directory as the final name, so the eventual rename is a same-
        // filesystem metadata-only operation. Not `write_file_atomically` itself: that takes the
        // full content up front, which would put the whole bundle back in memory — exactly what
        // streaming via `response.chunk()` exists to avoid.
        let temp_path = file_utils::temp_path_for(path)?;
        let mut file = std::fs::File::create(&temp_path)
            .map_err(|e| format!("Error while creating the bundle file: {}", e))?;
        let mut cleanup = RemoveTempOnDrop::armed(&temp_path);

        while let Some(chunk) = response.chunk()
            .await
            .map_err(|e| self.describe_transport_error("downloading the bundle", REMOTE_READ_TIMEOUT, e))?
        {
            std::io::Write::write_all(&mut file, &chunk)
                .map_err(|e| format!("Error while writing the bundle file: {}", e))?;
        }

        // Fsync via the still-open write handle, never by reopening `temp_path` — a handle
        // reopened after the fact cannot force a durable fsync on every platform this runs on.
        if file_utils::fsync_enabled() {
            file.sync_all()
                .map_err(|e| format!("Error while syncing the bundle file: {}", e))?;
        }
        drop(file);

        std::fs::rename(&temp_path, path).map_err(|e| format!(
            "Error while moving the bundle into place at \"{}\": {}", path.to_string_lossy(), e
        ))?;

        // The rename succeeded: `temp_path` no longer names anything, so there is nothing left
        // for the guard to clean up regardless of what happens next.
        cleanup.disarm();

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                file_utils::sync_dir_or_taint(parent, &[path])?;
            }
        }

        Ok(true)
    }
}

/// Removes the temp file at `path` when dropped, unless [`Self::disarm`] was called first.
/// [`RemoteClient::fetch_bundle_to`]'s one use of this guards *every* early return between
/// creating its temp file and the rename that publishes it succeeding — not just the download
/// loop — so a later failure (`sync_all`, the rename itself) can never leave the temp copy behind
/// either.
struct RemoveTempOnDrop<'a> {
    path: &'a std::path::Path,
    armed: bool,
}

impl<'a> RemoveTempOnDrop<'a> {
    fn armed(path: &'a std::path::Path) -> RemoveTempOnDrop<'a> {
        RemoveTempOnDrop { path, armed: true }
    }

    /// Call once the temp file has been consumed (renamed away) so `Drop` becomes a no-op —
    /// there is nothing left at `path` to remove, and nothing wrong with there not being.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RemoveTempOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(self.path);
        }
    }
}

/// Fetch everything reachable from a parcel head that is missing locally: the parcel
/// graph, every parcel's signature sidecar, and the full tree/blob closure — verified
/// object by object before storing.
///
/// The walk stops at any parcel already reachable from a **local ref head** — every pallet
/// and every meta pallet (`@office`, `@haul`, …), since they share one object store and a
/// ref of either kind is an equally good witness. Their closures are complete by
/// construction: a ref only moves once its objects are all present (a `stack` writes them
/// first; a `lower` or `franchise` fetches the whole closure before the fast-forward). So a
/// lower that brings one new parcel walks one parcel, not the whole history — the
/// transfer-economics half of the bounded-negotiation guarantee.
///
/// It still heals an interrupted earlier sync. An interruption leaves the ref where it was,
/// so the objects it half-fetched sit *above* the bound and are re-walked exactly as before.
/// What is no longer re-walked is history behind a ref, which was proven complete when that
/// ref moved. (`audit` is what re-proves a whole history; this is a fetch, not an audit.)
///
/// The old walk also re-probed the remote for the signature of every unsigned parcel on
/// every sync, since "no sidecar here" is indistinguishable from "not fetched yet". Behind
/// the bound, it no longer asks.
///
/// # Arguments
/// * `client` - The remote.
/// * `head`   - The parcel hash to fetch from.
///
/// # Returns
/// * `Ok(FetchStats)` - What was actually transferred, and how many parcels were walked.
/// * `Err(String)`    - If a transfer or verification failed.
pub async fn fetch_history(client: &RemoteClient, head: &str) -> Result<FetchStats, String> {
    let mut stats = FetchStats::default();

    // Every local ref head — user pallets and meta pallets alike — and therefore every
    // closure already known complete. Empty for a franchise into a fresh warehouse, which
    // walks everything, as it must.
    let complete: Vec<String> = pallet_utils::all_pallet_refs()?
        .into_iter()
        .map(|(_, head)| head)
        .collect();

    let mut parcel_frontier: Vec<String> = vec![head.to_string()];
    let mut seen_parcels: HashSet<String> = HashSet::new();
    let mut seen_trees: HashSet<String> = HashSet::new();
    let mut seen_blobs: HashSet<String> = HashSet::new();

    while !parcel_frontier.is_empty() {
        let candidates: Vec<String> = parcel_frontier.drain(..)
            .filter(|hash| seen_parcels.insert(hash.clone()))
            .collect();

        let mut wave: Vec<String> = Vec::new();

        for hash in candidates {
            if !is_known_complete(&hash, &complete)? {
                wave.push(hash);
            }
        }

        if wave.is_empty() {
            continue;
        }

        stats.walked_parcels += wave.len();
        stats.fetched_objects += fetch_missing_objects(client, &wave).await?;
        stats.fetched_signatures += fetch_missing_signatures(client, &wave).await?;

        // The parcels are present now; their trees and parents drive the next waves.
        let mut tree_frontier: Vec<String> = Vec::new();

        for hash in &wave {
            let parcel = object_utils::load_parcel(hash)?;

            tree_frontier.push(parcel.tree_hash.clone());
            parcel_frontier.extend(parcel.parents);
        }

        while !tree_frontier.is_empty() {
            let tree_wave: Vec<String> = tree_frontier.drain(..)
                .filter(|hash| seen_trees.insert(hash.clone()))
                .collect();

            if tree_wave.is_empty() {
                continue;
            }

            stats.fetched_objects += fetch_missing_objects(client, &tree_wave).await?;

            let mut blob_wave: Vec<String> = Vec::new();
            let mut recipe_wave: Vec<String> = Vec::new();

            for tree_hash in &tree_wave {
                let tree = object_utils::load_tree(tree_hash)?;

                for (_, file) in tree.get_files() {
                    if seen_blobs.insert(file.hash.clone()) {
                        blob_wave.push(file.hash.clone());

                        // A chunked file's entry names a recipe: fetch it with the blob wave, then
                        // descend it below for its chunks (which no bundle or blob wave carries).
                        if file.item_type.is_chunked() {
                            recipe_wave.push(file.hash.clone());
                        }
                    }
                }

                for (_, subtree) in tree.get_subtrees() {
                    tree_frontier.push(subtree.hash.clone());
                }
            }

            stats.fetched_objects += fetch_missing_objects(client, &blob_wave).await?;
            stats.fetched_objects += fetch_recipe_chunks(client, &recipe_wave).await?;
        }
    }

    Ok(stats)
}

/// Fetch a parcel head's history like [`fetch_history`], but path-prune the **content** walk
/// to a fetch `scope`: the full parcel graph, every signature, and the tree spine down to each
/// in-scope prefix are fetched, along with the in-scope subtrees and blobs in full; out-of-scope
/// subtree objects and blobs are skipped — they stay sealed by the hash the spine tree already
/// carries. Only the user pallet's content is fetched this way; office and other meta pallets
/// keep routing through the unscoped [`fetch_history`], because their audit reads full content.
///
/// A full (empty) scope is the whole store, so this delegates to [`fetch_history`] verbatim —
/// a full franchise or lower stays byte-for-byte identical, and the pruning below runs only for
/// a genuinely sparse warehouse.
///
/// Like [`fetch_history`], it heals an interrupted earlier sync: the ref is unmoved until the
/// whole scoped closure is present, so re-running re-walks only what is still missing (the
/// fetch primitives skip objects already on disk).
///
/// # Arguments
/// * `client` - The remote.
/// * `head`   - The parcel hash to fetch from.
/// * `scope`  - The warehouse fetch scope (the in-scope path prefixes).
///
/// # Returns
/// * `Ok(FetchStats)` - What was actually transferred, and how many parcels were walked.
/// * `Err(String)`    - If a transfer or verification failed.
pub async fn fetch_history_scoped(client: &RemoteClient,
                                  head: &str,
                                  scope: &MaterializationScope) -> Result<FetchStats, String> {
    if scope.is_full() {
        return fetch_history(client, head).await;
    }

    // Bound the walk at local ref heads, exactly like `fetch_history`: a closure already known
    // complete at this scope needs neither fetching nor walking.
    let complete: Vec<String> = pallet_utils::all_pallet_refs()?
        .into_iter()
        .map(|(_, head)| head)
        .collect();

    fetch_scoped_from(client, head, scope, &complete).await
}

/// Fetch the content newly brought into `scope` across a head's whole history — the walk behind
/// `expand`. Unlike [`fetch_history_scoped`], it is **not** bounded at local ref heads: widening
/// the scope invalidates the "reachable from a ref ⟹ closure complete" invariant for the
/// newly in-scope paths (that content was sealed, not fetched, behind those very refs), so the
/// history is re-walked in full. The fetch primitives still skip every object already on disk, so
/// only the genuinely newly in-scope objects transfer.
///
/// # Arguments
/// * `client` - The remote.
/// * `head`   - The parcel hash to widen from.
/// * `scope`  - The widened fetch scope.
///
/// # Returns
/// * `Ok(FetchStats)` - What was actually transferred.
/// * `Err(String)`    - If a transfer or verification failed.
pub async fn fetch_expanded(client: &RemoteClient,
                            head: &str,
                            scope: &MaterializationScope) -> Result<FetchStats, String> {
    if scope.is_full() {
        return fetch_history(client, head).await;
    }

    fetch_scoped_from(client, head, scope, &[]).await
}

/// The shared path-pruned walk behind [`fetch_history_scoped`] and [`fetch_expanded`]: fetch the
/// full parcel graph and signatures, the tree spine to each in-scope prefix, and the in-scope
/// subtrees and blobs in full, sealing out-of-scope objects by hash. `complete` bounds the
/// parcel walk at closures already known complete at this scope (empty for a full re-walk).
async fn fetch_scoped_from(client: &RemoteClient,
                           head: &str,
                           scope: &MaterializationScope,
                           complete: &[String]) -> Result<FetchStats, String> {
    let mut stats = FetchStats::default();

    let mut parcel_frontier: Vec<String> = vec![head.to_string()];
    let mut seen_parcels: HashSet<String> = HashSet::new();

    // Two dedup ledgers, kept apart on purpose. A spine node's classification depends on its
    // *path* (the same tree hash at two paths seals different siblings), so spine visits are keyed
    // by (hash, path). An in-scope subtree's whole closure is fetched regardless of where it
    // sits, so it is keyed by hash alone.
    let mut walked_spine: HashSet<(String, String)> = HashSet::new();
    let mut walked_full: HashSet<String> = HashSet::new();
    let mut seen_blobs: HashSet<String> = HashSet::new();

    while !parcel_frontier.is_empty() {
        let candidates: Vec<String> = parcel_frontier.drain(..)
            .filter(|hash| seen_parcels.insert(hash.clone()))
            .collect();

        let mut wave: Vec<String> = Vec::new();

        for hash in candidates {
            if !is_known_complete(&hash, complete)? {
                wave.push(hash);
            }
        }

        if wave.is_empty() {
            continue;
        }

        stats.walked_parcels += wave.len();
        stats.fetched_objects += fetch_missing_objects(client, &wave).await?;
        stats.fetched_signatures += fetch_missing_signatures(client, &wave).await?;

        // Each parcel's root tree is a spine node (path ""); descend the spine, collecting the
        // in-scope subtree roots whose full closure the batched walk below fetches.
        let mut spine_frontier: Vec<(String, String)> = Vec::new();
        let mut in_scope_roots: Vec<String> = Vec::new();

        for hash in &wave {
            let parcel = object_utils::load_parcel(hash)?;
            spine_frontier.push((parcel.tree_hash.clone(), String::new()));
            parcel_frontier.extend(parcel.parents);
        }

        // The spine is narrow (the depth to each in-scope prefix), so this sequential descent is
        // cheap; the parallel bulk is the in-scope closure walk that follows.
        while let Some((tree_hash, path)) = spine_frontier.pop() {
            if !walked_spine.insert((tree_hash.clone(), path.clone())) {
                continue;
            }

            stats.fetched_objects += fetch_missing_objects(client, std::slice::from_ref(&tree_hash)).await?;

            let tree = object_utils::load_tree(&tree_hash)?;
            let mut spine_blobs: Vec<String> = Vec::new();
            let mut spine_recipes: Vec<String> = Vec::new();

            for (name, subtree) in tree.get_subtrees() {
                let child = scope_join(&path, name);

                match scope.classify(&child) {
                    ScopeClass::InScope => in_scope_roots.push(subtree.hash.clone()),
                    ScopeClass::Spine => spine_frontier.push((subtree.hash.clone(), child)),
                    ScopeClass::OutOfScope => {}
                }
            }

            for (name, file) in tree.get_files() {
                // A file entry on the spine is a sibling of the in-scope path — out of scope — so
                // it is sealed, unless the scope names this exact path in scope (a scope prefix
                // names a directory, so this stays classifier-driven rather than assumed).
                if scope.classify(&scope_join(&path, name)) == ScopeClass::InScope
                    && seen_blobs.insert(file.hash.clone())
                {
                    spine_blobs.push(file.hash.clone());

                    // An in-scope chunked file named on the spine: fetch its chunks too. An
                    // out-of-scope one is sealed above (never added), so its recipe never lands and
                    // its chunks are never named — sparse fetches nothing out of scope.
                    if file.item_type.is_chunked() {
                        spine_recipes.push(file.hash.clone());
                    }
                }
            }

            stats.fetched_objects += fetch_missing_objects(client, &spine_blobs).await?;
            stats.fetched_objects += fetch_recipe_chunks(client, &spine_recipes).await?;
        }

        // The in-scope subtree closures — the parallel bulk, fetched in batched waves exactly as
        // the unscoped walk does. Everything under an in-scope prefix is in scope, so no further
        // classification is needed here.
        let mut tree_frontier = in_scope_roots;

        while !tree_frontier.is_empty() {
            let tree_wave: Vec<String> = tree_frontier.drain(..)
                .filter(|hash| walked_full.insert(hash.clone()))
                .collect();

            if tree_wave.is_empty() {
                continue;
            }

            stats.fetched_objects += fetch_missing_objects(client, &tree_wave).await?;

            let mut blob_wave: Vec<String> = Vec::new();
            let mut recipe_wave: Vec<String> = Vec::new();

            for tree_hash in &tree_wave {
                let tree = object_utils::load_tree(tree_hash)?;

                for (_, file) in tree.get_files() {
                    if seen_blobs.insert(file.hash.clone()) {
                        blob_wave.push(file.hash.clone());

                        // Everything under an in-scope prefix is in scope, so every chunked file
                        // here has its chunks fetched (via the recipe just fetched in the blob wave).
                        if file.item_type.is_chunked() {
                            recipe_wave.push(file.hash.clone());
                        }
                    }
                }

                for (_, subtree) in tree.get_subtrees() {
                    tree_frontier.push(subtree.hash.clone());
                }
            }

            stats.fetched_objects += fetch_missing_objects(client, &blob_wave).await?;
            stats.fetched_objects += fetch_recipe_chunks(client, &recipe_wave).await?;
        }
    }

    Ok(stats)
}

/// Join a warehouse path key with a child name (root key is the empty string). A local copy of
/// the same rule the tree walks elsewhere use, kept here so the fetch has no cross-module dep.
fn scope_join(key: &str, name: &str) -> String {
    if key.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", key, name)
    }
}

/// Whether a parcel's whole closure is already present, and so needs neither fetching nor
/// walking: it is here, and it is reachable from a local ref head.
///
/// Only locally-present parcels are tested. A parcel we have not fetched yet cannot be
/// behind a local ref, and asking would force the commit-graph to build records for an
/// ancestry that is not here.
fn is_known_complete(hash: &str, complete_heads: &[String]) -> Result<bool, String> {
    if !file_utils::does_object_exist(hash)? {
        return Ok(false);
    }

    for head in complete_heads {
        // `is_ancestor` prunes on the commit-graph's generation numbers, so this costs the
        // gap between the two, not the length of history.
        if hash == head || merge_utils::is_ancestor(hash, head)? {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Fetch (concurrently) the objects of the given hashes that are missing locally.
/// Every downloaded object is hash-verified by `store_object_bytes` before it lands.
///
/// `pub(crate)` (not just a private helper of this module's own history walks): §3.2's
/// heal-driven refetch (`recovery_utils::attempt_heal_driven_refetch`) also calls this directly,
/// for a *targeted*, hash-addressed fetch of exactly the recorded hashes a taint's remainder
/// names — deliberately bypassing `fetch_history`/`fetch_history_scoped`'s own "already reachable
/// from a local ref" bound (see that function's doc comment): that bound is sound for their own
/// purpose (skip re-walking a closure a ref already proves complete) but means neither ever
/// re-verifies or re-fetches one specific object inside an otherwise-already-complete parcel —
/// exactly the shape a vanished-but-still-referenced object recorded against the *current*,
/// already-published pallet head takes. A direct `GET /v1/objects/{hash}` is deliberately
/// path-blind server-side (`forklift-server`'s own `get_object` doc comment) and so finds an
/// object regardless of which pallet(s) reference it or whether their closure is already
/// considered complete — this function already IS that primitive (the history walks above call
/// it per-wave), just never previously called with a caller-chosen, non-walk-discovered hash
/// list.
///
/// # Returns
/// * `Ok(usize)`   - How many objects were fetched.
/// * `Err(String)` - If a transfer or verification failed.
pub(crate) async fn fetch_missing_objects(client: &RemoteClient, hashes: &[String]) -> Result<usize, String> {
    let mut missing: Vec<String> = Vec::new();

    for hash in hashes {
        if !file_utils::does_object_exist(hash)? {
            missing.push(hash.clone());
        }
    }

    if missing.is_empty() {
        return Ok(0);
    }

    // Batch fetch first: one round trip per chunk, a bundle-format stream back
    // (forklift's packfile moment). A remote without the endpoint answers 404 and
    // everything falls back to loose GETs; whatever a batch did not deliver (the
    // remote may lack objects) is fetched loose below too.
    if missing.len() > 1 {
        for chunk in missing.chunks(BATCH_FETCH_CHUNK) {
            match client.fetch_batch(chunk).await? {
                Some(bytes) => { bundle_utils::import_bundle_bytes(&bytes)?; }
                None => break,
            }
        }

        let mut leftover: Vec<String> = Vec::new();

        for hash in &missing {
            if !file_utils::does_object_exist(hash)? {
                leftover.push(hash.clone());
            }
        }

        if leftover.is_empty() {
            return Ok(missing.len());
        }

        let fetched_by_batch = missing.len() - leftover.len();
        let loose = fetch_loose_objects(client, &leftover).await?;

        return Ok(fetched_by_batch + loose);
    }

    fetch_loose_objects(client, &missing).await
}

/// Fetch every chunk of the given recipes that is missing locally — the second half of fetching a
/// chunked file, run *after* the recipes themselves have landed (they ride the ordinary blob wave,
/// since a tree entry names the recipe like any other file object). Bundles never carry chunks
/// (trees don't reference them, so no closure walk ever emits one), so a franchise/lower/expand
/// imports the tree+recipe closure and then fetches the in-scope chunks per object here, exactly
/// the "a bundle is an optimization; missing objects fall back to loose GET" contract.
///
/// Deliberately calls [`fetch_loose_objects`] directly rather than [`fetch_missing_objects`] —
/// chunks always fetch one presigned `GET` each, **never** through `POST /v1/objects/batch`, no
/// matter how many are missing (DESIGN.html §9.4b: "franchise, lower and expand fetch chunks
/// per-object after the bundle wave"). A chunk is capped at 4 MiB and already hash-verified on
/// store, so a bundle buys chunks nothing a loose fetch doesn't already give; routing them
/// through `batch` would only be a redirect an offloading head has to mint and a client has to
/// follow for no benefit.
///
/// Each recipe is loaded from the now-present local object (which re-hashes it and runs the
/// `sum(sizes) == total` structural check) and its chunk hashes are collected — deduplicated across
/// recipes so a chunk shared by two files is fetched once. `store_object_bytes` hash-verifies every
/// fetched chunk and enforces the per-chunk ceiling on the way in. Only in-scope recipes are ever
/// passed here: an out-of-scope recipe is sealed, never fetched, so its chunks are never named
/// (the store invariant "recipe absent ⟹ chunks absent" holds under sparse fetch).
///
/// # Arguments
/// * `client`        - The remote.
/// * `recipe_hashes` - Recipe hashes whose chunks to fetch (already present locally).
///
/// # Returns
/// * `Ok(usize)`   - How many chunk objects were fetched.
/// * `Err(String)` - If a recipe is unreadable, or a chunk transfer/verification failed.
async fn fetch_recipe_chunks(client: &RemoteClient, recipe_hashes: &[String]) -> Result<usize, String> {
    if recipe_hashes.is_empty() {
        return Ok(0);
    }

    let mut chunk_hashes: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for recipe_hash in recipe_hashes {
        for chunk_hash in object_utils::recipe_chunk_hashes(recipe_hash)? {
            if seen.insert(chunk_hash.clone()) {
                chunk_hashes.push(chunk_hash);
            }
        }
    }

    let mut missing: Vec<String> = Vec::new();

    for hash in &chunk_hashes {
        if !file_utils::does_object_exist(hash)? {
            missing.push(hash.clone());
        }
    }

    if missing.is_empty() {
        return Ok(0);
    }

    fetch_loose_objects(client, &missing).await
}

/// Fetch (concurrently) the given objects one GET each. All of them are assumed
/// missing locally.
async fn fetch_loose_objects(client: &RemoteClient, missing: &[String]) -> Result<usize, String> {
    let semaphore = Arc::new(Semaphore::new(CONCURRENT_TRANSFERS));
    let mut tasks: JoinSet<Result<(), String>> = JoinSet::new();

    for hash in missing {
        let client = client.clone();
        let hash = hash.clone();
        let semaphore = Arc::clone(&semaphore);

        tasks.spawn(async move {
            let _permit = semaphore.acquire().await
                .map_err(|_| "The transfer pool was closed unexpectedly.".to_string())?;

            let bytes = client.fetch_object(&hash).await?;

            object_utils::store_object_bytes(&hash, &bytes)?;

            Ok(())
        });
    }

    join_all(tasks).await?;

    Ok(missing.len())
}

/// D4's dedicated corrupt-candidate fetch (DESIGN.html §3.1.1): one loose `GET` per flagged hash,
/// verified and force-stored via [`object_utils::force_store_object_bytes`] — the corrupt copy is
/// superseded by an atomic rename, never deleted first.
///
/// Deliberately never routes through [`fetch_missing_objects`]: that function's own dedup filter
/// (`file_utils::does_object_exist`) answers "yes" for a corrupt-but-present dentry, so a flagged
/// hash would simply never be requested, and — on its batch/bundle path — the post-batch presence
/// recheck would read the still-corrupt file back as "delivered" even when nothing actually
/// landed and the store gate discarded it. Sending flagged hashes down this dedicated path instead
/// of the shared one makes both of those failure modes unreachable by construction rather than
/// requiring every gate along the shared path to be bypassed correctly.
///
/// All `flagged` hashes are assumed corrupt-but-present in the *loose* store; the loose-presence
/// half of `does_object_exist` is never consulted, for the reason above. The pack-presence half is
/// a different question, though: a hash can be corrupt loose and — independently — already sitting
/// good in a pack (e.g. packed once, then re-written loose and damaged after). Skipping *only* a
/// pack hit ([`pack_utils::is_in_packs`]) costs nothing: it never answers "yes" for a corrupt-loose
/// dentry (a pack's own index is a separate structure from the loose fan-out, so a loose-only
/// corruption can't taint it), and it needs no taint-gate check either (see
/// [`file_utils::does_object_exist`]'s own doc comment on why a pack hit is exempt). A pack hit
/// skips the fetch and counts as recovered without writing a redundant loose duplicate; a check
/// error is treated as "not packed" so the flagged hash still gets its forced re-fetch rather than
/// silently skipping a genuine recovery.
///
/// Deliberately routed through [`join_all_independent`], not the shared [`join_all`]: each
/// flagged hash is an independent recovery attempt, and a 404 or a bad transfer on one hash must
/// never cancel a sibling hash's task before it force-stores its own recovered bytes — `join_all`'s
/// first-error-wins/abort-the-rest discipline is exactly wrong here (see that function's own doc
/// for why it is right for its other callers, where the batch is one all-or-nothing unit).
///
/// # Returns
/// `(usize, BTreeMap<String, String>)` — the count of `flagged` hashes now known good (either
/// fetched and force-stored, or found already good in a pack and left alone — see the pack-only
/// guard above), and every hash that was *not* among them mapped to its own transfer/verification/
/// store failure, so a caller can quote the exact reason a specific hash's recovery failed instead
/// of a joined, unattributed string. A hash present in `flagged` but accounted for by neither the
/// count nor this map failed via a task panic: [`join_all_independent`]'s own per-outcome `Err` for
/// a `JoinError` carries no hash to attribute it to (see [`HASH_ERROR_DELIMITER`]'s own doc for how
/// an ordinary failure's `Err` does), so it is reflected only in the count coming up short.
pub(crate) async fn fetch_corrupt_replacements(
    client: &RemoteClient,
    flagged: &[String],
) -> (usize, BTreeMap<String, String>) {
    let semaphore = Arc::new(Semaphore::new(CONCURRENT_TRANSFERS));
    let mut tasks: JoinSet<Result<(), String>> = JoinSet::new();

    for hash in flagged {
        let client = client.clone();
        let hash = hash.clone();
        let semaphore = Arc::clone(&semaphore);

        tasks.spawn(async move {
            async {
                // Pack-only guard (see this function's own doc comment): a hash already good in a
                // pack needs no re-fetch, regardless of what's wrong with its loose copy. A check
                // error falls through to the ordinary fetch, so a broken presence check can never
                // suppress a genuine recovery.
                if pack_utils::is_in_packs(&hash).unwrap_or(false) {
                    return Ok(());
                }

                let _permit = semaphore.acquire().await
                    .map_err(|_| "The transfer pool was closed unexpectedly.".to_string())?;

                let bytes = client.fetch_object(&hash).await?;

                object_utils::force_store_object_bytes(&hash, &bytes)?;

                Ok(())
            }.await.map_err(|e: String| format!("{}{}{}", hash, HASH_ERROR_DELIMITER, e))
        });
    }

    let mut recovered = 0usize;
    let mut failures: BTreeMap<String, String> = BTreeMap::new();
    for outcome in join_all_independent(tasks).await {
        match outcome {
            Ok(()) => recovered += 1,
            // An ordinary per-hash failure carries its own hash, prefixed above — split it back
            // off. A `JoinError` (panic), wrapped by `join_all_independent` itself into a generic
            // "A transfer task failed: ..." string, never contains the delimiter and so is
            // correctly left unattributed (see this function's own `# Returns` doc).
            Err(combined) => {
                if let Some((hash, message)) = combined.split_once(HASH_ERROR_DELIMITER) {
                    if flagged.iter().any(|flagged_hash| flagged_hash == hash) {
                        failures.insert(hash.to_string(), message.to_string());
                    }
                }
            }
        }
    }

    (recovered, failures)
}

/// A control character, never legitimate in a hash or in the human-readable transport errors this
/// module produces, used only to prefix [`fetch_corrupt_replacements`]'s own per-task `Err` with
/// the hash it belongs to — so a genuine per-hash failure still reaches [`join_all_independent`]
/// as a real task-level `Err` (preserving the exact shape a revert to the shared [`join_all`] would
/// abort on), while the hash rides along for the caller to split back off.
const HASH_ERROR_DELIMITER: char = '\u{1}';

/// Fetch (concurrently) the signature sidecars of the given parcels, where the sidecar
/// is missing locally. Unsigned parcels (no sidecar on the remote either) are fine.
///
/// # Returns
/// * `Ok(usize)`   - How many sidecars were fetched.
/// * `Err(String)` - If a transfer failed.
async fn fetch_missing_signatures(client: &RemoteClient,
                                  parcel_hashes: &[String]) -> Result<usize, String> {
    let mut wanted: Vec<String> = Vec::new();

    for hash in parcel_hashes {
        if sign_utils::load_raw_parcel_signature(hash)?.is_none() {
            wanted.push(hash.clone());
        }
    }

    if wanted.is_empty() {
        return Ok(0);
    }

    let semaphore = Arc::new(Semaphore::new(CONCURRENT_TRANSFERS));
    let mut tasks: JoinSet<Result<usize, String>> = JoinSet::new();

    for hash in wanted {
        let client = client.clone();
        let semaphore = Arc::clone(&semaphore);

        tasks.spawn(async move {
            let _permit = semaphore.acquire().await
                .map_err(|_| "The transfer pool was closed unexpectedly.".to_string())?;

            match client.fetch_signature(&hash).await? {
                Some(bytes) => {
                    sign_utils::store_raw_parcel_signature(&hash, &bytes)?;
                    Ok(1)
                }
                None => Ok(0),
            }
        });
    }

    // Routed through the same `join_all` every other coordinator in this module uses, rather
    // than a second, hand-rolled copy of its join/drain loop — see `join_all`'s doc for the
    // not-return-while-writing guarantee this gets for free; `sign_utils::store_raw_parcel_signature`
    // is one of the two store helpers that guarantee names.
    join_all(tasks).await.map(|counts| counts.into_iter().sum())
}

/// Lift one pallet: negotiate the missing objects, upload them (and the new parcels'
/// signatures) in parallel, then move the remote ref with a CAS.
///
/// # Arguments
/// * `client`      - The remote.
/// * `pallet`      - The pallet name on the remote.
/// * `local_head`  - The local head parcel of the pallet.
/// * `remote_head` - The remote's current head of the pallet (from the handshake).
///
/// # Returns
/// * `Ok(LiftResult)` - Up to date, or the transfer stats.
/// * `Err(String)`    - If the remote is ahead/diverged, or a transfer failed.
pub async fn lift_pallet(client: &RemoteClient,
                         pallet: &str,
                         local_head: &str,
                         remote_head: Option<&str>,
                         chunking_supported: bool) -> Result<LiftResult, String> {
    lift_pallet_inner(client, pallet, local_head, remote_head, false, chunking_supported).await
}

/// `lift_pallet`, allowing one sanctioned non-descendant update: the office lift right
/// after a re-genesis (§8.7), where the new chain replaces — rather than extends — the
/// remote's office head that the local anchor adopted. The server enforces the same
/// exception narrowly on its side.
async fn lift_pallet_inner(client: &RemoteClient,
                           pallet: &str,
                           local_head: &str,
                           remote_head: Option<&str>,
                           adopted_reset: bool,
                           chunking_supported: bool) -> Result<LiftResult, String> {
    if remote_head == Some(local_head) {
        return Ok(LiftResult::UpToDate);
    }

    if let Some(remote_head) = remote_head {
        if !file_utils::does_object_exist(remote_head)? {
            return Err(format!(
                "The remote's pallet \"{}\" has parcels this warehouse does not know \
                (head {}). \"lower\" first.",
                pallet, remote_head
            ));
        }

        if !adopted_reset && !merge_utils::is_ancestor(remote_head, local_head)? {
            return Err(format!(
                "The local pallet \"{}\" and the remote have diverged. \"lower\" the \
                remote parcels and consolidate before lifting.",
                pallet
            ));
        }
    }

    // The new parcels: everything reachable from the local head that the remote does not
    // already have — the remote head, and every ancestor of it. The walk stops at the remote
    // head and at any ancestor of it (a merge's other side rejoins below the remote head), so a
    // linear lift touches O(new parcels) and a merge never re-walks the shared slice. Pruning at
    // every ancestor — not just the remote head hash — is also what keeps a sparse workspace
    // liftable: an interior parcel the remote already has may carry an out-of-scope change whose
    // object this workspace never fetched, and re-walking it would try to load that sealed object.
    // The remote provably has it (it is an ancestor of the remote head, whose closure is
    // complete there), so it is correctly never uploaded and never walked.
    let mut new_parcels: Vec<String> = Vec::new();
    let mut queue: Vec<String> = vec![local_head.to_string()];
    let mut visited: HashSet<String> = HashSet::new();

    while let Some(hash) = queue.pop() {
        if Some(hash.as_str()) == remote_head || !visited.insert(hash.clone()) {
            continue;
        }

        if let Some(remote_head) = remote_head {
            if merge_utils::is_ancestor(&hash, remote_head)? {
                continue;
            }
        }

        let parcel = object_utils::load_parcel(&hash)?;

        queue.extend(parcel.parents);
        new_parcels.push(hash);
    }

    // Candidate objects for the negotiation: each new parcel's tree, walked against
    // its parents' trees — a subtree identical to *any* parent's at the same path is
    // skipped whole, the same skip the merge walk and the pallet diff use. A one-line
    // change on a 100k-file warehouse thus negotiates the changed path, not the full
    // closure.
    let mut candidates: Vec<String> = new_parcels.clone();
    let mut seen_trees: HashSet<String> = HashSet::new();
    let mut seen_blobs: HashSet<String> = HashSet::new();
    let mut seen_recipes: HashSet<String> = HashSet::new();

    // Oldest first: a parcel's parents are remote-known or already processed, so
    // everything a base "explains" is on the remote or in the candidates already.
    for parcel_hash in new_parcels.iter().rev() {
        let parcel = object_utils::load_parcel(parcel_hash)?;

        // Every parent's tree, not just the first. A merge parcel that
        // adopted an out-of-scope sibling by hash from its *second* parent is explained by that
        // parent — which the remote already has, or which is uploaded in this same session — so
        // treating a subtree as base-explained when it matches ANY parent stops the walk from
        // trying to load an object a sparse workspace never fetched. An ordinary single-parent
        // parcel is the N=1 case: identical behavior, and a strictly-not-larger candidate set.
        let base_trees: Vec<String> = parcel.parents.iter()
            .map(|parent| object_utils::load_parcel(parent).map(|p| p.tree_hash))
            .collect::<Result<_, _>>()?;

        collect_changed_closure(&parcel.tree_hash, "", &base_trees,
                                &mut seen_trees, &mut seen_blobs, &mut seen_recipes,
                                &mut candidates, chunking_supported)?;
    }

    // Control-plane objects — parcels, trees, and recipes — are promoted synchronously when a
    // storage-backed head commits the session; working blobs and chunks are promoted out of band
    // by the staging verifier and only presence-checked. Classify from the sets the closure walk
    // already built (`new_parcels`, `seen_trees` and `seen_recipes`), rather than re-deriving each
    // object's type on the wire. A recipe is small and structural (like a tree), so it belongs in
    // the synchronous half; its chunks are the large, many, out-of-band half.
    let mut control_plane: HashSet<String> = new_parcels.iter().cloned().collect();
    control_plane.extend(seen_trees.iter().cloned());
    control_plane.extend(seen_recipes.iter().cloned());

    // One flow serves both heads: negotiate upload targets, PUT the missing objects straight to
    // presigned staging URLs and/or to the control plane, and commit the staged session. Falls
    // back to `missing` + per-object `PUT` against a remote that predates `upload-targets`.
    let session = new_lift_session();
    let uploaded_objects = negotiate_and_upload(
        client, &session, &candidates, &control_plane, chunking_supported,
    ).await?;

    // The signatures of the new parcels travel with them.
    let mut uploaded_signatures = 0usize;

    for parcel_hash in &new_parcels {
        if let Some(bytes) = sign_utils::load_raw_parcel_signature(parcel_hash)? {
            client.upload_signature(parcel_hash, bytes).await?;
            uploaded_signatures += 1;
        }
    }

    client.update_ref(pallet, remote_head, local_head).await?;

    Ok(LiftResult::Lifted(LiftStats {
        new_parcels: new_parcels.len(),
        uploaded_objects,
        uploaded_signatures,
        old_head: remote_head.map(|hash| hash.to_string()),
    }))
}

/// Collect the objects of a tree that its bases — the trees at the same path in the parcel's
/// parents — do not explain: a subtree or file identical to **any** parent's is skipped whole, a
/// changed subtree is descended with each parent's matching child as a base. An empty base set
/// collects the full closure (a root parcel has no parents).
///
/// The multi-parent base set is a straight generalization of the
/// single-parent walk: a merge parcel's subtree adopted by hash from its second parent matches
/// that parent here and is pruned, so the walk never loads an object a sparse workspace holds
/// only by seal. It is scope-agnostic and correct in full stores too — an object pruned against a
/// parent is provably on the remote (that parent is already there or is uploaded in this session),
/// exactly the guarantee the first-parent-only walk gave for linear history.
// Three dedup ledgers (trees, blobs/chunks, recipes) plus the candidate accumulator, the path
// prefix for error naming, the base set, and the chunking capability — each meaningfully distinct
// and threaded through the recursion, so a parameter object would only obscure them.
#[allow(clippy::too_many_arguments)]
fn collect_changed_closure(tree_hash: &str,
                           path_prefix: &str,
                           base_tree_hashes: &[String],
                           seen_trees: &mut HashSet<String>,
                           seen_blobs: &mut HashSet<String>,
                           seen_recipes: &mut HashSet<String>,
                           candidates: &mut Vec<String>,
                           chunking_supported: bool) -> Result<(), String> {
    // Record the visit before checking base-explained, not after: content-addressing means the
    // same subtree hash can recur at another path in the same walk (e.g. a merge adopting one
    // side's out-of-scope subtree under two names), and that recurrence must be recognized even
    // when the FIRST visit returned early because a parent explained it. Skipping it there is
    // still complete — same hash means same content, and a base-explained tree's own closure is
    // already covered (its parent is remote-known or was walked earlier in this same session) —
    // the identical induction this function's ancestry guarantee already relies on.
    let first_visit = seen_trees.insert(tree_hash.to_string());

    // Explained by some parent at this path (or already walked): the remote has it, so it needs
    // neither upload nor descent — and, critically, its object is never loaded.
    if base_tree_hashes.iter().any(|hash| hash == tree_hash) || !first_visit {
        return Ok(());
    }

    candidates.push(tree_hash.to_string());

    let tree = object_utils::load_tree(tree_hash)?;

    let bases = base_tree_hashes.iter()
        .map(|hash| object_utils::load_tree(hash))
        .collect::<Result<Vec<_>, _>>()?;

    // The union across every parent tree: a file is base-explained when ANY parent maps that name
    // to that exact hash; a subtree's per-parent child hashes are threaded into the recursion, so
    // a deeper subtree is pruned against whichever parent explains it.
    let mut base_file_hashes: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut base_subtree_hashes: HashMap<&str, Vec<&str>> = HashMap::new();

    for base in &bases {
        for (name, file) in base.get_files() {
            base_file_hashes.entry(name.as_str()).or_default().push(file.hash.as_str());
        }
        for (name, subtree) in base.get_subtrees() {
            base_subtree_hashes.entry(name.as_str()).or_default().push(subtree.hash.as_str());
        }
    }

    for (name, file) in tree.get_files() {
        let explained = base_file_hashes.get(name.as_str())
            .is_some_and(|hashes| hashes.contains(&file.hash.as_str()));

        // A chunked file's entry hash names a recipe, whose chunks ride the byte plane as
        // ordinary objects. Two independent guards apply, in this order:
        if file.item_type.is_chunked() {
            // 1. The remote must support chunked files at all. Absent the handshake capability
            //    (an old head), refuse client-side, before any negotiation or upload, naming the
            //    path — an old head's `gc` would silently collect a recipe's chunks (B1), so a
            //    chunk-aware client never lifts chunked content there. Checked for every chunked
            //    entry this walk visits (explained or not), so it also catches one that is
            //    unchanged-but-newly-reachable in this lift.
            if !chunking_supported {
                return Err(scope_utils::chunked_remote_refusal(&join_path(path_prefix, name)).into());
            }

            // 2. An identical recipe on a parent at this name ⟹ the remote already has that
            //    recipe and its whole chunk closure (a base's closure is complete on the remote).
            if explained {
                continue;
            }

            // First encounter of this recipe: negotiate the recipe (control plane) and descend it
            // to enumerate every chunk (blobs). Without the chunks in `candidates`, the upload
            // negotiation never learns to send them, and the remote's ref would advance over a
            // recipe whose chunks never arrived — the client half of §9.4b W4. Per-chunk dedup is
            // free from the negotiation: an appended-to file re-lists all its chunk hashes here,
            // and `upload-targets` reports the unchanged ones `present`.
            if seen_recipes.insert(file.hash.clone()) {
                candidates.push(file.hash.clone());

                for chunk_hash in object_utils::recipe_chunk_hashes(&file.hash)? {
                    if seen_blobs.insert(chunk_hash.clone()) {
                        candidates.push(chunk_hash);
                    }
                }
            }

            continue;
        }

        if explained {
            continue;
        }

        if seen_blobs.insert(file.hash.clone()) {
            candidates.push(file.hash.clone());
        }
    }

    for (name, subtree) in tree.get_subtrees() {
        let child_bases: Vec<String> = base_subtree_hashes.get(name.as_str())
            .map(|hashes| hashes.iter().map(|hash| hash.to_string()).collect())
            .unwrap_or_default();

        collect_changed_closure(&subtree.hash, &join_path(path_prefix, name), &child_bases,
                                seen_trees, seen_blobs, seen_recipes, candidates,
                                chunking_supported)?;
    }

    Ok(())
}

/// Join a directory path prefix and an entry name into the entry's warehouse path (`""` prefix
/// yields the bare name) — used only to name a path in an error message, never for lookups.
fn join_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", prefix, name)
    }
}

/// Refuse to put an object above the whole-object ceiling on the wire — the client-side half of
/// the maintainer's chosen posture for a grandfathered giant (see `bundle_utils`'s writer-side
/// refusal for the full reasoning: such an object stays readable locally forever, but no
/// migration preserves its signed identity, so nothing accepts it in transport). Checked here,
/// where the upload path already holds the object's bytes for the imminent network call, so
/// refusing costs nothing extra and the bytes never reach the wire — an honest client-side
/// failure instead of the server's own import refusal surfacing as an opaque mid-lift error.
fn refuse_if_over_ceiling_for_upload(hash: &str, bytes: &[u8]) -> Result<(), CoreError> {
    scope_utils::refuse_if_over_object_ceiling(&format!("object {}", hash), bytes.len())
}

/// Load one object's bytes and apply the transport-ceiling refusal, off the async runtime
/// (review round S2 fix hole): `retrieve_object_by_hash` is synchronous pack/loose-object I/O
/// (decompression, hash verification), not a `.await` point, so running it directly inside a
/// spawned task blocks whichever runtime worker polls that task for however long the read takes.
/// On the production multi-thread runtime with [`CONCURRENT_TRANSFERS`] (24) workers all
/// eventually doing this at once, every in-flight upload's body-send stream and its
/// [`RemoteClient::send_with_watchdog`] loop go unpolled while their worker is pinned here — on
/// the next poll, `select!`'s random branch order can let the watchdog observe a stale
/// `silent_for()` before the body-send future gets a chance to refresh it, producing a false
/// "timed out" verdict for a transfer that was never actually silent. `spawn_blocking` moves the
/// read onto tokio's separate blocking thread pool, which exists independently of the runtime's
/// scheduler flavor (present on `current_thread` too — see the blocking-pool construction tokio
/// shares between `build_current_thread_runtime` and `build_threaded_runtime`), so the async
/// workers keep polling every other in-flight task while this one reads from disk.
///
/// `scope_root` re-enters the caller's storage-root scope on the blocking thread (the same idiom
/// [`crate::util::fanout_utils::fanout_map`] and the server's own `blocking` helper use):
/// [`StorageRootScope`] is thread-local and is **not** inherited by a spawned task, whether that
/// task runs via `spawn_blocking` or — on a genuine multi-thread runtime — plain `tokio::spawn`/
/// `JoinSet::spawn` (`Runtime::block_on`'s own doc: "the future will execute on the current
/// thread, but all spawned tasks will execute on the thread pool"). So `scope_root` must be read
/// by the *caller*, before it ever spawns the task this function runs inside of — reading it in
/// here instead would capture whatever (likely empty) scope happens to belong to the worker
/// thread that picked up the spawned task, not the caller's. `None` (the CLI, which resolves by
/// working directory, and the process-global bay context) needs nothing re-entered — the
/// blocking thread already shares both.
///
/// `delay` is the test seam for the starvation falsifier below — always `Duration::ZERO` outside
/// `#[cfg(test)]` callers, for the identical reason `scope_root` is a parameter rather than
/// self-discovered: it must reflect the *caller's* thread-local test knob, not whatever the
/// spawned task's own thread happens to see.
async fn retrieve_object_for_upload(hash: String,
                                    scope_root: Option<std::path::PathBuf>,
                                    delay: std::time::Duration) -> Result<Vec<u8>, String> {
    tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let _scope = scope_root.as_deref().map(StorageRootScope::enter);

        if !delay.is_zero() {
            std::thread::sleep(delay);
        }

        let bytes = file_utils::retrieve_object_by_hash(&hash)?;
        refuse_if_over_ceiling_for_upload(&hash, &bytes)?;
        Ok(bytes)
    })
        .await
        .map_err(|e| format!("The object-retrieval task panicked: {}", e))?
}

#[cfg(test)]
thread_local! {
    /// Test-only seam (review round S2 fix hole, falsifier for the `spawn_blocking` fix above):
    /// lets a test make the blocking retrieval artificially slow, long enough that a runtime
    /// whose only worker is stuck *synchronously* inside the read would visibly miss timer ticks
    /// a sibling task is trying to fire. Thread-local, not a process-global `static`: `cargo test`
    /// runs different tests on different OS threads, so a thread-local set by one test's own
    /// thread is invisible to every other test running concurrently — a `static` would leak the
    /// delay across them. Read by [`upload_objects`]/[`upload_to_targets`] on their own (caller's)
    /// thread, before spawning any task — see [`retrieve_object_for_upload`]'s doc for why that
    /// placement matters.
    static TEST_RETRIEVAL_DELAY: std::cell::Cell<std::time::Duration> =
        const { std::cell::Cell::new(std::time::Duration::ZERO) };
}

#[cfg(test)]
fn test_retrieval_delay() -> std::time::Duration {
    TEST_RETRIEVAL_DELAY.with(|cell| cell.get())
}

#[cfg(not(test))]
fn test_retrieval_delay() -> std::time::Duration {
    std::time::Duration::ZERO
}

#[cfg(test)]
fn set_test_retrieval_delay(delay: std::time::Duration) {
    TEST_RETRIEVAL_DELAY.with(|cell| cell.set(delay));
}

/// Upload (concurrently) the objects of the given hashes.
async fn upload_objects(client: &RemoteClient, hashes: &[String]) -> Result<(), String> {
    if hashes.is_empty() {
        return Ok(());
    }

    // Read once, synchronously, on whichever thread is currently driving this function's own
    // poll — before any task below is spawned onto (possibly) a different one. See
    // `retrieve_object_for_upload`'s doc for why this can't just be read again inside it.
    let scope_root = globals::current_scope_root();
    let delay = test_retrieval_delay();

    let semaphore = Arc::new(Semaphore::new(CONCURRENT_TRANSFERS));
    let mut tasks: JoinSet<Result<(), String>> = JoinSet::new();

    for hash in hashes {
        let client = client.clone();
        let hash = hash.clone();
        let semaphore = Arc::clone(&semaphore);
        let scope_root = scope_root.clone();

        tasks.spawn(async move {
            let _permit = semaphore.acquire().await
                .map_err(|_| "The transfer pool was closed unexpectedly.".to_string())?;

            let bytes = retrieve_object_for_upload(hash.clone(), scope_root, delay).await?;

            client.upload_object(&hash, bytes).await
        });
    }

    join_all(tasks).await.map(|_| ())
}

/// Refuse a lift whose commit would need more than one paginated batch (§9.4b Stage 3, W3) when
/// the remote does not advertise chunking support. See [`negotiate_and_upload`] for why: the
/// additive `more` field that makes pagination safe shipped *with* chunking, not before it, and a
/// remote that ignores it would silently sweep away a later batch's still-staged objects.
///
/// # Arguments
/// * `staged_count`       - How many distinct objects `upload-targets` staged for this lift.
/// * `chunking_supported` - Whether the remote's handshake advertised chunking support.
///
/// # Returns
/// * `Ok(())`         - One batch suffices, or the remote understands pagination either way.
/// * `Err(CoreError)` - The `commit_pagination_unsupported` refusal.
fn refuse_if_commit_pagination_unsupported(staged_count: usize,
                                           chunking_supported: bool) -> Result<(), CoreError> {
    if staged_count <= MAX_MISSING_BATCH || chunking_supported {
        return Ok(());
    }

    Err(scope_utils::commit_pagination_unsupported_refusal(staged_count))
}

/// The one upload flow that serves both a storage-backed (staging) head and a direct head.
/// Negotiates targets, uploads the missing objects — straight to presigned staging URLs and/or
/// to the control plane — commits the staged session when there is one, and returns how many
/// objects it uploaded (for [`LiftStats`], staged and direct alike). Falls back to the legacy
/// `missing` + per-object `PUT` against a remote that predates `upload-targets`.
///
/// `control_plane` names the hashes that are parcels or trees — small objects the commit
/// verifies and promotes synchronously; every other staged hash is a working blob, promoted out
/// of band by the staging verifier and only presence-checked at commit.
///
/// `chunking_supported` gates nothing about chunked files here (that refusal already fired
/// earlier, in the closure walk) — it gates whether *this* remote understands paginated commits at
/// all (§9.4b W3), which matters for *any* large lift, chunked or not.
async fn negotiate_and_upload(client: &RemoteClient,
                              session: &str,
                              candidates: &[String],
                              control_plane: &HashSet<String>,
                              chunking_supported: bool) -> Result<usize, String> {
    let Some(negotiation) = client.upload_targets(session, candidates).await? else {
        // An older remote with no `upload-targets`: negotiate the missing set and PUT each body
        // to the control plane, exactly as before. No staging, no commit batching — each object is
        // verified inline on its own PUT, so the pagination gate below does not apply here.
        let missing = client.missing_objects(candidates).await?;
        upload_objects(client, &missing).await?;
        return Ok(missing.len());
    };

    // Before a single byte is uploaded: a commit that will need more than one paginated batch
    // requires a remote that understands the additive `more` field (§9.4b W3), which shipped with
    // chunking support. A pre-chunking staging head ignores an unrecognized `more` (defaults to
    // `false`) and sweeps its staging prefix after the very first batch — silently stranding
    // whatever a later batch still needed staged, so the lift would fail non-deterministically at
    // commit time with a misleading "blob not ready", *after* the whole (potentially enormous)
    // upload already ran. Refusing here, right after negotiation (which already knows the exact
    // staged count and cost nothing but a cheap, already-paginated round trip), is the honest
    // failure before any bytes move.
    refuse_if_commit_pagination_unsupported(negotiation.targets.len(), chunking_supported)?;

    // `present` is already on the remote (skip). `direct` goes to the control plane for inline
    // verification; `targets` go straight to storage under the session's staging prefix.
    upload_objects(client, &negotiation.direct).await?;
    upload_to_targets(client, &negotiation.targets).await?;

    // Only a staging head hands back targets; a direct head's are empty and it needs no commit
    // (every `direct` PUT was verified inline). When there was staging, the commit verifies and
    // promotes it before the ref update — nothing staged is fetchable until then.
    if !negotiation.targets.is_empty() {
        let (control, blobs) = classify_staged(&negotiation.targets, control_plane);
        commit_staged_session(client, session, &control, &blobs).await?;
    }

    Ok(negotiation.direct.len() + negotiation.targets.len())
}

/// Split the staged hashes into the control-plane objects (parcels and trees — promoted
/// synchronously at commit) and the working blobs (promoted out of band, presence-checked).
/// Pure, so the split is unit-testable without a remote.
fn classify_staged(targets: &BTreeMap<String, String>,
                   control_plane: &HashSet<String>) -> (Vec<String>, Vec<String>) {
    targets.keys()
        .cloned()
        .partition(|hash| control_plane.contains(hash))
}

/// Upload (concurrently) the staged objects to their presigned storage URLs — the same bounded
/// fan-out the fetch and direct-upload paths use ([`CONCURRENT_TRANSFERS`]).
async fn upload_to_targets(client: &RemoteClient,
                           targets: &BTreeMap<String, String>) -> Result<(), String> {
    if targets.is_empty() {
        return Ok(());
    }

    // See `upload_objects`'s identical capture for why this must happen here, before any task
    // is spawned, rather than inside `retrieve_object_for_upload` itself.
    let scope_root = globals::current_scope_root();
    let delay = test_retrieval_delay();

    let semaphore = Arc::new(Semaphore::new(CONCURRENT_TRANSFERS));
    let mut tasks: JoinSet<Result<(), String>> = JoinSet::new();

    for (hash, url) in targets {
        let client = client.clone();
        let hash = hash.clone();
        let url = url.clone();
        let semaphore = Arc::clone(&semaphore);
        let scope_root = scope_root.clone();

        tasks.spawn(async move {
            let _permit = semaphore.acquire().await
                .map_err(|_| "The transfer pool was closed unexpectedly.".to_string())?;

            let bytes = retrieve_object_for_upload(hash.clone(), scope_root, delay).await?;

            client.put_presigned(&url, bytes).await
        });
    }

    join_all(tasks).await.map(|_| ())
}

/// Commit a staged lift session, paginating the hash lists and retrying each batch with bounded
/// backoff while a blob is still being promoted out of band by the staging verifier (the one
/// transient failure — every other commit failure surfaces at once). Gives up with a clear,
/// safe-to-retry error rather than hanging on a stuck verifier.
///
/// A lift touching a maximal chunked file lists too many chunk hashes for one request (Lambda's
/// ~6 MB synchronous body), so `control_plane`/`blobs` are paginated at [`MAX_MISSING_BATCH`] and
/// every batch but the last carries `more: true`. The head verifies/presence-checks each batch but
/// gates its session-wide staging sweep on the final (`more: false`) batch, so an early batch never
/// discards chunks a later batch still needs. A small lift is one batch (`more: false`), byte-for-
/// byte the pre-pagination behaviour.
async fn commit_staged_session(client: &RemoteClient,
                               session: &str,
                               control_plane: &[String],
                               blobs: &[String]) -> Result<(), String> {
    let batches = build_commit_batches(control_plane, blobs, MAX_MISSING_BATCH);
    let last = batches.len() - 1; // `build_commit_batches` never returns an empty vec.

    for (index, (control, working)) in batches.iter().enumerate() {
        // `more` on every batch but the last: the head skips its staging sweep until the final
        // batch, so intermediate batches never discard a later batch's still-staged objects.
        let more = index < last;
        commit_one_batch(client, session, control, working, more).await?;
    }

    Ok(())
}

/// Commit one paginated batch, retrying with bounded backoff while a blob (or chunk) is still
/// being promoted out of band. A `more` batch re-verifies idempotently on retry and never sweeps.
async fn commit_one_batch(client: &RemoteClient,
                          session: &str,
                          control_plane: &[String],
                          blobs: &[String],
                          more: bool) -> Result<(), String> {
    let mut delay = COMMIT_BACKOFF_START;

    for attempt in 1..=MAX_COMMIT_ATTEMPTS {
        match client.commit_lift(session, control_plane, blobs, more).await? {
            CommitOutcome::Committed => return Ok(()),
            CommitOutcome::BlobNotReady => {}
        }

        if attempt < MAX_COMMIT_ATTEMPTS {
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(COMMIT_BACKOFF_CAP);
        }
    }

    Err(format!(
        "The remote's staging verifier has not finished promoting this lift's blobs after {} \
        attempts. The upload is safe — retry the lift once the remote has caught up.",
        MAX_COMMIT_ATTEMPTS
    ))
}

/// Partition a session's `control_plane` and `blobs` hash lists into commit batches, each with at
/// most `cap` hashes *combined* (the head caps `control_plane.len() + blobs.len()` per request).
/// Control-plane hashes fill first (they carry the recipes/trees a later batch's chunks belong to),
/// then blobs; a batch is never split across the two lists' boundary in a way that reorders either.
/// Always returns at least one batch — even for two empty lists — so the caller's final-batch
/// staging sweep always runs.
fn build_commit_batches(control_plane: &[String],
                        blobs: &[String],
                        cap: usize) -> Vec<(Vec<String>, Vec<String>)> {
    let cap = cap.max(1);
    let mut batches: Vec<(Vec<String>, Vec<String>)> = Vec::new();
    let mut control = control_plane.iter();
    let mut working = blobs.iter();

    loop {
        let mut control_batch: Vec<String> = Vec::new();
        let mut working_batch: Vec<String> = Vec::new();
        let mut room = cap;

        while room > 0 {
            match control.next() {
                Some(hash) => { control_batch.push(hash.clone()); room -= 1; }
                None => break,
            }
        }
        while room > 0 {
            match working.next() {
                Some(hash) => { working_batch.push(hash.clone()); room -= 1; }
                None => break,
            }
        }

        if control_batch.is_empty() && working_batch.is_empty() {
            break;
        }

        batches.push((control_batch, working_batch));
    }

    if batches.is_empty() {
        // Nothing staged (both lists empty): still one final-batch commit so the sweep runs, the
        // exact single-shot behaviour of the pre-pagination path when it was handed empty lists.
        batches.push((Vec::new(), Vec::new()));
    }

    batches
}

/// Await every task of a set, surfacing the first failure and discarding the rest — first error
/// wins, so a later, possibly less actionable failure (e.g. one surfaced only by best-effort
/// cleanup after the real one) never overwrites the first. Generic over the task payload `T` so
/// every coordinator in this module — fetching objects, fetching signature sidecars, and both
/// upload paths — routes through this one implementation instead of each hand-rolling its own
/// copy of this loop.
///
/// The coordinating call must not return while a sibling task's body may still be writing
/// (`object_utils::store_object_bytes` and `sign_utils::store_raw_parcel_signature` both have no
/// await between reading the fetched bytes and finishing the write) — see [`drain_remaining`],
/// which this delegates to on the error path. Any panic evidence [`drain_remaining`] observed
/// while draining is appended to the first error below, never substituted for it.
///
/// This guarantee holds only while the returned future is polled to completion: wrapping this
/// call in `tokio::time::timeout` or a `select!` branch can drop the `JoinSet` without draining
/// it, reinstating the race this function exists to close.
async fn join_all<T: Send + 'static>(mut tasks: JoinSet<Result<T, String>>) -> Result<Vec<T>, String> {
    let mut results: Vec<T> = Vec::new();

    while let Some(result) = tasks.join_next().await {
        match result.map_err(|e| format!("A transfer task failed: {}", e)).and_then(|inner| inner) {
            Ok(value) => results.push(value),
            Err(first_error) => {
                return Err(match drain_remaining(tasks).await {
                    Some(panic_note) => format!("{} (additionally, {})", first_error, panic_note),
                    None => first_error,
                });
            }
        }
    }

    Ok(results)
}

/// Await every task of a set to its own natural completion, keeping each task's individual
/// outcome instead of [`join_all`]'s first-error-wins/abort-the-rest — the counterpart for a
/// caller whose tasks are independent recovery attempts rather than one all-or-nothing batch (its
/// sole caller today, [`fetch_corrupt_replacements`], has the details on why that distinction
/// matters there).
///
/// Because nothing here is ever aborted, this needs none of [`join_all`]/[`drain_remaining`]'s
/// not-return-while-writing machinery: every task, including one whose sibling already failed,
/// runs to completion on its own and is joined here, so `object_utils::force_store_object_bytes`
/// never races a cancellation. A panicked task collapses into an `Err` in its own slot, the same
/// shape a task's own `Err(String)` return takes — so a sibling panic is reported like any other
/// per-hash failure, never silently dropped and never mistaken for a clean success.
async fn join_all_independent<T: Send + 'static>(mut tasks: JoinSet<Result<T, String>>) -> Vec<Result<T, String>> {
    let mut outcomes = Vec::new();

    while let Some(result) = tasks.join_next().await {
        outcomes.push(result.map_err(|e| format!("A transfer task failed: {}", e)).and_then(|inner| inner));
    }

    outcomes
}

/// Abort and drain every task still in `tasks`. This is the other half of the invariant
/// [`join_all`] needs: a coordinating call must never return while a task pool it spawned is
/// still running — the same class of gap
/// [`crate::model::task::TaskExecutor::execute`] closes at the worker-pool level (see its doc for
/// the full guarantee), not one specific to this module. `abort_all` only *signals* cancellation,
/// and tokio can only land that signal at a task's next await point — a task already past its
/// network fetch and inside this module's synchronous, non-yielding store helpers (see
/// [`join_all`]'s doc for which two, and why) keeps running regardless, so the signal alone is
/// not enough. Draining here, before the caller returns whatever error it already has, is what
/// closes that gap.
///
/// Deliberately not unified with `TaskExecutor::execute`'s drain despite the resemblance: that
/// one is a worker-pool loop reporting a panic into the executor's shared error state, this one
/// is a one-shot task-set drain folding panic evidence into a returned message — different
/// levels, and the shape they share is only a handful of lines, too little to justify an
/// abstraction that would cost more than the duplication it removes.
///
/// Most results are discarded: a cancelled join (the abort actually landing), an ordinary task
/// error, even a late success — the caller already has its first error, and first error wins. A
/// *panic* is the one exception: this crate's core never prints on its own, so a sibling's panic
/// during the drain window must not simply vanish. Every `JoinError` for which
/// [`tokio::task::JoinError::is_panic`] is true has its payload collected into a compact summary
/// and returned — for the caller to append to the error it already has, parenthetically, never to
/// substitute for it: the first error observed remains the one a caller of [`join_all`] acts on,
/// with the panic evidence riding along as context, not replacing it. `Some` combined note if one
/// or more siblings panicked, `None` if nothing did.
///
/// `tokio::task::JoinSet::shutdown` is deliberately not used here even though it is documented as
/// exactly `abort_all` followed by draining every result: it discards those results outright,
/// which is incompatible with collecting the panic evidence described above.
///
/// Like [`join_all`], this guarantee holds only while the returned future is polled to
/// completion — a `timeout` or `select!` around the caller can drop `tasks` before this drain
/// ever runs.
async fn drain_remaining<T: Send + 'static>(mut tasks: JoinSet<T>) -> Option<String> {
    tasks.abort_all();

    let mut panics: Vec<String> = Vec::new();

    while let Some(result) = tasks.join_next().await {
        if let Err(join_error) = result {
            if let Ok(payload) = join_error.try_into_panic() {
                panics.push(panic_payload_message(payload));
            }
        }
    }

    if panics.is_empty() {
        None
    } else if panics.len() == 1 {
        Some(format!("a sibling task panicked during the drain: {}", panics[0]))
    } else {
        Some(format!(
            "{} sibling tasks panicked during the drain: {}", panics.len(), panics.join("; ")
        ))
    }
}

/// Best-effort extraction of a human-readable message from a task's panic payload for
/// [`drain_remaining`]. `&str` and `String` cover every panic this codebase raises (a bare
/// `panic!("literal")` or a formatted `panic!("{}", ...)`); anything else falls back to a fixed
/// placeholder rather than losing the fact that a panic happened at all.
fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        message.to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "panic with a non-string payload".to_string()
    }
}

/// What the trust/office synchronization of a lower/franchise did.
#[derive(Default)]
pub struct TrustSyncStats {
    /// Whether the remote's trust anchor was adopted locally (first contact).
    pub adopted_anchor: bool,

    /// Whether the local office pallet moved to the remote's head.
    pub office_moved: bool,

    /// The transfer stats of the office history fetch.
    pub fetch: FetchStats,
}

/// Adopt the remote's trust state (lower/franchise direction): fetch the office
/// history, adopt the anchor on first contact, and fast-forward the local office ref.
/// A remote whose anchor differs from the local one is refused — that is either
/// tampering or an unrelated warehouse, and no data is worth guessing which.
///
/// # Arguments
/// * `client` - The remote.
/// * `info`   - The remote's handshake.
///
/// # Returns
/// * `Ok(TrustSyncStats)` - What happened.
/// * `Err(String)`        - On anchor mismatch, or a failed transfer.
pub async fn adopt_remote_trust(client: &RemoteClient,
                                info: &WarehouseInfo) -> Result<TrustSyncStats, String> {
    let mut stats = TrustSyncStats::default();

    let Some(remote_trust) = &info.trust else {
        return Ok(stats);
    };

    let remote_office_head = info.pallets.get(&office_utils::office_wire_key())
        .ok_or("The remote has a trust anchor but no office pallet; it is corrupt.".to_string())?;

    if let Some(local) = office_utils::read_trust_anchor()? {
        if local.genesis != remote_trust.genesis {
            // A re-genesis (§8.7) is a conspicuous trust reset: never adopted
            // silently. When the remote's new anchor names our genesis as its prior,
            // the instruction is the conscious re-accept command; anything else is
            // another warehouse — or tampering.
            if remote_trust.prior_genesis.as_deref() == Some(local.genesis.as_str()) {
                return Err(format!(
                    "The remote's trust anchor was RESET (re-genesis): new genesis {} \
                    replaces this warehouse's {}. This changes who controls the \
                    warehouse — verify out-of-band that the reset is legitimate, then \
                    accept it consciously with \"office accept-regenesis\".",
                    remote_trust.genesis, local.genesis
                ));
            }

            return Err(format!(
                "The remote's trust anchor (genesis {}) differs from this warehouse's \
                (genesis {}). This is another warehouse — or tampering. Refusing to sync.",
                remote_trust.genesis, local.genesis
            ));
        }
    }

    stats.fetch = fetch_history(client, remote_office_head).await?;

    if office_utils::read_trust_anchor()?.is_none() {
        office_utils::write_trust_anchor(&remote_trust.to_anchor())?;
        stats.adopted_anchor = true;
    }

    match pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME)? {
        None => {
            pallet_utils::set_meta_pallet_head(OFFICE_PALLET_NAME, remote_office_head)?;
            stats.office_moved = true;
        }
        Some(local_head) if &local_head == remote_office_head => {}
        Some(local_head) if merge_utils::is_ancestor(&local_head, remote_office_head)? => {
            pallet_utils::set_meta_pallet_head(OFFICE_PALLET_NAME, remote_office_head)?;
            stats.office_moved = true;
        }
        // The local office is ahead: the next lift pushes it.
        Some(local_head) if merge_utils::is_ancestor(remote_office_head, &local_head)? => {}
        Some(_) => {
            return Err(
                "The local and remote office histories have diverged. This can be two \
                admins changing the office concurrently — or tampering. The office has no \
                automatic merge yet (its records interdepend), so it is kept linear: \
                reconcile the two office chains by hand before syncing.".to_string()
            );
        }
    }

    Ok(stats)
}

/// Consciously accept a remote's re-genesis (§8.7): the loud, deliberate counterpart
/// of the refusal `adopt_remote_trust` raises when the remote's anchor was reset.
/// Verifies the chain of custody (the new anchor names the local genesis as its
/// prior), fetches and verifies the new office chain, then replaces the local anchor
/// and moves the office ref. The *decision* to trust the reset is the caller's — this
/// is the mechanical half.
///
/// # Arguments
/// * `client` - The remote.
///
/// # Returns
/// * `Ok((TrustAnchor, TrustAnchor))` - The replaced and the adopted anchor.
/// * `Err(String)`                    - If there is nothing to accept, the custody
///                                      chain does not match, or a transfer failed.
pub async fn accept_regenesis(client: &RemoteClient) -> Result<(office_utils::TrustAnchor, office_utils::TrustAnchor), String> {
    let Some(local) = office_utils::read_trust_anchor()? else {
        return Err(
            "This warehouse has no trust anchor; a plain \"lower\" adopts the remote's \
            trust on first contact.".to_string()
        );
    };

    let info = client.fetch_info().await?;

    let Some(remote_trust) = &info.trust else {
        return Err("The remote has no trust anchor; there is no re-genesis to accept.".to_string());
    };

    if remote_trust.genesis == local.genesis {
        return Err("The remote's trust anchor matches this warehouse's; there is nothing to accept.".to_string());
    }

    if remote_trust.prior_genesis.as_deref() != Some(local.genesis.as_str()) {
        return Err(format!(
            "The remote's anchor (genesis {}) does not name this warehouse's genesis \
            ({}) as its prior — this is not a re-genesis of the chain you trust, but \
            another warehouse or a second reset you have not seen. Refusing.",
            remote_trust.genesis, local.genesis
        ));
    }

    let remote_office_head = info.pallets.get(&office_utils::office_wire_key())
        .ok_or("The remote has a trust anchor but no office pallet; it is corrupt.".to_string())?;

    fetch_history(client, remote_office_head).await?;

    // Never adopt an anchor whose chain does not even verify against itself.
    let new_anchor = remote_trust.to_anchor();
    crate::util::audit_utils::verify_office_chain(&new_anchor, remote_office_head)?;

    office_utils::replace_trust_anchor(&new_anchor)?;
    pallet_utils::set_meta_pallet_head(OFFICE_PALLET_NAME, remote_office_head)?;

    Ok((local, new_anchor))
}

/// Push the local trust state (lift direction): establish the anchor on the remote when
/// it has none, and lift the office pallet so the key registry is on the remote before
/// the working pallet's signed parcels arrive.
///
/// # Arguments
/// * `client` - The remote.
/// * `info`   - The remote's handshake.
///
/// # Returns
/// * `Ok(Some(LiftResult))` - Trust is established locally; the office lift's outcome.
/// * `Ok(None)`             - This warehouse has no trust; nothing to push.
/// * `Err(String)`          - On anchor mismatch, a remote that is ahead, or a failed
///                            transfer.
pub async fn push_local_trust(client: &RemoteClient,
                              info: &WarehouseInfo) -> Result<Option<LiftResult>, String> {
    let Some(local) = office_utils::read_trust_anchor()? else {
        if info.trust.is_some() {
            return Err(
                "The remote has trust established but this warehouse does not. \
                \"lower\" first to adopt the remote's office.".to_string()
            );
        }

        return Ok(None);
    };

    if let Some(remote_trust) = &info.trust {
        if remote_trust.genesis != local.genesis {
            // This warehouse re-genesised and the remote still holds the prior
            // anchor: push the replacement. The server gates it — only its operator
            // authority (the static token) may sanction a trust reset, and only one
            // that adopts the remote's current office head.
            if local.prior_genesis.as_deref() == Some(remote_trust.genesis.as_str()) {
                client.put_trust(&TrustAnchorDto::from(&local)).await?;
            } else if remote_trust.prior_genesis.as_deref() == Some(local.genesis.as_str()) {
                return Err(format!(
                    "The remote's trust anchor was RESET (re-genesis): new genesis {} \
                    replaces this warehouse's {}. This changes who controls the \
                    warehouse — verify out-of-band that the reset is legitimate, then \
                    accept it consciously with \"office accept-regenesis\".",
                    remote_trust.genesis, local.genesis
                ));
            } else {
                return Err(format!(
                    "The remote's trust anchor (genesis {}) differs from this warehouse's \
                    (genesis {}). This is another warehouse — or tampering. Refusing to lift.",
                    remote_trust.genesis, local.genesis
                ));
            }
        }
    } else {
        client.put_trust(&TrustAnchorDto::from(&local)).await?;
    }

    let Some(office_head) = pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME)? else {
        return Err("Trust is established but the office pallet is missing.".to_string());
    };

    // Right after a re-genesis the office lift replaces the remote's chain instead of
    // extending it — allowed exactly when the local anchor adopts the remote's head.
    let office_key = office_utils::office_wire_key();
    let remote_office_head = info.pallets.get(&office_key).map(|hash| hash.as_str());
    let adopted_reset = local.adopts.as_deref() == remote_office_head && remote_office_head.is_some();

    let result = lift_pallet_inner(
        client,
        &office_key,
        &office_head,
        remote_office_head,
        adopted_reset,
        // The office pallet carries only structural, tracked-metadata objects — never a chunked
        // file — so the capability never actually gates it; threaded honestly all the same.
        info.chunking,
    ).await?;

    Ok(Some(result))
}

/// The outcome of lifting one meta pallet (its wire ref, e.g. `@manifest`).
pub struct MetaPalletLift {
    pub pallet: String,
    pub result: LiftResult,
}

/// Lift every *non-office* meta pallet (the manifest, and future ones) to the remote,
/// after the office and trust are already established there. Meta pallets are ordinary
/// signed pallets from the server's point of view — object upload plus a fast-forward
/// CAS — so this reuses `lift_pallet`; a diverged one errors (lower first), exactly like
/// a working pallet. The office is excluded: it is lifted with the trust state
/// (`push_local_trust`) so the remote holds the keys before any pallet that relies on
/// them arrives.
///
/// # Arguments
/// * `client` - The remote.
/// * `info`   - The remote's handshake.
///
/// # Returns
/// * `Ok(Vec<MetaPalletLift>)` - Per-pallet outcomes (empty when none exist).
/// * `Err(String)`             - If a pallet diverged, the remote is ahead, or a
///                               transfer failed.
pub async fn lift_meta_pallets(client: &RemoteClient,
                               info: &WarehouseInfo) -> Result<Vec<MetaPalletLift>, String> {
    let mut lifts = Vec::new();

    for name in pallet_utils::list_meta_pallets()? {
        if name == OFFICE_PALLET_NAME {
            continue;
        }

        let Some(local_head) = pallet_utils::get_meta_pallet_head(&name)? else {
            continue;
        };

        let wire = pallet_utils::PalletRef::meta(&name).to_wire();
        let remote_head = info.pallets.get(&wire).map(String::as_str);

        // Meta pallets never carry a chunked file; the capability is threaded for uniformity.
        let result = lift_pallet(client, &wire, &local_head, remote_head, info.chunking).await?;
        lifts.push(MetaPalletLift { pallet: wire, result });
    }

    Ok(lifts)
}

/// The outcome of adopting the remote's meta pallets.
#[derive(Default)]
pub struct MetaAdoptResult {
    /// Meta pallets fast-forwarded or first adopted (wire refs, e.g. `@manifest`).
    pub adopted: Vec<String>,

    /// Meta pallets whose local and remote heads diverged, as `(bare name, remote head)`.
    /// Their remote history has been fetched; the caller applies the pallet's merge
    /// policy (the manifest merges cleanly; nothing else has one yet).
    pub diverged: Vec<(String, String)>,
}

/// Adopt the remote's *non-office* meta pallets (lower / franchise direction): fetch each
/// and fast-forward the local ref, without ever materializing into the working directory
/// (meta pallets are not working content). A pallet present only on the remote is adopted
/// outright; one that diverged has its remote side fetched and is returned for the caller
/// to merge. The office is excluded — `adopt_remote_trust` handles it.
///
/// # Arguments
/// * `client` - The remote.
/// * `info`   - The remote's handshake.
///
/// # Returns
/// * `Ok(MetaAdoptResult)` - What was adopted, and what diverged.
/// * `Err(String)`         - If a transfer failed.
pub async fn adopt_meta_pallets(client: &RemoteClient,
                                info: &WarehouseInfo) -> Result<MetaAdoptResult, String> {
    let mut result = MetaAdoptResult::default();

    for (key, remote_head) in &info.pallets {
        let Some(name) = key.strip_prefix(pallet_utils::META_QUALIFIER) else {
            continue; // Not a meta pallet (user pallets are handled by lower/franchise).
        };

        if name == OFFICE_PALLET_NAME {
            continue;
        }

        match pallet_utils::get_meta_pallet_head(name)? {
            None => {
                fetch_history(client, remote_head).await?;
                pallet_utils::set_meta_pallet_head(name, remote_head)?;
                result.adopted.push(key.clone());
            }
            // Up to date, or local is ahead — both decidable from the *local* ancestry
            // alone (these walks never load the not-yet-fetched remote head).
            Some(local) if &local == remote_head => {}
            Some(local) if merge_utils::is_ancestor(remote_head, &local)? => {}
            Some(local) => {
                // A fast-forward or a divergence — deciding either needs the remote head's
                // ancestry, so fetch it first, then classify.
                fetch_history(client, remote_head).await?;

                if merge_utils::is_ancestor(&local, remote_head)? {
                    pallet_utils::set_meta_pallet_head(name, remote_head)?;
                    result.adopted.push(key.clone());
                } else {
                    result.diverged.push((name.to_string(), remote_head.clone()));
                }
            }
        }
    }

    Ok(result)
}

/// Resolve the display names of everyone enrolled in this warehouse's office, for the
/// CLI display paths (`history`, `office list`). Names live only in the provider's
/// directory (§8.12: the chain is pseudonymous), so resolution is a request to the
/// configured remote — which decides, knowing who is asking, which names this caller
/// may see. Bounded by the office roster (∝ enrolled users, never history size).
///
/// Best-effort throughout: no remote configured, no office, or any failure yields an
/// empty map and the pseudonymous identifiers stay on screen. Resolution is display
/// sugar, never a verification input, so it can never fail a command.
pub async fn resolve_office_display_names() -> BTreeMap<String, String> {
    // No remote means no directory to ask; only local profile names exist, and those
    // are already the operator's own.
    let Ok(client) = RemoteClient::from_config() else {
        return BTreeMap::new();
    };

    let identifiers = match office_utils::read_office_state() {
        Ok(state) => state.users.into_iter().map(|user| user.identifier).collect::<Vec<String>>(),
        Err(_) => return BTreeMap::new(),
    };

    client.resolve(identifiers).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use crate::builder::object::loose_object_builder::LooseObjectBuilder;
    use crate::model::remote::ErrorResponse;

    /// A host ending in `.onion` is recognized as an onion service; a clearnet host, an IP, and a
    /// bare/garbage string are not — and neither is a `.onion` that only appears in the path or a
    /// query string, since it is the *host* that must be the onion address.
    #[test]
    fn only_onion_hosts_are_recognized() {
        assert!(is_onion_url("http://abcdefghij234567.onion"));
        assert!(is_onion_url("http://abcdefghij234567.onion:80/v1/warehouse"));
        assert!(is_onion_url("http://SubDomain.ABCDEF.onion"), "case-insensitive host match");
        assert!(is_onion_url("http://abcdefghij234567.onion."), "trailing FQDN dot is still onion");

        assert!(!is_onion_url("http://127.0.0.1:9418"));
        assert!(!is_onion_url("https://forklift.example.com"));
        assert!(!is_onion_url("http://example.com/path/to.onion"), "path, not host");
        assert!(!is_onion_url("http://example.com/?q=x.onion"), "query, not host");
        assert!(!is_onion_url("not a url"));
    }

    /// The routing policy is exactly: `On` always dials through Tor, `Off` never does, and `Auto`
    /// does iff the remote is an onion host. This is the whole contract the client's transport
    /// choice rests on, proven without a socket.
    #[test]
    fn tor_routing_policy() {
        let onion = "http://abcdefghij234567.onion";
        let clearnet = "http://127.0.0.1:9418";

        assert!(should_route_through_tor(&TorMode::Auto, onion));
        assert!(!should_route_through_tor(&TorMode::Auto, clearnet));

        assert!(should_route_through_tor(&TorMode::On, onion));
        assert!(should_route_through_tor(&TorMode::On, clearnet), "on routes even clearnet");

        assert!(!should_route_through_tor(&TorMode::Off, onion), "off never routes, even onion");
        assert!(!should_route_through_tor(&TorMode::Off, clearnet));
    }

    /// `remote.tor` parsing: the three canonical values plus their truthy/falsey synonyms, and an
    /// unrecognized value degrading to the safe default (`Auto`) rather than erroring.
    #[test]
    fn tor_mode_parsing() {
        for on in ["on", "On", " ON ", "true", "yes", "1"] {
            assert_eq!(TorMode::parse(on), TorMode::On, "{on:?} is on");
        }
        for off in ["off", "OFF", "false", "no", "0"] {
            assert_eq!(TorMode::parse(off), TorMode::Off, "{off:?} is off");
        }
        for auto in ["auto", "", "sometimes", "socks"] {
            assert_eq!(TorMode::parse(auto), TorMode::Auto, "{auto:?} degrades to auto");
        }
    }

    /// Building a client for an onion remote with an explicit proxy succeeds (the SOCKS proxy is
    /// accepted and wired in), and a non-onion remote under the default `Auto` settings builds a
    /// direct client — the construction path both peers rely on, exercised without any live Tor.
    #[test]
    fn a_tor_client_builds_for_an_onion_remote() {
        let onion = RemoteClient::new_with_tor(
            "http://abcdefghij234567.onion",
            Some("secret".to_string()),
            TorSettings { mode: TorMode::Auto, proxy: DEFAULT_TOR_PROXY.to_string() },
        );
        assert!(onion.is_ok(), "an onion remote builds through the SOCKS proxy: {:?}", onion.err());

        let direct = RemoteClient::new_with_tor(
            "http://127.0.0.1:9418",
            None,
            TorSettings::default(),
        );
        assert!(direct.is_ok(), "a clearnet remote builds directly: {:?}", direct.err());
    }

    /// The server-to-client wire round trip (§7.4): a server-side refusal carries its stable code
    /// in the additive `ErrorResponse.code` field; after that body crosses the wire (here through
    /// real serde), the client's `classify_remote_error` re-frames it so it classifies as the *same*
    /// typed refusal — the same code a local refusal would. The exit-code half of the contract is
    /// asserted in `forklift`'s `output` tests; here we prove the code survives the wire.
    #[test]
    fn a_server_refusal_code_crosses_the_wire_and_classifies_the_same() {
        // The server tagged a 422 with a stable refusal code + next step (as `error_body` does).
        let server_body = ErrorResponse {
            error: "\"big.bin\" is a large file stored in chunks, ...".to_string(),
            code: Some(scope_utils::CODE_CHUNKED_TRANSPORT_UNSUPPORTED.to_string()),
            next_step: Some("Upgrade the remote ...".to_string()),
        };

        // Cross the wire byte-for-byte.
        let json = serde_json::to_string(&server_body).unwrap();
        let parsed: ErrorResponse = serde_json::from_str(&json).unwrap();

        let classified = classify_remote_error(
            422, "the negotiation", parsed.error, parsed.code, parsed.next_step,
        );

        // The client re-lifts it into a typed refusal with the same code — never a leaked frame.
        match CoreError::from(classified) {
            CoreError::Refusal { code, message, next_step } => {
                assert_eq!(code, RefusalCode::ChunkedTransportUnsupported);
                assert!(message.contains("The remote refused the negotiation (422)"),
                    "the client wraps the server message with context: {}", message);
                assert!(!message.contains('\u{1f}'), "no frame leaks: {:?}", message);
                assert_eq!(next_step, "Upgrade the remote ...");
            }
            other => panic!("expected a typed refusal, got {:?}", other),
        }
    }

    /// An **old** server (no `code` field) or a **plain** error keeps working: the client shows the
    /// wrapped message and classifies generically — never a spurious refusal code.
    #[test]
    fn an_uncoded_wire_error_stays_generic() {
        let classified = classify_remote_error(
            500, "the negotiation", "boom".to_string(), None, None,
        );
        assert_eq!(CoreError::from(classified.clone()), CoreError::Other(classified));
    }

    /// A code a newer server sends that this client does not know degrades to a generic error
    /// (with the wrapped message), rather than being invented into a taxonomy exit code — but the
    /// wire's next_step still survives, folded into the message, so recovery guidance is not lost
    /// just because the code was unrecognized.
    #[test]
    fn an_unknown_wire_code_degrades_generically() {
        let classified = classify_remote_error(
            422, "the negotiation", "future refusal".to_string(),
            Some("some_future_code".to_string()), Some("do this".to_string()),
        );
        match CoreError::from(classified) {
            CoreError::Other(message) => {
                assert!(message.contains("future refusal"));
                assert!(message.contains("do this"), "next_step guidance survives: {}", message);
            }
            other => panic!("expected Other, got {:?}", other),
        }
    }

    /// A fresh warehouse root for one test, entered as the active storage-root scope for
    /// its lifetime — `is_known_complete` reads the object store and the commit-graph
    /// under it. Each test gets its own directory, so parallel tests never collide.
    struct Scratch {
        root: PathBuf,
        _scope: StorageRootScope,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let root = std::env::temp_dir().join(format!(
                "forklift-remote-test-{}-{}-{}", name, std::process::id(), id
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = StorageRootScope::enter(&root);

            Scratch { root, _scope: scope }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Store a minimal parcel (a dummy, shared tree hash — ancestry never reads the
    /// tree) with the given parents, tagged so otherwise-identical parcels still hash
    /// distinctly. Mirrors the idiom already used by `merge_utils`'s own ancestry tests.
    fn stack(parents: Vec<String>, tag: &str) -> String {
        let parcel = crate::model::parcel::Parcel {
            tree_hash: "0".repeat(64),
            parents,
            actions: Vec::new(),
            description: Some(tag.to_string()),
        };
        let mut object = LooseObjectBuilder::build_parcel(&parcel);
        object.store().unwrap();
        object.hash
    }

    #[test]
    fn encode_path_segments_encodes_the_reserved_set_but_preserves_separators() {
        // The reserved/unsafe characters Copilot flagged (space, #, ?, %) each round-trip when
        // percent-decoded, and `/` stays a literal separator — never itself encoded to `%2F` —
        // so a multi-segment path still arrives as multiple segments on the wire.
        assert_eq!(encode_path_segments("a b"), "a%20b");
        assert_eq!(encode_path_segments("a#b"), "a%23b");
        assert_eq!(encode_path_segments("a?b"), "a%3Fb");
        assert_eq!(encode_path_segments("a%b"), "a%25b");
        assert_eq!(encode_path_segments("src/a b/c#d"), "src/a%20b/c%23d");

        // Unreserved characters (alphanumerics, `-`, `_`, `.`, `~`) are left untouched.
        assert_eq!(encode_path_segments("src/api-v2_final.txt~bak"), "src/api-v2_final.txt~bak");

        // An empty segment (leading/trailing/doubled `/`) is preserved as empty, not collapsed.
        assert_eq!(encode_path_segments("/a//b/"), "/a//b/");
    }

    #[test]
    fn a_hash_never_fetched_is_never_complete() {
        let _scratch = Scratch::new("known-complete-absent");

        let phantom_hash = "f".repeat(64);
        assert!(!is_known_complete(&phantom_hash, &[phantom_hash.clone()]).unwrap());
    }

    #[test]
    fn a_hash_that_is_itself_a_complete_head_is_complete() {
        let _scratch = Scratch::new("known-complete-self");

        let head = stack(Vec::new(), "head");
        assert!(is_known_complete(&head, &[head.clone()]).unwrap());
    }

    #[test]
    fn an_ancestor_of_a_complete_head_is_complete() {
        let _scratch = Scratch::new("known-complete-ancestor");

        let root = stack(Vec::new(), "root");
        let child = stack(vec![root.clone()], "child");

        assert!(is_known_complete(&root, &[child]).unwrap());
    }

    #[test]
    fn an_unrelated_parcel_is_not_complete() {
        let _scratch = Scratch::new("known-complete-unrelated");

        let trunk_root = stack(Vec::new(), "trunk-root");
        let trunk_tip = stack(vec![trunk_root], "trunk-tip");
        let other = stack(Vec::new(), "other-branch-root");

        assert!(!is_known_complete(&other, &[trunk_tip]).unwrap());
    }

    #[test]
    fn no_complete_heads_means_nothing_is_complete() {
        let _scratch = Scratch::new("known-complete-no-heads");

        let head = stack(Vec::new(), "lonely");
        assert!(!is_known_complete(&head, &[]).unwrap());
    }

    #[test]
    fn every_complete_head_is_checked_not_just_the_first() {
        let _scratch = Scratch::new("known-complete-second-head");

        let root = stack(Vec::new(), "root");
        let child = stack(vec![root.clone()], "child");
        let unrelated = stack(Vec::new(), "unrelated");

        // `unrelated` (checked first) is not an ancestry match; `child` (checked second)
        // is — the loop must not stop at the first miss.
        assert!(is_known_complete(&root, &[unrelated, child]).unwrap());
    }

    /// The classification split the staged-lift commit relies on: parcels and trees go to the
    /// control plane (promoted synchronously), everything else is a blob (presence-checked).
    #[test]
    fn staged_objects_split_into_control_plane_and_blobs() {
        let parcel = "a".repeat(64);
        let tree = "b".repeat(64);
        let blob_one = "c".repeat(64);
        let blob_two = "d".repeat(64);

        let control_plane: HashSet<String> =
            [parcel.clone(), tree.clone()].into_iter().collect();

        let mut targets = BTreeMap::new();
        for hash in [&parcel, &tree, &blob_one, &blob_two] {
            targets.insert(hash.clone(), format!("https://storage/staging/s/{}", hash));
        }

        let (mut control, mut blobs) = classify_staged(&targets, &control_plane);
        control.sort();
        blobs.sort();

        assert_eq!(control, vec![parcel.clone(), tree.clone()]);
        assert_eq!(blobs, vec![blob_one, blob_two]);
    }

    /// A staged set of only control-plane objects (a metadata-only lift with no file content)
    /// yields no blobs, so the commit promotes everything synchronously and never waits on the
    /// out-of-band verifier.
    #[test]
    fn a_control_plane_only_stage_has_no_blobs() {
        let parcel = "a".repeat(64);
        let tree = "b".repeat(64);
        let control_plane: HashSet<String> =
            [parcel.clone(), tree.clone()].into_iter().collect();

        let mut targets = BTreeMap::new();
        targets.insert(parcel.clone(), "u1".to_string());
        targets.insert(tree.clone(), "u2".to_string());

        let (control, blobs) = classify_staged(&targets, &control_plane);
        assert_eq!(control.len(), 2);
        assert!(blobs.is_empty());
    }

    /// A small lift fits in one batch, which carries both lists intact and, being the last (only)
    /// batch, is committed with `more: false` (the caller's sweep runs) — the pre-pagination path.
    #[test]
    fn a_small_commit_is_a_single_batch() {
        let control: Vec<String> = vec!["a".repeat(64), "b".repeat(64)];
        let blobs: Vec<String> = vec!["c".repeat(64)];

        let batches = build_commit_batches(&control, &blobs, MAX_MISSING_BATCH);

        assert_eq!(batches.len(), 1, "a small lift is one batch");
        assert_eq!(batches[0].0, control, "the control plane rides intact");
        assert_eq!(batches[0].1, blobs, "the blobs ride intact");
    }

    /// Two empty lists still yield one (empty) batch, so the final-batch staging sweep always runs.
    #[test]
    fn an_empty_commit_still_yields_one_batch_so_the_sweep_runs() {
        let batches = build_commit_batches(&[], &[], MAX_MISSING_BATCH);
        assert_eq!(batches.len(), 1);
        assert!(batches[0].0.is_empty() && batches[0].1.is_empty());
    }

    /// The cap applies to the two lists *combined*: control-plane hashes fill each batch first,
    /// then blobs, and neither list is reordered across the batch boundary. Every batch but the
    /// last is exactly `cap` hashes; the last carries the remainder.
    #[test]
    fn batches_respect_the_combined_cap_and_preserve_order() {
        let control: Vec<String> = (0..3).map(|i| format!("c{}", i)).collect();
        let blobs: Vec<String> = (0..3).map(|i| format!("b{}", i)).collect();

        // cap = 4: batch 1 takes all 3 control + 1 blob; batch 2 takes the remaining 2 blobs.
        let batches = build_commit_batches(&control, &blobs, 4);

        assert_eq!(batches.len(), 2, "6 hashes at cap 4 is two batches");
        assert_eq!(batches[0].0, vec!["c0", "c1", "c2"], "control fills first, in order");
        assert_eq!(batches[0].1, vec!["b0"], "then blobs, in order");
        assert_eq!(batches[0].0.len() + batches[0].1.len(), 4, "the first batch is exactly the cap");
        assert_eq!(batches[1].0, Vec::<String>::new(), "control is exhausted");
        assert_eq!(batches[1].1, vec!["b1", "b2"], "the last batch carries the blob remainder");

        // Reassembling the batches recovers the inputs exactly (nothing dropped or duplicated).
        let seen_control: Vec<String> = batches.iter().flat_map(|(c, _)| c.clone()).collect();
        let seen_blobs: Vec<String> = batches.iter().flat_map(|(_, b)| b.clone()).collect();
        assert_eq!(seen_control, control);
        assert_eq!(seen_blobs, blobs);
    }

    /// The commit-pagination gate (§9.4b W3, the pure boundary): a staged count at or under the
    /// per-batch cap is fine regardless of remote support (one batch either way); over the cap,
    /// only a chunking-capable remote (which understands the additive `more` field) may proceed —
    /// a non-chunking remote is refused, naming the exact staged count. The over-cap+chunking→Ok
    /// arm is asserted here in isolation; the wire-level positive case (driving a real
    /// `negotiate_and_upload` past the gate) is intentionally not duplicated, since it would need
    /// a >10k-candidate upload spawn — the refusal-side wire test
    /// (`a_large_lift_to_a_non_chunking_remote_refuses_before_any_upload`) already proves
    /// `negotiate_and_upload` threads the capability into this same gate.
    #[test]
    fn commit_pagination_gate_refuses_only_when_over_cap_and_unsupported() {
        assert!(
            refuse_if_commit_pagination_unsupported(MAX_MISSING_BATCH, false).is_ok(),
            "exactly the cap needs only one batch, even against a non-chunking remote"
        );
        assert!(
            refuse_if_commit_pagination_unsupported(MAX_MISSING_BATCH + 1, true).is_ok(),
            "a chunking-capable remote may paginate past the cap"
        );
        assert!(
            refuse_if_commit_pagination_unsupported(1, false).is_ok(),
            "a tiny lift is never gated"
        );

        let error = refuse_if_commit_pagination_unsupported(MAX_MISSING_BATCH + 1, false)
            .expect_err("over the cap, against a non-chunking remote, must refuse");
        let CoreError::Refusal { code, message, next_step } = error else {
            panic!("expected a typed refusal, got {:?}", error);
        };

        assert_eq!(code, RefusalCode::CommitPaginationUnsupported);
        assert!(
            message.contains(&(MAX_MISSING_BATCH + 1).to_string()),
            "the refusal names the staged count: {}", message
        );
        assert!(next_step.contains("Upgrade the remote"), "{}", next_step);
    }

    /// The fallback decision: only a `404`/`405` means the remote lacks the endpoint; every
    /// other non-success status is a real error the caller must surface, not silently fall back
    /// on (falling back on, say, a `500` would mask it).
    #[test]
    fn only_404_and_405_trigger_the_legacy_fallback() {
        assert!(endpoint_absent(reqwest::StatusCode::NOT_FOUND));
        assert!(endpoint_absent(reqwest::StatusCode::METHOD_NOT_ALLOWED));

        for status in [
            reqwest::StatusCode::OK,
            reqwest::StatusCode::UNAUTHORIZED,
            reqwest::StatusCode::FORBIDDEN,
            reqwest::StatusCode::CONFLICT,
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
        ] {
            assert!(!endpoint_absent(status), "{} must not fall back", status);
        }
    }

    /// The commit-retry decision: only a `422` carrying the shared blob-not-ready marker is
    /// transient. A control-plane object never uploaded, a corrupt staged object, and any
    /// non-`422` are all terminal — retrying them would just waste the backoff budget.
    #[test]
    fn only_the_blob_not_ready_marker_is_retried() {
        let unprocessable = reqwest::StatusCode::UNPROCESSABLE_ENTITY;

        // The exact message a staging head builds for a blob still in staging (mirrors head.rs).
        let not_ready = format!(
            "Blob {} is {}; the lift session is not ready to commit.",
            "a".repeat(64), LIFT_SESSION_BLOB_NOT_READY
        );
        assert!(is_transient_commit_failure(unprocessable, &not_ready));

        // Terminal 422s: a missing control-plane object and a corrupt staged object.
        assert!(!is_transient_commit_failure(
            unprocessable,
            "Object x was not uploaded; the lift session is not ready to commit."
        ));
        assert!(!is_transient_commit_failure(
            unprocessable,
            "Staged object x is corrupt (it hashes to y); it was discarded, not promoted."
        ));

        // The marker on a non-422 status is not transient either (only a 422 carries it).
        assert!(!is_transient_commit_failure(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR, &not_ready
        ));
    }

    /// Review round 5, finding 2: `error_body_read_budget` must fold the caller's own
    /// `connect_timeout` into the flat `ERROR_BODY_READ_TIMEOUT` base — a Tor-routed client's
    /// (60s) connect budget must survive into the composed bound, not just the direct client's
    /// (5s) one. Asserts the arithmetic directly rather than constructing a live Tor client and
    /// waiting out the real budget (the exact ~72s shape deleted from this suite in 8b93d00).
    ///
    /// This pins one of two links; on its own it pins only the helper's arithmetic, not that
    /// `error_of` actually calls it (review round 6, finding 1 — nothing enforced that link
    /// before `missing_objects_bounds_the_error_body_read_after_a_wedged_500` below started
    /// asserting a lower bound on elapsed time). The two together — call-site-uses-helper and
    /// helper-arithmetic — pin `error_of`'s real behavior without a live remote.
    #[test]
    fn error_body_read_budget_folds_in_the_connect_timeout() {
        assert_eq!(
            error_body_read_budget(TEST_TOR_CONNECT_TIMEOUT),
            TEST_TOR_CONNECT_TIMEOUT + ERROR_BODY_READ_TIMEOUT,
            "a Tor-routed client's 60s connect budget must be folded into the error-body-read \
            bound, not just the flat 10s base — a bound that ignores the link's own latency \
            preempts healthy work on a slow-but-legitimate connection"
        );
        assert_eq!(
            error_body_read_budget(TEST_DIRECT_CONNECT_TIMEOUT),
            TEST_DIRECT_CONNECT_TIMEOUT + ERROR_BODY_READ_TIMEOUT,
            "the same folding must hold for a direct (5s) client too, not just Tor"
        );
    }

    /// A lift session id is a distinct, hyphenated uuid-shaped string — a safe single path
    /// component for a `staging/{session}/{hash}` key.
    #[test]
    fn lift_session_ids_are_unique_and_path_safe() {
        let one = new_lift_session();
        let two = new_lift_session();

        assert_ne!(one, two);
        assert_eq!(one.len(), 36);
        assert_eq!(one.matches('-').count(), 4);
        assert!(
            one.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
            "a session id must be a safe path component: {}", one
        );
    }

    fn store_blob(content: &str) -> String {
        let mut object = LooseObjectBuilder::build_blob(&crate::model::blob::Blob {
            content: content.as_bytes().to_vec(),
        });
        object.store().unwrap();
        object.hash
    }

    fn store_tree(entries: &[(&str, &str, crate::enums::dir_entry_type::DirEntryType)]) -> String {
        use crate::model::tree_item::TreeItem;

        let mut tree = TreeItem::new(
            String::new(), String::new(), crate::enums::dir_entry_type::DirEntryType::Tree
        );
        for (name, hash, item_type) in entries {
            tree.add_child(TreeItem::new(name.to_string(), hash.to_string(), *item_type));
        }

        let mut object = LooseObjectBuilder::build_tree(&tree);
        object.store().unwrap();
        object.hash
    }

    /// The lift closure walk prunes a subtree against **every** parent, not just the
    /// first — so a merge parcel that adopted an out-of-scope sibling by hash from its *second*
    /// parent treats that subtree as base-explained and never loads it. This is what makes a
    /// sparse-workspace merge liftable; it is also a strictly-not-larger candidate set in a full
    /// store. Modeled on the exact review construction.
    #[test]
    fn the_closure_walk_prunes_a_subtree_adopted_from_the_second_parent() {
        use crate::enums::dir_entry_type::DirEntryType::{Normal, Tree};

        let _scratch = Scratch::new("closure-multi-parent");

        // An in-scope file edited on ours, an out-of-scope file edited on theirs.
        let api_v1 = store_blob("api v1");
        let api_v2 = store_blob("api v2");
        let web_v0 = store_blob("web v0");
        let web_v1 = store_blob("web v1");

        let api_base = store_tree(&[("a.txt", &api_v1, Normal)]);
        let api_ours = store_tree(&[("a.txt", &api_v2, Normal)]);
        let web_base = store_tree(&[("w.txt", &web_v0, Normal)]);
        let web_theirs = store_tree(&[("w.txt", &web_v1, Normal)]);

        // ours changed api (web unchanged); theirs changed web (api unchanged); the merge combines
        // ours' api with theirs' web.
        let src_ours = store_tree(&[("api", &api_ours, Tree), ("web", &web_base, Tree)]);
        let src_theirs = store_tree(&[("api", &api_base, Tree), ("web", &web_theirs, Tree)]);
        let src_merge = store_tree(&[("api", &api_ours, Tree), ("web", &web_theirs, Tree)]);

        let root_ours = store_tree(&[("src", &src_ours, Tree)]);
        let root_theirs = store_tree(&[("src", &src_theirs, Tree)]);
        let root_merge = store_tree(&[("src", &src_merge, Tree)]);

        let walk = |bases: &[String]| -> Vec<String> {
            let mut seen_trees = HashSet::new();
            let mut seen_blobs = HashSet::new();
            let mut seen_recipes = HashSet::new();
            let mut candidates = Vec::new();
            collect_changed_closure(&root_merge, "", bases, &mut seen_trees, &mut seen_blobs,
                                    &mut seen_recipes, &mut candidates, true)
                .expect("the closure walk must not load a pruned object");
            candidates
        };

        let multi = walk(&[root_ours.clone(), root_theirs.clone()]);
        let single = walk(std::slice::from_ref(&root_ours)); // the old first-parent-only base

        // Multi-parent: the merge only combines the two parents' subtrees, so nothing below the
        // merge spine needs uploading. The second parent's out-of-scope subtree (and its blob) is
        // pruned — never collected, never loaded — and so is the first parent's.
        assert!(multi.contains(&root_merge) && multi.contains(&src_merge), "the merge spine is new");
        assert!(!multi.contains(&web_theirs), "the second parent's subtree must be pruned");
        assert!(!multi.contains(&web_v1), "the second parent's blob must be pruned");
        assert!(!multi.contains(&api_ours), "the first parent's subtree must be pruned");

        // First-parent-only (the old walk): the second parent's subtree is NOT explained by the
        // first parent, so it is collected — and in a sparse store its load would fail.
        assert!(single.contains(&web_theirs) && single.contains(&web_v1),
            "the first-parent-only walk collects the absent second-parent subtree");

        assert!(multi.len() < single.len(),
            "the multi-parent base prunes strictly more here: {} vs {}", multi.len(), single.len());
    }

    /// A base-explained tree must be recorded as seen even though it returns early — otherwise
    /// the identical (content-deduplicated) hash reappearing at a second path, where no base
    /// explains it there, gets redundantly loaded and walked. Two paths reference the SAME
    /// subtree hash; the first is base-explained (skipped), the second is not. Deleting the
    /// object from the store before the walk proves it is never loaded at the second path either
    /// — the walk must recognize the hash as already seen, not attempt to load and descend it.
    #[test]
    fn a_base_explained_tree_is_marked_seen_so_a_second_path_does_not_reload_it() {
        use crate::enums::dir_entry_type::DirEntryType::{Normal, Tree};

        let _scratch = Scratch::new("closure-dedup-seen");

        let shared_file = store_blob("shared content");
        let shared = store_tree(&[("f.txt", &shared_file, Normal)]);

        // The base has `shared` at "one" only. The new tree references the SAME `shared` hash at
        // both "one" (base-explained there) and "two" (no base entry there at all).
        let base_root = store_tree(&[("one", &shared, Tree)]);
        let new_root = store_tree(&[("one", &shared, Tree), ("two", &shared, Tree)]);

        // Prove the object is never loaded on the "two" path: delete it from the store up front
        // and confirm the walk still succeeds and never collects it — a load on the "two" path
        // would fail (and the walk would error) because the object is gone.
        let (folder, file_name) = crate::util::file_utils::get_path_for_object(&shared).unwrap();
        std::fs::remove_file(PathBuf::from(folder).join(file_name))
            .expect("the shared tree object must exist to delete");

        let mut seen_trees = HashSet::new();
        let mut seen_blobs = HashSet::new();
        let mut seen_recipes = HashSet::new();
        let mut candidates = Vec::new();
        collect_changed_closure(&new_root, "", &[base_root], &mut seen_trees, &mut seen_blobs,
                                &mut seen_recipes, &mut candidates, true)
            .expect("the second path must be recognized as already-seen, not re-loaded");

        assert!(!candidates.contains(&shared),
            "the base-explained subtree must not be re-collected at the second path");
        assert!(seen_trees.contains(&shared),
            "the base-explained subtree must be marked seen so a second path skips it too");
    }

    /// Build and store a recipe from the given chunk contents (each a real, stored `Chunk`
    /// object), returning `(recipe_hash, chunk_hashes)`. The recipe's `content_hash` and sizes are
    /// consistent, so it passes the structural load check the closure descent runs.
    fn store_recipe(chunk_contents: &[&str]) -> (String, Vec<String>) {
        use crate::model::chunk::Chunk;
        use crate::model::recipe::{Recipe, RecipeChunk};

        let mut chunk_hashes: Vec<String> = Vec::new();
        let mut recipe_chunks: Vec<RecipeChunk> = Vec::new();
        let mut hasher = blake3::Hasher::new();

        for content in chunk_contents {
            let bytes = content.as_bytes().to_vec();
            hasher.update(&bytes);

            let mut object = LooseObjectBuilder::build_chunk(&Chunk { content: bytes.clone() });
            object.store().unwrap();
            recipe_chunks.push(RecipeChunk { hash: object.hash.clone(), size: bytes.len() as u64 });
            chunk_hashes.push(object.hash);
        }

        let total_size = recipe_chunks.iter().map(|chunk| chunk.size).sum();
        let recipe = Recipe {
            content_hash: hasher.finalize().to_hex().to_string(),
            total_size,
            chunks: recipe_chunks,
        };

        let mut object = LooseObjectBuilder::build_recipe(&recipe);
        object.store().unwrap();
        (object.hash, chunk_hashes)
    }

    /// Against a remote that does **not** advertise chunking, the lift closure walk refuses a
    /// chunked file entry before any negotiation or upload: an old head's `gc` would silently
    /// collect a recipe's chunks (B1). Named by its full path; a plain sibling in the same tree is
    /// unaffected (proving the guard is scoped to the one chunked entry, not the walk). The walk
    /// refuses before loading the recipe, so a placeholder hash is enough — a load-order guarantee.
    #[test]
    fn the_lift_refuses_a_chunked_file_to_a_non_chunking_remote() {
        use crate::enums::dir_entry_type::DirEntryType::{Normal, NormalChunked, Tree};

        let _scratch = Scratch::new("closure-chunked-refuses-old-remote");

        let plain = store_blob("small file");
        let fake_recipe_hash = "a".repeat(64);

        let src = store_tree(&[
            ("plain.txt", &plain, Normal),
            ("big.bin", &fake_recipe_hash, NormalChunked),
        ]);
        let root = store_tree(&[("src", &src, Tree)]);

        let mut seen_trees = HashSet::new();
        let mut seen_blobs = HashSet::new();
        let mut seen_recipes = HashSet::new();
        let mut candidates = Vec::new();

        // `false` = the remote's handshake omitted the chunking capability.
        let error = collect_changed_closure(&root, "", &[], &mut seen_trees, &mut seen_blobs,
                                            &mut seen_recipes, &mut candidates, false)
            .expect_err("a chunked file entry must refuse a lift to a non-chunking remote");

        let (code, message, next_step) = scope_utils::decode_refusal(&error)
            .expect("the refusal must decode via the shared sentinel framing");

        assert_eq!(code, scope_utils::CODE_CHUNKED_TRANSPORT_UNSUPPORTED);
        assert!(message.contains("src/big.bin"), "the refusal names the full path: {}", message);
        assert!(next_step.contains("Upgrade the remote"), "it points at the remote: {}", next_step);
    }

    /// Against a chunk-aware remote, the lift closure walk descends a chunked file's recipe: the
    /// recipe rides the control plane (`seen_recipes`) and every chunk rides the blob plane
    /// (`seen_blobs`), so the negotiation learns to upload all of them. A plain sibling still
    /// negotiates as an ordinary blob. This is the client half of §9.4b W4 — without the chunk
    /// hashes in `candidates`, the remote's ref would advance over a recipe whose chunks never came.
    #[test]
    fn the_lift_negotiates_a_chunked_files_recipe_and_chunks_to_a_chunking_remote() {
        use crate::enums::dir_entry_type::DirEntryType::{Normal, NormalChunked, Tree};

        let _scratch = Scratch::new("closure-chunked-descends");

        let plain = store_blob("small file");
        let (recipe_hash, chunk_hashes) = store_recipe(&["chunk-a", "chunk-b", "chunk-c"]);

        let src = store_tree(&[
            ("plain.txt", &plain, Normal),
            ("big.bin", &recipe_hash, NormalChunked),
        ]);
        let root = store_tree(&[("src", &src, Tree)]);

        let mut seen_trees = HashSet::new();
        let mut seen_blobs = HashSet::new();
        let mut seen_recipes = HashSet::new();
        let mut candidates = Vec::new();

        collect_changed_closure(&root, "", &[], &mut seen_trees, &mut seen_blobs,
                                &mut seen_recipes, &mut candidates, true)
            .expect("a chunked file must negotiate, not refuse, against a chunking remote");

        // The recipe is a control-plane candidate; every chunk is a blob candidate.
        assert!(candidates.contains(&recipe_hash), "the recipe is negotiated");
        assert!(seen_recipes.contains(&recipe_hash), "the recipe is classified control-plane");
        for chunk in &chunk_hashes {
            assert!(candidates.contains(chunk), "every chunk is negotiated: {}", chunk);
            assert!(seen_blobs.contains(chunk), "every chunk is classified as a blob: {}", chunk);
        }
        // The recipe is not double-classified as a blob.
        assert!(!seen_blobs.contains(&recipe_hash), "the recipe is not a blob");
        assert!(candidates.contains(&plain), "the plain sibling still negotiates as a blob");
    }

    /// Plant a blob above the whole-object ceiling directly, bypassing `LooseObject::store`'s
    /// write-side ceiling with a raw, non-durable write. The only way such an object can exist
    /// locally is if it predates the ceiling — mirrors the grandfathered-giant fixture
    /// `bundle_utils`'s own writer-side-refusal tests use (there, imported via an old-version
    /// bundle; here, planted directly, since this module has no bundle-import dependency).
    fn store_giant_blob_bypassing_ceiling() -> String {
        use crate::model::blob::Blob;

        let mut object = LooseObjectBuilder::build_blob(&Blob {
            content: vec![0u8; object_utils::MAX_OBJECT_BYTES + 1],
        });
        let compressed = object.compress().unwrap();
        let (path, file_name) = file_utils::get_path_for_object(&object.hash).unwrap();
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(std::path::Path::new(&path).join(&file_name), compressed).unwrap();
        object.hash
    }

    /// `upload_objects` (the direct-PUT / legacy-remote upload path) refuses an over-ceiling
    /// object before ever touching the network: the size check runs immediately after the bytes
    /// are loaded, before the client call — so pointing the client at an address nothing listens
    /// on still produces the honest size refusal rather than a connection error, proving the
    /// bytes never left. This is the client-side half of the maintainer's chosen posture: a
    /// grandfathered giant refuses honestly at the source instead of surfacing as an opaque
    /// mid-lift error from the server's own import refusal.
    #[test]
    fn upload_objects_refuses_an_over_ceiling_object_before_the_wire() {
        let _scratch = Scratch::new("upload-objects-ceiling");
        let hash = store_giant_blob_bypassing_ceiling();

        // Nothing listens here: if the wire were ever touched, this would surface as a
        // connection error, not the ceiling refusal.
        let client = RemoteClient::new("http://127.0.0.1:1", None).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

        let error = runtime.block_on(upload_objects(&client, &[hash.clone()])).unwrap_err();
        let (code, message, next_step) = scope_utils::decode_refusal(&error)
            .expect("the refusal must decode via the shared sentinel framing");

        assert_eq!(code, scope_utils::CODE_OVERSIZED_TRANSPORT_UNSUPPORTED);
        assert!(message.contains(&hash), "the refusal names the object: {}", message);
        assert!(next_step.contains("signed identity"), "states no migration exists: {}", next_step);
    }

    /// The same refusal on `upload_to_targets` (the presigned-PUT staging path) — the other of
    /// the two upload flows `negotiate_and_upload` dispatches between.
    #[test]
    fn upload_to_targets_refuses_an_over_ceiling_object_before_the_wire() {
        let _scratch = Scratch::new("upload-to-targets-ceiling");
        let hash = store_giant_blob_bypassing_ceiling();

        let client = RemoteClient::new("http://127.0.0.1:1", None).unwrap();
        let mut targets = BTreeMap::new();
        targets.insert(hash.clone(), "http://127.0.0.1:1/presigned".to_string());

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let error = runtime.block_on(upload_to_targets(&client, &targets)).unwrap_err();
        let (code, message, _) = scope_utils::decode_refusal(&error)
            .expect("the refusal must decode via the shared sentinel framing");

        assert_eq!(code, scope_utils::CODE_OVERSIZED_TRANSPORT_UNSUPPORTED);
        assert!(message.contains(&hash), "the refusal names the object: {}", message);
    }

    // -----------------------------------------------------------------------------------
    // Review round S2 fix hole: `upload_objects`/`upload_to_targets` called the synchronous
    // `retrieve_object_by_hash` directly inside a spawned async task, no `spawn_blocking`. On
    // the production multi-thread runtime, with `CONCURRENT_TRANSFERS` (24) workers all
    // eventually blocked in pack retrieval at once, in-flight upload watchdogs go unpolled and
    // can observe a stale `silent_for()` on their next poll — a false "timed out" verdict for a
    // transfer that was never actually silent. A direct starvation test (two concurrent uploads,
    // assert the fast one isn't delayed by the slow one's read) is ~50% flaky on `select!`'s
    // branch-order tiebreak; this instead pins the underlying invariant directly — the runtime
    // keeps making progress on other tasks while a transfer task reads from disk — which is
    // deterministic regardless of `select!`'s internal tiebreak.
    // -----------------------------------------------------------------------------------

    /// With the blocking retrieval moved onto `spawn_blocking`'s separate pool, a single-worker
    /// runtime's one async worker thread stays free to keep polling a sibling task while the
    /// read (here, artificially stretched to 5s via [`set_test_retrieval_delay`]) runs on a
    /// different OS thread entirely. A 100ms ticker should fire close to 50 times over that
    /// window; `>= 35` leaves slack for scheduling jitter without being satisfiable by a runtime
    /// that spent most of the window synchronously blocked (which deterministically misses on
    /// the order of 50 ticks — see the reverted-fix run this test's doc references).
    ///
    /// Review round 5, finding 3: the ticker count alone does not exercise *where*
    /// `current_scope_root()`/the delay seam are captured — moving that capture from
    /// `upload_objects`'s own loop into `retrieve_object_for_upload` itself (see that function's
    /// doc for why the two placements differ on a genuine multi-thread runtime) would leave the
    /// ticker count green regardless, because `Scratch`'s scope would simply become invisible to
    /// the spawned task and the retrieval would silently fail to find the blob — the count isn't
    /// sensitive to *which* thread the capture happens on, only to whether the read blocks. So
    /// this also asserts the discarded `upload_objects` result discriminates the two placements:
    /// correctly captured, the read succeeds and the call fails only at the network step
    /// (nothing listens on `127.0.0.1:1`); captured on the wrong thread, the read itself fails
    /// first (the object is "not found" under the wrong, unscoped root) and the network is never
    /// reached at all. Empirically confirmed both message shapes before writing this assertion:
    /// correctly captured — `"Error while uploading object <hash>: Connection refused (os error
    /// 61)"`; captured on the wrong thread — `"Error while reading object from file
    /// \".forklift/objects/...\": entity not found"`. Distinct enough that
    /// `contains("refused") || contains("connect")` (the same idiom
    /// `fetch_info_against_a_refused_connection_does_not_claim_a_timeout` already uses in this
    /// file) cleanly picks out the correct-placement shape and rejects the wrong one.
    #[test]
    fn upload_objects_retrieval_does_not_starve_the_runtime() {
        let _scratch = Scratch::new("upload-retrieval-no-starve");
        let hash = store_blob("upload-retrieval-no-starve-blob");

        set_test_retrieval_delay(std::time::Duration::from_secs(5));
        struct ResetDelay;
        impl Drop for ResetDelay {
            fn drop(&mut self) {
                set_test_retrieval_delay(std::time::Duration::ZERO);
            }
        }
        let _reset_delay = ResetDelay;

        // Nothing listens here: the artificial delay happens entirely inside the retrieval
        // step, before any network call, so a *successful* retrieval must still fail afterward —
        // at the network step, with a connection-refused message. See this test's own doc for
        // why that specific failure shape is what proves the scope-capture placement is correct.
        let client = RemoteClient::new("http://127.0.0.1:1", None).unwrap();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();

        let ticks = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let ticks_for_ticker = Arc::clone(&ticks);

        let outcome = runtime.block_on(async move {
            let ticker = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    ticks_for_ticker.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            });

            let outcome = upload_objects(&client, &[hash]).await;
            ticker.abort();
            outcome
        });

        let error = outcome.expect_err(
            "nothing listens on 127.0.0.1:1 — this must fail, never succeed"
        );
        assert!(
            error.to_lowercase().contains("refused") || error.to_lowercase().contains("connect"),
            "must fail at the network step (proving the retrieval itself succeeded, having found \
            the blob under the correctly-captured scope), not while reading the object off disk \
            — a retrieval failure here would mean the scope was captured on the wrong thread and \
            the blob was never actually reached: {}", error
        );

        let observed = ticks.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            observed >= 35,
            "expected the single-worker runtime to keep firing ~100ms ticks throughout the \
            ~5s artificially-slow retrieval (~50 ticks), only observed {} — the retrieval \
            starved the runtime's only worker instead of running on the blocking pool",
            observed
        );
    }

    // -----------------------------------------------------------------------------------
    // The commit-pagination gate, end to end (§9.4b Stage 3, W3): a hand-rolled remote so a
    // non-chunking head can be simulated at all (both shipped heads always advertise chunking
    // now). Mirrors the raw-TCP mock pattern `forklift/tests/remote.rs`'s `HookServer` already
    // uses for the hook protocol — proven compatible with `reqwest` as the client.
    // -----------------------------------------------------------------------------------

    /// A minimal HTTP endpoint standing in for a storage-backed remote head, answering only the
    /// two things `negotiate_and_upload` needs to reach its pagination gate: the handshake (with a
    /// caller-chosen `chunking` flag) and `upload-targets` (every requested hash comes back staged,
    /// at a URL on this same server, so an actual upload attempt is directly observable). Anything
    /// else hit (a staging `PUT`, or a commit call) increments `upload_or_commit_hits` — the signal
    /// that the client proceeded past the gate.
    struct FakeStagingRemote {
        url: String,
        upload_or_commit_hits: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl FakeStagingRemote {
        /// One request handled per spawned thread — `CONCURRENT_TRANSFERS` (24) client-side
        /// uploads run in parallel, and a large-lift test staging 10,000+ objects (one connection
        /// each, `Connection: close`) needs that concurrency to run in seconds rather than minutes:
        /// a single-threaded accept loop serializes every round trip.
        fn start(chunking: bool) -> FakeStagingRemote {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let accepted_hits = Arc::clone(&hits);
            let base = url.clone();

            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    let counted = Arc::clone(&accepted_hits);
                    let base = base.clone();

                    std::thread::spawn(move || {
                        handle_fake_remote_request(stream, chunking, &base, &counted);
                    });
                }
            });

            FakeStagingRemote { url, upload_or_commit_hits: hits }
        }

        fn upload_or_commit_hits(&self) -> usize {
            self.upload_or_commit_hits.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Handle exactly one connection/request for [`FakeStagingRemote`].
    fn handle_fake_remote_request(
        mut stream: std::net::TcpStream,
        chunking: bool,
        base: &str,
        hits: &std::sync::atomic::AtomicUsize,
    ) {
        use std::io::Write;

        let Some((_method, path, _had_auth, body)) = read_test_request(&mut stream) else { return };

        let (status, response_body): (&str, String) = if path == "/v1/warehouse" {
            (
                "200 OK",
                format!(
                    r#"{{"protocol":"{}","default_pallet":"main","pallets":{{}},"trust":null,"chunking":{}}}"#,
                    PROTOCOL_VERSION, chunking
                ),
            )
        } else if path == "/v1/objects/upload-targets" {
            // Every requested hash comes back staged, at a URL under this same server — so a
            // client that proceeds to upload is directly observable below.
            let request: UploadTargetsRequest = serde_json::from_slice(&body).unwrap();
            let targets: BTreeMap<String, String> = request.hashes.into_iter()
                .map(|hash| {
                    let target = format!("{}/staging/{}", base, hash);
                    (hash, target)
                })
                .collect();
            let response = UploadTargetsResponse { present: Vec::new(), targets, direct: Vec::new() };
            ("200 OK", serde_json::to_string(&response).unwrap())
        } else {
            // A staging PUT or a commit call: exactly the upload/commit phase the gate exists to
            // prevent from ever running when it refuses.
            hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ("200 OK", "{}".to_string())
        };

        let _ = write!(
            stream,
            "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status, response_body.len(), response_body
        );
        let _ = stream.flush();
    }

    /// Read one HTTP/1.1 request (start line, an `Authorization` header check, and a
    /// content-length body; no other header inspection is needed by any of this file's fakes).
    /// Returns `(method, path, had_authorization_header, body)`.
    fn read_test_request(stream: &mut std::net::TcpStream) -> Option<(String, String, bool, Vec<u8>)> {
        use std::io::Read;

        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];

        let header_end = loop {
            if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                break position + 4;
            }

            match stream.read(&mut chunk) {
                Ok(0) => return None,
                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                Err(_) => return None,
            }
        };

        let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let mut start_line = head.lines().next()?.split_whitespace();
        let method = start_line.next()?.to_string();
        let path = start_line.next()?.to_string();

        let had_authorization = head.lines()
            .any(|line| line.to_ascii_lowercase().starts_with("authorization:"));

        let content_length: usize = head.lines()
            .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
            .and_then(|line| line.split_once(':'))
            .and_then(|(_, value)| value.trim().parse().ok())
            .unwrap_or(0);

        let mut body = buffer[header_end..].to_vec();

        while body.len() < content_length {
            match stream.read(&mut chunk) {
                Ok(0) => return None,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
                Err(_) => return None,
            }
        }

        body.truncate(content_length);

        Some((method, path, had_authorization, body))
    }

    /// A synthetic candidate set larger than `MAX_MISSING_BATCH` — hashes that name no real
    /// object. Fine for a test proving the gate refuses *before* anything is read or uploaded (the
    /// only path that ever touches local storage is downstream of the refusal), but not for a test
    /// that lets negotiation proceed to an actual upload — use [`store_many_blobs`] for those.
    fn oversized_candidate_set() -> Vec<String> {
        (0..MAX_MISSING_BATCH + 1).map(|i| format!("{:064x}", i)).collect()
    }

    /// Store `count` distinct, real (tiny) blob objects and return their hashes — for a test whose
    /// candidates must survive an actual `retrieve_object_by_hash` + upload attempt, unlike
    /// [`oversized_candidate_set`]'s placeholder hashes.
    fn store_many_blobs(tag: &str, count: usize) -> Vec<String> {
        (0..count).map(|i| store_blob(&format!("{}-{}", tag, i))).collect()
    }

    /// The reviewer's exact scenario: against a remote whose handshake omits chunking, a lift
    /// needing more than one commit batch refuses **before any upload** — proven by asserting the
    /// fake server's staging/commit endpoints were never hit at all.
    #[test]
    fn a_large_lift_to_a_non_chunking_remote_refuses_before_any_upload() {
        let remote = FakeStagingRemote::start(false);
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let candidates = oversized_candidate_set();
        let control_plane: HashSet<String> = HashSet::new();

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let error = runtime.block_on(negotiate_and_upload(
            &client, "session-1", &candidates, &control_plane, false,
        )).expect_err("a commit needing multiple batches must refuse a non-chunking remote");

        let (code, message, _) = scope_utils::decode_refusal(&error)
            .expect("the refusal must decode via the shared sentinel framing");
        assert_eq!(code, scope_utils::CODE_COMMIT_PAGINATION_UNSUPPORTED);
        assert!(message.contains(&candidates.len().to_string()), "{}", message);

        assert_eq!(
            remote.upload_or_commit_hits(), 0,
            "nothing was uploaded or committed: the whole upload was never wasted"
        );
    }

    /// A small (single-batch) lift to a non-chunking remote is completely unaffected: the gate
    /// only ever fires when pagination would actually be needed.
    #[test]
    fn a_small_lift_to_a_non_chunking_remote_is_unaffected() {
        let _scratch = Scratch::new("small-lift-non-chunking-remote");
        let remote = FakeStagingRemote::start(false);
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let candidates = store_many_blobs("small", 5);
        let control_plane: HashSet<String> = HashSet::new();

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(negotiate_and_upload(
            &client, "session-3", &candidates, &control_plane, false,
        )).expect("a single-batch lift to an old server is unaffected by the pagination gate");

        assert!(
            remote.upload_or_commit_hits() > 0,
            "the small lift's upload/commit phase ran normally"
        );
    }

    // -----------------------------------------------------------------------------------
    // The batch redirect, over a real socket (the §9.4b LocalStack pass fix). An offloading
    // head answers `POST /v1/objects/batch` with a redirect to a presigned `GET`; the bug this
    // fix closes is that reqwest's default policy replays a `307`/`308` redirect with the
    // *original* request — method and body — which re-`POST`s this call's signed JSON at a URL
    // presigned for `GET` only (a real S3-backed head answers `403 SignatureDoesNotMatch`,
    // LocalStack `500`). `fetch_batch` must instead follow the redirect by hand: a bare `GET`,
    // no body, no `Authorization` header (the presigned URL is self-authorizing). These tests
    // also cover `fetch_recipe_chunks`'s designed-transport invariant: it must never reach
    // `/v1/objects/batch` at all, only ever loose per-object `GET`s.
    // -----------------------------------------------------------------------------------

    /// A minimal HTTP server standing in for an OFFLOADING head. Unlike [`FakeStagingRemote`]
    /// (which never redirects), `POST /v1/objects/batch` here answers a caller-chosen redirect
    /// status pointing at a same-origin `GET` target serving real bundle bytes, and
    /// `GET /v1/objects/{hash}` serves individually-registered object bytes (the loose-fetch
    /// path). Every request's method, path, and whether it carried an `Authorization` header
    /// are recorded, so a test can assert exactly which endpoint a client hit and whether it
    /// leaked its bearer token to the redirect target.
    struct FakeOffloadingRemote {
        url: String,
        hits: Arc<Mutex<Vec<(String, String, bool)>>>,
    }

    impl FakeOffloadingRemote {
        /// `redirect_status` is what `POST /v1/objects/batch` answers with (this fix's server
        /// half only ever emits `303`, but `307`/`308` from an older or non-conforming head
        /// must be followed identically); `bundle` is served at the redirect target;
        /// `objects` seeds the loose `GET /v1/objects/{hash}` endpoint.
        fn start(
            redirect_status: u16,
            bundle: Vec<u8>,
            objects: HashMap<String, Vec<u8>>,
        ) -> FakeOffloadingRemote {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let hits: Arc<Mutex<Vec<(String, String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
            let accepted_hits = Arc::clone(&hits);
            let base = url.clone();
            let bundle = Arc::new(bundle);
            let objects = Arc::new(objects);

            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    let hits = Arc::clone(&accepted_hits);
                    let base = base.clone();
                    let bundle = Arc::clone(&bundle);
                    let objects = Arc::clone(&objects);

                    std::thread::spawn(move || {
                        handle_offloading_request(stream, redirect_status, &base, &bundle, &objects, &hits);
                    });
                }
            });

            FakeOffloadingRemote { url, hits }
        }

        /// How many requests hit `/v1/objects/batch` — must stay `0` for a chunk fetch.
        fn batch_hits(&self) -> usize {
            self.hits.lock().unwrap().iter().filter(|(_, path, _)| path == "/v1/objects/batch").count()
        }

        /// How many requests hit exactly `path`.
        fn hits_for(&self, path: &str) -> usize {
            self.hits.lock().unwrap().iter().filter(|(_, p, _)| p == path).count()
        }

        /// Whether any recorded request to `path` carried an `Authorization` header.
        fn any_had_auth(&self, path: &str) -> bool {
            self.hits.lock().unwrap().iter().any(|(_, p, had_auth)| p == path && *had_auth)
        }
    }

    /// Handle exactly one connection/request for [`FakeOffloadingRemote`].
    fn handle_offloading_request(
        mut stream: std::net::TcpStream,
        redirect_status: u16,
        base: &str,
        bundle: &[u8],
        objects: &HashMap<String, Vec<u8>>,
        hits: &Mutex<Vec<(String, String, bool)>>,
    ) {
        use std::io::Write;

        let Some((method, path, had_auth, _body)) = read_test_request(&mut stream) else { return };
        hits.lock().unwrap().push((method.clone(), path.clone(), had_auth));

        if method == "POST" && path == "/v1/objects/batch" {
            let reason = match redirect_status {
                303 => "See Other",
                307 => "Temporary Redirect",
                308 => "Permanent Redirect",
                _ => "Redirect",
            };
            let location = format!("{}/responses/bundle", base);
            let _ = write!(
                stream,
                "HTTP/1.1 {} {}\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                redirect_status, reason, location
            );
            let _ = stream.flush();
            return;
        }

        if method == "GET" && path == "/responses/bundle" {
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                Content-Length: {}\r\nConnection: close\r\n\r\n",
                bundle.len()
            );
            let _ = stream.write_all(bundle);
            let _ = stream.flush();
            return;
        }

        if method == "GET" {
            if let Some(hash) = path.strip_prefix("/v1/objects/") {
                if let Some(bytes) = objects.get(hash) {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                        Content-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    let _ = stream.write_all(bytes);
                    let _ = stream.flush();
                    return;
                }
            }
        }

        let _ = write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let _ = stream.flush();
    }

    /// The general bug, reproduced against the fast in-process suite (the original repro needed
    /// real S3 + a real head, since the in-memory fakes' `offload_response` only offloads when
    /// explicitly put in staging mode, and no prior test drove an actual `reqwest`-backed client
    /// through that redirect): a multi-object batch fetch against an offloading head must land
    /// every object by following the redirect **by hand**, whatever status it carries.
    #[test]
    fn fetch_missing_objects_follows_a_batch_redirect_by_hand_without_leaking_auth() {
        for redirect_status in [303u16, 307, 308] {
            // The "server side": build the exact bundle bytes a real `POST /v1/objects/batch`
            // would answer, then tear the scope down before the "client side" begins. A hash is
            // purely a function of an object's bytes, so `first`/`second` name the same objects
            // regardless of which scope built them.
            let (first, second, bundle) = {
                let _server_scope = Scratch::new(&format!("offload-batch-server-{}", redirect_status));
                let first = store_blob(&format!("first-{}", redirect_status));
                let second = store_blob(&format!("second-{}", redirect_status));
                let bundle =
                    bundle_utils::build_partial_bundle(&[first.clone(), second.clone()]).unwrap();
                (first, second, bundle)
            };

            // The "client side": a fresh, empty store, and a remote that redirects the batch
            // POST to a same-origin GET serving that bundle.
            let _client_scope = Scratch::new(&format!("offload-batch-client-{}", redirect_status));
            let remote = FakeOffloadingRemote::start(redirect_status, bundle, HashMap::new());
            let client = RemoteClient::new(&remote.url, Some("shhh".to_string())).unwrap();
            let hashes = vec![first.clone(), second.clone()];

            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let fetched = runtime.block_on(fetch_missing_objects(&client, &hashes))
                .unwrap_or_else(|e| panic!("status {}: the redirect must be followed, not \
                    replayed as a POST: {}", redirect_status, e));

            assert_eq!(fetched, 2, "status {}", redirect_status);
            assert!(file_utils::does_object_exist(&first).unwrap(), "status {}", redirect_status);
            assert!(file_utils::does_object_exist(&second).unwrap(), "status {}", redirect_status);

            assert_eq!(
                remote.batch_hits(), 1,
                "exactly one batch round trip, status {}", redirect_status
            );
            assert!(
                remote.any_had_auth("/v1/objects/batch"),
                "the batch POST itself still carries this remote's bearer token, status {}",
                redirect_status
            );
            assert!(
                !remote.any_had_auth("/responses/bundle"),
                "the presigned-URL follow-up must not carry this remote's bearer token, status {}",
                redirect_status
            );
        }
    }

    /// A redirect that carries no usable `Location` header (a malformed or hostile head) is an
    /// honest error, not a panic and not a silently empty result.
    #[test]
    fn fetch_batch_errors_honestly_on_a_locationless_redirect() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());

        std::thread::spawn(move || {
            use std::io::Write;

            if let Ok((mut stream, _)) = listener.accept() {
                let _ = read_test_request(&mut stream);
                let _ = write!(
                    stream,
                    "HTTP/1.1 303 See Other\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.flush();
            }
        });

        let client = RemoteClient::new(&url, None).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let error = runtime.block_on(client.fetch_batch(&["a".repeat(64)])).unwrap_err();

        assert!(
            error.to_lowercase().contains("location"),
            "the error should name the missing Location header: {}", error
        );
    }

    /// The designed transport for chunks (DESIGN.html §9.4b: "franchise, lower and expand fetch
    /// chunks per-object after the bundle wave") — `fetch_recipe_chunks` must never route
    /// through `POST /v1/objects/batch`, no matter how many chunks are missing at once, unlike
    /// the general `fetch_missing_objects` path it deliberately does not delegate to for this.
    #[test]
    fn fetch_recipe_chunks_never_touches_the_batch_endpoint() {
        let _scratch = Scratch::new("recipe-chunks-loose-only");

        // Two chunks, built but never stored — "missing locally", the precondition
        // `fetch_recipe_chunks` expects for the chunks it fetches.
        let chunk_a = LooseObjectBuilder::build_chunk(&crate::model::chunk::Chunk {
            content: b"chunk a bytes".to_vec(),
        });
        let chunk_b = LooseObjectBuilder::build_chunk(&crate::model::chunk::Chunk {
            content: b"chunk b bytes, a bit longer".to_vec(),
        });

        // The recipe itself must already be present locally (it rides the ordinary blob wave in
        // production; `fetch_recipe_chunks` only ever runs after that has landed).
        let recipe_hash = {
            use crate::model::recipe::{Recipe, RecipeChunk};

            let recipe = Recipe {
                content_hash: "f".repeat(64),
                total_size: (chunk_a.content.len() + chunk_b.content.len()) as u64,
                chunks: vec![
                    RecipeChunk { hash: chunk_a.hash.clone(), size: chunk_a.content.len() as u64 },
                    RecipeChunk { hash: chunk_b.hash.clone(), size: chunk_b.content.len() as u64 },
                ],
            };
            let mut object = LooseObjectBuilder::build_recipe(&recipe);
            object.store().unwrap();
            object.hash
        };

        assert!(!file_utils::does_object_exist(&chunk_a.hash).unwrap(), "chunk a starts missing");
        assert!(!file_utils::does_object_exist(&chunk_b.hash).unwrap(), "chunk b starts missing");

        let mut objects = HashMap::new();
        objects.insert(chunk_a.hash.clone(), chunk_a.content.clone());
        objects.insert(chunk_b.hash.clone(), chunk_b.content.clone());

        // The batch endpoint is wired (so a regression fails loudly instead of hanging), but
        // must never be hit.
        let remote = FakeOffloadingRemote::start(303, Vec::new(), objects);
        let client = RemoteClient::new(&remote.url, None).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let fetched = runtime.block_on(fetch_recipe_chunks(&client, &[recipe_hash]))
            .expect("both chunks fetch loose");

        assert_eq!(fetched, 2);
        assert!(file_utils::does_object_exist(&chunk_a.hash).unwrap());
        assert!(file_utils::does_object_exist(&chunk_b.hash).unwrap());

        assert_eq!(remote.batch_hits(), 0, "chunks never route through the batch endpoint");
        assert_eq!(remote.hits_for(&format!("/v1/objects/{}", chunk_a.hash)), 1);
        assert_eq!(remote.hits_for(&format!("/v1/objects/{}", chunk_b.hash)), 1);
    }

    /// FORK-45 finding #3: `fetch_corrupt_replacements` deliberately drops every presence check —
    /// that's by design, so a flagged-corrupt hash always gets force-fetched even though the old
    /// `does_object_exist` predicate would have called it "present". But `does_object_exist` was
    /// really two checks bundled into one (packs, then a gated loose stat) — dropping the predicate
    /// dropped both, and the pack half never had anything to do with the corrupt-loose problem this
    /// function exists to solve. Pins: a hash already good in a *pack* is left alone (no network
    /// hit, no redundant loose duplicate written), while a hash whose *loose* copy is corrupt is
    /// still force-fetched and repaired — in the same call, so the pack-only guard can't be
    /// satisfied by accidentally skipping every hash.
    #[test]
    fn fetch_corrupt_replacements_skips_a_pack_resident_hash_but_still_repairs_a_corrupt_loose_one() {
        let _scratch = Scratch::new("corrupt-replacements-pack-guard");

        // A hash already packed, correctly, before this call ever runs — the exact shape `compact`
        // leaves behind: packed, and no longer loose at all. Written as a raw content-addressed
        // object directly (mirroring `pack_utils`'s own `store_loose` test fixture) rather than via
        // `LooseObjectBuilder`, so the hash is exactly `blake3(pack_resident_content)` and the fake
        // remote below can serve those same raw bytes back under that hash.
        let pack_resident_content = b"already packed and perfectly fine".to_vec();
        let pack_resident_hash = blake3::hash(&pack_resident_content).to_hex().to_string();
        let (resident_folder, resident_file_name) =
            file_utils::get_path_for_object(&pack_resident_hash).unwrap();
        file_utils::write_object_to_file(
            std::path::Path::new(&resident_folder), &resident_file_name,
            zstd::encode_all(pack_resident_content.as_slice(), 0).unwrap()
        ).unwrap();
        let stats = pack_utils::compact(false, false).unwrap();
        assert_eq!(stats.objects_packed, 1, "the fixture object must actually land in a pack");
        assert!(pack_utils::is_in_packs(&pack_resident_hash).unwrap(), "fixture precondition: packed");
        let (pack_folder, pack_file_name) = file_utils::get_path_for_object(&pack_resident_hash).unwrap();
        let pack_resident_loose_path = std::path::Path::new(&pack_folder).join(&pack_file_name);
        assert!(!pack_resident_loose_path.exists(), "fixture precondition: no loose copy survives the compact");

        // A hash whose loose dentry is corrupt (content does not hash to its own filename) — the
        // shape `hash_mismatch` classifies as a D4 corrupt candidate upstream.
        let genuine_content = b"the real bytes this hash belongs to".to_vec();
        let corrupt_hash = blake3::hash(&genuine_content).to_hex().to_string();
        let (corrupt_folder, corrupt_file_name) = file_utils::get_path_for_object(&corrupt_hash).unwrap();
        let wrong_bytes = zstd::encode_all(b"not the genuine bytes at all".as_slice(), 0).unwrap();
        file_utils::write_object_to_file(
            std::path::Path::new(&corrupt_folder), &corrupt_file_name, wrong_bytes
        ).unwrap();

        // The remote serves genuine, correct bytes for *both* hashes — including the pack-resident
        // one, so a revert that re-fetches it would find a "successful", non-erroring transfer
        // (the failure mode the finding describes is wasted transfer + a redundant object, not a
        // broken one).
        let mut objects = HashMap::new();
        objects.insert(pack_resident_hash.clone(), pack_resident_content.clone());
        objects.insert(corrupt_hash.clone(), genuine_content.clone());
        let remote = FakeOffloadingRemote::start(303, Vec::new(), objects);
        let client = RemoteClient::new(&remote.url, None).unwrap();

        let flagged = vec![pack_resident_hash.clone(), corrupt_hash.clone()];
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (recovered, failures) = runtime.block_on(fetch_corrupt_replacements(&client, &flagged));

        assert_eq!(failures, BTreeMap::new(), "neither hash should fail: {:?}", failures);
        assert_eq!(recovered, 2, "both hashes must be accounted for as recovered");

        // The pack-resident hash was never re-fetched...
        assert_eq!(
            remote.hits_for(&format!("/v1/objects/{}", pack_resident_hash)), 0,
            "a pack-resident hash must not be re-fetched from the remote"
        );
        // ...and no redundant loose duplicate was written for it.
        assert!(
            !pack_resident_loose_path.exists(),
            "a pack-resident hash must not gain a redundant loose duplicate"
        );
        assert!(pack_utils::is_in_packs(&pack_resident_hash).unwrap(), "still packed, untouched");

        // The corrupt-loose hash, by contrast, was force-fetched and repaired.
        assert_eq!(
            remote.hits_for(&format!("/v1/objects/{}", corrupt_hash)), 1,
            "a flagged-corrupt hash must still be force-fetched exactly once"
        );
        assert_eq!(
            file_utils::retrieve_object_by_hash(&corrupt_hash).unwrap(), genuine_content,
            "the corrupt loose copy must be superseded by the genuine bytes"
        );
    }

    // -----------------------------------------------------------------------------------
    // `join_all` / `drain_remaining`: a transfer task's body must not still be running once the
    // coordinating call has returned an error, and a sibling's panic during the drain window must
    // leave evidence rather than vanish. `join_all` is the single implementation every
    // coordinator in this module routes through — fetching objects, fetching signature sidecars,
    // and both upload paths — so the tests below, exercising `join_all`/`drain_remaining`
    // directly, structurally cover all of them, including the two real task bodies that motivate
    // the fix: `object_utils::store_object_bytes` and `sign_utils::store_raw_parcel_signature`
    // (see `join_all`'s doc for the exact no-await shape they share that motivates this). The
    // tests reproduce that shape with `std::thread::sleep` — never `tokio::time::sleep`, which
    // would give cancellation somewhere to land — gated by atomic flags so a sibling is provably
    // resident, and provably past whatever it is waiting on, before the task driving each
    // scenario proceeds. Needs a real second worker thread to mean anything, unlike this file's
    // other tests: a `current_thread` runtime could never run the sleeping task and a spin-wait
    // concurrently.
    // -----------------------------------------------------------------------------------

    /// Multi-thread runtime shared by the drain/join tests below — a real second worker thread is
    /// what lets an unyielding sibling and whatever gates on it actually run concurrently; see the
    /// section note above for why a `current_thread` runtime could never exercise this.
    fn build_drain_test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap()
    }

    /// Bounded *blocking* spin-wait shared by the tests below for a condition set by another
    /// concurrently-resident task's body — e.g. "has the sibling started" or "has the fast task
    /// already returned its error". Deliberately synchronous (`std::thread::sleep`, never
    /// `tokio::time::sleep`), mirroring `model::task`'s test helper of the same name and
    /// rationale: an `.await` here would itself be an await point, and used inside the
    /// "unyielding sibling" below, that would reopen exactly the cancellation window these tests
    /// exist to rule out (`abort_all` can only land at an await point — a blocking poll has none).
    /// Polls `cond` every 5ms for up to 10s so a real regression times out the test instead of
    /// hanging CI; on timeout, sets `starved` rather than asserting directly, so a degenerate
    /// schedule fails loudly with an environment-attributed message (asserted by the caller after
    /// the fact) instead of one that would wrongly implicate the code under test.
    fn spin_until(cond: impl Fn() -> bool, starved: &std::sync::atomic::AtomicBool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);

        while !cond() {
            if std::time::Instant::now() >= deadline {
                starved.store(true, std::sync::atomic::Ordering::SeqCst);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Spawns into `tasks` the "unyielding sibling" shape shared by every test below — mirroring
    /// the two real task bodies `join_all`/`drain_remaining` guard
    /// (`object_utils::store_object_bytes` and `sign_utils::store_raw_parcel_signature`; see
    /// `join_all`'s doc for the exact no-await shape being mirrored here). Sets `started` as soon
    /// as it is first polled — these bare spawned bodies have no other await point, so
    /// `abort_all` (which can only cancel a task suspended at one) cancels a never-polled task
    /// outright; `started` is what proves this sibling's first, and only, poll has begun, i.e. it
    /// is already inside its uninterruptible synchronous body, before whatever follows happens.
    /// If `gate` is given, blocking-waits (`spin_until`, never an `.await`) for that flag next —
    /// ordering this sibling's unyielding stretch after whatever the caller gates on without
    /// opening a new await point for `abort_all` to land in — then blocks its worker thread for
    /// `unyielding_duration` with `std::thread::sleep`, sets `completed`, and finally hands off to
    /// `finish` to produce the task's result (an ordinary value, an error, or a panic). From
    /// `started` to `finish`, this body never awaits: the gate and the sleep are both synchronous,
    /// so the whole thing is one uninterruptible run once it starts.
    fn spawn_unyielding_sibling<T: Send + 'static>(
        tasks: &mut JoinSet<T>,
        started: Arc<std::sync::atomic::AtomicBool>,
        completed: Arc<std::sync::atomic::AtomicBool>,
        gate: Option<(Arc<std::sync::atomic::AtomicBool>, Arc<std::sync::atomic::AtomicBool>)>,
        unyielding_duration: std::time::Duration,
        finish: impl FnOnce() -> T + Send + 'static,
    ) {
        tasks.spawn(async move {
            started.store(true, std::sync::atomic::Ordering::SeqCst);

            if let Some((flag, starved)) = gate {
                spin_until(|| flag.load(std::sync::atomic::Ordering::SeqCst), &starved);
            }

            std::thread::sleep(unyielding_duration);
            completed.store(true, std::sync::atomic::Ordering::SeqCst);
            finish()
        });
    }

    /// Spawns into `tasks` the "fast" sibling shared by the error-path tests below: blocking-waits
    /// (`spin_until`) for `gate` (the unyielding sibling's start), then immediately stores
    /// `signal` (if given) right before producing `finish` — so a later wait on `signal` orders
    /// itself after this task's own completion, not merely after its start.
    fn spawn_fast_sibling<T: Send + 'static>(
        tasks: &mut JoinSet<T>,
        gate: (Arc<std::sync::atomic::AtomicBool>, Arc<std::sync::atomic::AtomicBool>),
        signal: Option<Arc<std::sync::atomic::AtomicBool>>,
        finish: impl FnOnce() -> T + Send + 'static,
    ) {
        let (flag, starved) = gate;

        tasks.spawn(async move {
            spin_until(|| flag.load(std::sync::atomic::Ordering::SeqCst), &starved);

            if let Some(signal) = signal {
                signal.store(true, std::sync::atomic::Ordering::SeqCst);
            }

            finish()
        });
    }

    /// The extracted drain in isolation: `drain_remaining` must not return while a sibling is
    /// still inside synchronous, non-yielding work. `abort_all` only *signals* cancellation, and
    /// tokio can only land that signal at an await point — a task with none between where it is
    /// and the end of its body keeps running regardless (see `drain_remaining`'s doc comment).
    #[test]
    fn drain_remaining_waits_out_a_sibling_stuck_in_synchronous_work() {
        let runtime = build_drain_test_runtime();

        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let starved = Arc::new(std::sync::atomic::AtomicBool::new(false));

        runtime.block_on(async {
            let mut tasks: JoinSet<()> = JoinSet::new();
            spawn_unyielding_sibling(
                &mut tasks,
                Arc::clone(&started),
                Arc::clone(&completed),
                None,
                std::time::Duration::from_millis(300),
                || (),
            );

            // Waits until the sibling is confirmed started — i.e. its one and only poll has
            // begun, putting it inside the unyielding sleep, the only place a cancellation signal
            // could ever land for it — before draining.
            spin_until(|| started.load(std::sync::atomic::Ordering::SeqCst), &starved);

            drain_remaining(tasks).await;
        });

        assert!(
            !starved.load(std::sync::atomic::Ordering::SeqCst),
            "test environment starved the sibling (it never started within the deadline) — not a \
             drain bug"
        );
        assert!(
            completed.load(std::sync::atomic::Ordering::SeqCst),
            "drain_remaining must not return while a sibling's synchronous body may still be running"
        );
    }

    /// `join_all`'s actual error path: a fast failure must not cut off a sibling that is already
    /// past its network fetch and inside the synchronous write, and a second failure surfacing
    /// only during the drain must never override the first one already captured.
    ///
    /// The ordering is enforced up to a wide scheduling margin, not absolutely: `first_returned`
    /// is set by the fast sibling immediately *before* it returns its own error, so at flag-set
    /// time that error is not yet sitting in the `JoinSet` — the future only completes, landing
    /// the result, a hair later when that poll returns. The slow sibling only starts its 1s
    /// unyielding stretch *after* observing the flag, which puts the fast error's *readiness*
    /// well ahead of the slow one's — but `join_next`'s order among simultaneously-ready results
    /// is unspecified, and nothing observable from outside `join_all` marks the moment the fast
    /// error is actually *consumed* by the coordinator. Full determinism is therefore impossible
    /// to guarantee from here: a coordinator stalled longer than the 1s window could still see
    /// both results ready and pick either first. The margin makes that failure mode practically
    /// unreachable, not theoretically impossible. Full causal gating — blocking the slow
    /// sibling's unyielding stretch until the fast error is actually *consumed* inside
    /// `join_all`, not merely returned — was considered and declined: consumption is only
    /// observable via a test-only hook inside `join_all`/`drain_remaining`, and the global armed
    /// state and cross-test serialization such a hook would need outweigh the sub-observable race
    /// it would close.
    #[test]
    fn join_all_drains_a_synchronous_sibling_before_surfacing_the_first_error() {
        let runtime = build_drain_test_runtime();

        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let first_returned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let starved = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let result = runtime.block_on(async {
            let mut tasks: JoinSet<Result<(), String>> = JoinSet::new();

            spawn_unyielding_sibling(
                &mut tasks,
                Arc::clone(&started),
                Arc::clone(&completed),
                Some((Arc::clone(&first_returned), Arc::clone(&starved))),
                std::time::Duration::from_secs(1),
                // A late failure: proves the drain does not let a second error overwrite the
                // first one `join_all` already captured — first error wins.
                || Err("late boom".to_string()),
            );

            spawn_fast_sibling(
                &mut tasks,
                (Arc::clone(&started), Arc::clone(&starved)),
                Some(Arc::clone(&first_returned)),
                || Err("boom".to_string()),
            );

            join_all(tasks).await
        });

        assert!(
            !starved.load(std::sync::atomic::Ordering::SeqCst),
            "test environment starved a sibling (a wait never observed its condition within the \
             deadline) — not a join_all bug"
        );
        // The margin makes it merely unreachable, not impossible (see the doc comment above): if
        // `join_next` did surface the late sibling's error, that is either the coordinator
        // stalled past the 1s window (both results were ready, and join order among ready
        // results is unspecified — a scheduling artifact, not this test's concern) or a genuine
        // first-error-wins regression in `join_all` — the two are indistinguishable from a single
        // run. A one-off failure here is almost certainly the former; rerun to check. A failure
        // that reproduces persistently is the latter and should be treated as a real regression,
        // not dismissed as flake.
        if result == Err("late boom".to_string()) {
            panic!(
                "join_next surfaced the late sibling's error (\"late boom\") instead of the fast \
                 sibling's (\"boom\"); either the coordinator stalled past the late sibling's 1s \
                 window (environment — see this test's doc comment) or first-error-wins \
                 regressed in join_all (code) — rerun to tell them apart; a persistent failure is \
                 the code."
            );
        }
        assert_eq!(result, Err("boom".to_string()), "the first-observed failure wins, not the late one");
        assert!(
            completed.load(std::sync::atomic::Ordering::SeqCst),
            "join_all must not return while the sibling's synchronous body may still be running"
        );
    }

    /// The panic-evidence half of the drain contract (see `drain_remaining`'s doc): the drain must
    /// not silently discard a sibling's panic — it must be appended to the error `join_all`
    /// already captured, never substituted for it, so the panic leaves evidence instead of
    /// vanishing (this crate's core never prints on its own). Same ordering discipline, and the
    /// same scheduling-margin caveat, as the test above: `first_returned` gates the panicking
    /// sibling's unyielding stretch so its panic cannot even become *ready* until after the fast
    /// sibling's "boom" does — not a guarantee that "boom" has already been *consumed* by the
    /// coordinator by then.
    ///
    /// The panic below prints via the default panic hook as part of this test's output —
    /// expected, not a test-harness failure.
    #[test]
    fn join_all_appends_panic_evidence_from_the_drain_window_without_losing_the_first_error() {
        const PANIC_MESSAGE: &str = "sibling panic evidence for the drain-window test";

        let runtime = build_drain_test_runtime();

        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let first_returned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let starved = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let result = runtime.block_on(async {
            let mut tasks: JoinSet<Result<(), String>> = JoinSet::new();

            spawn_unyielding_sibling(
                &mut tasks,
                Arc::clone(&started),
                Arc::clone(&completed),
                Some((Arc::clone(&first_returned), Arc::clone(&starved))),
                std::time::Duration::from_secs(1),
                || panic!("{}", PANIC_MESSAGE),
            );

            spawn_fast_sibling(
                &mut tasks,
                (Arc::clone(&started), Arc::clone(&starved)),
                Some(Arc::clone(&first_returned)),
                || Err("boom".to_string()),
            );

            join_all(tasks).await
        });

        assert!(
            !starved.load(std::sync::atomic::Ordering::SeqCst),
            "test environment starved a sibling (a wait never observed its condition within the \
             deadline) — not a panic-evidence bug"
        );
        assert!(
            completed.load(std::sync::atomic::Ordering::SeqCst),
            "join_all must not return before the panicking sibling's synchronous body finishes \
             running"
        );

        let error = result.expect_err("join_all must surface the first error, not the sibling's panic");

        // Same reorder caveat as the "drains a synchronous sibling" test above (see its doc
        // comment for the full reasoning): if the panicking sibling's failure led instead of
        // "boom", that is either the coordinator stalled past the 1s window (environment) or
        // first-error-wins regressed (code) — indistinguishable from a single run, so rerun
        // before concluding which; a persistent failure is the code.
        if !error.starts_with("boom") {
            panic!(
                "join_next surfaced the panicking sibling's failure before the fast sibling's \
                 \"boom\" (got: {}); either the coordinator stalled past the panicking sibling's \
                 1s window (environment — see the doc comment on the sibling test above) or \
                 first-error-wins regressed in join_all (code) — rerun to tell them apart; a \
                 persistent failure is the code.",
                error
            );
        }
        assert!(
            error.starts_with("boom"),
            "the first-observed failure must lead the message, got: {}", error
        );
        assert!(
            error.contains(PANIC_MESSAGE),
            "the sibling's panic evidence must be appended to the first error, got: {}", error
        );
    }

    /// Pins [`join_all_independent`]'s own doc claim — a panicked sibling "is reported like any
    /// other per-hash failure, never silently dropped and never mistaken for a clean success" —
    /// which the tests above only exercise indirectly through [`join_all`]/[`drain_remaining`],
    /// never through this function itself. Deterministic and hook-free: unlike `join_all`'s own
    /// panic-evidence tests, nothing here is ever aborted or drained mid-flight, so there is no
    /// scheduling-margin window to race — every task runs to its own natural completion and is
    /// simply collected. Mutation: make the collecting loop `continue` on a `JoinError` instead of
    /// recording it (silently dropping the panicked sibling instead of turning it into its own
    /// `Err` slot) — this test goes red on both the outcome count and the "exactly one `Err`"
    /// assertion below.
    #[test]
    fn join_all_independent_reports_a_panicked_sibling_as_its_own_err_alongside_two_oks() {
        const PANIC_MESSAGE: &str = "sibling panic evidence for join_all_independent";

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

        let outcomes = runtime.block_on(async {
            let mut tasks: JoinSet<Result<u32, String>> = JoinSet::new();
            tasks.spawn(async { panic!("{}", PANIC_MESSAGE) });
            tasks.spawn(async { Ok(1) });
            tasks.spawn(async { Ok(2) });

            join_all_independent(tasks).await
        });

        assert_eq!(outcomes.len(), 3, "every task's own outcome must come back, panicked or not");

        let errors: Vec<&String> = outcomes.iter().filter_map(|o| o.as_ref().err()).collect();
        assert_eq!(errors.len(), 1, "exactly one outcome must be the panicked sibling's own Err: {:?}", outcomes);
        assert!(
            errors[0].contains(PANIC_MESSAGE),
            "the panicked sibling's Err must carry its own panic payload, got: {}", errors[0]
        );

        let mut oks: Vec<u32> = outcomes.into_iter().filter_map(|o| o.ok()).collect();
        oks.sort();
        assert_eq!(oks, vec![1, 2], "both ordinary siblings must still come back as clean successes");
    }

    // -----------------------------------------------------------------------------------
    // FORK-49: a remote that completes the TCP connect and then never finishes its response
    // must fail with a timeout, not hang the caller forever (previously: neither `reqwest`
    // client this module builds set any timeout at all). The fixtures below **park** their
    // handler thread on a channel receive that is only ever unblocked by the fixture being
    // dropped at the end of the test — never by returning early, which would drop the
    // `TcpStream`, send a FIN, and let the client fail instantly on a connection-closed error
    // that has nothing to do with any timeout; a test built that way would pass even against an
    // unfixed client. Each "must time out" assertion wraps the call in an outer
    // `tokio::time::timeout` (a hard ceiling well past the timeout under test) so a future
    // regression that drops the fix fails the suite loudly instead of wedging CI forever, and
    // checks the failure is specifically a *timeout* — not merely "some error" — since a
    // connection-closed error and a timeout are otherwise easy to conflate. Each "must stay
    // unbounded" assertion instead races the call against a timer and requires it to *still be
    // running* once the timer fires — the mirror-image failure mode, where a silent remote must
    // never be enough on its own to fail a call the settled contract says may only be abandoned.
    //
    // `TEST_DIRECT_CONNECT_TIMEOUT`/`TEST_TIGHT_READ_TIMEOUT`/`TEST_LOOSE_READ_TIMEOUT` mirror the
    // production `REMOTE_CONNECT_TIMEOUT`/`REMOTE_READ_TIMEOUT`/`FETCH_OBJECT_READ_TIMEOUT` as
    // their own constants (rather than referencing the production ones directly) purely so this
    // module keeps compiling against a tree with those constants renamed or removed — every test
    // below was at some point run against the pre-fix tree to confirm it was red for the right
    // reason before the corresponding fix landed.
    // -----------------------------------------------------------------------------------

    const TEST_DIRECT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    const TEST_TOR_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
    const TEST_TIGHT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    const TEST_LOOSE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
    /// Mirrors production `UPLOAD_SILENCE_BUDGET` — see `TEST_TIGHT_READ_TIMEOUT`'s doc for why
    /// this is its own constant rather than a reference to the production one.
    const TEST_UPLOAD_SILENCE_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

    /// `classify` is pure and total over the two booleans `reqwest::Error` exposes — this pins
    /// all four combinations directly, including the one no live socket can construct: a genuine
    /// kernel `ETIMEDOUT` on an already-established connection (`is_timeout() == true`,
    /// `is_connect() == false`) is indistinguishable, from these two booleans alone, from this
    /// client's own configured `read_timeout` firing — which is exactly why `ReadTimedOut` is
    /// deliberately the same variant either way, and why `describe_transport_error` must not
    /// name a specific bound in that case.
    #[test]
    fn classify_covers_all_four_boolean_combinations() {
        assert_eq!(
            classify(true, true), TransportFailure::ConnectTimedOut,
            "connect timeout: is_connect() and is_timeout() both true"
        );
        assert_eq!(
            classify(false, true), TransportFailure::ReadTimedOut,
            "read/silence timeout (or a kernel ETIMEDOUT on an established socket): only \
            is_timeout() true"
        );
        assert_eq!(
            classify(true, false), TransportFailure::Other,
            "a refused connect, DNS failure, or TLS failure: is_connect() true but not a \
            timeout — every hyper-util connector error is tagged Connect regardless of cause, \
            so is_connect() alone must never be read as \"timed out\""
        );
        assert_eq!(
            classify(false, false), TransportFailure::Other,
            "neither flag set: some other transport failure entirely"
        );
    }

    /// A refused connection is `is_connect() == true` but never `is_timeout() ==
    /// true` — checking `is_connect()` alone (as an earlier version of `describe_transport_error`
    /// did) wrongly reports it as "could not connect ... within {connect_timeout}", discarding
    /// the real cause and claiming a timeout that never happened. A listener bound then
    /// immediately dropped leaves the OS to refuse any connection to that port near-instantly
    /// (`ECONNREFUSED`), the cheapest real (non-fixture) way to exercise this — no fake error
    /// construction, no fixture standing in for the OS.
    #[test]
    fn fetch_info_against_a_refused_connection_does_not_claim_a_timeout() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // now genuinely closed: nothing is listening on `addr` any more

        let client = RemoteClient::new(&format!("http://{}", addr), None).unwrap();
        // A refused connection must fail near-instantly; this ceiling exists only to turn a
        // regression into a loud failure instead of a hang, not because this is expected to run
        // anywhere near it.
        let hard_ceiling = std::time::Duration::from_secs(10);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.fetch_info()).await
        });

        let inner = outcome.unwrap_or_else(|_| panic!(
            "fetch_info hung past the test's own {:?} ceiling — a refused connection must fail \
            promptly, never hang", hard_ceiling
        ));
        let message = match inner {
            Err(message) => message,
            Ok(_) => panic!("a refused connection must not appear to succeed"),
        };

        assert!(
            !message.to_lowercase().contains("timed out"),
            "a refused connection never timed out, and must not be reported as if it did: {}",
            message
        );
        assert!(
            message.to_lowercase().contains("refused") || message.to_lowercase().contains("connect"),
            "must name something useful about the actual cause, not a generic wrapper message: {}",
            message
        );
    }

    /// A remote that accepts the connection, reads the request in full, and then genuinely goes
    /// silent: it never writes a byte of response. See the section note above for the parking
    /// pattern and why a returning handler would be a false pass. Shared by every "must time out"
    /// and "must stay unbounded" test below — the same fixture proves both properties, on
    /// whichever call each test points it at.
    struct SilentRemote {
        url: String,
        /// Owns the sender; dropping it — when this value goes out of scope at the end of the
        /// test — is what finally unblocks the parked handler thread's `recv`, letting it close
        /// the connection. Never signaled mid-test, so the handler is genuinely parked, not
        /// polling, for the whole test body.
        _park: std::sync::mpsc::Sender<()>,
    }

    impl SilentRemote {
        fn start() -> SilentRemote {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let (tx, rx) = std::sync::mpsc::channel::<()>();

            std::thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    let _ = read_test_request(&mut stream);
                    let _ = rx.recv();
                    drop(stream);
                }
            });

            SilentRemote { url, _park: tx }
        }
    }

    /// Race `future` against a `check_after` timer and require it to **still be running** once
    /// the timer fires — the "unbounded direction" pin: a silent remote must never be enough, on
    /// its own, to fail a call the settled contract says may only ever be abandoned, not failed.
    /// Panics (naming whatever the future resolved to) if it finishes first; returns normally
    /// (the passing case) if the timer wins.
    async fn assert_still_running<F, T>(label: &str, check_after: std::time::Duration, future: F)
    where
        F: std::future::Future<Output = T>,
    {
        tokio::select! {
            _ = future => panic!(
                "{} already finished after only {:?} — a silent remote must never be enough on \
                its own to fail a call this contract says may only be abandoned.",
                label, check_after
            ),
            _ = tokio::time::sleep(check_after) => {
                // Still running, as required — the passing case.
            }
        }
    }

    /// `fetch_info` against a remote that connects and then never writes anything must fail with
    /// a timeout — not hang forever, and not fail for some unrelated reason.
    #[test]
    fn fetch_info_times_out_against_a_silent_remote() {
        let remote = SilentRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let effective_budget = TEST_DIRECT_CONNECT_TIMEOUT + TEST_TIGHT_READ_TIMEOUT;
        let hard_ceiling = effective_budget + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.fetch_info()).await
        });

        let inner = outcome.unwrap_or_else(|_| panic!(
            "fetch_info hung past the test's own {:?} ceiling — no timeout fired at all",
            hard_ceiling
        ));
        let message = match inner {
            Err(message) => message,
            Ok(_) => panic!("a silent remote must not appear to succeed"),
        };

        assert!(
            message.to_lowercase().contains("timed out"),
            "must fail specifically with a timeout, not some other transport error: {}", message
        );
        // The message must name the *effective* bound (connect + silence) that actually
        // governs, not the raw silence budget alone.
        assert!(
            message.contains(&format!("{:?}", effective_budget)),
            "must name the effective bound {:?}, not some other figure: {}", effective_budget, message
        );
    }

    /// `fetch_signature` must be bounded exactly like `fetch_info` — pins that it stays wired to
    /// `bounded_reads` and never silently drifts back to the unbounded client (which would
    /// restore the hang).
    #[test]
    fn fetch_signature_times_out_against_a_silent_remote() {
        let remote = SilentRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let hard_ceiling = TEST_DIRECT_CONNECT_TIMEOUT + TEST_TIGHT_READ_TIMEOUT
            + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.fetch_signature(&"a".repeat(64))).await
        });

        let message = outcome
            .unwrap_or_else(|_| panic!(
                "fetch_signature hung past the test's own {:?} ceiling — no timeout fired at all",
                hard_ceiling
            ))
            .expect_err("a silent remote must not appear to succeed");

        assert!(
            message.to_lowercase().contains("timed out"),
            "must fail specifically with a timeout, not some other transport error: {}", message
        );
    }

    /// `fetch_bundle_to` must be bounded exactly like `fetch_info` — same pin as
    /// `fetch_signature`'s, for the same reason.
    #[test]
    fn fetch_bundle_to_times_out_against_a_silent_remote() {
        let remote = SilentRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let dest = std::env::temp_dir().join(format!(
            "forklift-fetch-bundle-to-silent-{}-{}", std::process::id(), line!()
        ));
        let hard_ceiling = TEST_DIRECT_CONNECT_TIMEOUT + TEST_TIGHT_READ_TIMEOUT
            + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.fetch_bundle_to(&dest)).await
        });

        let message = outcome
            .unwrap_or_else(|_| panic!(
                "fetch_bundle_to hung past the test's own {:?} ceiling — no timeout fired at all",
                hard_ceiling
            ))
            .expect_err("a silent remote must not appear to succeed");

        assert!(
            message.to_lowercase().contains("timed out"),
            "must fail specifically with a timeout, not some other transport error: {}", message
        );
        // A silent remote never gets past headers, so no temp file is ever created here — the
        // dedicated test below is what actually exercises the mid-download cleanup.
        assert!(!dest.exists(), "no file should appear at the destination at all in this scenario");

        let _ = std::fs::remove_file(&dest);
    }

    /// A mid-download failure — the new timeout being the routine case now — must never leave a
    /// truncated file at the destination, which would otherwise be this warehouse's
    /// own latest bundle. Unlike the silent-remote scenario above, [`LyingContentLengthRemote`]
    /// gets past headers and writes a few real bytes before going quiet, so the temp file this
    /// exercises genuinely exists (and has content) at the moment the timeout fires and the
    /// cleanup runs.
    #[test]
    fn fetch_bundle_to_leaves_no_truncated_file_on_a_mid_download_timeout() {
        let remote = LyingContentLengthRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let dest = std::env::temp_dir().join(format!(
            "forklift-fetch-bundle-to-truncated-{}-{}", std::process::id(), line!()
        ));
        let hard_ceiling = TEST_DIRECT_CONNECT_TIMEOUT + TEST_TIGHT_READ_TIMEOUT
            + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.fetch_bundle_to(&dest)).await
        });

        let message = outcome
            .unwrap_or_else(|_| panic!(
                "fetch_bundle_to hung past the test's own {:?} ceiling — no timeout fired at all",
                hard_ceiling
            ))
            .expect_err("a body that never completes must not appear to succeed");

        assert!(
            message.to_lowercase().contains("timed out"),
            "must fail specifically with a timeout, not some other transport error: {}", message
        );
        assert!(
            !dest.exists(),
            "a mid-download timeout must never leave a truncated file at the destination"
        );

        // The cleanup must be pinned, not just its visible effect — a test that only
        // checks `!dest.exists()` would still pass if the temp-file `remove_file` call were
        // simply deleted, since the temp file is never renamed to `dest` in this scenario either
        // way. Scoped to this test's own temp-file prefix (not a bare ".tmp" scan) since
        // `std::env::temp_dir()` is shared with whatever else is running concurrently.
        let dest_name = dest.file_name().unwrap().to_string_lossy().into_owned();
        let temp_prefix = format!("{}.tmp", dest_name);
        let leftover_temp_files: Vec<String> = std::fs::read_dir(dest.parent().unwrap())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(&temp_prefix))
            .collect();
        assert!(
            leftover_temp_files.is_empty(),
            "the temp file must be cleaned up on a mid-download failure, not just absent from \
            the final destination: {:?}", leftover_temp_files
        );

        let _ = std::fs::remove_file(&dest);
    }

    /// `missing_objects`'s server work scales with the batch, so the contract forbids bounding it
    /// at all (see its own doc comment). Pins the "unbounded direction": a silent
    /// remote alone must never make this call fail within a window comfortably past the tight
    /// bounded budget — if it did, either this call or `request()`'s own default client had been
    /// silently rewired onto a bounded one.
    #[test]
    fn missing_objects_is_not_flat_bounded_by_silence() {
        let remote = SilentRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let check_after = TEST_DIRECT_CONNECT_TIMEOUT + TEST_TIGHT_READ_TIMEOUT
            + std::time::Duration::from_secs(5);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(assert_still_running(
            "missing_objects", check_after, client.missing_objects(&["a".repeat(64)]),
        ));
    }

    /// `fetch_batch`'s server work is size-dependent (see its own doc comment) — same "unbounded
    /// direction" pin as `missing_objects`'s.
    #[test]
    fn fetch_batch_is_not_flat_bounded_by_silence() {
        let remote = SilentRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let check_after = TEST_DIRECT_CONNECT_TIMEOUT + TEST_TIGHT_READ_TIMEOUT
            + std::time::Duration::from_secs(5);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(assert_still_running(
            "fetch_batch", check_after, client.fetch_batch(&["a".repeat(64)]),
        ));
    }

    /// `fetch_subtree`'s server work is size-dependent (see its own doc comment) — same
    /// "unbounded direction" pin as `missing_objects`'s.
    #[test]
    fn fetch_subtree_is_not_flat_bounded_by_silence() {
        let remote = SilentRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let check_after = TEST_DIRECT_CONNECT_TIMEOUT + TEST_TIGHT_READ_TIMEOUT
            + std::time::Duration::from_secs(5);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(assert_still_running(
            "fetch_subtree", check_after, client.fetch_subtree(&"p".repeat(64), "src"),
        ));
    }

    /// A remote that accepts the connection, reads the request, sends response headers claiming
    /// a `Content-Length` far larger than the body it actually writes, and then genuinely goes
    /// silent — the connection stays open (no FIN), so the client is left waiting for bytes that
    /// are never coming, not told the body ended early. Parks on the same channel-recv pattern as
    /// [`SilentRemote`], for the same reason.
    struct LyingContentLengthRemote {
        url: String,
        _park: std::sync::mpsc::Sender<()>,
    }

    impl LyingContentLengthRemote {
        fn start() -> LyingContentLengthRemote {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let (tx, rx) = std::sync::mpsc::channel::<()>();

            std::thread::spawn(move || {
                use std::io::Write;

                if let Ok((mut stream, _)) = listener.accept() {
                    let _ = read_test_request(&mut stream);
                    // Claims 1000 bytes, writes 4, then parks — the rest never arrives and the
                    // connection is never closed.
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                         Content-Length: 1000\r\nConnection: keep-alive\r\n\r\ntiny"
                    );
                    let _ = stream.flush();
                    let _ = rx.recv();
                    drop(stream);
                }
            });

            LyingContentLengthRemote { url, _park: tx }
        }
    }

    /// `fetch_object` against a remote whose `Content-Length` outlives the bytes it actually
    /// sends must fail with a timeout, not hang waiting for a body that is never coming. Ceiling
    /// sized to `TEST_LOOSE_READ_TIMEOUT`: `fetch_object` rides `bounded_object_reads`, not the
    /// tight `bounded_reads` the other three calls use.
    #[test]
    fn fetch_object_times_out_against_a_content_length_lie() {
        let remote = LyingContentLengthRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let hash = "a".repeat(64);
        let hard_ceiling = TEST_DIRECT_CONNECT_TIMEOUT + TEST_LOOSE_READ_TIMEOUT
            + std::time::Duration::from_secs(20);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.fetch_object(&hash)).await
        });

        let message = outcome
            .unwrap_or_else(|_| panic!(
                "fetch_object hung past the test's own {:?} ceiling — no timeout fired at all",
                hard_ceiling
            ))
            .expect_err("a body that never completes must not appear to succeed");

        assert!(
            message.to_lowercase().contains("timed out"),
            "must fail specifically with a timeout, not some other transport error: {}", message
        );
    }

    // -----------------------------------------------------------------------------------
    // The unbounded error-body read: `error_of` calls `response.json()` with no bound of its own
    // once a non-success status line has arrived. `self.http` (which `missing_objects` rides)
    // carries only a `connect_timeout`, no `read_timeout` at all — so a remote that delivers a
    // full status line and headers and then wedges before writing the body hangs the caller
    // forever. `ERROR_BODY_READ_TIMEOUT` bounds the read itself, in `error_of`, rather than
    // trying to fix it with a fifth client — a client cannot help here, since the body-read
    // bound is inherited from whichever client sent the request, and `error_of` serves responses
    // from all of them. `commit_lift`'s own error-body read is deliberately left unbounded (a
    // pre-existing, ticketed defect — FORK-49 is scoped to reads whose result only shapes a
    // message, not this call's retry-vs-terminal control flow).
    // -----------------------------------------------------------------------------------

    /// A remote that delivers a non-success status line and full headers (claiming a body),
    /// then genuinely goes silent before writing a single byte of that body — the connection
    /// stays open (no FIN), so anything reading the body is left waiting on bytes that never
    /// arrive. Distinct from [`SilentRemote`] (silent *before* the status line even arrives) and
    /// [`LyingContentLengthRemote`] (a `2xx` success path): this is specifically the shape
    /// [`ERROR_BODY_READ_TIMEOUT`] exists for — status and headers fully delivered, only the
    /// error body itself left hanging.
    struct SilentErrorBodyRemote {
        url: String,
        _park: std::sync::mpsc::Sender<()>,
    }

    impl SilentErrorBodyRemote {
        fn start() -> SilentErrorBodyRemote {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let (tx, rx) = std::sync::mpsc::channel::<()>();

            std::thread::spawn(move || {
                use std::io::Write;

                if let Ok((mut stream, _)) = listener.accept() {
                    let _ = read_test_request(&mut stream);
                    let _ = write!(
                        stream,
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\n\
                         Content-Length: 4096\r\n\r\n"
                    );
                    let _ = stream.flush();
                    let _ = rx.recv();
                    drop(stream);
                }
            });

            SilentErrorBodyRemote { url, _park: tx }
        }
    }

    /// `missing_objects` rides `self.http`, unbounded (only a `connect_timeout`) — so before this
    /// fix, a `500` whose body then wedges hangs this call forever, with the status line and
    /// headers already fully delivered. The outer ceiling is a safety net, not the property under
    /// test: if the wrapper in `error_of` is missing, that outer ceiling is what trips (the test
    /// fails instead of hanging the suite), which is unambiguous evidence the internal bound is
    /// gone — status line and headers were fully delivered before the park, so the only thing
    /// left to hang on is the error-body read itself.
    ///
    /// Also asserts a **lower** bound on elapsed time (review round 6, finding 1): a red-then-green
    /// suite run alone does not prove `error_of` calls [`error_body_read_budget`] at all — nothing
    /// else in the suite requires that link, so a regression back to the bare
    /// [`ERROR_BODY_READ_TIMEOUT`] (dropping the folded-in [`REMOTE_CONNECT_TIMEOUT`]) still
    /// passes every other assertion here, which only ceiling-checks. `tokio::time::timeout` never
    /// fires early and this fixture never sends a body, so the read burns the *entire* budget —
    /// making the lower bound exact, not approximate. Together with
    /// `error_body_read_budget_folds_in_the_connect_timeout` (which pins the helper's own
    /// arithmetic), this closes the loop: one test pins call-site-uses-helper, the other pins
    /// helper-arithmetic.
    #[test]
    fn missing_objects_bounds_the_error_body_read_after_a_wedged_500() {
        let remote = SilentErrorBodyRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        // Written explicitly as the constant sum, not via `error_body_read_budget`, so this
        // assertion doesn't co-move with a corrupted helper — a helper bug would then move the
        // elapsed time it produces and this fixed lower bound in lockstep, pinning nothing.
        let lower_bound = REMOTE_CONNECT_TIMEOUT + ERROR_BODY_READ_TIMEOUT;
        let outer_ceiling = lower_bound + std::time::Duration::from_secs(10);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let started = std::time::Instant::now();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(outer_ceiling, client.missing_objects(&["a".repeat(64)])).await
        });
        let elapsed = started.elapsed();

        let error = outcome
            .unwrap_or_else(|_| panic!(
                "missing_objects hung past the test's own {:?} outer ceiling — the error-body \
                read is unbounded", outer_ceiling
            ))
            .expect_err("a 500 status must surface as an error, not succeed");

        assert!(
            elapsed >= lower_bound,
            "elapsed {:?} is under the {:?} the folded budget (REMOTE_CONNECT_TIMEOUT + \
            ERROR_BODY_READ_TIMEOUT) requires — error_of is no longer calling \
            error_body_read_budget, it's using the bare ERROR_BODY_READ_TIMEOUT (or less)",
            elapsed, lower_bound
        );

        assert!(
            error.contains("(500)"),
            "must name the actual status: {}", error
        );
        assert!(
            error.contains("Internal Server Error"),
            "on a timed-out/unparseable body, must fall back to the canonical reason — the same \
            fallback already used for a parse failure: {}", error
        );
    }

    /// Streams a body in `chunks`, sleeping `gap` before each one and flushing immediately after
    /// — a body that is always moving bytes, however slowly (never silent, per the settled
    /// contract quoted in `REMOTE_READ_TIMEOUT`'s doc), never a single stall long enough to trip
    /// a read/idle timeout, but with a *total* duration
    /// comfortably past one. Pins the finding that a per-request **total** deadline (the shape
    /// this fix's first attempt used) kills this kind of transfer even though nothing ever went
    /// silent — only a per-read/idle bound (`ClientBuilder::read_timeout`, which resets on every
    /// byte received) can tell a stalled transfer from a slow-but-progressing one apart.
    fn start_steady_drip_remote(chunks: Vec<Vec<u8>>, gap: std::time::Duration) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());

        std::thread::spawn(move || {
            use std::io::Write;

            if let Ok((mut stream, _)) = listener.accept() {
                let _ = read_test_request(&mut stream);
                let total_len: usize = chunks.iter().map(|c| c.len()).sum();
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    total_len
                );
                let _ = stream.flush();

                for chunk in chunks {
                    std::thread::sleep(gap);
                    let _ = stream.write_all(&chunk);
                    let _ = stream.flush();
                }
            }
        });

        url
    }

    /// `fetch_object` against a body that dribbles in slowly but steadily — every inter-chunk gap
    /// comfortably under
    /// `TEST_LOOSE_READ_TIMEOUT`, the total transfer comfortably past it — must **succeed**,
    /// never be treated as a stall. A total-deadline bound (rejected in favor of
    /// `read_timeout`-on-its-own-client, see `REMOTE_READ_TIMEOUT`'s doc) fails this test: it
    /// would kill the transfer partway through even though every single gap was healthy.
    #[test]
    fn fetch_object_survives_a_slow_but_steadily_progressing_body() {
        let gap = std::time::Duration::from_secs(20);
        let chunks: Vec<Vec<u8>> = (0..4).map(|i| format!("chunk-{}-of-the-drip", i).into_bytes()).collect();
        let expected: Vec<u8> = chunks.concat();
        // 4 gaps of 20s = 80s total, ~15s past the 65s effective loose budget (connect + read),
        // while no single gap comes anywhere near even the tight budget, let alone the loose one.
        let total_duration = gap * (chunks.len() as u32);
        let effective_loose_budget = TEST_DIRECT_CONNECT_TIMEOUT + TEST_LOOSE_READ_TIMEOUT;
        assert!(
            total_duration > effective_loose_budget,
            "the fixture must actually outlast the bound under test"
        );

        let url = start_steady_drip_remote(chunks, gap);
        let client = RemoteClient::new(&url, None).unwrap();
        let hash = "a".repeat(64);
        let outer_ceiling = total_duration + std::time::Duration::from_secs(20);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(outer_ceiling, client.fetch_object(&hash)).await
        });

        let bytes = outcome
            .unwrap_or_else(|_| panic!(
                "fetch_object hung past its own generous outer ceiling {:?}", outer_ceiling
            ))
            .unwrap_or_else(|e| panic!(
                "a slow-but-steady body must succeed — it was never silent, so it must never be \
                treated as a stall: {}", e
            ));

        assert_eq!(bytes, expected, "the full body must arrive intact despite the drip");
    }

    /// Starts a remote that answers `POST /v1/pallets/{pallet}` — what `update_ref` hits — only
    /// after `delay`. Stands in for the settled contract's slow first-push audit walk
    /// (`audit_utils.rs`): server-side work `update_ref` legitimately waits on before its first
    /// response byte, unbounded by design because it is scoped by the history segment being
    /// pushed, which on a first lift into an empty pallet is the whole history and can take
    /// minutes. Returns the remote's base URL; the connection closes itself after the one
    /// delayed response, so nothing needs parking or dropping here.
    fn start_slow_ref_update_remote(delay: std::time::Duration) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());

        std::thread::spawn(move || {
            use std::io::Write;

            if let Ok((mut stream, _)) = listener.accept() {
                let _ = read_test_request(&mut stream);
                std::thread::sleep(delay);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.flush();
            }
        });

        url
    }

    /// `update_ref` must ride out a response slower than the tight read/metadata budget, not fail
    /// at it — its server side legitimately runs an audit walk that can take minutes on a real
    /// first push into an empty pallet, and must never be cut off by a bound meant for a
    /// handshake or a single object. A fixture that only answers after the effective tight budget
    /// (connect + read) plus slack, succeeding here, is the shape that gets without a live slow
    /// server (the real case is that server-side audit walk — not something this test suite can
    /// wait out directly). If a future change ever gives `update_ref` the same per-request
    /// timeout the reads get, this is exactly what turns red: the call would fail at the timeout,
    /// well before this fixture ever answers.
    #[test]
    fn update_ref_outlives_the_read_metadata_timeout() {
        let past_read_timeout = TEST_DIRECT_CONNECT_TIMEOUT + TEST_TIGHT_READ_TIMEOUT
            + std::time::Duration::from_secs(3);
        let url = start_slow_ref_update_remote(past_read_timeout);
        let client = RemoteClient::new(&url, None).unwrap();
        let outer_ceiling = past_read_timeout + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(
                outer_ceiling,
                client.update_ref("main", None, &"a".repeat(64)),
            ).await
        });

        outcome
            .unwrap_or_else(|_| panic!(
                "update_ref hung past its own generous outer ceiling {:?}", outer_ceiling
            ))
            .unwrap_or_else(|e| panic!(
                "update_ref must not fail on a response that only arrived slowly, never on any \
                timeout: {}", e
            ));
    }

    /// A fixture standing in for a Tor SOCKS proxy this test fully controls: it TCP-accepts (the
    /// "connect" a proxied client dials through it) and then genuinely parks, never beginning the
    /// SOCKS5 handshake. From the connector's perspective this is still "connecting" — the whole
    /// proxy handshake is part of establishing the tunnel, exactly like the real onion circuit
    /// build `REMOTE_CONNECT_TIMEOUT_TOR`'s doc describes — so this is a faithful stand-in for a
    /// slow Tor dial without needing a live daemon.
    struct ParkingSocksProxy {
        addr: String,
        _park: std::sync::mpsc::Sender<()>,
    }

    impl ParkingSocksProxy {
        fn start() -> ParkingSocksProxy {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            let (tx, rx) = std::sync::mpsc::channel::<()>();

            std::thread::spawn(move || {
                if let Ok((stream, _)) = listener.accept() {
                    let _ = rx.recv();
                    drop(stream);
                }
            });

            ParkingSocksProxy { addr, _park: tx }
        }
    }

    /// A Tor-routed client's read/silence budget must not preempt its own, much larger, connect
    /// budget. Built directly through `new_with_tor`/`TorSettings` (`TorMode::On`, pointed at a
    /// fixture-controlled fake SOCKS proxy) — no live Tor daemon, no config file needed. With a
    /// flat `TEST_TIGHT_READ_TIMEOUT` `read_timeout` fired regardless of `connect_timeout`
    /// (`read_timeout` is armed and checked before the connector is even polled), this call would
    /// fail at ~10s even though `REMOTE_CONNECT_TIMEOUT_TOR` allows 60s; with the configured
    /// `read_timeout` accommodating whichever connect budget applies, the call must still be
    /// running well past both that flat tight budget and the direct connect budget.
    #[test]
    fn tor_routed_read_budget_does_not_preempt_the_tor_connect_budget() {
        let proxy = ParkingSocksProxy::start();
        let tor = TorSettings { mode: TorMode::On, proxy: format!("socks5h://{}", proxy.addr) };
        let client = RemoteClient::new_with_tor(
            "http://forklift-fork-49-tor-test.invalid", None, tor,
        ).unwrap();

        // Comfortably past both the old flat tight-budget bug and the direct connect budget — if
        // either were still preempting the Tor connect allowance, the call would already have
        // failed well before this fires.
        let check_after = TEST_DIRECT_CONNECT_TIMEOUT + TEST_TIGHT_READ_TIMEOUT
            + std::time::Duration::from_secs(10);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(assert_still_running("fetch_info (Tor-routed)", check_after, client.fetch_info()));
    }

    /// A fixture SOCKS5 proxy that **completes the handshake honestly** — greeting, method
    /// selection, and a successful `CONNECT` reply, per RFC 1928 — so the connector's own
    /// `connect_timeout` enforcement is satisfied and the call moves past the connect phase
    /// entirely, and only *then* parks, never forwarding a byte of the "target" response. Exists
    /// to reach the read-silence branch specifically: unlike [`ParkingSocksProxy`] (which never
    /// even finishes the handshake, so its silence is itself a connect-phase failure), this
    /// proxy's silence happens strictly after connect — whatever fires next is `read_timeout`'s
    /// own generic sleep, the branch whose reported bound this fixture exists to check.
    struct HandshakeCompletingSocksProxy {
        addr: String,
        _park: std::sync::mpsc::Sender<()>,
    }

    impl HandshakeCompletingSocksProxy {
        fn start() -> HandshakeCompletingSocksProxy {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            let (tx, rx) = std::sync::mpsc::channel::<()>();

            std::thread::spawn(move || {
                use std::io::{Read, Write};

                let Ok((mut stream, _)) = listener.accept() else { return };

                // Greeting: VER(1) NMETHODS(1) METHODS(NMETHODS) — consume it, then unconditionally
                // select "no auth" (0x00).
                let mut header = [0u8; 2];
                if stream.read_exact(&mut header).is_err() { return; }
                let mut methods = vec![0u8; header[1] as usize];
                if stream.read_exact(&mut methods).is_err() { return; }
                if stream.write_all(&[0x05, 0x00]).is_err() { return; }

                // CONNECT request: VER(1) CMD(1) RSV(1) ATYP(1) DST.ADDR DST.PORT(2) — the address
                // form varies by ATYP; consume whichever the client actually sent.
                let mut req_head = [0u8; 4];
                if stream.read_exact(&mut req_head).is_err() { return; }
                let address_read = match req_head[3] {
                    0x01 => stream.read_exact(&mut [0u8; 4 + 2]),
                    0x04 => stream.read_exact(&mut [0u8; 16 + 2]),
                    0x03 => {
                        let mut len = [0u8; 1];
                        if stream.read_exact(&mut len).is_err() { return; }
                        stream.read_exact(&mut vec![0u8; len[0] as usize + 2])
                    }
                    _ => return,
                };
                if address_read.is_err() { return; }

                // Success reply: VER REP=0(succeeded) RSV ATYP=IPv4 BND.ADDR=0.0.0.0 BND.PORT=0 —
                // a minimal but valid "the tunnel is up" answer.
                if stream.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).is_err() { return; }

                // Handshake genuinely done — now park, never reading or answering the tunneled
                // HTTP request that follows.
                let _ = rx.recv();
                drop(stream);
            });

            HandshakeCompletingSocksProxy { addr, _park: tx }
        }
    }

    /// The timed-out message must name the *effective* bound (connect + silence) that actually
    /// governs the client — `bounded_read_timeout` computes what the client is actually
    /// configured with, and `describe_transport_error` must report that same figure, not the raw
    /// silence constant alone. The gap is widest, and nothing before this test checked it, on a
    /// Tor-routed remote: the effective bound is 70s (60s Tor
    /// connect + 10s silence), not the 10s an earlier version of the message printed regardless of
    /// transport. Uses [`HandshakeCompletingSocksProxy`], not [`ParkingSocksProxy`]: this test
    /// needs the failure to land specifically in the read-silence branch, which requires connect
    /// to have genuinely succeeded first.
    #[test]
    fn fetch_info_message_names_the_effective_tor_bound() {
        let proxy = HandshakeCompletingSocksProxy::start();
        let tor = TorSettings { mode: TorMode::On, proxy: format!("socks5h://{}", proxy.addr) };
        let client = RemoteClient::new_with_tor(
            "http://forklift-fork-49-tor-message-test.invalid", None, tor,
        ).unwrap();

        let effective_budget = TEST_TOR_CONNECT_TIMEOUT + TEST_TIGHT_READ_TIMEOUT;
        let hard_ceiling = effective_budget + std::time::Duration::from_secs(20);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.fetch_info()).await
        });

        let inner = outcome.unwrap_or_else(|_| panic!(
            "fetch_info hung past the test's own {:?} ceiling — no timeout fired at all",
            hard_ceiling
        ));
        let message = match inner {
            Err(message) => message,
            Ok(_) => panic!("a parked proxy must not appear to succeed"),
        };

        assert!(
            message.to_lowercase().contains("timed out"),
            "must fail specifically with a timeout: {}", message
        );
        assert!(
            !message.to_lowercase().contains("could not connect"),
            "must land in the read-silence branch, not the connect branch — the SOCKS handshake \
            completed successfully in this fixture: {}", message
        );
        assert!(
            message.contains(&format!("{:?}", effective_budget)),
            "must name the effective Tor bound {:?} (60s connect + 10s silence), not some other \
            figure — an operator on an onion remote needs the real number to decide whether to \
            retry or raise a bound: {}", effective_budget, message
        );
    }

    /// Pins the one link a live Tor-routed upload test would otherwise need ~72s of real wall
    /// time to prove (60s Tor connect + 10s silence + verify) — that `new_with_tor` with
    /// `TorMode::On` selects [`REMOTE_CONNECT_TIMEOUT_TOR`] (60s) as this instance's
    /// `connect_timeout`, not the direct 5s [`REMOTE_CONNECT_TIMEOUT`]. Everything downstream of
    /// that selection — that `send_with_watchdog`'s phase-2 budget folds `self.connect_timeout`
    /// back in, and that the composed message names whatever `connect_timeout` this instance
    /// carries — is already pinned without a live Tor circuit at all: see
    /// `upload_object_times_out_after_a_fully_received_body_that_never_responds`, which exercises
    /// the identical mechanism on the direct 5s client in ~17s. This test is the only remaining
    /// link between the two: that Tor routing actually selects the 60s constant in the first
    /// place. `new_with_tor` does no I/O (a `reqwest::Client` dials lazily, on first request), so
    /// this is instant — no live proxy, no live remote, no real wait.
    #[test]
    fn new_with_tor_selects_the_60s_connect_budget() {
        let tor = TorSettings { mode: TorMode::On, proxy: DEFAULT_TOR_PROXY.to_string() };
        let client = RemoteClient::new_with_tor(
            "http://forklift-fork-49-tor-connect-budget-test.invalid", None, tor,
        ).unwrap();

        assert_eq!(
            client.connect_timeout, TEST_TOR_CONNECT_TIMEOUT,
            "TorMode::On must select the 60s Tor connect budget, not the direct 5s one"
        );
    }

    // -----------------------------------------------------------------------------------
    // FORK-49 slice 2: the upload path. `upload_object` and `put_presigned` used to hang forever
    // against a remote that accepted the connection and then simply stopped reading — no error,
    // no exit. `read_timeout` (the read-path fix, slice 1) cannot be reused here: it is a flat,
    // non-resetting deadline covering connect *and* the whole request-body send, so arming it on
    // an upload would cap total upload time and kill a healthy large transfer on a slow link —
    // worse than the bug. `UPLOAD_SILENCE_BUDGET`/`send_with_watchdog` are a different mechanism:
    // a shared timestamp updated every time `reqwest::Body`'s underlying stream actually yields a
    // chunk to hyper, checked by a polling loop that fires only once that timestamp has gone
    // stale for the whole budget — see `UPLOAD_SILENCE_BUDGET`'s own doc for the full reasoning.
    //
    // The wedged-remote fixture below reads only the request headers and then genuinely stops
    // reading the body (parks, never closes) — bodies are sized in the tens of megabytes,
    // comfortably past the ~0.9-2.2 MiB of client-side buffering the spike measured on
    // macOS/loopback (and CI also runs Linux/Windows, where TCP window autotuning typically
    // buffers *more*, not less), so the watchdog's own stall detection is what bounds these
    // tests on every platform, never a coincidence of one machine's buffer sizing.
    // -----------------------------------------------------------------------------------

    /// Body size for the wedged-upload tests below: deliberately tens of megabytes, not a number
    /// close to the ~0.9-2.2 MiB buffering ceiling the spike measured on macOS/loopback, and CI
    /// also runs Linux and Windows, where TCP window autotuning typically buffers *more*. A body
    /// too close to (or under) that ceiling risks the whole payload fitting inside hyper's own
    /// prefetch buffer before the fixture ever stops draining it — the client would then finish
    /// "sending" without ever stalling, and the test would hang waiting on a response that never
    /// arrives instead of exercising the watchdog at all. This margin is what makes the watchdog's
    /// own stall detection the thing that bounds the test, on every platform, not a lucky
    /// coincidence of one machine's buffer sizing.
    const WEDGED_UPLOAD_BODY_LEN: usize = 32 * 1024 * 1024;

    /// Real bytes (not all-zero) of [`WEDGED_UPLOAD_BODY_LEN`] length — content doesn't matter
    /// here beyond being real data the client must actually move, not something a naive
    /// implementation could special-case away.
    fn oversized_upload_body() -> Vec<u8> {
        (0..WEDGED_UPLOAD_BODY_LEN).map(|i| (i % 251) as u8).collect()
    }

    /// A remote that accepts the connection, reads only the request headers, and then genuinely
    /// stops reading — never touching the body, never closing the connection (no FIN). From the
    /// client's own kernel outward the peer looks alive but is not consuming anything, which is
    /// exactly the FORK-49 slice-2 bug: a body-send stream that stalls because the peer stopped
    /// reading, not because it went away. Parks on the same channel-recv pattern as `SilentRemote`
    /// (slice 1, above) — dropped, never signaled mid-test, only once `_park` goes out of scope
    /// at the end of the test.
    struct WedgedUploadRemote {
        url: String,
        _park: std::sync::mpsc::Sender<()>,
    }

    impl WedgedUploadRemote {
        fn start() -> WedgedUploadRemote {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let (tx, rx) = std::sync::mpsc::channel::<()>();

            std::thread::spawn(move || {
                use std::io::Read;

                if let Ok((mut stream, _)) = listener.accept() {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 4096];
                    // Read only up through the header terminator — deliberately never the body.
                    // Draining the body (even discarding it) would let the client finish sending
                    // without ever hitting real backpressure, defeating the whole point of this
                    // fixture.
                    loop {
                        if buffer.windows(4).position(|w| w == b"\r\n\r\n").is_some() {
                            break;
                        }
                        match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let _ = rx.recv();
                    drop(stream);
                }
            });

            WedgedUploadRemote { url, _park: tx }
        }
    }

    /// `upload_object` against a remote that accepts the connection, reads the request headers,
    /// and then genuinely stops reading the body must fail with a timeout — not hang the caller
    /// forever. Before FORK-49 slice 2, neither the body-send client nor any watchdog bounded
    /// this call at all.
    #[test]
    fn upload_object_times_out_against_a_wedged_remote() {
        let remote = WedgedUploadRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let hash = "a".repeat(64);
        let action = format!("uploading object {}", hash);
        let effective_budget = TEST_DIRECT_CONNECT_TIMEOUT + TEST_UPLOAD_SILENCE_BUDGET;
        let hard_ceiling = effective_budget + std::time::Duration::from_secs(25);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(
                hard_ceiling, client.upload_object(&hash, oversized_upload_body()),
            ).await
        });

        let message = outcome
            .unwrap_or_else(|_| panic!(
                "upload_object hung past the test's own {:?} ceiling — no timeout fired at all",
                hard_ceiling
            ))
            .expect_err("a wedged remote must not appear to succeed");

        assert!(
            message.to_lowercase().contains("timed out"),
            "must fail specifically with a timeout, not some other transport error: {}", message
        );
        assert_eq!(
            message, RemoteClient::mutation_read_timeout_message(&action),
            "the watchdog kill must carry the same mutation-uncertainty wording as a real \
            reqwest::Error of the same shape, composed through the same function, not some \
            generic or ad hoc message: {}", message
        );
    }

    /// `put_presigned` against the same wedged shape must be bounded exactly like `upload_object`
    /// — pins that it is independently wired to the watchdog rather than accidentally covered by
    /// `upload_object`'s own wiring. It is the higher-risk site: it dials straight through
    /// `self.no_redirect.put(url)`, bypassing `request`/`request_on` entirely (no bearer token
    /// attached), so nothing about `upload_object`'s watchdog wiring guarantees this one got the
    /// same fix, even though both now ride the same `no_redirect` client for redirect handling.
    #[test]
    fn put_presigned_times_out_against_a_wedged_remote() {
        let remote = WedgedUploadRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let action = "uploading to a staging URL";
        let effective_budget = TEST_DIRECT_CONNECT_TIMEOUT + TEST_UPLOAD_SILENCE_BUDGET;
        let hard_ceiling = effective_budget + std::time::Duration::from_secs(25);
        let url = remote.url.clone();

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(
                hard_ceiling, client.put_presigned(&url, oversized_upload_body()),
            ).await
        });

        let message = outcome
            .unwrap_or_else(|_| panic!(
                "put_presigned hung past the test's own {:?} ceiling — no timeout fired at all",
                hard_ceiling
            ))
            .expect_err("a wedged remote must not appear to succeed");

        assert!(
            message.to_lowercase().contains("timed out"),
            "must fail specifically with a timeout, not some other transport error: {}", message
        );
        assert_eq!(
            message, RemoteClient::mutation_read_timeout_message(action),
            "the watchdog kill must carry the same mutation-uncertainty wording as a real \
            reqwest::Error of the same shape: {}", message
        );
    }

    // -----------------------------------------------------------------------------------
    // The post-send phase (review round S2-F1): a remote that reads the request headers *and*
    // the entire body, then wedges before writing a response, used to hang forever even after the
    // send-phase watchdog above — `send_with_watchdog` broke out of its loop the instant
    // `progress.is_exhausted()` and then awaited the response with nothing bounding it, on the
    // theory that this matched every other mutation's unbounded response wait (`update_ref`'s
    // audit walk in particular). That theory doesn't hold here: `update_ref`'s wait is unbounded
    // because its server-side work (the pushed history segment) can legitimately take minutes and
    // the client has no way to size it in advance; `upload_object`'s post-receive work is inline
    // hash verification of the exact bytes the client just sent, capped by `body_len` — a
    // quantity the client already has in hand. That's what makes bounding this phase honest.
    //
    // A follow-up round (S2-F2/S2-F3) found the *first* fix too tight: `post_send_verify_budget`
    // alone ignores connect latency and the in-flight tail hyper/the OS kernel can still be
    // flushing the instant `is_exhausted()` flips — see `send_with_watchdog`'s doc for the full
    // reasoning. The budget these tests now expect is `phase2_budget`
    // (`connect + UPLOAD_SILENCE_BUDGET + post_send_verify_budget(body_len)`), not
    // `post_send_verify_budget(body_len)` alone.
    //
    // `SilentRemote` (above, from slice 1) is exactly the fixture this needs: it already reads a
    // request in full via `read_test_request` (headers *and* body, draining to `Content-Length`)
    // and then parks without ever writing a response — a wedge that happens *after* a complete
    // receive, never during one. That's what makes it different from `WedgedUploadRemote`, which
    // stops reading at the header terminator and never touches the body: the two fixtures probe
    // the two phases this watchdog now separately bounds.
    // -----------------------------------------------------------------------------------

    /// `upload_object` against a remote that reads the whole request and then goes silent — never
    /// writing a response — must fail with a timeout scaled to the body size, not hang forever.
    /// This is the gap the review round found: the send-phase watchdog alone does not cover it,
    /// since the stream is genuinely exhausted (the whole body really was delivered) by the time
    /// this remote stops responding.
    #[test]
    fn upload_object_times_out_after_a_fully_received_body_that_never_responds() {
        let remote = SilentRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let hash = "e".repeat(64);
        let action = format!("uploading object {}", hash);
        // Deliberately small — the whole point is that this bound is tight for an ordinary-sized
        // object, not that it needs a huge body to exercise (that's the *send*-phase tests' job).
        let body = vec![9u8; 64 * 1024];
        let phase2_budget = TEST_DIRECT_CONNECT_TIMEOUT + TEST_UPLOAD_SILENCE_BUDGET
            + post_send_verify_budget(body.len());
        let hard_ceiling = phase2_budget + std::time::Duration::from_secs(20);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.upload_object(&hash, body)).await
        });

        let message = outcome
            .unwrap_or_else(|_| panic!(
                "upload_object hung past the test's own {:?} ceiling — no timeout fired at all",
                hard_ceiling
            ))
            .expect_err("a remote that never responds must not appear to succeed");

        assert!(
            message.to_lowercase().contains("timed out"),
            "must fail specifically with a timeout, not some other transport error: {}", message
        );
        assert_eq!(
            message, RemoteClient::mutation_post_send_timeout_message(&action, phase2_budget),
            "a post-send wedge must carry the post-send wording (transmission known-complete, \
            verification may have already happened) and name the full phase-2 budget (connect + \
            silence + verify, not verify alone), not the mid-body composer's: {}", message
        );
        assert!(
            message.contains("finished streaming the request body")
                && message.contains("may already have received, verified, and stored"),
            "must not understate what's known (the full body really was handed off), nor \
            overstate it (claiming the bytes are certainly \"on the wire\" — S2-F7): {}", message
        );
    }

    /// `put_presigned` against the same fully-drained-then-silent shape must be bounded exactly
    /// like `upload_object` — both route through the same `send_with_watchdog`, so this is mainly
    /// a check that `put_presigned` passes its own `body_len` through correctly (a copy-paste that
    /// dropped the argument, or passed `0`, would still compile and would still send a real
    /// request, but would silently produce the wrong budget or the wrong message for this site
    /// alone) rather than a distinct mechanism to prove.
    #[test]
    fn put_presigned_times_out_after_a_fully_received_body_that_never_responds() {
        let remote = SilentRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let action = "uploading to a staging URL";
        let body = vec![9u8; 64 * 1024];
        let phase2_budget = TEST_DIRECT_CONNECT_TIMEOUT + TEST_UPLOAD_SILENCE_BUDGET
            + post_send_verify_budget(body.len());
        let hard_ceiling = phase2_budget + std::time::Duration::from_secs(20);
        let url = remote.url.clone();

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.put_presigned(&url, body)).await
        });

        let message = outcome
            .unwrap_or_else(|_| panic!(
                "put_presigned hung past the test's own {:?} ceiling — no timeout fired at all",
                hard_ceiling
            ))
            .expect_err("a remote that never responds must not appear to succeed");

        assert!(
            message.to_lowercase().contains("timed out"),
            "must fail specifically with a timeout, not some other transport error: {}", message
        );
        assert_eq!(
            message, RemoteClient::mutation_post_send_timeout_message(action, phase2_budget),
            "a post-send wedge must carry the post-send wording: {}", message
        );
    }

    /// Starts a remote that reads the request headers, then drains the body slowly but
    /// deterministically — `iterations` cycles of "sleep `gap`, then read exactly `read_size`
    /// bytes (or the remainder)" — and only answers `200 OK` once the whole declared
    /// `Content-Length` has arrived. Mirrors `start_steady_drip_remote`'s read-direction fixture
    /// (a transfer that is always moving, however slowly, must never be treated as a stall — see
    /// that fixture's doc) for the write direction: it is the *server's* paced draining, not a
    /// chunked write schedule, that paces the client's own body-send stream through ordinary TCP
    /// backpressure. The inner read loop blocks until it actually has `read_size` bytes (or the
    /// remainder) rather than accepting whatever one `read()` call happens to return, so the
    /// outer cadence — one drain burst every `gap` — is deterministic regardless of kernel socket
    /// buffer sizing, which is what makes the fixture's total duration predictable across
    /// platforms.
    fn start_slow_draining_remote(read_size: usize, gap: std::time::Duration) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());

        std::thread::spawn(move || {
            use std::io::{Read, Write};

            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                let header_end = loop {
                    if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                        break position + 4;
                    }
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                    }
                };

                let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
                let content_length: usize = head.lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
                    .and_then(|line| line.split_once(':'))
                    .and_then(|(_, value)| value.trim().parse().ok())
                    .unwrap_or(0);

                let mut received = buffer.len() - header_end;
                let mut read_buf = vec![0u8; read_size];

                while received < content_length {
                    std::thread::sleep(gap);
                    let target = std::cmp::min(read_size, content_length - received);
                    let mut got = 0usize;
                    while got < target {
                        match stream.read(&mut read_buf[got..target]) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => got += n,
                        }
                    }
                    received += got;
                }

                let _ = write!(
                    stream, "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.flush();
            }
        });

        url
    }

    /// `upload_object` against a body that drains slowly but steadily — every inter-read gap
    /// comfortably under the effective watchdog budget, the total transfer comfortably past it —
    /// must **succeed**, never be treated as a stall. This is the anti-regression test and the
    /// whole point of the watchdog: a naive per-request *total* deadline (the shape rejected for
    /// the read path too, see `REMOTE_READ_TIMEOUT`'s doc) fails this test, killing the transfer
    /// partway through even though no single gap ever went silent.
    #[test]
    fn upload_object_survives_a_slow_but_steadily_draining_remote() {
        let read_size = 4 * 1024 * 1024;
        let gap = std::time::Duration::from_secs(3);
        let iterations = 10u32;
        let body_len = read_size * iterations as usize;
        let effective_budget = TEST_DIRECT_CONNECT_TIMEOUT + TEST_UPLOAD_SILENCE_BUDGET;
        let total_duration_floor = gap * iterations;
        assert!(
            total_duration_floor > effective_budget,
            "the fixture must actually outlast the budget under test: {:?} vs {:?}",
            total_duration_floor, effective_budget
        );
        assert!(
            gap < effective_budget,
            "every individual gap must stay comfortably under the budget — otherwise this test \
            would exercise the wedged case, not the steady one"
        );

        let bytes: Vec<u8> = (0..body_len).map(|i| (i % 251) as u8).collect();
        let url = start_slow_draining_remote(read_size, gap);
        let client = RemoteClient::new(&url, None).unwrap();
        let hash = "b".repeat(64);
        let outer_ceiling = total_duration_floor + std::time::Duration::from_secs(30);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(outer_ceiling, client.upload_object(&hash, bytes)).await
        });

        outcome
            .unwrap_or_else(|_| panic!(
                "upload_object hung past its own generous outer ceiling {:?}", outer_ceiling
            ))
            .unwrap_or_else(|e| panic!(
                "a slow-but-steady upload must succeed — it was never silent, so it must never \
                be treated as a stall: {}", e
            ));
    }

    /// A remote that fully drains a request (headers and body, like `read_test_request`),
    /// records the raw header block verbatim, and answers `200 OK` — for asserting exactly what
    /// headers a streamed upload actually sent, not just that the transfer succeeded. The headers
    /// are sent over `tx` *before* the response is written, so by the time the client observes a
    /// successful response, the header capture has already happened — no race to poll for.
    fn start_header_capturing_remote() -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel::<String>();

        std::thread::spawn(move || {
            use std::io::{Read, Write};

            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                let header_end = loop {
                    if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                        break position + 4;
                    }
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                    }
                };

                let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
                let content_length: usize = head.lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
                    .and_then(|line| line.split_once(':'))
                    .and_then(|(_, value)| value.trim().parse().ok())
                    .unwrap_or(0);

                let mut received = buffer.len() - header_end;
                while received < content_length {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => received += n,
                    }
                }

                let _ = tx.send(head);

                let _ = write!(
                    stream, "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.flush();
            }
        });

        (url, rx)
    }

    /// The streamed upload body must carry an explicit `Content-Length`, never fall back to
    /// `Transfer-Encoding: chunked` — a presigned S3 `PUT` rejects chunked framing outright (see
    /// `put_presigned`'s doc and `watched_upload_body`'s). `reqwest::Body::wrap_stream`'s body
    /// always reports an unknown `size_hint`, so this header only exists because
    /// `upload_object`/`put_presigned` set it explicitly — this pins that they still do.
    #[test]
    fn upload_object_sends_an_explicit_content_length_not_chunked_encoding() {
        let (url, rx) = start_header_capturing_remote();
        let client = RemoteClient::new(&url, None).unwrap();
        let hash = "c".repeat(64);
        let body = vec![7u8; 200 * 1024];
        let expected_len = body.len();

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(client.upload_object(&hash, body))
            .expect("a fully-draining remote must let the upload succeed");

        let headers = rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("the fixture must have captured request headers by the time the call returned");
        let lower = headers.to_ascii_lowercase();

        assert!(
            lower.contains(&format!("content-length: {}", expected_len)),
            "the streamed upload must carry an explicit Content-Length matching the real body \
            size, not rely on chunked framing: {}", headers
        );
        assert!(
            !lower.contains("transfer-encoding"),
            "must never fall back to chunked transfer-encoding — a presigned S3 PUT rejects it \
            outright: {}", headers
        );
    }

    // -----------------------------------------------------------------------------------
    // Review round S2-F2: `is_exhausted()` means every chunk was handed to *hyper*, not that
    // every byte reached the *peer*. `start_delayed_read_remote` below is the deterministic,
    // cross-platform way to exercise the gap between those two facts without depending on any
    // particular OS/hyper buffering threshold (which the FORK-49 spike already established is
    // non-deterministic even on one machine — see `WEDGED_UPLOAD_BODY_LEN`'s doc): a body small
    // enough to fit inside *any* platform's default TCP send/receive window (a couple KB, far
    // under even the smallest realistic default) gets fully accepted into the outgoing pipeline —
    // and so reported "exhausted" by the client's own stream — almost instantly, regardless of
    // whether the peer's *application* has read a single byte. The fixture's deliberate pause
    // before it ever calls `read()` stands in for exactly the invisible, still-genuinely-
    // happening work (an in-flight tail draining slowly) S2-F2 is about — the client cannot tell
    // it apart from having actually reached the remote already, and must not be too impatient
    // about it.
    // -----------------------------------------------------------------------------------

    /// Starts a remote that reads only the request headers immediately, then pauses for `delay`
    /// *before touching the body at all*, then drains the body fully and answers `200 OK`. See
    /// this section's own comment for why a small body plus this pause is a deterministic stand-in
    /// for S2-F2's real failure (a slow-draining in-flight tail the client cannot observe).
    fn start_delayed_read_remote(delay: std::time::Duration) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());

        std::thread::spawn(move || {
            use std::io::{Read, Write};

            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                let header_end = loop {
                    if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                        break position + 4;
                    }
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                    }
                };

                let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
                let content_length: usize = head.lines()
                    .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
                    .and_then(|line| line.split_once(':'))
                    .and_then(|(_, value)| value.trim().parse().ok())
                    .unwrap_or(0);

                // Deliberately does not read a byte of the body yet — the client's own kernel
                // and hyper's buffering will have already accepted a small body regardless.
                std::thread::sleep(delay);

                let mut received = buffer.len().saturating_sub(header_end);
                while received < content_length {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => received += n,
                    }
                }

                let _ = write!(
                    stream, "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.flush();
            }
        });

        url
    }

    /// `upload_object` must survive a remote that pauses before reading a small body — the
    /// deterministic stand-in for S2-F2's real failure (see this section's own comment). A
    /// verify-only phase-2 budget (`post_send_verify_budget` alone — the shape S2-F2 found too
    /// tight) is far shorter than `delay` here and would fail this test; the real `phase2_budget`
    /// (which folds the full send-phase allowance back in as flush margin, see
    /// `send_with_watchdog`'s doc) comfortably covers it. This is exactly the test a fix that
    /// only re-tightened `post_send_verify_budget`'s own constants — without folding the
    /// send-phase allowance back in — would still fail.
    #[test]
    fn upload_object_survives_a_remote_that_pauses_before_reading_a_small_body() {
        let body_len = 2 * 1024;
        let verify_only_budget = post_send_verify_budget(body_len);
        let phase2_budget = TEST_DIRECT_CONNECT_TIMEOUT + TEST_UPLOAD_SILENCE_BUDGET + verify_only_budget;
        // Comfortably past the (too-tight) verify-only budget, comfortably short of the real
        // phase-2 budget — the gap this test exists to probe.
        let delay = verify_only_budget + std::time::Duration::from_secs(8);
        assert!(
            delay > verify_only_budget,
            "delay must exceed the too-tight verify-only budget, or this doesn't prove anything \
            about S2-F2 at all"
        );
        assert!(
            delay < phase2_budget,
            "delay must stay under the real phase-2 budget, or this is a timeout test in \
            disguise rather than a \"must survive\" one"
        );

        let url = start_delayed_read_remote(delay);
        let client = RemoteClient::new(&url, None).unwrap();
        let hash = "f".repeat(64);
        let body = vec![3u8; body_len];
        let outer_ceiling = phase2_budget + std::time::Duration::from_secs(20);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(outer_ceiling, client.upload_object(&hash, body)).await
        });

        outcome
            .unwrap_or_else(|_| panic!(
                "upload_object hung past its own generous outer ceiling {:?}", outer_ceiling
            ))
            .unwrap_or_else(|e| panic!(
                "a remote that is genuinely still working (just invisibly to the client) must \
                not be treated as a stall: {}", e
            ));
    }

    // -----------------------------------------------------------------------------------
    // Review round S2-F5: `wrap_stream`'s body cannot be cloned to replay a redirect (verified
    // against this pinned `reqwest`/`tower-http` version — see `describe_upload_redirect`'s doc),
    // so a `3xx` a streamed upload receives is no longer followed the way the old
    // `.body(Vec<u8>)` used to follow it. Pre-1.0 that behavior change needs no compatibility
    // shim, but it must be loud and tested, not a silent fall-through to a generic "refused"
    // message.
    // -----------------------------------------------------------------------------------

    /// A remote that reads a full request and answers with a redirect (`status`, `location`)
    /// instead of a normal response — for proving a streamed upload surfaces it as a specific,
    /// named error rather than following it silently or reporting a bare status code.
    fn start_redirecting_remote(status: u16, location: &str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let status = status;
        let location = location.to_string();

        std::thread::spawn(move || {
            use std::io::Write;

            if let Ok((mut stream, _)) = listener.accept() {
                let _ = read_test_request(&mut stream);
                let reason = match status {
                    301 => "Moved Permanently",
                    302 => "Found",
                    303 => "See Other",
                    307 => "Temporary Redirect",
                    308 => "Permanent Redirect",
                    _ => "Redirect",
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {} {}\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    status, reason, location
                );
                let _ = stream.flush();
            }
        });

        url
    }

    /// `put_presigned` against a remote that answers a fully-received upload with a `307` must
    /// surface a specific, named error — never silently retry elsewhere (it structurally cannot:
    /// the body is a one-shot stream, already consumed) and never fall through to a bare "refused
    /// (307)" that gives no hint a redirect was even involved. This is the "live case" review
    /// round S2-F5 named: an S3 bucket reached via the wrong regional endpoint answers `PUT` with
    /// exactly this shape.
    #[test]
    fn put_presigned_reports_a_redirect_by_name_instead_of_following_or_hiding_it() {
        let location = "https://correct-region.example.com/bucket/key?X-Amz-Signature=redirected";
        let url = start_redirecting_remote(307, location);
        let client = RemoteClient::new(&url, None).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let error = runtime.block_on(client.put_presigned(&url, vec![1u8; 4096]))
            .expect_err("a redirect must not appear to succeed");

        assert!(
            error.to_lowercase().contains("redirect"),
            "must name what actually happened, not a bare status code: {}", error
        );
        assert!(
            error.contains("307"),
            "must name the actual status: {}", error
        );
        assert!(
            error.contains(location),
            "must surface the Location header so the caller can see exactly where the remote \
            pointed, not just that a redirect happened: {}", error
        );
    }

    /// `upload_object` gets the same treatment for consistency (it also rides `no_redirect`,
    /// which returns a raw `3xx` straight to the `is_redirection()` guard) even though its
    /// target — this module's own control plane — is not the "live case" S2-F5 named; a lighter
    /// check than `put_presigned`'s since the mechanism (`describe_upload_redirect`) is shared
    /// and already fully pinned there.
    #[test]
    fn upload_object_reports_a_redirect_by_name() {
        let location = "https://elsewhere.example.com/v1/objects/moved";
        let url = start_redirecting_remote(302, location);
        let client = RemoteClient::new(&url, None).unwrap();
        let hash = "a".repeat(64);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let error = runtime.block_on(client.upload_object(&hash, vec![1u8; 4096]))
            .expect_err("a redirect must not appear to succeed");

        assert!(
            error.to_lowercase().contains("redirect") && error.contains("302") && error.contains(location),
            "must name the redirect, its status, and its target: {}", error
        );
    }

    // -----------------------------------------------------------------------------------
    // The `303` redirect hole: `is_redirection()` guards `upload_object`/`put_presigned`, but
    // both rode `self.http`, which auto-follows. `tower-http`'s `follow_redirect` middleware
    // (`follow_redirect/mod.rs`, `SEE_OTHER` arm) unconditionally forces the body to
    // `BodyRepr::Empty` and rewrites the method to `GET` *before* the `take()` guard the
    // `MOVED_PERMANENTLY | FOUND` arm relies on to skip non-`POST` methods — so a `303` to a
    // streamed `PUT` silently becomes a bare `GET`, and a `2xx` at the target makes the call
    // return `Ok(())` having stored nothing. The 302/307 tests above cannot catch this: a `PUT`
    // simply misses the `method == POST` condition in the other arm. The fix routes both sites
    // through `self.no_redirect` instead of special-casing `303`, so no future tower-http
    // redirect-matrix change can reopen this hole.
    // -----------------------------------------------------------------------------------

    /// A second live listener a redirect can point at: answers any request with a bare `200 OK`
    /// and no body, and records whether it was ever contacted. Landing on this flag being `true`
    /// is what would prove a redirect was actually followed — the discriminator this test needs
    /// beyond just "an `Err` came out", since an unrelated `Err` could come out for the wrong
    /// reason and still leave this looking green.
    fn start_landing_remote() -> (String, Arc<std::sync::atomic::AtomicBool>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let landed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let landed_writer = landed.clone();

        std::thread::spawn(move || {
            use std::io::Write;

            if let Ok((mut stream, _)) = listener.accept() {
                let _ = read_test_request(&mut stream);
                landed_writer.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.flush();
            }
        });

        (url, landed)
    }

    /// The fixed hole itself: a `303` to `put_presigned` must not be auto-followed. Before the
    /// fix (`self.http`, which auto-follows) this returns `Ok(())` with the landing flag `true` —
    /// the redirect target really was reached as a bare `GET` and its `200` read back as success.
    /// After the fix (`self.no_redirect`) the raw `303` comes back to the existing guard, the
    /// landing flag stays `false`, and the error names the status and location by the same
    /// mechanism the 302/307 tests already pin.
    #[test]
    fn put_presigned_does_not_follow_a_303_to_a_2xx_landing() {
        let (landing_url, landed) = start_landing_remote();
        let url = start_redirecting_remote(303, &landing_url);
        let client = RemoteClient::new(&url, None).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let error = runtime.block_on(client.put_presigned(&url, vec![1u8; 4096]))
            .expect_err("a 303 must not be silently followed to a 2xx landing");

        assert!(
            error.to_lowercase().contains("redirect") && error.contains("303") && error.contains(&landing_url),
            "must name the redirect, its status, and its target: {}", error
        );
        assert!(
            !landed.load(std::sync::atomic::Ordering::SeqCst),
            "the landing listener must never have been contacted — that is what proves no \
            follow was attempted, rather than merely that an error came out"
        );
    }

    /// Same hole, `upload_object` site — the two client selections in the fix (`upload_object` →
    /// `self.no_redirect` via `request_on`, `put_presigned` → `self.no_redirect.put`) change
    /// independently, so each needs its own falsifier.
    #[test]
    fn upload_object_does_not_follow_a_303_to_a_2xx_landing() {
        let (landing_url, landed) = start_landing_remote();
        let url = start_redirecting_remote(303, &landing_url);
        let client = RemoteClient::new(&url, None).unwrap();
        let hash = "b".repeat(64);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let error = runtime.block_on(client.upload_object(&hash, vec![1u8; 4096]))
            .expect_err("a 303 must not be silently followed to a 2xx landing");

        assert!(
            error.to_lowercase().contains("redirect") && error.contains("303") && error.contains(&landing_url),
            "must name the redirect, its status, and its target: {}", error
        );
        assert!(
            !landed.load(std::sync::atomic::Ordering::SeqCst),
            "the landing listener must never have been contacted — that is what proves no \
            follow was attempted, rather than merely that an error came out"
        );
    }

    /// Pins the exact wording [`RemoteClient::mutation_read_timeout_message`] produces — the
    /// message the upload watchdog manufactures on a mid-body stall, with no `reqwest::Error` to
    /// hand `classify` (a killed watchdog future is simply dropped, never polled to failure). Must
    /// carry the mutation-uncertainty wording: a body-send stall means the request may, or may
    /// not, have been already underway to the remote, so the caller must never be told nothing
    /// happened (the connect-timeout wording) or that the response merely never arrived (the read
    /// path's own, non-mutation wording) — only that retrying converges. Must also *not* overclaim
    /// (review round S2-F7): `silent_for()` is measured from before `send()` is even called, so
    /// this can fire having pulled zero chunks — "the request was sent" would assert more than is
    /// actually known.
    #[test]
    fn mutation_read_timeout_message_carries_the_uncertainty_wording() {
        let message = RemoteClient::mutation_read_timeout_message("uploading object aaaa");

        assert!(
            message.contains("may or may not have fully reached the remote"),
            "must carry the mutation-uncertainty wording, spanning the whole zero-to-full range \
            (S2-F7), not claim nothing happened: {}", message
        );
        assert!(
            message.contains("retrying is safe"),
            "must tell the caller retrying converges: {}", message
        );
        assert!(
            !message.to_lowercase().contains("nothing was sent"),
            "a body-send stall means the request was (or may have been) partly sent — must never \
            claim nothing was sent, that is the connect-timeout wording: {}", message
        );
        assert!(
            !message.contains("the request was sent"),
            "must not overclaim full delivery when at most partial delivery is known (S2-F7): {}",
            message
        );
    }

    /// Pins the exact wording [`RemoteClient::mutation_post_send_timeout_message`] produces — the
    /// message the upload watchdog manufactures on a *post-send* stall (review round S2-F1/S2-F7).
    /// Distinct from the mid-body composer above: here every chunk really was handed to hyper, so
    /// the message can (and must) say the remote may already have received, verified, and stored
    /// the bytes — but it must still say only what's *locally observable* ("finished streaming …
    /// the request body" — handed to hyper) rather than assert delivery ("the bytes are on the
    /// wire" — S2-F2 found `is_exhausted()` does not mean that), and must name the actual budget
    /// that governed rather than a vague phrase.
    #[test]
    fn mutation_post_send_timeout_message_carries_the_uncertainty_wording() {
        let budget = std::time::Duration::from_secs(17);
        let message = RemoteClient::mutation_post_send_timeout_message("uploading object aaaa", budget);

        assert!(
            message.contains("finished streaming the request body"),
            "must state only the locally-observable fact (handed off to hyper), not a claim \
            about what the remote received: {}", message
        );
        assert!(
            message.contains("may already have received, verified, and stored the bytes"),
            "must carry the mutation-uncertainty wording, properly hedged with \"may\": {}", message
        );
        assert!(
            !message.contains("the bytes are on the wire"),
            "must not overclaim active transmission — the peer may have read zero bytes if fully \
            wedged, which is exactly what S2-F2 found `is_exhausted()` cannot rule out: {}", message
        );
        assert!(
            message.contains(&format!("{:?}", budget)),
            "must name the actual budget that governed, not a generic phrase: {}", message
        );
    }
}
