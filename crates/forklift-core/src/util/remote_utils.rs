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
/// up, and the backoff between attempts — client patience for one narrow, transient condition:
/// the staged objects are already verified content-correct, just not yet visible to the verifier
/// that gates the commit. Only that case is retried; a corrupt or missing object fails at once.
/// The schedule (~0.2s doubling to a 3s cap, spanning about 24s of sleep: 0.2+0.4+0.8+1.6+3×7) is
/// chosen to comfortably outlast ordinary promotion lag while still surfacing a genuinely stuck
/// verifier as an error rather than hanging the lift forever.
///
/// No calibration evidence, and saying so is the point: nothing committed to this repository
/// measures any head's promotion lag, so ~24s is an uncalibrated policy choice about how long a
/// lift waits before giving up. What would calibrate it is a spike that stages a blob against a
/// given head and records the interval until the commit stops answering
/// [`CommitOutcome::BlobNotReady`]; until one exists, do not treat this number as tuned for any
/// deployment.
///
/// Exhausting the schedule is *not* the uncertain-outcome case the rest of this module's budgets
/// report. Reaching here means every staged object was accepted and verified content-correct and
/// only promotion is outstanding, so [`commit_one_batch`] returns its own error stating the
/// upload is safe and the lift can simply be re-run — a definite claim this path is entitled to
/// make and [`RemoteClient::mutation_read_timeout_message`] deliberately is not.
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
/// `fetch_batch` and `fetch_subtree` share the reason neither may ride *this* budget — their
/// server sides build a bundle, every requested object fully into memory, *before* the first byte,
/// work whose cost depends on object sizes the client cannot know in advance, so no flat silence
/// budget over their responses is honest — but they no longer share the same fate, and this clause
/// splits accordingly. `fetch_subtree` is still not bounded at all, and stays that way until it has
/// its own scaled/measured budget or an abandon-and-fall-back lane. `fetch_batch`'s `POST` took the
/// other exit named just above: it is bounded *before the status line* by a different mechanism
/// entirely — a head-wait timer carried on [`Posture::HeadDeadlineNoRedirect`] and sized by
/// [`BATCH_HEAD_PATIENCE`] — which prices no server-side build work at all, and *after* the status
/// line by a client-level silence budget of its own, [`FETCH_OBJECT_READ_TIMEOUT`] rather than this
/// constant (see that constant's doc for why the looser figure, and
/// [`Posture::HeadDeadlineNoRedirect`]'s for how the two phases hand off). That split is what keeps
/// the second budget from ever pricing the build: the head timer is strictly the tighter of the
/// two, so no client-level read timeout can fire before the status line. See the comment at each of
/// those two call sites.
/// `missing_objects`/`upload_targets` differ in exactly the property that matters here: their
/// server sides only walk up to `MAX_MISSING_BATCH`/`MAX_UPLOAD_TARGETS_BATCH` *hashes* — a cost
/// this client can size from its own request body, unlike an object's byte size it doesn't have yet
/// — before the first byte, so like `resolve` they ride [`Posture::TotalDeadline`] rather than
/// staying unbounded, sized per call by [`RemoteClient::presence_negotiation_budget`] (see that
/// method's own doc for the arithmetic and accepted residual). `resolve` is unbounded by *this*
/// mechanism too (no client-level `read_timeout`), but is not left unbounded outright: it rides
/// [`Posture::TotalDeadline`] instead, a genuine per-request `RequestBuilder::timeout` the module
/// applies itself rather than the call site — sized at `connect_timeout + REMOTE_READ_TIMEOUT`,
/// reusing this constant's *value* for that arithmetic without joining the class of calls it
/// silence-bounds (see that variant's own doc for why a *total* deadline, not a silence budget, is
/// the right shape for `resolve` specifically).
/// `update_ref`, `commit_lift`, and the streamed-upload paths (`upload_object`, `put_presigned`)
/// are unbounded for reasons of their own — see each call's own doc, and
/// [`clients::Clients::send_with_watchdog`]'s for the uploads.
///
/// `read_timeout` is a `ClientBuilder`-level setting with no per-request override — it cannot be
/// switched off for one specific request — so a call carries one exactly when the client its
/// posture selects was built with one. [`clients::Clients`]'s field docs are the enumeration that
/// stays current; what matters here is that the client `update_ref` rides
/// ([`Posture::UnboundedNoRedirect`]'s, shared with [`Posture::TotalDeadlineNoRedirect`]) carries
/// none, and must not be given one. **`update_ref` must never move to [`Posture::BoundedReads`]**
/// — nor may the client it already rides acquire a `read_timeout`, which is the same hazard by a
/// shorter route, and the cheaper-looking one now that a client combining no-auto-redirect *with* a
/// silence budget exists to copy from: `update_ref`'s
/// server side legitimately runs a parcel-closure audit walk — scoped by the history segment
/// being pushed, which on a first lift into an empty pallet is the whole history — before its
/// first response byte; that can take minutes with *no* bytes moving at all, which this constant
/// would (correctly) call silence if it applied there. This is not an accident of wiring — FORK-94
/// found `update_ref`'s pre-first-byte cost is a *derived* quantity (an audit walk), unlike
/// `missing_objects`/`upload_targets` and whatever remains ticketed at [`UnboundedTicket::Fork92`],
/// whose priced quantity is the request body's own enumerated content; giving it a flat silence
/// budget before that walk-equivalence question is settled would be exactly the kind of guess this
/// module's bounded clients exist to avoid.
///
/// This is client patience, not a bound derived from any specific head's measured cost: 10s of
/// silence, on top of whichever connect budget applies, is a policy choice about how long this
/// caller waits for the first byte before calling it silence, not a computation of any head's
/// true worst case. Calibration evidence supporting that choice, not deriving it: a healthy
/// connection carrying real progress, however slow the link, essentially never goes a full 10s
/// without delivering a byte; and this module places only calls whose pre-first-byte cost is
/// O(constant) — a single lookup, or serving an already-built file, as `forklift-server`'s own
/// handlers for these three calls happen to do today — onto this budget at all, never a scaled
/// or open-ended one. That classification, not any one head's specific implementation of it, is
/// what makes 10s generous here; a future head whose handler for one of these three calls does
/// unbounded work before its first byte would need to stop riding this budget, not get a bigger
/// number.
const REMOTE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The read/metadata silence budget for [`RemoteClient::fetch_object`] — see
/// [`Posture::BoundedObjectReads`]. Deliberately loose, not tuned to feel responsive: a single
/// object's pre-first-byte cost is bounded by `object_utils::MAX_OBJECT_BYTES` (64MiB) — a
/// constant this crate defines and any head fetches against, a client-known ceiling rather than a
/// specific head's own trait — and a flat budget over that ceiling can be honest, if it is loose
/// enough.
///
/// `objects/batch` is the instructive contrast, and an earlier version of this doc got the contrast
/// wrong: it said that endpoint "stays fully unbounded for exactly this reason: it has no such
/// per-call ceiling." **It has one.** Both heads reject an over-cap batch at `MAX_MISSING_BATCH` —
/// the same protocol constant this file already imports (`forklift-server`'s `post_objects_batch`,
/// `forklift-aws-lambda`'s `Head::reject_oversized_batch`) — so that ceiling is a protocol
/// property, client-known, exactly like `MAX_OBJECT_BYTES`. What actually differs is its size: the
/// implied worst case is a *count* cap times the per-object byte ceiling, 10,000 × 64MiB, which
/// bounds the space and is useless as a budget. No flat silence budget over the *build* is honest —
/// the conclusion the old sentence reached by a false route — which is why nothing about that
/// endpoint's response phase is priced from a response size. Both of its phases are bounded anyway,
/// by mechanisms that price no response size at all: the head-wait by [`BATCH_HEAD_PATIENCE`],
/// before the status line; the body read by this constant, after it, on the terms the next
/// paragraph states.
///
/// Calibration evidence, not derivation: `server.rs`'s `get_object` handler documents that it
/// "buffers the whole object in memory" via `retrieve_object_by_hash`, which content-verifies
/// before returning and, for a packed/delta object, decompresses and reconstructs it in memory
/// too — all inside `blocking(...)`, entirely before `bytes.into_response()` writes a single
/// byte. That is evidence a buffering implementation's worst case at the 64MiB ceiling is well
/// inside 60s, not a computation of it — this client's patience is chosen at 60s for the ceiling
/// regardless of which head serves it, and 60s is not to feel snappy either way.
///
/// Accepted residual, recorded rather than silently absorbed: `server.rs` also documents
/// grandfathered pre-ceiling blobs (from before `MAX_OBJECT_BYTES` existed) as served "whole and
/// genuinely unbounded" — for a multi-gigabyte one, even 60s can be too tight, making it
/// permanently, deterministically unfetchable through this bound. Knowingly accepted for this
/// slice; the root fix (streaming these handlers instead of buffering, removing the size-dependent
/// pre-first-byte phase entirely) is FORK-85.
///
/// Reused unmodified by **both** of `fetch_batch`'s stations, for two different reasons that end
/// in the same place (see that function's own doc for the full reasoning):
///
/// - Its redirect-follow `GET` reads bytes an offloading store already finished writing before it
///   ever presigned the URL, not bytes still being buffered server-side the way this constant's own
///   sizing reasoning above assumes — a strictly easier case than the one this value was tuned for,
///   so the same loose silence budget is at least as defensible there.
/// - Its `POST`'s **body read** — the direct, non-redirect response path — gets it via the
///   client [`Posture::HeadDeadlineNoRedirect`] selects. This budget never prices the bundle build:
///   the head-wait timer is strictly the tighter of the two and always fires first, so by the time
///   this one can matter the status line has already arrived. What it prices is only the gaps
///   *between* body bytes.
///
/// **Why a flat silence budget is honest there at all, stated as a forward-facing contract rather
/// than an observation about today's heads.** Both heads that exist materialize the whole bundle
/// before writing any response byte (`forklift-server`'s `post_objects_batch` awaits a complete
/// `Vec<u8>` and only then builds a response; `forklift-aws-lambda`'s `Head::batch` returns a
/// finished `Vec<u8>` in its non-offloading branch, and in its offloading branch writes that same
/// finished bundle to storage and answers with a redirect URL instead) — so on the direct path
/// headers arriving *implies* the build is done, and mid-body silence has no legitimate server-side
/// cause at all. A future streaming head would reintroduce mid-body gaps, but each gap would be one
/// object's build, bounded by the same per-object ceiling this constant was already sized against.
/// **The clause that binds:** a head that can legitimately exceed one object's build cost of
/// silence mid-body must not be served by this budget — it has to change the posture, not the
/// number.
const FETCH_OBJECT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How long [`RemoteClient::fetch_batch`]'s `POST` waits for the remote to produce a complete
/// status line and header section, *on top of* whichever connect budget applies — see
/// [`RemoteClient::batch_head_budget`], which is what actually gets armed; this constant alone is
/// not it. Carried by [`Posture::HeadDeadlineNoRedirect`], whose external timer wraps the `send()`
/// future, and `send()` resolves the moment the header section arrives — so this bounds the
/// **head-wait alone** (connect, request transmission, the wait for headers) and structurally
/// cannot observe a response body that is still arriving. The body read that follows is bounded by
/// a separate mechanism on the same posture — the client-level silence budget
/// [`FETCH_OBJECT_READ_TIMEOUT`] sizes — so neither phase can wait indefinitely on a silent remote,
/// and the call carries no [`UnboundedTicket`].
///
/// This is client patience, not a derivation of any head's worst case — and the two head shapes it
/// faces support it on two independent grounds:
///
/// - **An API-Gateway-fronted head cannot spend longer than 29s before something answers.** API
///   Gateway enforces a hard, non-configurable 29-second integration timeout on a synchronous
///   Lambda proxy integration, and `infra/aws-serverless`'s own `control_plane_timeout_s` rejects
///   at plan time any Lambda timeout above it (`variables.tf`'s `validation` block;
///   `forklift-aws-lambda`'s `head.rs` names the same ceiling in its own doc). Past it the gateway
///   answers the client itself, which is a *response*, not a hang. 45s clears that with margin, so
///   against such a head this bound never fires on a legitimately slow build — there it protects
///   against a **wedge**, and against nothing else.
/// - **Against a self-hosted `forklift-server`, the *measured* pre-first-byte cost is nowhere near
///   it** — measured, and only measured; see the retraction below for what that does not cover.
///   Time-to-first-byte for `POST /v1/objects/batch` (0.3.0, 3 runs per point):
///   0.4–0.9ms for 1 object / 256KiB; 4.1–13.0ms for 50 objects / 12.8MiB; 14.1–39.3ms for 203
///   objects / 52.5MiB loose, 52ms for the same set after `forklift compact`.
///
/// **What that measurement is not, stated rather than laundered.** It was taken warm-page-cache, on
/// a local SSD, over loopback. **Cold I/O on network-backed storage is unmeasured, and is the
/// plausible worst case for a self-hosted head.** So 45s is not derived from a worst case at all:
/// it was chosen to clear the one *structural* ceiling that exists, and it happens to sit orders of
/// magnitude above the only build cost anyone has measured. The theoretical ceiling —
/// `MAX_MISSING_BATCH` (10,000) objects at `MAX_OBJECT_BYTES` (64MiB) each — bounds the space and
/// is useless as a budget (see [`FETCH_OBJECT_READ_TIMEOUT`]'s doc); it is named here only so the
/// next reader does not re-derive it and mistake it for a sizing argument.
///
/// **Accepted residuals, both of them.** A legitimately slow head now hard-fails identically on
/// every retry rather than being waited out; that knowingly reverses the reason
/// [`RemoteClient::fetch_batch`]'s own doc used to give for staying unbounded, and is accepted
/// because a wedge and an over-budget build are indistinguishable at the client while a loud
/// failure is the lesser wrong. And an expiry abandons the in-flight request, so the head may
/// finish the build anyway and a retry re-pays it — harmless, since the result is ephemeral, but
/// worth saying once. (The third residual an earlier version of this list carried — an unbounded
/// body read — is closed: see [`FETCH_OBJECT_READ_TIMEOUT`]'s doc for the budget that closed it and
/// the terms it is honest on.) The falsifier that reopens **the number, not the mechanism**: a
/// measured legitimate build exceeding this patience against a real deployment.
///
/// **What acting on that falsifier now costs, which it did not when the sentence was written.**
/// These two constants used to be independent; the phase-precedence assertion below has since made
/// this one a *strict* lower bound on [`FETCH_OBJECT_READ_TIMEOUT`]. So raising the patience is
/// free only up to that ceiling — past it the build fails, and the only way through is to raise the
/// silence budget too, which is not local: it also loosens [`RemoteClient::fetch_object`] and the
/// redirect-follow `GET`. Loud rather than silent, but a measurement that lands above the ceiling
/// reopens two constants, not one.
const BATCH_HEAD_PATIENCE: std::time::Duration = std::time::Duration::from_secs(45);

/// **The phase-precedence pin.** `fetch_batch`'s two bounds must fire in a fixed order — the
/// head-wait timer strictly before the client-level `read_timeout` on the same request — and that
/// order is a relationship between these two constants, not a property of any type.
///
/// Both budgets fold in the *same* `connect_timeout` term ([`RemoteClient::batch_head_budget`] adds
/// it to the patience; [`bounded_read_timeout`] adds it to the silence budget), so the whole margin
/// between them is the difference of the bare constants asserted here, whichever connect budget
/// this client instance carries — 5s direct or 60s over Tor alike.
///
/// Two shipped claims rest on this ordering, and both become false the moment it inverts. That no
/// client-level read timeout can fire before the status line, which is what lets
/// [`RemoteClient::head_wait_expired_message`] keep its exact-figure wording and what keeps the
/// body silence budget from ever pricing the server's bundle build. And that a `ReadTimedOut`
/// reaching [`RemoteClient::describe_transport_error`] under [`Posture::HeadDeadlineNoRedirect`]
/// can only be post-header, which is what entitles that arm to say response headers had already
/// arrived. Raising the patience past the silence budget would swap which mechanism fires first and
/// silently falsify both — so it fails the build here instead.
const _: () = assert!(
    BATCH_HEAD_PATIENCE.as_nanos() < FETCH_OBJECT_READ_TIMEOUT.as_nanos(),
    "BATCH_HEAD_PATIENCE must stay strictly under FETCH_OBJECT_READ_TIMEOUT: fetch_batch's \
    head-wait timer has to fire before the client-level read_timeout on the same request, or the \
    head-wait wording and the post-header wording both start lying"
);

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
/// the same way [`bounded_read_timeout`] and [`clients::Clients::send_with_watchdog`]'s own phase
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
///
/// Also reused, unmodified, by [`RemoteClient::resolve`] to size its own
/// [`Posture::TotalDeadline`] payload: the arithmetic — connect budget plus a post-connect
/// allowance — is identical, only what consumes the result differs (a resettable client-level
/// sleep here; a non-resetting per-request total deadline there, since `resolve`'s pre-first-byte
/// risk has nothing yet to "make progress" on for a silence budget to protect).
fn bounded_read_timeout(connect_timeout: std::time::Duration,
                        silence_budget: std::time::Duration) -> std::time::Duration {
    connect_timeout + silence_budget
}

/// How large each chunk handed to `reqwest::Body::wrap_stream` is for an upload's
/// watchdog-guarded body stream (see [`clients::Clients::send_with_watchdog`]). Small enough that the
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
/// before [`clients::Clients::send_with_watchdog`] gives up and abandons the request, *on top of*
/// whichever connect budget applies — same reasoning as [`bounded_read_timeout`]'s: a per-request
/// bound starts before connect too, so the connect allowance is added in rather than layered on
/// blind.
///
/// This is a **different mechanism** from [`REMOTE_READ_TIMEOUT`], deliberately not reused (see
/// this module's "why the read-path tool does not work here" note): `read_timeout` is a flat,
/// non-resetting deadline that covers connect *and* the entire pre-headers send phase, which is
/// exactly wrong for an upload — arming it here would cap the whole upload's total duration and
/// kill a healthy transfer on any link slower than the window it allows. This budget instead
/// governs a hand-rolled watchdog ([`clients::Clients::send_with_watchdog`]) driven by a timestamp
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

/// How often [`clients::Clients::send_with_watchdog`] wakes up to re-check its shared progress
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
/// docs for the constants. This alone is *not* [`clients::Clients::send_with_watchdog`]'s actual
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

/// Ceiling on the server's per-hash presence-check cost that
/// [`RemoteClient::presence_negotiation_budget`] prices into `missing_objects`/`upload_targets`'s
/// total deadline — see that method's own doc for the full arithmetic. An upper bound, not a
/// measured average, on `does_object_exist`'s (`file_utils.rs`) real per-hash cost — the primitive
/// both calls' server side loops over. `pub(crate)`, not `pub` — every sibling budget constant in
/// this file ([`REMOTE_READ_TIMEOUT`], [`UPLOAD_SILENCE_BUDGET`], [`POST_SEND_VERIFY_BASE`]) is
/// module-private; this one only needs to reach `file_utils.rs`'s own FORK-92 measurement spike
/// (`spike_fork92_presence_rate`, `cargo test --release -p forklift-core
/// spike_fork92_presence_rate -- --ignored --nocapture`), which imports this exact constant rather
/// than mirroring its value in a second, unlinked literal — a production change here cannot
/// silently drift out of sync with what that spike actually checks. Same-crate reach is all that
/// import needs, so `pub(crate)` is the right width, not a wider `pub`.
pub(crate) const PRESENCE_ALLOWANCE_MS_PER_OP: f64 = 5.0;

/// The post-connect allowance [`RemoteClient::single_write_budget`] folds `self.connect_timeout`
/// into, for `upload_signature` and `put_trust` — the two single-write endpoints whose server
/// side runs a bounded, known-shape sequence after the body is already in hand rather than the
/// derived, unknowable-in-advance one `update_ref`'s audit walk runs.
///
/// **Client patience, not a server-priced ceiling.** This is how long this caller is willing to
/// wait for one of these two small, idempotent writes to finish before it gives up loudly and in
/// a retry-safe way — a policy choice this client makes about itself, the same category as
/// [`REMOTE_CONNECT_TIMEOUT`]. It is not, and must never become again, an arithmetic expression
/// over some specific head's implementation: an earlier version of this constant computed itself
/// as `2 * HOOK_CLIENT_TIMEOUT + 5s`, importing a constant whose entire meaning was
/// `forklift-server`'s own per-hook allowance — wrong even against that one head, since
/// `forklift-server`'s hooks are deployment-optional, and structurally wrong against
/// `forklift-aws-lambda`, which serves both these routes and runs no hooks at all. `HOOK_CLIENT_TIMEOUT`
/// now lives in `forklift-server` alone, precisely so a `forklift-core` budget cannot reach it
/// without adding a dependency this workspace does not have.
///
/// **What the committed evidence actually bounds — and why it does not size this constant.** The
/// 25s value is unchanged from before this doc was rewritten (verified bit-identical at
/// `forklift-server`'s 10s hook timeout at the time of the change). The facts that originally
/// motivated it turn out not to support the ceiling they were used to justify:
/// `forklift-server`'s committed suite asserts each of its hooks fails closed within
/// **`[8s, 15s)`** against a hook that never answers
/// (`tests::an_authentication_hook_that_never_answers_fails_closed_near_the_hook_timeout` and its
/// admission sibling, in `forklift-server/src/server.rs`). Those tests' own doc comments name a
/// ~10.00s production figure, but no assertion pins it, so it is not something this doc may lean
/// on. `upload_signature`'s handler runs that pair in sequence and **no test measures the pair**,
/// so the only pair bound the suite establishes is the sum of the individual upper bounds:
/// **under 30s, which 25s does not clear.**
///
/// That is deliberately left standing rather than resolved by raising the number. A client budget
/// sized to clear some head's hook pair would be the very derivation this constant exists to stop
/// — and it could not succeed anyway, since the pair bound is a property of one head's
/// configuration and the residuals below are unbounded regardless. 25s is how long *this client*
/// waits; a head that needs longer is a head this client gives up on, loudly and retry-safely.
///
/// **Named residuals — a bounded list of the ones this fix found, not a claim that no others
/// exist:**
///
/// - `put_trust` holds `warehouse.writes` for the whole of its handler — the same mutex the
///   ref-update handler holds across closure verification, ancestry and the office-chain verify,
///   work this module elsewhere documents can legitimately run minutes. A first-contact `put_trust`
///   racing another client's long first lift on the same warehouse exceeds this budget regardless
///   of its size; see [`RemoteClient::put_trust`]'s own doc for why that is accepted.
/// - `upload_signature`'s handler (on `forklift-server`) reaches `office_utils::read_office_state`,
///   which loads one object per user record and one per key record — O(roster), not O(constant),
///   so a large roster shifts it without bound.
/// - `forklift-server`'s hook-bound handlers run their work on its shared `spawn_blocking` pool,
///   the same pool the minutes-long ref-update verification occupies there. Queue wait is
///   unbounded and priced nowhere in this budget.
/// - `forklift-aws-lambda` serves both these routes too (`Head::signature_put`/`Head::put_trust`)
///   with no hook concept at all — none of the hook-timing evidence above applies to it. What its
///   handlers spend instead — Lambda cold start, plus the S3 (`S3ObjectStore`) and DynamoDB
///   (`DynamoRefStore`) round trips backing `ObjectStore`/`RefStore` — has no calibration evidence
///   behind this budget at all; this constant applies to that head only as an uncalibrated
///   uniform policy, not a tuned one.
///
/// The consequence of any of these is a mutation reported as uncertain-outcome when it in fact
/// succeeded. Both endpoints are idempotent, so the recovery is an ordinary retry — but a retry
/// against the *same* condition fails identically, since this budget is fixed rather than adaptive.
const SINGLE_WRITE_ALLOWANCE: std::time::Duration = std::time::Duration::from_secs(25);

/// Shared state between an upload's body-send stream ([`UploadChunks`]) and
/// [`clients::Clients::send_with_watchdog`]'s polling loop: a timestamp updated every time the stream
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
/// budget [`clients::Clients::send_with_watchdog`] checks `silent_for()` against, not whether it
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
    /// [`clients::Clients::send_with_watchdog`] compares against the budget.
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
/// buffer may hold more on top of that — see [`clients::Clients::send_with_watchdog`]'s doc (review
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

/// The private home of [`RemoteClient`]'s `reqwest::Client`s, and the only way to reach any
/// of them. How many there are is [`Clients`]'s own field list, deliberately not restated here:
/// the count has changed twice and a number in prose is the first thing to rot.
///
/// **Why this module exists.** Which client a call used to use — and therefore whether its
/// response wait was bounded at all — used to be decided by which field name a caller happened to
/// type (`self.http`, `self.no_redirect`, `self.bounded_reads`, `self.bounded_object_reads` — the
/// four that existed then), every one of them reachable from every method on `RemoteClient`.
/// Enumerating "which calls ride the unbounded
/// ones" by reading the source got tried three times and got three different answers (two, then
/// four, then eight) — and the eight-count parser, which matched a fixed list of spellings, still
/// missed a call built by handing a client straight to the request-builder helper rather than going
/// through the one convenience spelling the parser recognized. A fixed list of spellings cannot see
/// a spelling it was not written for.
///
/// So the fields are private to this module, and [`Clients::pick`] — the only function that reads
/// them — is private too, reachable only from inside `mod clients` itself. The module hands out
/// exactly two operations instead, and neither of them hands out a client, or a builder, or
/// anything else a caller could send by hand: [`Clients::send_on`], which takes a [`Posture`] — a
/// required argument, not a default — plus a [`RequestDestination`], a method and a [`SendBody`],
/// applies that posture's own payload (currently [`Posture::TotalDeadline`]'s or
/// [`Posture::TotalDeadlineNoRedirect`]'s total deadline, or
/// [`Posture::HeadDeadlineNoRedirect`]'s head-wait budget) on whichever client `pick` chose, sends
/// the request itself, and returns a [`SendOutcome`]; and
/// [`Clients::send_with_watchdog`], which takes the upload's bytes and the pieces of a request,
/// builds the watchdog-guarded body and the request itself, and returns a [`WatchdogOutcome`].
/// Neither function can be skipped in favor of
/// reaching `pick` directly, because nothing outside `mod clients` can name `pick` at all — that is
/// a privacy error, not a convention. A new call site cannot reach any client, nor send a request
/// of any kind, without the compiler forcing it through one of these two, payload-
/// applying paths; there is no ambient "just use the default one" path left to fall into, and no
/// lower-level escape hatch left standing beside them either.
///
/// **Why both of them own `send()` rather than returning a builder.** A `reqwest::RequestBuilder`
/// handed back to a call site is a request this module has already stopped governing: the only
/// bound it can still carry is whatever was attached before the hand-off, and the in-flight
/// `send()` future — the only thing an outer bound could wrap, or a watchdog could race — belongs
/// to whoever calls it. [`Clients::send_with_watchdog`] needed that future first, and holds it in
/// a `select!` beside its own poll loop. [`Clients::send_on`] holds it for the same structural
/// reason, and [`Posture::HeadDeadlineNoRedirect`] is what that ownership immediately bought: a
/// bound that stops at the status line is a timer wrapped around the `send()` future, which is not
/// something a call site holding its own future could ever have been given. The other gain is
/// where a payload can be applied: at the exhaustive match over [`Posture`] here, rather than only
/// in whatever a builder can be told before it leaves. The cost is that a body has to be described rather than
/// chained on — see [`SendBody`], which is also where the one behaviour that description had to
/// reproduce by hand is recorded.
///
/// **The residual trust base is this module, full stop — nothing outside it.** That used to
/// require two named exceptions (`RemoteClient`'s own builder-returning wrapper and a
/// since-deleted `client_for`, which handed back the bare `&reqwest::Client` itself for the one
/// call site that must not attach a bearer token); both were replaced by
/// [`RemoteClient::send_on`]/[`RemoteClient::send_on_presigned`], which forward to
/// [`Clients::send_on`] and return a [`SendOutcome`], so neither ever touches a `reqwest::Client`
/// — or a `reqwest::RequestBuilder` — at all. The watchdog-guarded
/// uploads (`upload_object`, `put_presigned`) were already outside the trust base the same way. That
/// the external base is empty, not merely small, is a procedural claim, not an assertion to take on
/// faith: `grep -n 'reqwest::Client\b' crates/forklift-core/src/util/remote_utils.rs` and
/// `grep -n 'reqwest::RequestBuilder' crates/forklift-core/src/util/remote_utils.rs`, each read
/// against which lines are doc comments, currently find either type spelled outside a comment only
/// inside `mod clients` itself — nowhere else in the file. Domain: this file, at this commit;
/// re-run both greps to re-check the claim, and do not trust this sentence past the next edit that
/// moves either type.
///
/// **Physical clients, three independent axes.** *Redirect policy* (mutations, and the
/// one-shot streamed-upload bodies, must never auto-follow; reads may). *Whether a read/metadata
/// silence bound applies at all* (some carry a client-level `read_timeout`, some deliberately do
/// not; see [`REMOTE_READ_TIMEOUT`]'s doc for why the rest don't have an honest flat budget yet).
/// *How loose that bound is*, for the ones that do ([`FETCH_OBJECT_READ_TIMEOUT`] instead of
/// [`REMOTE_READ_TIMEOUT`] — see that constant's doc). The axes are genuinely independent, and the
/// current set proves it rather than merely asserting it: one client combines no-auto-redirect
/// *with* a client-level `read_timeout`, which no client did until `fetch_batch`'s `POST` needed
/// both at once. Which client has which combination is [`Clients`]'s field docs, and only
/// there — an enumeration in this paragraph is exactly the thing that goes stale the next time a
/// call needs a combination nothing yet builds. Every client shares the
/// same proxy/connect-timeout configuration, built once by [`Clients::build`] (called from
/// [`RemoteClient::new_with_tor`]), which is also where each bounded client's actual `read_timeout`
/// is computed via [`bounded_read_timeout`] rather than being the raw silence-budget constant.
mod clients {
    use super::{bounded_read_timeout, FETCH_OBJECT_READ_TIMEOUT, REMOTE_READ_TIMEOUT};

    /// Which `reqwest::Client` a request rides, and — since a client-level `read_timeout` alone
    /// does not cover every phase of every call — what else (if anything) bounds that call's
    /// response wait: [`Self::TotalDeadline`]'s or [`Self::TotalDeadlineNoRedirect`]'s own payload,
    /// spanning the whole response; [`Self::HeadDeadlineNoRedirect`]'s, spanning only the wait for
    /// the status line and headers, with its client's own silence budget taking over after them;
    /// or nothing at all for
    /// [`Self::UnboundedFollowsRedirects`]/[`Self::UnboundedNoRedirect`] (see
    /// [`UnboundedTicket`]'s doc). Every request [`super::RemoteClient`] sends through
    /// [`Clients::pick`] must name one of
    /// these explicitly; `pick` has no other argument to construct a request from, so there is
    /// nothing to fall back to. The watchdog-guarded uploads bound their own response wait a
    /// different way — see [`Clients::send_with_watchdog`]'s own doc — and so never construct a
    /// `Posture` at all; there is no variant here standing in for that mechanism to keep in sync
    /// with it.
    ///
    /// `#[derive(Clone, Copy)]`: [`super::RemoteClient::describe_transport_error`] takes a
    /// `Posture` by value, since it reports the exact posture a call was actually armed with —
    /// every call site needs to arm a request with a posture and then hand that same posture to
    /// the composer, and `Copy` is what lets a call site bind one local and use it twice (once
    /// arming the request through [`Clients::send_on`], once handing the outcome to
    /// [`super::RemoteClient::response_from_send`]) without a borrow fight. Nothing here
    /// derives `Debug`/`PartialEq`: no code compares or prints a `Posture`, and a derive whose
    /// stated purpose has no caller is exactly the kind of unenforced claim this module avoids.
    #[derive(Clone, Copy)]
    pub(super) enum Posture {
        /// [`REMOTE_READ_TIMEOUT`]-bounded via a client-level `read_timeout`. For the
        /// O(constant)-pre-first-byte calls: `fetch_info`, `fetch_signature`, `fetch_bundle_to`.
        ///
        /// **Auto-follows redirects — never select this for a mutation.** The client this
        /// selects carries no `redirect::Policy::none()`, for the same reason [`Clients::http`]
        /// doesn't: nothing about adding a `read_timeout` also disables auto-follow, they are
        /// independent axes (this module's own doc names all three). A mutation that rode this
        /// posture would reopen exactly the hole FORK-89 closed — a `3xx` silently followed and
        /// read back as success. So reaching for *this* variant to "just add a bound" to
        /// `upload_signature`/`put_trust` would silently reopen their redirect exposure; they
        /// carry their bound as a per-request payload on [`Self::TotalDeadlineNoRedirect`]
        /// instead.
        ///
        /// **A client combining no-auto-redirect *with* a client-level `read_timeout` now
        /// exists** — the one [`Self::HeadDeadlineNoRedirect`] selects. An earlier version of this
        /// paragraph said none did and used that to rule the combination out for
        /// `upload_signature`/`put_trust`; that route is closed, and anyone reopening their
        /// still-live "should these carry a silence bound too" question must not be sent down it.
        /// The reason those two do not have one is unchanged and has nothing to do with which
        /// clients exist: `read_timeout` resets on every byte, so it cannot bound a remote that
        /// answers a mutation with an endless trickle — see
        /// [`super::tests::resolve_gives_up_on_a_remote_that_never_stops_trickling`] for that exact
        /// failure mode driven end to end on a different call. A silence budget *in addition to*
        /// their total deadline is now constructible; whether it would buy anything is open.
        BoundedReads,
        /// [`FETCH_OBJECT_READ_TIMEOUT`]-bounded via a client-level `read_timeout` — the same
        /// shape as [`Self::BoundedReads`], just with the looser budget `fetch_object`'s
        /// size-dependent server work needs. `fetch_object`, and `fetch_batch`'s redirect-follow
        /// `GET` — see [`FETCH_OBJECT_READ_TIMEOUT`]'s doc for why the latter shares it.
        ///
        /// **Auto-follows redirects — never select this for a mutation**, same reasoning and
        /// same trap as [`Self::BoundedReads`]'s own doc.
        ///
        /// Shares its silence budget, but not its client, with
        /// [`Self::HeadDeadlineNoRedirect`] — that one needs the same figure on a client that
        /// never auto-follows. Both therefore render the same duration when a read timeout fires,
        /// which is why [`super::RemoteClient::describe_transport_error`]'s two arms have to be
        /// told apart by their words rather than by their figures: `fetch_batch` routes both of
        /// its stations through one composer call with one action string, so the figure alone
        /// cannot say which of them stalled.
        BoundedObjectReads,
        /// Auto-follows redirects; no client-level `read_timeout`. The bound is the payload
        /// itself — a genuine *total* per-request deadline (`RequestBuilder::timeout`) that
        /// [`Clients::send_on`] reads off this variant and applies unconditionally, so the
        /// promise is discharged by the module, not merely asserted at the call site the way the
        /// deleted `OwnTimeoutFollowsRedirects` posture used to (that variant carried no payload at
        /// all; nothing checked that any caller of it actually called `.timeout(...)`) — and the
        /// way the since-deleted `client_for` reopened one layer down, by handing back a bare
        /// client that skipped this extraction entirely for its one caller. `resolve`,
        /// `missing_objects`, and `upload_targets` — the latter two sized per call by
        /// [`super::RemoteClient::presence_negotiation_budget`] rather than `resolve`'s own fixed
        /// `connect_timeout + REMOTE_READ_TIMEOUT`; see that method's own doc for why a *total*
        /// deadline is sound for them specifically, the identical reasoning `resolve`'s own doc
        /// gives below but grounded in a request-body-enumerated cap instead of cosmetic-sugar
        /// disposability.
        ///
        /// A *silence* budget ([`Self::BoundedReads`]) is not a substitute — but not for the
        /// reason "resolve has nothing to move before headers arrive": `BoundedReads`'s own
        /// pre-header deadline is fixed and non-resetting (see [`REMOTE_READ_TIMEOUT`]'s own
        /// doc), so it already terminates correctly against a remote that goes fully silent and
        /// never answers at all — confirmed directly, not assumed: probed a
        /// [`Self::BoundedReads`] request against a permanently silent remote and it failed with
        /// a genuine timeout at 15.003s, no hang. What a silence budget cannot catch is the
        /// opposite shape: a remote that *does* start answering and then trickles bytes slowly,
        /// forever, resetting the clock on every byte (this file's own settled contract: a
        /// transfer that is moving bytes, however slowly, is never silence). For
        /// `fetch_info`/`fetch_signature`/`fetch_bundle_to` that tolerance is the whole point — a
        /// real, slow transfer must never be killed. For `resolve` it is a liability instead: the
        /// call is cosmetic display sugar whose fallback (pseudonyms) is free, so there is no
        /// reason to accept an unbounded wait for a slow-but-real trickle the way the other three
        /// do. `resolve` carries `connect_timeout + REMOTE_READ_TIMEOUT` — see
        /// [`super::RemoteClient::resolve`]'s own doc for the residual this accepts, and
        /// [`super::tests::resolve_gives_up_on_a_remote_that_never_stops_trickling`] for the
        /// falsifying test — it calls `resolve` itself against the trickling shape, unlike
        /// `super::tests::send_on_applies_a_total_deadline_that_ignores_progress`, which pins
        /// only the seam's own wiring and never calls `resolve`.
        TotalDeadline(std::time::Duration),
        /// Same shape as [`Self::TotalDeadline`] — a genuine *total* per-request deadline, applied
        /// the same way by [`Clients::send_on`] — but on the client that never auto-follows a
        /// redirect, for a mutation that also needs a bound. `upload_signature` and `put_trust`,
        /// against `forklift-server`: single-write endpoints whose server side runs a fixed number
        /// of individually-capped hooks after the body is already in hand — not O(constant) file
        /// I/O overall, since `upload_signature`'s handler also reaches an O(roster) office-state
        /// read and `put_trust` can hold the warehouse write lock for as long as a concurrent
        /// ref-update runs — so a total deadline is honest as a price on the hook sequence, not as
        /// a ceiling on the whole handler; see [`super::SINGLE_WRITE_ALLOWANCE`]'s own doc for the
        /// arithmetic and the residuals it does not price, including the second head that runs no
        /// hooks at all. But the mutation invariant still applies: neither
        /// may ride [`Self::TotalDeadline`] itself, since that variant is carried on
        /// [`Clients::http`], the auto-following client, and a mutation riding it would reopen the
        /// exact silent-`3xx`-success hole FORK-89 closed (see [`Clients::pick`]'s own doc for
        /// which client each variant selects). Sized by
        /// [`super::RemoteClient::single_write_budget`] — see that method's own doc for the
        /// arithmetic and the accepted residual.
        TotalDeadlineNoRedirect(std::time::Duration),
        /// Never auto-follows a redirect, **and** carries a client-level `read_timeout` — the one
        /// posture whose client combines both axes, and the reason a client had to exist for it
        /// (neither existing client could be given the axis it lacked: the no-redirect client is
        /// shared with `update_ref`'s posture, and that call must never carry a silence budget;
        /// the loose bounded-reads client genuinely needs auto-follow, since an offloading head's
        /// object endpoint can answer with a redirect to storage).
        ///
        /// **Two mechanisms, two phases, in a fixed order.** `head` bounds the **head-wait alone**
        /// — connect, request transmission, and the wait for a complete status line and header
        /// section: [`Clients::send_on`] arms it as an external `tokio::time::timeout` around the
        /// `send()` future, and `send()` resolves the moment the header section has arrived, so
        /// that timer structurally cannot observe a body that is still coming. Everything after
        /// the header section is the client's own `read_timeout`, a **silence** budget that resets
        /// on every byte received — so a large healthy bundle is never killed by it however slow
        /// the link, and only a genuinely stalled transfer fails. That is the whole reason this
        /// variant exists rather than `fetch_batch`'s `POST` simply moving to
        /// [`Self::TotalDeadlineNoRedirect`]: a *total* deadline over a response whose size the
        /// request does not bound would kill large healthy bundles, identically on every retry.
        ///
        /// **Read what that does and does not promise.** No phase can wait indefinitely on a
        /// *silent* remote. The call's duration is still not bounded: a remote delivering one byte
        /// per sub-budget interval keeps this request alive forever, which is the deliberate price
        /// of a silence budget and not a gap to close by swapping in a total deadline.
        ///
        /// The order of the two is not incidental — the head timer is always the tighter, pinned
        /// at the constants (see [`super::BATCH_HEAD_PATIENCE`]'s doc for the assertion and the two
        /// shipped sentences that rest on it). A producer arming a `head` at or above the client's
        /// own `read_timeout` would break both: [`check_head_deadline_payload`] is the tripwire.
        ///
        /// A head-wait expiry is **not** a `reqwest::Error`. It is [`SendOutcome::HeadWaitExpired`],
        /// which never reaches [`super::classify`] — so `is_timeout()`'s inability to see it is
        /// discharged by construction rather than by a rule someone has to remember; there is no
        /// path that could ask. Dually, at every expiry the connection is known established (see
        /// [`clamp_head_deadline_payload`]), which is what entitles
        /// [`super::RemoteClient::head_wait_expired_message`] to say so. A fired `read_timeout`
        /// *is* an ordinary `reqwest::Error` and does reach `classify`, where this posture's arm
        /// in [`super::RemoteClient::describe_transport_error`] gets wording of its own.
        ///
        /// **This variant carries no [`UnboundedTicket`], and that is the change worth noticing.**
        /// It used to, for a `body_read` field naming the residual its head-wait bound left open;
        /// closing that residual is what removed the field. `grep 'UnboundedTicket::'` still
        /// enumerates every call with an unbounded response phase, and this call is simply no
        /// longer one of them.
        ///
        /// **Only the no-redirect twin exists.** Its one consumer, `fetch_batch`'s `POST`, rides
        /// a no-auto-follow client so that a `307`/`308` cannot re-`POST` its body at a URL
        /// presigned for `GET` only (see [`super::RemoteClient::fetch_batch`]'s own doc), and
        /// bounding either phase must not disturb that. *Bounded negative:* head-deadline
        /// variants number exactly one; the domain is this enum; re-check by reading it. The
        /// trigger for minting the follows-redirects twin is a consumer appearing — most
        /// plausibly `fetch_subtree` gaining a production caller.
        HeadDeadlineNoRedirect {
            head: std::time::Duration,
        },
        /// Auto-follows redirects; no client-level `read_timeout`; **nothing else bounds this
        /// call's response wait either** — no per-request override, no watchdog. Deliberate, not
        /// an oversight: the ticket that owns adding a bound is a required part of this variant,
        /// not a comment beside it, so a reviewer sees it in any diff that constructs one.
        ///
        /// The payload is never pattern-matched out — [`Clients::pick`] routes on the variant
        /// alone, not on which ticket it carries — so it is `#[allow(dead_code)]` rather than
        /// read. That is the point: this field's whole job is to be *present in the source*, not
        /// consumed by anything at runtime.
        #[allow(dead_code)]
        UnboundedFollowsRedirects(UnboundedTicket),
        /// Same as [`Self::UnboundedFollowsRedirects`], but on the client that never auto-follows
        /// a redirect — every currently-unbounded mutation reaches this variant, since a mutation
        /// must never silently follow a `3xx` (see [`super::RemoteClient::describe_mutation_redirect`]'s
        /// doc) regardless of whether its response wait also happens to be bounded.
        #[allow(dead_code)]
        UnboundedNoRedirect(UnboundedTicket),
    }

    /// The ticket that owns bounding a call's still-unbounded response wait — a required payload
    /// of the **two** [`Posture`] variants that have one
    /// ([`Posture::UnboundedFollowsRedirects`], [`Posture::UnboundedNoRedirect`]), not an optional
    /// annotation. A closed set, not a free
    /// string: every variant here is a call still genuinely unbounded, recorded
    /// as-is (a later PR's job is to shrink this set). Adding a new variant is itself the visible,
    /// greppable act of accepting a new unbounded call site.
    ///
    /// **A recorded erosion that resolved without ever firing.** While
    /// [`Posture::HeadDeadlineNoRedirect`] was bounded up to the status line and unbounded after
    /// it, it carried a ticket for that residual — and the compiler could force the payload at its
    /// construction sites without forcing the *next* partially-bounded posture to declare one at
    /// all. The recorded trigger was to hoist the ticket into an `Option`-typed field on every
    /// variant once a **second** partially-bounded posture appeared. No second one ever did; the
    /// first stopped being partially bounded instead (both of its phases now refuse to wait
    /// indefinitely on a silent remote — which is not the same as bounding the call's duration,
    /// see that variant's own doc). The trigger stands unchanged for whenever a genuinely
    /// partially-bounded variant next appears.
    ///
    /// `grep 'UnboundedTicket::'` enumerates every call with an unbounded response phase, full
    /// stop — but **run it and you get more lines than there are such calls**, and the difference
    /// is not a defect in either. Only the constructions outside the test module and outside doc
    /// comments are call sites; the rest are this file talking about the ticket. Read the hits
    /// against those two boundaries rather than counting them. Its domain: every construction site
    /// of those two variants — exactly the variants
    /// whose response wait is unbounded, and both require this
    /// payload. What makes that exhaustive is *not* merely that [`Clients::pick`]'s own match over
    /// `Posture` is exhaustive — `pick` only chooses a *client*, it says nothing about whether a
    /// payload-carrying variant's payload actually gets applied before the request goes out. A call
    /// site that could reach `pick` directly, bypassing payload application, could hold a
    /// bounded-looking posture like [`Posture::TotalDeadline`] and still be unbounded in practice,
    /// with no `UnboundedTicket` construction anywhere for this grep to find — not a hypothetical:
    /// the deleted `client_for` handed back a bare client for exactly this reason, for its one
    /// caller, and a payload-carrying posture routed through it would have hung forever unticketed.
    /// What actually closes the set now is that [`Clients::pick`] is private to `mod clients`,
    /// reachable only from [`Clients::send_on`] — the sole place a `Posture` ever becomes a
    /// request on the wire, and the one place that applies an exhaustive match (mirroring `pick`'s own)
    /// to read off any payload a variant carries before sending it. Reaching a client at all means
    /// having gone through that match, so a payload-carrying variant can no longer go unbounded by
    /// way of a second, competing path, and a call with an unbounded response phase can only be
    /// `UnboundedFollowsRedirects`/`UnboundedNoRedirect`, both
    /// requiring this ticket by construction.
    /// [`Clients::send_with_watchdog`] closes the analogous gap for uploads the same way it always
    /// did: by never constructing a `Posture` at all, so its own bound (the watchdog) needs no
    /// representation here either. Before trusting the grep again, re-check both conditions: that
    /// `Clients::pick` is still private (no `pub` on its `fn pick` line), and that every way to send
    /// a request still goes through [`Clients::send_on`] or [`Clients::send_with_watchdog`] —
    /// or re-derive by hand whether some new path is actually bounded. The leak detector for the
    /// second condition is
    /// `grep -n '\.send()' crates/forklift-core/src/util/remote_utils.rs`, read against which
    /// lines are comments: every remaining hit must fall inside `mod clients`, including in the
    /// test module. It guards the floor rather than the property —
    /// a *third* send entry point added to this module would leave it green — so the enumeration
    /// that actually stands for "exactly two ways onto the wire" is the sentence above naming
    /// both, and a third would have to be added to it to be reachable at all.
    ///
    /// The set this enumerates is meant to reach zero: every call ticketed here eventually earns
    /// a real budget and moves off [`Posture::UnboundedFollowsRedirects`]/
    /// [`Posture::UnboundedNoRedirect`] onto a bounded posture instead. When the last variant is
    /// removed, delete this enum and the two fully-unbounded `Posture` variants in the same change
    /// — they exist only to carry this payload and mean nothing without it.
    /// [`Posture::HeadDeadlineNoRedirect`] is the worked example of the other exit: it dropped its
    /// ticket and kept its bounds, because it stopped being unbounded rather than being deleted.
    /// Until then,
    /// whatever variants remain are a live gap awaiting a budget, not settled design to leave
    /// standing.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum UnboundedTicket {
        /// `fetch_subtree`, and nothing else — it is the last call still on this ticket.
        /// FORK-92's current keystone
        /// (narrowed after review, 2026-08-01): the priced quantity **equals the
        /// server's workload by construction** — the request body enumerates it (a hash list), and
        /// the server iterates that same list under a cap both sides share
        /// (`MAX_MISSING_BATCH`/`MAX_UPLOAD_TARGETS_BATCH`), or (for `fetch_subtree`) is
        /// worst-case-bounded by the equivalent shared cap. That is *why* a scaled budget is
        /// fixable here at all — not (as an earlier framing had it) merely that the work "scales
        /// with input the client cannot size in advance": the client can size it, from its own
        /// request. No such budget has landed for `fetch_subtree`, so it stays unbounded.
        ///
        /// `missing_objects` and `upload_targets` shared this exact keystone and were the first to
        /// cash it in: they moved off this ticket onto [`super::Posture::TotalDeadline`],
        /// carrying [`super::RemoteClient::presence_negotiation_budget`] — see that method's own
        /// doc for the budget and [`super::PRESENCE_ALLOWANCE_MS_PER_OP`] for the rate it prices
        /// in.
        ///
        /// **Both of `fetch_batch`'s stations have since left this ticket, by two different
        /// routes, and neither route was "a scaled budget landed after all."** Its `POST` was
        /// filed here for the same reason `fetch_subtree` still is — a bundle built entirely into
        /// memory before the first byte, at a cost the client cannot size. It is off the ticket
        /// because both of its phases now refuse to wait indefinitely on a silent remote without
        /// pricing that build at all: a head-wait timer before the status line
        /// ([`super::Posture::HeadDeadlineNoRedirect`], sized by
        /// [`super::RemoteClient::batch_head_budget`]) and a resetting silence budget after it, on
        /// that posture's own client. Its follow-up `GET` — taken only when the head redirects to
        /// storage — was filed here as an open attribution question instead: it carries no request
        /// body to enumerate anything, so this keystone never applied to it as written, and it had
        /// no other ticket to belong to. Resolved without inventing a scaling term for it: an
        /// offloading store finishes writing the bundle to its response key and only *then*
        /// presigns the `GET` URL (`crates/forklift-aws-lambda/src/aws/s3.rs`'s
        /// `offload_response`), so the bytes it reads are already fully materialized — the same
        /// shape [`super::RemoteClient::fetch_object`]'s read is, and it rides
        /// [`super::Posture::BoundedObjectReads`] accordingly. See
        /// [`super::RemoteClient::fetch_batch`]'s own doc for the full reasoning on both.
        Fork92,
        /// `update_ref`: unbounded for a different reason than the calls FORK-92 collected — its
        /// server-side
        /// cost is a *derived* quantity (an audit walk over the pushed history segment), not one
        /// the request enumerates, so it was split out rather than sharing that mechanism.
        /// FORK-94.
        Fork94,
        /// `commit_lift`: its error-body read decides retry-vs-terminal control flow, so bounding
        /// it is a retry-contract design question, not a flat constant. FORK-91.
        Fork91,
    }

    /// The `reqwest::Client`s a request against the remote may ride — private to this
    /// module; [`Clients::pick`] is the only way any of them leaves it. The fields below are the
    /// enumeration; no count is restated anywhere, because the axes each field names are what a
    /// reader actually needs and a number is what goes stale.
    #[derive(Clone)]
    pub(super) struct Clients {
        /// Auto-follows redirects (reqwest's default `redirect::Policy`); no client-level
        /// `read_timeout`, so once past connect it waits out any silence, however long — unless a
        /// per-request `RequestBuilder::timeout(...)` is layered on top, which
        /// [`Clients::send_on`] does for [`Posture::TotalDeadline`] alone. Selected
        /// by [`Posture::UnboundedFollowsRedirects`] and [`Posture::TotalDeadline`].
        ///
        /// **`commit_lift` is a mutation that rides this client** — the one FORK-89 left
        /// unfixed. See [`super::RemoteClient::commit_lift`]'s own doc for the standing gap this
        /// is: auto-following exposes it to the same silent-3xx-success hole FORK-89 closed for
        /// every other mutation in this module.
        http: reqwest::Client,
        /// Same endpoint as [`Self::http`], automatic redirect-following disabled
        /// (`redirect::Policy::none()`); no `read_timeout` either. Selected by
        /// [`Posture::UnboundedNoRedirect`] and [`Posture::TotalDeadlineNoRedirect`] — the second
        /// layers a per-request total
        /// deadline on top via the same [`Clients::send_on`] extraction [`Posture::TotalDeadline`]
        /// uses; never a client-level `read_timeout` — and directly (bypassing
        /// [`Posture`] entirely) by
        /// [`Clients::send_with_watchdog`] — the watchdog-guarded upload paths: reqwest's default
        /// policy replays a `307`/`308` with the original method *and* body, and `tower-http`'s
        /// `SEE_OTHER`/`MOVED_PERMANENTLY`/`FOUND` arms force a mutation's method to `GET` and
        /// body empty — either way a redirect must come back raw rather than being auto-followed
        /// (FORK-89; see [`super::RemoteClient::describe_mutation_redirect`]'s doc). **Not every
        /// mutation rides this client**: `commit_lift` does not — see [`Self::http`]'s own doc
        /// and [`super::RemoteClient::commit_lift`]'s for that standing,
        /// deliberately-not-silently-closed gap.
        ///
        /// **This client must never be given a `read_timeout`, and that is the sharpest rule in
        /// this struct.** `update_ref` rides it, and `update_ref`'s server side legitimately runs
        /// a parcel-closure audit walk that can take minutes with no bytes moving at all (see
        /// [`REMOTE_READ_TIMEOUT`]'s doc) — a silence budget here would abandon a healthy push
        /// mid-audit, identically on every retry. The temptation is real and now cheaper-looking:
        /// [`Self::bounded_object_reads_no_redirect`] exists precisely because a call needed
        /// no-auto-redirect *and* a silence budget, and adding one line here looks like the same
        /// thing without a new field. It is not. Pinned by
        /// [`super::tests::update_ref_stays_unbounded_against_the_loose_silence_budget`], which is
        /// sized against the loose budget on purpose — the tight-scaled `update_ref` pin beside it
        /// cannot see this mistake at all.
        no_redirect: reqwest::Client,
        /// Same endpoint as [`Self::http`], plus a `read_timeout` of
        /// [`bounded_read_timeout`]`(connect_timeout, `[`REMOTE_READ_TIMEOUT`]`)`. Selected only
        /// by [`Posture::BoundedReads`].
        bounded_reads: reqwest::Client,
        /// Same endpoint as [`Self::http`], plus a `read_timeout` of
        /// [`bounded_read_timeout`]`(connect_timeout, `[`FETCH_OBJECT_READ_TIMEOUT`]`)` — the
        /// same shape as [`Self::bounded_reads`], just with the looser silence budget
        /// `fetch_object`'s size-dependent server work (and, sharing the same budget,
        /// `fetch_batch`'s redirect-follow `GET`) needs. Selected only by
        /// [`Posture::BoundedObjectReads`].
        bounded_object_reads: reqwest::Client,
        /// The two axes no other client combines: automatic redirect-following disabled
        /// (`redirect::Policy::none()`, exactly as [`Self::no_redirect`] has it) **plus** a
        /// `read_timeout` of
        /// [`bounded_read_timeout`]`(connect_timeout, `[`FETCH_OBJECT_READ_TIMEOUT`]`)` (exactly as
        /// [`Self::bounded_object_reads`] has it). Selected only by
        /// [`Posture::HeadDeadlineNoRedirect`], whose one consumer — `fetch_batch`'s `POST` —
        /// needs both at once: it must not let a `307`/`308` replay its body at a `GET`-only
        /// presigned URL, and its response body must not be able to stall forever.
        ///
        /// **Its own client rather than one more line on an existing one**, because neither
        /// existing client could be given the axis it lacks. [`Self::no_redirect`] cannot take a
        /// `read_timeout`: `update_ref` rides it, and that call must never carry a silence budget
        /// (see that field's own doc). [`Self::bounded_object_reads`] cannot drop auto-follow: an
        /// offloading head's object endpoint answers `fetch_object` with a redirect to storage,
        /// which that client is expected to follow. And `read_timeout` is a `ClientBuilder`-level
        /// setting with no per-request override, so there is no third option where one client
        /// carries the budget for one call and not another.
        ///
        /// Reuses [`FETCH_OBJECT_READ_TIMEOUT`] rather than minting a tighter constant, and the
        /// reuse is load-bearing twice over: it is what makes the budget honest for a bundle body
        /// (see that constant's doc) and it is what keeps the head-wait timer strictly tighter than
        /// this client's `read_timeout` (see [`super::BATCH_HEAD_PATIENCE`]'s doc for the pinned
        /// ordering). A smaller constant here would satisfy neither.
        bounded_object_reads_no_redirect: reqwest::Client,
    }

    impl Clients {
        /// Build every client from one shared connect-timeout/proxy configuration. See
        /// [`super::RemoteClient::build`]'s own doc for why one `connect_timeout` must reach every
        /// client rather than being decided by one path and overwritten on a field by another.
        pub(super) fn build(connect_timeout: std::time::Duration,
                            proxy: Option<&reqwest::Proxy>) -> Result<Clients, String> {
            let mut http = reqwest::Client::builder()
                .connect_timeout(connect_timeout);
            let mut no_redirect = reqwest::Client::builder()
                .connect_timeout(connect_timeout)
                .redirect(reqwest::redirect::Policy::none());
            // `read_timeout` is armed and checked before the connector is even polled, so using
            // the raw silence budget here would let it preempt this exact `connect_timeout` —
            // `bounded_read_timeout` adds it in first so the connect phase always gets its full
            // allowance regardless of which budget (direct or Tor) applies to this instance.
            let mut bounded_reads = reqwest::Client::builder()
                .connect_timeout(connect_timeout)
                .read_timeout(bounded_read_timeout(connect_timeout, REMOTE_READ_TIMEOUT));
            let mut bounded_object_reads = reqwest::Client::builder()
                .connect_timeout(connect_timeout)
                .read_timeout(bounded_read_timeout(connect_timeout, FETCH_OBJECT_READ_TIMEOUT));
            // Both axes at once, which no other client here has: `no_redirect`'s policy and
            // `bounded_object_reads`' budget. Deliberately not expressed as a tweak to either of
            // those — see this field's own doc for why neither could take the other's axis.
            let mut bounded_object_reads_no_redirect = reqwest::Client::builder()
                .connect_timeout(connect_timeout)
                .redirect(reqwest::redirect::Policy::none())
                .read_timeout(bounded_read_timeout(connect_timeout, FETCH_OBJECT_READ_TIMEOUT));

            // The same proxy governs every client: the redirect-following ones and the
            // hand-following ones alike route through Tor when the remote does.
            if let Some(proxy) = proxy {
                http = http.proxy(proxy.clone());
                no_redirect = no_redirect.proxy(proxy.clone());
                bounded_reads = bounded_reads.proxy(proxy.clone());
                bounded_object_reads = bounded_object_reads.proxy(proxy.clone());
                bounded_object_reads_no_redirect =
                    bounded_object_reads_no_redirect.proxy(proxy.clone());
            }

            let http = http.build()
                .map_err(|e| format!("Error while creating the HTTP client: {}", e))?;
            let no_redirect = no_redirect.build()
                .map_err(|e| format!("Error while creating the HTTP client: {}", e))?;
            let bounded_reads = bounded_reads.build()
                .map_err(|e| format!("Error while creating the HTTP client: {}", e))?;
            let bounded_object_reads = bounded_object_reads.build()
                .map_err(|e| format!("Error while creating the HTTP client: {}", e))?;
            let bounded_object_reads_no_redirect = bounded_object_reads_no_redirect.build()
                .map_err(|e| format!("Error while creating the HTTP client: {}", e))?;

            Ok(Clients {
                http, no_redirect, bounded_reads, bounded_object_reads,
                bounded_object_reads_no_redirect,
            })
        }

        /// Choose which client a `posture` rides. Exhaustive by construction — a new
        /// [`Posture`] variant that this `match` does not cover fails to compile, so this function
        /// can never silently fall through to a default client.
        ///
        /// **Private on purpose — not `pub(super)`.** Choosing a client says nothing about whether
        /// a payload-carrying posture's payload gets applied; a caller that reached this function
        /// directly could hand back a bare `reqwest::Client` for a bounded-looking posture like
        /// [`Posture::TotalDeadline`] with its `Duration` never read at all, exactly the hole the
        /// deleted `client_for` reopened for its one caller (see [`super::UnboundedTicket`]'s own
        /// doc for the closure claim this bug falsified). Keeping this function unreachable from
        /// outside `mod clients` means the only way to turn a `Posture` into a request on the wire
        /// is [`Self::send_on`], which calls this and then unconditionally applies the payload — so
        /// there is no second, competing path left for a future caller to reach for instead.
        fn pick(&self, posture: &Posture) -> &reqwest::Client {
            match posture {
                Posture::BoundedReads => &self.bounded_reads,
                Posture::BoundedObjectReads => &self.bounded_object_reads,
                Posture::TotalDeadline(_) => &self.http,
                Posture::TotalDeadlineNoRedirect(_) => &self.no_redirect,
                Posture::HeadDeadlineNoRedirect { .. } => &self.bounded_object_reads_no_redirect,
                Posture::UnboundedFollowsRedirects(_) => &self.http,
                Posture::UnboundedNoRedirect(_) => &self.no_redirect,
            }
        }

        /// The only way a request reaches any client: send it on whichever one
        /// [`Self::pick`] chooses, with `posture`'s own payload — currently
        /// [`Posture::TotalDeadline`]'s or [`Posture::TotalDeadlineNoRedirect`]'s `Duration`, or
        /// [`Posture::HeadDeadlineNoRedirect`]'s `head` — applied first, and return a
        /// [`SendOutcome`]. The two payload kinds are applied by two different mechanisms, and
        /// deliberately so: a total deadline is `RequestBuilder::timeout`, which reqwest runs
        /// until the response body has finished, while a head deadline is an external
        /// `tokio::time::timeout` around the `send()` future alone — which is the only reason
        /// this module had to take ownership of `send()` before a head-wait bound could exist at
        /// all. `pick` is private to this
        /// module and this is its only caller, so there is no path from a `Posture` to a client that
        /// can skip this extraction; a future payload-carrying variant forces the match below to be
        /// revisited (it has no wildcard arm, deliberately, mirroring `pick`'s own exhaustiveness)
        /// rather than silently compiling with the new payload dropped — the failure mode both the
        /// deleted `OwnTimeoutFollowsRedirects` posture and the deleted `client_for` had, one layer
        /// apart.
        ///
        /// **Sends rather than returning a builder**, unlike the `request_on` it replaced. That is
        /// what keeps the `send()` future inside this module — see the module's own doc for why
        /// that matters — and it is what makes `body` a described [`SendBody`] rather than
        /// something a call site chains on afterward. The one behaviour that description owns and
        /// a chained `RequestBuilder::json` used to: [`SendBody::Json`] sets
        /// `Content-Type: application/json` and [`SendBody::Bytes`] does not, at this one seam
        /// instead of at every call site.
        ///
        /// `destination` names the two request shapes callers need — see
        /// [`RequestDestination`]'s own doc — the same split [`Self::send_with_watchdog`] makes for
        /// uploads, and for the same reason: a caller-supplied builder, or a caller-supplied bare
        /// client, is exactly the seam a posture's payload can go missing through. `connect_timeout`
        /// is threaded in as a parameter, the same shape [`Self::send_with_watchdog`] already takes
        /// it (see that method's own doc for why it is a parameter here rather than a field this
        /// module stores): it exists for the two payload guards below —
        /// [`check_total_deadline_payload`], a debug-build tripwire against a *future* payload
        /// producer, and [`check_head_deadline_payload`]/[`clamp_head_deadline_payload`], whose
        /// clamp is not a tripwire but a live repair, for the reason that function's own doc
        /// gives. Neither fires against any producer this module has today.
        ///
        /// Returns a [`SendOutcome`] rather than a `Result<_, String>` for the same reason
        /// [`Self::send_with_watchdog`] returns a [`WatchdogOutcome`]: this module's boundary is
        /// boundedness, not operator-facing wording. The two adapters that cross that boundary are
        /// [`super::RemoteClient::response_from_send`] and
        /// [`super::RemoteClient::response_from_send_mutation`].
        pub(super) async fn send_on(&self,
                                    posture: Posture,
                                    connect_timeout: std::time::Duration,
                                    method: reqwest::Method,
                                    destination: RequestDestination<'_>,
                                    body: SendBody) -> SendOutcome {
            let (total_deadline, head_deadline) = match &posture {
                // Same arm for both: [`Posture::TotalDeadlineNoRedirect`] carries the identical
                // total-deadline payload shape as [`Posture::TotalDeadline`], differing only in
                // which client `Self::pick` sends it on — so the payload check and the deadline
                // application below apply unconditionally to both rather than being duplicated
                // for a second variant that would drift out of sync with this one.
                Posture::TotalDeadline(duration) | Posture::TotalDeadlineNoRedirect(duration) => {
                    check_total_deadline_payload(*duration, connect_timeout);
                    (Some(*duration), None)
                }
                // The head deadline is *clamped*, not merely asserted, and the two are a pair:
                // the check fails loudly in a debug build, the clamp keeps the released binary
                // out of the state where a `HeadWaitExpired` message would claim a connection
                // that was never established. See `clamp_head_deadline_payload`'s own doc.
                Posture::HeadDeadlineNoRedirect { head } => {
                    check_head_deadline_payload(*head, connect_timeout);
                    (None, Some(clamp_head_deadline_payload(*head, connect_timeout)))
                }
                // Listed explicitly, not `_`, so a future payload-carrying `Posture` variant
                // forces this match to be revisited too instead of silently falling through to
                // `None` and dropping the new payload.
                Posture::BoundedReads
                | Posture::BoundedObjectReads
                | Posture::UnboundedFollowsRedirects(_)
                | Posture::UnboundedNoRedirect(_) => (None, None),
            };

            let client = self.pick(&posture);

            let mut builder = match destination {
                RequestDestination::Authenticated { base, token, path } => {
                    let mut builder = client.request(method, format!("{}{}", base, path));
                    if let Some(token) = token {
                        builder = builder.bearer_auth(token);
                    }
                    builder
                }
                RequestDestination::Presigned { url } => client.request(method, url),
            };

            if let Some(duration) = total_deadline {
                builder = builder.timeout(duration);
            }

            let send_fut = body.apply(builder).send();

            // The three cells of `Result<Result<_, reqwest::Error>, Elapsed>` map one-to-one onto
            // the three `SendOutcome` variants, and the `Elapsed` is consumed here: it never
            // leaves this function, which is what makes "a head-wait expiry is never a
            // `reqwest::Error` and never reaches `classify`" a property of the type rather than a
            // rule to remember. `tokio::time::timeout` wraps only `send()`, which resolves once
            // the header section has arrived — so nothing here can observe, or cut off, a body.
            match head_deadline {
                Some(head) => match tokio::time::timeout(head, send_fut).await {
                    Ok(Ok(response)) => SendOutcome::Sent(response),
                    Ok(Err(e)) => SendOutcome::Transport(e),
                    Err(_elapsed) => SendOutcome::HeadWaitExpired { budget: head },
                },
                None => match send_fut.await {
                    Ok(response) => SendOutcome::Sent(response),
                    Err(e) => SendOutcome::Transport(e),
                },
            }
        }

        /// Send a watchdog-guarded upload — the operation that used to be promised by
        /// `Posture::WatchdogNoRedirect` (deleted) and discharged by a caller-assembled
        /// `reqwest::RequestBuilder` passed into `RemoteClient::send_with_watchdog`. That shape let
        /// three things drift apart with nothing to catch it: the [`super::UploadProgress`] woven
        /// into the request body could be a different one than the watchdog polled; the watchdog
        /// could simply not be called (`builder.send()` compiled fine and was silently unbounded);
        /// and the explicit `Content-Length` [`super::watched_upload_body`] requires could be
        /// forgotten, silently falling back to `Transfer-Encoding: chunked` (which a presigned S3
        /// `PUT` rejects outright). This function owns all three pairings by construction: it takes
        /// the raw upload `bytes`, not a builder, so the body and the progress the watchdog polls
        /// are always the same [`super::UploadProgress`]; it always runs the watchdog loop, so
        /// there is no bare `.send()` for a call site to reach for instead; and it always sets
        /// `Content-Length` itself, so no call site can omit it.
        ///
        /// **What actually makes the body/progress pairing hold is sole-sourcing, not a type the
        /// compiler checks.** [`super::watched_upload_body`] and `UploadProgress::new` are both
        /// file-private, and this function's own two lines below are their only call — nothing in
        /// either function's signature stops a second call site from constructing its own
        /// `UploadProgress`, elsewhere in this file, and pairing it with a body built from a
        /// different one. `grep -n 'watched_upload_body(' crates/forklift-core/src/util/remote_utils.rs`
        /// and `grep -n 'UploadProgress::new()' crates/forklift-core/src/util/remote_utils.rs` are
        /// the procedure that checks this: read against which lines are doc comments (this
        /// paragraph names both patterns literally, so it is its own hit) and which are this
        /// function's own definition and call, any further hit is a second caller, and a second
        /// caller is exactly the seam reopening — the same failure shape as the deleted
        /// `Posture::WatchdogNoRedirect`, just one level down. Nothing (no compiler error, no test)
        /// catches that on its own; a reviewer noticing the new call site is what actually keeps
        /// this pairing closed. Re-run both greps before trusting this doc, and treat any hit
        /// outside this function and its own doc comment as the claim no longer holding, not as a
        /// benign refactor to wave through.
        ///
        /// Always rides [`Self::no_redirect`] — never auto-follows a redirect — for the same
        /// reason [`Posture::UnboundedNoRedirect`] does (see that client field's own doc): a
        /// one-shot streamed body cannot be replayed if reqwest's default policy tried to
        /// auto-follow a `3xx`. `destination` names the two request shapes callers need — see
        /// [`RequestDestination`]'s own doc, shared with [`Self::send_on`] — since a
        /// caller-supplied builder is exactly the seam this function exists to close; letting a
        /// caller hand one in would reopen it.
        ///
        /// Bounds the wait itself (through `phase1_budget`/`phase2_budget` below) rather than
        /// asserting a bound the way the deleted posture did; the caller supplies `connect_timeout`
        /// because that budget — like every other one this module computes — has to account for
        /// the connect phase's own latency (a Tor circuit most sharply), and this module has no
        /// field of its own to read it from (see this module's own doc for why `RemoteClient`'s
        /// fields stay on `RemoteClient` rather than moving in here).
        ///
        /// Returns a [`WatchdogOutcome`] rather than a `Result<_, String>`: this module's boundary
        /// is boundedness, not operator-facing wording — composing the actual message (which of
        /// [`super::RemoteClient::describe_mutation_transport_error`],
        /// `mutation_read_timeout_message`, or `mutation_post_send_timeout_message` applies, and
        /// what action string to name) stays [`super::RemoteClient`]'s job, the same as it always
        /// was.
        ///
        /// The two phases, and the reasoning behind `phase1_budget`/`phase2_budget`, are unchanged
        /// from the pre-refactor `RemoteClient::send_with_watchdog` — see review rounds S2-F1
        /// through S2-F7, referenced throughout this file's other watchdog-adjacent docs
        /// ([`super::UploadProgress`], [`super::UploadChunks`], [`super::UPLOAD_SILENCE_BUDGET`],
        /// [`super::post_send_verify_budget`]) for the full history of why the shape is what it is.
        pub(super) async fn send_with_watchdog(&self,
                                               connect_timeout: std::time::Duration,
                                               destination: RequestDestination<'_>,
                                               method: reqwest::Method,
                                               bytes: Vec<u8>) -> WatchdogOutcome {
            let progress = super::UploadProgress::new();
            let (body, body_len) = super::watched_upload_body(bytes, progress.clone());

            let mut builder = match destination {
                RequestDestination::Authenticated { base, token, path } => {
                    let mut builder = self.no_redirect.request(method, format!("{}{}", base, path));
                    if let Some(token) = token {
                        builder = builder.bearer_auth(token);
                    }
                    builder
                }
                RequestDestination::Presigned { url } => self.no_redirect.request(method, url),
            };
            builder = builder.header(reqwest::header::CONTENT_LENGTH, body_len).body(body);

            let phase1_budget = connect_timeout + super::UPLOAD_SILENCE_BUDGET;
            let phase2_budget = phase1_budget + super::post_send_verify_budget(body_len);
            let send_fut = builder.send();
            tokio::pin!(send_fut);

            loop {
                tokio::select! {
                    result = &mut send_fut => {
                        return match result {
                            Ok(response) => WatchdogOutcome::Sent(response),
                            Err(e) => WatchdogOutcome::Transport(e),
                        };
                    }
                    _ = tokio::time::sleep(super::UPLOAD_WATCHDOG_POLL_INTERVAL) => {
                        let exhausted = progress.is_exhausted();
                        let budget = if exhausted { phase2_budget } else { phase1_budget };

                        if progress.silent_for() >= budget {
                            return if exhausted {
                                WatchdogOutcome::SilentAfterSend { budget: phase2_budget }
                            } else {
                                WatchdogOutcome::SilentDuringSend
                            };
                        }
                    }
                }
            }
        }
    }

    /// The total-deadline payload check [`Clients::send_on`] performs, extracted as a free
    /// function for one reason: it is the only way to falsify the check without sending anything.
    /// The seam it guards is `async` and owns `send()`, so a test that drove the seam would have
    /// to stand up a runtime and name a host — and a test that reaches DNS before its assert can
    /// no longer tell a deleted assert apart from an unreachable name, which is exactly the
    /// separation the falsifying test was written to buy. Driving this function directly costs
    /// nothing and keeps that separation: no runtime, no I/O, nothing to resolve.
    ///
    /// What the check is for: every producer of a `TotalDeadline`/`TotalDeadlineNoRedirect`
    /// payload builds it as `connect_timeout + a positive addend`
    /// (`RemoteClient::resolve_budget`, `RemoteClient::presence_negotiation_budget`,
    /// `RemoteClient::single_write_budget`) — never merely a post-connect budget on its own — so
    /// the connector's own timeout always fires first on a connect-phase stall, classifying it as
    /// `TransportFailure::ConnectTimedOut` rather than `ReadTimedOut`. All three satisfy this by
    /// construction, so it cannot fire against any call site this module actually has; it exists
    /// to catch the next producer that doesn't.
    ///
    /// What a violating payload costs is the *wording*, not the figure. Were
    /// `duration <= connect_timeout`, that deadline would fire first and produce a bare `TimedOut`
    /// with no `hyper_util` error in its source chain, so `is_connect()` reads false and it lands
    /// in `ReadTimedOut` — which still reports `duration`, still exactly the bound that fired. The
    /// damage is that `describe_transport_error`'s `TotalDeadline` text says the remote "did not
    /// complete its answer" when in fact no connection was ever established and nothing was sent —
    /// the same overstatement the mutation path treats as a contract violation. Hence a
    /// `debug_assert`: adequate for a wording hazard, and deliberately not an invariant the
    /// released binary's numbers depend on. It compiles out entirely in a release build, same as
    /// any `debug_assert!`.
    ///
    /// **The head-deadline payload does not get the same treatment**, and the asymmetry is the
    /// point — see [`clamp_head_deadline_payload`]: the same violation costs a *false* sentence
    /// there rather than a merely overstated one, so that payload is repaired instead of asserted.
    ///
    /// Falsified directly — bypassing every producer and hand-constructing a violating payload —
    /// by [`super::tests::the_total_deadline_payload_check_catches_a_violating_payload`]; see that
    /// test's own doc for why none of the three producers could ever reach the failing branch, and
    /// [`super::RemoteClient::describe_transport_error`]'s doc for the wording hazard a violation
    /// would cause. **What that test pins is this function, not that the seam still calls it** —
    /// the link between the two is [`Clients::send_on`]'s single call above, and this function
    /// having no other caller. Check both before trusting the pairing:
    /// `grep -n 'check_total_deadline_payload' crates/forklift-core/src/util/remote_utils.rs`.
    pub(super) fn check_total_deadline_payload(duration: std::time::Duration,
                                               connect_timeout: std::time::Duration) {
        debug_assert!(
            duration > connect_timeout,
            "a Posture::TotalDeadline/TotalDeadlineNoRedirect payload ({:?}) must \
            strictly exceed this request's connect_timeout ({:?}) — otherwise a \
            connect-phase stall lands in TransportFailure::ReadTimedOut instead of \
            ConnectTimedOut, and describe_transport_error reports that the remote did \
            not finish answering when no connection was ever established",
            duration, connect_timeout
        );
    }

    /// What [`clamp_head_deadline_payload`] raises a violating head-deadline payload by, above the
    /// `connect_timeout` that payload failed to clear. Deliberately small and deliberately
    /// arbitrary: no producer in this module can reach that branch (the only one,
    /// [`super::RemoteClient::batch_head_budget`], builds `connect_timeout + a positive addend`),
    /// so this value is never armed against a real remote. Only its **strict positivity** is
    /// load-bearing — that is what keeps the connector's own timeout firing first on a
    /// connect-phase stall, and so keeps every [`SendOutcome::HeadWaitExpired`] describing a
    /// connection that really was established.
    const HEAD_DEADLINE_REPAIR_ADDEND: std::time::Duration = std::time::Duration::from_secs(1);

    /// The loud half of the head-deadline payload discipline, and the sibling of
    /// [`check_total_deadline_payload`]: a payload at or below `connect_timeout` is a producer
    /// bug, and [`clamp_head_deadline_payload`] silently repairing it in every build would hide
    /// that bug from whoever introduced it.
    ///
    /// Kept as its own function rather than folded into the clamp so the clamp stays pure. A clamp
    /// that panicked on the branch it exists to handle could not have its repair asserted at all
    /// in a debug build — the profile the suite runs in — which would leave the repair itself
    /// unfalsifiable exactly where it matters.
    ///
    /// **The payload is fenced on both sides, and the two fences guard two different sentences.**
    /// Below `connect_timeout` the head timer could fire during connect, and
    /// [`super::RemoteClient::head_wait_expired_message`] would claim a connection that never
    /// existed. At or above this posture's client-level `read_timeout` the ordering inverts the
    /// other way: that read timeout would fire *first*, pre-headers, producing a `reqwest::Error`
    /// whose [`super::RemoteClient::describe_transport_error`] arm says response headers had
    /// already arrived — when none had. The one producer that exists satisfies both by
    /// construction and cannot violate the upper fence at all
    /// ([`super::RemoteClient::batch_head_budget`] is `connect_timeout` plus a constant a const
    /// assertion holds under [`super::FETCH_OBJECT_READ_TIMEOUT`]; see
    /// [`super::BATCH_HEAD_PATIENCE`]'s doc). This exists to catch the next producer, which is why
    /// the upper fence is a `debug_assert!` and not a clamp: unlike the lower one, no repair value
    /// is obviously right — a payload that big means the producer wanted something this posture
    /// does not offer.
    ///
    /// Falsified in both directions by
    /// [`super::tests::the_head_deadline_payload_check_catches_a_violating_payload`] and
    /// [`super::tests::the_head_deadline_payload_check_catches_a_payload_past_the_silence_budget`];
    /// see the first's doc for why its fixture uses a payload exactly *equal* to `connect_timeout`
    /// rather than a lesser one.
    pub(super) fn check_head_deadline_payload(head: std::time::Duration,
                                              connect_timeout: std::time::Duration) {
        debug_assert!(
            head > connect_timeout,
            "a Posture::HeadDeadlineNoRedirect head payload ({:?}) must strictly exceed \
            this request's connect_timeout ({:?}) — otherwise the external head-wait timer \
            fires during connect, and head_wait_expired_message tells the operator the \
            connection was established when none ever was",
            head, connect_timeout
        );
        debug_assert!(
            head < bounded_read_timeout(connect_timeout, FETCH_OBJECT_READ_TIMEOUT),
            "a Posture::HeadDeadlineNoRedirect head payload ({:?}) must stay strictly under \
            this posture's own client-level read_timeout ({:?}) — otherwise that read timeout \
            fires before the status line, and describe_transport_error tells the operator the \
            remote had already sent response headers when it had sent nothing",
            head, bounded_read_timeout(connect_timeout, FETCH_OBJECT_READ_TIMEOUT)
        );
    }

    /// The head-deadline payload repair [`Clients::send_on`] performs — the
    /// [`Posture::HeadDeadlineNoRedirect`] counterpart of [`check_total_deadline_payload`], and
    /// deliberately **not** the same shape.
    ///
    /// Both guard the same producer rule — every payload is `connect_timeout + a positive addend`
    /// — but a violation costs differently. For a total deadline it costs a worse-than-intended
    /// bound and one overstated sentence, so a `debug_assert!` is proportionate. For a head
    /// deadline it costs a **false claim in the binary that ships**: a payload at or below
    /// `connect_timeout` lets the external timer fire *during* connect, and
    /// [`super::RemoteClient::head_wait_expired_message`] then tells the operator the remote
    /// accepted the connection and went quiet, when nothing ever connected and nothing was sent.
    /// A `debug_assert!` compiles out in release, so it promises nothing at all about that binary;
    /// a clamp cannot produce the state in the first place.
    ///
    /// So this raises rather than asserts, with [`check_head_deadline_payload`] beside it for the
    /// loud debug-build failure — the two are called in that order, at the one seam. The size of
    /// the raise ([`HEAD_DEADLINE_REPAIR_ADDEND`]) is arbitrary and may be: what matters is only
    /// that the result strictly exceeds `connect_timeout`.
    ///
    /// **That is also what entitles the expiry wording to assert an established connection.**
    /// `connect_timeout` wraps the whole connector — DNS, TCP, TLS, a SOCKS handshake — so a
    /// connect-phase stall always resolves the `send()` future with a `reqwest::Error` strictly
    /// before a payload clamped above it could fire.
    ///
    /// Pure and total, so both directions are assertable in any profile with no runtime, no host
    /// name and no socket — see
    /// [`super::tests::the_head_deadline_payload_clamp_raises_a_violating_payload`]. **What that
    /// test pins is this function, not that the seam still calls it** — the link is
    /// [`Clients::send_on`]'s single call, and this function having no other caller:
    /// `grep -n 'clamp_head_deadline_payload' crates/forklift-core/src/util/remote_utils.rs`.
    pub(super) fn clamp_head_deadline_payload(head: std::time::Duration,
                                              connect_timeout: std::time::Duration)
                                              -> std::time::Duration {
        if head > connect_timeout {
            head
        } else {
            connect_timeout + HEAD_DEADLINE_REPAIR_ADDEND
        }
    }

    /// The body [`Clients::send_on`] puts on a request, described rather than chained on by the
    /// call site — which is the price of the seam owning `send()`: there is no builder left for a
    /// call site to hang `.json(...)`/`.body(...)` off.
    ///
    /// **The [`Self::Json`] arm sets `Content-Type: application/json`; the [`Self::Bytes`] arm
    /// does not. That difference is the entire reason the two variants are distinct** — otherwise
    /// one `Body(Vec<u8>)` variant would do. Every json call site used to reach the wire through
    /// `RequestBuilder::json`, which sets that header itself when the request does not already
    /// carry one (reqwest 0.12.28, `async_impl/request.rs`'s `RequestBuilder::json`); a
    /// pre-serialized `Vec<u8>` handed to `RequestBuilder::body` does not. Setting it here rather
    /// than at every call site is the same argument this module makes for discharging a
    /// posture's payload at one seam. Pinned in both directions by
    /// [`super::tests::the_json_arm_sets_a_json_content_type_and_the_bytes_arm_does_not`].
    ///
    /// **The two heads do not agree on how much this matters, and only one of them fails loudly.**
    /// `forklift-server` extracts these bodies with `axum::Json<T>`, whose `FromRequest` returns
    /// `MissingJsonContentType` when the header is absent or names a non-json media type (axum
    /// 0.8.9, `src/json.rs`'s `json_content_type`) — so a dropped header is a refusal on every
    /// json endpoint there. `forklift-aws-lambda` parses the same bodies with
    /// `serde_json::from_slice` (`entrypoint.rs`'s `parse_json`) and never reads the header at
    /// all, so it would keep accepting them. Do not restate this as "the heads reject it": that
    /// is true of one head. The client sets the header because the protocol says the body is
    /// json, and because the stricter head is entitled to hold it to that.
    ///
    /// Bodies are pre-serialized at the call site rather than `send_on` being generic over
    /// `T: Serialize`, which would force a dummy type at every body-less site. One consequence:
    /// a serialization failure surfaces here, as its own error, instead of being deferred into a
    /// `reqwest::Error` at send time.
    pub(super) enum SendBody {
        /// No body at all — the shape every `GET` this module sends uses.
        Empty,
        /// A pre-serialized json document, built by [`Self::json`]. Sets
        /// `Content-Type: application/json`.
        Json(Vec<u8>),
        /// Raw bytes that are not json and declare no `Content-Type` of their own —
        /// `RemoteClient::upload_signature`'s signature sidecar.
        Bytes(Vec<u8>),
    }

    impl SendBody {
        /// Serialize `value` into a [`Self::Json`] body. Fallible, and deliberately so: this is
        /// where a serialization failure becomes visible, rather than at send time inside a
        /// `reqwest::Error` that `classify` would then have to describe as a transport problem.
        pub(super) fn json<T: serde::Serialize + ?Sized>(value: &T) -> Result<SendBody, String> {
            serde_json::to_vec(value)
                .map(SendBody::Json)
                .map_err(|e| format!("Error while encoding the request body for the remote: {}", e))
        }

        /// Put this body on `builder`. Private to `mod clients`, and the only place a
        /// `Content-Type` is decided: a caller able to apply a body for itself would be a second
        /// place that decision could be made, which is the seam this type exists to close.
        fn apply(self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
            match self {
                SendBody::Empty => builder,
                SendBody::Json(bytes) => builder
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(bytes),
                SendBody::Bytes(bytes) => builder.body(bytes),
            }
        }
    }

    /// What [`Clients::send_on`] found, before [`super::RemoteClient`] turns it into an
    /// operator-facing message. The sibling of [`WatchdogOutcome`], and a structured outcome for
    /// the same reason: composing that message is policy this module deliberately does not own.
    ///
    /// Its first two variants are what `Result<reqwest::Response, reqwest::Error>` would be, and
    /// staying an enum was not an accident to be simplified away even while there were only two —
    /// a `Result` invites a call site to `?` the error straight into its own `String`, which is
    /// exactly the step that must go through
    /// [`super::RemoteClient::response_from_send`]/[`super::RemoteClient::response_from_send_mutation`]
    /// so the posture-dependent wording is chosen once. The same shape [`WatchdogOutcome`] already
    /// has, for the same reason. The third variant is now also what makes that `Result` shape
    /// impossible: a head-wait expiry is not a `reqwest::Error` and must never be describable as
    /// one.
    pub(super) enum SendOutcome {
        /// The request was sent and a response came back — success or not is the caller's status
        /// check to make; this variant only means the wait itself completed.
        Sent(reqwest::Response),
        /// A genuine `reqwest::Error`: connect, DNS, TLS, a fired total-deadline payload, or a
        /// fired client-level `read_timeout`. Everything [`super::classify`] can see.
        Transport(reqwest::Error),
        /// The external head-wait timer [`Posture::HeadDeadlineNoRedirect`] arms fired: the
        /// connection was established — every producer's budget strictly exceeds
        /// `connect_timeout`, and the seam clamps one that doesn't (see
        /// [`clamp_head_deadline_payload`]) — and `budget` elapsed with no complete header
        /// section.
        ///
        /// **Not a `reqwest::Error`, and never shown to [`super::classify`].** The type is what
        /// carries that, not a comment: there is no `reqwest::Error` in this variant to hand it,
        /// so no path exists that could ask, and `is_timeout()`'s inability to fire for a
        /// `tokio::time::error::Elapsed` never becomes a question anyone has to answer. `budget`
        /// is the figure that actually fired — post-clamp, not whatever a producer handed in —
        /// so [`super::RemoteClient::head_wait_expired_message`] may name it exactly.
        HeadWaitExpired { budget: std::time::Duration },
    }

    /// Which physical request a call builds through this module — shared by
    /// [`Clients::send_with_watchdog`] (the two watchdog-guarded upload call sites) and
    /// [`Clients::send_on`] (every other request), named explicitly rather than letting
    /// a caller hand in a pre-built `reqwest::RequestBuilder` or a bare `reqwest::Client` (the seam
    /// both of those functions exist to close; see their own docs).
    pub(super) enum RequestDestination<'a> {
        /// A path relative to the remote's `base`, with the bearer token attached (if one is
        /// configured) — the control-plane shape almost every caller needs:
        /// `RemoteClient::upload_object` (through `send_with_watchdog`) and every ordinary
        /// `RemoteClient::send_on` call (through `Clients::send_on`).
        Authenticated {
            base: &'a str,
            token: Option<&'a str>,
            path: &'a str,
        },
        /// An absolute, self-authorizing URL, with **no** bearer token — a presigned storage
        /// request carries its own credentials in its query string, and attaching this remote's
        /// bearer token to a request bound for a different host would be a needless credential leak
        /// (see [`super::RemoteClient::put_presigned`]'s own doc). `put_presigned` (through
        /// `send_with_watchdog`) and `RemoteClient::fetch_batch`'s redirect-follow `GET` (through
        /// `Clients::send_on`, via `RemoteClient::send_on_presigned`).
        Presigned { url: &'a str },
    }

    /// What [`Clients::send_with_watchdog`] found, before [`super::RemoteClient`] turns it into an
    /// operator-facing message. A structured outcome rather than a `Result<_, String>` because
    /// composing that message is policy this module deliberately does not own — see
    /// [`Clients::send_with_watchdog`]'s own doc.
    pub(super) enum WatchdogOutcome {
        /// The request was sent and a response came back — success or not is the caller's status
        /// check to make; this variant only means the wait itself completed.
        Sent(reqwest::Response),
        /// A genuine `reqwest::Error` — a real transport failure, not a watchdog kill. Maps onto
        /// [`super::RemoteClient::describe_mutation_transport_error`].
        Transport(reqwest::Error),
        /// The watchdog killed the request during the send phase: `phase1_budget` elapsed with no
        /// chunk pulled from the body and the stream not yet exhausted. Carries no budget, unlike
        /// [`Self::SilentAfterSend`]: it maps onto
        /// [`super::RemoteClient::mutation_read_timeout_message`], which is deliberately shared,
        /// word-for-word, with a genuinely ambiguous `reqwest::Error` timeout on an
        /// already-established connection (see that function's own doc) — a mid-body watchdog kill
        /// is the same *shape* of failure as that ambiguous case, so it deliberately gets the same
        /// wording rather than a more precise one the mechanism happens to have on hand. Naming
        /// `phase1_budget` here anyway would be dead weight: no consumer of this variant would ever
        /// read it, and a value nothing reads is worse than no field at all — the lesson
        /// `UnboundedTicket`'s own carried-but-unread payloads do *not* generalize to, since those
        /// earn their keep by being greppable even unread, and this field wouldn't.
        SilentDuringSend,
        /// The watchdog killed the request after the stream reported itself exhausted: `budget`
        /// (the larger, post-send figure) elapsed with still no response. Maps onto
        /// [`super::RemoteClient::mutation_post_send_timeout_message`], which does name this
        /// budget.
        SilentAfterSend { budget: std::time::Duration },
    }
}

use clients::{Clients, Posture, RequestDestination, SendBody, SendOutcome, UnboundedTicket, WatchdogOutcome};

/// The remote endpoint: base URL, optional bearer token, and the HTTP clients — see the
/// [`clients`] module's own doc for why there is more than one of them and why they are not fields here
/// any more.
#[derive(Clone)]
pub struct RemoteClient {
    /// The `reqwest::Client`s this remote may send a request on, gated behind
    /// [`clients::Posture`] — see the [`clients`] module's own doc.
    clients: Clients,
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

    /// `is_timeout() && !is_connect()`. **How precisely this can be reported depends on the
    /// [`clients::Posture`] the request was armed with**, which is why
    /// [`RemoteClient::describe_transport_error`] takes that posture rather than a bare duration.
    ///
    /// The underlying ambiguity is real: this can be a client-configured timeout, or a genuine
    /// kernel `ETIMEDOUT` on an already-established connection — the OS returns that once TCP
    /// retransmissions are exhausted, roughly 15 minutes on common Linux/macOS defaults — and
    /// `reqwest::Error::is_timeout()` cannot tell the two apart: it matches any `io::Error` with
    /// `kind() == TimedOut` anywhere in the source chain, not only its own synthetic marker for a
    /// client-configured timeout.
    ///
    /// With no armed total, that ambiguity is unresolvable and no figure is named at all: naming
    /// one would be right most of the time and wrong by orders of magnitude the rest. With a
    /// silence budget the configured value is a genuine *lower* bound, reported as one ("at
    /// least"). With a total deadline it is exact — every producer of one builds it from a
    /// connect allowance plus a post-connect addend (`grep -n "Posture::TotalDeadline("` for the
    /// current set), all far under the kernel figure above, so the armed deadline is always what
    /// fires.
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
    /// When the settings route this remote through Tor (see [`should_route_through_tor`]), every
    /// underlying client (see [`RemoteClient`]'s own doc) dials through the Tor SOCKS proxy
    /// and uses [`REMOTE_CONNECT_TIMEOUT_TOR`] as its connect budget instead of
    /// [`REMOTE_CONNECT_TIMEOUT`]; each `read_timeout` a client carries is computed from
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

        // A Tor dial's connect phase covers the whole SOCKS handshake and onion circuit build,
        // which can legitimately take tens of seconds — far past what a direct dial should ever
        // need (see `REMOTE_CONNECT_TIMEOUT_TOR`'s doc).
        let connect_timeout = if routes_through_tor {
            REMOTE_CONNECT_TIMEOUT_TOR
        } else {
            REMOTE_CONNECT_TIMEOUT
        };

        Self::build(url, token, &tor, routes_through_tor, connect_timeout)
    }

    /// Build the underlying clients and assemble a [`RemoteClient`], given a
    /// `connect_timeout` already decided by the caller — [`Self::new_with_tor`] is the only
    /// production caller, and decides it from [`should_route_through_tor`] as its doc describes.
    /// Split out from `new_with_tor` so [`Self::new_test_with_connect_timeout`] (test-only) can
    /// supply an arbitrary `connect_timeout` that actually gets built into every client, rather
    /// than one construction path deciding the timeout and a second one overwriting only the
    /// `connect_timeout` field afterward — the latter would leave the inner clients armed
    /// with a *different* bound than the field reports, which is not a client any production
    /// constructor could ever produce and would silently invalidate any test built on top of it
    /// that touches `describe_transport_error` or `send_with_watchdog`, both of which trust this
    /// field to describe what the clients actually carry.
    fn build(url: &str,
             token: Option<String>,
             tor: &TorSettings,
             routes_through_tor: bool,
             connect_timeout: std::time::Duration) -> Result<RemoteClient, String> {
        let proxy = if routes_through_tor {
            Some(reqwest::Proxy::all(&tor.proxy).map_err(|e| format!(
                "Error while configuring the Tor proxy \"{}\": {}", tor.proxy, e
            ))?)
        } else {
            None
        };

        let clients = Clients::build(connect_timeout, proxy.as_ref())?;

        Ok(RemoteClient {
            clients,
            connect_timeout,
            base: url.trim_end_matches('/').to_string(),
            token,
        })
    }

    /// Test-only: build a client with an arbitrary injected `connect_timeout`, threaded into the
    /// inner clients via [`Self::build`] rather than overwritten on the field afterward (see
    /// that method's doc for why the two are not equivalent). Never routes through Tor — `tor` is
    /// irrelevant to a plain/`.invalid` test URL, and passing `routes_through_tor: false`
    /// explicitly here means that is a direct fact about this constructor, not a side effect of
    /// `TorMode::Auto`'s onion-sniffing that a future `.onion` call site could quietly flip.
    /// Needed because the two production constructors can only ever produce one of two
    /// `connect_timeout` values — [`REMOTE_CONNECT_TIMEOUT`] (5s) or
    /// [`REMOTE_CONNECT_TIMEOUT_TOR`] (60s) — so a fixture built through either one can never
    /// separate "reads this instance's own `connect_timeout`" from "hardcodes whichever of those
    /// two constants that fixture happens to carry": a Tor-mode fixture asserting the Tor-folded
    /// budget stayed green under review round 7's mutation that hardcoded
    /// `REMOTE_CONNECT_TIMEOUT_TOR` directly in [`Self::error_body_budget`], because the field and
    /// the hardcoded constant were the same value at that one fixture point. A value no production
    /// constructor can ever emit closes that gap — see `error_body_budget_reads_this_field_not_a_rival_constant`.
    #[cfg(test)]
    fn new_test_with_connect_timeout(url: &str, connect_timeout: std::time::Duration) -> RemoteClient {
        RemoteClient::build(url, None, &TorSettings::default(), false, connect_timeout)
            .expect("test fixture URL must build a client")
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

    /// Send a request against this remote's control plane, riding whichever client
    /// `posture` selects and with that posture's own payload already applied — see
    /// [`clients::Clients::send_on`]'s own doc for the mechanism that guarantees the latter
    /// unconditionally, not merely at this one call site. `posture` is a required argument, not a
    /// default — there is no more "the seam every call that needs something other than the
    /// default client uses"; every call, including the ones that used to ride an implicit
    /// default, names its posture explicitly here.
    ///
    /// Returns a [`clients::SendOutcome`], not a `Result<_, String>`: the wording for a transport
    /// failure depends on whether this call is a read or a mutation, which is the caller's fact,
    /// not this wrapper's. Every caller hands the outcome straight to
    /// [`Self::response_from_send`] or [`Self::response_from_send_mutation`].
    ///
    /// Always attaches the bearer token, if one is configured: this is the authenticated,
    /// relative-path shape almost every call needs. The one exception —
    /// [`Self::fetch_batch`]'s redirect-follow `GET`, an absolute presigned URL that must *not*
    /// carry this remote's token — goes through [`Self::send_on_presigned`] instead; see that
    /// function's own doc.
    async fn send_on(&self,
                     posture: Posture,
                     method: reqwest::Method,
                     path: &str,
                     body: SendBody) -> SendOutcome {
        self.clients.send_on(
            posture, self.connect_timeout, method,
            RequestDestination::Authenticated { base: &self.base, token: self.token.as_deref(), path },
            body,
        ).await
    }

    /// The presigned counterpart of [`Self::send_on`] — same posture, same payload guarantee,
    /// but sending to an absolute, self-authorizing URL with **no** bearer token attached. Down
    /// to one live caller — [`Self::fetch_batch`]'s redirect-follow `GET` (a presigned storage URL
    /// is self-authorizing; forwarding a bearer token meant for the control plane would be a
    /// needless credential leak) — since [`Self::put_presigned`], this shape's other historical
    /// caller, moved onto [`clients::Clients::send_with_watchdog`], which builds its own
    /// bearer-token-free request entirely inside [`clients`] rather than reaching back out here.
    ///
    /// Both this function and [`Self::send_on`] are thin wrappers around
    /// [`clients::Clients::send_on`], which is the only place in this module a `Posture` ever
    /// becomes a request on the wire — neither wrapper, nor anything else outside `mod clients`,
    /// can reach a bare `reqwest::Client`, or even a `reqwest::RequestBuilder`, at all any more
    /// (the bare client was the deleted `client_for`'s whole
    /// shape, and the hole it reopened one layer below this module's own guarantee). Kept as its
    /// own function rather than inlined into `fetch_batch`, same reasoning `client_for` gave: a
    /// second caller reappearing (another self-authorizing, no-bearer-token request) is more
    /// likely than this staying permanently singular, and inlining now would just have to be
    /// undone.
    async fn send_on_presigned(&self,
                               posture: Posture,
                               method: reqwest::Method,
                               url: &str,
                               body: SendBody) -> SendOutcome {
        self.clients.send_on(
            posture, self.connect_timeout, method, RequestDestination::Presigned { url }, body,
        ).await
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
    /// an HTTP status) on any call riding a [`Posture`] through [`Self::send_on`] or
    /// [`Self::send_on_presigned`]. Thin: all it does is read `is_connect()`/`is_timeout()` off
    /// `e` and hand them to [`classify`], then render the resulting [`TransportFailure`] — the
    /// actual case analysis lives there, pure and unit-tested over all four boolean combinations,
    /// because the combination this function cares least about (a genuine multi-minute kernel
    /// timeout on an already-established socket) is not practically constructible in a test at
    /// all; see [`TransportFailure::ReadTimedOut`]'s doc.
    ///
    /// Takes the exact `posture` a call was armed with, not a bare silence-budget `Duration` —
    /// what bound (if any) actually governed the wait is a function of which posture the request
    /// rode, and a call site could otherwise name a bound different from the one that applied.
    /// The `ReadTimedOut` arm renders that per-posture: [`Posture::BoundedReads`]/
    /// [`Posture::BoundedObjectReads`] report a *lower* bound ("at least") on
    /// [`bounded_read_timeout`]'s own value, unchanged from before this reshape, since a
    /// resetting silence budget only ever guarantees the wait went on at least that long.
    /// [`Posture::TotalDeadline`] instead reports its own payload verbatim, with no "at least" —
    /// a non-resetting `RequestBuilder::timeout` is a genuine upper bound the call could never
    /// have exceeded, so the exact figure is honest. (The `debug_assert!` in
    /// [`clients::check_total_deadline_payload`], which [`clients::Clients::send_on`]'s
    /// `TotalDeadline` arm calls, guards this arm's *wording*, not its
    /// figure — see that function's own doc for why the number stays correct either way.)
    /// [`Posture::UnboundedFollowsRedirects`]/[`Posture::UnboundedNoRedirect`] name no figure at
    /// all — nothing was armed, so [`TransportFailure::ReadTimedOut`]'s own doc's ambiguity
    /// (a client `read_timeout` vs. a genuine multi-minute kernel `ETIMEDOUT`) applies at full
    /// force here, the one case this function cannot narrow at all.
    /// [`Posture::TotalDeadlineNoRedirect`]'s
    /// arm exists only so this match stays exhaustive — every call riding it is a mutation and
    /// never reaches this function at all (see [`Self::describe_mutation_transport_error`]'s doc).
    ///
    /// **[`Posture::HeadDeadlineNoRedirect`]'s arm has to be distinguishable by its words, because
    /// its figure is already taken.** It reports the same lower bound
    /// [`Posture::BoundedObjectReads`] does — the same constant, on a client built the same way —
    /// and `fetch_batch` hands this function both postures under one action string, choosing by
    /// which of its two stations produced the bytes. Rendering the same figure with interchangeable
    /// wording would collapse exactly the distinction the two-posture split exists to keep, so
    /// this arm says what the other cannot: that response headers had already arrived and the
    /// silence began after them. That is available structurally rather than by luck — under this
    /// posture the head-wait timer is strictly the tighter bound and produces no `reqwest::Error`
    /// at all, so a `ReadTimedOut` reaching here can only be post-header (see
    /// [`clients::check_head_deadline_payload`] for the fence, [`BATCH_HEAD_PATIENCE`]'s doc for
    /// the constant-level pin). [`TransportFailure::ReadTimedOut`]'s kernel-`ETIMEDOUT` ambiguity
    /// does not touch that claim: on an established connection a kernel timeout means an even
    /// longer silence, which "at least" already covers.
    ///
    /// A head-wait *expiry* does not reach this function either, from any posture: it is not a
    /// `reqwest::Error`, so it never reaches [`classify`] — [`Self::head_wait_expired_message`] is
    /// its composer.
    fn describe_transport_error(&self,
                                action: &str,
                                posture: Posture,
                                e: reqwest::Error) -> String {
        match classify(e.is_connect(), e.is_timeout()) {
            TransportFailure::ConnectTimedOut => format!(
                "Timed out while {}: could not connect to the remote within {:?}.",
                action, self.connect_timeout
            ),
            TransportFailure::ReadTimedOut => match posture {
                Posture::BoundedReads => format!(
                    "Timed out while {}: the remote did not respond within at least {:?}.",
                    action, bounded_read_timeout(self.connect_timeout, REMOTE_READ_TIMEOUT)
                ),
                Posture::BoundedObjectReads => format!(
                    "Timed out while {}: the remote did not respond within at least {:?}.",
                    action, bounded_read_timeout(self.connect_timeout, FETCH_OBJECT_READ_TIMEOUT)
                ),
                Posture::TotalDeadline(deadline) => format!(
                    "Timed out while {}: the remote did not complete its answer within this \
                    request's {:?} total deadline.",
                    action, deadline
                ),
                // Required only for this match's exhaustiveness — every call riding
                // `TotalDeadlineNoRedirect` is a mutation and routes its transport errors through
                // `describe_mutation_transport_error` instead, never through this function.
                Posture::TotalDeadlineNoRedirect(deadline) => format!(
                    "Timed out while {}: the remote did not complete its answer within this \
                    request's {:?} total deadline.",
                    action, deadline
                ),
                // A silence budget, so "at least" — and it must not read like the
                // `BoundedObjectReads` arm above, which prints the identical figure. What
                // separates them is the phase: the head-wait timer produces no `reqwest::Error`
                // (it produces `SendOutcome::HeadWaitExpired`, routed to
                // `head_wait_expired_message`) and always fires first, so anything landing here
                // stalled *after* the header section. Naming the gap rather than the elapsed
                // total is the other half: this budget resets on every byte, so the figure bounds
                // one silent gap, never how long the transfer ran.
                Posture::HeadDeadlineNoRedirect { .. } => format!(
                    "Timed out while {}: the remote sent response headers and then stopped \
                    sending, with no further bytes for at least {:?}. That budget is on silence \
                    between bytes, not on how long the whole transfer may take.",
                    action, bounded_read_timeout(self.connect_timeout, FETCH_OBJECT_READ_TIMEOUT)
                ),
                Posture::UnboundedFollowsRedirects(_) | Posture::UnboundedNoRedirect(_) => format!(
                    "Timed out while {}: the remote did not respond.",
                    action
                ),
            },
            TransportFailure::Other => format!("Error while {}: {}", action, Self::root_cause(&e)),
        }
    }

    /// The mutation counterpart of [`Self::describe_transport_error`], reached by all six
    /// mutations (`update_ref`, `upload_object`, `put_presigned`, `upload_signature`, `put_trust`,
    /// `commit_lift`) regardless of whether that mutation auto-follows a redirect or not — five of
    /// the six never do; `commit_lift` still does, the standing FORK-89 gap [`Clients::http`]'s own
    /// doc names (see below, and [`super::RemoteClient::commit_lift`]'s own doc). Takes no
    /// `Posture` because its wording
    /// does not vary by one: unlike the read path, a mutation's `ReadTimedOut` is always the same
    /// uncertain shape regardless of which bound (if any) actually fired, so every one of the six
    /// funnels through the identical composer. `update_ref` rides [`Posture::UnboundedNoRedirect`]
    /// (moved off the auto-following client by FORK-89); `upload_signature`/`put_trust` ride
    /// [`Posture::TotalDeadlineNoRedirect`] — bounded, but on the identical never-auto-follow
    /// client, so their transport failures land here the same way; `upload_object`/`put_presigned`
    /// ride [`clients::Clients::send_with_watchdog`] directly, on the same never-auto-follow
    /// client, but bypassing [`Posture`] entirely (moved by the earlier fix for the `303` redirect
    /// hole, then off `Posture` again once a payload-free posture proved unable to guarantee the
    /// watchdog was actually applied); only `commit_lift` still rides the auto-following client
    /// ([`Posture::UnboundedFollowsRedirects`]) directly. `upload_object`/`put_presigned`'s
    /// watchdog also produces a genuine `reqwest::Error` for an actual transport failure (as
    /// opposed to a watchdog kill, which produces no `reqwest::Error` at all — see
    /// [`clients::WatchdogOutcome::Transport`]'s doc), so a transport failure on any of the six
    /// still lands here for anything `classify` can actually see (a connect failure, or a
    /// `reqwest::Error`-bearing timeout on the response side — a fired
    /// [`Posture::TotalDeadlineNoRedirect`] deadline is exactly such a `reqwest::Error`, `is_timeout()`
    /// true, same as any other). Same [`classify`] dispatch, but the [`TransportFailure::ReadTimedOut`]
    /// wording differs from the read path's: on these clients that case can only be a timeout on an
    /// *established* connection — after the request bytes were already sent — so the settled
    /// contract requires the uncertainty be carried in the message rather than asserted away: it
    /// may have completed on the remote, and the caller must decide whether to check before
    /// retrying, never be told nothing happened.
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
    /// [`clients::Clients::send_with_watchdog`]'s upload watchdog can produce the identical
    /// wording (via [`Self::response_from_watchdog`]) without a
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

    /// The message for a [`clients::SendOutcome::HeadWaitExpired`] on a *read* call: the external
    /// head-wait timer [`Posture::HeadDeadlineNoRedirect`] arms fired before a complete status line
    /// and header section arrived. The read half of the pair
    /// [`Self::mutation_read_timeout_message`] completes for the mutation path — which takes the
    /// existing uncertainty wording rather than this one, since a head-wait expiry on a mutation
    /// means the request may have fully arrived and had effect.
    ///
    /// **Three things this must not say, and between them they are the whole contract:**
    ///
    /// - **Not "nothing reached the remote."** The timer covers request transmission as well as
    ///   the wait for headers, so the body may have been mid-flight, or fully delivered, when it
    ///   fired. This is a read, so re-running is safe either way — but the wording still may not
    ///   assert a fact it does not have.
    /// - **Not "the remote is fine, just slow"** — nor its opposite. A wedged head and a head
    ///   legitimately still building a large bundle are indistinguishable from here; that
    ///   indistinguishability is the accepted residual [`BATCH_HEAD_PATIENCE`]'s own doc records,
    ///   and naming either possibility as *the* cause would be inventing the one bit this client
    ///   does not have.
    /// - **No "at least."** Unlike a silence budget, this timer never resets on progress, so
    ///   `budget` is a genuine upper bound on the wait and the exact figure is honest — the same
    ///   distinction [`Self::describe_transport_error`] already draws between its
    ///   [`Posture::BoundedReads`] and [`Posture::TotalDeadline`] arms.
    ///
    /// What it *may* assert is that the connection was established, and only because the seam
    /// guarantees it rather than because it is usually true: see
    /// [`clients::clamp_head_deadline_payload`], which keeps every armed budget strictly above
    /// `connect_timeout`, so a connect-phase stall always resolves the send future as a
    /// `reqwest::Error` first.
    ///
    /// A free function of `action` and `budget`, like [`Self::mutation_read_timeout_message`]:
    /// nothing it says depends on any other instance state.
    fn head_wait_expired_message(action: &str, budget: std::time::Duration) -> String {
        format!(
            "Timed out while {}: the remote accepted the connection but sent no response \
            headers within {:?}. The request may have reached it — the remote may be wedged, or \
            still building an answer this bound was too short for, and the two look identical \
            from here.",
            action, budget
        )
    }

    /// The message for a *post-send* mutation stall — the watchdog's second phase (see
    /// [`clients::Clients::send_with_watchdog`]): the stream reported itself exhausted (every chunk was
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
    /// an unflushed tail at the instant this fires (see [`clients::Clients::send_with_watchdog`]'s
    /// doc, S2-F2) — so this states only the locally-observable fact, not a claim about what the
    /// remote has.
    fn mutation_post_send_timeout_message(action: &str, budget: std::time::Duration) -> String {
        format!(
            "Timed out while {}: the client finished streaming the request body, but no \
            response arrived within {:?}. The remote may already have received, verified, and \
            stored the bytes — so retrying is safe; re-uploading the same content is a no-op if \
            it already landed.",
            action, budget
        )
    }

    /// Turn a [`clients::WatchdogOutcome`] — the mechanism-level result of
    /// [`clients::Clients::send_with_watchdog`], which owns bounding the two-phase upload wait
    /// itself (see that method's own doc for the `phase1_budget`/`phase2_budget` reasoning, review
    /// rounds S2-F1 through S2-F7) — into this call's own operator-facing error. The four outcomes
    /// map onto the three composers every other mutation transport failure already goes through:
    /// [`Self::describe_mutation_transport_error`] for a genuine `reqwest::Error`,
    /// [`Self::mutation_read_timeout_message`] for a send-phase watchdog kill, and
    /// [`Self::mutation_post_send_timeout_message`] for a post-send one — a successful send just
    /// unwraps to the response, status yet unchecked. Kept on `RemoteClient`, not folded into
    /// [`clients`], for the same reason those three composers are: wording is this type's job, not
    /// the client module's boundary — see [`clients`]'s own doc.
    fn response_from_watchdog(&self,
                              action: &str,
                              outcome: WatchdogOutcome) -> Result<reqwest::Response, String> {
        match outcome {
            WatchdogOutcome::Sent(response) => Ok(response),
            WatchdogOutcome::Transport(e) => Err(self.describe_mutation_transport_error(action, e)),
            WatchdogOutcome::SilentDuringSend => Err(Self::mutation_read_timeout_message(action)),
            WatchdogOutcome::SilentAfterSend { budget } => {
                Err(Self::mutation_post_send_timeout_message(action, budget))
            }
        }
    }

    /// Turn a [`clients::SendOutcome`] — the mechanism-level result of
    /// [`clients::Clients::send_on`] — into a *read* call's own operator-facing error. The read
    /// half of the pair [`Self::response_from_send_mutation`] completes, and the sibling of
    /// [`Self::response_from_watchdog`]: wording is `RemoteClient`'s job, not `mod clients`'s.
    ///
    /// Takes the exact `posture` the call was armed with, because
    /// [`Self::describe_transport_error`] renders its `ReadTimedOut` arm per posture — see that
    /// function's own doc. `Posture` is `Copy` precisely so a call site can bind one local, arm
    /// the request with it, and hand the same value here.
    ///
    /// [`clients::SendOutcome::HeadWaitExpired`] does **not** take `posture` into its wording at
    /// all: it routes to [`Self::head_wait_expired_message`], which already knows everything there
    /// is to know — only one posture can produce that outcome, and the outcome carries the exact
    /// figure that fired.
    ///
    /// The one read that does not use this is [`Self::resolve`], which is best-effort by contract
    /// and degrades every failure to an empty map — there is no message for it to compose.
    fn response_from_send(&self,
                          action: &str,
                          posture: Posture,
                          outcome: SendOutcome) -> Result<reqwest::Response, String> {
        match outcome {
            SendOutcome::Sent(response) => Ok(response),
            SendOutcome::Transport(e) => Err(self.describe_transport_error(action, posture, e)),
            SendOutcome::HeadWaitExpired { budget } => {
                Err(Self::head_wait_expired_message(action, budget))
            }
        }
    }

    /// The mutation counterpart of [`Self::response_from_send`], routing through
    /// [`Self::describe_mutation_transport_error`] instead. Takes no `Posture` for the same reason
    /// that composer doesn't: a mutation's transport wording does not vary by which bound (if any)
    /// actually fired — see [`Self::describe_mutation_transport_error`]'s own doc.
    ///
    /// Kept separate rather than folded into [`Self::response_from_send`] behind a flag: which of
    /// the two composers a call needs is a fact about the call, and a boolean parameter is exactly
    /// the shape that lets a new mutation quietly pick up the read wording — the wording that
    /// tells an operator nothing reached the remote.
    ///
    /// [`clients::SendOutcome::HeadWaitExpired`] takes [`Self::mutation_read_timeout_message`],
    /// not the read path's [`Self::head_wait_expired_message`]: the head-wait timer covers request
    /// transmission, so on a mutation an expiry means the request may have fully arrived and
    /// already had effect — precisely the uncertainty that wording exists to carry, and it is
    /// shared here word-for-word rather than restated for the same reason a mid-body watchdog kill
    /// shares it. **No mutation in this module arms a head deadline today**, so this arm has no
    /// current caller; it exists for totality, and it is the arm that must not be "simplified"
    /// into the read wording when one appears.
    fn response_from_send_mutation(&self,
                                   action: &str,
                                   outcome: SendOutcome) -> Result<reqwest::Response, String> {
        match outcome {
            SendOutcome::Sent(response) => Ok(response),
            SendOutcome::Transport(e) => Err(self.describe_mutation_transport_error(action, e)),
            SendOutcome::HeadWaitExpired { .. } => Err(Self::mutation_read_timeout_message(action)),
        }
    }

    /// Compose a loud, specific error for a `3xx` response to a mutation. Every call site that
    /// reaches this (`upload_object`, `put_presigned`, `update_ref`, `upload_signature`,
    /// `put_trust` — FORK-89 widened this from the original two) goes out on the never-auto-follow
    /// client ([`Posture::UnboundedNoRedirect`] for `update_ref`, [`Posture::TotalDeadlineNoRedirect`]
    /// for `upload_signature`/`put_trust`, or — for `upload_object`/`put_presigned` —
    /// directly via [`clients::Clients::send_with_watchdog`]), which never
    /// auto-follows *any* redirect status — so this is a local, unconditional invariant
    /// of *this client*, not a claim about how any particular `3xx` happens to behave under a
    /// dependency's redirect matrix. Do not turn it back into one: an earlier version of this doc
    /// asserted exactly that (which status codes `tower-http`'s auto-following policy skips for a
    /// given method) and was false for `303` — `follow_redirect`'s `SEE_OTHER` arm forces the body
    /// empty and the method to `GET` unconditionally, unlike the `MOVED_PERMANENTLY | FOUND` arm,
    /// which only does so for a `POST` — the hole FORK-89 closed. The guarantee here holds because
    /// of the client selection, not because of which status codes a dependency version happens to
    /// redirect on.
    ///
    /// A silent redirect must never look like success: an operator seeing a bare "refused (307)"
    /// would have no idea a redirect was even involved, and for a streamed upload the body is a
    /// one-shot stream that structurally cannot be replayed at a new target even if the caller
    /// wanted to retry there. Names the status and, when present, the `Location` header the remote
    /// pointed at, so the caller can see exactly what happened rather than guess.
    fn describe_mutation_redirect(action: &str, response: &reqwest::Response) -> String {
        let status = response.status();
        let location = response.headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("(no Location header)");

        format!(
            "The remote redirected {} ({}) to {} — a mutation must never silently follow a \
            redirect (retrying at an unannounced target risks applying somewhere the caller never \
            approved), so this failed instead of retrying at the new location. Check the remote's \
            configured URL and retry.",
            action, status.as_u16(), location
        )
    }

    /// The actual error-body-read bound to arm for *this* client instance — folds this
    /// instance's own `connect_timeout` into [`error_body_read_budget`]'s arithmetic (review
    /// round 5 finding 2). Split out as its own `&self` method, rather than inlining the call to
    /// the free function at the one call site, so a test can pin that the right instance field is
    /// actually read: not just read from *some* client (round 6 finding 1's fix was itself
    /// unpinned in that dimension — every behavioral test of [`Self::error_of`] necessarily used a
    /// direct client, so nothing distinguished "reads `self.connect_timeout`" from "hardcodes the
    /// direct 5s constant"), but read from an instance whose `connect_timeout` a production
    /// constructor could never have produced (round 7 found the *next* fixture — a `TorMode::On`
    /// one — had the identical problem one level up, at 60s instead of 5s) — see
    /// `error_body_budget_reads_this_field_not_a_rival_constant` in the test module.
    fn error_body_budget(&self) -> std::time::Duration {
        error_body_read_budget(self.connect_timeout)
    }

    /// Turn a non-success response into the client-facing error, threading the server's refusal
    /// code (§7.4) through the taxonomy when the body carries one. The body read is bounded by
    /// [`Self::error_body_budget`] (review round 5 finding 2, so this instance's own
    /// `connect_timeout` is folded in) — see that method's and [`error_body_read_budget`]'s docs
    /// for why this call in particular needs its own bound rather than inheriting one from
    /// whichever client sent the request, and why the flat [`ERROR_BODY_READ_TIMEOUT`] alone is
    /// not it.
    async fn error_of(&self, response: reqwest::Response, action: &str) -> String {
        let status = response.status();
        let budget = self.error_body_budget();

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
        let posture = Posture::BoundedReads;
        let outcome = self.send_on(
            posture, reqwest::Method::GET, "/v1/warehouse", SendBody::Empty
        ).await;
        let response = self.response_from_send(
            &format!("reaching the remote {}", self.base), posture, outcome
        )?;

        if !response.status().is_success() {
            return Err(self.error_of(response, "the handshake").await);
        }

        let info: WarehouseInfo = response.json()
            .await
            .map_err(|e| if e.is_timeout() {
                self.describe_transport_error(
                    &format!("reaching the remote {}", self.base), posture, e
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

    /// The total-deadline budget to arm for *this* client instance's call to
    /// [`Self::missing_objects`] or [`Self::upload_targets`], for one chunk of `n` hashes — folds
    /// this instance's own `connect_timeout` into the arithmetic, the same shape
    /// [`Self::error_body_budget`]/[`Self::resolve_budget`] already carry (`self.connect_timeout`
    /// plus a post-connect addend), for the identical reason: a Tor dial's 60s connect allowance
    /// must never be undercut by a budget sized for a direct remote, or the deadline expires during
    /// circuit build on every call, every retry — see
    /// `presence_negotiation_budget_reads_this_field_not_a_rival_constant` in the test module.
    ///
    /// `POST_SEND_VERIFY_BASE` covers dispatch/scheduling overhead — reused rather than minting a
    /// second constant for the identical role it already plays for the upload post-send phase.
    /// `n * PRESENCE_ALLOWANCE_MS_PER_OP` prices the chunk's own per-hash presence-check cost.
    ///
    /// Sound as a *total*, non-resetting deadline — where the same shape is *not* sound for
    /// `fetch_batch`'s bundle response (see [`Posture::HeadDeadlineNoRedirect`]'s doc for the
    /// two-phase shape it takes instead) or for `fetch_subtree` (see
    /// [`UnboundedTicket::Fork92`]'s own doc) — specifically
    /// because `n` is never a guess: it is `batch.len()`, the exact size of the request body this
    /// call just sent, itself capped at `MAX_MISSING_BATCH`/`MAX_UPLOAD_TARGETS_BATCH`. There is no
    /// large-but-healthy response at a larger `n` this budget could mistake for a stall, because no
    /// response larger than what `n` already prices is possible — unlike an object-bundle fetch,
    /// whose response size the request does not bound at all.
    fn presence_negotiation_budget(&self, n: usize) -> std::time::Duration {
        self.connect_timeout + POST_SEND_VERIFY_BASE + std::time::Duration::from_secs_f64(
            n as f64 * PRESENCE_ALLOWANCE_MS_PER_OP / 1000.0
        )
    }

    /// Ask which of the given objects the remote lacks (batched).
    ///
    /// Deliberately **not** on [`Posture::BoundedReads`]: the server side consults up to
    /// `MAX_MISSING_BATCH` (10,000) hashes before its first response byte, work that scales with
    /// the batch rather than being O(constant) — the settled contract puts that in the
    /// scaled/measured-budget category, not the flat one `REMOTE_READ_TIMEOUT` is honest for.
    /// Rides [`Posture::TotalDeadline`] instead, sized per batch by
    /// [`Self::presence_negotiation_budget`]. Accepted residual: a remote whose per-hash presence
    /// check genuinely exceeds [`PRESENCE_ALLOWANCE_MS_PER_OP`] — an overloaded or pathologically
    /// slow deployment — gets abandoned rather than waited out; this call's error is already a
    /// plain, retry-safe `Err`, so an ordinary retry is the recovery path, not a looser budget.
    pub async fn missing_objects(&self, hashes: &[String]) -> Result<Vec<String>, String> {
        let mut missing: Vec<String> = Vec::new();

        for batch in hashes.chunks(MAX_MISSING_BATCH) {
            let posture = Posture::TotalDeadline(self.presence_negotiation_budget(batch.len()));
            let outcome = self.send_on(
                posture,
                reqwest::Method::POST, "/v1/objects/missing",
                SendBody::json(&MissingObjectsRequest { hashes: batch.to_vec() })?,
            ).await;
            let response = self.response_from_send(
                "negotiating with the remote", posture, outcome
            )?;

            if !response.status().is_success() {
                return Err(self.error_of(response, "the negotiation").await);
            }

            let body: MissingObjectsResponse = response.json()
                .await
                .map_err(|e| if e.is_timeout() {
                    self.describe_transport_error("negotiating with the remote", posture, e)
                } else {
                    format!("The remote's negotiation response is not valid JSON: {}", e)
                })?;

            missing.extend(body.missing);
        }

        Ok(missing)
    }

    /// The total-deadline budget to arm for *this* client instance's call to [`Self::resolve`] —
    /// folds this instance's own `connect_timeout` into [`bounded_read_timeout`]'s arithmetic.
    /// Split out as its own `&self` method, rather than inlining the call to
    /// `bounded_read_timeout` at `resolve`'s one call site, so a test can pin that the right
    /// instance field is actually read: not just read from *some* client. [`Self::error_body_budget`]
    /// carries the identical shape (`self.connect_timeout` plus a fixed post-connect addend) for a
    /// sibling budget, and its own doc explains why that distinction needs a `connect_timeout` no
    /// production constructor can produce — the direct and Tor-mode constructors only ever emit
    /// [`REMOTE_CONNECT_TIMEOUT`] or [`REMOTE_CONNECT_TIMEOUT_TOR`], so a test built through either
    /// one can never separate "reads this field" from "hardcodes whichever of those two constants
    /// that fixture happens to carry." See `resolve_budget_reads_this_field_not_a_rival_constant`
    /// and `resolve_budget_is_70s_for_a_real_tor_mode_client` in the test module: the same
    /// two-test pair `error_body_budget` carries, and for the same reason — neither subsumes the
    /// other, see `error_body_budget_reads_this_field_not_a_rival_constant`'s own doc for why.
    fn resolve_budget(&self) -> std::time::Duration {
        bounded_read_timeout(self.connect_timeout, REMOTE_READ_TIMEOUT)
    }

    /// Resolve operator identifiers to display names through the server
    /// (`POST /v1/resolve`). Best-effort by the resolution failure policy: a server
    /// without a resolution hook (or that predates the endpoint, a `404`), an
    /// unreachable remote, or a malformed answer all resolve to an empty map — the
    /// caller shows the pseudonymous identifiers. The *server* decides which names
    /// this caller may see (§8.12); the client only asks.
    ///
    /// Rides [`Posture::TotalDeadline`], carrying `connect_timeout + REMOTE_READ_TIMEOUT` — about
    /// 15s direct, about 70s over Tor, a genuine **total** per-request deadline
    /// (`RequestBuilder::timeout`) applied by [`clients::Clients::send_on`] itself. Deliberately *not*
    /// [`Posture::BoundedReads`], even though it reuses [`REMOTE_READ_TIMEOUT`]'s value — and not
    /// because a silence budget fails to cover total silence: `BoundedReads`'s own pre-header
    /// deadline is fixed and non-resetting (see [`REMOTE_READ_TIMEOUT`]'s own doc), so it already
    /// terminates correctly against a remote that never answers at all. What it cannot cover is
    /// the opposite shape: a remote that *does* start answering and then trickles bytes slowly,
    /// forever, resetting the clock on every byte (the whole FORK-49 contract: a transfer moving
    /// bytes, however slowly, is never silence). Riding `BoundedReads` would have removed
    /// `resolve`'s only real *total* bound and replaced it with none: a remote trickling one byte
    /// every few seconds, once past headers, would keep the silence clock perpetually reset and
    /// the call alive forever, reopening exactly the FORK-49 hang this module exists to close, on
    /// a direct remote as much as a Tor one — proven directly by calling `resolve` itself against
    /// exactly that shape, [`tests::resolve_gives_up_on_a_remote_that_never_stops_trickling`], not
    /// merely argued. (`tests::send_on_applies_a_total_deadline_that_ignores_progress` pins
    /// the seam's own wiring the same way, but never calls `resolve` — it stays green
    /// whatever posture `resolve`'s call site names, so it does not by itself prove this claim.)
    ///
    /// It used to carry a flat `RequestBuilder::timeout(5s)` at the call site instead — also a
    /// total deadline, but structurally incapable of covering a Tor dial: per `reqwest`'s own
    /// docs that timeout starts at connect, and 5s is smaller than the connect allowance this
    /// same file deliberately grants a Tor dial ([`REMOTE_CONNECT_TIMEOUT_TOR`] = 60s, because —
    /// this file's own stated reason — circuit build alone can legitimately take tens of
    /// seconds). So on Tor the old 5s deadline could expire before the request was ever sent;
    /// how often it actually did is not something this file measures or claims. The other defect
    /// the old 5s shared with `OwnTimeoutFollowsRedirects`, the posture it rode: the bound was a
    /// call-site promise nothing checked, not a payload the module itself applied. Both defects
    /// are fixed the same way — a real `Duration` payload the seam reads and applies.
    ///
    /// **Accepted residual:** `/v1/resolve`'s own pre-first-byte server work can legitimately run
    /// close to this 15s direct budget on its own, before any network latency at all.
    /// `forklift-server/src/server.rs`'s hook client is built with a flat 10s timeout
    /// (`server.rs:288-289`), and a single call can pay that twice: `post_resolve` first calls
    /// `check_auth` (`server.rs:1775`, defined at `server.rs:533`), which on a bearer-token
    /// auth-cache miss makes its own hook round trip via `authenticate_via_hook`
    /// (`server.rs:569`) — up to 10s — before `post_resolve` makes its *own* resolution-hook call
    /// (`server.rs:1803`) — up to another 10s. A 15s total deadline can therefore abandon a
    /// request a slow-hook deployment is still legitimately serving. Accepted deliberately:
    /// `resolve` is cosmetic display sugar and its fallback to pseudonyms is free (never a
    /// command failure), so giving up early is the right failure for this one call — unlike
    /// `fetch_info`/`fetch_signature`/`fetch_bundle_to`, whose `BoundedReads` silence budget never
    /// has to make this trade because a *silence* bound cannot mistake a slow-but-real transfer
    /// for a hang in the first place.
    pub async fn resolve(&self, identifiers: Vec<String>) -> BTreeMap<String, String> {
        if identifiers.is_empty() {
            return BTreeMap::new();
        }

        let total_deadline = self.resolve_budget();
        // Every failure degrades to the designed fallback, including a serialization failure the
        // seam now surfaces up front rather than deferring into a `reqwest::Error` — see this
        // function's own doc for why an empty map is the right answer to all of them here.
        let Ok(body) = SendBody::json(&ResolveRequest { identifiers }) else {
            return BTreeMap::new();
        };

        let SendOutcome::Sent(response) = self.send_on(
            Posture::TotalDeadline(total_deadline), reqwest::Method::POST, "/v1/resolve", body
        ).await else {
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

    /// The head-wait budget to arm for *this* client instance's [`Self::fetch_batch`] `POST` —
    /// folds this instance's own `connect_timeout` into [`BATCH_HEAD_PATIENCE`], the same shape
    /// [`Self::error_body_budget`]/[`Self::resolve_budget`]/[`Self::presence_negotiation_budget`]/
    /// [`Self::single_write_budget`] already carry, for the identical reason: a Tor dial's 60s
    /// connect allowance must never be undercut by a budget sized for a direct remote, or the bound
    /// expires during circuit build on every call and every retry. 50s direct, 105s over Tor.
    ///
    /// Being `connect_timeout + a positive addend` is also what discharges the producer rule this
    /// posture's *wording* depends on — see [`clients::clamp_head_deadline_payload`]: a payload at
    /// or below `connect_timeout` would let the head timer fire during connect, and
    /// [`Self::head_wait_expired_message`] would then assert an established connection that never
    /// existed. This is the only producer of such a payload, and it satisfies the rule by
    /// construction; the seam clamps anyway, because "by construction" is a property of today's
    /// producers rather than of the released binary.
    ///
    /// Its own `&self` method rather than inline arithmetic at `fetch_batch`'s call site, so a test
    /// can pin that the right instance field is actually read: not just *some* client's. See
    /// `batch_head_budget_reads_this_field_not_a_rival_constant` and
    /// `batch_head_budget_is_105s_for_a_real_tor_mode_client` in the test module — the same
    /// two-test pair each sibling budget carries, neither subsuming the other.
    fn batch_head_budget(&self) -> std::time::Duration {
        self.connect_timeout + BATCH_HEAD_PATIENCE
    }

    /// Fetch many objects in one round trip as a bundle-format stream
    /// (`POST /v1/objects/batch`). `None` when the remote predates the endpoint
    /// (a `404`) — the caller falls back to loose fetches.
    ///
    /// An offloading (storage-backed) head cannot stream a large bundle back through its own
    /// control plane, so it answers this `POST` with a redirect to a presigned `GET` of the
    /// bundle bytes under an ephemeral response key (`303 See Other` from a fixed head; a
    /// `307`/`308` from an older one is followed identically). The redirect is followed **by
    /// hand**, never by reqwest's automatic policy (this call goes out on a client whose redirect
    /// policy is `none`, selected by the posture named two paragraphs below and deliberately not
    /// restated here — a posture named twice in one doc is a posture that rots once): a `307`/`308`
    /// replays
    /// the original request verbatim — method and JSON body — which would re-`POST` this call's
    /// body at a URL SigV4-signed for `GET` only, failing signature verification (`500` on
    /// LocalStack, `403 SignatureDoesNotMatch` on real AWS) rather than fetching anything. The
    /// follow-up `GET` also deliberately omits this remote's `Authorization` header: the
    /// presigned URL is self-authorizing, and forwarding a bearer token meant for the control
    /// plane to a storage host it was never issued for would be a needless credential leak — see
    /// [`Self::send_on_presigned`]'s doc for why this call reaches that URL through it rather
    /// than through [`Self::send_on`].
    ///
    /// The initial `POST` above is bounded in **two phases, by two mechanisms** — it rides
    /// [`Posture::HeadDeadlineNoRedirect`], whose head-wait timer is sized by
    /// [`Self::batch_head_budget`] and whose client carries a [`FETCH_OBJECT_READ_TIMEOUT`]
    /// silence budget for everything after the header section. Deliberately
    /// **not** [`Posture::BoundedReads`], and deliberately not a *total* deadline either: the
    /// server builds the whole requested bundle — every object fully into memory — before its
    /// first response byte (`forklift-server/src/server.rs`'s `post_objects_batch`;
    /// `forklift-aws-lambda`'s `Head::batch` likewise, in both of its branches), and that
    /// cost depends on the byte sizes of objects this client doesn't have yet. No flat budget over
    /// the *build* is honest, and none is armed over it: the head-wait timer prices no server work
    /// at all, and being strictly the tighter of the two it always fires first, so the silence
    /// budget only ever sees a body whose bytes were finished before the first one was written.
    /// Because that budget resets on every byte, a large bundle over a slow link is never killed
    /// by it. **Neither phase can wait indefinitely on a silent remote; neither bounds the call's
    /// duration** — a remote trickling one byte per interval keeps this `POST` alive as long as it
    /// likes, which is the accepted price of a silence budget rather than a gap left open.
    ///
    /// What that buys is the failure this call previously had no defence against at all: a remote
    /// that accepted the connection and then never answered — before the status line or after it —
    /// hung the calling command forever, with
    /// no output and no way out short of killing the process. What it costs is stated in
    /// [`BATCH_HEAD_PATIENCE`]'s own doc, and it **reverses** what an earlier version of this
    /// paragraph promised: a head legitimately slower than that patience now fails identically on
    /// every retry rather than being waited out. Accepted knowingly — a wedge and an over-budget
    /// build are indistinguishable from here, loud failure is the lesser wrong, and against the one
    /// head shape whose slow builds are structurally capped the case cannot arise at all.
    ///
    /// The redirect-follow `GET` below does not share that reasoning: an offloading store writes
    /// the bundle to its storage-backed response key and only *then* presigns and returns the
    /// `GET` URL (`crates/forklift-aws-lambda/src/aws/s3.rs`'s `offload_response`, called from
    /// `head.rs`'s `batch` after `build_partial_bundle` already ran) — so by the time this station
    /// is reached, the bytes it reads are already fully materialized at a known size, not still
    /// being assembled by whatever answers the request. That is the same shape
    /// [`Self::fetch_object`]'s own read is, not the size-unknown-in-advance one the `POST` above
    /// has, so it rides [`Posture::BoundedObjectReads`] — the same [`FETCH_OBJECT_READ_TIMEOUT`]
    /// silence budget, which resets on every byte received and so rides out a large-but-progressing
    /// bundle, only ever cutting off a genuinely stalled one. **The two stations reach the same
    /// figure by different arguments, and share one composer call under one action string**, which
    /// is why their [`Self::describe_transport_error`] arms have to be told apart by wording — see
    /// that function's doc.
    pub async fn fetch_batch(&self, hashes: &[String]) -> Result<Option<Vec<u8>>, String> {
        let post_posture = Posture::HeadDeadlineNoRedirect { head: self.batch_head_budget() };
        let outcome = self.send_on(
            post_posture,
            reqwest::Method::POST, "/v1/objects/batch",
            SendBody::json(&MissingObjectsRequest { hashes: hashes.to_vec() })?,
        ).await;
        let response = self.response_from_send(
            "batch-fetching from the remote", post_posture, outcome
        )?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        // `body_posture` follows whichever station actually produced the response bytes below:
        // the `POST` above when there was no redirect to follow — whose client-level silence
        // budget is what governs the phase these bytes are read in — or the redirect-follow
        // `GET`'s own silence budget when there was. The two budgets are the same size and reached
        // by different arguments, so a single posture would render the right figure with the wrong
        // provenance, and the composer's two arms would have nothing left to tell apart.
        let (response, body_posture) = match response.status() {
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
                // body — the request the redirect target is actually presigned for. On
                // [`Posture::BoundedObjectReads`], reaching the same silence budget the `POST`
                // above reaches through its own posture, but on its own grounds — see this
                // function's own doc for why this station's cost shape matches
                // [`Self::fetch_object`]'s, not the `POST`'s.
                let redirect_posture = Posture::BoundedObjectReads;
                let outcome = self.send_on_presigned(
                    redirect_posture, reqwest::Method::GET, &location, SendBody::Empty
                ).await;
                let response = self.response_from_send(
                    "following the batch redirect", redirect_posture, outcome
                )?;
                (response, redirect_posture)
            }
            _ => (response, post_posture),
        };

        if !response.status().is_success() {
            return Err(self.error_of(response, "the batch fetch").await);
        }

        response.bytes()
            .await
            .map(|bytes| Some(bytes.to_vec()))
            .map_err(|e| self.describe_transport_error("reading the batch response", body_posture, e))
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
    /// Deliberately **not** on [`Posture::BoundedReads`]: the server side walks and buffers the
    /// whole resolved subtree closure into memory before its first response byte
    /// (`forklift-server/src/server.rs`'s `get_subtree` handler notes an uncapped closure "would
    /// buffer an arbitrarily large bundle in memory"), cost the client cannot bound in advance.
    /// It shares that much with [`Self::fetch_batch`], and no longer shares its fate: that call's
    /// `POST` took the head-wait exit ([`Posture::HeadDeadlineNoRedirect`]) because it hangs
    /// against a wedged remote in production today, while this one has no production caller at
    /// all, so a budget here would price nothing reachable. This stays unbounded until it has its
    /// own scaled budget or an abandon-and-fall-back lane — or until a production caller appears,
    /// which is the trigger to reconsider rather than a reason to guess now.
    pub async fn fetch_subtree(&self, parcel: &str, path: &str) -> Result<Option<Vec<u8>>, String> {
        let posture = Posture::UnboundedFollowsRedirects(UnboundedTicket::Fork92);
        let outcome = self.send_on(
            posture,
            reqwest::Method::GET, &format!(
                "/v1/parcels/{}/subtree/{}", parcel, encode_path_segments(path)
            ),
            SendBody::Empty,
        ).await;
        let response = self.response_from_send(
            &format!("fetching subtree \"{}\" from the remote", path), posture, outcome
        )?;

        if endpoint_absent(response.status()) || response.status() == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
            return Ok(None);
        }

        if !response.status().is_success() {
            return Err(self.error_of(response, &format!("the subtree fetch for \"{}\"", path)).await);
        }

        response.bytes()
            .await
            .map(|bytes| Some(bytes.to_vec()))
            .map_err(|e| self.describe_transport_error("reading the subtree response", posture, e))
    }

    /// Fetch one object's raw bytes. On [`Posture::BoundedObjectReads`], not [`Posture::BoundedReads`]
    /// — see [`FETCH_OBJECT_READ_TIMEOUT`]'s doc for why this call needs the looser budget.
    pub async fn fetch_object(&self, hash: &str) -> Result<Vec<u8>, String> {
        let posture = Posture::BoundedObjectReads;
        let outcome = self.send_on(
            posture, reqwest::Method::GET, &format!("/v1/objects/{}", hash), SendBody::Empty
        ).await;
        let response = self.response_from_send(
            &format!("fetching object {}", hash), posture, outcome
        )?;

        if !response.status().is_success() {
            return Err(self.error_of(response, &format!("object {}", hash)).await);
        }

        response.bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|e| self.describe_transport_error(
                &format!("reading object {}", hash), posture, e
            ))
    }

    /// Upload one object's raw bytes to the control plane (`PUT /v1/objects/{hash}`), where the
    /// remote verifies the hash inline before the object becomes fetchable. This is the direct
    /// path — for the objects `upload-targets` returns in `direct`, and the whole missing set on
    /// the legacy fallback.
    ///
    /// The body streams through [`clients::Clients::send_with_watchdog`] (FORK-49 slice 2, moved
    /// off a caller-assembled builder onto this operation by a later slice — see that method's own
    /// doc for why): a remote that accepts the connection and then never reads the body must not
    /// hang this call forever, but a per-request *total* deadline would kill a healthy large
    /// upload on a slow link — the same reasoning [`REMOTE_READ_TIMEOUT`]'s doc gives for the read
    /// path, applied to the send side instead of the receive side. A remote that reads the whole
    /// body and then wedges (during the inline hash-verify this endpoint's own doc names above) is
    /// bounded too — see [`clients::Clients::send_with_watchdog`]'s doc for that phase's own,
    /// size-scaled bound instead of the unbounded wait an earlier version of this fix left it with.
    pub async fn upload_object(&self, hash: &str, bytes: Vec<u8>) -> Result<(), String> {
        let action = format!("uploading object {}", hash);
        let path = format!("/v1/objects/{}", hash);
        let outcome = self.clients.send_with_watchdog(
            self.connect_timeout,
            RequestDestination::Authenticated { base: &self.base, token: self.token.as_deref(), path: &path },
            reqwest::Method::PUT,
            bytes,
        ).await;
        let response = self.response_from_watchdog(&action, outcome)?;

        if response.status().is_redirection() {
            return Err(Self::describe_mutation_redirect(&action, &response));
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
    /// Deliberately **not** on [`Posture::BoundedReads`], the same reasoning as
    /// [`Self::missing_objects`]: the server side (`forklift-server/src/server.rs`'s
    /// `post_upload_targets`) walks up to `MAX_UPLOAD_TARGETS_BATCH` (1,000) hashes, checking each
    /// against on-disk object presence, before its first response byte — work that scales with the
    /// batch rather than being O(constant). Rides [`Posture::TotalDeadline`] like
    /// `missing_objects`, sized per batch by [`Self::presence_negotiation_budget`] — same accepted
    /// residual: a per-hash presence check genuinely slower than [`PRESENCE_ALLOWANCE_MS_PER_OP`]
    /// gets abandoned rather than waited out, recovered by an ordinary retry rather than a looser
    /// budget.
    pub async fn upload_targets(&self,
                                session: &str,
                                hashes: &[String]) -> Result<Option<UploadTargetsResponse>, String> {
        let mut merged = UploadTargetsResponse {
            present: Vec::new(),
            targets: BTreeMap::new(),
            direct: Vec::new(),
        };

        for batch in hashes.chunks(MAX_UPLOAD_TARGETS_BATCH) {
            let posture = Posture::TotalDeadline(self.presence_negotiation_budget(batch.len()));
            let outcome = self.send_on(
                posture,
                reqwest::Method::POST, "/v1/objects/upload-targets",
                SendBody::json(&UploadTargetsRequest {
                    session: session.to_string(), hashes: batch.to_vec()
                })?,
            ).await;
            let response = self.response_from_send(
                "negotiating upload targets", posture, outcome
            )?;

            if endpoint_absent(response.status()) {
                return Ok(None);
            }

            if !response.status().is_success() {
                return Err(self.error_of(response, "the upload negotiation").await);
            }

            let body: UploadTargetsResponse = response.json()
                .await
                .map_err(|e| if e.is_timeout() {
                    self.describe_transport_error("negotiating upload targets", posture, e)
                } else {
                    format!("The remote's upload-targets response is not valid JSON: {}", e)
                })?;

            merged.present.extend(body.present);
            merged.targets.extend(body.targets);
            merged.direct.extend(body.direct);
        }

        Ok(Some(merged))
    }

    /// Upload one object's bytes straight to a presigned storage URL (a staging `PUT`). The
    /// URL's own signature is the authorization, so this deliberately carries **no** bearer
    /// token: [`RequestDestination::Presigned`] carries no token field to attach, so
    /// [`clients::Clients::send_with_watchdog`] structurally cannot attach one — there is no
    /// `Option<&str>` for a future edit to accidentally start populating on this path, the way
    /// there would be if this call reused [`RequestDestination::Authenticated`] with the token set
    /// to `None`. The no-auto-follow client this rides (moved off the auto-following client in the
    /// fix for the `303` redirect hole) cannot leak it to the storage host either, even were the
    /// storage host the remote itself: it is built (in [`Self::new_with_tor`]) with no default
    /// headers of its own, exactly like the auto-following one.
    ///
    /// Same watchdog-guarded body as [`Self::upload_object`] (FORK-49 slice 2) — see that call's
    /// doc. This site is the higher-risk of the two: it dials a different host (object storage,
    /// not the control plane) and the explicit `Content-Length`
    /// [`clients::Clients::send_with_watchdog`] sets internally matters especially here — a
    /// presigned S3 `PUT` is signed for a specific framing, and `Transfer-Encoding: chunked`
    /// (what `reqwest`/hyper fall back to without an explicit length on a streamed body) is not
    /// it; S3 rejects a chunked presigned `PUT` outright.
    async fn put_presigned(&self, url: &str, bytes: Vec<u8>) -> Result<(), String> {
        let action = "uploading to a staging URL";
        let outcome = self.clients.send_with_watchdog(
            self.connect_timeout,
            RequestDestination::Presigned { url },
            reqwest::Method::PUT,
            bytes,
        ).await;
        let response = self.response_from_watchdog(action, outcome)?;

        if response.status().is_redirection() {
            return Err(Self::describe_mutation_redirect(action, &response));
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
    ///
    /// **Two standing gaps, recorded not silently absorbed — distinct facts, neither implies the
    /// other:**
    ///
    /// 1. **Response wait is unbounded** (rides [`Posture::UnboundedFollowsRedirects`],
    ///    [`UnboundedTicket::Fork91`]): this call's error-body read decides retry-vs-terminal
    ///    control flow, so bounding it is a retry-contract design question — FORK-91's own scope,
    ///    still open.
    /// 2. **No redirect guard** (a *separate* axis from 1, not covered by fixing it): this is
    ///    this module's one remaining mutation that still rides the auto-following client with no
    ///    `is_redirection()` check. FORK-89 moved every other mutation (`update_ref`,
    ///    `upload_object`, `put_presigned`, `upload_signature`, `put_trust`) onto the
    ///    never-auto-follow client because reqwest's default redirect policy, combined with
    ///    `tower-http`'s `follow_redirect` middleware, silently turns a `3xx` into a bare `GET`
    ///    whose `2xx` at the target reads back as a fabricated success — **but FORK-89 shipped
    ///    without `commit_lift`**: its stated scope named only those five call sites, and this one
    ///    was left out as a known gap rather than folded in silently (see the auto-following
    ///    client's own doc, [`Clients::http`]). Do not read gap 1's ticket (FORK-91) as covering
    ///    this: FORK-91 is about *how long* to wait, not *whether a redirect is trusted* — closing
    ///    one leaves the other exactly as open. No ticket currently owns gap 2 on its own.
    ///
    /// Gap 2, concretely: an ALB or storage-backed head that answers this `POST` with a `303` gets
    /// auto-followed as a bare `GET`; a `2xx` at whatever the `Location` header names makes this
    /// function return `Ok(Committed)` for a session whose objects were never actually verified or
    /// promoted. `Ok(BlobNotReady)`'s eventual promotion is unaffected — the exposure is
    /// specifically a redirected `2xx` masquerading as a genuine commit response.
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

        let outcome = self.send_on(
            Posture::UnboundedFollowsRedirects(UnboundedTicket::Fork91),
            reqwest::Method::POST, &format!("/v1/lift/{}/commit", session),
            SendBody::json(&body)?,
        ).await;
        let response = self.response_from_send_mutation("committing the lift session", outcome)?;

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
        let posture = Posture::BoundedReads;
        let outcome = self.send_on(
            posture, reqwest::Method::GET, &format!("/v1/signatures/{}", parcel_hash),
            SendBody::Empty,
        ).await;
        let response = self.response_from_send(
            &format!("fetching the signature of {}", parcel_hash), posture, outcome
        )?;

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
                &format!("reading the signature of {}", parcel_hash), posture, e
            ))
    }

    /// The total-deadline budget to arm for *this* client instance's call to
    /// [`Self::upload_signature`] or [`Self::put_trust`] — folds this instance's own
    /// `connect_timeout` into [`SINGLE_WRITE_ALLOWANCE`]'s arithmetic, the same shape
    /// [`Self::error_body_budget`]/[`Self::resolve_budget`]/[`Self::presence_negotiation_budget`]
    /// already carry (`self.connect_timeout` plus a post-connect addend), for the identical
    /// reason: a Tor dial's 60s connect allowance must never be undercut by a budget sized for a
    /// direct remote. Split out as its own `&self` method rather than inlining the arithmetic at
    /// either call site, so a test can pin that the right instance field is actually read — see
    /// `single_write_budget_reads_this_field_not_a_rival_constant` in the test module. 30s direct,
    /// 85s over Tor.
    ///
    /// Sound as a *total*, non-resetting deadline for the reason [`SINGLE_WRITE_ALLOWANCE`]'s own
    /// doc gives: the dominant term on both calls' server sides is a fixed number of individually
    /// capped hooks, run after the body is already in hand — not a derived quantity like
    /// `update_ref`'s audit walk, which is why that call stays unbounded and these two do not.
    ///
    /// It is *not* the whole of either server side, and [`SINGLE_WRITE_ALLOWANCE`]'s doc lists the
    /// terms this arithmetic leaves as residuals rather than prices. Read that list before
    /// treating this budget as a ceiling on anything but the hook sequence.
    ///
    /// One consequence worth knowing at the call sites: a per-request total deadline spans the
    /// *error body* read too, so [`Self::error_of`]'s own budget is not the binding one once the
    /// server has already spent most of this. A refusal that arrives very late in the budget can
    /// have its body cut off, degrading a typed refusal code and its recovery guidance to the bare
    /// canonical status reason. Both calls were unbounded before, so that body always arrived;
    /// this is a real change, accepted because the alternative is the indefinite hang.
    fn single_write_budget(&self) -> std::time::Duration {
        self.connect_timeout + SINGLE_WRITE_ALLOWANCE
    }

    /// Upload a parcel's signature sidecar.
    ///
    /// Rides [`Posture::TotalDeadlineNoRedirect`], sized by [`Self::single_write_budget`] — not
    /// the auto-following client: this call mutates the remote, and this client never
    /// auto-follows a redirect on a mutation — a local, unconditional invariant of *this client*,
    /// holding regardless of which status code or dependency version is in play (see
    /// [`Self::describe_mutation_redirect`]'s doc). Before FORK-89 this rode the auto-following
    /// client with no `is_redirection()` guard, so a `303` (which `tower-http`'s `SEE_OTHER` arm
    /// forces to a bare `GET` unconditionally) silently landed at the redirect target instead of
    /// storing anything, and a `2xx` there read back as a fabricated success. Between FORK-89 and
    /// this fix it rode [`Posture::UnboundedNoRedirect`] — the redirect hole closed, but the
    /// response wait itself still unbounded — until [`Self::single_write_budget`]'s own doc
    /// argued a flat bound is honest for this call's server side too.
    pub async fn upload_signature(&self, parcel_hash: &str, bytes: Vec<u8>) -> Result<(), String> {
        let action = format!("uploading the signature of {}", parcel_hash);
        let outcome = self.send_on(
            Posture::TotalDeadlineNoRedirect(self.single_write_budget()),
            reqwest::Method::PUT, &format!("/v1/signatures/{}", parcel_hash),
            SendBody::Bytes(bytes),
        ).await;
        let response = self.response_from_send_mutation(&action, outcome)?;

        if response.status().is_redirection() {
            return Err(Self::describe_mutation_redirect(&action, &response));
        }

        if !response.status().is_success() {
            return Err(self.error_of(response, &format!("the signature of {}", parcel_hash)).await);
        }

        Ok(())
    }

    /// Establish the trust anchor on the remote (idempotent for an identical anchor).
    ///
    /// Rides [`Posture::TotalDeadlineNoRedirect`], sized by [`Self::single_write_budget`] — same
    /// reasoning as [`Self::upload_signature`]'s doc: a mutation on this client never
    /// auto-follows any redirect, unconditionally, and before FORK-89 this call had no such
    /// guard at all.
    ///
    /// **Accepted residual:** the server holds `warehouse.writes` — the same mutex the ref-update
    /// handler holds across closure verification, ancestry and the office-chain verify, work this
    /// module elsewhere documents can legitimately run minutes — for the whole of this call's
    /// handler, including its own read/write of the trust anchor. A first-contact `put_trust`
    /// racing another client's long first lift on the same warehouse can therefore exceed
    /// [`Self::single_write_budget`], however generous. Not absorbed by inflating the budget:
    /// minutes cannot be absorbed by a flat bound sized for O(constant) work. Accepted because it
    /// is rare (`put_trust` fires on first contact or re-genesis, not per lift), the failure is
    /// loud and correctly worded (the standard mutation-uncertainty message, not a hang), and a
    /// retry converges once the racing lift releases the lock.
    pub async fn put_trust(&self, anchor: &TrustAnchorDto) -> Result<(), String> {
        let action = "uploading the trust anchor";
        let outcome = self.send_on(
            Posture::TotalDeadlineNoRedirect(self.single_write_budget()),
            reqwest::Method::PUT, "/v1/trust",
            SendBody::json(anchor)?,
        ).await;
        let response = self.response_from_send_mutation(action, outcome)?;

        if response.status().is_redirection() {
            return Err(Self::describe_mutation_redirect(action, &response));
        }

        if !response.status().is_success() {
            return Err(self.error_of(response, "the trust anchor").await);
        }

        Ok(())
    }

    /// Commit a ref update (the CAS of a lift).
    ///
    /// Rides [`Posture::UnboundedNoRedirect`] (FORK-89) — same reasoning as
    /// [`Self::upload_signature`]'s doc. This call is the one of the three that is a `POST`, not
    /// a `PUT`: `tower-http`'s `MOVED_PERMANENTLY | FOUND` arm forces a `POST`'s method to `GET`
    /// and body to empty (unlike its `PUT` handling, which that arm leaves alone), so before
    /// FORK-89 this call was additionally exposed to a silent-success `301`/`302`, on top of the
    /// `303` every mutation on the auto-following client shared. Moving off it closes all of it
    /// the same way, uniformly.
    pub async fn update_ref(&self,
                            pallet: &str,
                            old_head: Option<&str>,
                            new_head: &str) -> Result<(), String> {
        let body = RefUpdateRequest {
            old_head: old_head.map(|hash| hash.to_string()),
            new_head: new_head.to_string(),
        };
        let action = format!("moving the remote pallet \"{}\"", pallet);

        let outcome = self.send_on(
            Posture::UnboundedNoRedirect(UnboundedTicket::Fork94),
            reqwest::Method::POST, &format!("/v1/pallets/{}", pallet),
            SendBody::json(&body)?,
        ).await;
        let response = self.response_from_send_mutation(&action, outcome)?;

        if response.status().is_redirection() {
            return Err(Self::describe_mutation_redirect(&action, &response));
        }

        if !response.status().is_success() {
            return Err(self.error_of(response, &format!("moving pallet \"{}\"", pallet)).await);
        }

        Ok(())
    }

    /// Download the remote's latest bundle into a file. On [`Posture::BoundedReads`]: the bundle
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
        let posture = Posture::BoundedReads;
        let outcome = self.send_on(
            posture, reqwest::Method::GET, "/v1/bundles/latest", SendBody::Empty
        ).await;
        let mut response = self.response_from_send("fetching the bundle", posture, outcome)?;

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
            .map_err(|e| self.describe_transport_error("downloading the bundle", posture, e))?
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
/// [`clients::Clients::send_with_watchdog`] loop go unpolled while their worker is pinned here — on
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
    /// This pins one of four links; on its own it pins only the helper's arithmetic, not that
    /// `error_of` actually calls it (review round 6, finding 1 — nothing enforced that link
    /// before `missing_objects_bounds_the_error_body_read_after_a_wedged_500` below started
    /// asserting a lower bound on elapsed time), not that the accessor computing the budget reads
    /// *this instance's* `connect_timeout` field at all, rather than hardcoding whichever constant
    /// a given fixture's client happens to carry (review round 7 — a fixture built through a
    /// production constructor can only ever carry `REMOTE_CONNECT_TIMEOUT` or
    /// `REMOTE_CONNECT_TIMEOUT_TOR`, so it cannot separate "reads the field" from "hardcodes that
    /// one value"; see `error_body_budget_reads_this_field_not_a_rival_constant`, which injects a
    /// value neither constructor can produce), and not that the value at the one client mode that
    /// actually ships with a non-default budget (`TorMode::On`) is the correct 70s rather than some
    /// other value a future change (e.g. a cap) could silently substitute (review round 8, finding
    /// 1; see `error_body_budget_is_70s_for_a_real_tor_mode_client`). All four together pin
    /// `error_of`'s real behavior, on any client, without a live remote.
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

    /// A remote that reads one request and answers it with a bare status line and an empty body —
    /// no redirect, no bundle, nothing to follow. Enough to drive the status-dispatch lanes of a
    /// call that has more than one of them.
    fn start_status_only_remote(status: u16, reason: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());

        std::thread::spawn(move || {
            use std::io::Write;

            if let Ok((mut stream, _)) = listener.accept() {
                let _ = read_test_request(&mut stream);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {} {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    status, reason
                );
                let _ = stream.flush();
            }
        });

        url
    }

    /// **The absence lane, which had no test at all until this one.** A head that predates the
    /// batch endpoint answers the `POST` with a `404`, and `fetch_batch` must turn that into
    /// `Ok(None)` — the signal that sends its caller to loose fetches.
    ///
    /// What makes this load-bearing rather than incidental coverage is the head-wait bound now
    /// armed on that same `POST`: an expiry must never be confused with absence. The two lanes
    /// share no code — `Ok(None)` is constructed at one point, after a response exists and its
    /// status reads `404`, while every transport failure returns before any status exists to read
    /// — but until this fixture existed, "share no code" was a reading of the source with nothing
    /// standing behind it. The production caller branches on `Some`/`None`/`Err` separately, so
    /// conflating expiry with absence would silently reroute a wedged head into the
    /// predates-the-endpoint fallback instead of failing the command.
    /// `fetch_batch_times_out_against_a_silent_remote` pins the other half: that an expiry is an
    /// `Err`, not a `None`.
    ///
    /// The `500` half is what makes this discriminate. An implementation that mapped *any*
    /// non-success status to `Ok(None)` — the plausible over-broad version of this lane — passes
    /// the `404` half alone and fails here, because a server error must surface as an error and
    /// not as "this head is too old".
    ///
    /// The only test that existed before this one exercises `endpoint_absent`, the *pure status
    /// predicate*, which `fetch_batch` does not even call — it compares the status itself.
    #[test]
    fn fetch_batch_maps_a_404_to_the_absence_signal_and_a_500_to_an_error() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

        let absent = RemoteClient::new(&start_status_only_remote(404, "Not Found"), None).unwrap();
        let outcome = runtime.block_on(absent.fetch_batch(&["a".repeat(64)]))
            .expect("a 404 is the absence signal, not a failure");
        assert!(
            outcome.is_none(),
            "a 404 must produce the absence signal the caller falls back on, not bundle bytes: \
            {:?}", outcome
        );

        let broken = RemoteClient::new(
            &start_status_only_remote(500, "Internal Server Error"), None
        ).unwrap();
        let error = runtime.block_on(broken.fetch_batch(&["a".repeat(64)]))
            .expect_err("a server error must not be reported as the endpoint being absent");
        assert!(
            error.contains("500"),
            "the error must name the status the remote actually sent. This assertion used to \
            carry an `|| contains(\"batch\")` disjunct, which could not fail: every error path \
            out of fetch_batch names the batch action, so the disjunct was satisfied by any Err \
            at all — including a head-wait expiry, which is precisely the outcome this fixture \
            exists to keep distinguishable from a real refusal: {}", error
        );
    }

    /// A remote that answers `POST /v1/objects/batch` with a `303` redirect to
    /// `/responses/bundle`, then — on the follow-up `GET` to that location — accepts the
    /// connection, reads the request, and genuinely goes silent: it never writes a byte of
    /// response. The redirect-follow station's own [`SilentRemote`]-equivalent: the first hop
    /// succeeds and hands back a real redirect, only the second hop — the one now bounded — stalls.
    struct RedirectThenSilentRemote {
        url: String,
        /// Owns the sender; dropping it at the end of the test is what finally unblocks the
        /// second hop's parked handler, letting it close the connection. Never signaled mid-test.
        _park: std::sync::mpsc::Sender<()>,
    }

    impl RedirectThenSilentRemote {
        fn start() -> RedirectThenSilentRemote {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let base = url.clone();
            let (tx, rx) = std::sync::mpsc::channel::<()>();

            std::thread::spawn(move || {
                use std::io::Write;

                if let Ok((mut stream, _)) = listener.accept() {
                    let _ = read_test_request(&mut stream);
                    let _ = write!(
                        stream,
                        "HTTP/1.1 303 See Other\r\nLocation: {}/responses/bundle\r\n\
                        Content-Length: 0\r\nConnection: close\r\n\r\n",
                        base
                    );
                    let _ = stream.flush();
                }

                if let Ok((mut stream, _)) = listener.accept() {
                    let _ = read_test_request(&mut stream);
                    let _ = rx.recv();
                    drop(stream);
                }
            });

            RedirectThenSilentRemote { url, _park: tx }
        }
    }

    /// The falsifying test for this station's bound: `fetch_batch`'s redirect-follow `GET` — the
    /// one that reads bundle bytes off a presigned storage URL — must fail with a timeout, not
    /// hang forever, against a remote that redirects and then goes silent. Before this fix this
    /// station rode [`Posture::UnboundedFollowsRedirects`], which this fixture would hang against
    /// forever.
    #[test]
    fn fetch_batch_redirect_follow_times_out_against_a_silent_remote() {
        let remote = RedirectThenSilentRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let hard_ceiling = TEST_DIRECT_CONNECT_TIMEOUT + TEST_LOOSE_READ_TIMEOUT
            + std::time::Duration::from_secs(20);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.fetch_batch(&["a".repeat(64)])).await
        });

        let message = outcome
            .unwrap_or_else(|_| panic!(
                "fetch_batch hung past the test's own {:?} ceiling — no timeout fired at all",
                hard_ceiling
            ))
            .expect_err("a redirect target that never answers must not appear to succeed");

        assert!(
            message.to_lowercase().contains("timed out"),
            "must fail specifically with a timeout, not some other transport error: {}", message
        );
    }

    /// Same two-hop shape as [`RedirectThenSilentRemote`], but the second hop serves the bundle
    /// as a steady drip instead of going silent — always moving, however slowly, never silent
    /// long enough to trip a silence budget, with a total duration comfortably past one. The
    /// redirect-follow station's own sibling to `start_steady_drip_remote`
    /// (`fetch_object_survives_a_slow_but_steadily_progressing_body`'s own fixture).
    fn start_batch_redirect_then_drip_remote(chunks: Vec<Vec<u8>>, gap: std::time::Duration) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let base = url.clone();

        std::thread::spawn(move || {
            use std::io::Write;

            if let Ok((mut stream, _)) = listener.accept() {
                let _ = read_test_request(&mut stream);
                let _ = write!(
                    stream,
                    "HTTP/1.1 303 See Other\r\nLocation: {}/responses/bundle\r\n\
                    Content-Length: 0\r\nConnection: close\r\n\r\n",
                    base
                );
                let _ = stream.flush();
            }

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

    /// The property that makes a loose *silence* budget — not a total deadline, and not the
    /// tighter [`Posture::BoundedReads`] silence budget — the right mechanism for this station: a
    /// bundle that dribbles in slowly but steadily, comfortably past the effective loose silence
    /// budget in total duration but never silent for anywhere near a single gap of it, must still
    /// succeed. Mirrors `fetch_object_survives_a_slow_but_steadily_progressing_body`'s own gap and
    /// chunk-count reasoning, applied to the redirect-follow station instead.
    #[test]
    fn fetch_batch_redirect_follow_survives_a_slow_but_steadily_progressing_bundle() {
        let gap = std::time::Duration::from_secs(20);
        let chunks: Vec<Vec<u8>> = (0..4).map(|i| format!("bundle-chunk-{}-drip", i).into_bytes()).collect();
        let expected: Vec<u8> = chunks.concat();
        // 4 gaps of 20s = 80s total, ~15s past the 65s effective loose budget (connect + read),
        // while no single gap comes anywhere near it either — the sole property this fixture
        // pins. Each gap (20s) does exceed the *tight* [`Posture::BoundedReads`] effective
        // budget (connect + read = 15s) — deliberately: that gap width is what makes the
        // BoundedReads mutation fail this test (confirmed by running it), the discriminating
        // property this fixture exists for.
        let total_duration = gap * (chunks.len() as u32);
        let effective_loose_budget = TEST_DIRECT_CONNECT_TIMEOUT + TEST_LOOSE_READ_TIMEOUT;
        assert!(
            total_duration > effective_loose_budget,
            "the fixture must actually outlast the bound under test"
        );

        let url = start_batch_redirect_then_drip_remote(chunks, gap);
        let client = RemoteClient::new(&url, None).unwrap();
        let outer_ceiling = total_duration + std::time::Duration::from_secs(20);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(outer_ceiling, client.fetch_batch(&["a".repeat(64)])).await
        });

        let bytes = outcome
            .unwrap_or_else(|_| panic!(
                "fetch_batch hung past its own generous outer ceiling {:?}", outer_ceiling
            ))
            .unwrap_or_else(|e| panic!(
                "a slow-but-steady bundle must succeed — it was never silent, so it must never be \
                treated as a stall: {}", e
            ));

        assert_eq!(
            bytes, Some(expected),
            "the full bundle must arrive intact despite the drip"
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
    /// Mirrors production `ERROR_BODY_READ_TIMEOUT` — its own constant, not
    /// `TEST_TIGHT_READ_TIMEOUT`, even though the two happen to share a value today:
    /// `TEST_TIGHT_READ_TIMEOUT` mirrors the unrelated `REMOTE_READ_TIMEOUT`, and the whole point
    /// of a per-production-constant mirror (see `TEST_TIGHT_READ_TIMEOUT`'s doc) is that an
    /// assertion built on it must not silently track a change to a *different* production
    /// constant that happens to coincide in value today.
    const TEST_ERROR_BODY_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    /// Mirrors production `BATCH_HEAD_PATIENCE` — its own constant, not a reference to the
    /// production one, for the reason `TEST_TIGHT_READ_TIMEOUT`'s doc gives: an assertion built on
    /// a mirror keeps meaning what it meant when it was written, and a change to the production
    /// constant has to be re-affirmed here rather than moving both sides of an assertion together.
    const TEST_BATCH_HEAD_PATIENCE: std::time::Duration = std::time::Duration::from_secs(45);

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

    /// `resolve` must **return** — its designed fallback, an empty map — against a remote that
    /// connects and then never writes anything, not hang forever. `resolve` has no `Result` to
    /// inspect the way its siblings above do (it is best-effort by contract: see its own doc), so
    /// this pins the one property their message-content assertions can't stand in for here: that
    /// the call actually completes within its own effective budget at all. Sibling coverage to
    /// `fetch_info`/`fetch_signature`/`fetch_bundle_to`'s own `*_times_out_against_a_silent_remote`
    /// tests above — every [`Posture::BoundedReads`]/[`Posture::TotalDeadline`] carrier gets one.
    ///
    /// **Verified not to distinguish [`Posture::TotalDeadline`] from [`Posture::BoundedReads`]
    /// — by a committed, re-run test, not a transcribed one-off measurement.** A fully silent
    /// remote never sends even a partial header, and [`REMOTE_READ_TIMEOUT`]'s own doc is explicit
    /// that *before* headers arrive its client-level `read_timeout` is a fixed, non-resetting
    /// deadline — so `Posture::BoundedReads` alone already terminates against total silence, with
    /// no total deadline needed. `fetch_info_times_out_against_a_silent_remote` above establishes
    /// exactly this: `fetch_info` rides `Posture::BoundedReads` (no `TotalDeadline` payload
    /// anywhere in the picture) against this same [`SilentRemote`] fixture and asserts both a
    /// genuine timeout and the effective 15s budget that produced it. So this test would stay
    /// green even if `resolve`'s call site rode `Posture::BoundedReads` with no total bound at
    /// all — it is real coverage (a fully-unbounded posture, or a dropped bound entirely, still
    /// fails it), just not the falsifying test for that specific call-site choice.
    /// `resolve_gives_up_on_a_remote_that_never_stops_trickling` below is: it calls `resolve`
    /// itself against a remote that never goes silent at all, the one shape a silence budget
    /// structurally cannot catch and a total deadline must.
    /// (`send_on_applies_a_total_deadline_that_ignores_progress`, also below, uses that same
    /// shape of remote but drives the seam directly rather than calling `resolve` — real
    /// coverage of the seam's own wiring, not of which posture `resolve`'s call site
    /// actually names.)
    #[test]
    fn resolve_times_out_against_a_silent_remote() {
        let remote = SilentRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let effective_budget = TEST_DIRECT_CONNECT_TIMEOUT + TEST_TIGHT_READ_TIMEOUT;
        let hard_ceiling = effective_budget + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let names = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.resolve(vec!["agent-1".to_string()])).await
        })
            .unwrap_or_else(|_| panic!(
                "resolve hung past the test's own {:?} ceiling — no total deadline fired at all",
                hard_ceiling
            ));

        assert!(
            names.is_empty(),
            "a silent remote must degrade resolve to its designed fallback (pseudonyms, an \
            empty map), not somehow produce names: {:?}", names
        );
    }

    /// Starts a remote that sends full headers (declaring `total_bytes` via `Content-Length`)
    /// immediately, then writes exactly one byte every `gap` — continuous, healthy progress,
    /// never silent for longer than `gap` at a stretch — until the body completes or the client
    /// gives up and drops the connection (detected via a failed write, which ends the loop early
    /// so the handler thread does not outlive the test). The shape a *silence* budget
    /// (`ClientBuilder::read_timeout`, [`Posture::BoundedReads`]/[`Posture::BoundedObjectReads`])
    /// is specifically designed to never fail against — see
    /// `fetch_object_survives_a_slow_but_steadily_progressing_body`, which pins exactly that for
    /// the sibling posture — which is what makes it the fixture that isolates
    /// [`Posture::TotalDeadline`]'s one distinguishing property: firing anyway.
    fn start_continuously_trickling_remote(total_bytes: usize, gap: std::time::Duration) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());

        std::thread::spawn(move || {
            use std::io::Write;

            if let Ok((mut stream, _)) = listener.accept() {
                let _ = read_test_request(&mut stream);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                    Content-Length: {}\r\n\r\n",
                    total_bytes
                );
                let _ = stream.flush();

                for _ in 0..total_bytes {
                    std::thread::sleep(gap);
                    if stream.write_all(b"x").is_err() {
                        break; // the client already gave up; nothing left to prove
                    }
                    let _ = stream.flush();
                }
            }
        });

        url
    }

    /// The falsifying test for the seam's own [`Posture::TotalDeadline`] handling: a genuine
    /// *total*, non-resetting deadline (`RequestBuilder::timeout`), applied by
    /// [`clients::Clients::send_on`]
    /// itself against a remote that never goes silent — the one shape a *silence* budget
    /// ([`Posture::BoundedReads`]) structurally cannot catch, because it resets its own clock on
    /// every byte (this file's own settled contract, `REMOTE_READ_TIMEOUT`'s doc: "a transfer
    /// that is moving bytes… is never silent").
    ///
    /// **Does not exercise `resolve`'s call site.** This pins the seam's wiring
    /// directly — it hand-constructs `Posture::TotalDeadline` and calls [`RemoteClient::send_on`]
    /// itself —
    /// which is what makes it fast and deterministic, but also means it says nothing about which
    /// posture `resolve`'s own call site actually names: it would stay green whether `resolve`
    /// passed `resolve_budget()` down, hardcoded some other total deadline, or rode
    /// `Posture::BoundedReads` instead. See
    /// [`tests::resolve_gives_up_on_a_remote_that_never_stops_trickling`] for the test that calls
    /// `resolve` itself and pins that.
    ///
    /// A short [`Posture::TotalDeadline`] (`TEST_DIRECT_CONNECT_TIMEOUT + 1s` = 6s — deliberately
    /// above the client's own `connect_timeout`, the invariant
    /// [`clients::check_total_deadline_payload`] enforces on every `TotalDeadline`
    /// payload) against a remote writing one byte every 500ms — never silent for longer than
    /// 500ms, an order of magnitude under every silence budget in this file — must still be cut
    /// off at ~6s, because a `RequestBuilder::timeout` does not reset on anything. `total_bytes`
    /// (100) at that gap makes the full trickle 50s: far longer than `hard_ceiling` (21s), so the
    /// fixture is still comfortably writing when the ceiling would fire — not literally unbounded,
    /// just sized well past this test's own horizon. If the deadline were silently dropped, the
    /// outer `tokio::time::timeout` — not the assertion below it — is what would catch it, loudly,
    /// rather than the test quietly passing on a technicality.
    #[test]
    fn send_on_applies_a_total_deadline_that_ignores_progress() {
        let url = start_continuously_trickling_remote(100, std::time::Duration::from_millis(500));
        let client = RemoteClient::new(&url, None).unwrap();
        let deadline = TEST_DIRECT_CONNECT_TIMEOUT + std::time::Duration::from_secs(1);
        let hard_ceiling = deadline + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, async {
                // The seam resolves as soon as headers arrive — this fixture writes them
                // immediately, so `send_on` returns `Sent` almost instantly regardless of whether
                // the deadline is applied at all (confirmed: without the body read below, this
                // test passed for the wrong reason, observing a `200` in well under a second).
                // Reading the body is what actually exercises the deadline: reqwest's own docs
                // for `RequestBuilder::timeout` state it runs "until the response body has
                // finished", so it is the `.bytes()` await — not the send — that must be what
                // observes the cutoff.
                let response = match client.send_on(
                    Posture::TotalDeadline(deadline), reqwest::Method::GET, "/v1/probe",
                    SendBody::Empty,
                ).await {
                    SendOutcome::Sent(response) => response,
                    SendOutcome::Transport(e) => return Err(e),
                    // Unreachable for a `TotalDeadline` posture — only
                    // `Posture::HeadDeadlineNoRedirect` arms the external timer that produces
                    // this — and required only so this match stays exhaustive. A panic rather
                    // than a silent re-route, so a seam that ever produced it here would be
                    // loud instead of being absorbed into this test's own error path.
                    SendOutcome::HeadWaitExpired { budget } => panic!(
                        "a TotalDeadline posture must never produce a head-wait expiry \
                        (budget {:?}) — only Posture::HeadDeadlineNoRedirect arms that timer",
                        budget
                    ),
                };

                response.bytes().await
            }).await
        });

        let inner = outcome.unwrap_or_else(|_| panic!(
            "send_on(TotalDeadline) hung past the test's own {:?} outer ceiling — the total \
            deadline never fired at all, even though the remote never stopped writing bytes",
            hard_ceiling
        ));

        let error = inner.expect_err(&format!(
            "a remote making continuous progress every 500ms must still be cut off by this \
            call's {:?} total deadline — if the full body arrived, the deadline was never \
            actually applied", deadline
        ));

        assert!(
            error.is_timeout(),
            "must fail specifically with a timeout, not some other transport error: {}", error
        );
    }

    /// **The phase split's whole point, as a standing assertion rather than a spike observation.**
    /// [`Posture::HeadDeadlineNoRedirect`]'s *head* timer bounds the wait for the status line and
    /// headers and nothing past it — so a remote that answers with headers promptly and then
    /// trickles a body for far longer than the head budget must **succeed**, every byte of it.
    ///
    /// **This is the trickling direction, not the stalling one, and the two are easy to confuse.**
    /// It drives a direct (non-redirect) response body on the batch `POST` under exactly the
    /// posture `fetch_batch` uses, which makes it look like coverage of that station's silence
    /// budget. It is not: its fixture never stops writing, so the client-level `read_timeout` this
    /// posture also carries is never even approached. `fetch_batch_times_out_on_a_stalled_direct_body`
    /// is the falsifier for that budget; this one is the guard against it being a total deadline.
    ///
    /// **This is the fixture that discriminates**, which is why it is worth its seconds. Implement
    /// the same posture as a *total* deadline (`RequestBuilder::timeout` — the mechanism
    /// [`Posture::TotalDeadline`] uses, one line away in the seam's own match) and this test goes
    /// red: reqwest's own docs for that method state it runs "until the response body has
    /// finished", so the `.bytes()` await below would be cut off at `head`. The head-deadline
    /// implementation stays green because `tokio::time::timeout` wraps only the `send()` future,
    /// which resolves the moment the header section arrives — after that there is no timer left
    /// running to observe a body at all. `send_on_applies_a_total_deadline_that_ignores_progress`
    /// above is its mirror image: the same trickling fixture, the rival mechanism, the opposite
    /// required outcome.
    ///
    /// **Drives the seam, not `fetch_batch`.** That is what makes it fast — `connect_timeout` is
    /// injected at 1s and the head budget is 2s, so the fixture's ~5s body genuinely outlasts the
    /// bound without paying the production 50s — and it is also its limit: it says nothing about
    /// which posture `fetch_batch`'s own call site names.
    /// `fetch_batch_times_out_against_a_silent_remote` is what pins that.
    ///
    /// The head budget deliberately exceeds `connect_timeout`, the invariant
    /// [`clients::clamp_head_deadline_payload`] enforces on every payload — so the clamp is a
    /// no-op here and the figure under test is the one written below. It also sits far under this
    /// posture's own client-level `read_timeout` (1s connect + 60s), the other fence
    /// [`clients::check_head_deadline_payload`] holds, so neither guard interferes with what this
    /// test measures.
    #[test]
    fn send_on_head_deadline_does_not_bound_a_body_that_arrives_after_it() {
        let body_bytes = 20usize;
        let gap = std::time::Duration::from_millis(250);
        let connect_timeout = std::time::Duration::from_secs(1);
        let head = connect_timeout + std::time::Duration::from_secs(1);
        let body_duration = gap * (body_bytes as u32);
        assert!(
            body_duration > head * 2,
            "the fixture's own body ({:?}) must genuinely outlast the head budget ({:?}), or this \
            test cannot tell a head deadline from a total one at all", body_duration, head
        );

        let url = start_continuously_trickling_remote(body_bytes, gap);
        let client = RemoteClient::new_test_with_connect_timeout(&url, connect_timeout);
        let hard_ceiling = body_duration + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let bytes = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, async {
                let posture = Posture::HeadDeadlineNoRedirect { head };
                let response = match client.send_on(
                    posture, reqwest::Method::POST, "/v1/objects/batch", SendBody::Empty,
                ).await {
                    SendOutcome::Sent(response) => response,
                    SendOutcome::Transport(e) => panic!(
                        "the head-wait must complete against a remote that answers immediately: \
                        {}", e
                    ),
                    SendOutcome::HeadWaitExpired { budget } => panic!(
                        "the head timer fired at {:?} even though headers arrived at once — it \
                        must bound the wait *for* headers, never anything after them", budget
                    ),
                };

                response.bytes().await
            }).await
        })
            .unwrap_or_else(|_| panic!(
                "the call hung past the test's own {:?} ceiling", hard_ceiling
            ))
            .unwrap_or_else(|e| panic!(
                "a body still arriving {:?} after headers must not be cut off by a {:?} \
                *head*-wait budget — that failure is what a total deadline of the same size \
                would produce, and the whole reason this posture is not one: {}",
                body_duration, head, e
            ));

        assert_eq!(
            bytes.len(), body_bytes,
            "the full body must arrive, not a prefix of it: a truncated read is the other shape \
            a wrongly-scoped bound produces"
        );
    }

    /// The falsifying test for [`clients::check_total_deadline_payload`]'s `debug_assert!`:
    /// every current producer of a `TotalDeadline`/`TotalDeadlineNoRedirect` payload
    /// (`RemoteClient::resolve_budget`, `RemoteClient::presence_negotiation_budget`,
    /// `RemoteClient::single_write_budget`) builds `self.connect_timeout + <positive constant>`,
    /// so none of them can ever construct a violating payload — the assert holds by construction
    /// for all three, and nothing in the suite exercises the branch where it actually fires. This
    /// test bypasses every producer and hand-constructs the violating payload directly (the same
    /// way `send_on_applies_a_total_deadline_that_ignores_progress` above hand-constructs a
    /// valid one).
    ///
    /// **It drives the check, not the seam, and that is a deliberate trade.** The predecessor of
    /// this test called the seam itself, which was possible only while the seam returned a
    /// `RequestBuilder` — it built nothing and sent nothing, so the assert fired with no runtime,
    /// no DNS and no socket in the picture. The seam now owns `send()`, so the same test through
    /// the seam would need a runtime and a host name, and a test that reaches DNS before its
    /// assert can no longer tell a deleted assert apart from an unreachable name — precisely the
    /// separation this test exists to buy. Extracting the check keeps the separation exactly;
    /// what it gives up is that this test no longer observes the seam *calling* the check. That
    /// link is `Clients::send_on`'s single call, and the check having no other caller — see
    /// [`clients::check_total_deadline_payload`]'s own doc for the grep that re-checks it.
    ///
    /// The payload is exactly equal to `connect_timeout` (10s each), not merely less than it —
    /// deleting the assert and weakening it from `>` to `>=` are two different mutants, and only
    /// an equal payload separates the correct implementation from both in one fixture. A strictly
    /// lesser payload (an earlier version of this test used 5s against a 10s connect_timeout)
    /// still satisfies `>=` (`5 >= 10` is false, the assert still fires, `#[should_panic]` is
    /// still satisfied), so the `>=` mutant survives that fixture green — it does not survive this
    /// one: at the equal payload, `>` gives `10 > 10` = false → fires (still catches the
    /// deleted-assert mutant too, which never fires at all); `>=` gives `10 >= 10` = true → does
    /// not fire → this test fails with "did not panic as expected", which is exactly what makes it
    /// a falsifier for that mutant rather than a fixture it happens to survive.
    ///
    /// Ignored outside a debug profile: `debug_assert!` compiles out entirely when
    /// `debug-assertions = false`, so under `cargo test --release` this assert never fires and the
    /// test would fail with "did not panic" for a reason unrelated to any regression in the assert
    /// itself — the ignore attribute keeps that a documented, deliberate skip rather than a
    /// surprise red build.
    #[test]
    #[cfg_attr(not(debug_assertions), ignore)]
    #[should_panic(expected = "must strictly exceed this request's connect_timeout")]
    fn the_total_deadline_payload_check_catches_a_violating_payload() {
        // Equal to connect_timeout (10s), not merely less than it — see this test's own doc for
        // why a strictly lesser payload cannot separate a deleted assert from one weakened to
        // `>=`. Nothing here is async and nothing here has a URL: the check is a pure function
        // over two `Duration`s, so there is no name to resolve and no socket to open.
        clients::check_total_deadline_payload(
            std::time::Duration::from_secs(10),
            std::time::Duration::from_secs(10),
        );
    }

    /// The head-deadline sibling of the test above, and it exists for the same two mutants: a
    /// deleted assert, and one weakened from `>` to `>=`. The payload is exactly *equal* to
    /// `connect_timeout` for the reason that test's doc spells out — a strictly lesser payload
    /// leaves the `>=` mutant green, an equal one does not.
    ///
    /// Ignored outside a debug profile, same as its sibling: `debug_assert!` compiles out when
    /// `debug-assertions = false`, so this would fail with "did not panic" under
    /// `cargo test --release` for a reason unrelated to any regression.
    ///
    /// This is only *half* of the head-deadline payload discipline, and deliberately the lesser
    /// half — see `the_head_deadline_payload_clamp_raises_a_violating_payload` below for the part
    /// that survives into a release build.
    #[test]
    #[cfg_attr(not(debug_assertions), ignore)]
    #[should_panic(expected = "must strictly exceed")]
    fn the_head_deadline_payload_check_catches_a_violating_payload() {
        clients::check_head_deadline_payload(
            std::time::Duration::from_secs(8),
            std::time::Duration::from_secs(8),
        );
    }

    /// The **upper** fence of the same check, and it guards a different sentence than the lower
    /// one. A `head` payload at or above this posture's own client-level `read_timeout` inverts
    /// the two mechanisms' order: the read timeout fires first, before any header section, and
    /// `RemoteClient::describe_transport_error`'s arm for this posture then tells the operator the
    /// remote had already sent response headers when it had sent nothing at all.
    ///
    /// The payload is exactly *equal* to the fence for the same reason its sibling above uses an
    /// equal one: a strictly greater payload leaves a `<=` mutant green (it would still fire), an
    /// equal one does not — at equality the correct `<` fires and `<=` does not, so only this
    /// fixture separates the two, and it still catches a deleted assert (which never fires).
    ///
    /// `connect_timeout` is 10s, a value no production constructor emits, so the payload is far
    /// above the lower fence and it is unambiguously *this* assert that fires. The budget is built
    /// from `TEST_LOOSE_READ_TIMEOUT` rather than the production constant, per the mirror
    /// discipline at the top of this module; if production's own figure ever rises past the
    /// mirror, this test fails loudly with "did not panic" rather than passing vacuously.
    ///
    /// Ignored outside a debug profile, same as its sibling. The release-side guarantee for the
    /// one producer that exists is not this assert but the const assertion beside
    /// `BATCH_HEAD_PATIENCE`, which fails the *build* rather than a test run.
    #[test]
    #[cfg_attr(not(debug_assertions), ignore)]
    #[should_panic(expected = "must stay strictly under")]
    fn the_head_deadline_payload_check_catches_a_payload_past_the_silence_budget() {
        let connect_timeout = std::time::Duration::from_secs(10);
        clients::check_head_deadline_payload(
            connect_timeout + TEST_LOOSE_READ_TIMEOUT,
            connect_timeout,
        );
    }

    /// **The half that survives release.** `check_head_deadline_payload` above compiles out when
    /// `debug-assertions = false`, so on its own it promises nothing about the shipped binary —
    /// and for this posture a violating payload does not merely produce a worse bound, it produces
    /// a *false sentence*: the timer fires during connect and
    /// `RemoteClient::head_wait_expired_message` tells the operator the remote accepted the
    /// connection when nothing ever connected. `clamp_head_deadline_payload` is what makes that
    /// state unreachable in every profile, so this test runs in every profile too.
    ///
    /// Three cases, and each rules out a different wrong implementation:
    ///
    /// - a valid payload passes through **unchanged** (an implementation that always added the
    ///   connect budget, or always returned a floor, would silently re-size every real call);
    /// - a payload exactly equal to `connect_timeout` is **raised** (the boundary case a `>=`
    ///   comparison would wave through, exactly as in the assert above);
    /// - a payload well under `connect_timeout` is raised to the same repaired figure.
    ///
    /// The closing assertion is the property itself rather than any of the three arithmetics: the
    /// result strictly exceeds `connect_timeout` in every case. An implementation that clamped to
    /// `connect_timeout` itself — the off-by-one this whole mechanism exists to prevent — passes
    /// "is not less than" and fails this.
    #[test]
    fn the_head_deadline_payload_clamp_raises_a_violating_payload() {
        let connect_timeout = std::time::Duration::from_secs(5);
        let valid = connect_timeout + std::time::Duration::from_secs(45);

        assert_eq!(
            clients::clamp_head_deadline_payload(valid, connect_timeout), valid,
            "a payload that already clears connect_timeout must be armed exactly as the producer \
            built it — every real call takes this branch, so a clamp that re-sized it would \
            silently change the bound the wording then names"
        );

        let equal = clients::clamp_head_deadline_payload(connect_timeout, connect_timeout);
        let lesser = clients::clamp_head_deadline_payload(
            std::time::Duration::from_secs(1), connect_timeout
        );

        assert_eq!(
            equal, lesser,
            "both violating payloads must be repaired to the same figure — the repair is a floor, \
            not a scaling of whatever the producer got wrong"
        );

        for (label, repaired) in [("equal to connect_timeout", equal), ("under it", lesser)] {
            assert!(
                repaired > connect_timeout,
                "a payload {} ({:?}) must be raised strictly above it, not merely to it: at \
                exactly connect_timeout the head timer still races the connector, which is the \
                whole state this clamp exists to make unreachable",
                label, repaired
            );
        }
    }

    /// The falsifying test for `resolve`'s own call site: that it actually hands
    /// `resolve_budget()`'s value down as a [`Posture::TotalDeadline`], not merely that
    /// the seam applies whatever `Posture::TotalDeadline` payload it is given.
    /// `send_on_applies_a_total_deadline_that_ignores_progress` above already pins the
    /// latter, but hand-constructs the posture itself and never calls `resolve` — see that
    /// test's own doc — so it says nothing about which posture, or which payload, `resolve`'s
    /// own call site actually names. This test calls [`RemoteClient::resolve`] directly.
    ///
    /// Against [`start_continuously_trickling_remote`] (headers immediately, then one byte
    /// every 500ms — never silent for longer than 500ms, an order of magnitude under every
    /// silence budget in this file), `resolve`'s own `Posture::TotalDeadline` must still cut the
    /// call off at `resolve_budget()` and degrade to the designed fallback (an empty map, see
    /// `resolve`'s own doc). A *silence* budget instead ([`Posture::BoundedReads`]) would reset
    /// its clock on every byte; within this test's observation window such a call never returns
    /// at all, which is why the outer `tokio::time::timeout` below exists as a backstop.
    ///
    /// **A bare "returned before the outer ceiling" check does not pin *when*.** Two distinct
    /// call-site defects both ignore `self.connect_timeout`: hardcoding
    /// `Posture::TotalDeadline(REMOTE_READ_TIMEOUT)` alone (10s, connect-blind) is one;
    /// hardcoding `Posture::TotalDeadline(REMOTE_CONNECT_TIMEOUT + REMOTE_READ_TIMEOUT)` (15s —
    /// folds in *a* connect budget, just never `self`'s own) is a second, closer one. Both were
    /// run against `resolve`'s actual call site as mutants and both went red against
    /// `lower_bound` below — 10.003s and 15.003s respectively, against an 18.75s bound — the
    /// same call-site-versus-mechanism gap `missing_objects_times_out_against_a_silent_remote`
    /// guards against for `missing_objects`, applied here to `resolve`.
    ///
    /// **Why 12.5s.** Every named `Duration` constant this module defines is a whole number of
    /// seconds except the two 200ms ones (`COMMIT_BACKOFF_START`/`UPLOAD_WATCHDOG_POLL_INTERVAL`)
    /// — re-check with `grep -n "Duration::from_secs\|Duration::from_millis"` over this file — so
    /// summing any two of them can never land on a half-second boundary. `connect_timeout` (12.5s,
    /// true budget 22.5s) sits on exactly that boundary: a checkable reason to prefer this value,
    /// not a claim that it is the only viable one. Stated as a property of the constants rather
    /// than as a list of their values deliberately — the property survives a constant being added,
    /// a list does not, and this module has repeatedly shipped stale lists of exactly that kind.
    ///
    /// This test excludes the two rivals named above, verified by running each as a mutant; it
    /// is not a completeness argument over any set of budgets, and other connect-blind values
    /// exist that it does not detect.
    ///
    /// **Accepted residual:** `REMOTE_READ_TIMEOUT`, `ERROR_BODY_READ_TIMEOUT`, and
    /// `UPLOAD_SILENCE_BUDGET` are all exactly 10s, so any `x + <one of these>` —
    /// `resolve_budget(x)`, `error_body_budget(x)`, or a hypothetical mutant that swapped the
    /// addend `resolve_budget` itself sums — returns numerically identical values for every `x`,
    /// including this test's own `connect_timeout`. No injected value can separate them, and
    /// neither can `resolve_budget_reads_this_field_not_a_rival_constant`'s own assertion, since
    /// `TEST_TIGHT_READ_TIMEOUT` mirrors that same 10s (see `TEST_ERROR_BODY_READ_TIMEOUT`'s own
    /// doc for the identical mirror-constant hazard). The gap is general — any 10s post-connect
    /// addend, not `error_body_budget` specifically.
    ///
    /// `total_bytes` (100) at the 500ms gap makes the full trickle 50s, comfortably longer than
    /// `hard_ceiling` (37.5s) below — the fixture is still writing when the ceiling fires. If
    /// the deadline is silently dropped, or the call site rewired onto `Posture::BoundedReads`,
    /// the outer `tokio::time::timeout` — not the assertion below it — is what catches it,
    /// loudly, rather than the test quietly hanging the suite.
    #[test]
    fn resolve_gives_up_on_a_remote_that_never_stops_trickling() {
        let url = start_continuously_trickling_remote(100, std::time::Duration::from_millis(500));
        let connect_timeout = std::time::Duration::from_millis(12_500);
        let client = RemoteClient::new_test_with_connect_timeout(&url, connect_timeout);
        let true_budget = connect_timeout + TEST_TIGHT_READ_TIMEOUT;
        let rival_flat_budget = TEST_TIGHT_READ_TIMEOUT;
        let rival_connect_blind_budget = TEST_DIRECT_CONNECT_TIMEOUT + TEST_TIGHT_READ_TIMEOUT;
        let lower_bound = rival_connect_blind_budget
            + (true_budget - rival_connect_blind_budget) / 2;
        let hard_ceiling = true_budget + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let started = std::time::Instant::now();
        let names = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.resolve(vec!["agent-1".to_string()])).await
        })
            .unwrap_or_else(|_| panic!(
                "resolve hung past the test's own {:?} ceiling — no total deadline fired at \
                all, even though the remote never stopped writing bytes",
                hard_ceiling
            ));
        let elapsed = started.elapsed();

        assert!(
            names.is_empty(),
            "a remote making continuous progress must still be cut off by resolve's own total \
            deadline and degrade to its designed fallback (pseudonyms, an empty map), not \
            somehow produce names: {:?}", names
        );

        assert!(
            elapsed >= lower_bound,
            "elapsed {:?} is under {:?} (the midpoint between the connect-blind rival budget \
            {:?} and the true budget {:?}) — resolve returned close to a call site that \
            ignores self.connect_timeout, whether by dropping it entirely (a flat {:?} rival) \
            or by hardcoding REMOTE_CONNECT_TIMEOUT in its place (the {:?} rival), rather than \
            folding in self.connect_timeout via resolve_budget()",
            elapsed, lower_bound, rival_connect_blind_budget, true_budget, rival_flat_budget,
            rival_connect_blind_budget
        );
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

    // -----------------------------------------------------------------------------------
    // The single-write mutation path: `upload_signature`/`put_trust` used to ride
    // `Posture::UnboundedNoRedirect`, genuinely unbounded, on the theory that no client here
    // combined no-auto-redirect with a client-level `read_timeout`. That theory was the wrong ask
    // even while it described the module — a client-level `read_timeout` resets on every byte, so
    // it would not have bounded a trickling remote either (see
    // `resolve_gives_up_on_a_remote_that_never_stops_trickling` above for that exact failure mode,
    // on a different call) — and it no longer describes the module at all: the client
    // `Posture::HeadDeadlineNoRedirect` selects combines exactly those two axes. Both now ride
    // `Posture::TotalDeadlineNoRedirect`, sized by `RemoteClient::single_write_budget` — the
    // same total-deadline mechanism `Posture::TotalDeadline` already gives `resolve` and the
    // presence-negotiation calls, just on the client that never auto-follows a redirect, so the
    // FORK-89 guard neither call ever loses stays in force.
    // -----------------------------------------------------------------------------------

    /// `upload_signature` against a remote that connects and then never writes anything must
    /// fail with a timeout close to its own `single_write_budget`, not hang forever (the rival
    /// this excludes: staying on the old unbounded posture, which this outer ceiling would catch
    /// as a hang) and not close to a connect-blind flat budget that drops `self.connect_timeout`
    /// (the rival the lower-bound assertion excludes). A third rival — hardcoding
    /// `REMOTE_CONNECT_TIMEOUT` in place of `self.connect_timeout` — is indistinguishable here at
    /// this client's production connect timeout (`5 + 25 == 5 + 25`); see
    /// `single_write_budget_reads_this_field_not_a_rival_constant` for the test that excludes it.
    ///
    /// The four `Duration`s below are literals, not `TEST_DIRECT_CONNECT_TIMEOUT +
    /// SINGLE_WRITE_ALLOWANCE` written symbolically. A symbolic derivation reads
    /// `SINGLE_WRITE_ALLOWANCE` directly, so it would move every bound below in lockstep with any
    /// future change to that constant's own value, the elapsed wall-clock time moving with it,
    /// and every assertion here staying green regardless — including a halving of the allowance,
    /// which is the under-pricing direction that turns a healthy slow write into an unresolvable
    /// uncertain-outcome error. Measured directly, both directions: at the current 25s allowance,
    /// this test's own elapsed time is ~30.003s, comfortably above the 27.5s `lower_bound` below;
    /// mutated to 15s, the real client still takes exactly as long as `single_write_budget()` now
    /// computes — `self.connect_timeout` (5s, unaffected) plus the mutated 15s allowance — so
    /// elapsed drops to ~20.003s, and this literal `lower_bound` (still 27.5s) correctly fails. A
    /// symbolic `lower_bound` would have dropped to ~17.5s in the same mutation, and 20.003s still
    /// clears *that*, staying green — the exact failure this literal exists to close. Literal
    /// values pin the numbers that must not move; they do not, and are not meant to, catch
    /// `single_write_budget` reading a hardcoded `REMOTE_CONNECT_TIMEOUT` in place of
    /// `self.connect_timeout` — that gap belongs to
    /// `single_write_budget_reads_this_field_not_a_rival_constant` alone, per its own doc.
    #[test]
    fn upload_signature_times_out_against_a_silent_remote() {
        let remote = SilentRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        // Literals, not `TEST_DIRECT_CONNECT_TIMEOUT + SINGLE_WRITE_ALLOWANCE` — see this test's
        // own doc for why a symbolic derivation would move every bound below in lockstep with a
        // mutation to SINGLE_WRITE_ALLOWANCE rather than catching it. 30s = 5s (direct connect) +
        // 25s (SINGLE_WRITE_ALLOWANCE); 25s is the flat-budget rival that drops the connect
        // timeout entirely.
        let effective_budget = std::time::Duration::from_secs(30);
        let rival_flat_budget = std::time::Duration::from_secs(25);
        let lower_bound = std::time::Duration::from_millis(27_500);
        let hard_ceiling = std::time::Duration::from_secs(45);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let started = std::time::Instant::now();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(
                hard_ceiling, client.upload_signature(&"c".repeat(64), vec![1u8; 64])
            ).await
        });
        let elapsed = started.elapsed();

        let message = outcome
            .unwrap_or_else(|_| panic!(
                "upload_signature hung past the test's own {:?} ceiling — no total deadline \
                fired at all", hard_ceiling
            ))
            .expect_err("a silent remote must not appear to succeed");

        assert!(
            message.to_lowercase().contains("timed out"),
            "must fail specifically with a timeout, not some other transport error: {}", message
        );
        assert!(
            elapsed >= lower_bound,
            "elapsed {:?} is under {:?} (the midpoint between the connect-blind rival budget \
            {:?} and the true budget {:?}) — upload_signature returned close to a call site that \
            ignores self.connect_timeout, dropping it entirely, rather than folding it in via \
            single_write_budget()",
            elapsed, lower_bound, rival_flat_budget, effective_budget
        );
    }

    /// `put_trust` against a remote that connects and then never writes anything — same pin as
    /// `upload_signature_times_out_against_a_silent_remote`, its own call site, including the same
    /// literal-not-symbolic reasoning for the four `Duration`s below (see that test's own doc).
    #[test]
    fn put_trust_times_out_against_a_silent_remote() {
        let remote = SilentRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        // Literals, not `TEST_DIRECT_CONNECT_TIMEOUT + SINGLE_WRITE_ALLOWANCE` — same reasoning as
        // `upload_signature_times_out_against_a_silent_remote`'s own comment.
        let effective_budget = std::time::Duration::from_secs(30);
        let rival_flat_budget = std::time::Duration::from_secs(25);
        let lower_bound = std::time::Duration::from_millis(27_500);
        let hard_ceiling = std::time::Duration::from_secs(45);
        let anchor = TrustAnchorDto {
            genesis: "g".repeat(64),
            enabled_at: 0,
            boundary: Vec::new(),
            prior_genesis: None,
            adopts: None,
        };

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let started = std::time::Instant::now();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.put_trust(&anchor)).await
        });
        let elapsed = started.elapsed();

        let message = outcome
            .unwrap_or_else(|_| panic!(
                "put_trust hung past the test's own {:?} ceiling — no total deadline fired at \
                all", hard_ceiling
            ))
            .expect_err("a silent remote must not appear to succeed");

        assert!(
            message.to_lowercase().contains("timed out"),
            "must fail specifically with a timeout, not some other transport error: {}", message
        );
        assert!(
            elapsed >= lower_bound,
            "elapsed {:?} is under {:?} (the midpoint between the connect-blind rival budget \
            {:?} and the true budget {:?}) — put_trust returned close to a call site that \
            ignores self.connect_timeout, dropping it entirely, rather than folding it in via \
            single_write_budget()",
            elapsed, lower_bound, rival_flat_budget, effective_budget
        );
    }

    /// **This test used to assert the opposite, and the flip is the point.** `fetch_batch`'s
    /// `POST` was unbounded outright, and its pin here was an "unbounded direction" one: a silent
    /// remote must not be enough to fail the call. It now rides
    /// [`Posture::HeadDeadlineNoRedirect`], so the same [`SilentRemote`] fixture must produce a
    /// timeout instead — the falsifying test for the hang the head-wait bound closes.
    ///
    /// The budget is the production one (`connect_timeout + BATCH_HEAD_PATIENCE`, 50s direct):
    /// there is no test-scale injection reachable through `fetch_batch` itself, since the patience
    /// is a constant and both production constructors emit one of two connect timeouts. What that
    /// buys instead is the strongest separation available here — the message must name **exactly
    /// 50s**, so a mutant rewiring this call site onto any other posture fails on the figure it
    /// prints, not merely on how long it took. It must also *not* say "at least": a non-resetting
    /// external timer is a true upper bound, unlike the silence budgets whose arms in
    /// `describe_transport_error` deliberately hedge.
    ///
    /// The seam-level companion is `send_on_head_deadline_does_not_bound_a_body_that_arrives_after_it`
    /// — this test would stay green under an implementation that armed a *total* deadline of the
    /// same size, and that one is what rules it out.
    #[test]
    fn fetch_batch_times_out_against_a_silent_remote() {
        let remote = SilentRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let effective_budget = TEST_DIRECT_CONNECT_TIMEOUT + TEST_BATCH_HEAD_PATIENCE;
        let hard_ceiling = effective_budget + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.fetch_batch(&["a".repeat(64)])).await
        });

        let message = outcome
            .unwrap_or_else(|_| panic!(
                "fetch_batch hung past the test's own {:?} ceiling — the head-wait bound never \
                fired at all", hard_ceiling
            ))
            .expect_err("a silent remote must not appear to succeed");

        assert!(
            message.to_lowercase().contains("timed out"),
            "must fail specifically with a timeout, not some other transport error: {}", message
        );
        assert!(
            message.contains(&format!("{:?}", effective_budget)),
            "must name the effective head budget {:?} exactly, not some other figure: {}",
            effective_budget, message
        );
        assert!(
            !message.to_lowercase().contains("at least"),
            "a non-resetting external timer is a true upper bound — the figure must be named \
            exactly, never hedged the way a resetting silence budget's wording has to: {}", message
        );
    }

    /// `fetch_subtree`'s server work is size-dependent (see its own doc comment) and it has no
    /// production caller, so it keeps the "unbounded direction" pin its sibling above just lost: a
    /// silent remote alone must never make this call fail within a window comfortably past the
    /// tight bounded budget.
    ///
    /// **Staying green is this test's second job.** It is the evidence that owning `send()` and
    /// adding a head-wait posture bounded `fetch_batch`'s `POST` and *nothing else* — a change
    /// that accidentally armed a bound at the seam for every caller would fail here, not in the
    /// test above.
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

    /// A remote that answers the batch `POST` **directly** — no redirect anywhere in the picture —
    /// with a `200`, a `Content-Length` far larger than what it writes, a few body bytes, and then
    /// genuine silence with the connection held open (no FIN). The direct station's own
    /// [`SilentRemote`], moved one phase later: the head-wait completes normally and it is the
    /// *body* that stalls.
    ///
    /// The prefix bytes are not decoration. Without them the stall would begin at the header
    /// boundary, and a bound that only covered the wait for a first body byte would look identical
    /// to one that covers every subsequent gap. Writing some body first makes this genuinely
    /// mid-body.
    struct StalledDirectBodyRemote {
        url: String,
        /// Owns the sender for the same reason [`SilentRemote`] does: dropping it at the end of
        /// the test unblocks the parked handler. Returning early instead would drop the stream,
        /// send a FIN, and fail the client instantly on a connection-closed error that has nothing
        /// to do with any budget — a test built that way passes against an unbounded client too.
        _park: std::sync::mpsc::Sender<()>,
    }

    impl StalledDirectBodyRemote {
        fn start() -> StalledDirectBodyRemote {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let (tx, rx) = std::sync::mpsc::channel::<()>();

            std::thread::spawn(move || {
                use std::io::Write;

                if let Ok((mut stream, _)) = listener.accept() {
                    let _ = read_test_request(&mut stream);
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                        Content-Length: 4096\r\n\r\nbundle-prefix"
                    );
                    let _ = stream.flush();
                    let _ = rx.recv();
                    drop(stream);
                }
            });

            StalledDirectBodyRemote { url, _park: tx }
        }
    }

    /// The redirect-follow station's counterpart to [`StalledDirectBodyRemote`]: hop 1 answers the
    /// `POST` with a `303`, hop 2 answers the presigned `GET` with `200`, a `Content-Length` it
    /// never delivers, a few body bytes, and then silence.
    ///
    /// **Why not [`RedirectThenSilentRemote`], which already exists.** That fixture's second hop
    /// goes silent *before* its status line, so `fetch_batch` fails inside
    /// `response_from_send("following the batch redirect", ...)` — a different composer call with
    /// a different action string. Two messages that differ only by action string prove nothing
    /// about the two postures' *arms*, which is the thing the shared figure puts at risk. Stalling
    /// after the status line is what routes this station into the same
    /// `describe_transport_error("reading the batch response", ...)` call the direct station uses,
    /// where the posture is the only thing left that differs.
    struct RedirectThenStalledBodyRemote {
        url: String,
        /// Parked for the same reason every silent fixture here is — see [`SilentRemote`].
        _park: std::sync::mpsc::Sender<()>,
    }

    impl RedirectThenStalledBodyRemote {
        fn start() -> RedirectThenStalledBodyRemote {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let base = url.clone();
            let (tx, rx) = std::sync::mpsc::channel::<()>();

            std::thread::spawn(move || {
                use std::io::Write;

                if let Ok((mut stream, _)) = listener.accept() {
                    let _ = read_test_request(&mut stream);
                    let _ = write!(
                        stream,
                        "HTTP/1.1 303 See Other\r\nLocation: {}/responses/bundle\r\n\
                        Content-Length: 0\r\nConnection: close\r\n\r\n",
                        base
                    );
                    let _ = stream.flush();
                }

                if let Ok((mut stream, _)) = listener.accept() {
                    let _ = read_test_request(&mut stream);
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                        Content-Length: 4096\r\n\r\nbundle-prefix"
                    );
                    let _ = stream.flush();
                    let _ = rx.recv();
                    drop(stream);
                }
            });

            RedirectThenStalledBodyRemote { url, _park: tx }
        }
    }

    /// **The falsifying test for `fetch_batch`'s body-read bound**, on the station that had no
    /// committed coverage in either direction: the direct, non-redirect response body of the batch
    /// `POST`. Before the client [`Posture::HeadDeadlineNoRedirect`] selects carried a
    /// `read_timeout`, this shape hung forever — the head-wait timer had already resolved at the
    /// status line and nothing was left watching the body.
    ///
    /// The budget is the production one (`connect_timeout + FETCH_OBJECT_READ_TIMEOUT`, 65s
    /// direct), so the assertions do the discriminating rather than the clock. The message must
    /// name **exactly 65s**, which separates this from a mutant wiring the posture onto the tight
    /// [`Posture::BoundedReads`] budget (15s) or onto no budget at all (hangs past the ceiling).
    /// It must say **"at least"**: this budget resets on every byte, so the figure is a lower
    /// bound on the silence and naming it exactly would be the head-wait timer's entitlement, not
    /// this one's. And it must name the **headers** already having arrived, which is what tells
    /// this station apart from the redirect-follow one at the same call site — the two share an
    /// action string and print the identical figure, so wording is the only thing left to
    /// distinguish them with, and
    /// `the_two_batch_stations_do_not_share_one_stall_message` is what pins that they actually
    /// differ.
    #[test]
    fn fetch_batch_times_out_on_a_stalled_direct_body() {
        let remote = StalledDirectBodyRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let effective_budget = TEST_DIRECT_CONNECT_TIMEOUT + TEST_LOOSE_READ_TIMEOUT;
        let hard_ceiling = effective_budget + std::time::Duration::from_secs(20);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.fetch_batch(&["a".repeat(64)])).await
        });

        let message = outcome
            .unwrap_or_else(|_| panic!(
                "fetch_batch hung past the test's own {:?} ceiling — a body that stops arriving \
                mid-transfer is the exact failure this station's silence budget exists to end",
                hard_ceiling
            ))
            .expect_err("a body that never finishes must not appear to succeed");

        assert!(
            message.to_lowercase().contains("timed out"),
            "must fail specifically with a timeout, not some other transport error: {}", message
        );
        assert!(
            message.contains(&format!("{:?}", effective_budget)),
            "must name the effective silence budget {:?} exactly, not the tight read budget and \
            not some other figure: {}", effective_budget, message
        );
        assert!(
            message.to_lowercase().contains("at least"),
            "a resetting silence budget only ever guarantees the gap was at least this long — the \
            figure must be hedged, unlike the head-wait timer's: {}", message
        );
        assert!(
            message.to_lowercase().contains("headers"),
            "the wording must say the response headers had already arrived, which is what names \
            *which* of fetch_batch's two stations stalled — they share one action string and one \
            figure: {}", message
        );
    }

    /// **The distinguishing test the shared figure forces.** `fetch_batch` hands both of its
    /// stations to one [`RemoteClient::describe_transport_error`] call under one action string,
    /// and both render the same duration — the direct body through
    /// [`Posture::HeadDeadlineNoRedirect`]'s client, the redirect-follow `GET` through
    /// [`Posture::BoundedObjectReads`]', both sized from [`FETCH_OBJECT_READ_TIMEOUT`]. So a
    /// per-station assertion on the figure certifies nothing about which one an operator is
    /// looking at. This drives both stalls against the real call and requires the two messages to
    /// differ.
    ///
    /// **Both fixtures must stall *after* their status line**, which is why this uses
    /// [`RedirectThenStalledBodyRemote`] rather than the older [`RedirectThenSilentRemote`]. A
    /// redirect target that never answers at all fails at a different composer call with a
    /// different action string — the messages would then differ for a reason that has nothing to
    /// do with the two arms, and this test would stay green against the very mutant it exists to
    /// catch. It did, when first written that way; that is why it says so here.
    ///
    /// Runs the two concurrently on one runtime rather than back to back: both fail at the same
    /// ~65s budget, so racing them costs one budget rather than two.
    ///
    /// **The mutant this test alone catches, established by running it rather than by reasoning
    /// about it:** give the *`BoundedObjectReads`* arm this arm's wording. Measured — this test
    /// fails on identical `left`/`right`, and
    /// [`fetch_batch_times_out_on_a_stalled_direct_body`] **passes**, because that test asserts on
    /// the direct arm, which the mutant does not touch. Nothing else in the file compares the two
    /// arms against each other, so without this test that mutant ships.
    ///
    /// The mirror mutant — giving *this* posture's arm the `BoundedObjectReads` wording — is **not**
    /// the one that justifies this test's ~65s, and an earlier version of this doc said it was. It
    /// fails two tests, not one: [`fetch_batch_times_out_on_a_stalled_direct_body`] trips first, on
    /// its `contains("headers")` assertion. Recorded because a coverage claim is exactly what a
    /// later reader consults when deciding a slow test is redundant, and this one was wrong in the
    /// direction that would have got the test deleted.
    #[test]
    fn the_two_batch_stations_do_not_share_one_stall_message() {
        let direct = StalledDirectBodyRemote::start();
        let redirected = RedirectThenStalledBodyRemote::start();
        let direct_client = RemoteClient::new(&direct.url, None).unwrap();
        let redirect_client = RemoteClient::new(&redirected.url, None).unwrap();
        let hard_ceiling = TEST_DIRECT_CONNECT_TIMEOUT + TEST_LOOSE_READ_TIMEOUT
            + std::time::Duration::from_secs(20);

        let direct_hashes = vec!["a".repeat(64)];
        let redirect_hashes = vec!["b".repeat(64)];
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let (direct_outcome, redirect_outcome) = runtime.block_on(async {
            tokio::join!(
                tokio::time::timeout(hard_ceiling, direct_client.fetch_batch(&direct_hashes)),
                tokio::time::timeout(hard_ceiling, redirect_client.fetch_batch(&redirect_hashes)),
            )
        });

        let direct_message = direct_outcome
            .unwrap_or_else(|_| panic!("the direct station hung past {:?}", hard_ceiling))
            .expect_err("a stalled direct body must not appear to succeed");
        let redirect_message = redirect_outcome
            .unwrap_or_else(|_| panic!("the redirect station hung past {:?}", hard_ceiling))
            .expect_err("a stalled redirect-target body must not appear to succeed");

        assert!(
            direct_message.to_lowercase().contains("timed out")
                && redirect_message.to_lowercase().contains("timed out"),
            "both stations must fail with a timeout before their wording can be compared at all: \
            direct {:?}, redirect {:?}", direct_message, redirect_message
        );
        let shared_action = "reading the batch response";
        assert!(
            direct_message.contains(shared_action) && redirect_message.contains(shared_action),
            "both stalls must reach the *body-read* composer call, the one both stations share — \
            a message naming any other action means this fixture stalled at the wrong phase and \
            the comparison below would be vacuous: direct {:?}, redirect {:?}",
            direct_message, redirect_message
        );
        assert_ne!(
            direct_message, redirect_message,
            "fetch_batch's two stations must not produce the same message: they share one action \
            string and one figure, so identical wording leaves an operator with no way to tell a \
            stalled control-plane answer from a stalled storage read"
        );
    }

    /// **The stuck-green guard for the body bound, and the reason the phase split exists at all.**
    /// A direct batch response whose body dribbles in steadily — every gap far under the silence
    /// budget, the whole transfer far past it *and* past the head-wait budget — must **succeed**,
    /// intact.
    ///
    /// Three rival implementations die here, which is what makes it worth its seconds. A *total*
    /// deadline over the response (`RequestBuilder::timeout`, one line away in the seam's own
    /// match) cuts the transfer off at its own figure however healthy it is. A head-wait timer
    /// widened to cover the body kills it at 50s. The tight [`Posture::BoundedReads`] silence
    /// budget (15s effective) kills it at the first 20s gap. Only a *loose, resetting* silence
    /// budget survives, which is precisely the mechanism this station is supposed to carry.
    ///
    /// Reuses `start_steady_drip_remote` — `fetch_object`'s own drip fixture, on the direct path
    /// with no redirect hop, which is exactly the shape the batch `POST` sees from a non-offloading
    /// head.
    #[test]
    fn fetch_batch_survives_a_slow_but_steadily_progressing_direct_body() {
        let gap = std::time::Duration::from_secs(20);
        let chunks: Vec<Vec<u8>> =
            (0..4).map(|i| format!("direct-chunk-{}-drip", i).into_bytes()).collect();
        let expected: Vec<u8> = chunks.concat();
        let total_duration = gap * (chunks.len() as u32);
        let effective_silence_budget = TEST_DIRECT_CONNECT_TIMEOUT + TEST_LOOSE_READ_TIMEOUT;
        let effective_head_budget = TEST_DIRECT_CONNECT_TIMEOUT + TEST_BATCH_HEAD_PATIENCE;

        assert!(
            total_duration > effective_silence_budget && total_duration > effective_head_budget,
            "the fixture ({:?}) must outlast both of this call's bounds — the silence budget {:?} \
            and the head budget {:?} — or it cannot tell a resetting per-gap bound from either of \
            the rival mechanisms", total_duration, effective_silence_budget, effective_head_budget
        );
        assert!(
            gap < effective_silence_budget,
            "no single gap ({:?}) may approach the silence budget ({:?}), or this fixture would \
            fail against the correct implementation too", gap, effective_silence_budget
        );

        let url = start_steady_drip_remote(chunks, gap);
        let client = RemoteClient::new(&url, None).unwrap();
        let outer_ceiling = total_duration + std::time::Duration::from_secs(20);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(outer_ceiling, client.fetch_batch(&["a".repeat(64)])).await
        });

        let bytes = outcome
            .unwrap_or_else(|_| panic!(
                "fetch_batch hung past its own generous outer ceiling {:?}", outer_ceiling
            ))
            .unwrap_or_else(|e| panic!(
                "a body that was never silent for more than {:?} must never be treated as a \
                stall, however long the whole transfer took: {}", gap, e
            ));

        assert_eq!(
            bytes, Some(expected),
            "the full body must arrive intact, not a prefix of it — a truncated read is the other \
            shape a wrongly-scoped bound produces"
        );
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
    // once a non-success status line has arrived. The auto-following client (`Posture::
    // UnboundedFollowsRedirects`, which `missing_objects` rides) carries only a `connect_timeout`,
    // no `read_timeout` at all — so a remote that delivers a
    // full status line and headers and then wedges before writing the body hangs the caller
    // forever. `ERROR_BODY_READ_TIMEOUT` bounds the read itself, in `error_of`, rather than
    // trying to fix it with yet another client — no client can help here, since the body-read
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

    /// `missing_objects` now rides [`Posture::TotalDeadline`] (FORK-92 budget 1), sized per batch
    /// by [`RemoteClient::presence_negotiation_budget`] — but that outer, per-request deadline is
    /// a *different* bound from `error_of`'s own inner [`RemoteClient::error_body_budget`], and
    /// this test's whole point survives that move only because the batch size below (2,000) is
    /// deliberately picked so the outer bound (`connect + POST_SEND_VERIFY_BASE + 2,000 *
    /// PRESENCE_ALLOWANCE_MS_PER_OP` = 17s direct) stays looser than the inner one (`connect +
    /// ERROR_BODY_READ_TIMEOUT` = 15s direct): the inner budget is still what actually fires here.
    /// At a small batch (see `missing_objects_times_out_against_a_silent_remote`'s own budget,
    /// ~7s at `n=1`) the outer deadline is the *tighter* of the two and would fire first instead —
    /// a real, deliberate interaction between the two bounds, not a gap: whichever fires first
    /// still surfaces as a plain `Err` through the same `error_of` fallback path either way (a
    /// timed-out `response.json()` and an elapsed inner `tokio::time::timeout` both take the same
    /// "no usable error body arrived" arm), so no call ever hangs regardless of which bound wins.
    ///
    /// Before this call had its own outer deadline at all, it rode the auto-following client with
    /// no bound of any kind — a `500` whose body then wedges hung it forever, with the status line
    /// and headers already fully delivered. The outer test-ceiling below is a safety net, not the
    /// property under test: if the wrapper in `error_of` were missing, that ceiling is what would
    /// trip (the test failing instead of hanging the suite) — unambiguous evidence the internal
    /// bound is gone, since status line and headers were fully delivered before the park, so the
    /// only thing left to hang on is the error-body read itself.
    ///
    /// Also asserts a **lower** bound on elapsed time (review round 6, finding 1): a red-then-green
    /// suite run alone does not prove `error_of` calls [`error_body_read_budget`] at all — nothing
    /// else in the suite requires that link, so a regression back to the bare
    /// [`ERROR_BODY_READ_TIMEOUT`] (dropping the folded-in [`REMOTE_CONNECT_TIMEOUT`]) still
    /// passes every other assertion here, which only ceiling-checks. `tokio::time::timeout` never
    /// fires early and this fixture never sends a body, so the read burns the *entire* budget —
    /// making the lower bound exact, not approximate.
    ///
    /// On its own this pins only that *some* connect-timeout-shaped value gets folded in — a
    /// direct client's `connect_timeout` is [`REMOTE_CONNECT_TIMEOUT`], so this cannot tell
    /// "reads `self.connect_timeout`" apart from "hardcodes the direct 5s constant" (review round
    /// 7: this is exactly the gap `error_body_budget_reads_this_field_not_a_rival_constant` closes,
    /// with a connect_timeout no production constructor can produce, so a hardcoded rival can never
    /// coincidentally match it). Four tests together pin the property: this one pins
    /// call-site-uses-the-budget, `error_body_read_budget_folds_in_the_connect_timeout` pins the
    /// arithmetic the budget computes, `error_body_budget_reads_this_field_not_a_rival_constant`
    /// pins that the accessor reads *this instance's* connect timeout field rather than hardcoding
    /// any constant a real constructor could produce, and
    /// `error_body_budget_is_70s_for_a_real_tor_mode_client` pins that the value at the one client
    /// mode that actually ships a non-default budget is the correct, uncapped 70s (review round 8,
    /// finding 1 — the previous field-read property alone does not imply this one: a future cap on
    /// the result would leave it unaffected while breaking this).
    #[test]
    fn missing_objects_bounds_the_error_body_read_after_a_wedged_500() {
        let remote = SilentErrorBodyRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        // A batch of 2,000 hashes — large enough that the call's own outer TotalDeadline
        // (`connect + POST_SEND_VERIFY_BASE + 2,000 * PRESENCE_ALLOWANCE_MS_PER_OP` = 17s direct)
        // stays looser than error_of's inner error_body_budget (15s direct), so the inner budget
        // is still what fires and this test still pins what it always pinned. Any real hash
        // string works — the fixture never inspects the body.
        let hashes: Vec<String> = (0..2000).map(|_| "a".repeat(64)).collect();
        // Mirrors, not the production constants directly (see the `TEST_*` block's doc above) —
        // written as the constant sum, not via `error_body_read_budget`, so this assertion
        // doesn't co-move with a corrupted helper — a helper bug would then move the elapsed time
        // it produces and this fixed lower bound in lockstep, pinning nothing.
        let lower_bound = TEST_DIRECT_CONNECT_TIMEOUT + TEST_ERROR_BODY_READ_TIMEOUT;
        // The bracket this fixture's batch size depends on, made an assertion rather than left as
        // a comment: the call's own outer TotalDeadline must stay looser than error_of's inner
        // error_body_budget (`lower_bound` above), or the outer bound fires first and every
        // assertion below would end up blaming error_of/error_body_read_budget for a regression
        // that actually lives in presence_negotiation_budget or its constants — a misleading red,
        // not a wrong one. Computed the same constant-sum-not-via-helper way as `lower_bound`,
        // for the identical reason: calling `presence_negotiation_budget` here would let a bug in
        // that method move this guard and the real behavior it is meant to guard against in
        // lockstep, silently validating a broken implementation instead of catching it.
        let outer_budget = TEST_DIRECT_CONNECT_TIMEOUT + POST_SEND_VERIFY_BASE
            + std::time::Duration::from_secs_f64(
                hashes.len() as f64 * PRESENCE_ALLOWANCE_MS_PER_OP / 1000.0
            );
        assert!(
            outer_budget > lower_bound,
            "this fixture's batch size ({} hashes, outer TotalDeadline {:?}) no longer stays \
            looser than error_of's own inner error_body_budget ({:?}) — resize `hashes` above \
            until it does; every assertion below assumes the inner budget is what fires, and a \
            red result here means they are about to fail for the wrong reason (the outer bound \
            firing first) rather than the one they exist to catch",
            hashes.len(), outer_budget, lower_bound
        );
        let outer_ceiling = lower_bound + std::time::Duration::from_secs(10);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let started = std::time::Instant::now();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(outer_ceiling, client.missing_objects(&hashes)).await
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
            "elapsed {:?} is under the {:?} the folded budget (connect timeout + error-body-read \
            timeout) requires — error_of is no longer calling error_body_read_budget, it's using \
            the bare error-body-read timeout (or less)",
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

    /// Review round 7, finding: a Tor-mode fixture (this test's predecessor,
    /// `error_body_budget_reads_this_instances_connect_timeout`) cannot tell
    /// "[`RemoteClient::error_body_budget`] reads `self.connect_timeout`" apart from "hardcodes
    /// [`REMOTE_CONNECT_TIMEOUT_TOR`] directly" — a mutation to the latter still passed, because a
    /// `TorMode::On` client's field genuinely *is* 60s, the same value the hardcoded rival would
    /// return. The direct-client version has the identical problem one level down (5s either way).
    /// No fixture built through a production constructor can separate the two: [`RemoteClient::new`]
    /// only ever yields [`REMOTE_CONNECT_TIMEOUT`] (5s) and [`RemoteClient::new_with_tor`] under
    /// `TorMode::On` only ever yields [`REMOTE_CONNECT_TIMEOUT_TOR`] (60s) — both are rivals a
    /// hardcoded mutant can impersonate. [`RemoteClient::new_test_with_connect_timeout`] injects a
    /// third value, `SENTINEL_CONNECT_TIMEOUT` (7s), that no production constructor can ever
    /// produce, so a mutant hardcoding *either* rival constant is caught here.
    ///
    /// 7 was checked against every named `Duration` constant this module defines before being
    /// picked, and collides with none of them. The procedure, so a reader can re-derive that
    /// instead of trusting it: `grep -n "Duration::from_secs\|Duration::from_millis"` over this
    /// file.
    ///
    /// **The values themselves are deliberately not transcribed here, and this is the canonical
    /// statement of why** — the four sibling sentinel docs below say the same thing by pointing at
    /// this paragraph. A transcribed census is a hand-maintained enumeration, and this module's
    /// history is that contracts survive in prose while enumerations rot: the commit that added
    /// this file's most recent `Duration` constant left **every** such list in this test module
    /// stale at once, including the one it wrote itself. What each of these tests actually rests on is
    /// the separation claim — this sentinel collides with nothing — and that claim is re-checkable
    /// in a second by the grep above. The census only ever decays between edits.
    ///
    /// A coincidental *sum* of two unrelated constants would not be a rival even where one exists:
    /// the mutation this test defends against is a hardcoded *connect_timeout* substitution — one
    /// of the two values a production constructor can actually produce — not an arbitrary
    /// recombination of budgets belonging to a different phase, which nothing in this module ever
    /// adds to a connect timeout. What would actually matter
    /// is a rival for this test's asserted *output*, `17s` (`7 + TEST_ERROR_BODY_READ_TIMEOUT`):
    /// the two values a hardcoded production `connect_timeout` would actually produce here instead
    /// of reading `self.connect_timeout` are `15s` (`REMOTE_CONNECT_TIMEOUT` +
    /// `TEST_ERROR_BODY_READ_TIMEOUT`) and `70s` (`REMOTE_CONNECT_TIMEOUT_TOR` +
    /// `TEST_ERROR_BODY_READ_TIMEOUT`), and `17s` separates from both. A separate literal `17s`
    /// does appear elsewhere in this module
    /// (`mutation_post_send_timeout_message_carries_the_uncertainty_wording`), but as an arbitrary
    /// sample duration for a message-formatting assertion, not a timeout this code ever arms — not
    /// a rival either. This is a separation report against the rivals that actually matter, not a
    /// claim that no other combination of this module's constants could ever sum to 17 —
    /// characterising a separation table as complete has cost a fresh defect on every attempt this
    /// module's history has tried it, so that claim is deliberately not made.
    ///
    /// This test does **not** subsume `error_body_budget_is_70s_for_a_real_tor_mode_client` below —
    /// an earlier round argued it did, on prose alone; nobody tested the argument, and it's false.
    /// Capping `error_body_budget`'s result at, say, 30s (`min(computed, 30s)`, a plausible future
    /// "bound the wait" change) leaves this test's 17s untouched — already comfortably under 30s —
    /// while silently breaking the real Tor-mode value the property exists to protect. The two
    /// tests pin different claims: this one, reads-the-field-not-a-rival-constant; the other, the
    /// value at the endpoint that actually ships. Both are required.
    #[test]
    fn error_body_budget_reads_this_field_not_a_rival_constant() {
        const SENTINEL_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(7);

        let client = RemoteClient::new_test_with_connect_timeout(
            "http://forklift-fork-49-error-body-budget-sentinel-test.invalid",
            SENTINEL_CONNECT_TIMEOUT,
        );

        assert_eq!(
            client.error_body_budget(),
            SENTINEL_CONNECT_TIMEOUT + TEST_ERROR_BODY_READ_TIMEOUT,
            "error_body_budget must fold in *this instance's* connect_timeout — a value neither \
            RemoteClient::new (5s) nor RemoteClient::new_with_tor under TorMode::On (60s) can ever \
            produce, so this cannot pass by coincidentally matching a hardcoded rival constant"
        );
    }

    /// Review round 8, finding 1: the sentinel test above pins *reads-the-field-not-a-rival* but
    /// not what `error_body_budget` actually returns for a real Tor-mode client — the two are
    /// different claims, and a round 7 argument that the sentinel subsumed this test's predecessor
    /// was accepted on prose alone and never tested. It doesn't hold: see that test's own doc for
    /// the falsifying mutation (a 30s cap) that leaves the sentinel green while this one reddens.
    /// Built the same no-I/O way as `new_with_tor_selects_the_60s_connect_budget` (`new_with_tor`
    /// never touches the network).
    #[test]
    fn error_body_budget_is_70s_for_a_real_tor_mode_client() {
        let tor = TorSettings { mode: TorMode::On, proxy: DEFAULT_TOR_PROXY.to_string() };
        let client = RemoteClient::new_with_tor(
            "http://forklift-fork-49-error-body-budget-tor-endpoint-test.invalid", None, tor,
        ).unwrap();

        assert_eq!(
            client.error_body_budget(),
            TEST_TOR_CONNECT_TIMEOUT + TEST_ERROR_BODY_READ_TIMEOUT,
            "a real TorMode::On client's error_body_budget must be exactly the Tor-folded 70s — \
            otherwise a Tor remote's refusal body gets killed early, discarding a typed \
            RefusalCode/next_step and degrading a machine caller to the wrong exit code"
        );
    }

    /// Mirrors [`error_body_budget_reads_this_field_not_a_rival_constant`] for
    /// `resolve`'s own [`RemoteClient::resolve_budget`] — the identical gap applies here for the
    /// identical reason: every existing behavioral test of `resolve` builds a direct client via
    /// `RemoteClient::new`, where `self.connect_timeout` == [`REMOTE_CONNECT_TIMEOUT`] ==
    /// [`TEST_DIRECT_CONNECT_TIMEOUT`] (5s) — so a hardcoded-constant mutant of `resolve_budget`
    /// (`REMOTE_CONNECT_TIMEOUT` in place of `self.connect_timeout`) is extensionally identical to
    /// the correct implementation everywhere those tests look, and would still give a Tor client
    /// only a 15s total deadline against a 60s connect allowance — the deadline firing during
    /// circuit build, the exact defect class this fix exists to close.
    ///
    /// `SENTINEL_CONNECT_TIMEOUT` (11s) is deliberately a different value from
    /// `error_body_budget`'s own sentinel (7s) — reusing it would make the two tests read as one
    /// case split in half rather than two independently readable pins — and, like that test's own
    /// sentinel, was checked against every named `Duration` constant this module defines before
    /// being picked, colliding with none of them. Procedure: `grep -n
    /// "Duration::from_secs\|Duration::from_millis"` over this file; the values are not
    /// transcribed here, for the reason
    /// [`error_body_budget_reads_this_field_not_a_rival_constant`]'s own doc gives at length.
    /// What would actually matter is a rival for this test's asserted
    /// *output*, 21s (`11 + TEST_TIGHT_READ_TIMEOUT`): the two values a hardcoded production
    /// `connect_timeout` would actually produce here are `15s` (`REMOTE_CONNECT_TIMEOUT` +
    /// `TEST_TIGHT_READ_TIMEOUT`) and `70s` (`REMOTE_CONNECT_TIMEOUT_TOR` +
    /// `TEST_TIGHT_READ_TIMEOUT`), and 21s separates from both. This is a separation report against those two rivals, not a
    /// claim that no other combination of this module's constants could ever sum to 21 —
    /// characterising a separation table as complete has cost a fresh defect on every attempt this
    /// module's history has tried it, so that claim is deliberately not made.
    #[test]
    fn resolve_budget_reads_this_field_not_a_rival_constant() {
        const SENTINEL_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(11);

        let client = RemoteClient::new_test_with_connect_timeout(
            "http://forklift-fork-96-resolve-budget-sentinel-test.invalid",
            SENTINEL_CONNECT_TIMEOUT,
        );

        assert_eq!(
            client.resolve_budget(),
            SENTINEL_CONNECT_TIMEOUT + TEST_TIGHT_READ_TIMEOUT,
            "resolve_budget must fold in *this instance's* connect_timeout — a value neither \
            RemoteClient::new (5s) nor RemoteClient::new_with_tor under TorMode::On (60s) can ever \
            produce, so this cannot pass by coincidentally matching a hardcoded rival constant"
        );
    }

    /// Mirrors [`error_body_budget_is_70s_for_a_real_tor_mode_client`] for
    /// [`RemoteClient::resolve_budget`], for the identical reason that test's own doc gives: the
    /// sentinel test above pins *reads-the-field-not-a-rival* but not what `resolve_budget`
    /// actually returns for a real Tor-mode client, and neither claim subsumes the other. Built
    /// the same no-I/O way (`new_with_tor` never touches the network).
    #[test]
    fn resolve_budget_is_70s_for_a_real_tor_mode_client() {
        let tor = TorSettings { mode: TorMode::On, proxy: DEFAULT_TOR_PROXY.to_string() };
        let client = RemoteClient::new_with_tor(
            "http://forklift-fork-96-resolve-budget-tor-endpoint-test.invalid", None, tor,
        ).unwrap();

        assert_eq!(
            client.resolve_budget(),
            TEST_TOR_CONNECT_TIMEOUT + TEST_TIGHT_READ_TIMEOUT,
            "a real TorMode::On client's resolve_budget must be exactly the Tor-folded 70s — \
            otherwise resolve's own total deadline fires during Tor circuit build, the exact \
            defect class this fix exists to close"
        );
    }

    /// Mirrors [`resolve_budget_reads_this_field_not_a_rival_constant`] and its siblings for
    /// [`RemoteClient::batch_head_budget`] — the identical gap for the identical reason: every
    /// behavioural test of `fetch_batch` builds a direct client via `RemoteClient::new`, where
    /// `self.connect_timeout` is [`REMOTE_CONNECT_TIMEOUT`] (5s), so a mutant hardcoding that
    /// constant in place of the field is extensionally identical everywhere those tests look —
    /// and would give a Tor client a head budget sized off a 5s connect allowance against a 60s
    /// one, the bound expiring during circuit build on every call.
    ///
    /// `SENTINEL_CONNECT_TIMEOUT` (17s) is a value no production constructor can emit, and it was
    /// checked against every named `Duration` constant this module defines before being picked,
    /// colliding with none of them. Procedure: `grep -n
    /// "Duration::from_secs\|Duration::from_millis"` over this file; the values are not
    /// transcribed here, for the reason
    /// [`error_body_budget_reads_this_field_not_a_rival_constant`]'s own doc gives at length —
    /// the version of this paragraph that shipped with the head-wait bound *did* transcribe them,
    /// and was stale on arrival, because the same commit added a `Duration` constant its own list
    /// omitted. This test's asserted *output*, 62s, separates from the two rivals a
    /// hardcoded production connect timeout would actually produce here: 50s
    /// (`REMOTE_CONNECT_TIMEOUT + BATCH_HEAD_PATIENCE`) and 105s (`REMOTE_CONNECT_TIMEOUT_TOR +
    /// BATCH_HEAD_PATIENCE`). A separation report against those two rivals, not a claim that no
    /// other combination of this module's constants could sum to 62 — characterising such a table
    /// as complete has cost a fresh defect on every attempt this module's history has made.
    ///
    /// The expected value is a literal for the reason
    /// [`single_write_budget_reads_this_field_not_a_rival_constant`]'s own doc gives: written
    /// symbolically as `SENTINEL_CONNECT_TIMEOUT + BATCH_HEAD_PATIENCE`, a change to the patience
    /// moves both sides of the assertion together and this test stays green through it.
    #[test]
    fn batch_head_budget_reads_this_field_not_a_rival_constant() {
        const SENTINEL_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(17);

        let client = RemoteClient::new_test_with_connect_timeout(
            "http://forklift-fork-104-batch-head-budget-sentinel-test.invalid",
            SENTINEL_CONNECT_TIMEOUT,
        );

        assert_eq!(
            client.batch_head_budget(),
            std::time::Duration::from_secs(62),
            "batch_head_budget must be this instance's 17s connect_timeout plus the 45s head \
            patience — a sum neither RemoteClient::new (5s → 50s) nor RemoteClient::new_with_tor \
            under TorMode::On (60s → 105s) can produce, so this cannot pass by coincidentally \
            matching a hardcoded rival constant"
        );
    }

    /// Mirrors [`resolve_budget_is_70s_for_a_real_tor_mode_client`] and its siblings for
    /// [`RemoteClient::batch_head_budget`], for the identical reason: the sentinel test above pins
    /// *reads-the-field-not-a-rival* but not what the budget actually is for a real Tor-mode
    /// client, and neither claim subsumes the other — a future cap on the result would leave the
    /// sentinel untouched while silently breaking this. Built the same no-I/O way (`new_with_tor`
    /// never touches the network).
    ///
    /// What the Tor figure protects is specific: `BATCH_HEAD_PATIENCE` alone (45s) is *smaller*
    /// than [`REMOTE_CONNECT_TIMEOUT_TOR`] (60s), so an implementation that dropped the connect
    /// term would arm a head-wait bound that expires before an onion circuit can even finish
    /// building — every batch fetch over Tor failing, deterministically, with a message asserting
    /// a connection that was still being made.
    #[test]
    fn batch_head_budget_is_105s_for_a_real_tor_mode_client() {
        let tor = TorSettings { mode: TorMode::On, proxy: DEFAULT_TOR_PROXY.to_string() };
        let client = RemoteClient::new_with_tor(
            "http://forklift-fork-104-batch-head-budget-tor-endpoint-test.invalid", None, tor,
        ).unwrap();

        assert_eq!(
            client.batch_head_budget(),
            TEST_TOR_CONNECT_TIMEOUT + TEST_BATCH_HEAD_PATIENCE,
            "a real TorMode::On client's batch_head_budget must be exactly the Tor-folded 105s"
        );
        assert!(
            client.batch_head_budget() > TEST_TOR_CONNECT_TIMEOUT,
            "and it must strictly exceed the connect budget it was built with — the producer rule \
            clamp_head_deadline_payload exists to repair, discharged here at the one producer"
        );
    }

    // -----------------------------------------------------------------------------------
    // FORK-92 (budget 1): `missing_objects`/`upload_targets` move off `UnboundedTicket::Fork92`
    // onto `Posture::TotalDeadline`, sized by `RemoteClient::presence_negotiation_budget`. The
    // four tests below mirror the `error_body_budget`/`resolve_budget` pattern directly above:
    // reads-n (an implementation ignoring the batch size and returning a flat constant must
    // fail), reads-this-instance's-connect_timeout-not-a-rival (the sentinel pattern), the real
    // Tor-mode value, and that the bound actually fires against a remote that never answers.
    // -----------------------------------------------------------------------------------

    /// A flat-constant implementation of `presence_negotiation_budget` — one that ignores `n`
    /// entirely — passes every other test in this section (the sentinel and Tor-mode tests both
    /// fix `n`). This is the one that would catch it: the budget at `n=1` and at
    /// `n=MAX_MISSING_BATCH` must differ, by exactly the arithmetic
    /// `presence_negotiation_budget`'s own doc states, not merely by *some* positive amount (a
    /// bug that scales the wrong term, or scales by the wrong rate, would still pass a
    /// "must differ" check but not an exact one).
    #[test]
    fn presence_negotiation_budget_reads_n() {
        let client = RemoteClient::new_test_with_connect_timeout(
            "http://forklift-fork-92-presence-budget-n-test.invalid",
            TEST_DIRECT_CONNECT_TIMEOUT,
        );

        let small = client.presence_negotiation_budget(1);
        let large = client.presence_negotiation_budget(MAX_MISSING_BATCH);

        assert!(
            large > small,
            "presence_negotiation_budget(n) must grow with n — got {:?} at n=1 and {:?} at \
            n={} (MAX_MISSING_BATCH); an implementation that ignores n and returns a flat \
            constant would make these equal", small, large, MAX_MISSING_BATCH
        );

        let expected_gap = std::time::Duration::from_secs_f64(
            (MAX_MISSING_BATCH - 1) as f64 * PRESENCE_ALLOWANCE_MS_PER_OP / 1000.0
        );
        assert_eq!(
            large - small, expected_gap,
            "the n=1 vs n=MAX_MISSING_BATCH gap must be exactly (n-1) * \
            PRESENCE_ALLOWANCE_MS_PER_OP — not merely some positive drift, which a wrong rate or \
            a scaling bug on the wrong term could also produce"
        );
    }

    /// Mirrors [`error_body_budget_reads_this_field_not_a_rival_constant`]/
    /// [`resolve_budget_reads_this_field_not_a_rival_constant`] for
    /// [`RemoteClient::presence_negotiation_budget`] — the identical gap for the identical reason:
    /// every existing behavioral test of `missing_objects`/`upload_targets` builds a direct client
    /// via `RemoteClient::new`, where `self.connect_timeout` == [`REMOTE_CONNECT_TIMEOUT`] (5s) —
    /// so a hardcoded-constant mutant (`REMOTE_CONNECT_TIMEOUT` in place of `self.connect_timeout`)
    /// is extensionally identical to the correct implementation everywhere those tests look, and
    /// would still give a Tor client a deadline sized off a 5s connect budget against a 60s connect
    /// allowance — the deadline firing during circuit build, [`REMOTE_CONNECT_TIMEOUT_TOR`]'s own
    /// reason for existing.
    ///
    /// `SENTINEL_CONNECT_TIMEOUT` (13s) is a third value, distinct from `error_body_budget`'s own
    /// sentinel (7s) and `resolve_budget`'s (11s) for the same "read independently, not one case
    /// split in half" reason those two are distinct from each other — and, like both, checked
    /// against every named `Duration` constant this module defines before being picked, colliding
    /// with none of them. Procedure: `grep -n "Duration::from_secs\|Duration::from_millis"` over
    /// this file; the values are not transcribed here, for the reason
    /// [`error_body_budget_reads_this_field_not_a_rival_constant`]'s own doc gives at length.
    /// `n` is fixed at 1 here (a 5ms scaled term) precisely so
    /// this test's asserted *output* — 15.005s (`13 + POST_SEND_VERIFY_BASE + 0.005`) — is what
    /// actually needs checking for collisions, and unlike `error_body_budget`/`resolve_budget`'s
    /// own two-addend sums, `presence_negotiation_budget` sums *three* terms (`connect_timeout`,
    /// `POST_SEND_VERIFY_BASE`, the `n`-scaled term); unlike those tests, the odd `.005s` fraction
    /// this asserts is not producible at all by summing any subset of this module's named
    /// (whole- or tenth-second) `Duration` constants — a fractional-second remainder no
    /// whole/tenth-second constant in this module can produce, so no enumeration is needed to make
    /// the point, and none is attempted: characterising a separation table as complete has cost a
    /// fresh defect on every attempt this module's history has tried it.
    #[test]
    fn presence_negotiation_budget_reads_this_field_not_a_rival_constant() {
        const SENTINEL_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(13);

        let client = RemoteClient::new_test_with_connect_timeout(
            "http://forklift-fork-92-presence-budget-sentinel-test.invalid",
            SENTINEL_CONNECT_TIMEOUT,
        );

        assert_eq!(
            client.presence_negotiation_budget(1),
            SENTINEL_CONNECT_TIMEOUT + POST_SEND_VERIFY_BASE
                + std::time::Duration::from_secs_f64(1.0 * PRESENCE_ALLOWANCE_MS_PER_OP / 1000.0),
            "presence_negotiation_budget must fold in *this instance's* connect_timeout — a value \
            neither RemoteClient::new (5s) nor RemoteClient::new_with_tor under TorMode::On (60s) \
            can ever produce, so this cannot pass by coincidentally matching a hardcoded rival \
            constant"
        );
    }

    /// Mirrors [`error_body_budget_is_70s_for_a_real_tor_mode_client`]/
    /// [`resolve_budget_is_70s_for_a_real_tor_mode_client`] for
    /// [`RemoteClient::presence_negotiation_budget`], for the identical reason those two give: the
    /// sentinel test above pins *reads-the-field-not-a-rival* but not what the budget actually
    /// returns for a real Tor-mode client, and neither claim subsumes the other — a future cap on
    /// the result (e.g. `min(computed, 30s)`) would leave the sentinel's small-`n` assertion
    /// untouched while silently breaking the real Tor-mode value this test pins. Built the same
    /// no-I/O way (`new_with_tor` never touches the network). `n` is `MAX_MISSING_BATCH` — the
    /// largest batch either call ever actually sends — so this also pins the budget at the real
    /// worst case, not an arbitrary small `n`.
    #[test]
    fn presence_negotiation_budget_is_112s_for_a_real_tor_mode_client_at_the_missing_batch_cap() {
        let tor = TorSettings { mode: TorMode::On, proxy: DEFAULT_TOR_PROXY.to_string() };
        let client = RemoteClient::new_with_tor(
            "http://forklift-fork-92-presence-budget-tor-endpoint-test.invalid", None, tor,
        ).unwrap();

        let expected = TEST_TOR_CONNECT_TIMEOUT + POST_SEND_VERIFY_BASE
            + std::time::Duration::from_secs_f64(
                MAX_MISSING_BATCH as f64 * PRESENCE_ALLOWANCE_MS_PER_OP / 1000.0
            );
        assert_eq!(
            expected, std::time::Duration::from_secs(112),
            "sanity: this test's own name promises 112s — 60s connect + 2s base + \
            10,000 * 5ms = 60 + 2 + 50"
        );

        assert_eq!(
            client.presence_negotiation_budget(MAX_MISSING_BATCH),
            expected,
            "a real TorMode::On client's presence_negotiation_budget at the largest batch \
            missing_objects/upload_targets ever send must be exactly the Tor-folded value — \
            otherwise the deadline fires during Tor circuit build on the very calls this fix \
            exists to protect"
        );
    }

    /// Mirrors [`error_body_budget_reads_this_field_not_a_rival_constant`]/
    /// [`resolve_budget_reads_this_field_not_a_rival_constant`]/
    /// [`presence_negotiation_budget_reads_this_field_not_a_rival_constant`] for
    /// [`RemoteClient::single_write_budget`] — the identical gap: every end-to-end test of
    /// `upload_signature`/`put_trust` builds a direct client via `RemoteClient::new`, where
    /// `self.connect_timeout` == [`REMOTE_CONNECT_TIMEOUT`] (5s), so a hardcoded-constant mutant
    /// (`REMOTE_CONNECT_TIMEOUT` in place of `self.connect_timeout`) is extensionally identical to
    /// the correct implementation everywhere those tests look — `5 + SINGLE_WRITE_ALLOWANCE` either
    /// way — and neither elapsed-time end-to-end test above can catch it either, for the same
    /// reason `resolve_gives_up_on_a_remote_that_never_stops_trickling`'s own doc names for the
    /// identical rival: at production `connect_timeout`, the two implementations produce the exact
    /// same `Duration`.
    ///
    /// `SENTINEL_CONNECT_TIMEOUT` (9s) is a value distinct from the two production connect-timeout
    /// constants this test actually needs to separate from — [`REMOTE_CONNECT_TIMEOUT`] (5s) and
    /// [`REMOTE_CONNECT_TIMEOUT_TOR`] (60s) — and from every named `Duration` constant this module
    /// defines, checked by `grep -n "Duration::from_secs\|Duration::from_millis"` before being
    /// picked. Those values are not transcribed here either, for the reason
    /// [`error_body_budget_reads_this_field_not_a_rival_constant`]'s own doc gives at length —
    /// **and this doc had already learned that lesson on a different list while keeping this one**:
    /// it dropped an enumeration of the other tests' sentinels after that list rotted within the
    /// same commit that added
    /// [`super::tests::the_total_deadline_payload_check_catches_a_violating_payload`]'s own 10s
    /// sentinel, and then kept a census of the module's `Duration` values that went stale in its
    /// turn, for exactly the same reason. Recorded because a lesson applied to one list and not to
    /// its neighbour in the same paragraph is how the class survives being fixed. Reusing another
    /// test's sentinel value here would only cost readability (two tests reading as one case split
    /// in half), never this test's own correctness, since nothing about `single_write_budget`
    /// reads any other test's constant.
    ///
    /// This test's asserted output, 34s (`9 + 25`), separates from the two rivals a
    /// hardcoded production `connect_timeout` would actually produce here instead of reading
    /// `self.connect_timeout`: 30s (`REMOTE_CONNECT_TIMEOUT` + `SINGLE_WRITE_ALLOWANCE`) and 85s
    /// (`REMOTE_CONNECT_TIMEOUT_TOR` + `SINGLE_WRITE_ALLOWANCE`). This is a separation report
    /// against those two rivals, not a claim that no other combination of this module's constants
    /// could ever sum to 34 — characterising a separation table as complete has cost a fresh defect
    /// on every attempt this module's history has tried it, so that claim is deliberately not made.
    #[test]
    fn single_write_budget_reads_this_field_not_a_rival_constant() {
        const SENTINEL_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(9);

        let client = RemoteClient::new_test_with_connect_timeout(
            "http://forklift-fork-49-single-write-budget-sentinel-test.invalid",
            SENTINEL_CONNECT_TIMEOUT,
        );

        assert_eq!(
            client.single_write_budget(),
            std::time::Duration::from_secs(34),
            "single_write_budget must be this instance's 9s connect_timeout plus the 25s \
            allowance. The expected value is a literal, deliberately, and not \
            `SENTINEL_CONNECT_TIMEOUT + SINGLE_WRITE_ALLOWANCE`: written symbolically, a change to \
            the allowance's own arithmetic moves both sides of this assertion together and every \
            budget test in this module stays green — including a halving of the hook allowance, \
            which is the under-pricing direction that turns a healthy slow write into an \
            unresolvable uncertain-outcome error. Measured: that mutant left 114 of 114 tests \
            passing before this literal was introduced. 34s also separates from the two rivals a \
            hardcoded production connect_timeout would produce, 30s and 85s"
        );
    }

    /// `missing_objects` must terminate against a remote that connects, reads the request in
    /// full, and then genuinely goes silent — never answering — rather than hang forever. Before
    /// this fix it rode `Posture::UnboundedFollowsRedirects`, with no bound of any kind (the
    /// `UnboundedTicket::Fork92` gap this PR closes for this call); this is the falsifying test
    /// for that defect, the same shape as `fetch_info_times_out_against_a_silent_remote` and
    /// siblings above, applied to the total-deadline posture this call now rides.
    ///
    /// A merely-generous outer ceiling alone does not separate the right implementation from a
    /// plausible wrong one: `hard_ceiling` here is `effective_budget` (~7s at this `n`) `+ 15s` =
    /// ~22s, but `Posture::BoundedReads`'s own effective silence budget against a fully silent
    /// remote is *also* only 15s (direct) — so a mutant that rewired this call site onto
    /// `BoundedReads` instead of `TotalDeadline` would still return an `Err` comfortably inside
    /// that 22s ceiling, and the test would still pass, certifying nothing about which posture
    /// actually fired. `upper_bound` below is the assertion that actually distinguishes them, set
    /// at the *midpoint* between this call's own budget and that rival's effective silence budget
    /// (not an arbitrary margin over the true value): that split spends exactly as much slack on
    /// absorbing scheduling/dispatch noise as it keeps in reserve to still catch the rewiring, and
    /// makes the trade a reader who widens this margin later is making explicit — how much closer
    /// to the rival's own budget they are choosing to get.
    #[test]
    fn missing_objects_times_out_against_a_silent_remote() {
        let remote = SilentRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let hashes = vec!["a".repeat(64)];
        let effective_budget = client.presence_negotiation_budget(hashes.len());
        let rival_bounded_reads_budget = TEST_DIRECT_CONNECT_TIMEOUT + TEST_TIGHT_READ_TIMEOUT;
        let upper_bound = effective_budget + (rival_bounded_reads_budget - effective_budget) / 2;
        let hard_ceiling = effective_budget + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let started = std::time::Instant::now();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.missing_objects(&hashes)).await
        });
        let elapsed = started.elapsed();

        outcome
            .unwrap_or_else(|_| panic!(
                "missing_objects hung past the test's own {:?} ceiling — no total deadline fired \
                at all", hard_ceiling
            ))
            .expect_err("a silent remote must not appear to succeed");

        assert!(
            elapsed <= upper_bound,
            "elapsed {:?} exceeds {:?} (the midpoint between effective_budget {:?} and \
            BoundedReads' own {:?} silence budget) — missing_objects took closer to a \
            Posture::BoundedReads-shaped silence budget than its own presence_negotiation_budget \
            total deadline; a rewiring onto BoundedReads would still return an Err inside the \
            generous hard_ceiling above, so this tighter bound is what actually separates the two",
            elapsed, upper_bound, effective_budget, rival_bounded_reads_budget
        );
    }

    /// `upload_targets` must terminate against a remote that connects, reads the request in
    /// full, and then genuinely goes silent — same defect, same fix, same falsifying shape as
    /// [`missing_objects_times_out_against_a_silent_remote`] immediately above, for the sibling
    /// call this PR moves off the identical unbounded posture — including its `upper_bound`
    /// assertion, for the identical `BoundedReads`-shaped-mutant reason that test's own doc gives.
    #[test]
    fn upload_targets_times_out_against_a_silent_remote() {
        let remote = SilentRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let hashes = vec!["a".repeat(64)];
        let effective_budget = client.presence_negotiation_budget(hashes.len());
        let rival_bounded_reads_budget = TEST_DIRECT_CONNECT_TIMEOUT + TEST_TIGHT_READ_TIMEOUT;
        let upper_bound = effective_budget + (rival_bounded_reads_budget - effective_budget) / 2;
        let hard_ceiling = effective_budget + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let started = std::time::Instant::now();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(
                hard_ceiling, client.upload_targets("session-fork-92-silent-test", &hashes)
            ).await
        });
        let elapsed = started.elapsed();

        let inner = outcome.unwrap_or_else(|_| panic!(
            "upload_targets hung past the test's own {:?} ceiling — no total deadline fired \
            at all", hard_ceiling
        ));
        assert!(
            inner.is_err(),
            "a silent remote must not appear to succeed"
        );
        assert!(
            elapsed <= upper_bound,
            "elapsed {:?} exceeds {:?} (the midpoint between effective_budget {:?} and \
            BoundedReads' own {:?} silence budget) — upload_targets took closer to a \
            Posture::BoundedReads-shaped silence budget than its own presence_negotiation_budget \
            total deadline; a rewiring onto BoundedReads would still return an Err inside the \
            generous hard_ceiling above, so this tighter bound is what actually separates the two",
            elapsed, upper_bound, effective_budget, rival_bounded_reads_budget
        );
    }

    // -----------------------------------------------------------------------------------
    // Transport-error composer posture: `missing_objects`/`upload_targets` used to map a
    // `.send()`/`.json()` failure with a bare `format!`, so a genuine deadline expiry read to an
    // operator as a raw reqwest Debug string, never told it was a timeout at all. Both now route
    // through `describe_transport_error`, which — since the reshape below — takes the exact
    // `Posture` a call was armed with rather than a bare silence-budget `Duration`, so it can
    // report `Posture::TotalDeadline`'s own payload verbatim (an exact figure, not "at least")
    // instead of double-counting `self.connect_timeout` against it.
    // -----------------------------------------------------------------------------------

    /// The falsifying test for `missing_objects`'s send-error branch: before this fix it composed
    /// a bare `format!("Error while negotiating with the remote: {}", e)` on `.send()` failure —
    /// no timeout wording, no figure — so an operator staring at a stuck negotiation had no way to
    /// tell "the remote is genuinely gone" from "the deadline this call itself armed just
    /// expired." This pins that the composed message now names *that exact armed total*, read
    /// from [`RemoteClient::presence_negotiation_budget`] itself rather than a hand-written
    /// duration literal — a hand-written literal could silently drift from the real arithmetic and
    /// this test would never notice.
    ///
    /// **Why a 4s sentinel connect_timeout.** Neither [`RemoteClient::new`] (5s,
    /// [`REMOTE_CONNECT_TIMEOUT`]) nor [`RemoteClient::new_with_tor`] under `TorMode::On` (60s,
    /// [`REMOTE_CONNECT_TIMEOUT_TOR`]) can ever produce it, so a rival implementation that
    /// hardcodes either production constant in place of `self.connect_timeout` cannot
    /// coincidentally match this test's expected figure. It is also absent from this module's own
    /// named `Duration` constants and from every connect-timeout sentinel already in use elsewhere
    /// in this file — checked, at the time of writing, via `grep -n
    /// "Duration::from_secs\|Duration::from_millis"` and `grep -n
    /// "new_test_with_connect_timeout"` plus the two production constructors. **Both sets are the
    /// procedure, not a transcription**: earlier versions of this paragraph carried both lists
    /// inline and both were stale within two commits, which is the reason
    /// [`error_body_budget_reads_this_field_not_a_rival_constant`]'s own doc gives at length for
    /// dropping them everywhere in this module. Re-run the greps; do not trust a remembered set.
    ///
    /// At `n=1` the true budget is `4s + POST_SEND_VERIFY_BASE (2s) + 1 * \
    /// PRESENCE_ALLOWANCE_MS_PER_OP (5ms) = 6.005s`. The `.005` fraction only ever comes from
    /// `PRESENCE_ALLOWANCE_MS_PER_OP`, and the reason is structural rather than a census: every
    /// named `Duration` constant this module defines is a whole number of seconds except the two
    /// 200ms ones (`COMMIT_BACKOFF_START`/`UPLOAD_WATCHDOG_POLL_INTERVAL`), so every sum of them
    /// has a fractional part that is a multiple of 0.2 (`.0`/`.2`/`.4`/`.6`/`.8`) and never
    /// `.005` — a message containing `6.005s` could only have come from the real arithmetic. Put
    /// as a property of the constants rather than of a list of their values on purpose: this form
    /// survives a new constant being added and is re-checkable by the same grep, where the list it
    /// replaces was falsified by the next constant to land. Measured (not merely reasoned
    /// about) against the rivals below, run as hand-applied mutants at this test's own `S=4s,
    /// n=1`:
    ///
    /// | Rival | Prints | Separated from 6.005s |
    /// |---|---|---|
    /// | status-quo bare `format!` | no figure, no timeout wording | yes |
    /// | double-counting (`connect + total budget`) | 10.005s | yes |
    /// | connect-blind (drops `self.connect_timeout`) | 2.005s | yes |
    /// | wrong arm / silence path with `REMOTE_READ_TIMEOUT` | 14s | yes |
    /// | hardcoded production connect (5s / 60s) | 7.005s / 62.005s | yes |
    ///
    /// This is not a completeness argument over every possible wrong implementation — only the
    /// rivals above were run as mutants and measured.
    #[test]
    fn missing_objects_deadline_message_names_the_armed_total() {
        const SENTINEL_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

        let remote = SilentRemote::start();
        let client = RemoteClient::new_test_with_connect_timeout(&remote.url, SENTINEL_CONNECT_TIMEOUT);
        let hashes = vec!["a".repeat(64)];
        let expected_budget = client.presence_negotiation_budget(hashes.len());
        assert_eq!(
            expected_budget,
            std::time::Duration::from_secs(6) + std::time::Duration::from_millis(5),
            "sanity: this test's own doc promises 6.005s — 4s connect + 2s POST_SEND_VERIFY_BASE \
            + 1 * 5ms PRESENCE_ALLOWANCE_MS_PER_OP"
        );
        let hard_ceiling = expected_budget + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.missing_objects(&hashes)).await
        });

        let message = outcome
            .unwrap_or_else(|_| panic!(
                "missing_objects hung past the test's own {:?} ceiling — no total deadline fired \
                at all", hard_ceiling
            ))
            .expect_err("a silent remote must not appear to succeed");

        assert!(
            message.contains(&format!("{:?}", expected_budget)),
            "must name the exact armed total {:?} — read from presence_negotiation_budget \
            itself, not a hand-written duration literal: {}", expected_budget, message
        );
        assert!(
            message.to_lowercase().contains("timed out"),
            "must carry the timeout wording, not the status-quo bare format!: {}", message
        );
        assert!(
            !message.to_lowercase().contains("connect"),
            "must not carry the connect-timeout wording — this is the ReadTimedOut branch, \
            armed on a TotalDeadline posture, not ConnectTimedOut: {}", message
        );
    }

    /// The `upload_targets` counterpart of [`missing_objects_deadline_message_names_the_armed_total`]
    /// — a separate code path (its own `.send()` map_err site), so it gets its own test rather than
    /// being parameterised into that one. Same defect, same fix, same rival table (that test's own
    /// doc carries it; not repeated here).
    #[test]
    fn upload_targets_deadline_message_names_the_armed_total() {
        const SENTINEL_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

        let remote = SilentRemote::start();
        let client = RemoteClient::new_test_with_connect_timeout(&remote.url, SENTINEL_CONNECT_TIMEOUT);
        let hashes = vec!["a".repeat(64)];
        let expected_budget = client.presence_negotiation_budget(hashes.len());
        let hard_ceiling = expected_budget + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(
                hard_ceiling, client.upload_targets("session-composer-posture-test", &hashes)
            ).await
        });

        let inner = outcome.unwrap_or_else(|_| panic!(
            "upload_targets hung past the test's own {:?} ceiling — no total deadline fired \
            at all", hard_ceiling
        ));
        let message = match inner {
            Err(message) => message,
            Ok(_) => panic!("a silent remote must not appear to succeed"),
        };

        assert!(
            message.contains(&format!("{:?}", expected_budget)),
            "must name the exact armed total {:?} — read from presence_negotiation_budget \
            itself, not a hand-written duration literal: {}", expected_budget, message
        );
        assert!(
            message.to_lowercase().contains("timed out"),
            "must carry the timeout wording, not the status-quo bare format!: {}", message
        );
        assert!(
            !message.to_lowercase().contains("connect"),
            "must not carry the connect-timeout wording — this is the ReadTimedOut branch, \
            armed on a TotalDeadline posture, not ConnectTimedOut: {}", message
        );
    }

    /// A remote that sends a full `200 OK` with JSON headers and a `Content-Length` far larger
    /// than the partial JSON fragment it actually writes, then genuinely goes silent — same
    /// parked-connection shape as [`LyingContentLengthRemote`], but for a JSON body instead of raw
    /// bytes, and generic to any request path (via [`read_test_request`]) rather than one
    /// endpoint. Exists for the tests below: a fully [`SilentRemote`] never gets past headers, so
    /// it cannot distinguish `missing_objects`/`upload_targets`'s `response.json()` timeout branch
    /// (route through the composer) from their JSON-parse-failure branch (the unconditional "not
    /// valid JSON" message) — both look identical against total silence, because neither branch is
    /// ever reached without headers. This fixture reaches the body-read arm specifically: headers
    /// and a fragment arrive, then the connection stalls forever mid-body.
    struct PartialJsonThenSilentRemote {
        url: String,
        _park: std::sync::mpsc::Sender<()>,
    }

    impl PartialJsonThenSilentRemote {
        fn start() -> PartialJsonThenSilentRemote {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let (tx, rx) = std::sync::mpsc::channel::<()>();

            std::thread::spawn(move || {
                use std::io::Write;

                if let Ok((mut stream, _)) = listener.accept() {
                    let _ = read_test_request(&mut stream);
                    // Claims 4096 bytes of JSON, writes one opening brace, then parks — the rest
                    // never arrives and the connection is never closed, so response.json() is
                    // left waiting on bytes that are never coming rather than failing to parse
                    // what little it already has.
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: 4096\r\n\r\n{{"
                    );
                    let _ = stream.flush();
                    let _ = rx.recv();
                    drop(stream);
                }
            });

            PartialJsonThenSilentRemote { url, _park: tx }
        }
    }

    /// Distinguishes the body-phase fix from a cosmetic one: reverting `missing_objects`'s
    /// `response.json()` map_err back to the unconditional "not valid JSON" message (dropping the
    /// `is_timeout()` branch) would still pass every silent-remote test above — a fully silent
    /// remote never sends headers at all, so it never reaches `response.json()`'s error arm in the
    /// first place. Against [`PartialJsonThenSilentRemote`] it does: the deadline still fires (the
    /// `TotalDeadline` posture bounds the whole response, body included — a total timeout carries
    /// into the body read, not just `.send()`), and the message must be the deadline message, not
    /// the JSON-parse-failure one, since the remote never sent garbage — it sent a valid-so-far
    /// prefix and then nothing further.
    #[test]
    fn missing_objects_deadline_message_survives_a_partial_json_body() {
        const SENTINEL_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

        let remote = PartialJsonThenSilentRemote::start();
        let client = RemoteClient::new_test_with_connect_timeout(&remote.url, SENTINEL_CONNECT_TIMEOUT);
        let hashes = vec!["a".repeat(64)];
        let expected_budget = client.presence_negotiation_budget(hashes.len());
        let hard_ceiling = expected_budget + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.missing_objects(&hashes)).await
        });

        let message = outcome
            .unwrap_or_else(|_| panic!(
                "missing_objects hung past the test's own {:?} ceiling — no total deadline fired \
                at all", hard_ceiling
            ))
            .expect_err("a body that never completes must not appear to succeed");

        assert!(
            message.contains(&format!("{:?}", expected_budget)),
            "a partial JSON body followed by silence must still compose the deadline message \
            naming the exact armed total {:?}, not fall through to the unconditional \"not \
            valid JSON\" message: {}", expected_budget, message
        );
        assert!(
            !message.to_lowercase().contains("not valid json"),
            "must not be the JSON-parse-failure message — the remote never finished sending, it \
            did not send garbage: {}", message
        );
    }

    /// A remote that answers `200` with a complete, correctly framed body that is **not** JSON,
    /// then closes. The counterpart of [`PartialJsonThenSilentRemote`]: there the body never
    /// finishes and the failure is a timeout; here it finishes immediately and the failure is a
    /// parse error. Shaped like the captive-portal/proxy case that motivates the distinction — a
    /// `200` carrying an HTML interstitial.
    struct MalformedJsonRemote {
        url: String,
    }

    impl MalformedJsonRemote {
        fn start() -> MalformedJsonRemote {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());

            std::thread::spawn(move || {
                use std::io::Write;

                if let Ok((mut stream, _)) = listener.accept() {
                    let _ = read_test_request(&mut stream);
                    let body = "<html><body>Sign in to continue</body></html>";
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(), body
                    );
                    let _ = stream.flush();
                }
            });

            MalformedJsonRemote { url }
        }
    }

    /// Pins the `else` half of `missing_objects`'s `if e.is_timeout()` guard, which the two
    /// partial-body tests above cannot reach: their fixture only ever produces the timeout case,
    /// so widening the condition to `true` — routing *every* `response.json()` failure through the
    /// composer — leaves both of them green.
    ///
    /// Measured under that mutant, against a proxy or captive portal answering `200` with an HTML
    /// interstitial: the message becomes `"Error while negotiating with the remote: expected value
    /// at line 1 column 1"`. A decode error is neither a connect failure nor a timeout, so
    /// `classify` routes it to [`TransportFailure::Other`] and it degrades to the generic
    /// transport wrapper — it does **not** fabricate a deadline figure. So the wording assertion
    /// is the one doing the pinning here, and it is the only one: the elapsed check below is a
    /// fixture-health check, not a rival-excluding assertion, since any run that reached the armed
    /// budget ended via the total deadline and so could not have produced this wording anyway. It
    /// earns its place by naming the *fixture* as the suspect when it fails, rather than the
    /// implementation.
    ///
    /// Worth stating because the prediction that motivated this test was that the mutant would
    /// quote an exact, fabricated deadline. Running it showed otherwise, and the consequence is
    /// milder than predicted: the operator loses the "not valid JSON" framing rather than being
    /// told a false number.
    #[test]
    fn missing_objects_reports_a_parse_failure_as_a_parse_failure() {
        const SENTINEL_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

        let remote = MalformedJsonRemote::start();
        let client = RemoteClient::new_test_with_connect_timeout(&remote.url, SENTINEL_CONNECT_TIMEOUT);
        let hashes = vec!["a".repeat(64)];
        let armed_budget = client.presence_negotiation_budget(hashes.len());

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let started = std::time::Instant::now();
        let message = runtime
            .block_on(client.missing_objects(&hashes))
            .expect_err("an HTML body behind a JSON content type must not appear to succeed");
        let elapsed = started.elapsed();

        assert!(
            elapsed < armed_budget,
            "the failure must be observed on arrival, well inside the {:?} armed deadline — it \
            took {:?}, which means this fixture is exercising the timeout path, not the parse \
            path this test exists for", armed_budget, elapsed
        );
        assert!(
            message.to_lowercase().contains("not valid json"),
            "a complete but unparseable body must keep the parse-failure wording: {}", message
        );
    }

    /// The `upload_targets` counterpart of
    /// [`missing_objects_reports_a_parse_failure_as_a_parse_failure`] — same guard, same mutant,
    /// its own test since it is a separate code path.
    #[test]
    fn upload_targets_reports_a_parse_failure_as_a_parse_failure() {
        const SENTINEL_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

        let remote = MalformedJsonRemote::start();
        let client = RemoteClient::new_test_with_connect_timeout(&remote.url, SENTINEL_CONNECT_TIMEOUT);
        let hashes = vec!["a".repeat(64)];
        let armed_budget = client.presence_negotiation_budget(hashes.len());

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let started = std::time::Instant::now();
        let outcome = runtime.block_on(
            client.upload_targets("session-composer-posture-parse-test", &hashes)
        );
        let elapsed = started.elapsed();

        let message = match outcome {
            Err(message) => message,
            Ok(_) => panic!("an HTML body behind a JSON content type must not appear to succeed"),
        };

        assert!(
            elapsed < armed_budget,
            "the failure must be observed on arrival, well inside the {:?} armed deadline — it \
            took {:?}, which means this fixture is exercising the timeout path, not the parse \
            path this test exists for", armed_budget, elapsed
        );
        assert!(
            message.to_lowercase().contains("not valid json"),
            "a complete but unparseable body must keep the parse-failure wording: {}", message
        );
    }

    /// The third `is_timeout()` json guard, on `fetch_info`, gets the same treatment as the two
    /// above. It predates this reshape and was the *least* covered of the three: mutating it to
    /// `if true` left the entire crate suite green, because every existing `fetch_info` test
    /// drives either a refused connect or header-phase silence, neither of which ever reaches
    /// `response.json()`'s error arm at all.
    ///
    /// The handshake is the first call any remote operation makes, so this is the guard a captive
    /// portal meets first: a `200` carrying an HTML sign-in page on `/v1/warehouse` must be
    /// reported as a malformed handshake, not as a transport failure against a bound that never
    /// fired.
    #[test]
    fn fetch_info_reports_a_parse_failure_as_a_parse_failure() {
        let remote = MalformedJsonRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let message = match runtime.block_on(client.fetch_info()) {
            Err(message) => message,
            Ok(_) => panic!("an HTML body behind a JSON content type must not appear to succeed"),
        };

        assert!(
            message.to_lowercase().contains("not valid json"),
            "a complete but unparseable handshake body must keep the parse-failure wording, not \
            degrade to the generic transport wrapper: {}", message
        );
    }

    /// The `upload_targets` counterpart of
    /// [`missing_objects_deadline_message_survives_a_partial_json_body`] — same defect, same fix,
    /// same fixture, its own test since it is a separate code path.
    #[test]
    fn upload_targets_deadline_message_survives_a_partial_json_body() {
        const SENTINEL_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

        let remote = PartialJsonThenSilentRemote::start();
        let client = RemoteClient::new_test_with_connect_timeout(&remote.url, SENTINEL_CONNECT_TIMEOUT);
        let hashes = vec!["a".repeat(64)];
        let expected_budget = client.presence_negotiation_budget(hashes.len());
        let hard_ceiling = expected_budget + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(
                hard_ceiling, client.upload_targets("session-composer-posture-json-test", &hashes)
            ).await
        });

        let inner = outcome.unwrap_or_else(|_| panic!(
            "upload_targets hung past the test's own {:?} ceiling — no total deadline fired \
            at all", hard_ceiling
        ));
        let message = match inner {
            Err(message) => message,
            Ok(_) => panic!("a body that never completes must not appear to succeed"),
        };

        assert!(
            message.contains(&format!("{:?}", expected_budget)),
            "a partial JSON body followed by silence must still compose the deadline message \
            naming the exact armed total {:?}, not fall through to the unconditional \"not \
            valid JSON\" message: {}", expected_budget, message
        );
        assert!(
            !message.to_lowercase().contains("not valid json"),
            "must not be the JSON-parse-failure message — the remote never finished sending, it \
            did not send garbage: {}", message
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

    /// Starts a remote that answers whatever request it receives with a `200` and `body` — empty
    /// or a JSON payload, the caller's choice — only after `delay`, never a byte before that.
    /// Shared by every fixture in this file that needs a remote whose pre-first-byte work
    /// legitimately takes a while but then answers correctly: `update_ref`'s slow first-push
    /// audit-walk stand-in (`audit_utils.rs`; server-side work `update_ref` legitimately waits on
    /// before its first response byte, unbounded by design because it is scoped by the history
    /// segment being pushed, which on a first lift into an empty pallet is the whole history and
    /// can take minutes) and `resolve`'s discriminating-window fixture. Returns the remote's base
    /// URL; the connection closes itself after the one delayed response, so nothing needs parking
    /// or dropping here.
    fn start_slow_answering_remote(delay: std::time::Duration, body: &str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let body = body.to_string();

        std::thread::spawn(move || {
            use std::io::Write;

            if let Ok((mut stream, _)) = listener.accept() {
                let _ = read_test_request(&mut stream);
                std::thread::sleep(delay);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                    Connection: close\r\n\r\n{}",
                    body.len(), body
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
        let url = start_slow_answering_remote(past_read_timeout, "");
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

    /// **The pin that catches the one-line version of the body-read fix**, and the reason the test
    /// above cannot: adding a `read_timeout` to the shared `Clients::no_redirect` client instead of
    /// building `Clients::bounded_object_reads_no_redirect` is the shortest way to bound
    /// `fetch_batch`'s body, and it silently hands the same silence budget to `update_ref` — the
    /// one call in this module that must never carry one, because its server side legitimately runs
    /// a parcel-closure audit walk that moves no bytes for minutes.
    ///
    /// `update_ref_outlives_the_read_metadata_timeout` above cannot see that mistake at all. Its
    /// fixture answers after `connect + tight read + 3s` = 18s, composed from the *tight* mirrors —
    /// comfortably inside the 65s a loose-scaled budget would allow, so the wrongly-bounded client
    /// still answers in time and that test stays green. This one is sized against the **loose**
    /// budget precisely to close that gap, which is why it is worth its 70 seconds.
    ///
    /// The "unbounded direction" shape (see `assert_still_running`): a silent remote must not be
    /// enough on its own to fail this call, checked past the loose budget with slack. It also
    /// stands as the standing evidence that the new client bounded `fetch_batch`'s `POST` and
    /// nothing else on the no-redirect side — `fetch_subtree_is_not_flat_bounded_by_silence` covers
    /// only the auto-following client, so it cannot witness this at all.
    #[test]
    fn update_ref_stays_unbounded_against_the_loose_silence_budget() {
        let remote = SilentRemote::start();
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let check_after = TEST_DIRECT_CONNECT_TIMEOUT + TEST_LOOSE_READ_TIMEOUT
            + std::time::Duration::from_secs(5);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(assert_still_running(
            "update_ref", check_after, client.update_ref("main", None, &"a".repeat(64)),
        ));
    }

    /// The discriminating fixture for `resolve`'s own [`Posture::TotalDeadline`]: a remote that
    /// connects instantly (loopback) and then stays silent for a fixed delay before answering
    /// correctly — it *does* eventually answer, which is exactly why this test alone cannot pin
    /// termination (see the note on that below). The test itself asserts that this delay sits
    /// strictly between the *old* flat bound `resolve` used to carry (a plain
    /// `RequestBuilder::timeout(5s)`, not mirrored as a `TEST_*` constant since the production
    /// value it mirrored no longer exists to drift against) and the *new*
    /// [`Posture::TotalDeadline`] budget (`TEST_DIRECT_CONNECT_TIMEOUT + TEST_TIGHT_READ_TIMEOUT`
    /// — the same arithmetic production computes via [`bounded_read_timeout`]) — so a future
    /// change to either bound fails this test loudly instead of leaving a stale comment. Before
    /// the fix: the old 5s deadline fires mid-wait, `send` returns `Err`, and `resolve` falls
    /// back to an empty map — pseudonyms, even though the remote was healthy and answering. After
    /// the fix: the delay is comfortably inside the new budget, so the real mapping comes back.
    /// Asserts the returned mapping itself, not merely that the call returned promptly — a
    /// timing-only assertion is green in both worlds (5s empty-map return, ~15s real-map return),
    /// so it would pin nothing.
    ///
    /// **Not a termination pin.** This fixture always answers eventually, so it is green whether
    /// `resolve` is bounded at ~15s *or not bounded at all* — exactly the gap that let `resolve`
    /// briefly ride [`Posture::BoundedReads`] (a silence budget, no total bound) through review:
    /// this test stayed green throughout, because its remote is never silent forever. It still
    /// earns its keep by pinning the *lower* edge (must not still be the old flat 5s), which
    /// `resolve_times_out_against_a_silent_remote` below does not cover — that one pins
    /// termination against a remote that never answers at all, the property this one structurally
    /// cannot test.
    #[test]
    fn resolve_survives_silence_past_the_old_five_second_bound() {
        // Not a `TEST_*` mirror: this is `resolve`'s own historical bound, a value this PR
        // deletes from production entirely — there is no live constant left to mirror or drift
        // against, only the fixed number the fix replaced.
        let old_flat_bound = std::time::Duration::from_secs(5);
        let new_budget = TEST_DIRECT_CONNECT_TIMEOUT + TEST_TIGHT_READ_TIMEOUT;
        let delay = std::time::Duration::from_secs(6);

        assert!(
            delay > old_flat_bound,
            "the fixture's delay {:?} must exceed the old flat bound {:?}, or this test no \
            longer discriminates the old behavior from the new — it would pass even against the \
            reverted fix", delay, old_flat_bound
        );
        assert!(
            delay < new_budget,
            "the fixture's delay {:?} must stay under the new budget {:?}, or this test would \
            fail for an unrelated reason (the remote genuinely never answering in time), not the \
            one it exists to catch", delay, new_budget
        );

        let url = start_slow_answering_remote(delay, r#"{"names":{"agent-1":"Real Display Name"}}"#);
        let client = RemoteClient::new(&url, None).unwrap();
        let outer_ceiling = new_budget + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let names = runtime.block_on(async {
            tokio::time::timeout(outer_ceiling, client.resolve(vec!["agent-1".to_string()])).await
        })
            .unwrap_or_else(|_| panic!(
                "resolve hung past its own generous outer ceiling {:?}", outer_ceiling
            ));

        assert_eq!(
            names.get("agent-1").map(String::as_str), Some("Real Display Name"),
            "a remote that answers correctly after {:?} — past the old flat 5s bound, well \
            inside the new connect+read budget — must return the real mapping, not fall back \
            to pseudonyms: got {:?}", delay, names
        );
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
    /// `upload_object`'s own wiring. It is the higher-risk site: it calls
    /// `clients::Clients::send_with_watchdog` with `RequestDestination::Presigned` (no bearer token
    /// attached, by construction — see that destination variant's own doc), so nothing about
    /// `upload_object`'s `RequestDestination::Authenticated` wiring guarantees this one got the
    /// same fix, even though both now ride the same operation and the same no-auto-follow client.
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

    /// The falsifying test for the one behaviour the send-owner seam had to reproduce by hand.
    /// Every json call site used to reach the wire through `RequestBuilder::json`, which sets
    /// `Content-Type: application/json` itself; the seam pre-serializes instead and hands the
    /// bytes to `RequestBuilder::body`, which sets no content type at all — so
    /// [`clients::SendBody::Json`] has to set the header. A single `Body(Vec<u8>)` variant would
    /// compile and pass every other test in this file: the `TcpListener` fixtures never inspect a
    /// request header unless, like [`start_header_capturing_remote`], they are written to. What
    /// it would break is `forklift-server`, whose `axum::Json<T>` extractor refuses a body with
    /// no json content type — see [`clients::SendBody`]'s own doc, which also names the head that
    /// would *not* have noticed.
    ///
    /// **Both directions in one test, because the claim has two halves.** Asserting only that
    /// json carries the header leaves "the `Bytes` arm does not set it" — the entire reason the
    /// two variants are distinct rather than one — with no falsifier at all, and an
    /// implementation that set `application/json` unconditionally would pass. `put_trust` is the
    /// json site (a `TrustAnchorDto` body) and `upload_signature` the bytes site (a raw signature
    /// sidecar); both are mutations that treat the fixture's `200` as success, so each returns
    /// `Ok` once its headers have been captured.
    #[test]
    fn the_json_arm_sets_a_json_content_type_and_the_bytes_arm_does_not() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

        let (json_url, json_rx) = start_header_capturing_remote();
        let json_client = RemoteClient::new(&json_url, None).unwrap();
        let anchor = TrustAnchorDto {
            genesis: "g".repeat(64),
            enabled_at: 0,
            boundary: Vec::new(),
            prior_genesis: None,
            adopts: None,
        };
        runtime.block_on(json_client.put_trust(&anchor))
            .expect("a 200-answering remote must let put_trust succeed");
        let json_headers = json_rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("the fixture must have captured request headers by the time the call returned");

        assert!(
            json_headers.to_ascii_lowercase().contains("content-type: application/json"),
            "a SendBody::Json site must declare its body as json — forklift-server's axum Json \
            extractor refuses a body without the header, and nothing else on this path sets it: {}",
            json_headers
        );

        let (bytes_url, bytes_rx) = start_header_capturing_remote();
        let bytes_client = RemoteClient::new(&bytes_url, None).unwrap();
        runtime.block_on(bytes_client.upload_signature(&"d".repeat(64), vec![9u8; 64]))
            .expect("a 200-answering remote must let upload_signature succeed");
        let bytes_headers = bytes_rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("the fixture must have captured request headers by the time the call returned");

        assert!(
            !bytes_headers.to_ascii_lowercase().contains("application/json"),
            "a SendBody::Bytes site must not claim its raw body is json — that difference is the \
            whole reason Json and Bytes are separate variants: {}",
            bytes_headers
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
    // against this pinned `reqwest`/`tower-http` version — see `describe_mutation_redirect`'s doc),
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
    /// check than `put_presigned`'s since the mechanism (`describe_mutation_redirect`) is shared
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
    // both rode the auto-following client, which auto-follows. `tower-http`'s `follow_redirect`
    // middleware (`follow_redirect/mod.rs`, `SEE_OTHER` arm) unconditionally forces the body to
    // `BodyRepr::Empty` and rewrites the method to `GET` *before* the `take()` guard the
    // `MOVED_PERMANENTLY | FOUND` arm relies on to skip non-`POST` methods — so a `303` to a
    // streamed `PUT` silently becomes a bare `GET`, and a `2xx` at the target makes the call
    // return `Ok(())` having stored nothing. The 302/307 tests above cannot catch this: a `PUT`
    // simply misses the `method == POST` condition in the other arm. The fix routes both sites
    // through the never-auto-follow client (today: `clients::Clients::send_with_watchdog`,
    // routing around `Posture` entirely) instead of special-casing `303`, so no future
    // tower-http redirect-matrix change can reopen this hole.
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
    /// fix (the auto-following client) this returns `Ok(())` with the landing flag `true` —
    /// the redirect target really was reached as a bare `GET` and its `200` read back as success.
    /// After the fix (the never-auto-follow client) the raw `303` comes back to the existing
    /// guard, the landing flag stays `false`, and the error names the status and location by the
    /// same mechanism the 302/307 tests already pin.
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

    /// Same hole, `upload_object` site — both call sites now reach the same
    /// `clients::Clients::send_with_watchdog` operation and the same no-auto-follow client, but
    /// each builds its own `RequestDestination` (`Authenticated` here, `Presigned` for
    /// `put_presigned`) and could in principle regress independently of the other, so each keeps
    /// its own falsifier rather than trusting `put_presigned`'s test to cover this site too.
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

    // -----------------------------------------------------------------------------------
    // FORK-89: `update_ref`, `upload_signature`, and `put_trust` had no `is_redirection()` guard
    // at all and rode the auto-following client, unlike `upload_object`/`put_presigned` above.
    // A `303` to any of the three — a `PUT` for the latter two, exactly the shape the 303 hole
    // above already proves is silently followed on the auto-following client — became a bare
    // `GET`, and a `2xx`
    // at the target read back as a fabricated success having mutated nothing. `update_ref` is a
    // `POST`, so it carries a second, wider exposure the two `PUT`s do not: `tower-http`'s
    // `MOVED_PERMANENTLY | FOUND` arm *also* forces a `POST`'s method to `GET` and body to empty,
    // conditioned on `method == POST` — a condition a `PUT` never satisfies, so `301`/`302` never
    // touched `upload_signature`/`put_trust` even before this fix, but did silently follow for
    // `update_ref`. Each site changes its client selection independently, so each needs its own
    // falsifier — same reasoning as the `upload_object`/`put_presigned` pair above.
    // -----------------------------------------------------------------------------------

    /// `update_ref` must not silently follow a `303` to a `2xx` landing. Before the fix (the
    /// implicit-default client, then the auto-following client) this returns `Ok(())` with the
    /// landing flag `true`; after (the never-auto-follow client + the `is_redirection()` guard)
    /// the raw `303` is reported by name and the landing listener is never contacted.
    #[test]
    fn update_ref_does_not_follow_a_303_to_a_2xx_landing() {
        let (landing_url, landed) = start_landing_remote();
        let url = start_redirecting_remote(303, &landing_url);
        let client = RemoteClient::new(&url, None).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let error = runtime.block_on(client.update_ref("main", None, &"a".repeat(64)))
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

    /// `update_ref`'s `POST`-specific exposure: a `302` is the case `upload_signature`/`put_trust`
    /// (both `PUT`s) were never vulnerable to even on the unfixed auto-following client —
    /// `tower-http`'s `MOVED_PERMANENTLY | FOUND` arm only rewrites a `POST`. This is exactly the
    /// asymmetry an earlier version of this test suite missed by only covering `303`: a
    /// regression that moved `update_ref` back onto the auto-following client while somehow
    /// keeping `upload_signature`/`put_trust` on the never-auto-follow one would pass every `303`
    /// test here and still silently drop a lift's ref update.
    #[test]
    fn update_ref_does_not_follow_a_302_to_a_2xx_landing() {
        let (landing_url, landed) = start_landing_remote();
        let url = start_redirecting_remote(302, &landing_url);
        let client = RemoteClient::new(&url, None).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let error = runtime.block_on(client.update_ref("main", None, &"a".repeat(64)))
            .expect_err("a 302 must not be silently followed to a 2xx landing");

        assert!(
            error.to_lowercase().contains("redirect") && error.contains("302") && error.contains(&landing_url),
            "must name the redirect, its status, and its target: {}", error
        );
        assert!(
            !landed.load(std::sync::atomic::Ordering::SeqCst),
            "the landing listener must never have been contacted — that is what proves no \
            follow was attempted, rather than merely that an error came out"
        );
    }

    /// `upload_signature` must not silently follow a `303` to a `2xx` landing — the same hole,
    /// its own client selection (`send_on(Posture::TotalDeadlineNoRedirect(self
    /// .single_write_budget()), ...)`, the never-auto-follow client).
    #[test]
    fn upload_signature_does_not_follow_a_303_to_a_2xx_landing() {
        let (landing_url, landed) = start_landing_remote();
        let url = start_redirecting_remote(303, &landing_url);
        let client = RemoteClient::new(&url, None).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let error = runtime.block_on(client.upload_signature(&"c".repeat(64), vec![1u8; 64]))
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

    /// `put_trust` must not silently follow a `303` to a `2xx` landing — the same hole, its own
    /// client selection.
    #[test]
    fn put_trust_does_not_follow_a_303_to_a_2xx_landing() {
        let (landing_url, landed) = start_landing_remote();
        let url = start_redirecting_remote(303, &landing_url);
        let client = RemoteClient::new(&url, None).unwrap();
        let anchor = TrustAnchorDto {
            genesis: "g".repeat(64),
            enabled_at: 0,
            boundary: Vec::new(),
            prior_genesis: None,
            adopts: None,
        };

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let error = runtime.block_on(client.put_trust(&anchor))
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
