//! The client side of the remote protocol (`docs/format/REMOTE_PROTOCOL.md`): the HTTP
//! client and the sync engines behind `lift`, `lower` and `clone`. Everything here
//! returns data — the commands own the words.
//!
//! Transfers are parallel by design (DESIGN.html §4.1): object fetches and uploads fan
//! out over concurrent connections, bounded by [`CONCURRENT_TRANSFERS`].

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use crate::model::remote::{
    CommitLiftRequest, ErrorResponse, HoldAnswer, HoldsRequest, HoldsResponse,
    MissingObjectsRequest, MissingObjectsResponse, RefUpdateRequest, ResolveRequest,
    ResolveResponse, TrustAnchorDto, UploadTargetsRequest, UploadTargetsResponse, WarehouseInfo,
    BATCH_CUT_BUDGET, BATCH_CUT_DEADLINE, BATCH_HEADER, LIFT_COMMIT_HEADER, LIFT_COMMIT_NOT_READY, MAX_HOLDS_CANDIDATES, MAX_MISSING_BATCH,
    MAX_UPLOAD_TARGETS_BATCH, PROTOCOL_VERSION,
};
use crate::enums::config_scope::ConfigScope;
use crate::error::{CoreError, RefusalCode};
use crate::globals::{self, StorageRootScope};
use crate::util::office_utils::OFFICE_PALLET_NAME;
use crate::util::lock_utils::RefLock;
use crate::util::pallet_utils::RefPreconditions;
use crate::util::progress_utils::Progress;
use crate::util::scope_utils::{self, PathScope, ScopeClass};
use crate::util::body_utils::{self, BodyReadError, JsonReadError};
use crate::util::object_utils::MAX_OBJECT_BYTES;
use crate::util::{
    audit_utils, bundle_utils, config_utils, fetch_ledger, file_utils, haul_utils, held_heads, merge_utils, meta_tree, object_utils,
    office_utils, origin_utils, pack_utils, pallet_utils, sealed_utils, sign_utils, take_utils, trust_utils,
};

/// How many object transfers run concurrently.
pub const CONCURRENT_TRANSFERS: usize = 24;

/// How many response chunks may wait between a batch or subtree transfer and its importer.
const IMPORT_CHANNEL_DEPTH: usize = 16;

/// One piece of a response body on its way to the importer. `End` is sent only when the transfer
/// read the body to its end, so a channel that closes without it was cut short.
enum BodyPiece {
    Bytes(bytes::Bytes),
    End,
}

/// A blocking reader over the pieces a transfer forwards — the importer's side of the channel.
struct ChannelBody {
    receiver: tokio::sync::mpsc::Receiver<BodyPiece>,
    current: bytes::Bytes,
    ended: bool,
}

impl ChannelBody {
    fn new(receiver: tokio::sync::mpsc::Receiver<BodyPiece>) -> ChannelBody {
        ChannelBody { receiver, current: bytes::Bytes::new(), ended: false }
    }
}

impl std::io::Read for ChannelBody {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.current.is_empty() {
            if self.ended || buf.is_empty() {
                return Ok(0);
            }

            match self.receiver.blocking_recv() {
                Some(BodyPiece::Bytes(chunk)) => self.current = chunk,
                Some(BodyPiece::End) => self.ended = true,
                None => return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the transfer stopped before the response was complete",
                )),
            }
        }

        let n = buf.len().min(self.current.len());
        buf[..n].copy_from_slice(&self.current.split_to(n));
        Ok(n)
    }
}

/// The largest handshake body read. Its size grows with the warehouse's pallet list, which has
/// no limit of its own, so the cap is set far past any real warehouse — a few hundred thousand
/// pallets — while still bounding what a hostile remote can make a client hold.
const HANDSHAKE_BODY_CAP: u64 = 64 * 1024 * 1024;

/// Per-hash allowance in a `missing` answer: a 64-character hash, its quotes and separator, with
/// room to spare.
const MISSING_BODY_BYTES_PER_HASH: u64 = 128;

/// Per-candidate allowance in a `holds` answer: a 64-character hash, its quotes, the longest
/// answer and the separators, with room to spare.
const HOLDS_BODY_BYTES_PER_CANDIDATE: u64 = 128;

/// The cap for a structured error body. A refusal may list paths or hashes, so it is not held to
/// [`body_utils::SMALL_BODY_CAP`]; one larger than this is reported as unread rather than parsed.
const ERROR_BODY_CAP: u64 = 1024 * 1024;

/// Read a refusal's error body within [`ERROR_BODY_CAP`] and `budget`. A body that cannot be read
/// in time, or at all, stands in as the status's reason; one over the cap says it was not read.
async fn read_error_body(response: reqwest::Response, budget: std::time::Duration) -> ErrorResponse {
    let status = response.status();
    let reason = || status.canonical_reason().unwrap_or("unknown error").to_string();
    let unread = |error: String| ErrorResponse { error, code: None, next_step: None };

    let read = body_utils::read_json_capped::<ErrorResponse>(response, ERROR_BODY_CAP);

    match tokio::time::timeout(budget, read).await {
        Ok(Ok(body)) => body,
        Ok(Err(JsonReadError::Body(BodyReadError::OverCap { cap }))) => unread(format!(
            "{} (the remote's description of the error is larger than {} bytes and was not read)", reason(), cap
        )),
        Ok(Err(_)) | Err(_) => unread(reason()),
    }
}

/// Per-hash allowance in an `upload-targets` answer: a presigned upload URL (about 500 bytes in
/// practice) keyed by its hash.
const UPLOAD_TARGETS_BODY_BYTES_PER_HASH: u64 = 8 * 1024;

/// The most identifier bytes, as serialised, one resolve request carries, so a roster of long
/// identifiers is split rather than refused whole. A quarter of the smallest body limit a server
/// can set above zero (1 MiB), which leaves the request's fixed envelope ample room. An
/// identifier longer than this on its own still goes, alone in its request.
const RESOLVE_REQUEST_IDENTIFIER_BYTES: usize = 256 * 1024;

/// The most resolve requests in flight at once: enough that a large roster costs a few deadlines
/// rather than one per request, few enough not to swamp a server's directory hook.
const RESOLVE_REQUESTS_IN_FLIGHT: usize = 4;

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
        .map(encode_path_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Percent-encode a value as ONE URL path segment against [`PATH_SEGMENT`], `/` included.
fn encode_path_segment(value: &str) -> String {
    utf8_percent_encode(value, PATH_SEGMENT).to_string()
}

/// The path of the ref-update and delete routes of `pallet`, `/v1/pallets/{pallet}`. The name
/// travels as literal segments (`feature/x`, never `feature%2Fx`) and always last in the path:
/// an API gateway or reverse proxy that decodes `%2F` before routing would split a name sent as
/// one encoded segment, while literal segments arrive as they were sent, and with nothing after
/// the name no literal can be mistaken for one of its components. A name that is no valid
/// reference is refused here, never sent: the same intermediaries may collapse `//` and resolve
/// `.` and `..` segments, so the server could not be relied on to see, and refuse, what was sent.
fn pallet_route(pallet: &str) -> Result<String, String> {
    Ok(format!("/v1/pallets/{}", encode_path_segments(sendable_pallet(pallet)?)))
}

/// The path of the holds route of `pallet`, `/v1/holds/{pallet}`, the name carried and checked as
/// in [`pallet_route`].
fn holds_route(pallet: &str) -> Result<String, String> {
    Ok(format!("/v1/holds/{}", encode_path_segments(sendable_pallet(pallet)?)))
}

/// `pallet` if it parses as a reference ([`pallet_utils::PalletRef::parse`]), else the refusal.
fn sendable_pallet(pallet: &str) -> Result<&str, String> {
    pallet_utils::PalletRef::parse(pallet)?;
    Ok(pallet)
}

/// How many objects one batch-fetch request asks for: bounds the response the server
/// builds in memory while still amortizing the round trip over many objects.
const BATCH_FETCH_CHUNK: usize = 512;

/// The backoff between retried attempts, a staged lift's commits and a throttled object fetch's
/// GETs alike: ~0.2 s doubling to a 3 s cap. Every sleep spends from the caller's own deadline
/// ([`Budgets::commit_deadline`], [`Budgets::throttle_patience`]), which is what bounds the
/// retrying, so the schedule only paces it.
const RETRY_BACKOFF_START: std::time::Duration = std::time::Duration::from_millis(200);
const RETRY_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(3);

/// How long a fetch keeps retrying a `429`, from its first attempt (see
/// [`RemoteClient::send_waiting_out_throttle`]). Uncalibrated policy: the hosted API's default-route
/// throttle (50 rps, burst 100) is what a loose fallback of [`CONCURRENT_TRANSFERS`] GETs runs into,
/// and nothing measures how long it takes to drain. A fetch still throttled past this fails loudly
/// with the remote's answer.
const FETCH_THROTTLE_PATIENCE: std::time::Duration = std::time::Duration::from_secs(30);

/// The wait before retrying a throttled attempt: the remote's `Retry-After` when it sent one,
/// never shorter than the backoff, so a `Retry-After: 0` cannot become a tight loop.
fn throttle_wait(retry_after: Option<std::time::Duration>, backoff: std::time::Duration) -> std::time::Duration {
    retry_after.map_or(backoff, |retry_after| retry_after.max(backoff))
}

/// `wait` plus a random share of up to half of `backoff`, so the [`CONCURRENT_TRANSFERS`] fetches
/// one throttle refused together do not all come back in the same instant and drain the bucket
/// again. Only ever lengthens `wait`, so a `Retry-After` floor still holds.
fn jittered(wait: std::time::Duration, backoff: std::time::Duration) -> std::time::Duration {
    use rand::RngCore;

    let share = rand::rngs::OsRng.next_u32() as f64 / u32::MAX as f64;
    wait + backoff.mul_f64(share / 2.0)
}

/// How long a staged lift keeps retrying one commit batch, on top of the connect bound: every
/// attempt, backoff sleep and `Retry-After` wait spends from it, so a batch fails within
/// connect + this however many attempts it makes. Uncalibrated policy, like the attempt count it
/// replaced: nothing committed measures a head's promotion lag. Long enough to outlast ordinary
/// verifier lag and a few gateway timeouts; short enough that a stuck verifier surfaces as an error.
const COMMIT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

/// The post-connect allowance of one commit attempt, capped by what the deadline leaves. 30 s clears
/// API Gateway's hard 29 s integration timeout, so against a gateway-fronted head an attempt ends
/// with the gateway's own answer rather than this bound.
const COMMIT_ATTEMPT_ALLOWANCE: std::time::Duration = std::time::Duration::from_secs(30);

/// `commit_lift` rides [`Posture::HeadDeadlineNoRedirect`] with at most this as its head budget, so
/// the same ordering [`BATCH_HEAD_PATIENCE`]'s assert pins for `fetch_batch` binds here: the attempt
/// bound has to fire before the client-level read_timeout on the same request.
const _: () = assert!(
    COMMIT_ATTEMPT_ALLOWANCE.as_nanos() < FETCH_OBJECT_READ_TIMEOUT.as_nanos(),
    "COMMIT_ATTEMPT_ALLOWANCE must stay strictly under FETCH_OBJECT_READ_TIMEOUT: a commit \
    attempt's head-wait timer has to fire before the client-level read_timeout on the same request"
);

/// The least post-connect time an attempt must have to be started: enough for a status line to
/// come back from a head that is already answering. Also what keeps every attempt's total deadline
/// strictly above the connect bound (`check_total_deadline_payload`), so a connect-phase stall
/// still reads as a failure to connect.
const MIN_COMMIT_ATTEMPT: std::time::Duration = std::time::Duration::from_secs(2);

/// One lift-commit attempt that did not fail terminally, classified from the status line and
/// headers alone — never the body, which a reset or a slow link can cut short and so cannot be
/// what decides whether to retry. Everything but `Committed` is retried until the deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CommitAttempt {
    /// The session's objects are verified and promoted; the ref update may proceed.
    Committed,

    /// The remote's own word (the [`LIFT_COMMIT_HEADER`] header): a blob's upload is still staged
    /// and not yet accepted into the warehouse.
    NotReady,

    /// A `429`: the remote rate-limited the commit. `retry_after` is its `Retry-After` in
    /// delta-seconds, when it sent a readable one.
    Throttled { retry_after: Option<std::time::Duration> },

    /// A `5xx` without the not-ready header: a gateway timeout, a crash, a bug. The commit is
    /// idempotent and promotion resumes where it stopped, so a retry is safe and may finish it.
    Undetermined { status: u16 },

    /// The connection could not be made, so nothing was sent.
    NotSent { detail: String },

    /// The request may have reached the remote but no answer came back: a connection that failed
    /// after connecting, or the attempt's deadline firing.
    Lost { detail: String },
}

impl CommitAttempt {
    /// Whether the remote answered with a status line. Only an answered attempt says anything about
    /// the remote, so only one may pick the message a deadline reports.
    fn answered(&self) -> bool {
        matches!(self, CommitAttempt::Committed | CommitAttempt::NotReady
            | CommitAttempt::Throttled { .. } | CommitAttempt::Undetermined { .. })
    }
}

/// A `Retry-After` header's value as delta-seconds; `None` for an HTTP date or anything else.
fn retry_after_seconds(response: &reqwest::Response) -> Option<std::time::Duration> {
    let value = response.headers().get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;

    value.trim().parse::<u64>().ok().map(std::time::Duration::from_secs)
}

/// `url`'s path and query, the part after its origin. Built from the parts: slicing the
/// serialization at the origin's length would cut wrongly whenever the URL carries userinfo,
/// which the serialization keeps and the origin omits.
fn origin_relative_path(url: &reqwest::Url) -> String {
    match url.query() {
        Some(query) => format!("{}?{}", url.path(), query),
        None => url.path().to_string(),
    }
}

/// Whether a status means the remote does not implement an endpoint at all (an older build):
/// a `404` (no such route) or `405` (the path exists for other methods only). The caller falls
/// back to the legacy path. Any other non-success status is a real error, not an absence.
fn endpoint_absent(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::METHOD_NOT_ALLOWED
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

/// What one [`RemoteClient::fetch_batch`] call came to, short of an error. Every outcome but
/// `Imported` and `Partial` closes the remote's batch lane; see that function's doc.
#[derive(Debug)]
pub enum BatchOutcome {
    /// The bundle was read to its end and imported.
    Imported(bundle_utils::ImportStats),
    /// The bundle was read to its end and imported, but the remote marked it cut at its byte
    /// budget ([`BATCH_CUT_BUDGET`]): it stopped before looking for everything asked, and the rest
    /// is worth asking for again.
    Partial(bundle_utils::ImportStats),
    /// The bundle was read to its end and imported, but the remote marked it cut for running short
    /// of time ([`BATCH_CUT_DEADLINE`]).
    OutOfTime(bundle_utils::ImportStats),
    /// The remote has no batch endpoint.
    EndpointAbsent,
    /// The remote sent no status line in time, so the batch was abandoned.
    Degraded,
}

/// How [`RemoteClient::fetch_batch_as`] reads a batch's bundle.
enum BatchRead {
    /// Into the store.
    Import,
    /// To the receiver, object by object.
    Deliver(Box<dyn FnMut(String, Vec<u8>) -> Result<(), String> + Send>),
}

/// Why the remote's batch download stopped being used, after which the command fetched one
/// object at a time.
// Read from `RemoteClient::batch_lane_closure` once the command's fetches are done. The lane
// belongs to the client, not to one fetch: every fetch a command makes through the same client
// (the office, the meta pallets, the pallet itself) shares it, so the command reports it once
// rather than each fetch's stats carrying a copy.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchLaneClosure {
    /// The remote does not serve batch downloads.
    EndpointAbsent,
    /// The remote was too slow to start a batch answer.
    Degraded,
    /// The remote ran short of time to answer a batch, and sent only part of it.
    OutOfTime,
}

/// What a lift actually transferred.
pub struct LiftStats {
    pub new_parcels: usize,
    pub uploaded_objects: usize,
    pub uploaded_signatures: usize,
    pub old_head: Option<String>,

    /// Why the landed head could not be added to the held-heads record, if it could not. The lift
    /// stands; only a candidate for a later prune is lost, so the caller warns.
    pub unrecorded: Option<String>,
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
    /// Case- and surrounding-whitespace-insensitive.
    ///
    /// **The unrecognized-value fallback is no longer reachable from configuration.**
    /// `config_utils::parse_config` refuses any `remote.tor` outside
    /// `config_utils::REMOTE_TOR_VALUES` (`auto`/`on`/`off`) while reading the file, precisely
    /// because falling back to `Auto` here is a silent privacy failure — it leaves every
    /// non-onion remote un-proxied for a user who believes they configured `on`. The fallback
    /// survives for a direct caller, and so does the wider vocabulary (`true`/`yes`/`1`), which
    /// the file parse does *not* accept.
    pub(crate) fn parse(value: &str) -> TorMode {
        match normalize_tor_value(value).as_str() {
            "on" | "true" | "yes" | "1" => TorMode::On,
            "off" | "false" | "no" | "0" => TorMode::Off,
            _ => TorMode::Auto,
        }
    }
}

/// The one normalization a `remote.tor` value gets, wherever it is read or checked: surrounding
/// whitespace trimmed, ASCII case folded.
///
/// Exported because the check and the read live in different modules — `config_utils` decides
/// whether a value is *admissible* (`auto`/`on`/`off`) and [`TorMode::parse`] decides what it
/// *means*. Both answers have to be computed from the same spelling or the pair silently drifts:
/// a value the check accepts under one normalization and the reader resolves to [`TorMode::Auto`]
/// under another is exactly the un-proxied-clearnet-dial failure the check exists to prevent.
/// They each had their own copy of this line; now there is one.
pub fn normalize_tor_value(value: &str) -> String {
    value.trim().to_ascii_lowercase()
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
    /// to the defaults for anything *unset*.
    ///
    /// **An unreadable configuration file is an error, not a default.** These two keys decide
    /// whether a remote is dialled through Tor at all, so the failure direction matters: a
    /// broken file degrading to [`TorMode::Auto`] silently drops the user out of the mode they
    /// configured and dials a clearnet remote directly. Absent is a default; unparseable is a
    /// refusal that names the file and the key.
    ///
    /// The warehouse configuration is consulted first (see [`config_utils::get_effective_value`]):
    /// correct for every caller that already has "a warehouse" of its own to have an opinion —
    /// which is every caller except one. See [`Self::from_global_config`] for that exception.
    ///
    /// # Returns
    /// * `Ok(TorSettings)` - The configured settings, defaulted for anything unset.
    /// * `Err(String)`     - A configuration file could not be read or does not parse.
    pub fn from_config() -> Result<TorSettings, String> {
        let mode = match config_utils::get_effective_value(config_utils::KEY_REMOTE_TOR)? {
            Some((value, _)) => TorMode::parse(&value),
            None => TorMode::Auto,
        };

        let proxy = config_utils::get_effective_value(config_utils::KEY_REMOTE_TOR_PROXY)?
            .map(|(value, _)| value)
            .unwrap_or_else(|| DEFAULT_TOR_PROXY.to_string());

        Ok(TorSettings { mode, proxy })
    }

    /// Like [`Self::from_config`], but *global* configuration only — never the warehouse scope.
    ///
    /// The one caller this exists for is `clone`'s initial handshake, which now (deliberately)
    /// runs before the target warehouse exists, so there is no "this warehouse" to have an
    /// opinion about Tor — only the ambient working directory, which may happen to sit inside a
    /// *different*, unrelated warehouse. `from_config`'s effective (warehouse-then-global) lookup
    /// would silently pick up that unrelated warehouse's `remote.tor`/`remote.torProxy` — a
    /// cwd-dependent, security-relevant surprise for a brand-new clone elsewhere. Before clone
    /// moved its handshake earlier, this could never happen: the handshake ran *inside* the fresh
    /// (still-empty) target, whose warehouse scope had never had anything to set — so global was,
    /// in effect, the only scope that could ever apply. This preserves that.
    pub fn from_global_config() -> Result<TorSettings, String> {
        let mode = match config_utils::get_scoped_value(config_utils::KEY_REMOTE_TOR, ConfigScope::Global)? {
            Some(value) => TorMode::parse(&value),
            None => TorMode::Auto,
        };

        let proxy = config_utils::get_scoped_value(config_utils::KEY_REMOTE_TOR_PROXY, ConfigScope::Global)?
            .unwrap_or_else(|| DEFAULT_TOR_PROXY.to_string());

        Ok(TorSettings { mode, proxy })
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
/// [`RemoteClient::download_bundle`] — each of whose server side does O(constant) work (a single
/// lookup, or serving an already-built file) before writing anything, so a flat 10s pre-first-byte
/// budget is honest. `fetch_object` needs a much looser budget of its own
/// ([`FETCH_OBJECT_READ_TIMEOUT`]) since its server side is size-dependent, not O(constant).
/// `fetch_batch` and `fetch_subtree` share the reason neither may ride *this* budget — their server
/// sides build a bundle, every requested object fully into memory, *before* the first byte, work
/// whose cost depends on object sizes the client cannot know in advance, so no flat silence budget
/// over their responses is honest. Both took the other exit named just above: each is bounded
/// *before the status line* by a different mechanism entirely — a head-wait timer carried on
/// [`Posture::HeadDeadlineNoRedirect`] and sized by [`BATCH_HEAD_PATIENCE`] — which prices no
/// server-side build work at all, and *after* the status line by a client-level silence budget of
/// its own, [`FETCH_OBJECT_READ_TIMEOUT`] rather than this constant (see that constant's doc for
/// why the looser figure, and [`Posture::HeadDeadlineNoRedirect`]'s for how the two phases hand
/// off). That split is what keeps the second budget from ever pricing the build: the head timer is
/// strictly the tighter of the two, so no client-level read timeout can fire before the status
/// line. See the comment at each call site. `missing_objects`/`upload_targets` take the same two
/// phases on a redirect-following client ([`Posture::HeadDeadline`]): a head-wait sized by
/// [`RemoteClient::presence_negotiation_budget`], then the same object-read silence budget on the
/// body, inside a total sized by the body's byte cap ([`RemoteClient::presence_body_budget`]).
/// `resolve` is unbounded by *this* mechanism too (no
/// client-level `read_timeout`), but is not left unbounded outright: it rides
/// [`Posture::TotalDeadline`] instead, a genuine per-request `RequestBuilder::timeout` the module
/// applies itself rather than the call site — sized at `connect_timeout + REMOTE_READ_TIMEOUT`,
/// reusing this constant's *value* for that arithmetic without joining the class of calls it
/// silence-bounds (see that variant's own doc for why a *total* deadline, not a silence budget, is
/// the right shape for `resolve` specifically). `update_ref` and the streamed-upload paths
/// (`upload_object`, `put_presigned`) are unbounded for reasons of their own — see each call's own
/// doc, and [`clients::Clients::send_with_watchdog`]'s for the uploads.
///
/// `read_timeout` is a `ClientBuilder`-level setting with no per-request override — it cannot be
/// switched off for one specific request — so a call carries one exactly when the client its
/// posture selects was built with one. [`clients::Clients`]'s field docs are the enumeration that
/// stays current; what matters here is that the client `update_ref` rides
/// ([`Posture::UnboundedNoRedirect`]'s, shared with the watchdog-guarded uploads) carries none, and
/// must not be given one. **`update_ref` must never move to [`Posture::BoundedReads`]** — nor may
/// the client it already rides acquire a `read_timeout`, which is the same hazard by a shorter
/// route, and the cheaper-looking one now that a client combining no-auto-redirect *with* a silence
/// budget exists to copy from: `update_ref`'s server side legitimately runs a parcel-closure audit
/// walk — scoped by the history segment being pushed, which on a first lift into an empty pallet is
/// the whole history — before its first response byte; that can take minutes with *no* bytes moving
/// at all, which this constant would (correctly) call silence if it applied there. This is not an
/// accident of wiring — FORK-94 found `update_ref`'s pre-first-byte cost is a *derived* quantity
/// (an audit walk), unlike `missing_objects`/`upload_targets`, whose priced quantity is the request
/// body's own enumerated content; giving it a flat silence budget before that walk-equivalence
/// question is settled would be exactly the kind of guess this module's bounded clients exist to
/// avoid.
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
/// `forklift-aws-lambda`'s `Head::reject_malformed_batch`) — so that ceiling is a protocol
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
///   objects / 52.5MiB loose, 52ms for the same set after `forklift store compact`.
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
/// **Accepted residuals, both of them.** A legitimately slow head is not waited out: an expiry
/// degrades the fetch to loose GETs for the rest of the command (see
/// [`RemoteClient::fetch_batch`]'s doc), which is slower than the bundle would have been but does
/// not fail, while `fetch_subtree`, which has no such lane, fails loudly. And an expiry abandons
/// the in-flight request, so the head may finish the build anyway — harmless, since the result is
/// ephemeral, but worth saying once. (The third residual an earlier version of this list carried —
/// an unbounded body read — is closed: see [`FETCH_OBJECT_READ_TIMEOUT`]'s doc for the budget that
/// closed it and the terms it is honest on.) The falsifier that reopens **the number, not the
/// mechanism**: a measured legitimate build exceeding this patience against a real deployment.
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

/// The base allowance for an error-body read (`read_error_body`, called by
/// [`RemoteClient::error_of`] under [`RemoteClient::error_body_budget`]), once a
/// non-success status line and headers have already arrived. Needed because no client's own bound
/// covers this: `error_of` serves responses from every client this type builds,
/// including the three deliberately-unbounded ones
/// (`http`/`no_redirect`/`upload_targets`'s negotiation calls — see those calls' own docs), so a
/// remote that answers a `5xx` with full headers and then wedges before writing the JSON body can
/// otherwise hang the caller forever even though the status line already told it the call failed.
///
/// Flat, not scaled like [`FETCH_OBJECT_READ_TIMEOUT`] or the negotiation calls' own read costs:
/// after a non-success status line, the error body this module parses (an [`ErrorResponse`]) is
/// small in practice regardless of which call produced it or why *that* call's own success path
/// is unbounded — the scaling reason a success body might be large or slow never applies to the
/// error one. A refusal that lists paths or hashes can run longer, up to [`ERROR_BODY_CAP`]; one
/// that does not arrive within this budget over a slow link loses its code and next step the same
/// way an unparseable one does.
/// On elapse, falls back to `status.canonical_reason()` — the exact fallback this
/// code already uses when the body fails to parse as JSON at all, so a timeout and a malformed
/// body are indistinguishable to the caller, which is right: both mean "no usable error body
/// arrived."
///
/// Not the actual timeout duration on its own — see
/// [`error_body_read_budget`] for why `self.budgets.connect` is folded in on top of this base,
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
fn error_body_read_budget(budgets: &Budgets) -> std::time::Duration {
    budgets.connect + budgets.error_body_read
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
/// This size sets a **per-connection throughput floor**: hyper pulls the
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

/// The **retryable head-wait floor**: how long this client waits for the status line of a small,
/// idempotent call before it gives up loudly and in a way that is safe to retry. It is shared by
/// every call whose failure costs nothing but a retry: `missing_objects`, `upload_targets`,
/// `upload_signature`, `put_trust`, and the post-send phase of an upload
/// ([`post_send_verify_budget`]).
///
/// **Client patience, not a price on any head's work.** It states how long this caller is willing
/// to wait, the same category as [`REMOTE_CONNECT_TIMEOUT`]. It is not computed from what some head
/// does before answering, and must never be again: an earlier single-write allowance computed
/// itself as `2 * HOOK_CLIENT_TIMEOUT + 5s`, importing `forklift-server`'s per-hook allowance. That
/// was wrong even against that head, whose hooks are optional, and meaningless against
/// `forklift-aws-lambda`, which serves the same routes and runs no hooks. `HOOK_CLIENT_TIMEOUT` now
/// lives in `forklift-server` alone, so a `forklift-core` budget cannot reach it.
///
/// **It bounds the head-wait, never the response.** Each call arms it as connect plus this floor
/// around `send()` alone ([`Posture::HeadDeadline`], [`Posture::HeadDeadlineNoRedirect`]), and the
/// body that follows has a budget of its own: for a success body [`FETCH_OBJECT_READ_TIMEOUT`]'s
/// silence budget, plus, on the presence calls' size-capped answers, a total sized by the cap
/// ([`RemoteClient::presence_body_budget`]); [`RemoteClient::error_body_budget`] for a refusal's.
/// So a refusal whose status line arrives just inside the floor still has its body read whole, and
/// its code survives. A request-spanning deadline cut that body off and turned a typed refusal into
/// an untyped timeout.
///
/// **A floor, raised by size where the call's own request says it should be.** `missing_objects`
/// and `upload_targets` wait `max(floor, n * PRESENCE_ALLOWANCE_MS_PER_OP)` for a chunk of `n`
/// hashes (see [`RemoteClient::presence_negotiation_budget`]). The larger of the two is used, not
/// their sum, so the head-wait stays under the body's silence budget at every chunk size the wire
/// allows; the assertion below pins that.
///
/// **Calibration rows: acceptance checks, not derivations.** Each records one head's measured or
/// tested head-wait against this floor. When a row fails, the choices are to raise the floor (if
/// the raise is still a wait worth making for a small idempotent call), to accept that head's slow
/// tail as a residual, or to redesign. Never to fold the head's cost into the arithmetic.
///
/// - `forklift-aws-lambda`: the worst head-wait measured during FORK-104 was about 9.7s, inside the
///   floor. That was a one-off measurement, not a re-runnable spike; nothing in the tree
///   repeats it.
/// - `forklift-server`, `upload_signature`'s hook pair: the suite asserts each hook fails closed
///   within `[8s, 15s)` against a hook that never answers
///   (`tests::an_authentication_hook_that_never_answers_fails_closed_near_the_hook_timeout` and its
///   admission sibling). No test measures the pair, so the bound it establishes is the sum of the
///   two upper bounds: under 30s, which this floor matches with no margin. A deployment whose two
///   hooks both run to their timeout can therefore be abandoned. That is an accepted residual: the
///   write is idempotent, and the server's own answer in that case is a refusal anyway.
/// - `forklift-server`, presence checks: `spike_fork92_presence_rate` (in `file_utils.rs`) checks
///   the per-hash rate against [`PRESENCE_ALLOWANCE_MS_PER_OP`] directly. It re-runs on demand.
///
/// **Named residuals.** `upload_signature`'s handler on `forklift-server` reads the office state,
/// which is O(roster). That server's hook-bound handlers run on its shared `spawn_blocking` pool,
/// beside the minutes-long ref-update verification, so they can wait in its queue. And a Lambda
/// cold start is uncalibrated here. Any of these surfaces as an uncertain-outcome error on a write
/// that in fact succeeded. Every call this floor serves is idempotent, so a retry is the recovery.
/// The floor is fixed rather than adaptive, so a retry against the same slow condition fails the
/// same way.
const RETRYABLE_HEAD_PATIENCE: std::time::Duration = std::time::Duration::from_secs(30);

/// **The phase-precedence pin for the retryable class.** The same ordering
/// [`BATCH_HEAD_PATIENCE`]'s assert pins for `fetch_batch`: a head-wait timer must fire strictly
/// before the client-level `read_timeout` on the same request, or a post-header stall and a
/// head-wait expiry swap wordings. The scaled presence term is pinned at both wire caps for the
/// same reason, since `presence_negotiation_budget` arms it as the head-wait.
const _: () = {
    assert!(
        RETRYABLE_HEAD_PATIENCE.as_nanos() < FETCH_OBJECT_READ_TIMEOUT.as_nanos(),
        "RETRYABLE_HEAD_PATIENCE must stay strictly under FETCH_OBJECT_READ_TIMEOUT"
    );
    assert!(
        (MAX_MISSING_BATCH as f64 * PRESENCE_ALLOWANCE_MS_PER_OP)
            < FETCH_OBJECT_READ_TIMEOUT.as_millis() as f64,
        "a MAX_MISSING_BATCH chunk's presence allowance must stay strictly under \
        FETCH_OBJECT_READ_TIMEOUT"
    );
    assert!(
        (MAX_UPLOAD_TARGETS_BATCH as f64 * PRESENCE_ALLOWANCE_MS_PER_OP)
            < FETCH_OBJECT_READ_TIMEOUT.as_millis() as f64,
        "a MAX_UPLOAD_TARGETS_BATCH chunk's presence allowance must stay strictly under \
        FETCH_OBJECT_READ_TIMEOUT"
    );
};

/// The conservative inline-verification throughput [`post_send_verify_budget`] assumes for the
/// size-scaled part of its bound. Deliberately far below real-world `blake3` throughput (typically
/// hundreds of MB/s to several GB/s single-threaded on modern hardware) to absorb a busy or loaded
/// server without risking a false stall on a healthy one. At this rate a maximum-size object adds
/// seconds, not minutes, on top of the retryable floor.
const POST_SEND_VERIFY_RATE_BYTES_PER_SEC: u64 = 8 * 1024 * 1024;

/// The slowest delivery [`RemoteClient::presence_body_budget`] waits out for a presence answer's
/// body, applied to the body's cap rather than to its length. Client patience for a slow link, not
/// a price on server work: by the time this clock runs the server has answered, and what is left
/// is transfer. The cap it multiplies is sized for the largest answer a head may send (an
/// `upload-targets` answer is allowed 8 KiB per hash, against about 500 bytes in practice), so a
/// real answer gets many times the time this rate alone would grant it.
const PRESENCE_BODY_RATE_BYTES_PER_SEC: u64 = 64 * 1024;

/// The post-send component of an upload's watchdog budget, for an upload of `body_len` bytes:
/// `base` (production passes [`RETRYABLE_HEAD_PATIENCE`]) plus `body_len` at
/// [`POST_SEND_VERIFY_RATE_BYTES_PER_SEC`]. This alone is *not*
/// [`clients::Clients::send_with_watchdog`]'s post-send budget; see that method's doc for why it
/// also folds in the send-phase allowance as a flush-time margin.
///
/// Once the body is handed off, the client waits for a status line, the same head-wait every
/// retryable call makes, so the fixed part is the retryable floor. The size-scaled part is client
/// patience for a head to check bytes this client already holds: an upload's server side has the
/// whole body before it answers, and the one part of its work this client can size is proportional
/// to `body_len`. That is all the scaling claims. The work a head does in this phase is **not**
/// client-known. `forklift-server` runs its hooks there too, so a fixed base sized for dispatch
/// overhead alone (it used to be 2s) abandoned healthy uploads behind a slow hook. The floor
/// answers that as patience, without pricing any head's hooks.
fn post_send_verify_budget(base: std::time::Duration, body_len: usize) -> std::time::Duration {
    base + std::time::Duration::from_secs_f64(
        body_len as f64 / POST_SEND_VERIFY_RATE_BYTES_PER_SEC as f64
    )
}

/// The per-hash rate that [`RemoteClient::presence_negotiation_budget`] scales
/// `missing_objects`/`upload_targets`'s head-wait by, once a chunk is large enough to lift it above
/// [`RETRYABLE_HEAD_PATIENCE`]. Calibrated client policy: the scaling quantity (`n`, the hashes
/// this client just sent) is the call's own, and the rate is checked against `does_object_exist`'s
/// (`file_utils.rs`) real per-hash cost, the primitive both calls' server side loops over, as an
/// acceptance check rather than derived from it. `pub(crate)`, not `pub` — every sibling budget
/// constant in this file ([`REMOTE_READ_TIMEOUT`], [`UPLOAD_SILENCE_BUDGET`],
/// [`RETRYABLE_HEAD_PATIENCE`]) is module-private; this one only needs to reach `file_utils.rs`'s
/// own FORK-92 measurement spike (`spike_fork92_presence_rate`, `cargo test --release -p
/// forklift-core spike_fork92_presence_rate -- --ignored --nocapture`), which imports this exact
/// constant rather than mirroring its value in a second, unlinked literal — a production change
/// here cannot silently drift out of sync with what that spike actually checks. Same-crate reach is
/// all that import needs, so `pub(crate)` is the right width, not a wider `pub`.
pub(crate) const PRESENCE_ALLOWANCE_MS_PER_OP: f64 = 5.0;

/// Every wall-clock budget a [`RemoteClient`] arms, carried by the instance rather than read from
/// the constants above at each use, so a test can build a client whose budgets are milliseconds and
/// watch each one fire without waiting out the production figure. The constants stay the single
/// statement of the production values; [`Budgets::production`] is the only place that reads them,
/// and every arm site reads the instance instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Budgets {
    /// The connect-phase bound: [`REMOTE_CONNECT_TIMEOUT`], or [`REMOTE_CONNECT_TIMEOUT_TOR`] for
    /// a remote dialed through Tor.
    connect: std::time::Duration,
    /// [`REMOTE_READ_TIMEOUT`]'s role: the silence budget of a constant-work read.
    read: std::time::Duration,
    /// [`FETCH_OBJECT_READ_TIMEOUT`]'s role: the looser silence budget of an object or bundle read.
    object_read: std::time::Duration,
    /// [`BATCH_HEAD_PATIENCE`]'s role: how long `fetch_batch` waits for its status line.
    batch_head_patience: std::time::Duration,
    /// [`ERROR_BODY_READ_TIMEOUT`]'s role: the allowance for reading a non-success response's body.
    error_body_read: std::time::Duration,
    /// [`UPLOAD_SILENCE_BUDGET`]'s role: how long an upload's body stream may stall.
    upload_silence: std::time::Duration,
    /// [`RETRYABLE_HEAD_PATIENCE`]'s role: how long a retryable call waits for its status line.
    retryable_head_patience: std::time::Duration,
    /// [`COMMIT_DEADLINE`]'s role: how long a lift keeps retrying one commit batch.
    commit_deadline: std::time::Duration,
    /// [`COMMIT_ATTEMPT_ALLOWANCE`]'s role: the post-connect allowance of one commit attempt.
    commit_attempt_allowance: std::time::Duration,
    /// [`MIN_COMMIT_ATTEMPT`]'s role: the least post-connect time a commit attempt starts with.
    min_commit_attempt: std::time::Duration,
    /// [`FETCH_THROTTLE_PATIENCE`]'s role: how long `fetch_object` keeps retrying a `429`.
    throttle_patience: std::time::Duration,
}

impl Budgets {
    /// The production budgets for a remote dialed directly or, when `routes_through_tor`, through
    /// Tor. The two differ only in the connect bound.
    const fn production(routes_through_tor: bool) -> Budgets {
        Budgets {
            connect: if routes_through_tor { REMOTE_CONNECT_TIMEOUT_TOR } else { REMOTE_CONNECT_TIMEOUT },
            read: REMOTE_READ_TIMEOUT,
            object_read: FETCH_OBJECT_READ_TIMEOUT,
            batch_head_patience: BATCH_HEAD_PATIENCE,
            error_body_read: ERROR_BODY_READ_TIMEOUT,
            upload_silence: UPLOAD_SILENCE_BUDGET,
            retryable_head_patience: RETRYABLE_HEAD_PATIENCE,
            commit_deadline: COMMIT_DEADLINE,
            commit_attempt_allowance: COMMIT_ATTEMPT_ALLOWANCE,
            min_commit_attempt: MIN_COMMIT_ATTEMPT,
            throttle_patience: FETCH_THROTTLE_PATIENCE,
        }
    }

    /// The phase-precedence pins, for budgets that are not the production ones: the compile-time
    /// asserts beside [`BATCH_HEAD_PATIENCE`], [`RETRYABLE_HEAD_PATIENCE`] and
    /// [`COMMIT_ATTEMPT_ALLOWANCE`] hold the production pairs, and this holds any other pair a test
    /// builds, so no test runs against an ordering production can never have. One pin is not
    /// mirrored: the scaled presence term against `object_read` depends on the batch a test sends,
    /// not on the budgets, so a test sending a batch too large for its own `object_read` trips
    /// [`clients::check_head_deadline_payload`] at the send instead.
    #[cfg(test)]
    fn checked(self) -> Budgets {
        assert!(
            self.batch_head_patience < self.object_read,
            "batch_head_patience ({:?}) must stay strictly under object_read ({:?}), as the \
            production pair is pinned to",
            self.batch_head_patience, self.object_read
        );
        assert!(
            self.retryable_head_patience < self.object_read,
            "retryable_head_patience ({:?}) must stay strictly under object_read ({:?}), as the \
            production pair is pinned to",
            self.retryable_head_patience, self.object_read
        );
        assert!(
            self.commit_attempt_allowance < self.object_read,
            "commit_attempt_allowance ({:?}) must stay strictly under object_read ({:?}), as the \
            production pair is pinned to",
            self.commit_attempt_allowance, self.object_read
        );
        self
    }
}

/// Shared state between an upload's body-send stream ([`UploadChunks`]) and
/// [`clients::Clients::send_with_watchdog`]'s polling loop: a timestamp updated every time the stream
/// actually yields a chunk (the only signal available that the transfer is still moving — see
/// [`UPLOAD_SILENCE_BUDGET`]'s doc), and a flag set once the stream is exhausted.
///
/// That second half does **not** mean "stop bounding the wait". An earlier version of this fix did
/// exactly that, on the theory that it matched `update_ref`'s own unbounded response wait, and the
/// theory does not hold: `update_ref`'s wait is unbounded because its server-side work is the
/// *pushed history segment*, unknowable in advance, while `upload_object` is a small, idempotent
/// write whose answer this client can wait for with ordinary retryable patience — the floor, plus a
/// size-scaled part for the bytes just sent (see [`post_send_verify_budget`]). A remote that reads
/// the whole body and then wedges is an ordinary failure, and with this phase unbounded it hung
/// exactly like the silent remote [`REMOTE_READ_TIMEOUT`]'s doc describes, on this exact path. What
/// exhaustion changes is *which* budget [`clients::Clients::send_with_watchdog`] checks
/// `silent_for()` against, not whether it checks at all — see that method's doc for the two-budget
/// shape and why phase 2's budget still has to be generous: `is_exhausted()` means every chunk was
/// handed to hyper, not that every byte reached the peer, so the post-exhaustion silence can
/// legitimately include time the OS kernel (or hyper's own internal buffer) is still spending
/// flushing an in-flight tail. A 64 MiB object on a 2 Mbit/s uplink can have several seconds of
/// genuinely-still-moving tail queued the instant this flag flips.
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
/// `bytes::Bytes` rather than copied into a `Vec<Vec<u8>>` up front:
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
/// buffer may hold more on top of that — see [`clients::Clients::send_with_watchdog`]'s doc for
/// why the post-send phase's budget has to account for that gap rather than
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
/// applies that posture's own payload (currently [`Posture::TotalDeadline`]'s total deadline, or
/// [`Posture::HeadDeadline`]'s or [`Posture::HeadDeadlineNoRedirect`]'s head-wait budget) on
/// whichever client `pick` chose, sends the request itself, and returns a [`SendOutcome`]; and
/// [`Clients::send_with_watchdog`], which takes the upload's bytes and the pieces of a request,
/// builds the watchdog-guarded body and the request itself, and returns a [`WatchdogOutcome`].
/// Neither function can be skipped in favor of reaching `pick` directly, because nothing outside
/// `mod clients` can name `pick` at all — that is a privacy error, not a convention. A new call
/// site cannot reach any client, nor send a request of any kind, without the compiler forcing it
/// through one of these two, payload-applying paths; there is no ambient "just use the default
/// one" path left to fall into, and no lower-level escape hatch left standing beside them either.
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
    use super::bounded_read_timeout;

    /// Which `reqwest::Client` a request rides, and — since a client-level `read_timeout` alone
    /// does not cover every phase of every call — what else (if anything) bounds that call's
    /// response wait: [`Self::TotalDeadline`]'s own payload, spanning the whole response;
    /// [`Self::HeadDeadline`]'s or [`Self::HeadDeadlineNoRedirect`]'s, spanning only the wait for
    /// the status line and headers, with its client's own silence budget taking over after them;
    /// or nothing at all for [`Self::UnboundedNoRedirect`] (see [`UnboundedTicket`]'s doc).
    /// Every request [`super::RemoteClient`] sends through [`Clients::pick`] must name one of
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
        /// O(constant)-pre-first-byte calls: `fetch_info`, `fetch_signature`, `download_bundle`.
        ///
        /// **Auto-follows redirects — never select this for a mutation.** The client this
        /// selects carries no `redirect::Policy::none()`, for the same reason [`Clients::http`]
        /// doesn't: nothing about adding a `read_timeout` also disables auto-follow, they are
        /// independent axes (this module's own doc names all three). A mutation that rode this
        /// posture would reopen exactly the hole FORK-89 closed — a `3xx` silently followed and
        /// read back as success. So reaching for *this* variant to "just add a bound" to
        /// `upload_signature`/`put_trust` would silently reopen their redirect exposure; they
        /// ride [`Self::HeadDeadlineNoRedirect`] instead, whose client never auto-follows.
        BoundedReads,
        /// [`FETCH_OBJECT_READ_TIMEOUT`]-bounded via a client-level `read_timeout` — the same
        /// shape as [`Self::BoundedReads`], just with the looser budget `fetch_object`'s
        /// size-dependent server work needs. `fetch_object`, and `fetch_batch`'s redirect-follow
        /// `GET` — see [`FETCH_OBJECT_READ_TIMEOUT`]'s doc for why the latter shares it.
        ///
        /// **Auto-follows redirects — never select this for a mutation**, same reasoning and
        /// same trap as [`Self::BoundedReads`]'s own doc.
        ///
        /// Shares its client with [`Self::HeadDeadline`], and its silence budget, but not its
        /// client, with [`Self::HeadDeadlineNoRedirect`] — that one needs the same figure on a
        /// client that never auto-follows. All three therefore render the same duration when a
        /// read timeout fires, which is why [`super::RemoteClient::describe_transport_error`]'s
        /// arms have to be told apart by their words rather than by their figures: `fetch_batch`
        /// routes two of them through one composer call with one action string, so the figure
        /// alone cannot say which of its stations stalled.
        BoundedObjectReads,
        /// Auto-follows redirects; no client-level `read_timeout`. The bound is the payload
        /// itself — a genuine *total* per-request deadline (`RequestBuilder::timeout`) that
        /// [`Clients::send_on`] reads off this variant and applies unconditionally, so the
        /// promise is discharged by the module, not merely asserted at the call site the way the
        /// deleted `OwnTimeoutFollowsRedirects` posture used to (that variant carried no payload at
        /// all; nothing checked that any caller of it actually called `.timeout(...)`) — and the
        /// way the since-deleted `client_for` reopened one layer down, by handing back a bare
        /// client that skipped this extraction entirely for its one caller. `resolve` alone: a
        /// total deadline cuts off whatever body is still arriving, which only a call whose every
        /// failure degrades for free can afford. `missing_objects` and `upload_targets` rode this
        /// too until a late refusal showed the cost: a status line that arrived inside the deadline
        /// lost its typed body to it. They ride [`Self::HeadDeadline`] now.
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
        /// `fetch_info`/`fetch_signature`/`download_bundle` that tolerance is the whole point — a
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
        /// variant exists rather than `fetch_batch`'s `POST` simply carrying a total deadline: a
        /// *total* deadline over a response whose size the request does not bound would kill
        /// large healthy bundles, identically on every retry.
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
        /// **Its consumers all need a client that never auto-follows.** `fetch_batch`'s `POST` so
        /// that a `307`/`308` cannot re-`POST` its body at a URL presigned for `GET` only (see
        /// [`super::RemoteClient::fetch_batch`]'s own doc), `commit_lift`, `upload_signature` and
        /// `put_trust` because they are mutations, and `fetch_subtree` because the protocol lets
        /// a storage-backed head answer it with a `307` to a presigned bundle, which
        /// [`super::RemoteClient::follow_bundle_redirect`] follows by hand without the
        /// `Authorization` header an auto-follow would carry along. A read whose redirect is safe
        /// to follow with every header it sent rides [`Self::HeadDeadline`] instead.
        HeadDeadlineNoRedirect {
            head: std::time::Duration,
        },
        /// [`Self::HeadDeadlineNoRedirect`]'s two phases on the client that auto-follows a redirect
        /// and carries the same [`super::FETCH_OBJECT_READ_TIMEOUT`] silence budget
        /// ([`Self::BoundedObjectReads`]'s): `head` bounds connect, request transmission and the
        /// wait for the header section, and the silence budget bounds the body after it. Everything
        /// that variant's doc says about the order of the two, the wording each expiry earns, and
        /// what the pair does not promise holds here unchanged.
        ///
        /// For the retryable reads, `missing_objects` and `upload_targets`: read-only `POST`s that
        /// rode an auto-following client before this variant existed, and whose redirect, replayed
        /// with its body, asks the same question again. reqwest drops the `Authorization` header on
        /// a cross-origin hop. **Never select this for a mutation**, for the reason
        /// [`Self::BoundedReads`]'s doc gives.
        ///
        /// The silence budget alone leaves a body that keeps trickling unbounded. Both calls'
        /// answers are capped by the request, so each reads its body through
        /// [`super::RemoteClient::read_presence_json`], whose total is sized by that cap.
        HeadDeadline {
            head: std::time::Duration,
        },
        /// Never auto-follows a redirect; no client-level `read_timeout`; **nothing else bounds
        /// this call's response wait either** — no per-request override, no watchdog. Deliberate,
        /// not an oversight: the ticket that owns adding a bound is a required part of this
        /// variant, not a comment beside it, so a reviewer sees it in any diff that constructs
        /// one. Every currently-unbounded call is a mutation, and a mutation must never silently
        /// follow a `3xx` (see [`super::RemoteClient::describe_mutation_redirect`]'s doc), so no
        /// auto-following unbounded twin exists; the last read that needed one, `fetch_subtree`,
        /// moved onto [`Self::HeadDeadlineNoRedirect`].
        ///
        /// The payload is never pattern-matched out — [`Clients::pick`] routes on the variant
        /// alone, not on which ticket it carries — so it is `#[allow(dead_code)]` rather than
        /// read. That is the point: this field's whole job is to be *present in the source*, not
        /// consumed by anything at runtime.
        #[allow(dead_code)]
        UnboundedNoRedirect(UnboundedTicket),
    }

    /// The ticket that owns bounding a call's still-unbounded response wait — a required payload
    /// of the one [`Posture`] variant that has one ([`Posture::UnboundedNoRedirect`]), not an
    /// optional annotation. A closed set, not a free
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
    /// of that variant — exactly the variant whose response wait is unbounded, and it requires
    /// this payload. What makes that exhaustive is *not* merely that [`Clients::pick`]'s own match over
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
    /// `UnboundedNoRedirect`, requiring this ticket by construction.
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
    /// a real budget and moves off [`Posture::UnboundedNoRedirect`] onto a bounded posture
    /// instead. When the last variant is removed, delete this enum and that `Posture` variant in
    /// the same change — it exists only to carry this payload and means nothing without it. The
    /// auto-following unbounded variant went that way when its last call, `fetch_subtree`, gained
    /// a head-wait bound.
    /// [`Posture::HeadDeadlineNoRedirect`] is the worked example of the other exit: it dropped its
    /// ticket and kept its bounds, because it stopped being unbounded rather than being deleted.
    /// Until then,
    /// whatever variants remain are a live gap awaiting a budget, not settled design to leave
    /// standing.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum UnboundedTicket {
        /// `update_ref`: its server-side cost is a *derived* quantity (an audit walk over the
        /// pushed history segment), not one the request enumerates, so no budget the client can
        /// size from its own request prices it. FORK-94. The calls FORK-92 collected (the presence
        /// negotiations, both `fetch_batch` stations and `fetch_subtree`) have all left this set
        /// for bounded postures.
        Fork94,
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
        /// [`Clients::send_on`] does for [`Posture::TotalDeadline`], the one posture that selects
        /// it. No mutation
        /// rides it: auto-following would expose one to the silent-3xx-success hole FORK-89
        /// closed.
        http: reqwest::Client,
        /// Same endpoint as [`Self::http`], automatic redirect-following disabled
        /// (`redirect::Policy::none()`); no `read_timeout` either. Selected by
        /// [`Posture::UnboundedNoRedirect`], and directly (bypassing [`Posture`] entirely) by
        /// [`Clients::send_with_watchdog`] — the watchdog-guarded upload paths: reqwest's default
        /// policy replays a `307`/`308` with the original method *and* body, and `tower-http`'s
        /// `SEE_OTHER`/`MOVED_PERMANENTLY`/`FOUND` arms force a mutation's method to `GET` and
        /// body empty — either way a redirect must come back raw rather than being auto-followed
        /// (FORK-89; see [`super::RemoteClient::describe_mutation_redirect`]'s doc). Every
        /// mutation rides this client.
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
        /// `fetch_batch`'s redirect-follow `GET`) needs. Selected by
        /// [`Posture::BoundedObjectReads`], and by [`Posture::HeadDeadline`], whose head-wait
        /// timer fires first and leaves this budget the body alone.
        bounded_object_reads: reqwest::Client,
        /// The two axes no other client combines: automatic redirect-following disabled
        /// (`redirect::Policy::none()`, exactly as [`Self::no_redirect`] has it) **plus** a
        /// `read_timeout` of
        /// [`bounded_read_timeout`]`(connect_timeout, `[`FETCH_OBJECT_READ_TIMEOUT`]`)` (exactly as
        /// [`Self::bounded_object_reads`] has it). Selected only by
        /// [`Posture::HeadDeadlineNoRedirect`], whose consumers need both at once: `fetch_batch`'s
        /// `POST` must not let a `307`/`308` replay its body at a `GET`-only presigned URL, and its
        /// response body must not be able to stall forever; `commit_lift` is a mutation, so it
        /// never follows a redirect, and its retry deadline needs the head-wait bounded;
        /// `upload_signature` and `put_trust` are mutations bounded by the retryable floor;
        /// `fetch_subtree` has `fetch_batch`'s cost shape and follows its redirect by hand.
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
        /// Build every client from one shared budgets/proxy configuration. See
        /// [`super::RemoteClient::build`]'s own doc for why one set of budgets must reach every
        /// client rather than being decided by one path and overwritten on a field by another.
        pub(super) fn build(budgets: &super::Budgets,
                            proxy: Option<&reqwest::Proxy>) -> Result<Clients, String> {
            let connect_timeout = budgets.connect;
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
                .read_timeout(bounded_read_timeout(connect_timeout, budgets.read));
            let mut bounded_object_reads = reqwest::Client::builder()
                .connect_timeout(connect_timeout)
                .read_timeout(bounded_read_timeout(connect_timeout, budgets.object_read));
            // Both axes at once, which no other client here has: `no_redirect`'s policy and
            // `bounded_object_reads`' budget. Deliberately not expressed as a tweak to either of
            // those — see this field's own doc for why neither could take the other's axis.
            let mut bounded_object_reads_no_redirect = reqwest::Client::builder()
                .connect_timeout(connect_timeout)
                .redirect(reqwest::redirect::Policy::none())
                .read_timeout(bounded_read_timeout(connect_timeout, budgets.object_read));

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
                Posture::HeadDeadline { .. } => &self.bounded_object_reads,
                Posture::HeadDeadlineNoRedirect { .. } => &self.bounded_object_reads_no_redirect,
                Posture::UnboundedNoRedirect(_) => &self.no_redirect,
            }
        }

        /// The only way a request reaches any client: send it on whichever one
        /// [`Self::pick`] chooses, with `posture`'s own payload — currently
        /// [`Posture::TotalDeadline`]'s `Duration`, or [`Posture::HeadDeadline`]'s or
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
        ///
        /// `headers` are the request's conditions (a bundle download's `Range` and `If-Range`),
        /// added before the body: never a `Content-Type`, which only [`SendBody`] decides.
        pub(super) async fn send_on(&self,
                                    posture: Posture,
                                    budgets: &super::Budgets,
                                    method: reqwest::Method,
                                    destination: RequestDestination<'_>,
                                    body: SendBody,
                                    headers: reqwest::header::HeaderMap) -> SendOutcome {
            let connect_timeout = budgets.connect;
            let (total_deadline, head_deadline) = match &posture {
                Posture::TotalDeadline(duration) => {
                    check_total_deadline_payload(*duration, connect_timeout);
                    (Some(*duration), None)
                }
                // The head deadline is *clamped*, not merely asserted, and the two are a pair:
                // the check fails loudly in a debug build, the clamp keeps the released binary
                // out of the state where a `HeadWaitExpired` message would claim a connection
                // that was never established. See `clamp_head_deadline_payload`'s own doc. Both
                // head-deadline variants ride a client with the same `object_read` silence budget,
                // so one check serves both.
                Posture::HeadDeadline { head } | Posture::HeadDeadlineNoRedirect { head } => {
                    check_head_deadline_payload(*head, connect_timeout, budgets.object_read);
                    (None, Some(clamp_head_deadline_payload(*head, connect_timeout)))
                }
                // Listed explicitly, not `_`, so a future payload-carrying `Posture` variant
                // forces this match to be revisited too instead of silently falling through to
                // `None` and dropping the new payload.
                Posture::BoundedReads
                | Posture::BoundedObjectReads
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

            let send_fut = body.apply(builder.headers(headers)).send();

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
        /// **The two budgets.** Every poll compares the time since the stream last yielded a chunk
        /// against one of two bounds, chosen by whether the stream has reported itself exhausted:
        ///
        /// - `phase1_budget` (`connect + upload_silence`) while the body is still streaming. The
        ///   silence clock starts when [`super::UploadProgress`] is built, before `send()` runs, so
        ///   the connect allowance is added in; past that it resets on every chunk hyper pulls
        ///   (see [`super::UPLOAD_SILENCE_BUDGET`]).
        /// - `phase2_budget` (`phase1_budget + post_send_verify_budget(body_len)`) once every chunk
        ///   has been handed off — the wait for the response, which is the retryable class's
        ///   head-wait: the floor plus a size-scaled part for the bytes just sent (see
        ///   [`super::post_send_verify_budget`]). It folds the whole send-phase
        ///   allowance back in rather than using the verify budget alone, as a flush-time margin:
        ///   exhaustion means hyper has every chunk, not that the peer has every byte (see
        ///   [`super::UploadChunks`]), so the silence after it can include an in-flight tail still
        ///   draining through hyper's buffer and the kernel's send buffer. A verify-only phase 2
        ///   is too tight for exactly that tail.
        ///
        /// `upload_object_survives_a_remote_that_pauses_before_reading_a_small_body` pins that
        /// phase 2 stays wider than the verify budget alone, and
        /// `upload_object_times_out_after_a_fully_received_body_that_never_responds` that it still
        /// fires on a remote that receives the whole body and never answers.
        pub(super) async fn send_with_watchdog(&self,
                                               budgets: &super::Budgets,
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

            let phase1_budget = budgets.connect + budgets.upload_silence;
            let phase2_budget = phase1_budget
                + super::post_send_verify_budget(budgets.retryable_head_patience, body_len);
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
    /// What the check is for: the one producer of a `TotalDeadline` payload,
    /// `RemoteClient::resolve_budget`, builds it as `connect_timeout + a positive addend` — never
    /// merely a post-connect budget on its own — so the connector's own timeout always fires first
    /// on a connect-phase stall, classifying it as `TransportFailure::ConnectTimedOut` rather than
    /// `ReadTimedOut`. It satisfies this by construction, so it cannot fire against any call site
    /// this module actually has; it exists to catch the next producer that doesn't.
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
    /// test's own doc for why no producer could ever reach the failing branch, and
    /// [`super::RemoteClient::describe_transport_error`]'s doc for the wording hazard a violation
    /// would cause. **What that test pins is this function, not that the seam still calls it** —
    /// the link between the two is [`Clients::send_on`]'s single call above, and this function
    /// having no other caller. Check both before trusting the pairing:
    /// `grep -n 'check_total_deadline_payload' crates/forklift-core/src/util/remote_utils.rs`.
    pub(super) fn check_total_deadline_payload(duration: std::time::Duration,
                                               connect_timeout: std::time::Duration) {
        debug_assert!(
            duration > connect_timeout,
            "a Posture::TotalDeadline payload ({:?}) must \
            strictly exceed this request's connect_timeout ({:?}) — otherwise a \
            connect-phase stall lands in TransportFailure::ReadTimedOut instead of \
            ConnectTimedOut, and describe_transport_error reports that the remote did \
            not finish answering when no connection was ever established",
            duration, connect_timeout
        );
    }

    /// What [`clamp_head_deadline_payload`] raises a violating head-deadline payload by, above the
    /// `connect_timeout` that payload failed to clear. Deliberately small and deliberately
    /// arbitrary: no producer in this module can reach that branch (each builds `connect_timeout +
    /// a positive addend`), so this value is never armed against a real remote. Only its **strict
    /// positivity** is load-bearing — that is what keeps the connector's own timeout firing first
    /// on a connect-phase stall, and so keeps every [`SendOutcome::HeadWaitExpired`] describing a
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
    /// already arrived — when none had. Every producer satisfies both by construction: each is
    /// `connect_timeout` plus an addend a const assertion holds under
    /// [`super::FETCH_OBJECT_READ_TIMEOUT`] (see [`super::BATCH_HEAD_PATIENCE`]'s,
    /// [`super::RETRYABLE_HEAD_PATIENCE`]'s and [`super::COMMIT_ATTEMPT_ALLOWANCE`]'s). This exists
    /// to catch the next producer, which is why the upper fence is a `debug_assert!` and not a
    /// clamp: unlike the lower one, no repair value is obviously right — a payload that big means
    /// the producer wanted something this posture does not offer.
    ///
    /// Falsified in both directions by
    /// [`super::tests::the_head_deadline_payload_check_catches_a_violating_payload`] and
    /// [`super::tests::the_head_deadline_payload_check_catches_a_payload_past_the_silence_budget`];
    /// see the first's doc for why its fixture uses a payload exactly *equal* to `connect_timeout`
    /// rather than a lesser one.
    pub(super) fn check_head_deadline_payload(head: std::time::Duration,
                                              connect_timeout: std::time::Duration,
                                              object_read: std::time::Duration) {
        debug_assert!(
            head > connect_timeout,
            "a head-deadline payload ({:?}) must strictly exceed \
            this request's connect_timeout ({:?}) — otherwise the external head-wait timer \
            fires during connect, and head_wait_expired_message tells the operator the \
            connection was established when none ever was",
            head, connect_timeout
        );
        debug_assert!(
            head < bounded_read_timeout(connect_timeout, object_read),
            "a head-deadline payload ({:?}) must stay strictly under \
            this posture's own client-level read_timeout ({:?}) — otherwise that read timeout \
            fires before the status line, and describe_transport_error tells the operator the \
            remote had already sent response headers when it had sent nothing",
            head, bounded_read_timeout(connect_timeout, object_read)
        );
    }

    /// The head-deadline payload repair [`Clients::send_on`] performs — the
    /// [`Posture::HeadDeadline`]/[`Posture::HeadDeadlineNoRedirect`] counterpart of
    /// [`check_total_deadline_payload`], and deliberately **not** the same shape.
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
    #[derive(Clone)]
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
        /// The external head-wait timer a head-deadline posture arms fired: the
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
    /// The budgets this instance's clients were actually built with — [`Budgets::production`]
    /// for whichever routing `should_route_through_tor` selected at construction, outside tests.
    /// Every arm site and every message reads them from here, so a transport failure is reported
    /// against the bound that actually applied rather than a hardcoded guess (see
    /// `describe_transport_error`).
    budgets: Budgets,
    base: String,
    token: Option<String>,
    /// Set once the batch endpoint has shown it cannot serve this remote: it answered `404`, or it
    /// sent no status line inside [`Budgets::batch_head_patience`]. From then on
    /// [`fetch_missing_objects`] goes straight to loose GETs instead of paying the head wait again
    /// for every later wave of the same command. Shared by every clone, since a command's walk
    /// hands clones to its workers; never reopened, since nothing this instance sees afterwards
    /// says the endpoint got any faster. It keeps the reason the lane first closed for, which
    /// the command reads back through [`Self::batch_lane_closure`] to tell the user why it ran
    /// slow.
    batch_lane_closed: std::sync::Arc<std::sync::OnceLock<BatchLaneClosure>>,
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

/// What a transport failure that is a dropped connection says after its action: a refused
/// connect, a reset, a body cut off. Its own words, so [`is_transient_failure`] can tell it from a
/// local `Error while …` and from a transport failure that repeats (DNS, TLS, a bad body).
const CONNECTION_FAILED: &str = "the connection to the remote failed: ";

/// Whether `message` reports a failure retrying may get past: a timeout, a connection that failed
/// or a server error (`5xx`) as this module composes them, anywhere in the message (a caller may
/// have wrapped it). Never a local failure, whatever its text quotes.
pub fn is_transient_failure(message: &str) -> bool {
    if message.contains("Timed out while ") || message.contains(&format!(": {}", CONNECTION_FAILED)) {
        return true;
    }

    // `classify_remote_error`'s uncoded shape: "The remote refused <action> (<status>): …".
    // Only the first status after the phrase is its own: a later one is the server's message.
    message.match_indices("The remote refused ").any(|(at, _)| {
        let tail = &message[at..];
        let status = tail.match_indices(" (").map(|(open, _)| &tail[open + 2..]).find(|status| {
            status.len() >= 6 && status.as_bytes()[..3].iter().all(u8::is_ascii_digit) && status[3..].starts_with("): ")
        });

        status.is_some_and(|status| status.starts_with('5'))
    })
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
        RemoteClient::new_with_tor(url, token, TorSettings::from_config()?)
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
        // A Tor dial's connect phase covers the whole SOCKS handshake and onion circuit build,
        // which can legitimately take tens of seconds — far past what a direct dial should ever
        // need (see `REMOTE_CONNECT_TIMEOUT_TOR`'s doc).
        Self::with_budgets_for(url, token, &tor, Budgets::production)
    }

    /// Decide the routing and build the client with the budgets `budgets_for` gives for it. The
    /// one path [`Self::new_with_tor`] and the test fixtures share, so a test that injects its
    /// own budgets still goes through the production routing decision.
    fn with_budgets_for(url: &str,
                        token: Option<String>,
                        tor: &TorSettings,
                        budgets_for: fn(bool) -> Budgets) -> Result<RemoteClient, String> {
        let routes_through_tor = should_route_through_tor(&tor.mode, url);
        Self::build(url, token, tor, routes_through_tor, budgets_for(routes_through_tor))
    }

    /// Build the underlying clients and assemble a [`RemoteClient`], given budgets already decided
    /// by the caller — [`Self::with_budgets_for`] is the only production caller, and decides them
    /// from [`should_route_through_tor`]. Split out so the test constructors can supply arbitrary
    /// [`Budgets`] that actually get built into every client, rather than one construction path
    /// deciding the budgets and a second one overwriting only the `budgets` field afterward — the
    /// latter would leave the inner clients armed with a *different* bound than the field reports,
    /// which is not a client any production constructor could ever produce and would silently
    /// invalidate any test built on top of it that touches `describe_transport_error` or
    /// `send_with_watchdog`, both of which trust this field to describe what the clients actually
    /// carry.
    fn build(url: &str,
             token: Option<String>,
             tor: &TorSettings,
             routes_through_tor: bool,
             budgets: Budgets) -> Result<RemoteClient, String> {
        let proxy = if routes_through_tor {
            Some(reqwest::Proxy::all(&tor.proxy).map_err(|e| format!(
                "Error while configuring the Tor proxy \"{}\": {}", tor.proxy, e
            ))?)
        } else {
            None
        };

        let clients = Clients::build(&budgets, proxy.as_ref())?;

        Ok(RemoteClient {
            clients,
            budgets,
            base: url.trim_end_matches('/').to_string(),
            token,
            batch_lane_closed: Default::default(),
        })
    }

    /// Test-only: build a client for a direct dial whose every budget is the caller's — the seam
    /// that lets a test watch a budget fire in milliseconds instead of waiting out the production
    /// figure, and inject a budget no production constructor can emit, so an accessor that reads
    /// a production constant in place of the instance's field cannot match it by coincidence.
    #[cfg(test)]
    fn new_test_with_budgets(url: &str, budgets: Budgets) -> RemoteClient {
        RemoteClient::build(url, None, &TorSettings::default(), false, budgets.checked())
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
                \"forklift config set {} <url>\".",
                config_utils::KEY_REMOTE_URL
            ))?;

        let token = config_utils::get_effective_value(config_utils::KEY_REMOTE_TOKEN)?
            .map(|(value, _)| value);

        RemoteClient::new(&url, token)
    }

    /// Create the client every command of a warehouse talks to its remote through: the configured
    /// remote, and in a partial warehouse only when it is the recorded origin
    /// ([`origin_utils::checked_remote`]). [`from_config`](RemoteClient::from_config) stays only
    /// where no partial warehouse dials through it: `mirror`, which refuses a partial warehouse
    /// before it builds a client, and the best-effort display-name lookup, which reads no history
    /// and no heads.
    ///
    /// # Returns
    /// * `Ok(RemoteClient)` - The client.
    /// * `Err(CoreError)`   - No remote is configured, a partial warehouse's remote is not its
    ///   origin or it has none (`non_origin_remote`), or the client could not be built.
    pub fn for_warehouse() -> Result<RemoteClient, CoreError> {
        let (url, token) = origin_utils::checked_remote()?;

        Ok(RemoteClient::new(&url, token)?)
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
    /// the second as the first sends a user toward heavier remedies — clone, reproduce, or
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
    /// [`Self::follow_bundle_redirect`]'s `GET` to another origin, an absolute presigned URL that
    /// must *not* carry this remote's token — goes through [`Self::send_on_presigned`] instead;
    /// see that function's own doc.
    async fn send_on(&self,
                     posture: Posture,
                     method: reqwest::Method,
                     path: &str,
                     body: SendBody) -> SendOutcome {
        self.send_on_with(posture, method, path, body, reqwest::header::HeaderMap::new()).await
    }

    /// [`Self::send_on`] with request `headers` (see [`clients::Clients::send_on`]).
    async fn send_on_with(&self,
                          posture: Posture,
                          method: reqwest::Method,
                          path: &str,
                          body: SendBody,
                          headers: reqwest::header::HeaderMap) -> SendOutcome {
        self.clients.send_on(
            posture, &self.budgets, method,
            RequestDestination::Authenticated { base: &self.base, token: self.token.as_deref(), path },
            body, headers,
        ).await
    }

    /// [`Self::send_on`] to an absolute `url` on this remote's own origin (a same-origin redirect
    /// target), with the bearer token, and with no body. The base is the URL's origin, not
    /// [`Self::base`], since the target's path is already complete.
    async fn send_on_own_origin(&self,
                                posture: Posture,
                                method: reqwest::Method,
                                url: &reqwest::Url) -> SendOutcome {
        let origin = url.origin().ascii_serialization();
        let path = origin_relative_path(url);
        self.clients.send_on(
            posture, &self.budgets, method,
            RequestDestination::Authenticated {
                base: &origin, token: self.token.as_deref(), path: &path
            },
            SendBody::Empty, reqwest::header::HeaderMap::new(),
        ).await
    }

    /// The presigned counterpart of [`Self::send_on`] — same posture, same payload guarantee,
    /// but sending to an absolute, self-authorizing URL with **no** bearer token attached. Down
    /// to one live caller — [`Self::follow_bundle_redirect`]'s cross-origin `GET` (a presigned
    /// storage URL is self-authorizing; forwarding a bearer token meant for the control plane
    /// would be a needless credential leak) — since [`Self::put_presigned`], this shape's other historical
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
            posture, &self.budgets, method, RequestDestination::Presigned { url }, body,
            reqwest::header::HeaderMap::new(),
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
    /// [`CONNECTION_FAILED`] when `e` is a connection that was refused, reset or cut off before
    /// or during the answer, which retrying unchanged may get past; nothing for any other
    /// transport failure (DNS, TLS, a body that cannot be decoded, a redirect loop), which
    /// retrying unchanged repeats.
    fn connection_failed(e: &reqwest::Error) -> &'static str {
        use std::io::ErrorKind;

        let mut current: Option<&(dyn std::error::Error + 'static)> = Some(e);
        let mut dropped = e.is_body() && !e.is_decode();

        while let Some(error) = current {
            if let Some(io) = error.downcast_ref::<std::io::Error>() {
                dropped |= matches!(io.kind(), ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe | ErrorKind::UnexpectedEof
                    | ErrorKind::NotConnected);
            }

            // A remote that closes before its answer is complete (a restart, a dropped connection
            // before the status line) fails as hyper's incomplete message, with no `io::Error`
            // beneath it.
            if let Some(hyper) = error.downcast_ref::<hyper::Error>() {
                dropped |= hyper.is_incomplete_message();
            }

            current = error.source();
        }

        if dropped { CONNECTION_FAILED } else { "" }
    }

    /// Whether `e` failed in the TLS handshake (a certificate the client rejected, a peer that
    /// does not speak TLS): rustls's own error somewhere in the chain. It arrives wrapped in
    /// `io::Error`s, whose `source()` skips the error they wrap, so the walk steps into each.
    fn tls_rejected(e: &reqwest::Error) -> bool {
        let mut current: Option<&(dyn std::error::Error + 'static)> = Some(e);

        while let Some(error) = current {
            if error.is::<rustls::Error>() {
                return true;
            }

            current = match error.downcast_ref::<std::io::Error>().and_then(|io| io.get_ref()) {
                Some(wrapped) => Some(wrapped),
                None => error.source(),
            };
        }

        false
    }

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
    /// [`Posture::TotalDeadline`] instead reports its own payload verbatim, with no "at least" — a
    /// non-resetting `RequestBuilder::timeout` is a genuine upper bound the call could never have
    /// exceeded, so the exact figure is honest. (The `debug_assert!` in
    /// [`clients::check_total_deadline_payload`], which [`clients::Clients::send_on`]'s
    /// `TotalDeadline` arm calls, guards this arm's *wording*, not its figure — see that function's
    /// own doc for why the number stays correct either way.) [`Posture::UnboundedNoRedirect`] names
    /// no figure at all — nothing was armed, so [`TransportFailure::ReadTimedOut`]'s own doc's
    /// ambiguity (a client `read_timeout` vs. a genuine multi-minute kernel `ETIMEDOUT`) applies at
    /// full force here, the one case this function cannot narrow at all.
    ///
    /// **The head-deadline arm, shared by [`Posture::HeadDeadline`] and
    /// [`Posture::HeadDeadlineNoRedirect`], has to be distinguishable by its words, because its
    /// figure is already taken.** It reports the same lower bound
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
                action, self.budgets.connect
            ),
            TransportFailure::ReadTimedOut => match posture {
                Posture::BoundedReads => format!(
                    "Timed out while {}: the remote did not respond within at least {:?}.",
                    action, bounded_read_timeout(self.budgets.connect, self.budgets.read)
                ),
                Posture::BoundedObjectReads => format!(
                    "Timed out while {}: the remote did not respond within at least {:?}.",
                    action, bounded_read_timeout(self.budgets.connect, self.budgets.object_read)
                ),
                Posture::TotalDeadline(deadline) => format!(
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
                Posture::HeadDeadline { .. } | Posture::HeadDeadlineNoRedirect { .. } => format!(
                    "Timed out while {}: the remote sent response headers and then stopped \
                    sending, with no further bytes for at least {:?}. That budget is on silence \
                    between bytes, not on how long the whole transfer may take.",
                    action, bounded_read_timeout(self.budgets.connect, self.budgets.object_read)
                ),
                Posture::UnboundedNoRedirect(_) => format!(
                    "Timed out while {}: the remote did not respond.",
                    action
                ),
            },
            TransportFailure::Other => format!("Error while {}: {}{}", action, Self::connection_failed(&e), Self::root_cause(&e)),
        }
    }

    /// The mutation counterpart of [`Self::describe_transport_error`], reached by five mutations
    /// (`update_ref`, `upload_object`, `put_presigned`, `upload_signature`, `put_trust`), none of
    /// which auto-follows a redirect, and by `commit_lift` for the transport failures it does not
    /// retry ([`Self::unanswered_commit`]). Takes no `Posture` because its wording does not vary by
    /// one: unlike the read path, a mutation's `ReadTimedOut` is always the same uncertain shape
    /// regardless of which bound (if any) actually fired, so every one of them funnels through the
    /// identical composer. `update_ref` rides [`Posture::UnboundedNoRedirect`] (moved off the
    /// auto-following client by FORK-89); `upload_signature`/`put_trust` ride
    /// [`Posture::HeadDeadlineNoRedirect`] — bounded, on a never-auto-follow client, so their
    /// transport failures land here the same way, and their head-wait expiries reach the same
    /// uncertainty wording through [`Self::response_from_send_mutation`];
    /// `upload_object`/`put_presigned` ride [`clients::Clients::send_with_watchdog`] directly, on
    /// the same never-auto-follow client, but bypassing [`Posture`] entirely (moved by the earlier
    /// fix for the `303` redirect hole, then off `Posture` again once a payload-free posture proved
    /// unable to guarantee the watchdog was actually applied). `upload_object`/`put_presigned`'s
    /// watchdog also produces a genuine `reqwest::Error` for an actual transport failure (as
    /// opposed to a watchdog kill, which produces no `reqwest::Error` at all — see
    /// [`clients::WatchdogOutcome::Transport`]'s doc), so a transport failure on any of them still
    /// lands here for anything `classify` can actually see (a connect failure, or a
    /// `reqwest::Error`-bearing timeout on the response side — a fired client-level `read_timeout`
    /// is exactly such a `reqwest::Error`, `is_timeout()` true, same as any other). Same
    /// [`classify`] dispatch, but the [`TransportFailure::ReadTimedOut`] wording differs from the
    /// read path's: on these clients that case can only be a timeout on an
    /// *established* connection — after the request bytes were already sent — so the settled
    /// contract requires the uncertainty be carried in the message rather than asserted away: it
    /// may have completed on the remote, and the caller must decide whether to check before
    /// retrying, never be told nothing happened.
    fn describe_mutation_transport_error(&self, action: &str, e: reqwest::Error) -> String {
        match classify(e.is_connect(), e.is_timeout()) {
            TransportFailure::ConnectTimedOut => format!(
                "Timed out while {}: could not connect to the remote within {:?}. Nothing was \
                sent — safe to retry.",
                action, self.budgets.connect
            ),
            TransportFailure::ReadTimedOut => Self::mutation_read_timeout_message(action),
            TransportFailure::Other => format!("Error while {}: {}{}", action, Self::connection_failed(&e), Self::root_cause(&e)),
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
    /// "may or may not have fully reached", not "was sent": `silent_for()`
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
    /// head-wait timer a head-deadline posture arms fired before a complete status line
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
    /// "the client finished streaming the request body", not "the request
    /// finished sending": exhaustion means hyper has every chunk, not that the peer has received
    /// every byte — hyper's own buffer, and the OS kernel's send buffer beneath it, can still hold
    /// an unflushed tail at the instant this fires (see [`clients::Clients::send_with_watchdog`]'s
    /// doc) — so this states only the locally-observable fact, not a claim about what the
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
    /// itself (see that method's own doc for the `phase1_budget`/`phase2_budget` reasoning) —
    /// into this call's own operator-facing error. The four outcomes
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
    /// shares it. `upload_signature` and `put_trust` reach it; it is the arm that must not be
    /// "simplified" into the read wording.
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
    /// client ([`Posture::UnboundedNoRedirect`] for `update_ref`,
    /// [`Posture::HeadDeadlineNoRedirect`] for `upload_signature`/`put_trust`, or — for
    /// `upload_object`/`put_presigned` — directly via [`clients::Clients::send_with_watchdog`]),
    /// which never auto-follows *any* redirect status — so this is a local, unconditional invariant
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
    /// instance's own `connect_timeout` into [`error_body_read_budget`]'s arithmetic. Split out as
    /// its own `&self` method, rather than inlining the call to the free function at its call
    /// site (`error_of`, which `commit_lift` reaches for its terminal refusals), so a test can pin
    /// that the right instance field is read without waiting a budget out:
    /// `error_body_budget_reads_this_field_not_a_rival_constant` reads it from an instance whose
    /// `connect_timeout` no production constructor could have
    /// produced, so a hardcoded direct (5s) or Tor (60s) connect bound cannot coincide with it. The
    /// timed tests of both call sites catch the direct hardcode as well, by elapsed time.
    fn error_body_budget(&self) -> std::time::Duration {
        error_body_read_budget(&self.budgets)
    }

    /// Turn a non-success response into the client-facing error, threading the server's refusal
    /// code (§7.4) through the taxonomy when the body carries one. The body read is bounded by
    /// [`Self::error_body_budget`] (so this instance's own `connect_timeout` is folded in) — see
    /// that method's and [`error_body_read_budget`]'s docs for why an error-body read needs its
    /// own bound rather than inheriting one from whichever client sent the request, and why the
    /// flat [`ERROR_BODY_READ_TIMEOUT`] alone is not it.
    async fn error_of(&self, response: reqwest::Response, action: &str) -> String {
        let status = response.status();
        let body = read_error_body(response, self.error_body_budget()).await;

        classify_remote_error(status.as_u16(), action, body.error, body.code, body.next_step)
    }

    /// Read a whole response body within `cap`, composing each failure the way this client
    /// reports it: a transport failure through [`Self::describe_transport_error`] under `action`
    /// and `posture`, an over-cap body as a refusal naming `what`.
    async fn read_body(&self,
                       response: reqwest::Response,
                       cap: u64,
                       action: &str,
                       posture: Posture,
                       what: &str) -> Result<Vec<u8>, String> {
        body_utils::read_body_capped(response, cap).await.map_err(|e| match e {
            BodyReadError::Transport(e) => self.describe_transport_error(action, posture, e),
            BodyReadError::OverCap { cap } => body_utils::over_cap_message(&format!("the remote's {}", what), cap),
        })
    }

    /// [`Self::read_body`] for a JSON body: a body that arrives whole but does not parse is
    /// reported as the remote's `what` not being valid JSON.
    async fn read_json<T: serde::de::DeserializeOwned>(&self,
                                                       response: reqwest::Response,
                                                       cap: u64,
                                                       action: &str,
                                                       posture: Posture,
                                                       what: &str) -> Result<T, String> {
        body_utils::read_json_capped(response, cap).await.map_err(|e| match e {
            JsonReadError::Body(BodyReadError::Transport(e)) => self.describe_transport_error(action, posture, e),
            JsonReadError::Body(BodyReadError::OverCap { cap }) => {
                body_utils::over_cap_message(&format!("the remote's {}", what), cap)
            }
            JsonReadError::Parse(e) => format!("The remote's {} is not valid JSON: {}", what, e),
        })
    }

    /// [`Self::read_json`] for a presence answer, under a total counted from the status line:
    /// [`Self::presence_body_budget`] for `cap`, a body whose size the request bounds. The client's
    /// silence budget alone resets on every byte, so a remote trickling a capped body could hold
    /// the call for as long as it likes; this is the bound on how long the whole answer may take
    /// once its headers are in. Derived from `cap` here rather than passed in, so the total cannot
    /// drift from the cap; a body read at another rate or floor needs its own reader.
    async fn read_presence_json<T: serde::de::DeserializeOwned>(&self,
                                                                response: reqwest::Response,
                                                                cap: u64,
                                                                action: &str,
                                                                posture: Posture,
                                                                what: &str) -> Result<T, String> {
        let budget = self.presence_body_budget(cap);
        tokio::time::timeout(budget, self.read_json(response, cap, action, posture, what)).await
            .unwrap_or_else(|_| Err(Self::body_deadline_message(action, budget)))
    }

    /// The message for a body that [`Self::read_presence_json`]'s budget cut off: the headers came,
    /// the whole answer did not. No "at least": the budget is a total, so the figure is exact.
    fn body_deadline_message(action: &str, budget: std::time::Duration) -> String {
        format!(
            "Timed out while {}: the remote sent response headers but did not finish its answer \
            within {:?} of them.",
            action, budget
        )
    }

    /// The total budget for a presence answer's body of at most `cap` bytes, counted from its
    /// status line: this client's silence budget (`connect + object_read`, the gap the body's
    /// transport already tolerates) plus `cap` at [`PRESENCE_BODY_RATE_BYTES_PER_SEC`]. A budget of
    /// its own, not the head-wait's: the head-wait waits on server work, while what remains once
    /// the headers are in is a transfer whose size the request bounds.
    ///
    /// Floored at the silence budget, and strictly above it (a cap is never zero), so a remote
    /// that goes quiet straight after its headers is reported as silence, and this figure is met
    /// only by one that keeps sending more slowly than the rate.
    fn presence_body_budget(&self, cap: u64) -> std::time::Duration {
        bounded_read_timeout(self.budgets.connect, self.budgets.object_read)
            + std::time::Duration::from_secs_f64(cap as f64 / PRESENCE_BODY_RATE_BYTES_PER_SEC as f64)
    }

    /// Fetch the warehouse handshake and check the protocol version.
    pub async fn fetch_info(&self) -> Result<WarehouseInfo, String> {
        let posture = Posture::BoundedReads;
        let outcome = self.send_waiting_out_throttle(
            posture, reqwest::Method::GET, "/v1/warehouse", SendBody::Empty, "the handshake"
        ).await?;
        let response = self.response_from_send(
            &format!("reaching the remote {}", self.base), posture, outcome
        )?;

        if !response.status().is_success() {
            return Err(self.error_of(response, "the handshake").await);
        }

        let info: WarehouseInfo = self.read_json(
            response, HANDSHAKE_BODY_CAP, &format!("reaching the remote {}", self.base), posture,
            "handshake",
        ).await?;

        if info.protocol != PROTOCOL_VERSION {
            return Err(format!(
                "The remote speaks protocol version \"{}\", this build speaks \"{}\". \
                Update the older side.",
                info.protocol, PROTOCOL_VERSION
            ));
        }

        if let Some(trust) = &info.trust {
            trust.validate().map_err(|problem| String::from(trust_utils::malformed(self.url(), &problem)))?;
        }

        // A head is written into a local ref as given, and only fetched (and so verified) when
        // it is missing. A non-canonical spelling of a held head — uppercase finds the same file
        // on a case-insensitive filesystem — would skip both and wedge the pallet. Neither the
        // name nor the value is repeated: both came from the remote. A tombstone's head is counted
        // as held and sent back as a ref's head, so it is held to the same spelling.
        let tombstone_heads = info.tombstones.iter().flat_map(|tombstones| tombstones.values());
        if info.pallets.values().chain(tombstone_heads).any(|head| !object_utils::is_object_hash(head)) {
            return Err(CoreError::refusal(
                RefusalCode::RemoteProtocolViolation,
                format!(
                    "The remote {} reports a pallet head that is not an object hash (64 lowercase \
                    hex characters). Refusing to sync with it.",
                    self.base
                ),
                "Report it to whoever runs the remote; retrying gets the same answer.",
            ).into());
        }

        Ok(info)
    }

    /// The head-wait budget to arm for *this* client instance's call to [`Self::missing_objects`]
    /// or [`Self::upload_targets`], for one chunk of `n` hashes: this instance's own
    /// `connect_timeout` plus the larger of the retryable floor ([`RETRYABLE_HEAD_PATIENCE`]) and
    /// the chunk's scaled presence cost (`n * PRESENCE_ALLOWANCE_MS_PER_OP`). The connect term is
    /// the same `self.budgets.connect` shape every sibling budget carries, so a Tor dial's 60s
    /// connect allowance is never undercut by a budget sized for a direct remote — see
    /// `presence_negotiation_budget_reads_this_field_not_a_rival_constant` in the test module.
    ///
    /// The larger of the two, not their sum: the floor is how long the client will wait for any
    /// retryable status line, and the scaled term lifts it for a batch big enough to need more. A
    /// sum would also break the fence every head-deadline payload keeps (strictly under
    /// `connect + object_read`) at `MAX_MISSING_BATCH`, where the scaled term alone is 50s; the
    /// const block under [`RETRYABLE_HEAD_PATIENCE`] pins that both wire caps' scaled terms stay
    /// under it. `connect + 30s` below 6,000 hashes, `connect + 50s` at the 10,000 cap.
    ///
    /// It bounds the wait for the status line only. A success body is read under
    /// [`Self::presence_body_budget`], sized by its byte cap, and a refusal's under
    /// [`Self::error_body_budget`] — so a refusal arriving late in the head-wait keeps its code.
    fn presence_negotiation_budget(&self, n: usize) -> std::time::Duration {
        let scaled = std::time::Duration::from_secs_f64(n as f64 * PRESENCE_ALLOWANCE_MS_PER_OP / 1000.0);
        self.retryable_head_budget().max(self.budgets.connect + scaled)
    }

    /// Ask which of the given objects the remote lacks (batched).
    ///
    /// Deliberately **not** on [`Posture::BoundedReads`]: the server side consults up to
    /// `MAX_MISSING_BATCH` (10,000) hashes before its first response byte, work that scales with
    /// the batch rather than being O(constant) — the settled contract puts that in the
    /// scaled/measured-budget category, not the flat one `REMOTE_READ_TIMEOUT` is honest for. Rides
    /// [`Posture::HeadDeadline`] instead, its head-wait bounded per batch by
    /// [`Self::presence_negotiation_budget`] and its answer's body by
    /// [`Self::presence_body_budget`]. Accepted residual: a remote that takes
    /// longer than either budget to answer — an overloaded or pathologically slow deployment — gets
    /// abandoned rather than waited out; this call's error is already a plain, retry-safe `Err`, so
    /// an ordinary retry is the recovery path, not a looser budget.
    pub async fn missing_objects(&self, hashes: &[String]) -> Result<Vec<String>, String> {
        let mut missing: Vec<String> = Vec::new();

        for batch in hashes.chunks(MAX_MISSING_BATCH) {
            let posture = Posture::HeadDeadline { head: self.presence_negotiation_budget(batch.len()) };
            let outcome = self.send_waiting_out_throttle(
                posture,
                reqwest::Method::POST, "/v1/objects/missing",
                SendBody::json(&MissingObjectsRequest { hashes: batch.to_vec() })?,
                "the presence check",
            ).await?;
            let response = self.response_from_send(
                "negotiating with the remote", posture, outcome
            )?;

            if !response.status().is_success() {
                return Err(self.error_of(response, "the negotiation").await);
            }

            let body: MissingObjectsResponse = self.read_presence_json(
                response, body_utils::per_item_body_cap(batch.len(), MISSING_BODY_BYTES_PER_HASH),
                "negotiating with the remote", posture, "negotiation response",
            ).await?;

            missing.extend(body.missing);
        }

        Ok(missing)
    }

    /// Ask whether `pallet` (its wire name) still holds each of `candidates`: whether its current
    /// head on the remote is the candidate or reaches it. Only the answers for candidates asked are
    /// returned, so a remote cannot add a hash of its own; a candidate it leaves out is absent.
    /// Asked [`MAX_HOLDS_CANDIDATES`] at a time, the most a head accepts in one request.
    ///
    /// The caller acts only on [`HoldAnswer::Held`]. An `Err` (a head too old to know the route
    /// answers `404`) confirms nothing either, so neither the error nor an answer it cannot use
    /// may free more than the current heads alone would.
    ///
    /// Rides [`Posture::HeadDeadline`], its head-wait sized by [`Self::retryable_head_budget`]: the
    /// head walks history before its first byte, for at most the 10s
    /// [`MAX_HOLDS_WALK_TIME`](crate::util::merge_utils::MAX_HOLDS_WALK_TIME) both heads share, well
    /// inside that budget. The answer's body is read under [`Self::presence_body_budget`].
    pub async fn holds(&self, pallet: &str, candidates: &[String]) -> Result<BTreeMap<String, HoldAnswer>, String> {
        let mut holds = BTreeMap::new();
        let action = format!("asking the remote what pallet \"{}\" holds", pallet);
        let route = holds_route(pallet).map_err(|error| format!("Error while {}: {}", action, error))?;

        for batch in candidates.chunks(MAX_HOLDS_CANDIDATES) {
            let posture = Posture::HeadDeadline { head: self.retryable_head_budget() };
            let outcome = self.send_waiting_out_throttle(
                posture,
                reqwest::Method::POST, &route,
                SendBody::json(&HoldsRequest { candidates: batch.to_vec() })?,
                "the holds check",
            ).await?;
            let response = self.response_from_send(&action, posture, outcome)?;

            if !response.status().is_success() {
                return Err(self.error_of(response, &format!("the holds check of pallet \"{}\"", pallet)).await);
            }

            let body: HoldsResponse = self.read_presence_json(
                response, body_utils::per_item_body_cap(batch.len(), HOLDS_BODY_BYTES_PER_CANDIDATE),
                &action, posture, "holds answer",
            ).await?;

            holds.extend(body.holds.into_iter().filter(|(candidate, _)| batch.contains(candidate)));
        }

        Ok(holds)
    }

    /// The total-deadline budget to arm for *this* client instance's call to [`Self::resolve`] —
    /// folds this instance's own `connect_timeout` into [`bounded_read_timeout`]'s arithmetic.
    /// Split out as its own `&self` method, rather than inlining the call to `bounded_read_timeout`
    /// at `resolve`'s one call site, so a test can pin that the right instance field is actually
    /// read: not just read from *some* client. [`Self::error_body_budget`] carries the identical
    /// shape (`self.budgets.connect` plus a fixed post-connect addend) for a sibling budget, and
    /// its own doc explains why that distinction needs a `connect_timeout` no production
    /// constructor can produce — the direct and Tor-mode constructors only ever emit
    /// [`REMOTE_CONNECT_TIMEOUT`] or [`REMOTE_CONNECT_TIMEOUT_TOR`], so a test built through either
    /// one can never separate "reads this field" from "hardcodes whichever of those two constants
    /// that fixture happens to carry." See `resolve_budget_reads_this_field_not_a_rival_constant`
    /// and `resolve_budget_is_70s_for_a_real_tor_mode_client` in the test module: the same two-test
    /// pair `error_body_budget` carries, and for the same reason — neither subsumes the other, see
    /// `error_body_budget_reads_this_field_not_a_rival_constant`'s own doc for why.
    fn resolve_budget(&self) -> std::time::Duration {
        bounded_read_timeout(self.budgets.connect, self.budgets.read)
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
    /// `forklift-server`'s hook client is built by `build_hook_client`, armed at a flat
    /// `HOOK_CLIENT_TIMEOUT` of 10s, and a single call can pay that twice: `post_resolve` first
    /// calls `check_auth`, which on a bearer-token auth-cache miss makes its own hook round trip
    /// via `authenticate_via_hook` — up to 10s — before `post_resolve` makes its *own*
    /// resolution-hook call through `post_hook` — up to another 10s. A 15s total deadline can
    /// therefore abandon a request a slow-hook deployment is still legitimately serving. Accepted
    /// deliberately: `resolve` is cosmetic display sugar and its fallback to pseudonyms is free
    /// (never a command failure), so giving up early is the right failure for this one call —
    /// unlike `fetch_info`/`fetch_signature`/`download_bundle`, whose `BoundedReads` silence
    /// budget never has to make this trade because a *silence* bound cannot mistake a
    /// slow-but-real transfer for a hang in the first place.
    ///
    /// Each distinct identifier is asked about once, in requests of at most
    /// [`MAX_RESOLVE_IDENTIFIERS`](crate::model::remote::MAX_RESOLVE_IDENTIFIERS) — the most a
    /// server accepts — and `RESOLVE_REQUEST_IDENTIFIER_BYTES`, so a large roster resolves in
    /// several requests rather than none. Up to `RESOLVE_REQUESTS_IN_FLIGHT` run at once, and
    /// each request's answer counts only for the identifiers it asked about, so the names shown
    /// never depend on which request finished first.
    ///
    /// The deadline above is per request, so the whole call can take one deadline per
    /// `RESOLVE_REQUESTS_IN_FLIGHT` requests against a remote that never answers. A roster that
    /// fits in that many requests — up to that many times `MAX_RESOLVE_IDENTIFIERS` short
    /// identifiers, though fewer long ones, since a request closes when the next identifier would
    /// not fit — is bounded by one deadline.
    pub async fn resolve(&self, identifiers: Vec<String>) -> BTreeMap<String, String> {
        let distinct: std::collections::BTreeSet<String> = identifiers.into_iter().collect();
        let mut batches: Vec<Vec<String>> = Vec::new();
        let mut batch_bytes = 0usize;

        for identifier in distinct {
            // As serialised, quotes and escapes included, plus the comma that separates it.
            let bytes = serde_json::to_string(&identifier).expect("a string serialises").len() + 1;
            let full = batches.last().is_none_or(|batch| {
                batch.len() >= crate::model::remote::MAX_RESOLVE_IDENTIFIERS
                    || batch_bytes + bytes > RESOLVE_REQUEST_IDENTIFIER_BYTES
            });

            if full {
                batches.push(Vec::new());
                batch_bytes = 0;
            }

            batch_bytes += bytes;
            batches.last_mut().expect("a batch was just ensured").push(identifier);
        }

        let mut requests = JoinSet::new();
        let in_flight = Arc::new(tokio::sync::Semaphore::new(RESOLVE_REQUESTS_IN_FLIGHT));

        for batch in batches {
            let client = self.clone();
            let in_flight = Arc::clone(&in_flight);

            requests.spawn(async move {
                let Ok(_permit) = in_flight.acquire().await else { return BTreeMap::new() };

                // The batch is sorted and distinct (it comes off a set), so it can be searched.
                client.resolve_batch(&batch).await.into_iter()
                    .filter(|(id, _)| batch.binary_search(id).is_ok()).collect()
            });
        }

        let mut names = BTreeMap::new();

        // A request that panicked resolves nothing, like any other failure here.
        while let Some(answer) = requests.join_next().await {
            names.extend(answer.unwrap_or_default());
        }

        names
    }

    /// One `POST /v1/resolve` of at most the server's identifier limit; see [`Self::resolve`].
    async fn resolve_batch(&self, identifiers: &[String]) -> BTreeMap<String, String> {
        let total_deadline = self.resolve_budget();
        // Every failure degrades to the designed fallback, including a serialization failure the
        // seam now surfaces up front rather than deferring into a `reqwest::Error` — see
        // `resolve`'s own doc for why an empty map is the right answer to all of them here.
        let cap = crate::model::remote::resolve_answer_cap(identifiers);
        let Ok(body) = SendBody::json(&ResolveRequest { identifiers: identifiers.into() }) else {
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

        match body_utils::read_json_capped::<ResolveResponse>(response, cap).await {
            Ok(body) => body.names,
            Err(_) => BTreeMap::new(),
        }
    }

    /// The head-wait budget to arm for *this* client instance's [`Self::fetch_batch`] `POST` —
    /// folds this instance's own `connect_timeout` into [`BATCH_HEAD_PATIENCE`], the same shape
    /// [`Self::error_body_budget`]/[`Self::resolve_budget`]/[`Self::presence_negotiation_budget`]/
    /// [`Self::retryable_head_budget`] already carry, for the identical reason: a Tor dial's 60s
    /// connect allowance must never be undercut by a budget sized for a direct remote, or the bound
    /// expires during circuit build on every call and every retry. 50s direct, 105s over Tor.
    ///
    /// Being `connect_timeout + a positive addend` is also what discharges the producer rule this
    /// posture's *wording* depends on — see [`clients::clamp_head_deadline_payload`]: a payload at
    /// or below `connect_timeout` would let the head timer fire during connect, and
    /// [`Self::head_wait_expired_message`] would then assert an established connection that never
    /// existed. Every producer of such a payload (this one, [`Self::presence_negotiation_budget`],
    /// [`Self::retryable_head_budget`], and `commit_lift`'s attempt bound) satisfies the rule by
    /// construction; the seam clamps anyway, because "by construction" is a property of today's
    /// producers rather than of the released binary.
    ///
    /// Its own `&self` method rather than inline arithmetic at `fetch_batch`'s call site, so a test
    /// can pin that the right instance field is actually read: not just *some* client's. See
    /// `batch_head_budget_reads_this_field_not_a_rival_constant` and
    /// `batch_head_budget_is_105s_for_a_real_tor_mode_client` in the test module — the same
    /// two-test pair each sibling budget carries, neither subsuming the other.
    fn batch_head_budget(&self) -> std::time::Duration {
        self.budgets.connect + self.budgets.batch_head_patience
    }

    /// Fetch many objects in one round trip as a bundle-format stream
    /// (`POST /v1/objects/batch`) and import them as the stream arrives (see
    /// [`Self::import_response`]). An answer the remote marked cut at its byte budget
    /// ([`BatchOutcome::Partial`]) leaves the lane open: what it sent is imported, and the caller
    /// asks again for the rest. Every other outcome but [`BatchOutcome::Imported`] closes this
    /// remote's batch lane (see `batch_lane_closed`), and the caller falls back to loose fetches
    /// for what is still missing:
    /// * [`BatchOutcome::EndpointAbsent`] — the remote predates the endpoint (a `404`).
    /// * [`BatchOutcome::OutOfTime`] — the remote marked its answer cut for running short of time
    ///   (see [`BATCH_CUT_DEADLINE`]). What it sent is imported. A store too slow to finish one
    ///   batch in the remote's time limit would cost that whole limit again on every later chunk,
    ///   and parallel loose GETs serve it better.
    /// * [`BatchOutcome::Degraded`] — no status line arrived inside [`Self::batch_head_budget`].
    ///   This is the one transport failure that degrades instead of failing the command, because it
    ///   is the one that says nothing about whether the remote works: the head is still building a
    ///   bundle it will not finish in time, and loose GETs price each object on its own. Every
    ///   other failure — a refused or reset connection, a stall after the status line, a `5xx` —
    ///   stays an error, since it says the remote or the link is broken, and loose GETs would only
    ///   meet the same thing once per object.
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
    /// hung the calling command forever, with no output and no way out short of killing the
    /// process. A head legitimately slower than [`BATCH_HEAD_PATIENCE`] is not waited out; it is
    /// abandoned for loose GETs, which is also what a wedged head gets: the two are
    /// indistinguishable from here, and the loose path either succeeds or fails on its own,
    /// per-object budgets. A wedge after the status line is a different case and stays an error.
    ///
    /// The redirect-follow `GET` below does not share that reasoning: an offloading store writes
    /// the bundle to its storage-backed response key and only *then* presigns and returns the
    /// `GET` URL (`crates/forklift-aws-lambda/src/aws/s3.rs`'s `offload_response`, called from
    /// `head.rs`'s `batch` once its bundle is finished) — so by the time this station
    /// is reached, the bytes it reads are already fully materialized at a known size, not still
    /// being assembled by whatever answers the request. That is the same shape
    /// [`Self::fetch_object`]'s own read is, not the size-unknown-in-advance one the `POST` above
    /// has, so it rides [`Posture::BoundedObjectReads`] — the same [`FETCH_OBJECT_READ_TIMEOUT`]
    /// silence budget, which resets on every byte received and so rides out a large-but-progressing
    /// bundle, only ever cutting off a genuinely stalled one. **The two stations reach the same
    /// figure by different arguments, and share one composer call under one action string**, which
    /// is why their [`Self::describe_transport_error`] arms have to be told apart by wording — see
    /// that function's doc.
    pub async fn fetch_batch(&self, hashes: &[String]) -> Result<BatchOutcome, String> {
        self.fetch_batch_as(hashes, BatchRead::Import).await
    }

    /// [`Self::fetch_batch`], handing each object the bundle carries to `deliver`, unverified,
    /// instead of storing it: for a caller that must write something before each object lands.
    /// An outcome's stats count the objects delivered as stored.
    pub async fn fetch_batch_delivering(&self,
                                        hashes: &[String],
                                        deliver: impl FnMut(String, Vec<u8>) -> Result<(), String> + Send + 'static)
                                        -> Result<BatchOutcome, String> {
        self.fetch_batch_as(hashes, BatchRead::Deliver(Box::new(deliver))).await
    }

    /// [`Self::fetch_batch`], reading the bundle as `read` says.
    async fn fetch_batch_as(&self, hashes: &[String], read: BatchRead) -> Result<BatchOutcome, String> {
        // Each hash is asked for once, and the response may carry nothing else (see
        // `StreamAdmission::Requested`).
        let mut requested: HashSet<String> = HashSet::new();
        let hashes: Vec<String> = hashes.iter()
            .filter(|hash| requested.insert((*hash).clone()))
            .cloned()
            .collect();

        let post_posture = Posture::HeadDeadlineNoRedirect { head: self.batch_head_budget() };
        let outcome = self.send_waiting_out_throttle(
            post_posture,
            reqwest::Method::POST, "/v1/objects/batch",
            SendBody::json(&MissingObjectsRequest { hashes })?,
            "the batch fetch",
        ).await?;
        if let SendOutcome::HeadWaitExpired { .. } = outcome {
            self.close_batch_lane(BatchLaneClosure::Degraded);
            return Ok(BatchOutcome::Degraded);
        }
        let response = self.response_from_send(
            "batch-fetching from the remote", post_posture, outcome
        )?;

        if endpoint_absent(response.status()) {
            self.close_batch_lane(BatchLaneClosure::EndpointAbsent);
            return Ok(BatchOutcome::EndpointAbsent);
        }

        // Read off the answer to the `POST` itself: a redirected answer carries it on the `303`,
        // not on the bundle behind it. A value this client does not know is no mark.
        let cut = response.headers().get(BATCH_HEADER).map(|value| value.as_bytes().to_vec());

        // `body_posture` follows whichever station actually produced the response bytes below:
        // the `POST` above when there was no redirect to follow — whose client-level silence
        // budget is what governs the phase these bytes are read in — or the redirect-follow
        // `GET`'s own silence budget when there was. The two budgets are the same size and reached
        // by different arguments, so a single posture would render the right figure with the wrong
        // provenance, and the composer's two arms would have nothing left to tell apart.
        let (response, body_posture) = self.follow_bundle_redirect(response, post_posture, "batch").await?;

        if !response.status().is_success() {
            return Err(self.error_of(response, "the batch fetch").await);
        }

        let action = "reading the batch response";
        let stats = match read {
            BatchRead::Import => {
                let admission = bundle_utils::StreamAdmission::Requested(requested);
                self.import_response(response, action, body_posture, admission).await?
            }
            BatchRead::Deliver(deliver) => bundle_utils::ImportStats {
                stored_objects: self.read_response(response, action, body_posture, move |body| {
                    bundle_utils::read_streamed_bundle(body, requested, deliver)
                }).await?,
                ..Default::default()
            },
        };

        match cut.as_deref() {
            Some(value) if value == BATCH_CUT_DEADLINE.as_bytes() => {
                self.close_batch_lane(BatchLaneClosure::OutOfTime);
                Ok(BatchOutcome::OutOfTime(stats))
            }
            Some(value) if value == BATCH_CUT_BUDGET.as_bytes() => Ok(BatchOutcome::Partial(stats)),
            _ => Ok(BatchOutcome::Imported(stats)),
        }
    }

    /// [`Self::send_on`], re-sending while the remote answers `429`: every route of the hosted API
    /// shares one throttle, so a read that meets it is expected to wait rather than fail the
    /// command. Every read rides this but [`Self::resolve`], which is best-effort and treats any
    /// refusal as "nothing resolved"; mutations do not (a staged lift's commit has its own retry).
    /// Each wait is the remote's `Retry-After` floored at the backoff (see [`throttle_wait`]),
    /// [`jittered`]; once the next one would overrun [`Budgets::throttle_patience`] from the first
    /// attempt, the last `429` is the error, named as `refused` (`what`, the refused request) with
    /// the patience it outlasted. Every other outcome, transport failures and head-wait expiries
    /// included, is handed back untouched.
    async fn send_waiting_out_throttle(&self,
                                       posture: Posture,
                                       method: reqwest::Method,
                                       path: &str,
                                       body: SendBody,
                                       what: &str) -> Result<SendOutcome, String> {
        self.send_waiting_out_throttle_with(posture, method, path, body, reqwest::header::HeaderMap::new(), what).await
    }

    /// [`Self::send_waiting_out_throttle`] with request `headers`, sent again on every retry.
    async fn send_waiting_out_throttle_with(&self,
                                            posture: Posture,
                                            method: reqwest::Method,
                                            path: &str,
                                            body: SendBody,
                                            headers: reqwest::header::HeaderMap,
                                            what: &str) -> Result<SendOutcome, String> {
        let deadline = tokio::time::Instant::now() + self.budgets.throttle_patience;
        let mut delay = RETRY_BACKOFF_START;

        loop {
            let outcome = self.send_on_with(posture, method.clone(), path, body.clone(), headers.clone()).await;
            let response = match outcome {
                SendOutcome::Sent(response)
                    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => response,
                outcome => return Ok(outcome),
            };

            let wait = jittered(throttle_wait(retry_after_seconds(&response), delay), delay);
            delay = (delay * 2).min(RETRY_BACKOFF_CAP);

            if tokio::time::Instant::now() + wait >= deadline {
                // No parenthesis in the action: `is_transient_failure` reads the first one after
                // "The remote refused " as the status.
                let action = format!("{} after retrying for up to {:?}", what, self.budgets.throttle_patience);
                return Err(self.error_of(response, &action).await);
            }

            drop(response);
            tokio::time::sleep(wait).await;
        }
    }

    /// Follow an offloading head's redirect of a bundle response (`what` names it in the
    /// messages) by hand, or hand back `response` and `posture` unchanged when it is not one.
    /// Any `301`/`302`/`303`/`307`/`308` is followed with a bodyless `GET`. To another origin (a
    /// presigned storage URL, which is self-authorizing) it carries no `Authorization` header, so
    /// a token meant for this remote never leaves it; to this remote's own origin it carries the
    /// token, as an auto-following client would have — that route may well require it. On
    /// [`Posture::BoundedObjectReads`], reaching the same silence budget the head-wait posture's
    /// client carries, but on its own grounds — see [`Self::fetch_batch`]'s doc for why this
    /// station's cost shape matches [`Self::fetch_object`]'s, not the request's.
    ///
    /// Returns the response the bundle bytes are read from and the posture that read runs under,
    /// so a body failure is reported against the station that actually produced the bytes.
    async fn follow_bundle_redirect(&self,
                                    response: reqwest::Response,
                                    posture: Posture,
                                    what: &str) -> Result<(reqwest::Response, Posture), String> {
        match response.status() {
            reqwest::StatusCode::MOVED_PERMANENTLY
            | reqwest::StatusCode::FOUND
            | reqwest::StatusCode::SEE_OTHER
            | reqwest::StatusCode::TEMPORARY_REDIRECT
            | reqwest::StatusCode::PERMANENT_REDIRECT => {
                // Resolved against the request's own URL, so a relative `Location` lands where an
                // auto-following client would have taken it.
                let location = response.headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| response.url().join(value).ok())
                    .ok_or_else(|| {
                        format!("The remote's {} redirect carried no usable Location header.", what)
                    })?;

                let redirect_posture = Posture::BoundedObjectReads;
                let own_origin = reqwest::Url::parse(&self.base)
                    .map_or(false, |base| base.origin() == location.origin());
                let outcome = if own_origin {
                    self.send_on_own_origin(redirect_posture, reqwest::Method::GET, &location).await
                } else {
                    self.send_on_presigned(
                        redirect_posture, reqwest::Method::GET, location.as_str(), SendBody::Empty
                    ).await
                };
                let response = self.response_from_send(
                    &format!("following the {} redirect", what), redirect_posture, outcome
                )?;
                Ok((response, redirect_posture))
            }
            _ => Ok((response, posture)),
        }
    }

    /// Whether [`Self::fetch_batch`] is still worth asking, for this instance and every clone of it.
    pub fn batch_lane_open(&self) -> bool {
        self.batch_lane_closed.get().is_none()
    }

    /// Why this remote's batch lane closed, if it did, for this instance and every clone of it.
    /// The first reason stands: two workers closing it at once both saw the lane fail.
    pub fn batch_lane_closure(&self) -> Option<BatchLaneClosure> {
        self.batch_lane_closed.get().copied()
    }

    fn close_batch_lane(&self, reason: BatchLaneClosure) {
        let _ = self.batch_lane_closed.set(reason);
    }

    /// Fetch the object closure of a subtree at a path of a parcel, as a bundle-format stream
    /// (`GET /v1/parcels/{parcel}/subtree/{path}`), and import it as it arrives (see
    /// [`Self::import_response`]). This is the **path-addressed** fetch: the
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
    /// * `parcel`  - The parcel whose tree the path is resolved in.
    /// * `path`    - The warehouse path key of the subtree (`/`-separated, e.g. `src/api`).
    /// * `subtree` - The hash of the tree at `path`, from the spine tree the caller holds. The
    ///   response may carry only that tree's object closure (see `StreamAdmission::Closure`).
    ///
    /// Deliberately **not** on [`Posture::BoundedReads`]: the server side walks and buffers the
    /// whole resolved subtree closure into memory before its first response byte
    /// (`forklift-server/src/server.rs`'s `get_subtree` handler notes an uncapped closure "would
    /// buffer an arbitrarily large bundle in memory"), cost the client cannot bound in advance.
    /// That is [`Self::fetch_batch`]'s `POST`'s shape exactly, so it rides the same
    /// [`Posture::HeadDeadlineNoRedirect`] with the same [`Self::batch_head_budget`]: a head-wait
    /// timer before the status line, the client's resetting silence budget after it. Unlike
    /// `fetch_batch`, an expiry here is an error, not a degrade: the fallback this call has is for
    /// a remote that refuses, and a remote that never answers is not one.
    pub async fn fetch_subtree(&self,
                               parcel: &str,
                               path: &str,
                               subtree: &str) -> Result<Option<bundle_utils::ImportStats>, String> {
        let posture = Posture::HeadDeadlineNoRedirect { head: self.batch_head_budget() };
        let outcome = self.send_waiting_out_throttle(
            posture,
            reqwest::Method::GET, &format!(
                "/v1/parcels/{}/subtree/{}", parcel, encode_path_segments(path)
            ),
            SendBody::Empty,
            &format!("the subtree fetch for \"{}\"", path),
        ).await?;
        let response = self.response_from_send(
            &format!("fetching subtree \"{}\" from the remote", path), posture, outcome
        )?;

        if endpoint_absent(response.status()) || response.status() == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
            return Ok(None);
        }

        let (response, body_posture) = self.follow_bundle_redirect(response, posture, "subtree").await?;

        if !response.status().is_success() {
            return Err(self.error_of(response, &format!("the subtree fetch for \"{}\"", path)).await);
        }

        let admission = bundle_utils::StreamAdmission::Closure { root: subtree.to_string() };
        self.import_response(response, "reading the subtree response", body_posture, admission).await.map(Some)
    }

    /// Import a batch or subtree response into the object store as it arrives, never holding the
    /// whole body: the legitimate size of one runs to tens of GiB, so no memory cap fits it.
    ///
    /// The response's chunks are forwarded over a bounded channel to a blocking task running
    /// [`bundle_utils::import_streamed_bundle`], which re-enters the caller's storage-root scope
    /// (thread-local, so a pool thread would otherwise resolve the store by working directory —
    /// the same idiom as `retrieve_object_for_upload`). Memory is one record, the channel, and the
    /// decoder's bounded window.
    ///
    /// The importer sees the end of the bundle only when the transfer reached the end of the body.
    /// A transfer that fails, or is dropped, closes the channel without that mark, which the
    /// importer reads as an error — so a body cut short never imports as a smaller bundle. When
    /// both sides fail, the transfer's error is the one reported: it is the cause.
    ///
    /// Three consequences of importing while the body arrives, all accepted:
    /// * A transfer that fails partway leaves the records before the break stored. Each is
    ///   verified against its hash, so they are ordinary present objects, and every caller decides
    ///   what to fetch next by presence rather than by this call's result.
    /// * Dropping this future (an outer timeout) drops the sender, and the importer stops at its
    ///   next read — after the record it is storing, never mid-record. Stores are content-addressed
    ///   and atomic, so a fallback storing the same hash meanwhile is harmless.
    /// * A slow importer stops the transfer being read once the channel fills. The silence budget
    ///   is armed only while the body is polled, so this never trips the client's own timeout; a
    ///   remote's idle timeout could, which is the price of not holding the bundle in memory.
    async fn import_response(&self,
                             response: reqwest::Response,
                             action: &str,
                             posture: Posture,
                             admission: bundle_utils::StreamAdmission) -> Result<bundle_utils::ImportStats, String> {
        self.read_response(response, action, posture, move |body| bundle_utils::import_streamed_bundle(body, admission)).await
    }

    /// [`Self::import_response`], with the body read by `read` on a blocking thread in this
    /// thread's storage root.
    async fn read_response<T: Send + 'static>(&self,
                                              mut response: reqwest::Response,
                                              action: &str,
                                              posture: Posture,
                                              read: impl FnOnce(std::io::BufReader<ChannelBody>) -> Result<T, String> + Send + 'static)
                                              -> Result<T, String> {
        let (sender, receiver) = tokio::sync::mpsc::channel::<BodyPiece>(IMPORT_CHANNEL_DEPTH);
        let scope_root = globals::current_scope_root();

        let importer = tokio::task::spawn_blocking(move || {
            let _scope = scope_root.as_deref().map(StorageRootScope::enter);
            read(std::io::BufReader::new(ChannelBody::new(receiver)))
        });

        let transferred: Result<(), String> = loop {
            // An importer that stops (a refused header, a bad record) closes the channel; waiting
            // on the next chunk instead would leave a stalled remote's silence budget to report a
            // timeout in place of the refusal.
            #[allow(clippy::disallowed_methods)] // streamed to the importer, never accumulated
            let next = tokio::select! {
                next = response.chunk() => next,
                _ = sender.closed() => break Ok(()),
            };

            match next {
                Ok(Some(chunk)) => {
                    // A closed channel means the importer already stopped; its result says why.
                    if sender.send(BodyPiece::Bytes(chunk)).await.is_err() {
                        break Ok(());
                    }
                }
                Ok(None) => {
                    let _ = sender.send(BodyPiece::End).await;
                    break Ok(());
                }
                Err(e) => break Err(self.describe_transport_error(action, posture, e)),
            }
        };

        drop(sender);

        let imported = importer.await;

        // When both sides fail, the transfer's error is the cause, even if the importer panicked.
        transferred?;
        imported.map_err(|e| format!("The bundle-import task panicked: {}", e))?
    }

    /// Fetch one object's raw bytes. On [`Posture::BoundedObjectReads`], not [`Posture::BoundedReads`]
    /// — see [`FETCH_OBJECT_READ_TIMEOUT`]'s doc for why this call needs the looser budget.
    ///
    /// A `429` is waited out (see [`Self::send_waiting_out_throttle`]). A degraded batch fetch
    /// sends a whole wave of these at once, which is exactly what a rate-limited head throttles.
    pub async fn fetch_object(&self, hash: &str) -> Result<Vec<u8>, String> {
        match self.fetch_object_classified(hash).await {
            ObjectFetch::Fetched(bytes) => Ok(bytes),
            ObjectFetch::Unreachable(e) | ObjectFetch::Refused(e) => Err(e),
        }
    }

    /// [`Self::fetch_object`], telling a remote that could not serve the object now from one that
    /// answered and refused: a cache warehouse's read refuses the first as a cache miss it can
    /// retry once the origin is reachable, and the second as the remote's own answer. A remote that
    /// could not be reached, stopped answering, answered `502`, `503` or `504`, or kept throttling
    /// past the patience is the first; any other answer is the second.
    pub async fn fetch_object_classified(&self, hash: &str) -> ObjectFetch {
        let posture = Posture::BoundedObjectReads;
        let outcome = match self.send_waiting_out_throttle(
            posture, reqwest::Method::GET, &format!("/v1/objects/{}", hash), SendBody::Empty,
            &format!("object {}", hash),
        ).await {
            Ok(outcome) => outcome,
            // Only a throttle that outlasted the patience is an error here.
            Err(e) => return ObjectFetch::Unreachable(e),
        };
        let response = match outcome {
            SendOutcome::Sent(response) => response,
            outcome => {
                let unsent = self.response_from_send(&format!("fetching object {}", hash), posture, outcome);
                return ObjectFetch::Unreachable(unsent.err().unwrap_or_default());
            }
        };

        if !response.status().is_success() {
            // A gateway or an unavailable server stands in front of an origin that is not up now;
            // any other status, a `500` included, is the origin's own answer for this object.
            let unavailable = matches!(response.status().as_u16(), 502..=504);
            let error = self.error_of(response, &format!("object {}", hash)).await;

            return if unavailable { ObjectFetch::Unreachable(error) } else { ObjectFetch::Refused(error) };
        }

        // An object over the ceiling cannot be stored however it arrives, so it is refused in
        // the ceiling's own terms rather than as a misbehaving remote: a store may still hold one
        // authored before the ceiling existed.
        match body_utils::read_body_capped(response, MAX_OBJECT_BYTES as u64).await {
            Ok(bytes) => ObjectFetch::Fetched(bytes),
            Err(BodyReadError::Transport(e)) => ObjectFetch::Unreachable(
                self.describe_transport_error(&format!("reading object {}", hash), posture, e)
            ),
            Err(BodyReadError::OverCap { cap }) => ObjectFetch::Refused(format!(
                "Object {} on the remote is larger than the {}-byte whole-object ceiling; refusing to fetch it.",
                hash, cap
            )),
        }
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
            &self.budgets,
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
    /// batch rather than being O(constant). Rides [`Posture::HeadDeadline`] like
    /// `missing_objects`, its head-wait bounded per batch by [`Self::presence_negotiation_budget`]
    /// and its body by [`Self::presence_body_budget`] — same accepted residual: a remote slower
    /// than either budget gets abandoned rather than waited out, recovered by an ordinary retry
    /// rather than a looser budget.
    pub async fn upload_targets(&self,
                                session: &str,
                                hashes: &[String]) -> Result<Option<UploadTargetsResponse>, String> {
        let mut merged = UploadTargetsResponse {
            present: Vec::new(),
            targets: BTreeMap::new(),
            direct: Vec::new(),
        };

        for batch in hashes.chunks(MAX_UPLOAD_TARGETS_BATCH) {
            let posture = Posture::HeadDeadline { head: self.presence_negotiation_budget(batch.len()) };
            let outcome = self.send_waiting_out_throttle(
                posture,
                reqwest::Method::POST, "/v1/objects/upload-targets",
                SendBody::json(&UploadTargetsRequest {
                    session: session.to_string(), hashes: batch.to_vec()
                })?,
                "the upload-target request",
            ).await?;
            let response = self.response_from_send(
                "negotiating upload targets", posture, outcome
            )?;

            if endpoint_absent(response.status()) {
                return Ok(None);
            }

            if !response.status().is_success() {
                return Err(self.error_of(response, "the upload negotiation").await);
            }

            let body: UploadTargetsResponse = self.read_presence_json(
                response, body_utils::per_item_body_cap(batch.len(), UPLOAD_TARGETS_BODY_BYTES_PER_HASH),
                "negotiating upload targets", posture, "upload-targets response",
            ).await?;

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
            &self.budgets,
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
    /// promote the session's staged control-plane objects and presence-check its blobs, before the
    /// ref update. Classified from the status line and headers alone (see [`CommitAttempt`]):
    ///
    /// - `2xx` → `Committed`;
    /// - the [`LIFT_COMMIT_HEADER`] not-ready header, whatever the status → `NotReady`;
    /// - `3xx` → a terminal redirect refusal. This rides the never-auto-follow client, and the
    ///   explicit guard is what routes a `3xx` to [`Self::describe_mutation_redirect`] rather than
    ///   a generic refusal: a followed `303` would land as a bare `GET` whose `2xx` reads back as a
    ///   commit that never ran;
    /// - `429` → `Throttled`, with its `Retry-After`;
    /// - any other `4xx` → a terminal refusal (auth, an absent endpoint, a corrupt, missing or
    ///   discarded object), its body read under the error-body budget only to word it;
    /// - `5xx` without the header → `Undetermined`;
    /// - a transport failure → `NotSent` when the connection could not be made (unless the TLS
    ///   handshake was rejected), `Lost` when it was reset or cut off after sending, and a
    ///   terminal error for anything else (a rejected TLS handshake, an answer that is not HTTP).
    ///
    /// `budget` is this attempt's total deadline, connect included; the caller keeps it strictly
    /// above the connect bound.
    async fn commit_lift(&self,
                         session: &str,
                         control_plane: &[String],
                         blobs: &[String],
                         more: bool,
                         budget: std::time::Duration) -> Result<CommitAttempt, String> {
        let body = CommitLiftRequest {
            control_plane: control_plane.to_vec(),
            blobs: blobs.to_vec(),
            more,
        };

        // A head-wait bound, not a total one: `budget` covers connect, the request and the wait
        // for the status line, which is all a retried attempt ever waits on. Only a terminal `4xx`
        // reads a body, under the error-body budget of its own, so an attempt that starts late in
        // the deadline still reads the refusal's code whole.
        let outcome = self.send_on(
            Posture::HeadDeadlineNoRedirect { head: budget },
            reqwest::Method::POST, &format!("/v1/lift/{}/commit", session),
            SendBody::json(&body)?,
        ).await;

        let response = match outcome {
            SendOutcome::Sent(response) => response,
            SendOutcome::Transport(e) => return self.unanswered_commit(e, budget),
            SendOutcome::HeadWaitExpired { budget } => return Ok(CommitAttempt::Lost {
                detail: format!("no answer arrived within {:?}", budget),
            }),
        };

        let status = response.status();

        if status.is_success() {
            return Ok(CommitAttempt::Committed);
        }

        let not_ready = response.headers().get(LIFT_COMMIT_HEADER)
            .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(LIFT_COMMIT_NOT_READY.as_bytes()));

        if not_ready {
            return Ok(CommitAttempt::NotReady);
        }

        if status.is_redirection() {
            return Err(Self::describe_mutation_redirect("committing the lift session", &response));
        }

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Ok(CommitAttempt::Throttled { retry_after: retry_after_seconds(&response) });
        }

        // A `408` is an intermediary giving up on the request before the remote answered it: no
        // more an answer about the session than a `5xx` is.
        if status.is_server_error() || status == reqwest::StatusCode::REQUEST_TIMEOUT {
            return Ok(CommitAttempt::Undetermined { status: status.as_u16() });
        }

        Err(self.error_of(response, "the lift commit").await)
    }

    /// Classify a commit attempt that got no status line. A failed connect is retried, whatever
    /// its cause, unless the TLS handshake was rejected; once connected, only a timeout or a
    /// connection reset or cut off is retried. A rejected handshake or an answer that is not HTTP
    /// repeats unchanged, so it ends the commit at once rather than at the deadline. Of the
    /// retried ones, a failed connect sent nothing (`is_connect()` is decided before a request
    /// byte is written); anything else may have reached the remote.
    fn unanswered_commit(&self, e: reqwest::Error, budget: std::time::Duration) -> Result<CommitAttempt, String> {
        // A failed connect sent nothing, so another attempt costs only time, and the lift talked to
        // this remote moments ago: a connect failure now is far likelier passing (a network
        // gone for a moment, a resolver timing out, a Tor circuit that did not build) than
        // permanent. The exception is a TLS handshake this client rejected, which repeats.
        if e.is_connect() && !Self::tls_rejected(&e) {
            return Ok(CommitAttempt::NotSent {
                detail: format!("could not connect to the remote ({}); nothing was sent", Self::root_cause(&e)),
            });
        }

        if !e.is_timeout() && Self::connection_failed(&e).is_empty() {
            return Err(self.describe_mutation_transport_error("committing the lift session", e));
        }

        if e.is_timeout() {
            return Ok(CommitAttempt::Lost { detail: format!("no answer arrived within {:?}", budget) });
        }

        Ok(CommitAttempt::Lost {
            detail: format!("the connection to the remote failed before an answer arrived ({})", Self::root_cause(&e)),
        })
    }

    /// Fetch a parcel's signature sidecar (`None` for unsigned parcels).
    pub async fn fetch_signature(&self, parcel_hash: &str) -> Result<Option<Vec<u8>>, String> {
        let posture = Posture::BoundedReads;
        let outcome = self.send_waiting_out_throttle(
            posture, reqwest::Method::GET, &format!("/v1/signatures/{}", parcel_hash),
            SendBody::Empty, &format!("the signature of {}", parcel_hash),
        ).await?;
        let response = self.response_from_send(
            &format!("fetching the signature of {}", parcel_hash), posture, outcome
        )?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !response.status().is_success() {
            return Err(self.error_of(response, &format!("the signature of {}", parcel_hash)).await);
        }

        self.read_body(
            response, body_utils::SMALL_BODY_CAP, &format!("reading the signature of {}", parcel_hash),
            posture, &format!("signature of {}", parcel_hash),
        ).await.map(Some)
    }

    /// The retryable class's head-wait for *this* client instance: its own `connect_timeout` plus
    /// the retryable floor ([`RETRYABLE_HEAD_PATIENCE`]). [`Self::upload_signature`] and
    /// [`Self::put_trust`] arm it as is; [`Self::presence_negotiation_budget`] raises it by batch
    /// size, so the floor is derived in this one place. The same `connect + addend` shape every
    /// sibling budget carries, so a Tor dial's 60s connect allowance is never undercut by a budget
    /// sized for a direct remote. Its own `&self` method so a test can pin that the right instance
    /// field is read — see `retryable_head_budget_reads_this_field_not_a_rival_constant`. 35s
    /// direct, 90s over Tor.
    ///
    /// It bounds the wait for the status line only. A refusal's body is then read under
    /// [`Self::error_body_budget`], counted from the status line, so a refusal arriving late in the
    /// head-wait keeps its code — see `a_late_refusal_keeps_its_code_at_every_retryable_call`. The
    /// total deadline the two writes used to ride spanned the body too, and cut such a refusal down
    /// to its bare status reason.
    fn retryable_head_budget(&self) -> std::time::Duration {
        self.budgets.connect + self.budgets.retryable_head_patience
    }

    /// Upload a parcel's signature sidecar.
    ///
    /// Rides [`Posture::HeadDeadlineNoRedirect`], sized by [`Self::retryable_head_budget`] — not
    /// an auto-following client: this call mutates the remote, and this client never
    /// auto-follows a redirect on a mutation — a local, unconditional invariant of *this client*,
    /// holding regardless of which status code or dependency version is in play (see
    /// [`Self::describe_mutation_redirect`]'s doc). Before FORK-89 this rode the auto-following
    /// client with no `is_redirection()` guard, so a `303` (which `tower-http`'s `SEE_OTHER` arm
    /// forces to a bare `GET` unconditionally) silently landed at the redirect target instead of
    /// storing anything, and a `2xx` there read back as a fabricated success. Between FORK-89 and
    /// FORK-49 it rode [`Posture::UnboundedNoRedirect`] — the redirect hole closed, but the
    /// response wait itself still unbounded. A head-wait expiry is reported with the mutation's
    /// uncertainty wording (see [`Self::response_from_send_mutation`]): the write may have landed.
    pub async fn upload_signature(&self, parcel_hash: &str, bytes: Vec<u8>) -> Result<(), String> {
        let action = format!("uploading the signature of {}", parcel_hash);
        let outcome = self.send_on(
            Posture::HeadDeadlineNoRedirect { head: self.retryable_head_budget() },
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
    /// Rides [`Posture::HeadDeadlineNoRedirect`], sized by [`Self::retryable_head_budget`] — same
    /// reasoning as [`Self::upload_signature`]'s doc: a mutation on this client never
    /// auto-follows any redirect, unconditionally, and before FORK-89 this call had no such
    /// guard at all. On `forklift-server` the handler does not wait for a concurrent lift's
    /// verification, however long it runs; what it can still wait on is that server's shared
    /// `spawn_blocking` pool (see [`RETRYABLE_HEAD_PATIENCE`]'s residuals).
    pub async fn put_trust(&self, anchor: &TrustAnchorDto) -> Result<(), String> {
        let action = "uploading the trust anchor";
        let outcome = self.send_on(
            Posture::HeadDeadlineNoRedirect { head: self.retryable_head_budget() },
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
    ///
    /// `tombstone`, on a creation (`old_head` `None`) of a pallet the remote deleted, is the head
    /// its tombstone holds: it acknowledges the deletion, which the remote requires.
    pub async fn update_ref(&self,
                            pallet: &str,
                            old_head: Option<&str>,
                            new_head: &str,
                            tombstone: Option<&str>) -> Result<(), String> {
        let body = RefUpdateRequest {
            old_head: old_head.map(|hash| hash.to_string()),
            new_head: new_head.to_string(),
            tombstone: tombstone.map(|hash| hash.to_string()),
        };
        let action = format!("moving the remote pallet \"{}\"", pallet);
        let route = pallet_route(pallet).map_err(|error| format!("Error while {}: {}", action, error))?;

        let outcome = self.send_on(
            Posture::UnboundedNoRedirect(UnboundedTicket::Fork94),
            reqwest::Method::POST, &route,
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

    /// Delete user pallet `pallet` on the remote, which expects it at `old_head` (a compare-and-set);
    /// the remote keeps that head under a tombstone. `old_head` travels in the query, not a body,
    /// since a proxy may drop a `DELETE`'s body. A retry whose first try landed answers as done.
    ///
    /// Rides [`Posture::HeadDeadlineNoRedirect`], sized by [`Self::retryable_head_budget`], as
    /// [`Self::put_trust`] does: the removal walks no history, and a mutation on this client never
    /// follows a redirect. A head-wait expiry says the delete may have landed.
    pub async fn delete_ref(&self, pallet: &str, old_head: &str) -> Result<(), String> {
        let action = format!("deleting the remote pallet \"{}\"", pallet);

        if !object_utils::is_object_hash(old_head) {
            return Err(format!("Error while {}: {} is not a parcel hash.", action, old_head));
        }
        let route = pallet_route(pallet).map_err(|error| format!("Error while {}: {}", action, error))?;

        let outcome = self.send_on(
            Posture::HeadDeadlineNoRedirect { head: self.retryable_head_budget() },
            reqwest::Method::DELETE, &format!("{}?old_head={}", route, old_head),
            SendBody::Empty,
        ).await;
        let response = self.response_from_send_mutation(&action, outcome)?;

        if response.status().is_redirection() {
            return Err(Self::describe_mutation_redirect(&action, &response));
        }

        if !response.status().is_success() {
            return Err(self.error_of(response, &format!("deleting pallet \"{}\"", pallet)).await);
        }

        Ok(())
    }

    /// Download the remote's latest bundle into `path`, resuming `kept` when the remote still
    /// serves the bundle it was cut from. On [`Posture::BoundedReads`]: the bundle is a
    /// **pre-built** file, so the server's pre-first-byte work is O(constant) — serving an
    /// already-built file — exactly the category [`REMOTE_READ_TIMEOUT`]'s flat budget is honest
    /// for. The chunked `response.chunk()` loop is precisely where the per-read, resets-on-progress
    /// half of `read_timeout` matters: a bundle can be large, and a slow-but-steady download must
    /// never be treated as a stall.
    ///
    /// `path` is a download area, never a bundle this warehouse serves: the caller imports it and
    /// only then publishes it as its own (see [`bundle_utils::publish_bundle_download`]).
    ///
    /// A download is kept across attempts only under a validator: a tag
    /// [`is_resumable_bundle_tag`] accepts and a declared length. Under one, the bytes land in
    /// `path` and `checkpoint` is told the length synced so far — when the download starts, every
    /// `checkpoint_bytes`, when the last byte lands, and, best effort, when it fails — so a later
    /// attempt can ask for the rest. `checkpoint` is told `None` when `path` holds nothing to
    /// resume: the remote has no bundle, or sent no validator. Without one, `path` is removed on
    /// any failure, so nothing is left behind.
    ///
    /// `kept` describes the first `kept.length` bytes of `path`, from an earlier attempt. A
    /// complete one is returned with no request. Otherwise the rest is asked for with `Range`
    /// under `If-Range`: a `206` is taken only when it starts at `kept.length`, carries
    /// `kept.etag` and states `kept.total`, and a `200` restarts `path` under its own tag. Any
    /// other `206`, and a `416`, drop `path` and fall back to one plain request.
    ///
    /// # Returns
    /// * `Ok(BundleDownload::Absent)`   - The remote has no bundle; `path` was removed.
    /// * `Ok(BundleDownload::Complete)` - `path` holds the whole bundle.
    /// * `Err(String)`                  - On any other failure.
    pub async fn download_bundle(&self,
                                 path: &std::path::Path,
                                 kept: Option<&PartialBundle>,
                                 checkpoint_bytes: u64,
                                 checkpoint: &mut (dyn FnMut(Option<&PartialBundle>) -> Result<(), String> + Send))
                                 -> Result<BundleDownload, String> {
        let mut kept = kept.cloned();

        if let Some(partial) = &kept {
            if !holds_download(path, partial) {
                remove_bundle_download(path)?;
                checkpoint(None)?;
                kept = None;
            } else if partial.length == partial.total {
                return Ok(BundleDownload::Complete { resumed: true });
            } else if partial.length == 0 {
                // Nothing kept to resume: the whole bundle comes in one piece, as from scratch.
                kept = None;
            }
        }

        loop {
            // Never encoded in transit: a decoded body loses the length and range that tie it to
            // the bytes kept.
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::ACCEPT_ENCODING, reqwest::header::HeaderValue::from_static("identity"));

            if let Some(partial) = &kept {
                let header = |value: &str| reqwest::header::HeaderValue::from_str(value)
                    .map_err(|e| format!("Error while asking for the rest of the bundle: {}", e));
                headers.insert(reqwest::header::RANGE, header(&format!("bytes={}-", partial.length))?);
                headers.insert(reqwest::header::IF_RANGE, header(&partial.etag)?);
            }

            let posture = Posture::BoundedReads;
            let outcome = self.send_waiting_out_throttle_with(
                posture, reqwest::Method::GET, "/v1/bundles/latest", SendBody::Empty, headers, "the bundle fetch"
            ).await?;
            let response = self.response_from_send("fetching the bundle", posture, outcome)?;
            let status = response.status();

            if status == reqwest::StatusCode::NOT_FOUND {
                remove_bundle_download(path)?;
                checkpoint(None)?;
                return Ok(BundleDownload::Absent);
            }

            let resumed = match (status, &kept) {
                (reqwest::StatusCode::PARTIAL_CONTENT, Some(partial)) if continues_download(&response, partial) => {
                    kept.take()
                }
                (reqwest::StatusCode::PARTIAL_CONTENT | reqwest::StatusCode::RANGE_NOT_SATISFIABLE, Some(_)) => {
                    // Not the rest of the bundle kept: start over with one plain request.
                    drop(response);
                    remove_bundle_download(path)?;
                    checkpoint(None)?;
                    kept = None;
                    continue;
                }
                (reqwest::StatusCode::PARTIAL_CONTENT, None) => {
                    return Err(CoreError::refusal(
                        RefusalCode::RemoteProtocolViolation,
                        format!(
                            "The remote {} answered a request for its whole bundle with part of one.",
                            self.base
                        ),
                        "Report it to whoever runs the remote; retrying gets the same answer.",
                    ).into());
                }
                (status, _) if status.is_success() => None,
                _ => return Err(self.error_of(response, "the bundle").await),
            };

            return self.receive_bundle(response, path, resumed, checkpoint_bytes, checkpoint).await;
        }
    }

    /// Write the body of `response`, the bundle or (when `resumed`) the rest of it, into `path`;
    /// see [`Self::download_bundle`].
    async fn receive_bundle(&self,
                            mut response: reqwest::Response,
                            path: &std::path::Path,
                            resumed: Option<PartialBundle>,
                            checkpoint_bytes: u64,
                            checkpoint: &mut (dyn FnMut(Option<&PartialBundle>) -> Result<(), String> + Send))
                            -> Result<BundleDownload, String> {
        let posture = Posture::BoundedReads;
        let was_resumed = resumed.is_some();
        let validator = resumed.clone().or_else(|| bundle_validator(&response));
        let write_error = |e: std::io::Error| format!("Error while writing the bundle file: {}", e);

        let Some(mut partial) = validator else {
            // Nothing to resume from: download as a whole, and leave nothing behind on a failure.
            remove_bundle_download(path)?;
            checkpoint(None)?;

            let mut file = std::fs::File::create(path)
                .map_err(|e| format!("Error while creating the bundle file: {}", e))?;
            let mut cleanup = RemoveFileOnDrop::armed(path);

            #[allow(clippy::disallowed_methods)] // streamed to the bundle file, never accumulated
            while let Some(chunk) = response.chunk()
                .await
                .map_err(|e| self.describe_transport_error("downloading the bundle", posture, e))?
            {
                std::io::Write::write_all(&mut file, &chunk).map_err(write_error)?;
            }

            sync_download(&file)?;
            cleanup.disarm();

            return Ok(BundleDownload::Complete { resumed: false });
        };

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(!was_resumed)
            .open(path)
            .map_err(|e| format!("Error while opening the bundle file: {}", e))?;

        if was_resumed {
            // Bytes past the kept length were never synced, so they are not trusted.
            file.set_len(partial.length).map_err(write_error)?;
            std::io::Seek::seek(&mut file, std::io::SeekFrom::End(0)).map_err(write_error)?;
        } else {
            checkpoint(Some(&partial))?;
        }

        let mut written = partial.length;
        let mut unsynced = 0u64;

        #[allow(clippy::disallowed_methods)] // streamed to the bundle file, never accumulated
        let streamed: Result<(), String> = loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) if written == partial.total => break Ok(()),
                Ok(None) => break Err(format!(
                    "Error while downloading the bundle: it ended after {} of its {} bytes.", written, partial.total
                )),
                Err(e) => break Err(self.describe_transport_error("downloading the bundle", posture, e)),
            };

            if chunk.len() as u64 > partial.total - written {
                break Err(format!(
                    "Error while downloading the bundle: the remote sent more than its {} bytes.", partial.total
                ));
            }

            if let Err(e) = std::io::Write::write_all(&mut file, &chunk) {
                break Err(write_error(e));
            }

            written += chunk.len() as u64;
            unsynced += chunk.len() as u64;

            if unsynced >= checkpoint_bytes && written < partial.total {
                if let Err(e) = sync_download(&file) {
                    break Err(e);
                }

                partial.length = written;
                checkpoint(Some(&partial))?;
                unsynced = 0;
            }
        };

        if let Err(error) = streamed {
            // Best effort: keep what did land, so the next attempt asks only for the rest.
            if written > partial.length && sync_download(&file).is_ok() {
                partial.length = written;
                let _ = checkpoint(Some(&partial));
            }

            return Err(error);
        }

        sync_download(&file)?;
        partial.length = written;
        checkpoint(Some(&partial))?;

        Ok(BundleDownload::Complete { resumed: was_resumed })
    }
}

/// How often a resumable bundle download is synced and its length recorded. A judgment figure,
/// with no measured calibration: it caps what a crash can cost at one interval, and the extra
/// syncs at one per interval.
pub const BUNDLE_CHECKPOINT_BYTES: u64 = 64 * 1024 * 1024;

/// The longest bundle tag a download is kept under. It only bounds a hostile header: the tags
/// forklift-server sends are far shorter.
const MAX_BUNDLE_TAG_BYTES: usize = 256;

/// A bundle download kept from an earlier attempt: the first `length` bytes of the bundle the
/// remote served under `etag`, which declared `total` bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartialBundle {
    /// The tag the remote served the bundle under, quotes included.
    pub etag: String,

    /// The bundle's whole length, as the remote declared it.
    pub total: u64,

    /// How much of it was synced to the download file. At most `total`.
    pub length: u64,
}

/// What [`RemoteClient::fetch_object_classified`] found.
#[derive(Debug)]
pub enum ObjectFetch {
    /// The object's bytes, unverified.
    Fetched(Vec<u8>),
    /// The remote could not serve it now: it could not be reached, stopped answering before the
    /// object arrived, answered as a gateway or unavailable server (`502`–`504`), or kept
    /// throttling.
    Unreachable(String),
    /// The remote answered, and refused to hand the object over.
    Refused(String),
}

/// What [`RemoteClient::download_bundle`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum BundleDownload {
    /// The remote has no bundle.
    Absent,

    /// The download file holds the whole bundle; `resumed` when some of it was kept from an
    /// earlier attempt.
    Complete { resumed: bool },
}

/// Whether a download may be kept under `tag`: a strong entity tag (quoted, so not weak), no
/// longer than [`MAX_BUNDLE_TAG_BYTES`], of visible ASCII only.
pub fn is_resumable_bundle_tag(tag: &str) -> bool {
    let bytes = tag.as_bytes();

    (2..=MAX_BUNDLE_TAG_BYTES).contains(&bytes.len())
        && bytes[0] == b'"'
        && bytes[bytes.len() - 1] == b'"'
        && bytes[1..bytes.len() - 1].iter().all(|byte| (0x21..=0x7e).contains(byte) && *byte != b'"')
}

/// The validator a whole-bundle `response` carries, as an empty download under it: one
/// resumable tag and one declared length, with the body sent as it is stored. `None` without
/// both, so nothing is kept.
fn bundle_validator(response: &reqwest::Response) -> Option<PartialBundle> {
    let headers = response.headers();
    let etag = single_header(headers, reqwest::header::ETAG).filter(|tag| is_resumable_bundle_tag(tag))?;
    let total = single_header(headers, reqwest::header::CONTENT_LENGTH).and_then(parse_decimal)?;

    is_identity_encoded(headers).then(|| PartialBundle { etag: etag.to_string(), total, length: 0 })
}

/// Whether the `206` in `response` is the rest of `kept`: it starts where `kept` ends, under the
/// same tag, of the same total, with the body sent as it is stored.
fn continues_download(response: &reqwest::Response, kept: &PartialBundle) -> bool {
    let headers = response.headers();
    let range = single_header(headers, reqwest::header::CONTENT_RANGE).and_then(parse_content_range);
    let length = headers.get_all(reqwest::header::CONTENT_LENGTH).iter().count();

    range == Some((kept.length, kept.total - 1, kept.total))
        && single_header(headers, reqwest::header::ETAG) == Some(kept.etag.as_str())
        && (length == 0 || single_header(headers, reqwest::header::CONTENT_LENGTH).and_then(parse_decimal) == Some(kept.total - kept.length))
        && is_identity_encoded(headers)
}

/// The value of `name` in `headers`, when it is sent exactly once and is visible ASCII.
fn single_header(headers: &reqwest::header::HeaderMap, name: reqwest::header::HeaderName) -> Option<&str> {
    let mut values = headers.get_all(name).iter();

    match (values.next(), values.next()) {
        (Some(value), None) => value.to_str().ok().map(str::trim),
        _ => None,
    }
}

/// Whether the body in `headers` is sent as it is stored: no `Content-Encoding` but `identity`.
fn is_identity_encoded(headers: &reqwest::header::HeaderMap) -> bool {
    headers.get_all(reqwest::header::CONTENT_ENCODING).iter()
        .all(|value| value.to_str().is_ok_and(|value| value.trim().eq_ignore_ascii_case("identity")))
}

/// A run of ASCII digits as a number.
fn parse_decimal(text: &str) -> Option<u64> {
    (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())).then(|| text.parse().ok()).flatten()
}

/// `bytes <first>-<last>/<total>` as its three numbers.
fn parse_content_range(text: &str) -> Option<(u64, u64, u64)> {
    let (unit, rest) = text.split_once(' ')?;
    let (range, total) = rest.trim().split_once('/')?;
    let (first, last) = range.split_once('-')?;

    unit.eq_ignore_ascii_case("bytes")
        .then(|| Some((parse_decimal(first)?, parse_decimal(last)?, parse_decimal(total)?)))
        .flatten()
}

/// Whether the download file at `path` still holds what `kept` says was synced to it: a file, not
/// a link, at least that long.
fn holds_download(path: &std::path::Path, kept: &PartialBundle) -> bool {
    kept.length <= kept.total
        && std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() >= kept.length)
}

/// Remove the download file at `path`, if there is one.
fn remove_bundle_download(path: &std::path::Path) -> Result<(), String> {
    file_utils::remove_unfollowed(path)
}

/// Sync a download through the handle that wrote it, never by reopening it: a handle reopened
/// after the fact cannot force a durable fsync on every platform this runs on.
fn sync_download(file: &std::fs::File) -> Result<(), String> {
    if file_utils::fsync_enabled() {
        file.sync_all().map_err(|e| format!("Error while syncing the bundle file: {}", e))?;
    }

    Ok(())
}

/// Removes the file at `path` when dropped, unless [`Self::disarm`] was called first.
/// [`RemoteClient::receive_bundle`] uses it for a download nothing could resume: it guards every
/// early return between creating the file and the download finishing, so a failure never leaves
/// a part of a bundle behind.
struct RemoveFileOnDrop<'a> {
    path: &'a std::path::Path,
    armed: bool,
}

impl<'a> RemoveFileOnDrop<'a> {
    fn armed(path: &'a std::path::Path) -> RemoveFileOnDrop<'a> {
        RemoveFileOnDrop { path, armed: true }
    }

    /// Call once the download finished, so `Drop` keeps the file.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RemoveFileOnDrop<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(self.path);
        }
    }
}

/// Fetch everything reachable from a parcel head that is missing locally: the parcel
/// graph, every parcel's signature, and the full tree/blob closure — verified
/// object by object before storing.
///
/// The walk stops at any parcel already reachable from a **local ref head** — every pallet
/// and every meta pallet (`@office`, `@haul`, …), since they share one object store and a
/// ref of either kind is an equally good witness. Their closures are complete by
/// construction: a ref only moves once its objects are all present (a `stack` writes them
/// first; a `lower` or `clone` fetches the whole closure before the fast-forward). So a
/// lower that brings one new parcel walks one parcel, not the whole history — the
/// transfer-economics half of the bounded-negotiation guarantee.
///
/// It still heals an interrupted earlier sync. An interruption leaves the ref where it was,
/// so the objects it half-fetched sit *above* the bound and are re-walked exactly as before.
/// What is no longer re-walked is history behind a ref, which was proven complete when that
/// ref moved. (`audit` is what re-proves a whole history; this is a fetch, not an audit.)
///
/// The old walk also re-probed the remote for the signature of every unsigned parcel on
/// every sync, since "no signature here" is indistinguishable from "not fetched yet". Behind
/// the bound, it no longer asks.
///
/// # Arguments
/// * `client` - The remote.
/// * `head`   - The parcel hash to fetch from.
///
/// # Returns
/// * `Ok(FetchStats)` - What was actually transferred, and how many parcels were walked.
/// * `Err(String)`    - If a transfer or verification failed.
pub async fn fetch_history(client: &RemoteClient,
                           head: &str,
                           progress: &Progress) -> Result<FetchStats, String> {
    fetch_history_within(client, head, &[], progress).await
}

/// [`fetch_history`], also bounded by `roots`: parcels whose closures are known complete without a
/// ref naming them. The git mirror passes its roots record (§9.8), whose parcels gc keeps live with
/// their closures, so a pallet the warehouse holds no ref for is not walked again from genesis.
///
/// # Arguments
/// * `client` - The remote.
/// * `head`   - The parcel hash to fetch from.
/// * `roots`  - Further parcels whose whole closure is present (absent ones are ignored).
///
/// # Returns
/// * `Ok(FetchStats)` - What was actually transferred, and how many parcels were walked.
/// * `Err(String)`    - If a transfer or verification failed.
pub async fn fetch_history_within(client: &RemoteClient,
                                  head: &str,
                                  roots: &[String],
                                  progress: &Progress) -> Result<FetchStats, String> {
    Ok(walk_history(client, head, roots, true, progress).await?.0)
}

/// The parcels and signatures of `head`'s history, bounded as [`fetch_history`] bounds them, and
/// none of their content, recording nothing: for a move of the origin, which reads the new server's
/// history to judge it and adopts nothing from it. The parcels it stores no ref reaches until a
/// fetch takes their head, so they are what any interrupted fetch leaves.
///
/// # Arguments
/// * `client` - The remote.
/// * `head`   - The parcel hash to fetch from.
///
/// # Returns
/// * `Ok(FetchStats)` - What was actually transferred, and how many parcels were walked.
/// * `Err(String)`    - If a transfer or verification failed.
pub async fn fetch_parcels_only(client: &RemoteClient,
                                head: &str,
                                progress: &Progress) -> Result<FetchStats, String> {
    Ok(walk_history(client, head, &[], false, progress).await?.0)
}

/// The bounded parcel walk of [`fetch_history_within`]: every parcel and signature from `head` down
/// to what is already known complete, and with `content`, every walked parcel's closure as well.
/// Without it, nothing below a parcel is fetched and the walked parcels' root trees are returned
/// instead, in walk order, one per parcel.
async fn walk_history(client: &RemoteClient,
                      head: &str,
                      roots: &[String],
                      content: bool,
                      progress: &Progress) -> Result<(FetchStats, Vec<String>), String> {
    let mut stats = FetchStats::default();
    let mut skipped_roots: Vec<String> = Vec::new();

    progress.status("Fetching history");

    // Every local ref head — user pallets and meta pallets alike — and therefore every
    // closure already known complete. Empty for a clone into a fresh warehouse, which
    // walks everything, as it must. In a cache, "complete" holds up to the record: a head's
    // parcels are all present, and its content is present or recorded.
    let mut complete: Vec<String> = pallet_utils::all_pallet_refs()?
        .into_iter()
        .map(|(_, head)| head)
        .collect();
    for root in roots {
        if file_utils::does_object_exist(root)? {
            complete.push(root.clone());
        }
    }

    let mut parcel_frontier: Vec<String> = vec![head.to_string()];
    let mut seen_parcels: HashSet<String> = HashSet::new();
    let mut seen = ContentSeen::default();
    let mut signatures = walk_signatures();

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
        stats.fetched_signatures += fetch_missing_signatures(client, &wave, &mut signatures).await?;
        report_fetch(progress, &stats);

        // The parcels are present now; their trees and parents drive the next waves.
        let mut tree_frontier: Vec<String> = Vec::new();

        for hash in &wave {
            let parcel = object_utils::load_parcel(hash)?;

            if content {
                tree_frontier.push(parcel.tree_hash.clone());
            } else {
                skipped_roots.push(parcel.tree_hash.clone());
            }
            parcel_frontier.extend(parcel.parents);
        }

        fetch_tree_closures(client, tree_frontier, &mut seen, &mut stats, progress).await?;
    }

    signatures.finish()?;
    Ok((stats, skipped_roots))
}

/// The signatures one fetch walk receives, held across all its waves and published as signature
/// tables, a full table as soon as one has gathered and the rest before the walk returns, so
/// before its caller moves a ref or records a root. A wave is one generation of parcels, a single
/// parcel on linear history, so a table per wave would still be a file per parcel. A walk that
/// dies first loses only the signatures it still held: their parcels are behind no ref and no
/// root yet, so the next fetch walks them again and fetches the signatures it finds missing.
fn walk_signatures() -> sign_utils::SignatureBatch {
    sign_utils::SignatureBatch::new(sign_utils::OnConflict::Refuse)
}

/// The trees and blobs a walk has already fetched, so a subtree shared between parcels is walked once.
#[derive(Default)]
struct ContentSeen {
    trees: HashSet<String>,
    blobs: HashSet<String>,
}

/// Fetch the whole closure of every tree in `tree_frontier`, level by level: each level's trees,
/// then their blobs and recipes, then each recipe's chunks.
async fn fetch_tree_closures(client: &RemoteClient,
                             mut tree_frontier: Vec<String>,
                             seen: &mut ContentSeen,
                             stats: &mut FetchStats,
                             progress: &Progress) -> Result<(), String> {
    while !tree_frontier.is_empty() {
        let tree_wave: Vec<String> = tree_frontier.drain(..)
            .filter(|hash| seen.trees.insert(hash.clone()))
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
                if seen.blobs.insert(file.hash.clone()) {
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
        report_fetch(progress, stats);
    }

    Ok(())
}

/// A cache warehouse's fetch of `head`: the parcels and signatures of its history, bounded as
/// [`fetch_history`] bounds them, and none of their content. Every root tree the walk reaches is
/// recorded in the sealed-object record ([`sealed_utils`]) where it is absent, and where it is
/// present so is every absent object beneath it, synced, before this returns, so before any
/// caller moves a ref onto the head: the ref gate finds each absence recorded. A bay that checks
/// the head out fetches its content with [`fetch_head_content`] first.
///
/// The record's readers take no lock, and one may read while this appends. Every object a local
/// ref reaches is present or already recorded, so what this appends, absent and unrecorded, no
/// local ref reaches yet: a reader that misses the append judges nothing that leans on it.
///
/// # Arguments
/// * `client` - The remote.
/// * `head`   - The parcel hash to fetch from.
///
/// # Returns
/// * `Ok(FetchStats)` - What was actually transferred, and how many parcels were walked.
/// * `Err(String)`    - If a transfer or verification failed, or the record could not be written.
///                      Whatever transferred stays, and no ref has moved.
pub async fn fetch_cache(client: &RemoteClient,
                         head: &str,
                         progress: &Progress) -> Result<FetchStats, String> {
    let (stats, skipped_roots) = walk_history(client, head, &[], false, progress).await?;

    sealed_utils::record_absences_under(&skipped_roots)?;

    Ok(stats)
}

/// In a cache warehouse, fetch the whole closure of `head`'s root tree, for a bay about to check
/// out or merge `head`; anywhere else nothing, since a user pallet's fetch already brought every
/// in-scope object. `head` itself must be present, fetched with its history first.
///
/// The fetch that brought `head` recorded its root, and a ref may already reach that root while it
/// is recorded and absent. The closure fills top-down, so while it fills, a present tree stands
/// over children not fetched yet and not recorded: a lock-free reader then sees an incomplete
/// closure, and a ref write leaning on it is refused until the fill ends. A fill that returns an
/// error records every absence beneath the root first, so the closure is whole up to the record
/// again. One that stops without returning (the process killed, or the future dropped) leaves the
/// root present and recorded over unrecorded absences: `heal` records them as it unseals the root,
/// and rerunning the command fills them. Once the closure
/// is present, the records of the trees and files it filled are removed, since a whole present
/// closure needs none.
///
/// # Arguments
/// * `client` - The remote.
/// * `head`   - The parcel whose content is fetched.
///
/// # Returns
/// * `Ok(FetchStats)` - What was actually transferred.
/// * `Err(String)`    - If the mode cannot be read, or a transfer or verification failed.
pub async fn fetch_head_content(client: &RemoteClient,
                                head: &str,
                                progress: &Progress) -> Result<FetchStats, String> {
    let mut stats = FetchStats::default();

    if scope_utils::is_cache_mode()? {
        let root = object_utils::load_parcel(head)?.tree_hash;
        let mut seen = ContentSeen::default();

        if let Err(why) = fetch_tree_closures(client, vec![root.clone()], &mut seen, &mut stats, progress).await {
            return Err(match sealed_utils::record_absences_under(&[root]) {
                Ok(()) => why,
                Err(unrecorded) => format!("{} What it left absent could not be recorded either: {}", why, unrecorded),
            });
        }

        sealed_utils::forget_filled(seen.trees.iter().chain(&seen.blobs))?;
    }

    Ok(stats)
}

/// A user pallet's fetch with a full fetch scope: [`fetch_cache`] in a cache warehouse, otherwise
/// [`fetch_history`].
async fn fetch_unscoped(client: &RemoteClient,
                        head: &str,
                        progress: &Progress) -> Result<FetchStats, String> {
    if scope_utils::is_cache_mode()? {
        fetch_cache(client, head, progress).await
    } else {
        fetch_history(client, head, progress).await
    }
}

/// Fetch a user pallet's head like [`fetch_history`], but path-prune the **content** walk to the
/// warehouse fetch scope: the full parcel graph, every signature, and the tree spine down to each
/// in-scope prefix are fetched, along with the in-scope subtrees and blobs in full; out-of-scope
/// subtree objects and blobs are skipped — they stay sealed by the hash the spine tree already
/// carries. Only the user pallet's content is fetched this way; office and other meta pallets
/// keep routing through the unscoped [`fetch_history`], because their audit reads full content.
///
/// A full warehouse delegates to [`fetch_history`] verbatim and records nothing, and a cache to
/// [`fetch_cache`], which fetches the parcels only and records the content it skips. A full warehouse
/// never becomes sparse (`scope prune` refuses one, and `clone` writes a scope only into a new
/// warehouse), so that check needs no lock.
///
/// In a sparse warehouse the fetch is recorded before anything transfers: under the
/// [`RefLock`], in one hold, it reads the fetch scope and adds the head to the fetched-heads
/// ledger ([`fetch_ledger`]), so a `scope fetch` that commits while this fetch runs walks the head at
/// the widened scope. Once the transfer completes it takes the lock again and re-reads the scope;
/// if a `scope fetch` widened it meanwhile, it records the head again and fetches at the new scope,
/// until the scope it fetched at is still current. Only then does it return and let the caller
/// write its ref.
///
/// Like [`fetch_history`], it heals an interrupted earlier sync: the ref is unmoved until the
/// whole scoped closure is present, so re-running re-walks only what is still missing (the
/// fetch primitives skip objects already on disk).
///
/// # Arguments
/// * `client` - The remote.
/// * `head`   - The parcel hash to fetch from.
/// * `pallet` - The pallet the head is fetched for, as its wire name (recorded in the ledger).
///
/// # Returns
/// * `Ok(FetchStats)` - What was actually transferred, and how many parcels were walked.
/// * `Err(String)`    - If a transfer or verification failed.
pub async fn fetch_history_scoped(client: &RemoteClient,
                                  head: &str,
                                  pallet: &str,
                                  progress: &Progress) -> Result<FetchStats, String> {
    if scope_utils::read_fetch_scope()?.is_full() {
        return fetch_unscoped(client, head, progress).await;
    }

    fetch_recorded(head, pallet, async |scope: &PathScope| {
        fetch_bounded_at(client, head, scope, progress).await
    }).await
}

/// [`fetch_history_scoped`] at a scope the caller names rather than the one on disk — for
/// `clone`, which fetches before it writes any scope, so that an `--only` naming nothing is
/// refused with nothing left behind. A sparse scope records the head in the ledger the same way,
/// under the [`RefLock`], before anything transfers. It does not re-check the scope afterwards:
/// no scope exists on disk for a `scope fetch` to widen until the caller writes this one.
///
/// # Arguments
/// * `client` - The remote.
/// * `head`   - The parcel hash to fetch from.
/// * `pallet` - The pallet the head is fetched for, as its wire name.
/// * `scope`  - The fetch scope to fetch at. A full one in a warehouse whose mode on disk is
///              cache fetches as [`fetch_cache`] does.
///
/// # Returns
/// * `Ok(FetchStats)` - What was actually transferred, and how many parcels were walked.
/// * `Err(String)`    - If a transfer or verification failed.
pub async fn fetch_history_at(client: &RemoteClient,
                              head: &str,
                              pallet: &str,
                              scope: &PathScope,
                              progress: &Progress) -> Result<FetchStats, String> {
    if scope.is_full() {
        return fetch_unscoped(client, head, progress).await;
    }

    {
        let lock = RefLock::acquire()?;
        fetch_ledger::record_fetch(&lock, head, pallet)?;
    }

    fetch_bounded_at(client, head, scope, progress).await
}

/// One transfer of a scoped fetch: the whole history at a full scope, otherwise the path-pruned
/// walk bounded at every local ref head (a closure already known complete at this scope needs
/// neither fetching nor walking).
async fn fetch_bounded_at(client: &RemoteClient,
                          head: &str,
                          scope: &PathScope,
                          progress: &Progress) -> Result<FetchStats, String> {
    if scope.is_full() {
        return fetch_history(client, head, progress).await;
    }

    fetch_scoped_from(client, head, scope, &ref_heads()?, &WalkSeen::default(), &mut WalkSeen::default(), progress).await
}

/// The record-and-recheck loop of [`fetch_history_scoped`], with the transfer passed in so a test
/// can change the scope while it runs.
async fn fetch_recorded<F>(head: &str, pallet: &str, mut transfer: F) -> Result<FetchStats, String>
where
    F: AsyncFnMut(&PathScope) -> Result<FetchStats, String>,
{
    let mut scope = {
        let lock = RefLock::acquire()?;
        let scope = scope_utils::read_fetch_scope()?;
        fetch_ledger::record_fetch(&lock, head, pallet)?;
        scope
    };

    let mut stats = FetchStats::default();

    loop {
        stats.add(&transfer(&scope).await?);

        let lock = RefLock::acquire()?;
        let current = scope_utils::read_fetch_scope()?;

        if current == scope {
            return Ok(stats);
        }

        // An `scope fetch` committed while this fetch ran: it may have dropped or never seen this
        // head, so record it again before fetching what the widened scope adds.
        fetch_ledger::record_fetch(&lock, head, pallet)?;
        drop(lock);

        scope = current;
    }
}

/// Every local user-pallet ref with its head. A meta pallet's closure is fetched whole whatever
/// the scope, so widening the scope adds nothing to it.
fn user_refs() -> Result<Vec<(pallet_utils::PalletRef, String)>, String> {
    Ok(pallet_utils::all_pallet_refs()?.into_iter()
        .filter(|(pallet, _)| pallet.namespace == pallet_utils::PalletNamespace::User)
        .collect())
}

/// Every local ref head, user and meta pallets alike.
fn ref_heads() -> Result<Vec<String>, String> {
    Ok(pallet_utils::all_pallet_refs()?.into_iter().map(|(_, head)| head).collect())
}

impl FetchStats {
    /// Add what `other` transferred and walked to these stats.
    pub fn add(&mut self, other: &FetchStats) {
        self.fetched_objects += other.fetched_objects;
        self.fetched_signatures += other.fetched_signatures;
        self.walked_parcels += other.walked_parcels;
    }
}

/// How many times `scope fetch` walks and tries to commit before it gives up on a warehouse that keeps
/// changing under it.
pub const EXPAND_ATTEMPTS: usize = 3;

/// The outcome of one [`Expansion::commit`].
pub enum ExpandCommit {
    /// The widened scope and the pruned ledger were written.
    Written(PathScope),

    /// Another `scope fetch` already brought every added prefix into scope; nothing was written.
    Covered(PathScope),

    /// The warehouse changed since the walk: walk again, then commit again.
    Retry,
}

/// The engine behind `scope fetch`: widen a sparse warehouse's fetch scope, fetching the newly
/// in-scope content of every closure the warehouse holds first.
///
/// Every closure means the bay's base, every local ref head and every present head in the
/// fetched-heads ledger ([`fetch_ledger`]): a ref's closure is complete only at the scope it was
/// fetched at, and a fetched head without a ref can become a ref later through a write that
/// fetches nothing. The heads are walked one at a time over shared dedup sets, so a history they
/// share is walked once.
///
/// A ledger head that no ref reaches and whose walk fails is dropped from the ledger rather than
/// failing the expand: its fetch went only part-way, so its closure is incomplete at the widened
/// scope, and the ref compare-and-set refuses it as a new head for that reason until a fetch of it
/// completes. The walk failing for a reason that is not the remote lacking the head (a dropped
/// connection) drops it too. A base or ref head whose walk fails refuses the expand, naming the
/// pallet.
///
/// The steps are separate so that a test can change the warehouse between them: [`walk`] fetches,
/// [`walk_new_ref_heads_locally`] covers ref heads that appeared since without the network, and
/// [`commit`] compares and writes under the [`RefLock`], never walking inside the hold.
///
/// [`walk`]: Expansion::walk
/// [`walk_new_ref_heads_locally`]: Expansion::walk_new_ref_heads_locally
/// [`commit`]: Expansion::commit
pub struct Expansion {
    /// The bay's pallet and base parcel.
    pallet: String,
    base: String,

    /// The prefixes being added, none of them in `from`.
    added: Vec<String>,

    /// The fetch scope the walk widens from, and the widened scope it walks at.
    from: PathScope,
    widened: PathScope,

    /// What the successful walks visited, shared across heads.
    seen: WalkSeen,

    /// Whether the base, and the ref heads, have had their network walk.
    base_walked: bool,
    refs_walked: bool,

    /// The heads walked successfully.
    walked_heads: HashSet<String>,

    /// Each ref head walked, with the parcels its walk added: parcels that ref reaches. A ledger
    /// head among them is dropped at the commit if that ref still stands there.
    reached: Vec<(String, HashSet<String>)>,

    /// Parcels covered by the local walk: built here, from parents in `seen` or in this set.
    local_parcels: HashSet<String>,

    /// Ledger heads whose walk failed, to be dropped from the ledger at the commit.
    unwalkable: BTreeSet<String>,

    /// Unwalkable heads a new ref reached, taken off that set once to be walked again.
    retried: HashSet<String>,

    /// Objects fetched so far.
    fetched_objects: usize,
}

impl Expansion {
    /// Start an expansion of the scope `from` by `added`, for the bay standing on `pallet` at
    /// `base`. No prefix in `added` may already be in `from`.
    pub fn new(pallet: &str, base: &str, from: PathScope, added: Vec<String>) -> Expansion {
        let mut prefixes: Vec<String> = from.prefixes().to_vec();
        prefixes.extend(added.iter().cloned());

        Expansion {
            pallet: pallet.to_string(),
            base: base.to_string(),
            added,
            widened: PathScope::from_prefixes(prefixes),
            from,
            seen: WalkSeen::default(),
            base_walked: false,
            refs_walked: false,
            walked_heads: HashSet::new(),
            reached: Vec::new(),
            local_parcels: HashSet::new(),
            unwalkable: BTreeSet::new(),
            retried: HashSet::new(),
            fetched_objects: 0,
        }
    }

    /// The prefixes this expansion adds.
    pub fn added(&self) -> &[String] {
        &self.added
    }

    /// The objects fetched so far.
    pub fn fetched_objects(&self) -> usize {
        self.fetched_objects
    }

    /// Fetch the base at the widened scope, once, so that the added prefixes can be checked
    /// against its tree before anything else is fetched.
    ///
    /// # Returns
    /// * `Ok(())`      - The base was walked.
    /// * `Err(String)` - It could not be; the error names the bay's pallet.
    pub async fn walk_base(&mut self, client: &RemoteClient) -> Result<(), String> {
        if !self.base_walked {
            let base = self.base.clone();
            let added = self.walk_head(client, &base).await
                .map_err(|e| unservable(&self.pallet, &e))?;
            // The base is normally its pallet's ref head, whose own walk then adds nothing.
            self.reached.push((base, added));
            self.base_walked = true;
        }

        Ok(())
    }

    /// Fetch at the widened scope: the base and every user-pallet ref head on the first call, then
    /// every present ledger head not yet walked or found unwalkable. A ledger head whose parcel is absent is
    /// not walked, since walking it would download it again.
    ///
    /// # Returns
    /// * `Ok(())`      - Every head was walked or found unwalkable.
    /// * `Err(String)` - The base or a ref head could not be walked; the error names its pallet.
    pub async fn walk(&mut self, client: &RemoteClient) -> Result<(), String> {
        self.walk_base(client).await?;

        if !self.refs_walked {
            for (pallet, head) in user_refs()? {
                let added = self.walk_head(client, &head).await
                    .map_err(|e| unservable(&pallet.to_wire(), &e))?;
                self.reached.push((head, added));
            }

            self.refs_walked = true;
        }

        for entry in fetch_ledger::read_ledger()? {
            if self.walked_heads.contains(&entry.head)
                || self.unwalkable.contains(&entry.head)
                || !file_utils::does_object_exist(&entry.head)?
            {
                continue;
            }

            // A head a ref reaches is in `seen` already, so its walk succeeds without fetching;
            // one that fails is reached by no ref.
            if self.walk_head(client, &entry.head).await.is_err() {
                self.unwalkable.insert(entry.head);
            }
        }

        Ok(())
    }

    /// Cover every ref head outside the walked set without the network, by walking its parcels
    /// back to that set. Such a head was written after the walk, by a fetch whose head the ledger
    /// holds or by a local build whose out-of-scope subtrees come from its parents, so walking the
    /// parcel graph is enough. Runs outside the lock.
    ///
    /// A ref head that reaches an unwalkable head is left uncovered the first time, and that head
    /// is taken off the unwalkable set: the fetch that wrote the ref may have completed it, so
    /// the next walk tries it again and the commit retries meanwhile. Reaching it a second time
    /// refuses the expand.
    ///
    /// # Returns
    /// * `Ok(())`      - Every ref head is covered, or one is left for the next walk.
    /// * `Err(String)` - A ref head reaches a head that failed its walk twice; the error names its
    ///   pallet.
    pub fn walk_new_ref_heads_locally(&mut self) -> Result<(), String> {
        'refs: for (pallet, head) in user_refs()? {
            let mut frontier: Vec<String> = vec![head.clone()];
            let mut visited: HashSet<String> = HashSet::new();

            while let Some(hash) = frontier.pop() {
                if self.seen.parcels.contains(&hash) || self.local_parcels.contains(&hash) {
                    continue;
                }

                if self.unwalkable.contains(&hash) {
                    if self.retried.insert(hash.clone()) {
                        self.unwalkable.remove(&hash);
                        continue 'refs;
                    }

                    return Err(unservable(&pallet.to_wire(), &format!(
                        "its history holds parcel {}, which was fetched only part-way and which the \
                        remote cannot serve.", short_hash(&hash)
                    )));
                }

                if visited.insert(hash.clone()) {
                    frontier.extend(object_utils::load_parcel(&hash)?.parents);
                }
            }

            self.local_parcels.extend(visited.iter().cloned());
            self.reached.push((head, visited));
        }

        Ok(())
    }

    /// Under the [`RefLock`], check that nothing changed that the walks missed, and if so write the
    /// pruned ledger and the widened scope, in that order. Only compares and writes inside the
    /// hold.
    ///
    /// It retries when the on-disk scope differs from the one the walk widened (re-widening from
    /// it first), when the ledger holds a present head that was neither walked nor found
    /// unwalkable, or when a ref head lies outside the walked set. An unwalkable ledger head is
    /// dropped. A ledger head whose parcel is still
    /// absent now is dropped: a fetch still in flight re-records it when it sees the new scope. So
    /// is one a walked ref reaches while that ref still stands where it was walked: the ref's own
    /// closure covers it, and a ref write that later leaves it records it again.
    ///
    /// # Returns
    /// * `Ok(ExpandCommit)` - What the commit did.
    /// * `Err(String)`      - If a record could not be read or written.
    pub fn commit(&mut self) -> Result<ExpandCommit, String> {
        let lock = RefLock::acquire()?;
        let current = scope_utils::read_fetch_scope()?;

        if current != self.from {
            // A concurrent expand committed: widen its scope instead. What the walk saw was
            // classified against the old scope, so start the walk again.
            let still: Vec<String> = self.added.iter()
                .filter(|prefix| current.classify(prefix) != ScopeClass::InScope)
                .cloned()
                .collect();

            if still.is_empty() {
                return Ok(ExpandCommit::Covered(current));
            }

            let (pallet, base, fetched_objects) = (self.pallet.clone(), self.base.clone(), self.fetched_objects);
            *self = Expansion::new(&pallet, &base, current, still);
            self.fetched_objects = fetched_objects;

            return Ok(ExpandCommit::Retry);
        }

        let refs = user_refs()?;
        let standing: HashSet<&str> = refs.iter().map(|(_, head)| head.as_str()).collect();
        let covered = |head: &str| self.reached.iter()
            .any(|(root, parcels)| standing.contains(root.as_str()) && parcels.contains(head));

        let mut ledger: Vec<fetch_ledger::LedgerEntry> = Vec::new();

        for entry in fetch_ledger::read_ledger()? {
            if !file_utils::does_object_exist(&entry.head)?
                || self.unwalkable.contains(&entry.head)
                || covered(&entry.head)
            {
                continue;
            }

            if !self.walked_heads.contains(&entry.head) {
                return Ok(ExpandCommit::Retry);
            }

            ledger.push(entry);
        }

        for (_, head) in &refs {
            if !self.seen.parcels.contains(head) && !self.local_parcels.contains(head) {
                return Ok(ExpandCommit::Retry);
            }
        }

        if ledger != fetch_ledger::read_ledger()? {
            fetch_ledger::write_ledger(&lock, &ledger)?;
        }

        scope_utils::set_fetch_scope(&self.widened)?;

        Ok(ExpandCommit::Written(self.widened.clone()))
    }

    /// Walk one head at the widened scope, merging what it saw into the shared sets only if the
    /// whole walk succeeded, so a failed head does not hide parcels from the heads after it.
    /// Returns the parcels the walk added, all of them reached from `head`.
    async fn walk_head(&mut self, client: &RemoteClient, head: &str) -> Result<HashSet<String>, String> {
        let mut seen = WalkSeen::default();
        let stats = fetch_scoped_from(client, head, &self.widened, &[], &self.seen, &mut seen, &Progress::NONE).await?;
        let added = seen.parcels.clone();

        self.seen.absorb(seen);
        self.fetched_objects += stats.fetched_objects;
        self.walked_heads.insert(head.to_string());

        Ok(added)
    }
}

/// The refusal for a closure `scope fetch` cannot widen.
fn unservable(pallet: &str, error: &str) -> String {
    format!(
        "Pallet \"{}\" cannot be fetched at the widened scope: {} Nothing was recorded.",
        pallet, error
    )
}

fn short_hash(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

/// The dedup sets of a scoped walk, kept apart on purpose. A spine node's classification depends
/// on its *path* (the same tree hash at two paths seals different siblings), so spine visits are
/// keyed by (hash, path). An in-scope subtree's whole closure is fetched regardless of where it
/// sits, so it is keyed by hash alone.
#[derive(Default)]
struct WalkSeen {
    parcels: HashSet<String>,
    spine: HashSet<(String, String)>,
    full: HashSet<String>,
    blobs: HashSet<String>,
}

impl WalkSeen {
    fn absorb(&mut self, other: WalkSeen) {
        self.parcels.extend(other.parcels);
        self.spine.extend(other.spine);
        self.full.extend(other.full);
        self.blobs.extend(other.blobs);
    }
}

/// Whether `key` is visited for the first time: in neither the committed `prior` set nor this
/// walk's own `seen`, which records it.
fn first_visit<K: Eq + std::hash::Hash + Clone>(prior: &HashSet<K>, seen: &mut HashSet<K>, key: &K) -> bool {
    !prior.contains(key) && seen.insert(key.clone())
}

/// The path-pruned walk behind [`fetch_history_scoped`] and [`Expansion`]: fetch the full parcel
/// graph and signatures, the tree spine to each in-scope prefix, and the in-scope subtrees and
/// blobs in full, sealing out-of-scope objects by hash. `complete` bounds the parcel walk at
/// closures already known complete at this scope (empty for a full re-walk). `prior` holds what
/// earlier walks at the same scope visited, which this one skips; `seen` collects what this one
/// visits.
async fn fetch_scoped_from(client: &RemoteClient,
                           head: &str,
                           scope: &PathScope,
                           complete: &[String],
                           prior: &WalkSeen,
                           seen: &mut WalkSeen,
                           progress: &Progress) -> Result<FetchStats, String> {
    let mut stats = FetchStats::default();

    progress.status("Fetching history");

    let mut parcel_frontier: Vec<String> = vec![head.to_string()];
    let mut signatures = walk_signatures();

    while !parcel_frontier.is_empty() {
        let candidates: Vec<String> = parcel_frontier.drain(..)
            .filter(|hash| first_visit(&prior.parcels, &mut seen.parcels, hash))
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
        stats.fetched_signatures += fetch_missing_signatures(client, &wave, &mut signatures).await?;
        report_fetch(progress, &stats);

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
            if !first_visit(&prior.spine, &mut seen.spine, &(tree_hash.clone(), path.clone())) {
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
                // it is sealed, unless the classifier holds it in scope: the root ignore file is in
                // every scope, though every declared prefix names a directory.
                if scope.classify(&scope_join(&path, name)) == ScopeClass::InScope
                    && first_visit(&prior.blobs, &mut seen.blobs, &file.hash)
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
                .filter(|hash| first_visit(&prior.full, &mut seen.full, hash))
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
                    if first_visit(&prior.blobs, &mut seen.blobs, &file.hash) {
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
            report_fetch(progress, &stats);
        }
    }

    signatures.finish()?;
    Ok(stats)
}

/// Report how far a history fetch has got. Its total is not known until the walk ends, so the
/// report carries the running counts rather than a percentage.
fn report_fetch(progress: &Progress, stats: &FetchStats) {
    progress.indeterminate(|| format!(
        "{} parcels walked, {} objects fetched", stats.walked_parcels, stats.fetched_objects
    ));
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
    fetch_missing_objects_in_chunks(client, hashes, BATCH_FETCH_CHUNK).await
}

/// [`fetch_missing_objects`], asking for at most `chunk` objects per batch.
async fn fetch_missing_objects_in_chunks(client: &RemoteClient,
                                         hashes: &[String],
                                         chunk_size: usize) -> Result<usize, String> {
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
    // (forklift's packfile moment). A remote without the endpoint, or one too slow to start its
    // answer or to finish it, or one that ran out of time for an answer, closes the batch lane and
    // everything falls back to loose GETs, for this wave and every later one. An answer cut at the
    // remote's byte budget is asked for again; whatever the batches did not deliver (the remote
    // may lack objects) is fetched loose below.
    if missing.len() > 1 {
        'chunks: for chunk in missing.chunks(chunk_size) {
            let mut asked: Vec<String> = chunk.to_vec();

            loop {
                if !client.batch_lane_open() {
                    break 'chunks;
                }

                match client.fetch_batch(&asked).await? {
                    BatchOutcome::Imported(_) => break,
                    // Cut at the remote's byte budget: ask again for what is still missing. A
                    // budget cut carries an object, so each round asks for less; one that carried
                    // nothing new will not make progress, and the rest of this chunk goes loose.
                    // The next chunk is still asked for: it is the one answer that misbehaved.
                    BatchOutcome::Partial(_) => {
                        let mut rest: Vec<String> = Vec::new();

                        for hash in &asked {
                            if !file_utils::does_object_exist(hash)? {
                                rest.push(hash.clone());
                            }
                        }

                        if rest.len() == asked.len() {
                            break;
                        }
                        if rest.is_empty() {
                            break;
                        }

                        asked = rest;
                    }
                    BatchOutcome::OutOfTime(_) | BatchOutcome::EndpointAbsent | BatchOutcome::Degraded => {
                        break 'chunks;
                    }
                }
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
/// (trees don't reference them, so no closure walk ever emits one), so a clone/lower/scope fetch
/// imports the tree+recipe closure and then fetches the in-scope chunks per object here, exactly
/// the "a bundle is an optimization; missing objects fall back to loose GET" contract.
///
/// Deliberately calls [`fetch_loose_objects`] directly rather than [`fetch_missing_objects`] —
/// chunks always fetch one presigned `GET` each, **never** through `POST /v1/objects/batch`, no
/// matter how many are missing (DESIGN.html §9.4b: "clone, lower and scope fetch all fetch chunks
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

/// Read the given objects concurrently, one GET each, into memory: each verified against its hash
/// and none stored.
///
/// # Returns
/// * `Ok(Vec<(String, Vec<u8>)>)` - Each hash with its bytes, in no particular order.
/// * `Err(String)`                - If a transfer failed or the remote answered with other bytes.
pub async fn read_objects_verified(client: &RemoteClient,
                                   hashes: &[String]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let semaphore = Arc::new(Semaphore::new(CONCURRENT_TRANSFERS));
    let mut tasks: JoinSet<Result<(String, Vec<u8>), String>> = JoinSet::new();

    for hash in hashes {
        let client = client.clone();
        let hash = hash.clone();
        let semaphore = Arc::clone(&semaphore);

        tasks.spawn(async move {
            let _permit = semaphore.acquire().await
                .map_err(|_| "The transfer pool was closed unexpectedly.".to_string())?;

            let bytes = client.fetch_object(&hash).await
                .map_err(|e| format!("Object {} could not be read from the remote: {}", hash, e))?;
            object_utils::verify_object_bytes(&hash, &bytes)
                .map_err(|_| format!("The remote answered object {} with other bytes.", hash))?;

            Ok((hash, bytes))
        });
    }

    join_all(tasks).await
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

/// Fetch (concurrently) the signatures of the given parcels that carry none locally, into
/// `signatures`, which the walk publishes as signature tables. Unsigned parcels (no signature on
/// the remote either) are fine.
///
/// # Returns
/// * `Ok(usize)`   - How many signatures were fetched.
/// * `Err(String)` - If a transfer failed, or a fetched signature could not be stored.
async fn fetch_missing_signatures(client: &RemoteClient,
                                  parcel_hashes: &[String],
                                  signatures: &mut sign_utils::SignatureBatch) -> Result<usize, String> {
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
    let mut tasks: JoinSet<Result<Option<(String, Vec<u8>)>, String>> = JoinSet::new();

    for hash in wanted {
        let client = client.clone();
        let semaphore = Arc::clone(&semaphore);

        tasks.spawn(async move {
            let _permit = semaphore.acquire().await
                .map_err(|_| "The transfer pool was closed unexpectedly.".to_string())?;

            Ok(client.fetch_signature(&hash).await?.map(|bytes| (hash, bytes)))
        });
    }

    let mut fetched = 0;
    for (hash, bytes) in join_all(tasks).await?.into_iter().flatten() {
        signatures.add(hash, bytes)?;
        fetched += 1;
    }

    Ok(fetched)
}

/// Lift one pallet: negotiate the missing objects, upload them (and the new parcels'
/// signatures) in parallel, then move the remote ref with a CAS.
///
/// # Arguments
/// * `client`      - The remote.
/// * `pallet`      - The pallet name on the remote.
/// * `local_head`  - The local head parcel of the pallet.
/// * `remote_head` - The remote's current head of the pallet (from the handshake).
/// * `tombstone`   - With no `remote_head`, the head of the remote's tombstone of the pallet, which
///                   the creation acknowledges (see [`acknowledged_tombstone`]).
/// * `info`        - The handshake: the remote's other heads, which bound the walk, and whether it
///                   takes chunked files.
/// * `progress`    - Where to report the lift's phases and how many objects have uploaded.
///
/// # Returns
/// * `Ok(LiftResult)` - Up to date, or the transfer stats.
/// * `Err(String)`    - If the remote is ahead/diverged, or a transfer failed.
pub async fn lift_pallet(client: &RemoteClient,
                         pallet: &str,
                         local_head: &str,
                         remote_head: Option<&str>,
                         tombstone: Option<&str>,
                         info: &WarehouseInfo,
                         progress: &Progress) -> Result<LiftResult, String> {
    lift_pallet_inner(client, pallet, local_head, remote_head, tombstone, false, info, progress).await
}

/// The tombstone a lift of user pallet `pallet` from `local_head` acknowledges, when the remote
/// (`info`) has the pallet only as a tombstone: its head, once `recreate` asks for the re-creation
/// and the local work descends from that head. Otherwise the lift is refused before anything is
/// uploaded, naming the step that fits the local work.
///
/// # Returns
/// * `Ok(Some(String))` - The tombstone head the creation acknowledges.
/// * `Ok(None)`         - The remote lists the pallet, or has no tombstone of it.
/// * `Err(String)`      - The [`RefusalCode::PalletDeletedOnRemote`] refusal, or why the local
///                        history could not be read.
pub fn acknowledged_tombstone(info: &WarehouseInfo,
                              pallet: &str,
                              local_head: &str,
                              recreate: bool) -> Result<Option<String>, String> {
    if info.pallets.contains_key(pallet) {
        return Ok(None);
    }

    let Some(head) = info.tombstone(pallet) else {
        return Ok(None);
    };

    let descends = head == local_head
        || (file_utils::does_object_exist(head)? && merge_utils::is_ancestor(head, local_head)?);

    if descends && recreate {
        return Ok(Some(head.clone()));
    }

    let drop = format!(
        "To drop the local pallet instead, shift to another pallet and run \"forklift pallet \
        delete {}\".",
        pallet
    );
    let (message, next_step) = match descends {
        true => (format!(
            "The remote's pallet \"{}\" was deleted at {}, and the name stays reserved for that \
            history. This warehouse's work descends from it: \"forklift lift --recreate\" re-creates \
            the pallet with it, acknowledging the deletion. {} Nothing was lifted.",
            pallet, head, drop
        ), "forklift lift --recreate".to_string()),
        false => (format!(
            "The remote's pallet \"{}\" was deleted at {}, and the name stays reserved for that \
            history, which this warehouse's work does not descend from{}. \"forklift pallet restore \
            {} --remote\" brings the pallet back on the remote; lower it and consolidate \
            it into this work, then lift. {} Nothing was lifted.",
            pallet, head,
            if recreate { ", so \"--recreate\" cannot re-create it" } else { "" },
            pallet, drop
        ), format!("forklift pallet restore {} --remote", pallet)),
    };

    Err(CoreError::refusal(RefusalCode::PalletDeletedOnRemote, message, next_step).into())
}

/// `lift_pallet`, allowing one sanctioned non-descendant update: the office lift right
/// after a re-genesis (§8.7), where the new chain replaces — rather than extends — the
/// remote's office head that the local anchor adopted. The server enforces the same
/// exception narrowly on its side.
async fn lift_pallet_inner(client: &RemoteClient,
                           pallet: &str,
                           local_head: &str,
                           remote_head: Option<&str>,
                           tombstone: Option<&str>,
                           adopted_reset: bool,
                           info: &WarehouseInfo,
                           progress: &Progress) -> Result<LiftResult, String> {
    if remote_head == Some(local_head) {
        return Ok(LiftResult::UpToDate);
    }

    if let Some(remote_head) = remote_head {
        if !file_utils::does_object_exist(remote_head)? {
            return Err(pallet_utils::remote_ahead_refusal(format!(
                "The remote's pallet \"{}\" has parcels this warehouse does not know \
                (head {}). \"forklift lower\" first.",
                pallet, remote_head
            )).into());
        }

        if !adopted_reset && !merge_utils::is_ancestor(remote_head, local_head)? {
            // Known locally but not lowered: the remote is ahead when it already holds everything
            // here, and diverged otherwise. The walk runs through the remote's history, which may
            // be only partly here (a head fetched before its ancestors) — or a local object may be
            // unreadable. The two cannot be told apart, so a walk that cannot finish classifies
            // nothing: it says what it could not decide, and that lowering brings the history in.
            return Err(match merge_utils::is_ancestor(local_head, remote_head) {
                Ok(true) => pallet_utils::remote_ahead_refusal(format!(
                    "The remote's pallet \"{}\" is ahead of this warehouse's (head {}). \"forklift lower\" \
                    first.",
                    pallet, remote_head
                )),
                Ok(false) => diverged_refusal(pallet, remote_head),
                Err(e) => CoreError::Other(format!(
                    "Could not tell whether the remote's pallet \"{}\" (head {}) is ahead of this \
                    warehouse's or diverged from it: {}. \"forklift lower\" brings in the remote's history, \
                    if that is what is missing.",
                    pallet, remote_head, e
                )),
            }.into());
        }
    }

    let (new_parcels, uploaded_objects, uploaded_signatures) =
        // An acknowledged tombstone's head is held on the remote (a gc root), so a re-creation
        // uploads only the work past it.
        upload_history(client, local_head, remote_head.or(tombstone),
                       &held_elsewhere(info, pallet), info.chunking, progress).await?;

    client.update_ref(pallet, remote_head, local_head, tombstone).await?;

    // After the compare-and-set, so a refused or failed lift records nothing; best-effort, since
    // the remote has the head now and an error here would read as a lift that did not land.
    let unrecorded = held_heads::record_held_head(pallet, local_head).err();

    Ok(LiftResult::Lifted(LiftStats {
        new_parcels,
        uploaded_objects,
        uploaded_signatures,
        old_head: remote_head.map(|hash| hash.to_string()),
        unrecorded,
    }))
}

/// The [`RefusalCode::Diverged`] refusal for `pallet` (its wire name), whose local head and the
/// remote's `remote_head` both hold parcels the other lacks; the remote's are already local. A
/// working pallet merges the remote head in through a pallet of its own, which `lower` refuses to
/// do implicitly; a meta pallet with replay rules is reconciled by `lower` itself. The office and
/// any other meta pallet have no merge, so theirs is an unclassified error: no command fixes it.
pub fn diverged_refusal(pallet: &str, remote_head: &str) -> CoreError {
    let short = &remote_head[..12.min(remote_head.len())];

    if let Some(name) = pallet.strip_prefix(pallet_utils::META_QUALIFIER) {
        if meta_tree::meta_rules(name).is_none() {
            return CoreError::Other(format!(
                "The local pallet \"{}\" and the remote have diverged, and it has no automatic \
                merge: reconcile the two histories by hand before lifting.",
                pallet
            ));
        }

        return CoreError::refusal(
            RefusalCode::Diverged,
            format!(
                "The local pallet \"{}\" and the remote have diverged. \"forklift lower\" reconciles it; \
                then lift again.",
                pallet
            ),
            "forklift lower",
        );
    }

    CoreError::refusal(
        RefusalCode::Diverged,
        format!(
            "The local pallet \"{p}\" and the remote have diverged. The remote parcels are \
            fetched; make the remote head its own pallet (\"forklift pallet create {p}-remote-{s} {s}\", \
            which moves this bay onto it), shift back (\"forklift shift {p}\"), consolidate it \
            (\"forklift consolidate {p}-remote-{s}\"), and lift the result.",
            p = pallet, s = short
        ),
        format!(
            "forklift pallet create {p}-remote-{s} {s}; then forklift shift {p}; then forklift \
            consolidate {p}-remote-{s}; then forklift lift",
            p = pallet, s = short
        ),
    )
}

/// The other heads a user pallet's lift stops its parcel walk at ([`new_parcels_parents_first`]):
/// the remote's other live user pallet heads whose parcel is present here. A meta pallet's history
/// never branches off a user pallet's, so it gets none. A tombstone's head is left out: dropping
/// one is an operator's routine cleanup, so stopping at it would make a lift refused for history
/// the remote no longer roots far likelier than a live head's deletion would (either fails closed,
/// at the remote's closure check). Like the stop itself, this is only a shortcut, so a head whose
/// presence cannot be read is left out.
fn held_elsewhere(info: &WarehouseInfo, pallet: &str) -> Vec<String> {
    let mut heads = Vec::new();

    if pallet.starts_with(pallet_utils::META_QUALIFIER) {
        return heads;
    }

    for (name, head) in &info.pallets {
        if name != pallet && !name.starts_with(pallet_utils::META_QUALIFIER) && !heads.contains(head)
            && file_utils::does_object_exist(head).unwrap_or(false) {
            heads.push(head.clone());
        }
    }

    heads
}

/// Upload the history `local_head` reaches and the remote does not — every parcel past
/// `remote_head` and the heads in `elsewhere` (all of them when there are none), its objects and
/// its signatures — without moving any ref. [`lift_pallet_inner`] follows it with the ref update;
/// [`push_local_trust`] uses it alone for a re-genesis anchor's adopted office history, which no
/// ref on the remote names.
///
/// # Returns
/// * `Ok((usize, usize, usize))` - The new parcels, uploaded objects and uploaded signatures.
/// * `Err(String)`               - If the history could not be read or a transfer failed.
async fn upload_history(client: &RemoteClient,
                        local_head: &str,
                        remote_head: Option<&str>,
                        elsewhere: &[String],
                        chunking_supported: bool,
                        progress: &Progress) -> Result<(usize, usize, usize), String> {
    // The new parcels the remote does not have, ordered parents-first — see
    // `new_parcels_parents_first`, which owns both the walk and the order the candidate
    // loop below depends on.
    progress.status("Finding objects to lift");

    let new_parcels = new_parcels_parents_first(local_head, remote_head, elsewhere)?;

    // Candidate objects for the negotiation: each new parcel's tree, walked against
    // its parents' trees — a subtree identical to *any* parent's at the same path is
    // skipped whole, the same skip the merge walk and the pallet diff use. A one-line
    // change on a 100k-file warehouse thus negotiates the changed path, not the full
    // closure.
    let mut candidates: Vec<String> = new_parcels.clone();
    let mut seen_trees: HashSet<String> = HashSet::new();
    let mut seen_blobs: HashSet<String> = HashSet::new();
    let mut seen_recipes: HashSet<String> = HashSet::new();

    // Parents first, guaranteed by `new_parcels_parents_first`: every parent of the parcel
    // being walked is remote-known or already processed, so everything a base "explains" is
    // on the remote or in the candidates already. Do not reorder this loop — a base that
    // explains a subtree of a parcel not yet processed drops that subtree from the candidate
    // set, and the push then fails the server's closure audit identically on every retry.
    for parcel_hash in &new_parcels {
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
    progress.status("Uploading objects");

    let session = new_lift_session();
    let uploaded_objects = negotiate_and_upload(
        client, &session, &candidates, &control_plane, chunking_supported, progress,
    ).await?;

    progress.status("Updating the remote");

    // The signatures of the new parcels travel with them.
    let mut uploaded_signatures = 0usize;

    for parcel_hash in &new_parcels {
        if let Some(bytes) = sign_utils::load_raw_parcel_signature(parcel_hash)? {
            client.upload_signature(parcel_hash, bytes).await?;
            uploaded_signatures += 1;
        }
    }

    Ok((new_parcels.len(), uploaded_objects, uploaded_signatures))
}

/// The new parcels of a lift — everything reachable from the local head that the remote does
/// not already have — **ordered parents-first**.
///
/// The walk stops at the remote head and any ancestor of it (a merge's other side rejoins below the
/// remote head), so a linear lift touches O(new parcels) and a merge never re-walks the shared
/// slice. The remote holds the whole closure of every head it lists, not only the lifted pallet's,
/// so the walk also stops at every head in `elsewhere`, and on a first lift (no remote head) at
/// their ancestors too: a new pallet branched off another one stops where it branched instead of
/// walking, negotiating and uploading the whole history, and work that merged in another pallet's
/// head stops there. Pruning at every ancestor — not just the head hashes — is also what keeps a
/// partial warehouse liftable: an interior parcel the remote already has may carry content this
/// warehouse never fetched (out of a sparse scope, or held by a cache only by hash), and re-walking
/// it would try to load it. The remote provably has it, so it is correctly never uploaded and never
/// walked.
///
/// The order is the caller's whole reason for this being one function. The candidate walk
/// prunes each parcel's subtrees against its parents' trees, and it may only do so once those
/// parents are on the remote or already in the candidate set — otherwise a base "explains"
/// content that is nowhere, the content is never negotiated or uploaded, and the push fails
/// the server's closure audit **identically on every retry**, wedging the lift fail-closed.
///
/// The order comes from a topological sort of the walked set against **its own** parent edges,
/// which the walk has already loaded. Only parents inside the set constrain anything: a parent
/// outside it is on the remote already, so it explains content that is provably there.
///
/// Deliberately *not* a sort by commit-graph generation number, though that is also
/// topological and was this fix's first form. Reading generations calls `graph_utils::ensure`,
/// which walks below the lifted set to the first parcel carrying a stored record, computes a
/// record for every parcel on the way, and persists the lot. On a lift with no held head to stop
/// at — a first lift into an empty remote, where `is_ancestor` never runs — that is a whole
/// commit-graph build and shard flush newly attached to the lift path. Ordering the walked set
/// against its own edges costs no object read the walk has not already paid for and writes nothing.
///
/// What that choice is *not* is a fix for absent ancestors, and the tempting version of this
/// comment said otherwise. See
/// `an_absent_ancestor_fails_the_walk_itself_whatever_the_order_is`: the walk already requires
/// the remote head's whole ancestry to be readable, because `is_ancestor` can only answer
/// *false* by exhausting it. A sealed-absent ancestor fails the lift under either ordering, and
/// it fails before any ordering happens.
///
/// The walk itself is a stack-driven DFS, and reversing a DFS preorder is *not* a topological
/// order — an unequal-branch merge (two parents sharing a grandparent, one branch longer) puts
/// a parcel before its own parent. That was FORK-109; the fix is the order, not the walk.
///
/// # Arguments
/// * `local_head`  - The local head parcel of the pallet.
/// * `remote_head` - The remote's current head, if the pallet exists there.
/// * `elsewhere`   - The remote's other held heads that are present here: see [`held_elsewhere`].
///
/// # Returns
/// * `Ok(Vec<String>)` - The new parcels, every parcel preceded by all of its in-set parents.
/// * `Err(String)`     - If a parcel could not be read, or the walked set contains a cycle.
fn new_parcels_parents_first(local_head: &str,
                             remote_head: Option<&str>,
                             elsewhere: &[String]) -> Result<Vec<String>, String> {
    let mut new_parcels: Vec<String> = Vec::new();
    let mut parents_of: HashMap<String, Vec<String>> = HashMap::new();
    let mut queue: Vec<String> = vec![local_head.to_string()];
    let mut visited: HashSet<String> = HashSet::new();

    while let Some(hash) = queue.pop() {
        if Some(hash.as_str()) == remote_head || elsewhere.contains(&hash) || !visited.insert(hash.clone()) {
            continue;
        }

        if let Some(remote_head) = remote_head {
            if merge_utils::is_ancestor(&hash, remote_head)? {
                continue;
            }
        }

        // Another head's ancestors only on a first lift, which has no head of its own to stop
        // at: on every other lift that would cost an ancestry walk per parcel per head, and a
        // merged-in head still stops the walk by its hash above. Only a shortcut: a head whose
        // ancestry cannot be read here answers "not known", and the parcel is walked.
        if remote_head.is_none()
            && elsewhere.iter().any(|head| merge_utils::is_ancestor(&hash, head).unwrap_or(false)) {
            continue;
        }

        let parcel = object_utils::load_parcel(&hash)?;

        queue.extend(parcel.parents.iter().cloned());
        parents_of.insert(hash.clone(), parcel.parents);
        new_parcels.push(hash);
    }

    // Kahn's algorithm over the walked set. `pending` counts only in-set parents, so a parcel
    // whose parents are all on the remote is ready immediately. `ready` is a BTreeSet rather
    // than a stack so the emitted order is a function of the DAG alone — two runs over the same
    // history produce the same order, which keeps a failure reproducible.
    let in_set: HashSet<&str> = new_parcels.iter().map(|hash| hash.as_str()).collect();
    let mut pending: HashMap<&str, usize> = HashMap::new();
    let mut children_of: HashMap<&str, Vec<&str>> = HashMap::new();

    for hash in &new_parcels {
        let in_set_parents = parents_of[hash].iter()
            .filter(|parent| in_set.contains(parent.as_str()));

        let mut count = 0;
        for parent in in_set_parents {
            children_of.entry(parent.as_str()).or_default().push(hash.as_str());
            count += 1;
        }
        pending.insert(hash.as_str(), count);
    }

    let mut ready: std::collections::BTreeSet<&str> = pending.iter()
        .filter(|(_, count)| **count == 0)
        .map(|(hash, _)| *hash)
        .collect();

    let mut ordered: Vec<String> = Vec::with_capacity(new_parcels.len());

    while let Some(hash) = ready.pop_first() {
        ordered.push(hash.to_string());

        for child in children_of.get(hash).into_iter().flatten() {
            let count = pending.get_mut(*child).expect("every walked parcel has a pending count");
            *count -= 1;
            if *count == 0 {
                ready.insert(child);
            }
        }
    }

    if ordered.len() != new_parcels.len() {
        return Err(format!(
            "The parcel graph has a cycle among the {} parcels being lifted (only {} could be \
             ordered after their parents); the warehouse is corrupt.",
            new_parcels.len(), ordered.len()
        ));
    }

    Ok(ordered)
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

/// Load one object's bytes and apply the transport-ceiling refusal, off the async runtime:
/// `retrieve_object_by_hash` is synchronous pack/loose-object I/O
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
    /// Test-only seam (the falsifier for the `spawn_blocking` move above):
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

/// Upload (concurrently) the objects of the given hashes, calling `on_uploaded` once per object
/// as each upload finishes.
async fn upload_objects(client: &RemoteClient,
                        hashes: &[String],
                        on_uploaded: &mut dyn FnMut()) -> Result<(), String> {
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

    join_all_with(tasks, on_uploaded).await.map(|_| ())
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
///
/// `progress` hears each finished upload against the total the negotiation settled on, and a
/// status when the staged commit starts.
async fn negotiate_and_upload(client: &RemoteClient,
                              session: &str,
                              candidates: &[String],
                              control_plane: &HashSet<String>,
                              chunking_supported: bool,
                              progress: &Progress) -> Result<usize, String> {
    let Some(negotiation) = client.upload_targets(session, candidates).await? else {
        // An older remote with no `upload-targets`: negotiate the missing set and PUT each body
        // to the control plane, exactly as before. No staging, no commit batching — each object is
        // verified inline on its own PUT, so the pagination gate below does not apply here.
        let missing = client.missing_objects(candidates).await?;
        upload_objects(client, &missing, &mut upload_tally(progress, missing.len())).await?;
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
    let mut tally = upload_tally(progress, negotiation.direct.len() + negotiation.targets.len());
    upload_objects(client, &negotiation.direct, &mut tally).await?;
    upload_to_targets(client, &negotiation.targets, &mut tally).await?;

    // Only a staging head hands back targets; a direct head's are empty and it needs no commit
    // (every `direct` PUT was verified inline). When there was staging, the commit verifies and
    // promotes it before the ref update — nothing staged is fetchable until then.
    if !negotiation.targets.is_empty() {
        let (control, blobs) = classify_staged(&negotiation.targets, control_plane);
        progress.status("Committing staged objects");
        commit_staged_session(client, session, &control, &blobs).await?;
    }

    Ok(negotiation.direct.len() + negotiation.targets.len())
}

/// A per-upload callback reporting `done/total` into `progress` — one tally shared by every
/// upload batch of a lift, so its percentage runs once from 0 to 100.
fn upload_tally(progress: &Progress, total: usize) -> impl FnMut() + '_ {
    let mut done = 0usize;
    move || {
        done += 1;
        progress.fraction(done, total, || format!("{}/{} objects", done, total));
    }
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
/// fan-out the fetch and direct-upload paths use ([`CONCURRENT_TRANSFERS`]) — calling
/// `on_uploaded` once per object as each upload finishes.
async fn upload_to_targets(client: &RemoteClient,
                           targets: &BTreeMap<String, String>,
                           on_uploaded: &mut dyn FnMut()) -> Result<(), String> {
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

    join_all_with(tasks, on_uploaded).await.map(|_| ())
}

/// Commit a staged lift session, paginating the hash lists and committing each batch with
/// [`commit_one_batch`], which retries until a deadline rather than hanging on a stuck verifier.
///
/// A lift touching a maximal chunked file lists too many chunk hashes for one request (Lambda's
/// ~6 MB synchronous body), so `control_plane`/`blobs` are paginated at [`MAX_MISSING_BATCH`] and
/// every batch but the last carries `more: true`. The head verifies/presence-checks each batch but
/// gates its session-wide staging sweep on the final (`more: false`) batch, so an early batch never
/// discards chunks a later batch still needs. A small lift is one batch (`more: false`).
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

/// Commit one paginated batch, retrying every answer but a terminal refusal until one deadline:
/// connect + [`Budgets::commit_deadline`], armed when the batch starts. Every attempt, backoff
/// sleep and `Retry-After` wait spends from it, so against a remote that never answers, or answers
/// and then wedges, the batch fails within that deadline whatever the attempt count.
///
/// An attempt starts only while more than connect + [`Budgets::min_commit_attempt`] remains, and
/// gets at most connect + [`Budgets::commit_attempt_allowance`] of it, so no attempt outlives the
/// deadline and each can still outlive its connect phase. A wait that would leave no more than that
/// is not taken: the batch fails at once instead. Retrying is safe because the commit is idempotent
/// and changes no ref: promotion resumes where an interrupted attempt stopped. A `more` batch never
/// sweeps.
async fn commit_one_batch(client: &RemoteClient,
                          session: &str,
                          control_plane: &[String],
                          blobs: &[String],
                          more: bool) -> Result<(), String> {
    let budgets = client.budgets;
    let window = budgets.connect + budgets.commit_deadline;
    let deadline = tokio::time::Instant::now() + window;
    let floor = budgets.connect + budgets.min_commit_attempt;
    let mut delay = RETRY_BACKOFF_START;
    let mut last_answer: Option<CommitAttempt> = None;
    let mut last_unanswered: Option<CommitAttempt> = None;
    let mut unanswered = 0;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());

        if remaining <= floor {
            break;
        }

        let budget = remaining.min(budgets.connect + budgets.commit_attempt_allowance);
        let attempt = client.commit_lift(session, control_plane, blobs, more, budget).await?;

        let wait = match &attempt {
            CommitAttempt::Committed => return Ok(()),
            CommitAttempt::Throttled { retry_after } => throttle_wait(*retry_after, delay),
            _ => delay,
        };
        delay = (delay * 2).min(RETRY_BACKOFF_CAP);

        if attempt.answered() {
            last_answer = Some(attempt);
        } else {
            unanswered += 1;
            last_unanswered = Some(attempt);
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());

        if remaining.saturating_sub(wait) <= floor {
            break;
        }

        tokio::time::sleep(wait).await;
    }

    Err(commit_deadline_message(last_answer.as_ref(), last_unanswered.as_ref(), unanswered, window))
}

/// What a commit batch that ran out of time reports. The headline is the last *answered* attempt's,
/// so a final attempt cut short by the deadline cannot change it, and no message claims the upload
/// was verified or will be: a staged copy may never be promoted. Each says the remote's pallet was
/// not moved, which holds because the ref update only follows a commit.
fn commit_deadline_message(last_answer: Option<&CommitAttempt>,
                           last_unanswered: Option<&CommitAttempt>,
                           unanswered: usize,
                           window: std::time::Duration) -> String {
    let unchanged = "The remote's pallet was not moved";
    let trailer = match unanswered {
        0 => String::new(),
        1 => " (1 attempt got no answer.)".to_string(),
        n => format!(" ({} attempts got no answer.)", n),
    };

    let missed = format!("the commit did not go through within its {:?} deadline", window);

    let headline = match last_answer {
        Some(CommitAttempt::NotReady) => format!(
            "{}; the remote's last answer was that it still holds a staged upload of this lift it \
            has not accepted into the warehouse. {}. Running the lift again uploads it afresh and \
            usually clears this; if it keeps happening, the remote's staging verifier is failing \
            and its operator needs to know.",
            missed, unchanged
        ),
        Some(CommitAttempt::Throttled { retry_after }) => format!(
            "{}; the remote's last answer was 429 (rate limited){}. {}; running the lift again is safe.",
            missed,
            retry_after.filter(|wait| wait.as_secs() > 0)
                .map(|wait| format!(", asking for a wait of {}s", wait.as_secs())).unwrap_or_default(),
            unchanged
        ),
        Some(CommitAttempt::Undetermined { status }) => format!(
            "{}; the remote's last answer was {}. {}; running the lift again is safe.",
            missed, status, unchanged
        ),
        _ => {
            let last = match last_unanswered {
                Some(CommitAttempt::NotSent { detail }) | Some(CommitAttempt::Lost { detail }) => detail.as_str(),
                _ => "no attempt could start",
            };

            return format!(
                "Timed out while committing the lift session: no attempt got an answer within {:?}; \
                the last: {}. {}; running the lift again is safe.",
                window, last, unchanged
            );
        }
    };

    format!("Timed out while committing the lift session: {}{}", headline, trailer)
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
/// every coordinator in this module — fetching objects, fetching signatures, and both
/// upload paths — routes through this one implementation instead of each hand-rolling its own
/// copy of this loop.
///
/// The coordinating call must not return while a sibling task's body may still be writing
/// (`object_utils::store_object_bytes` has no await between reading the fetched bytes and
/// finishing the write; fetched signatures are written by the walk, not by a task) — see
/// [`drain_remaining`],
/// which this delegates to on the error path. Any panic evidence [`drain_remaining`] observed
/// while draining is appended to the first error below, never substituted for it.
///
/// This guarantee holds only while the returned future is polled to completion: wrapping this
/// call in `tokio::time::timeout` or a `select!` branch can drop the `JoinSet` without draining
/// it, reinstating the race this function exists to close.
async fn join_all<T: Send + 'static>(tasks: JoinSet<Result<T, String>>) -> Result<Vec<T>, String> {
    join_all_with(tasks, &mut || {}).await
}

/// [`join_all`], calling `on_done` once for each task that succeeds, as it is joined — the hook
/// the upload paths count progress through.
async fn join_all_with<T: Send + 'static>(mut tasks: JoinSet<Result<T, String>>,
                                          on_done: &mut dyn FnMut()) -> Result<Vec<T>, String> {
    let mut results: Vec<T> = Vec::new();

    while let Some(result) = tasks.join_next().await {
        match result.map_err(|e| format!("A transfer task failed: {}", e)).and_then(|inner| inner) {
            Ok(value) => {
                on_done();
                results.push(value);
            }
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

/// Judge the remote's trust from its handshake alone, writing and fetching nothing: the pin and a
/// stripped anchor, an anchor without the office it governs, and an anchor other than the one held
/// here. Every refusal `prepare_remote_trust` raises before its first transfer comes from here, so a
/// caller that refuses for another reason first (a pallet the remote lacks) still refuses an
/// untrusted remote as such.
///
/// # Arguments
/// * `client` - The remote.
/// * `info`   - The remote's handshake.
/// * `expect` - The pinned genesis (`--expect`), if any.
///
/// # Returns
/// * `Ok(())`      - The remote's trust would be adopted or already matches.
/// * `Err(String)` - The refusal `prepare_remote_trust` would raise.
pub fn check_remote_trust(client: &RemoteClient, info: &WarehouseInfo, expect: Option<&str>) -> Result<(), String> {
    let local = office_utils::read_trust_anchor()?;
    judge_remote_trust(client.url(), info, local.as_ref(), expect).map(|_| ())
}

/// The read-only trust checks shared by `check_remote_trust` and `prepare_remote_trust`.
///
/// # Returns
/// * `Ok(Some(head))` - The remote holds an anchor this warehouse may adopt or already holds; its
///                      office head.
/// * `Ok(None)`       - The remote holds no anchor (and none is held or pinned here).
/// * `Err(String)`    - A `trust_changed` refusal, or a remote anchor without its office.
fn judge_remote_trust<'a>(url: &str,
                          info: &'a WarehouseInfo,
                          local: Option<&office_utils::TrustAnchor>,
                          expect: Option<&str>) -> Result<Option<&'a String>, String> {
    trust_utils::check_handshake_trust(url, info, local.map(|anchor| anchor.genesis.as_str()), expect)?;

    let Some(remote_trust) = &info.trust else {
        return Ok(None);
    };

    let remote_office_head = info.pallets.get(&office_utils::office_wire_key())
        .ok_or("The remote has a trust anchor but no office pallet; it is corrupt.".to_string())?;

    if let Some(local) = local {
        if local.genesis != remote_trust.genesis {
            // A re-genesis (§8.7) is a conspicuous trust reset: never adopted silently.
            return Err(trust_utils::anchor_changed(url, &local.genesis, remote_trust, "Refusing to sync.").into());
        }
    }

    Ok(Some(remote_office_head))
}

/// What the trust/office synchronization of a lower/clone did.
#[derive(Default)]
pub struct TrustSyncStats {
    /// The genesis of the remote's trust anchor, when it was adopted locally (first contact).
    pub adopted_genesis: Option<String>,

    /// Whether the local office pallet moved to the remote's head.
    pub office_moved: bool,

    /// The transfer stats of the office history fetch.
    pub fetch: FetchStats,
}

/// The remote's trust state, judged, fetched and verified, with nothing written yet: what
/// [`prepare_remote_trust`] found and [`PreparedTrust::commit`] adopts.
///
/// The office the remote serves is verified against the anchor before anything is written — the
/// held one, or on first contact the one the remote serves (already checked against any pin). On
/// first contact nothing is installed until the caller has verified the pallet history it takes
/// against that anchor ([`PreparedTrust::take_trust`]), so a refusal leaves no anchor held.
pub struct PreparedTrust {
    /// The genesis to announce once installed (first contact only).
    adopting: Option<TrustAnchorDto>,

    /// The held anchor's trust-file bytes, which the office write is conditioned on.
    held_bytes: Option<Vec<u8>>,

    /// The anchor the office was verified against.
    anchor: Option<office_utils::TrustAnchor>,

    /// The office state at the head the office stands at once committed.
    office_state: Option<office_utils::OfficeState>,

    /// The local office head, and where it moves to (`None`: it stays).
    local_office_head: Option<String>,
    new_office: Option<String>,

    /// The transfer stats of the office history fetch.
    fetch: FetchStats,

    /// At first contact, every local user-pallet head judged against the anchor
    /// ([`PreparedTrust::judge_local_heads`]); the install stands only while the user pallets
    /// hold no other.
    judged_heads: Option<Vec<String>>,
}

/// Judge, fetch and verify the remote's trust state for a lower or clone, writing nothing.
/// A remote whose anchor differs from the local one is refused — that is either tampering or an
/// unrelated warehouse, and no data is worth guessing which. So is a remote whose anchor was
/// stripped, and one that is not the `expect` pin; both are checked before anything is fetched.
/// The remote's office is then fetched and verified against the anchor it will stand under.
///
/// # Arguments
/// * `client` - The remote.
/// * `info`   - The remote's handshake.
/// * `expect` - The pinned genesis (`--expect`), if any.
///
/// # Returns
/// * `Ok(PreparedTrust)` - What to adopt; [`PreparedTrust::commit`] writes it.
/// * `Err(String)`       - A `trust_changed` refusal on an anchor mismatch, a stripped anchor or a
///                         missed pin, an office chain that does not verify, diverged offices, or a
///                         failed transfer.
pub async fn prepare_remote_trust(client: &RemoteClient,
                                  info: &WarehouseInfo,
                                  expect: Option<&str>) -> Result<PreparedTrust, String> {
    // Read once: the pin, the stripped-anchor check and the mismatch check below all judge the
    // same held anchor, and the office write is conditioned on its bytes.
    let local_read = office_utils::read_trust_anchor_with_bytes()?;
    let local = local_read.as_ref().map(|(anchor, _)| anchor);

    let mut prepared = PreparedTrust {
        adopting: None,
        held_bytes: local_read.as_ref().map(|(_, bytes)| bytes.clone()),
        anchor: local.cloned(),
        office_state: None,
        local_office_head: None,
        new_office: None,
        fetch: FetchStats::default(),
        judged_heads: None,
    };

    let Some(remote_office_head) = judge_remote_trust(client.url(), info, local, expect)? else {
        return Ok(prepared);
    };
    let Some(remote_trust) = &info.trust else {
        return Ok(prepared);
    };

    prepared.fetch = fetch_history(client, remote_office_head, &Progress::NONE).await?;
    // Before the office head moves: an anchor is only ever held with its adopted history. A held
    // anchor was adopted with it already, so only an anchor about to be written needs the walk.
    if local.is_none() {
        fetch_adopted_history(client, remote_trust).await?;
    }

    // Where the office moves, if anywhere.
    let local_office_head = pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME)?;

    let new_office = match &local_office_head {
        None => Some(remote_office_head.clone()),
        Some(local_head) if local_head == remote_office_head => None,
        Some(local_head) if merge_utils::is_ancestor(local_head, remote_office_head)? => Some(remote_office_head.clone()),
        // The local office is ahead: the next lift pushes it.
        Some(local_head) if merge_utils::is_ancestor(remote_office_head, local_head)? => None,
        Some(_) => {
            return Err(
                "The local and remote office histories have diverged. This can be two \
                admins changing the office concurrently — or tampering. The office has no \
                automatic merge yet (its records interdepend), so it is kept linear: \
                reconcile the two office chains by hand before syncing.".to_string()
            );
        }
    };

    // Verified before either write: an office the anchor does not vouch for is never installed,
    // and never becomes the office a held anchor's verdicts are judged against.
    let anchor = match local {
        Some(local) => local.clone(),
        None => remote_trust.to_anchor(),
    };
    let office_head = new_office.as_deref().or(local_office_head.as_deref())
        .expect("an office the remote serves is either taken or already local");
    let office_state = audit_utils::verify_office_chain_memoized(&anchor, office_head).map_err(|error| String::from(CoreError::refusal(
        RefusalCode::HistoryUnverified,
        format!(
            "The office history {} of {} does not verify against {}: {} Nothing was taken.",
            office_head, client.url(),
            match local { Some(_) => "the trust anchor this warehouse holds", None => "the trust anchor it serves" },
            String::from(error)
        ),
        "Run \"forklift audit\" where this history came from, and take it only once that passes.",
    )))?;

    prepared.adopting = local.is_none().then(|| remote_trust.clone());
    prepared.anchor = Some(anchor);
    prepared.office_state = Some(office_state);
    prepared.local_office_head = local_office_head;
    prepared.new_office = new_office;

    Ok(prepared)
}

impl PreparedTrust {
    /// Whether committing installs an anchor this warehouse does not hold yet.
    pub fn is_first_contact(&self) -> bool {
        self.adopting.is_some()
    }

    /// What a pallet taken alongside this trust is verified against before the commit: the
    /// served anchor and its verified office at first contact, nothing otherwise — a held anchor's
    /// take is verified against the local office once [`Self::commit`] has moved it
    /// ([`take_utils::TakeTrust::local`]).
    ///
    /// # Returns
    /// * `Some(TakeTrust)` - At first contact.
    /// * `None`            - Otherwise.
    pub fn take_trust(&self) -> Option<take_utils::TakeTrust> {
        self.adopting.as_ref()?;

        Some(take_utils::TakeTrust::FirstContact {
            anchor: self.anchor.clone().expect("first contact verified an anchor"),
            office_head: self.new_office.clone().or(self.local_office_head.clone())
                .expect("first contact verified an office"),
            office_state: self.office_state.clone().expect("first contact verified an office"),
        })
    }

    /// At first contact, judge every local user-pallet head against the anchor about to be
    /// installed, recording the ones it does not vouch for ([`take_utils::record_unvouched_heads`]).
    /// A first-contact [`Self::commit`] refuses without it. Nothing to do with an anchor held.
    ///
    /// # Arguments
    /// * `client`   - The remote the anchor comes from.
    /// * `verified` - Heads this command already verified against the served anchor.
    ///
    /// # Returns
    /// * `Ok(Vec<String>)` - The pallets the anchor does not vouch for, which the command names.
    /// * `Err(String)`     - The record could not be read or written.
    pub async fn judge_local_heads(&mut self, client: &RemoteClient, verified: &[&str]) -> Result<Vec<String>, String> {
        if self.adopting.is_none() {
            return Ok(Vec::new());
        }

        let anchor = self.anchor.as_ref().expect("first contact verified an anchor");
        let office_head = self.new_office.as_deref().or(self.local_office_head.as_deref())
            .expect("first contact verified an office");
        let office_state = self.office_state.as_ref().expect("first contact verified an office");

        let judgement = take_utils::record_unvouched_heads(client, anchor, office_head, office_state, verified).await?;
        self.judged_heads = Some(judgement.judged);

        Ok(judgement.unvouched)
    }

    /// Write the prepared trust: on first contact install the anchor with the office it governs;
    /// with an anchor held, fast-forward the office while that anchor still stands.
    ///
    /// # Returns
    /// * `Ok(TrustSyncStats)` - What happened, including the genesis adopted on first contact —
    ///                          which the caller must surface, since an adoption is never silent.
    /// * `Err(String)`        - If a write failed.
    pub fn commit(self) -> Result<TrustSyncStats, String> {
        let mut stats = TrustSyncStats { fetch: self.fetch, ..TrustSyncStats::default() };
        let new_office = self.new_office.as_deref();

        match (&self.adopting, &self.held_bytes) {
            // First contact: the anchor and the office it governs land together, in one hold, after
            // every refusal — an anchor written before one would stand unannounced, and the next
            // sync, finding it held, would never announce it either. A failed install that still
            // landed (a write can land and fail) is announced in its error for the same reason; one
            // that did not land holds nothing, and the next sync adopts and announces it.
            (Some(remote_trust), _) => {
                // A pallet that moved since its head was judged would be taken as vouched for.
                let Some(judged) = self.judged_heads.clone() else {
                    return Err(
                        "The local pallets were not judged against the remote's trust anchor, so \
                        it was not installed.".to_string()
                    );
                };

                let installed = office_utils::install_trust(&office_utils::TrustInstall {
                    anchor: self.anchor.as_ref().expect("first contact verified an anchor"),
                    replaces: None,
                    office: self.local_office_head.as_deref(),
                    new_office,
                    pallets: pallet_utils::PalletsPrecondition::WithinBoundary {
                        boundary: judged,
                        scope: pallet_utils::BoundaryScope::UserPallets,
                    },
                });

                if let Err(error) = installed {
                    let held = office_utils::read_trust_anchor().ok().flatten()
                        .is_some_and(|anchor| anchor.genesis == remote_trust.genesis);

                    return Err(match held {
                        true => format!(
                            "{} This warehouse now holds the remote's trust anchor (genesis {}).",
                            error, remote_trust.genesis
                        ),
                        false => error.to_string(),
                    });
                }

                stats.office_moved = new_office.is_some();
                stats.adopted_genesis = Some(remote_trust.genesis.clone());
            }
            // The anchor is held: the office fast-forwards only while it still is.
            (None, Some(anchor_bytes)) => {
                if let Some(new_office) = new_office {
                    pallet_utils::update_meta_pallet_head(
                        OFFICE_PALLET_NAME, self.local_office_head.as_deref(), new_office, &RefPreconditions {
                            anchor: pallet_utils::AnchorPrecondition::Bytes(anchor_bytes.clone()),
                            ..RefPreconditions::NONE
                        },
                    )?;
                    stats.office_moved = true;
                }
            }
            // No anchor on either side.
            (None, None) => {}
        }

        Ok(stats)
    }
}

/// Fetch the office history a remote's re-genesis anchor adopts, if it adopts any. The new office
/// chain starts fresh at the re-genesis, so following the office head never reaches it; without
/// this, a warehouse would hold an anchor pinning history it never had, and every `heal` would
/// report that pin dangling. Called before the anchor is written, so a failure leaves none held.
///
/// The adopted chain is frozen and attested by the anchor, so it is fetched, not re-verified.
async fn fetch_adopted_history(client: &RemoteClient, remote_trust: &TrustAnchorDto) -> Result<(), String> {
    if let Some(adopts) = &remote_trust.adopts {
        fetch_history(client, adopts, &Progress::NONE).await.map_err(|error| format!(
            "Could not fetch the office history {} adopted by the trust anchor of {}: {}",
            adopts, client.url(), error
        ))?;
    }

    Ok(())
}

/// Where a warehouse stands against its remote's trust anchor, as `office accept-regenesis` sees
/// it.
pub enum RegenesisState {
    /// The remote's anchor differs from the held one: the held anchor and the trust file's bytes
    /// it was read from, the remote's anchor, and the handshake that carried it.
    Pending {
        local: office_utils::TrustAnchor,
        local_bytes: Vec<u8>,
        remote: TrustAnchorDto,
        info: WarehouseInfo,
    },
    /// The remote's anchor is the held one.
    InSync { local: office_utils::TrustAnchor },
}

/// The re-genesis `office accept-regenesis` would act on. Shared by the describing and the
/// accepting path, so both refuse the same states the same way.
///
/// # Returns
/// * `Ok(RegenesisState)` - Whether a re-genesis is pending, with the anchors on each side.
/// * `Err(String)`        - This warehouse or the remote holds no anchor, or the handshake failed.
pub async fn pending_regenesis(client: &RemoteClient) -> Result<RegenesisState, String> {
    let Some((local, local_bytes)) = office_utils::read_trust_anchor_with_bytes()? else {
        return Err(
            "This warehouse has no trust anchor; a plain \"forklift lower\" adopts the remote's \
            trust on first contact.".to_string()
        );
    };

    let info = client.fetch_info().await?;

    trust_utils::check_handshake_trust(client.url(), &info, Some(local.genesis.as_str()), None)?;

    let Some(remote) = info.trust.clone() else {
        return Err("The remote has no trust anchor; there is no re-genesis to accept.".to_string());
    };

    if remote.genesis == local.genesis {
        return Ok(RegenesisState::InSync { local });
    }

    Ok(RegenesisState::Pending { local, local_bytes, remote, info })
}

/// Consciously accept a remote's re-genesis (§8.7): the loud, deliberate counterpart
/// of the refusal `prepare_remote_trust` raises when the remote's anchor was reset.
/// Verifies the chain of custody (the new anchor names the local genesis as its
/// prior), fetches and verifies the new office chain, then replaces the local anchor
/// and moves the office ref, together (see [`office_utils::install_trust`]). The *decision* to
/// trust the reset is the caller's — this is the mechanical half.
///
/// The acceptance is bound to a value: `confirmed` must be the remote's new genesis as it is
/// *now*, re-read here, so what is accepted is exactly what was shown — never whatever the remote
/// returns by the time this runs.
///
/// # Arguments
/// * `client`    - The remote.
/// * `confirmed` - The new genesis being accepted.
///
/// # Returns
/// * `Ok(Some((TrustAnchor, TrustAnchor, Vec<String>)))` - The replaced and the adopted anchor,
///                                            and the pallets the adopted one does not vouch for.
/// * `Ok(None)`                             - The remote's anchor is already the held one and
///                                            `confirmed` names it: already accepted.
/// * `Err(String)`                          - If either side holds no anchor, the custody chain
///                                            does not match or `confirmed` is not the remote's
///                                            genesis now (both `trust_changed` refusals), or a
///                                            transfer failed.
pub async fn accept_regenesis(client: &RemoteClient,
                              confirmed: &str)
    -> Result<Option<(office_utils::TrustAnchor, office_utils::TrustAnchor, Vec<String>)>, String> {
    let (local, local_bytes, remote_trust, info) = match pending_regenesis(client).await? {
        RegenesisState::Pending { local, local_bytes, remote, info } => (local, local_bytes, remote, info),
        // Nothing is pending. That is success only if the confirmed value is the one held;
        // otherwise the remote moved away from what was shown, and saying "nothing to accept"
        // would let a caller believe its value is now trusted.
        RegenesisState::InSync { local } if local.genesis == confirmed => return Ok(None),
        RegenesisState::InSync { local } => {
            return Err(trust_utils::confirm_not_pending(client.url(), &local.genesis, confirmed).into());
        }
    };
    let remote_trust = &remote_trust;

    if remote_trust.prior_genesis.as_deref() != Some(local.genesis.as_str()) {
        return Err(trust_utils::anchor_changed(
            client.url(), &local.genesis, remote_trust, "Refusing to accept it."
        ).into());
    }

    if remote_trust.genesis != confirmed {
        return Err(trust_utils::confirm_mismatch(client.url(), &local.genesis, remote_trust, confirmed).into());
    }

    let remote_office_head = info.pallets.get(&office_utils::office_wire_key())
        .ok_or("The remote has a trust anchor but no office pallet; it is corrupt.".to_string())?;

    // The office head the acceptance replaces, read before anything changes: the office ref moves
    // only while it still holds this.
    let local_office_head = pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME)?;

    fetch_history(client, remote_office_head, &Progress::NONE).await?;
    fetch_adopted_history(client, remote_trust).await?;

    // Never adopt an anchor whose chain does not even verify against itself.
    let new_anchor = remote_trust.to_anchor();
    let new_office_state = crate::util::audit_utils::verify_office_chain(&new_anchor, remote_office_head)?;

    // The local heads were vouched for by the anchor being replaced; the one replacing it judges
    // them afresh, as at first contact.
    let judgement = take_utils::record_unvouched_heads(client, &new_anchor, remote_office_head, &new_office_state, &[]).await?;

    // The anchor and the office move together, only while both are as this acceptance read them
    // and the user pallets hold only the heads judged above. Another bay that accepted the same re-genesis meanwhile installed exactly this, and the
    // acceptance stands.
    office_utils::install_trust(&office_utils::TrustInstall {
        anchor: &new_anchor,
        replaces: Some(&local_bytes),
        office: local_office_head.as_deref(),
        new_office: Some(remote_office_head),
        pallets: pallet_utils::PalletsPrecondition::WithinBoundary { boundary: judgement.judged, scope: pallet_utils::BoundaryScope::UserPallets },
    })?;

    Ok(Some((local, new_anchor, judgement.unvouched)))
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
    let local = office_utils::read_trust_anchor()?;

    // A stripped remote is refused here as it is on lower: uploading an anchor over an office
    // it never verified is exactly what the stripped state must not invite.
    trust_utils::check_handshake_trust(
        client.url(), info, local.as_ref().map(|anchor| anchor.genesis.as_str()), None
    )?;

    let Some(local) = local else {
        if info.trust.is_some() {
            return Err(
                "The remote has trust established but this warehouse does not. \
                \"forklift lower\" first to adopt the remote's office.".to_string()
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
            } else {
                return Err(trust_utils::anchor_changed(
                    client.url(), &local.genesis, remote_trust, "Refusing to lift."
                ).into());
            }
        }
    } else {
        // A re-genesis anchor's `adopts` names history no ref reaches — the new office chain
        // starts fresh — so the office lift below never carries it. Upload it first: the remote
        // refuses an anchor whose adopted history it does not hold, and every warehouse that
        // later adopts this anchor fetches that history from it.
        if let Some(adopts) = &local.adopts {
            upload_history(client, adopts, None, &[], info.chunking, &Progress::NONE).await?;
        }
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
        None,
        adopted_reset,
        info,
        &Progress::NONE,
    ).await?;

    Ok(Some(result))
}

/// The outcome of lifting one meta pallet (its wire ref, e.g. `@manifest`).
pub struct MetaPalletLift {
    pub pallet: String,

    /// The lift, or why it failed: a divergence (lower first), a remote that is ahead, a
    /// refusal of the pallet's content, or a failed transfer.
    pub result: Result<LiftResult, String>,
}

/// Lift every meta pallet but the office and `@haul` (the manifest, tags, and future ones) to
/// the remote, after the office and trust are already established there. Meta pallets are ordinary
/// signed pallets from the server's point of view — object upload plus a fast-forward
/// CAS — so this reuses `lift_pallet`; a diverged one fails (lower first), exactly like
/// a working pallet. The office is excluded: it is lifted with the trust state
/// (`push_local_trust`) so the remote holds the keys before any pallet that relies on
/// them arrives.
///
/// `@haul` is lifted after the working pallet ([`lift_haul_pallet`]), since a merge it records
/// may wait for that pallet's parcel.
///
/// Each pallet is lifted on its own: one that fails is reported in its outcome, and the
/// rest are still lifted.
///
/// # Arguments
/// * `client` - The remote.
/// * `info`   - The remote's handshake.
///
/// # Returns
/// * `Ok(Vec<MetaPalletLift>)` - Per-pallet outcomes (empty when none exist).
/// * `Err(String)`             - If the local meta pallets cannot be listed.
pub async fn lift_meta_pallets(client: &RemoteClient,
                               info: &WarehouseInfo) -> Result<Vec<MetaPalletLift>, String> {
    let mut lifts = Vec::new();

    for name in pallet_utils::list_meta_pallets()? {
        if name == OFFICE_PALLET_NAME || name == haul_utils::HAUL_PALLET_NAME {
            continue;
        }

        let wire = pallet_utils::PalletRef::meta(&name).to_wire();

        let local_head = match pallet_utils::get_meta_pallet_head(&name) {
            Ok(Some(local_head)) => local_head,
            Ok(None) => continue,
            Err(why) => {
                lifts.push(MetaPalletLift { pallet: wire, result: Err(why) });
                continue;
            }
        };

        let remote_head = info.pallets.get(&wire).map(String::as_str);

        // The handshake also says whether chunked files are taken, which never gates a meta pallet.
        let result = lift_pallet(client, &wire, &local_head, remote_head, None, info, &Progress::NONE).await;
        lifts.push(MetaPalletLift { pallet: wire, result });
    }

    Ok(lifts)
}

/// Lift the local `@haul` head, unless it would publish a merge record whose parcel the remote's
/// target is not known to hold ([`haul_utils::held_by_lift`]): then nothing is lifted and the
/// outcome fails naming each held record and its way out. Runs after the working pallet's lift
/// attempt, whatever its outcome; `landed` is `(pallet, head)` when that lift landed, and stands
/// in for the handshake's head of that pallet.
///
/// # Returns
/// * `None` - There is no local `@haul` head.
/// * `Some(MetaPalletLift)` - The lift, or why it failed or was held.
pub async fn lift_haul_pallet(client: &RemoteClient,
                              info: &WarehouseInfo,
                              landed: Option<(&str, &str)>) -> Option<MetaPalletLift> {
    let wire = pallet_utils::PalletRef::meta(haul_utils::HAUL_PALLET_NAME).to_wire();

    let local_head = match pallet_utils::get_meta_pallet_head(haul_utils::HAUL_PALLET_NAME) {
        Ok(Some(local_head)) => local_head,
        Ok(None) => return None,
        Err(why) => return Some(MetaPalletLift { pallet: wire, result: Err(why) }),
    };

    let remote_head = info.pallets.get(&wire).map(String::as_str);

    let remote_target_head = |target: &str| match landed {
        Some((pallet, head)) if pallet == target => Some(head.to_string()),
        _ => info.pallets.get(target).cloned(),
    };

    let result = match haul_utils::held_by_lift(&local_head, remote_head, &remote_target_head) {
        Err(why) => Err(why),
        Ok(held) if !held.is_empty() => {
            // A remote with no @haul gives "lower --keep" no remote side to keep.
            let drop = match remote_head {
                Some(_) => format!("; or drop the record with {}", haul_utils::drop_held(&held)),
                None => String::new(),
            };

            Err(format!(
                "{}. It goes once the remote's target holds the parcel: an operator who may write the target \
                 lifts it (lowering it first when the remote's head is not held here), then lift again{}.",
                haul_utils::describe_held(&held), drop))
        }
        Ok(_) => lift_pallet(client, &wire, &local_head, remote_head, None, info, &Progress::NONE).await,
    };

    Some(MetaPalletLift { pallet: wire, result })
}

/// Whether the local head of a meta pallet is ahead of the remote's: it descends from it and is
/// not it. Decided from the local ancestry alone, so the remote head need not be fetched.
pub fn is_local_ahead(remote_head: &str, local_head: &str) -> Result<bool, String> {
    Ok(local_head != remote_head && merge_utils::is_ancestor(remote_head, local_head)?)
}

/// Whether the content check this store can run itself refuses a meta pallet's lift: its local
/// head is ahead of `remote_head` and a new parcel breaks the pallet's rules against the local
/// verified office (an admission the office no longer grants, say). `false` when the check
/// passes, when the pallet is not ahead, or when it cannot be run here, so a failed lift for
/// any other reason (a transfer, a remote that moved) is never taken for a refusal.
pub fn lift_refused_by_content(name: &str, remote_head: Option<&str>) -> bool {
    let (Some(remote_head), Some(rules)) = (remote_head, meta_tree::meta_rules(name)) else {
        return false;
    };

    let refused = || -> Result<bool, String> {
        let Some(local) = pallet_utils::get_meta_pallet_head(name)? else { return Ok(false) };

        if !is_local_ahead(remote_head, &local)? {
            return Ok(false);
        }

        let office = crate::util::audit_utils::verified_local_office()?;

        Ok(meta_tree::verify_meta_content(name, rules, &office, meta_tree::Admission::Admit, Some(remote_head), &local).is_err())
    };

    refused().unwrap_or(false)
}

/// The outcome of adopting the remote's meta pallets.
#[derive(Default)]
pub struct MetaAdoptResult {
    /// Meta pallets fast-forwarded or first adopted (wire refs, e.g. `@manifest`).
    pub adopted: Vec<String>,

    /// Meta pallets whose local and remote heads diverged, as `(bare name, remote head)`.
    /// Their remote history has been fetched — and, for a meta pallet, verified from the
    /// two chains' common ancestor — and the caller applies the pallet's sync policy.
    pub diverged: Vec<(String, String)>,

    /// Meta pallets only this side moved: the local head descends from the remote one. As
    /// `(bare name, remote head)`. Nothing was fetched or moved for them.
    pub ahead: Vec<(String, String)>,

    /// Meta pallets that could not be adopted, as `(wire ref, why)`. One pallet failing never
    /// stops the others.
    pub failed: Vec<(String, String)>,
}

/// What adopting one meta pallet did.
enum MetaAdoptStep {
    /// Nothing to do: the local head is the remote's.
    UpToDate,

    /// Nothing to adopt: the local head descends from the remote's.
    Ahead,

    /// The local ref moved to the remote head.
    Adopted,

    /// The two heads diverged.
    Diverged,
}

/// Adopt the remote's *non-office* meta pallets (lower / clone direction): fetch each
/// and fast-forward the local ref, without ever materializing into the working directory
/// (meta pallets are not working content). A pallet present only on the remote is adopted
/// outright; one that diverged has its remote side fetched and is returned for the caller
/// to reconcile. A meta pallet's remote parcels pass the content check against the local
/// office before anything is adopted or reconciled. The office is excluded —
/// `prepare_remote_trust` handles it, and runs first.
///
/// Each pallet is adopted on its own: a refusal or a failed transfer is recorded against that
/// pallet, and the rest still sync.
///
/// # Arguments
/// * `client` - The remote.
/// * `info`   - The remote's handshake.
///
/// # Returns
/// * `MetaAdoptResult` - What was adopted, what diverged, and what failed.
pub async fn adopt_meta_pallets(client: &RemoteClient, info: &WarehouseInfo) -> MetaAdoptResult {
    let mut result = MetaAdoptResult::default();

    for (key, remote_head) in &info.pallets {
        let Some(name) = key.strip_prefix(pallet_utils::META_QUALIFIER) else {
            continue; // Not a meta pallet (user pallets are handled by lower/clone).
        };

        if name == OFFICE_PALLET_NAME {
            continue;
        }

        match adopt_meta_pallet(client, name, remote_head).await {
            Ok(MetaAdoptStep::UpToDate) => {}
            Ok(MetaAdoptStep::Ahead) => result.ahead.push((name.to_string(), remote_head.clone())),
            Ok(MetaAdoptStep::Adopted) => result.adopted.push(key.clone()),
            Ok(MetaAdoptStep::Diverged) => result.diverged.push((name.to_string(), remote_head.clone())),
            Err(why) => result.failed.push((key.clone(), why)),
        }
    }

    result
}

async fn adopt_meta_pallet(client: &RemoteClient, name: &str, remote_head: &str) -> Result<MetaAdoptStep, String> {
    meta_tree::check_meta_pallet_name(name)?;

    let local = pallet_utils::get_meta_pallet_head(name)?;

    // Up to date, or local is ahead — both decidable from the *local* ancestry alone (these
    // walks never load the not-yet-fetched remote head).
    if let Some(local) = &local {
        if local == remote_head {
            return Ok(MetaAdoptStep::UpToDate);
        }

        if is_local_ahead(remote_head, local)? {
            return Ok(MetaAdoptStep::Ahead);
        }
    }

    // A first adoption, a fast-forward or a divergence — deciding between the last two needs
    // the remote head's ancestry, so fetch it first, then classify.
    fetch_history(client, remote_head, &Progress::NONE).await?;

    let fast_forward = match &local {
        Some(local) => merge_utils::is_ancestor(local, remote_head)?,
        None => true,
    };

    // What the adoption is judged against, and so conditioned on: nothing for a pallet with no
    // rules, else the anchor and the office as read here.
    let mut preconditions = RefPreconditions::NONE;

    if let Some(rules) = meta_tree::meta_rules(name) {
        // The verified local base: the local head on a fast-forward, else the newest parcel
        // both chains share (a parcel the local side already verified).
        let base = match (&local, fast_forward) {
            (Some(local), true) => Some(local.clone()),
            (Some(local), false) => meta_tree::linear_common_ancestor(local, remote_head)?,
            (None, _) => None,
        };

        let anchor = office_utils::read_trust_anchor_with_bytes()?;
        let office_head = pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME)?;

        meta_tree::verify_meta_update(
            name, rules, meta_tree::Admission::Audit, anchor.as_ref().map(|(anchor, _)| anchor),
            office_head.as_deref(), base.as_deref(), remote_head
        ).map_err(|why| format!("The remote @{} was refused: {}", name, why))?;

        preconditions = RefPreconditions {
            office: pallet_utils::OfficePrecondition::At(office_head),
            anchor: match anchor {
                Some((_, bytes)) => pallet_utils::AnchorPrecondition::Bytes(bytes),
                None => pallet_utils::AnchorPrecondition::Absent,
            },
            pallets: pallet_utils::PalletsPrecondition::NotConsumed,
        };
    }

    if !fast_forward {
        return Ok(MetaAdoptStep::Diverged);
    }

    pallet_utils::update_meta_pallet_head(name, local.as_deref(), remote_head, &preconditions)?;

    Ok(MetaAdoptStep::Adopted)
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
    // No remote means no directory to ask, and Forklift holds no names of its own.
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

    // Revert: drop the no-rules arm of `diverged_refusal` → "@office" is classified, its remedy
    // "lower", which refuses it.
    #[test]
    fn only_a_pallet_with_a_merge_classifies_its_divergence() {
        let head = "a".repeat(64);
        let code = |pallet: &str| match diverged_refusal(pallet, &head) {
            CoreError::Refusal { code, next_step, .. } => Some((code, next_step)),
            CoreError::Other(_) => None,
        };

        assert_eq!(code("@office"), None);
        assert_eq!(code("@kv"), Some((RefusalCode::Diverged, "forklift lower".to_string())));
        assert!(code("main").is_some_and(|(code, step)| code == RefusalCode::Diverged
            && step.starts_with("forklift pallet create main-remote-aaaaaaaaaaaa aaaaaaaaaaaa; then forklift shift main;")));
    }
    use std::path::PathBuf;
    use std::sync::Mutex;
    use crate::builder::object::loose_object_builder::LooseObjectBuilder;
    use crate::model::remote::ErrorResponse;

    /// The anchors and office heads of a re-genesis acceptance, installed in a scratch warehouse
    /// whose office stands at the head read before the acceptance began.
    struct Regenesis {
        root: PathBuf,
        _scope: crate::globals::StorageRootScope,
        old_bytes: Vec<u8>,
        new: office_utils::TrustAnchor,
        local_head: String,
        remote_head: String,
    }

    impl Regenesis {
        fn new(name: &str) -> Regenesis {
            let root = std::env::temp_dir()
                .join(format!("forklift-remote-utils-regenesis-{}-{}", name, std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = crate::globals::StorageRootScope::enter(&root);
            crate::util::warehouse_utils::mark_for_test(crate::util::warehouse_utils::WarehouseMode::Full);

            let local_head = "a".repeat(64);
            let old = office_utils::TrustAnchor {
                genesis: "0".repeat(64), enabled_at: 1, boundary: Vec::new(), prior_genesis: None, adopts: None,
            };
            let new = office_utils::TrustAnchor {
                genesis: "1".repeat(64),
                enabled_at: 2,
                boundary: Vec::new(),
                prior_genesis: Some(old.genesis.clone()),
                adopts: Some(local_head.clone()),
            };

            office_utils::write_trust_anchor(&old).unwrap();
            pallet_utils::update_meta_pallet_head(OFFICE_PALLET_NAME, None, &local_head, &pallet_utils::RefPreconditions::NONE).unwrap();
            let old_bytes = office_utils::read_trust_anchor_bytes().unwrap().unwrap();

            Regenesis { root, _scope: scope, old_bytes, new, local_head, remote_head: "b".repeat(64) }
        }

        /// Move the office ref the way another bay would, behind this acceptance's back.
        fn move_office(&self, to: &str) {
            pallet_utils::update_meta_pallet_head(OFFICE_PALLET_NAME, Some(&self.local_head), to, &pallet_utils::RefPreconditions::NONE).unwrap();
        }

        /// Accept the re-genesis as decided against the old anchor and the local office head.
        fn install(&self) -> Result<(), pallet_utils::RefUpdateError> {
            office_utils::install_trust(&office_utils::TrustInstall {
                anchor: &self.new,
                replaces: Some(&self.old_bytes),
                office: Some(&self.local_head),
                new_office: Some(&self.remote_head),
                pallets: pallet_utils::PalletsPrecondition::NotConsumed,
            }).map(|_| ())
        }

        /// Assert neither the anchor nor the office moved from where the fixture put them.
        fn assert_untouched(&self) {
            assert_eq!(office_utils::read_trust_anchor_bytes().unwrap(), Some(self.old_bytes.clone()),
                "the previous anchor must stand, byte for byte");
            assert_eq!(pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME).unwrap(), Some(self.local_head.clone()));
        }
    }

    impl Drop for Regenesis {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_regenesis_whose_office_moved_elsewhere_is_refused_and_writes_nothing() {
        let fixture = Regenesis::new("moved-elsewhere");
        let elsewhere = "c".repeat(64);
        fixture.move_office(&elsewhere);

        let error = fixture.install().expect_err("an office moved meanwhile must refuse the re-genesis");

        assert_eq!(office_utils::read_trust_anchor_bytes().unwrap(), Some(fixture.old_bytes.clone()),
            "the previous anchor must stand, matching the office it still governs");
        assert_eq!(pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME).unwrap(), Some(elsewhere.clone()));
        assert!(matches!(&error, pallet_utils::RefUpdateError::OfficeMoved { found } if found.as_deref() == Some(elsewhere.as_str())),
            "{}", error);
    }

    #[test]
    fn a_regenesis_another_bay_already_installed_succeeds_without_writing() {
        // Another bay accepted the same re-genesis first: the anchor and the office both carry it.
        let fixture = Regenesis::new("already-installed");
        fixture.install().unwrap();

        fixture.install().expect("the acceptance stands: the anchor and the office carry the re-genesis");

        assert_eq!(office_utils::read_trust_anchor().unwrap(), Some(fixture.new.clone()));
        assert_eq!(pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME).unwrap(), Some(fixture.remote_head.clone()));
    }

    #[test]
    fn a_regenesis_whose_anchor_moved_is_refused_and_writes_nothing() {
        // Another bay's re-genesis changed the anchor but not (yet) the office: what this one read
        // is gone, so it refuses rather than replacing an anchor it never judged.
        let fixture = Regenesis::new("anchor-moved");
        let theirs = office_utils::TrustAnchor { genesis: "3".repeat(64), ..fixture.new.clone() };
        office_utils::install_trust(&office_utils::TrustInstall {
            anchor: &theirs,
            replaces: Some(&fixture.old_bytes),
            office: Some(&fixture.local_head),
            new_office: None,
            pallets: pallet_utils::PalletsPrecondition::NotConsumed,
        }).unwrap();

        let error = fixture.install().expect_err("a moved anchor must refuse the re-genesis");

        assert_eq!(office_utils::read_trust_anchor().unwrap(), Some(theirs), "the other bay's anchor must stand");
        assert_eq!(pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME).unwrap(), Some(fixture.local_head.clone()));
        assert!(matches!(error, pallet_utils::RefUpdateError::AnchorMoved), "{}", error);
    }

    // Revert: skip `restore_trust_file` → the new anchor stands beside the old office.
    #[test]
    fn a_trust_install_whose_office_write_fails_puts_the_previous_anchor_back() {
        let fixture = Regenesis::new("office-refused");
        office_utils::tests::OFFICE_WRITE_FAULT.with(|fault| fault.set(office_utils::tests::OfficeWriteFault::Refuse));

        let error = fixture.install().expect_err("a failed office write must fail the install");

        fixture.assert_untouched();
        assert!(error.to_string().contains("injected office write refusal"), "{}", error);
    }

    // Revert: restore on any office error without reading back → the old anchor beside the new
    // office.
    #[test]
    fn a_trust_install_whose_office_write_lands_and_reports_failure_stands() {
        let fixture = Regenesis::new("office-landed");
        office_utils::tests::OFFICE_WRITE_FAULT.with(|fault| fault.set(office_utils::tests::OfficeWriteFault::AfterLanding));

        let reported = fixture.install();

        assert_eq!(office_utils::read_trust_anchor().unwrap(), Some(fixture.new.clone()), "both writes landed, so the new anchor stands");
        assert_eq!(pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME).unwrap(), Some(fixture.remote_head.clone()));
        let Err(pallet_utils::RefUpdateError::Failed(message)) = reported else {
            panic!("a write that landed but failed is still reported: {:?}", reported);
        };
        assert!(message.contains("both written and stand"), "{}", message);
    }

    // Revert: release and re-take the ref lock between the anchor write and the office write →
    // the probe takes the lock.
    #[test]
    fn a_trust_install_holds_the_ref_lock_between_its_two_writes() {
        let fixture = Regenesis::new("one-hold");
        let root = fixture.root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT);
        let (probe_tx, probe_rx) = std::sync::mpsc::channel();

        office_utils::tests::BETWEEN_TRUST_WRITES.with(|seam| seam.set(Some(Box::new(move || {
            let probe = std::thread::spawn(move || {
                crate::util::lock_utils::RefLock::try_acquire_in(&root).map(|lock| lock.is_some())
            });
            probe_tx.send(probe.join().unwrap()).unwrap();
        }))));

        fixture.install().unwrap();

        assert_eq!(probe_rx.recv().unwrap(), Ok(false), "another holder must find the ref lock held between the writes");
    }

    #[test]
    fn a_regenesis_whose_office_did_not_move_installs_both() {
        let fixture = Regenesis::new("unmoved");

        fixture.install().unwrap();

        assert_eq!(office_utils::read_trust_anchor().unwrap(), Some(fixture.new.clone()));
        assert_eq!(pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME).unwrap(), Some(fixture.remote_head.clone()));
    }

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
            CoreError::Refusal { code, message, next_step, .. } => {
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

    /// A server error is transient however it is wrapped; a refusal the server meant is not, nor
    /// is a local failure whose text merely mentions a status.
    #[test]
    fn a_server_error_is_transient_and_a_refusal_is_not() {
        let server_error = classify_remote_error(503, "fetching objects", "busy".to_string(), None, None);
        assert!(is_transient_failure(&server_error));
        assert!(is_transient_failure(&format!("Could not adopt the remote @office: {}", server_error)));

        let refused = classify_remote_error(404, "fetching objects", "no such object (500): gone".to_string(), None, None);
        assert!(!is_transient_failure(&refused));
        assert!(!is_transient_failure("Error while writing \"x (500): y\": No space left on device"));
        assert!(is_transient_failure("Timed out while fetching objects: the remote did not respond."));
        assert!(is_transient_failure(&format!("Error while downloading the bundle: {}connection reset", CONNECTION_FAILED)));
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
            crate::util::warehouse_utils::mark_for_test(crate::util::warehouse_utils::WarehouseMode::Full);

            Scratch { root, _scope: scope }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A real version-3 batch bundle carrying one freshly built object, split into exactly
    /// `pieces` chunks for a drip fixture, and that object's hash. Built in a store of its own, so
    /// the caller's store starts without the object.
    fn drip_bundle(pieces: usize) -> (Vec<Vec<u8>>, String) {
        let (bytes, hash) = {
            let _source = Scratch::new("drip-bundle-source");
            let hash = stack(Vec::new(), "drip");
            (bundle_utils::build_partial_bundle(std::slice::from_ref(&hash)).unwrap(), hash)
        };
        let chunks: Vec<Vec<u8>> = bytes.chunks(bytes.len().div_ceil(pieces)).map(<[u8]>::to_vec).collect();
        assert_eq!(chunks.len(), pieces, "the bundle splits into exactly the pieces asked for");
        (chunks, hash)
    }

    /// [`stack`], but with a real tree, for the walks that actually descend one.
    fn stack_with_tree(parents: Vec<String>, tree_hash: &str, tag: &str) -> String {
        let parcel = crate::model::parcel::Parcel {
            tree_hash: tree_hash.to_string(),
            parents,
            actions: Vec::new(),
            description: Some(tag.to_string()),
        };
        let mut object = LooseObjectBuilder::build_parcel(&parcel);
        object.store().unwrap();
        object.hash
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
    fn a_pallet_route_carries_the_name_as_literal_segments() {
        // A gateway that decodes `%2F` before routing would split `feature%2Fx`; the name goes
        // out as the literal segments it already has, each still encoded on its own.
        assert_eq!(pallet_route("feature/x").unwrap(), "/v1/pallets/feature/x");
        assert_eq!(holds_route("feature/x").unwrap(), "/v1/holds/feature/x");
        assert_eq!(pallet_route("@office").unwrap(), "/v1/pallets/%40office");
        assert_eq!(holds_route("main").unwrap(), "/v1/holds/main");
    }

    #[test]
    fn a_name_that_is_no_valid_reference_never_becomes_a_route() {
        // An intermediary may collapse `//` and resolve `.`/`..` before the server sees the path,
        // so each of these could arrive as some other, valid name: none is ever sent.
        for name in ["a//b", "a/b/", "/main", "x/../main", "x/./main", "..", "", "Main"] {
            assert!(pallet_route(name).is_err(), "pallet route of {:?}", name);
            assert!(holds_route(name).is_err(), "holds route of {:?}", name);
        }
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
        let CoreError::Refusal { code, message, next_step, .. } = error else {
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

    /// `error_body_read_budget` must fold the caller's own
    /// `connect_timeout` into the flat `ERROR_BODY_READ_TIMEOUT` base — a Tor-routed client's
    /// (60s) connect budget must survive into the composed bound, not just the direct client's
    /// (5s) one. Asserts the arithmetic directly rather than constructing a live Tor client and
    /// waiting out the real budget (the exact ~72s shape deleted from this suite in 8b93d00).
    ///
    /// This pins one of four links; on its own it pins only the helper's arithmetic, not that
    /// `error_of` actually calls it (that link is pinned by
    /// `missing_objects_bounds_the_error_body_read_after_a_wedged_500` below, which asserts a
    /// lower bound on elapsed time), not that the accessor computing the budget reads
    /// *this instance's* `budgets.connect` field at all, rather than hardcoding whichever constant
    /// a given fixture's client happens to carry (a fixture built through a
    /// production constructor can only ever carry `REMOTE_CONNECT_TIMEOUT` or
    /// `REMOTE_CONNECT_TIMEOUT_TOR`, so it cannot separate "reads the field" from "hardcodes that
    /// one value"; see `error_body_budget_reads_this_field_not_a_rival_constant`, which injects a
    /// value neither constructor can produce), and not that the value at the one client mode that
    /// actually ships with a non-default budget (`TorMode::On`) is the correct 70s rather than some
    /// other value a future change (e.g. a cap) could silently substitute (see
    /// `error_body_budget_is_70s_for_a_real_tor_mode_client`). All four together pin
    /// `error_of`'s real behavior, on any client, without a live remote;
    /// `commit_lift_bounds_the_error_body_read_after_a_wedged_422` pins the budget's other call
    /// site the same way.
    #[test]
    fn error_body_read_budget_folds_in_the_connect_timeout() {
        assert_eq!(
            error_body_read_budget(&Budgets::production(true)),
            TEST_TOR_CONNECT_TIMEOUT + TEST_ERROR_BODY_READ_TIMEOUT,
            "a Tor-routed client's 60s connect budget must be folded into the error-body-read \
            bound, not just the flat 10s base — a bound that ignores the link's own latency \
            preempts healthy work on a slow-but-legitimate connection"
        );
        assert_eq!(
            error_body_read_budget(&Budgets::production(false)),
            TEST_DIRECT_CONNECT_TIMEOUT + TEST_ERROR_BODY_READ_TIMEOUT,
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

    /// FORK-109. The candidate walk may only prune a parcel's subtrees against its parents'
    /// trees once those parents are on the remote or already in the candidate set. The walk
    /// that feeds it is a stack-driven DFS, and the code used to reverse its preorder under a
    /// comment claiming "oldest first" — which reversing a preorder does not give you.
    ///
    /// The fixture is the smallest DAG that discriminates: an unequal-branch merge, where both
    /// parents share a grandparent and the DFS descends one branch to the bottom before
    /// touching the other. The test asserts the property in both directions — that the shipped
    /// order satisfies parents-first, and that the reversed preorder it replaced does not — so
    /// a revert to `.rev()` fails here rather than passing quietly.
    ///
    /// A linear chain and an equal-branch merge both pass under either order, which is why
    /// this went unnoticed: any fixture simpler than this one is green before and after.
    #[test]
    fn new_parcels_are_ordered_parents_first_across_an_unequal_branch_merge() {
        let _scratch = Scratch::new("fork109-parents-first");

        // R is the remote head. M merges A and B; both descend from P, whose parent is R.
        // The DFS pops from the back, so it descends B's side fully before reaching A.
        let r = stack(Vec::new(), "R remote head");
        let p = stack(vec![r.clone()], "P shared grandparent");
        let a = stack(vec![p.clone()], "A short branch");
        let b = stack(vec![p.clone()], "B other branch");
        let m = stack(vec![a.clone(), b.clone()], "M merge head");

        let parents_of: HashMap<&str, Vec<&str>> = HashMap::from([
            (m.as_str(), vec![a.as_str(), b.as_str()]),
            (a.as_str(), vec![p.as_str()]),
            (b.as_str(), vec![p.as_str()]),
            (p.as_str(), vec![r.as_str()]),
        ]);

        // Every parcel must appear after each of its parents that is also being pushed. A
        // parent outside the set is on the remote already, so it constrains nothing.
        let parents_first = |order: &[String]| -> bool {
            let position: HashMap<&str, usize> = order.iter()
                .enumerate()
                .map(|(index, hash)| (hash.as_str(), index))
                .collect();
            order.iter().enumerate().all(|(index, hash)| {
                parents_of.get(hash.as_str()).into_iter().flatten().all(|parent| {
                    position.get(parent).is_none_or(|parent_index| *parent_index < index)
                })
            })
        };

        let shipped = new_parcels_parents_first(&m, Some(&r), &[])
            .expect("the walk must succeed on a well-formed DAG");

        assert_eq!(shipped.len(), 4, "R is the remote head, so the push carries M, A, B and P");
        assert!(
            parents_first(&shipped),
            "the shipped order must put every parcel after its parents; got {:?}",
            shipped
        );

        // The other direction: the order this replaced, reconstructed exactly — the DFS
        // preorder, reversed. Without this the test above could pass for the wrong reason,
        // e.g. if the fixture happened to be order-insensitive.
        let mut preorder: Vec<String> = Vec::new();
        let mut queue: Vec<String> = vec![m.clone()];
        let mut visited: HashSet<String> = HashSet::new();
        while let Some(hash) = queue.pop() {
            if hash == r || !visited.insert(hash.clone()) {
                continue;
            }
            queue.extend(parents_of.get(hash.as_str()).into_iter().flatten()
                .map(|parent| parent.to_string()));
            preorder.push(hash);
        }
        let reversed: Vec<String> = preorder.into_iter().rev().collect();

        assert_eq!(reversed.len(), 4, "the two orders must range over the same parcels");
        assert!(
            !parents_first(&reversed),
            "the fixture must discriminate: reversing the DFS preorder is expected to put a \
             parcel before its own parent, but this order satisfies parents-first, so the \
             assertion above proves nothing. Order was {:?}",
            reversed
        );
    }

    /// A review of FORK-109 proposed that ordering by commit-graph generation would regress a
    /// sparse, shallow or narrowed warehouse, on the reasoning that `graph_utils::ensure` reads
    /// below the lifted set where an ancestor can be legitimately absent, while the walk prunes
    /// at every ancestor of the remote head and never goes there.
    ///
    /// **It does not, and this test is why the claim is not repeated in the doc comment above.**
    /// The walk prunes via `merge_utils::is_ancestor`, which can only answer *false* — the
    /// answer every new parcel needs — by exhausting the remote head's entire ancestry. So the
    /// walk already demands exactly the objects the argument assumes it avoids, and a
    /// sealed-absent ancestor fails the lift before any ordering happens, under either order.
    ///
    /// The reason to order against the walked set's own edges is cost, not this: it reads
    /// nothing the walk has not already read and persists no graph shard. Pinned here so the
    /// generation sort is not re-proposed on a correctness argument that does not hold.
    #[test]
    fn an_absent_ancestor_fails_the_walk_itself_whatever_the_order_is() {
        let _scratch = Scratch::new("fork109-absent-ancestor");

        // R is the remote head and its parent was never stored — a sealed or narrowed slice.
        let absent = "f".repeat(64);
        let r = stack(vec![absent], "R remote head over an absent parent");
        let a = stack(vec![r.clone()], "A new");
        let m = stack(vec![a.clone()], "M head");

        let walked = new_parcels_parents_first(&m, Some(&r), &[]);
        assert!(
            walked.is_err(),
            "the walk itself must fail on an unreadable ancestor of the remote head — if this \
             ever succeeds, the ordering choice becomes load-bearing for sparse warehouses and \
             the doc comment above needs revisiting. Got {:?}",
            walked
        );

        // And the generation route fails on the very same object, so it was never the cause.
        let via_generation = crate::util::graph_utils::generation(&m);
        assert!(
            via_generation.is_err(),
            "the generation route must fail on the same absent ancestor, not survive it"
        );
    }

    /// The walk stops at another head the remote holds and at its ancestors, so a new pallet
    /// sends only what lies past where it branched. A head whose ancestry cannot be read here
    /// is only a shortcut lost: the walk answers "not known" and goes on, as it would without it.
    ///
    /// With a remote head of its own, another head stops the walk by its hash alone: an ancestry
    /// check there would cost a walk per parcel per head on every lift.
    ///
    /// Revert: ignore `elsewhere` → `B` and `R` are walked too; propagate the other head's
    /// ancestry error → the walk fails; drop the hash stop → the merge of `N` walks it; check
    /// ancestry with a remote head too → the merge of `R` stops at it.
    #[test]
    fn the_walk_stops_at_another_held_head_and_walks_past_an_unreadable_one() {
        let _scratch = Scratch::new("walk-held-elsewhere");

        let b = stack(vec![], "B base");
        let r = stack(vec![b.clone()], "R another pallet's head");
        let n = stack(vec![r.clone()], "N new");

        assert_eq!(new_parcels_parents_first(&n, None, &[r.clone()]).unwrap(), vec![n.clone()]);
        assert_eq!(new_parcels_parents_first(&n, None, &[]).unwrap().len(), 3);

        let unreadable = stack(vec!["f".repeat(64)], "U a head over an absent parent");
        let walked = new_parcels_parents_first(&n, None, &[unreadable]).unwrap();
        assert_eq!(walked, vec![b.clone(), r.clone(), n.clone()]);

        let own = stack(vec![], "O the pallet's own remote head");
        let merges_head = stack(vec![own.clone(), n.clone()], "M merges the held head N in");
        assert_eq!(new_parcels_parents_first(&merges_head, Some(&own), &[n.clone()]).unwrap(), vec![merges_head]);
        let merges_older = stack(vec![own.clone(), r.clone()], "M merges R, an ancestor of N, in");
        assert_eq!(new_parcels_parents_first(&merges_older, Some(&own), &[n]).unwrap(), vec![b, r, merges_older]);
    }

    /// A user pallet's lift is bounded by the remote's live user pallet heads present here: never
    /// its own name, a meta pallet, a tombstone, or a head this warehouse lacks; and a meta
    /// pallet's lift by none.
    ///
    /// Revert: take `held_pallet_heads`, or drop any one condition → the first assertion fails;
    /// drop the early return → the second fails.
    #[test]
    fn a_user_pallets_lift_stops_only_at_the_remotes_other_live_user_heads_present_here() {
        let _scratch = Scratch::new("held-elsewhere");

        let main = stack(vec![], "main");
        let own = stack(vec![main.clone()], "own");
        let office = stack(vec![], "office");
        let deleted = stack(vec![], "deleted");
        let absent = "e".repeat(64);

        let info = WarehouseInfo {
            protocol: "x".to_string(),
            default_pallet: "main".to_string(),
            pallets: BTreeMap::from([
                ("main".to_string(), main.clone()),
                ("copy".to_string(), main.clone()),
                ("feature".to_string(), own),
                ("@office".to_string(), office),
                ("elsewhere".to_string(), absent),
            ]),
            trust: None,
            chunking: true,
            tombstones: Some(BTreeMap::from([("gone".to_string(), deleted)])),
        };

        assert_eq!(held_elsewhere(&info, "feature"), vec![main.clone()]);
        assert!(held_elsewhere(&info, "@manifest").is_empty());
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

    /// FORK-94's walk-cardinality spike, made permanent: **every blob the server's audit
    /// presence-checks is one the client already priced.**
    ///
    /// This is the property FORK-92's keystone needs from `update_ref`, and the reason that call
    /// was split out of it. For the four negotiation/fetch calls the priced quantity equals the
    /// server's workload by construction, because the request body enumerates the workload. For
    /// `update_ref` the quantity is *derived* — a walk on each side — so it holds only if the
    /// two walks prune compatibly. Before FORK-94 they did not: the client pruned against every
    /// parent, the server against `known_complete`'s root alone, so a sparse merge could under-
    /// price by orders of magnitude and false-trip identically on every retry.
    ///
    /// A count comparison would be the weaker test — two numbers can pass while the sets differ.
    /// This asserts **containment**: the set of blobs the server presence-checks is a subset of
    /// the client's candidate set. That is what makes the client's count an honest upper bound.
    ///
    /// Discriminating in both directions: the unbounded audit (`known_complete: None`) is the
    /// same walk with an empty base set at every parcel, and it reaches blobs the client never
    /// priced — so the containment fails there, and the assertion is not vacuous.
    ///
    /// **The fixture forks the branch *below* the bound, and that is load-bearing for the
    /// parent-bases half of the union, not the `known_complete` half.** A merge whose two sides
    /// both descend from `known_complete` does not discriminate: each side's own walk descends
    /// and marks the content it introduces, so `visited_trees` alone keeps the server off
    /// anything unpriced even with the parent bases removed. It is the *boundary* parent — `G`,
    /// the fork point below the bound, which no walk in this segment covers — that only the
    /// per-parcel immediate-parent loop can explain: `B`'s unchanged `api` entry matches `G`'s
    /// tree, and `known_complete`'s own tree (`K`) disagrees with `B` on `api`, so it explains
    /// nothing there. Gate off the parent-bases loop and the server has only `K`'s tree to prune
    /// `B` against; `api` stops being explained, the server descends it, and `blob_exists`
    /// touches the unchanged `api` blob the client correctly never priced. That is the
    /// sparse-merge under-pricing FORK-94's ticket described, and it is this fixture's falsifier
    /// for the parent-bases half: disabling that loop turns this test red on it.
    ///
    /// The `known_complete` half (`base_root`) is not, and cannot be made, load-bearing by this
    /// property, on any fixture: adding a base to the union only ever prunes the server's walk
    /// *further*, so removing `base_root` can only widen the server's touched set toward — never
    /// past — the client's, because a server audit without `base_root` still prunes against the
    /// same immediate-parent trees the client's own walk prunes against. Containment is
    /// `server ⊆ client`; `base_root` is slack with respect to that inequality on every topology,
    /// so no fixture can pin it here.
    ///
    /// What this does not establish: `load_base_tree` reaches candidate-base trees — already-
    /// remote ancestry (`K` and `G`'s subtrees) the client correctly never prices — so those
    /// loads sit outside this property by construction, not by an oversight in the fixture; only
    /// the `blob_exists` closure below records into `touched`. `chunks_missing` and
    /// `load_recipe_chunks` are stubs returning empty, and no chunked file is in this fixture, so
    /// the recipe → chunk arm on both sides is uncovered. And both walks mark-before-check on
    /// their tree sets (`collect_changed_closure`'s `seen_trees`, `verify_tree_closure`'s
    /// `visited_trees`), so containment only holds while the client's `new_parcels_parents_first`
    /// and the server's `parents_first` — two independently written topological sorts over two
    /// independently derived edge sets — agree on order; this fixture's segment has exactly one
    /// valid order (`B` before `merge`, forced by the parent edge between them), so it cannot
    /// detect a regression that makes the two sorts disagree.
    #[test]
    fn every_blob_the_server_presence_checks_was_priced_by_the_client_walk() {
        use crate::enums::dir_entry_type::DirEntryType::{Normal, Tree};
        use std::cell::RefCell;

        let _scratch = Scratch::new("fork94-walk-cardinality");

        let api_v1 = store_blob("api v1");
        let api_v2 = store_blob("api v2");
        let web_v0 = store_blob("web v0");
        let web_v1 = store_blob("web v1");

        let api_base = store_tree(&[("a.txt", &api_v1, Normal)]);
        let api_k = store_tree(&[("a.txt", &api_v2, Normal)]);
        let web_base = store_tree(&[("w.txt", &web_v0, Normal)]);
        let web_b = store_tree(&[("w.txt", &web_v1, Normal)]);

        // G is the fork point, below the bound. K (the bound) edits api; the branch B edits web
        // off G, never seeing K's api; the merge adopts one subtree from each.
        let root_g = store_tree(&[("api", &api_base, Tree), ("web", &web_base, Tree)]);
        let root_k = store_tree(&[("api", &api_k, Tree), ("web", &web_base, Tree)]);
        let root_b = store_tree(&[("api", &api_base, Tree), ("web", &web_b, Tree)]);
        let root_merge = store_tree(&[("api", &api_k, Tree), ("web", &web_b, Tree)]);

        let g = stack_with_tree(Vec::new(), &root_g, "G");
        let k = stack_with_tree(vec![g.clone()], &root_k, "K");
        let b = stack_with_tree(vec![g.clone()], &root_b, "B");
        let merge = stack_with_tree(vec![k.clone(), b.clone()], &root_merge, "merge");

        crate::util::graph_utils::build_from_heads(std::slice::from_ref(&merge))
            .expect("warm the commit graph");

        // The client's priced quantity, computed exactly as `lift_pallet_inner` computes it.
        let new_parcels = new_parcels_parents_first(&merge, Some(&k), &[])
            .expect("the segment walk must succeed");
        let mut candidates: Vec<String> = new_parcels.clone();
        let mut seen_trees = HashSet::new();
        let mut seen_blobs = HashSet::new();
        let mut seen_recipes = HashSet::new();
        for parcel_hash in &new_parcels {
            let parcel = object_utils::load_parcel(parcel_hash).expect("a parcel");
            let base_trees: Vec<String> = parcel.parents.iter()
                .map(|parent| object_utils::load_parcel(parent).map(|p| p.tree_hash))
                .collect::<Result<_, _>>()
                .expect("every parent resolves");
            collect_changed_closure(&parcel.tree_hash, "", &base_trees, &mut seen_trees,
                                    &mut seen_blobs, &mut seen_recipes, &mut candidates, true)
                .expect("the client walk must not load a pruned object");
        }
        let priced: HashSet<String> = candidates.iter().cloned().collect();

        // The server's audit, with every store read counted through the closures it already
        // takes — no production instrumentation, so this number is reproducible from the tree.
        let audit = |known_complete: Option<&str>| -> Vec<String> {
            let touched: RefCell<Vec<String>> = RefCell::new(Vec::new());
            let blob_exists = |hash: &str| -> Result<bool, String> {
                touched.borrow_mut().push(hash.to_string());
                Ok(true)
            };
            let chunks_missing = |_: &[String]| -> Result<Vec<String>, String> { Ok(Vec::new()) };
            let load_recipe_chunks =
                |_: &str| -> Result<Vec<String>, String> { Ok(Vec::new()) };
            let load_base_tree = |hash: &str| object_utils::load_tree(hash);

            crate::util::audit_utils::verify_parcel_closure_with(
                &merge, known_complete, pallet_utils::PalletNamespace::User, &blob_exists, &chunks_missing,
                &load_recipe_chunks, &load_base_tree,
            ).expect("the fixture is complete, so the audit must pass");

            touched.into_inner()
        };

        let bounded = audit(Some(&k));

        // The property. Stated as containment, not as a count.
        for hash in &bounded {
            assert!(
                priced.contains(hash),
                "the audit presence-checked {} which the client never priced; the walks have \
                 diverged again and `update_ref` cannot rejoin FORK-92's mechanism. \
                 priced={:?} touched={:?}",
                hash, candidates, bounded
            );
        }

        // The server reaches exactly one blob's worth of leaf work: `B`'s `api` entry is pruned
        // by `G`'s tree (see the doc comment above), so only the changed `web` blob is
        // presence-checked. The client's priced set is six objects, of which only that one is a
        // blob — the rest are the segment's two parcels and the three trees the closure walk
        // adds. Pinned as numbers too, so a regression that keeps containment while inflating
        // either walk still fails.
        assert_eq!(bounded.len(), 1, "only the branch's changed blob is reached: {:?}", bounded);
        assert_eq!(priced.len(), 6, "2 parcels + the branch root, its web subtree and blob, \
                   + the merge root: {:?}", candidates);

        // The falsifier: the unbounded audit is the same walk with no bases, and it reaches the
        // *unchanged* blobs the client correctly declined to price. If this ever stops finding
        // an unpriced object, the containment assertion above has gone vacuous.
        let unbounded = audit(None);
        assert!(
            unbounded.iter().any(|hash| !priced.contains(hash)),
            "the unbounded audit must touch something the client never priced, or the \
             containment assertion above proves nothing: {:?}",
            unbounded
        );
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

        let error = runtime.block_on(upload_objects(&client, &[hash.clone()], &mut || {})).unwrap_err();
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
        let error = runtime.block_on(upload_to_targets(&client, &targets, &mut || {})).unwrap_err();
        let (code, message, _) = scope_utils::decode_refusal(&error)
            .expect("the refusal must decode via the shared sentinel framing");

        assert_eq!(code, scope_utils::CODE_OVERSIZED_TRANSPORT_UNSUPPORTED);
        assert!(message.contains(&hash), "the refusal names the object: {}", message);
    }

    // -----------------------------------------------------------------------------------
    // `upload_objects`/`upload_to_targets` used to call the synchronous
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
    /// The ticker count alone does not exercise *where*
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

            let outcome = upload_objects(&client, &[hash], &mut || {}).await;
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
            &client, "session-1", &candidates, &control_plane, false, &Progress::NONE,
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
            &client, "session-3", &candidates, &control_plane, false, &Progress::NONE,
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
        /// [`Self::start_marking`] with no batch answer marked.
        fn start(
            redirect_status: u16,
            bundle: Vec<u8>,
            objects: HashMap<String, Vec<u8>>,
        ) -> FakeOffloadingRemote {
            FakeOffloadingRemote::start_marking(redirect_status, bundle, objects, None)
        }

        /// `redirect_status` is what `POST /v1/objects/batch` and every
        /// `GET /v1/parcels/{parcel}/subtree/{path}` answer with (this fix's server
        /// half only ever emits `303`, but `307`/`308` from an older or non-conforming head
        /// must be followed identically); `bundle` is served at the redirect target;
        /// `objects` seeds the loose `GET /v1/objects/{hash}` endpoint. `cut` is how the head
        /// cuts its batch answers:
        /// * `Some(FakeCut::Deadline)` marks every batch redirect [`BATCH_CUT_DEADLINE`] and serves
        ///   `bundle`, as a head that ran short of time does;
        /// * `Some(FakeCut::Budget(k))` answers each batch with a bundle of the first `k` requested
        ///   objects it holds, built for that request, marked [`BATCH_CUT_BUDGET`] when it left any
        ///   requested hash out — a head with a budget of `k` objects;
        /// * `Some(FakeCut::Other(value, k))` answers as `Budget(k)` does, but marks `value`.
        fn start_marking(
            redirect_status: u16,
            bundle: Vec<u8>,
            objects: HashMap<String, Vec<u8>>,
            cut: Option<FakeCut>,
        ) -> FakeOffloadingRemote {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            // The presigned target lives on its own listener — another origin, as storage is.
            let storage_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let storage = format!("http://{}", storage_listener.local_addr().unwrap());
            let hits: Arc<Mutex<Vec<(String, String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
            let bundle = Arc::new(Mutex::new(bundle));
            let objects = Arc::new(objects);

            for (listener, at_storage) in [(listener, false), (storage_listener, true)] {
                let hits = Arc::clone(&hits);
                let storage = storage.clone();
                let bundle = Arc::clone(&bundle);
                let objects = Arc::clone(&objects);

                std::thread::spawn(move || {
                    for stream in listener.incoming() {
                        let Ok(stream) = stream else { continue };
                        let hits = Arc::clone(&hits);
                        let storage = storage.clone();
                        let bundle = Arc::clone(&bundle);
                        let objects = Arc::clone(&objects);

                        std::thread::spawn(move || {
                            handle_offloading_request(
                                stream, redirect_status, cut, &storage, at_storage, &bundle, &objects, &hits
                            );
                        });
                    }
                });
            }

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

    /// How a [`FakeOffloadingRemote`] cuts its batch answers; see [`FakeOffloadingRemote::start_marking`].
    #[derive(Clone, Copy)]
    enum FakeCut {
        Budget(usize),
        Deadline,
        Other(&'static str, usize),
    }

    /// Point `bundle` at the first `carry` objects the batch request in `body` asks for and
    /// `objects` holds, as a head with a budget of `carry` objects answers; whether it left any
    /// requested object it holds out.
    fn carried_prefix(body: &[u8],
                      carry: usize,
                      objects: &HashMap<String, Vec<u8>>,
                      bundle: &Mutex<Vec<u8>>) -> bool {
        let request: serde_json::Value = serde_json::from_slice(body).unwrap();
        let requested: Vec<String> = request["hashes"].as_array().unwrap().iter()
            .map(|hash| hash.as_str().unwrap().to_string())
            .collect();
        let held: Vec<&String> = requested.iter().filter(|hash| objects.contains_key(*hash)).collect();

        let mut writer = bundle_utils::BundleWriter::new().unwrap();
        for hash in held.iter().take(carry) {
            writer.push(hash, &objects[*hash]).unwrap();
        }
        *bundle.lock().unwrap() = writer.finish().unwrap();

        held.len() > carry
    }

    /// Handle exactly one connection/request for [`FakeOffloadingRemote`].
    fn handle_offloading_request(
        mut stream: std::net::TcpStream,
        redirect_status: u16,
        cut: Option<FakeCut>,
        storage: &str,
        at_storage: bool,
        bundle: &Mutex<Vec<u8>>,
        objects: &HashMap<String, Vec<u8>>,
        hits: &Mutex<Vec<(String, String, bool)>>,
    ) {
        use std::io::Write;

        let Some((method, path, had_auth, body)) = read_test_request(&mut stream) else { return };
        hits.lock().unwrap().push((method.clone(), path.clone(), had_auth));

        let redirected = (method == "POST" && path == "/v1/objects/batch")
            || (method == "GET" && path.starts_with("/v1/parcels/") && path.contains("/subtree/"));
        if redirected {
            let reason = match redirect_status {
                301 => "Moved Permanently",
                302 => "Found",
                303 => "See Other",
                307 => "Temporary Redirect",
                308 => "Permanent Redirect",
                _ => "Redirect",
            };
            // A `301`/`302` answers with a relative `Location` — this remote's own origin, which
            // serves the bundle only to its token — so those runs also pin that one is resolved
            // against the request and followed authenticated. The rest point at storage.
            let location = match redirect_status {
                301 | 302 => "/responses/bundle".to_string(),
                _ => format!("{}/responses/bundle", storage),
            };
            let mark = match cut {
                Some(_) if method != "POST" => None,
                Some(FakeCut::Deadline) => Some(BATCH_CUT_DEADLINE),
                Some(FakeCut::Budget(carry)) => carried_prefix(&body, carry, objects, bundle).then_some(BATCH_CUT_BUDGET),
                Some(FakeCut::Other(value, carry)) => carried_prefix(&body, carry, objects, bundle).then_some(value),
                None => None,
            };
            let mark = mark.map(|value| format!("{}: {}\r\n", BATCH_HEADER, value)).unwrap_or_default();
            let _ = write!(
                stream,
                "HTTP/1.1 {} {}\r\nLocation: {}\r\n{}Content-Length: 0\r\nConnection: close\r\n\r\n",
                redirect_status, reason, location, mark
            );
            let _ = stream.flush();
            return;
        }

        if method == "GET" && path == "/responses/bundle" && !at_storage && !had_auth {
            let _ = write!(stream, "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = stream.flush();
            return;
        }

        if method == "GET" && path == "/responses/bundle" {
            let bundle = bundle.lock().unwrap().clone();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                Content-Length: {}\r\nConnection: close\r\n\r\n",
                bundle.len()
            );
            let _ = stream.write_all(&bundle);
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

    /// A wave whose first batch the remote marks cut for time asks for no second batch: what the
    /// first carried is kept, and every other object arrives loose. The mark rides the `303`, so
    /// it is read before the redirect is followed. An unmarked answer that is short (the remote
    /// lacks objects) keeps the lane open, and every chunk is asked for. The wave loop is driven
    /// with 4-hash chunks rather than the production 512, so the fixture stays small.
    /// Revert: drop the deadline arm from `fetch_batch` → the second chunk is posted too, and its
    /// answer carries objects it did not ask for, failing the fetch.
    #[test]
    fn a_batch_cut_for_time_closes_the_lane_and_the_rest_arrive_loose() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

        for out_of_time in [true, false] {
            let (hashes, objects, bundle) = {
                let _server_scope = Scratch::new(&format!("deadline-batch-server-{}", out_of_time));
                let hashes: Vec<String> = (0..10).map(|i| store_blob(&format!("wave-{}-{}", out_of_time, i))).collect();
                let objects: HashMap<String, Vec<u8>> = hashes.iter()
                    .map(|hash| (hash.clone(), file_utils::retrieve_object_by_hash(hash).unwrap()))
                    .collect();
                // A cut answer carries a prefix of the first chunk; the short one carries nothing,
                // so it fits every chunk's request.
                let carried: &[String] = if out_of_time { &hashes[..2] } else { &[] };
                (hashes.clone(), objects, bundle_utils::build_partial_bundle(carried).unwrap())
            };

            let _client_scope = Scratch::new(&format!("deadline-batch-client-{}", out_of_time));
            let cut = out_of_time.then_some(FakeCut::Deadline);
            let remote = FakeOffloadingRemote::start_marking(303, bundle, objects, cut);
            let client = RemoteClient::new(&remote.url, None).unwrap();

            let fetched = runtime.block_on(fetch_missing_objects_in_chunks(&client, &hashes, 4))
                .unwrap_or_else(|e| panic!("out of time {}: {}", out_of_time, e));

            assert_eq!(fetched, 10, "out of time {}", out_of_time);
            assert!(hashes.iter().all(|hash| file_utils::does_object_exist(hash).unwrap()));
            let loose = hashes.iter().filter(|hash| remote.hits_for(&format!("/v1/objects/{}", hash)) > 0).count();

            if out_of_time {
                assert_eq!(remote.batch_hits(), 1, "one batch, then no more");
                assert_eq!(loose, 8, "what the batch carried is not fetched again");
                assert_eq!(client.batch_lane_closure(), Some(BatchLaneClosure::OutOfTime));
            } else {
                assert_eq!(remote.batch_hits(), 3, "an unmarked short answer keeps the lane open");
                assert_eq!(loose, 10);
                assert_eq!(client.batch_lane_closure(), None);
            }
        }
    }

    /// A batch the remote cuts at its byte budget is asked for again until the chunk is in: no
    /// object goes loose, the lane stays open, and every chunk of the wave rides the batch. Here
    /// the head carries two objects per answer, so 10 hashes in 4-hash chunks take 2 + 2 + 1
    /// batches.
    /// Revert: let `fetch_missing_objects_in_chunks` stop at a budget cut, as at a deadline cut →
    /// one batch, then the rest loose.
    #[test]
    fn a_batch_cut_at_the_budget_is_asked_again_for_the_rest() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

        let (hashes, objects) = {
            let _server_scope = Scratch::new("budget-batch-server");
            let hashes: Vec<String> = (0..10).map(|i| store_blob(&format!("budget-wave-{}", i))).collect();
            let objects: HashMap<String, Vec<u8>> = hashes.iter()
                .map(|hash| (hash.clone(), file_utils::retrieve_object_by_hash(hash).unwrap()))
                .collect();
            (hashes, objects)
        };

        let _client_scope = Scratch::new("budget-batch-client");
        let remote = FakeOffloadingRemote::start_marking(303, Vec::new(), objects, Some(FakeCut::Budget(2)));
        let client = RemoteClient::new(&remote.url, None).unwrap();

        let fetched = runtime.block_on(fetch_missing_objects_in_chunks(&client, &hashes, 4)).unwrap();

        assert_eq!(fetched, 10);
        assert!(hashes.iter().all(|hash| file_utils::does_object_exist(hash).unwrap()));
        assert_eq!(remote.batch_hits(), 5, "each chunk is asked for until it is in");
        assert!(
            hashes.iter().all(|hash| remote.hits_for(&format!("/v1/objects/{}", hash)) == 0),
            "nothing goes loose"
        );
        assert_eq!(client.batch_lane_closure(), None);
    }

    /// A remote that marks a batch cut at its budget yet carries nothing breaks the protocol's
    /// promise of progress; the client asks it once per chunk and fetches that chunk loose rather
    /// than ask again forever. The lane stays open and the next chunk is still asked for: it is
    /// the one answer that misbehaved. 10 hashes in 4-hash chunks take one batch per chunk.
    /// Revert: drop the no-progress check from `fetch_missing_objects_in_chunks` → it posts the
    /// same chunk without end, and the fetch never returns; let it end the wave → one batch.
    #[test]
    fn a_budget_cut_that_carries_nothing_is_not_asked_again() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

        let (hashes, objects) = {
            let _server_scope = Scratch::new("empty-budget-batch-server");
            let hashes: Vec<String> = (0..10).map(|i| store_blob(&format!("empty-budget-{}", i))).collect();
            let objects: HashMap<String, Vec<u8>> = hashes.iter()
                .map(|hash| (hash.clone(), file_utils::retrieve_object_by_hash(hash).unwrap()))
                .collect();
            (hashes, objects)
        };

        let _client_scope = Scratch::new("empty-budget-batch-client");
        let remote = FakeOffloadingRemote::start_marking(303, Vec::new(), objects, Some(FakeCut::Budget(0)));
        let client = RemoteClient::new(&remote.url, None).unwrap();

        let fetch = fetch_missing_objects_in_chunks(&client, &hashes, 4);
        let fetched = runtime.block_on(async { tokio::time::timeout(std::time::Duration::from_secs(20), fetch).await })
            .expect("the client stops asking")
            .unwrap();

        assert_eq!(fetched, 10);
        assert!(hashes.iter().all(|hash| file_utils::does_object_exist(hash).unwrap()));
        assert_eq!(remote.batch_hits(), 3, "every chunk is asked for once");
        assert_eq!(client.batch_lane_closure(), None);
    }

    /// A batch answer marked with a value the protocol does not name, such as an older head's
    /// `truncated`, carries no mark: what came is kept, the chunk is not asked for again, the lane
    /// stays open and the next chunk is asked for. Each answer carries one object, so 10 hashes in
    /// 4-hash chunks take three batches and seven loose fetches.
    /// Revert: map an unknown value in `fetch_batch` to a budget cut → each chunk is asked for
    /// until it is in, ten batches; to a deadline cut → one batch, and the lane closes.
    #[test]
    fn a_batch_marked_with_an_unknown_value_is_not_cut() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

        let (hashes, objects) = {
            let _server_scope = Scratch::new("unknown-mark-batch-server");
            let hashes: Vec<String> = (0..10).map(|i| store_blob(&format!("unknown-mark-{}", i))).collect();
            let objects: HashMap<String, Vec<u8>> = hashes.iter()
                .map(|hash| (hash.clone(), file_utils::retrieve_object_by_hash(hash).unwrap()))
                .collect();
            (hashes, objects)
        };

        let _client_scope = Scratch::new("unknown-mark-batch-client");
        let remote = FakeOffloadingRemote::start_marking(303, Vec::new(), objects, Some(FakeCut::Other("truncated", 1)));
        let client = RemoteClient::new(&remote.url, None).unwrap();

        let fetched = runtime.block_on(fetch_missing_objects_in_chunks(&client, &hashes, 4)).unwrap();

        assert_eq!(fetched, 10);
        assert!(hashes.iter().all(|hash| file_utils::does_object_exist(hash).unwrap()));
        assert_eq!(remote.batch_hits(), 3, "each chunk is asked for once");
        let loose = hashes.iter().filter(|hash| remote.hits_for(&format!("/v1/objects/{}", hash)) > 0).count();
        assert_eq!(loose, 7, "what each batch carried is not fetched again");
        assert_eq!(client.batch_lane_closure(), None);
    }

    /// The subtree endpoint may be offloaded the same way: `REMOTE_PROTOCOL.md` lets a
    /// storage-backed head answer it with a `307` to a presigned bundle. It rides a no-redirect
    /// client, so the follow is by hand, and the bearer token never leaves the remote's origin —
    /// but goes along to it, where the fake refuses the bundle without one. Every redirect status
    /// an auto-following client took is still taken, a relative `Location` too.
    #[test]
    fn fetch_subtree_follows_a_redirect_by_hand_without_leaking_auth() {
        use crate::enums::dir_entry_type::DirEntryType;

        for redirect_status in [301u16, 302, 303, 307, 308] {
            let (blob, tree, bundle) = {
                let _server_scope = Scratch::new(&format!("offload-subtree-server-{}", redirect_status));
                let blob = store_blob(&format!("leaf-{}", redirect_status));
                let tree = store_tree(&[("leaf", &blob, DirEntryType::Normal)]);
                let bundle = bundle_utils::build_partial_bundle(&[tree.clone(), blob.clone()]).unwrap();
                (blob, tree, bundle)
            };

            let _client_scope = Scratch::new(&format!("offload-subtree-client-{}", redirect_status));
            let remote = FakeOffloadingRemote::start(redirect_status, bundle, HashMap::new());
            let client = RemoteClient::new(&remote.url, Some("shhh".to_string())).unwrap();
            let subtree_path = format!("/v1/parcels/{}/subtree/src", "p".repeat(64));

            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let stats = runtime.block_on(client.fetch_subtree(&"p".repeat(64), "src", &tree))
                .unwrap_or_else(|e| panic!("status {}: the redirect must be followed: {}", redirect_status, e));

            assert!(stats.is_some(), "status {}: a followed redirect is not an absent endpoint", redirect_status);
            assert!(file_utils::does_object_exist(&tree).unwrap(), "status {}", redirect_status);
            assert!(file_utils::does_object_exist(&blob).unwrap(), "status {}", redirect_status);
            assert_eq!(remote.hits_for(&subtree_path), 1, "status {}", redirect_status);
            assert_eq!(remote.hits_for("/responses/bundle"), 1, "status {}", redirect_status);
            assert!(remote.any_had_auth(&subtree_path), "status {}", redirect_status);
            let same_origin = matches!(redirect_status, 301 | 302);
            assert_eq!(
                remote.any_had_auth("/responses/bundle"), same_origin,
                "status {}: the follow-up carries this remote's bearer token to its own origin and \
                never to another one", redirect_status
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

    /// A handshake whose pallet head is not the canonical spelling of a hash is refused before
    /// any caller can write it into a local ref. Uppercase is the case that matters: on a
    /// case-insensitive filesystem it finds a held object's file, skips the fetch that would
    /// verify it, and the ref it leaves behind is refused on every later read. A tombstone's head
    /// is held to the same spelling. Revert: check the live heads alone → the tombstone passes.
    #[test]
    fn fetch_info_refuses_a_handshake_with_a_non_canonical_pallet_head() {
        let serve = |head: String, tombstone: String| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let body = serde_json::json!({
                "protocol": PROTOCOL_VERSION,
                "default_pallet": "main",
                "pallets": { "main": head },
                "trust": null,
                "tombstones": { "gone": tombstone },
            }).to_string();

            std::thread::spawn(move || {
                use std::io::Write;

                if let Ok((mut stream, _)) = listener.accept() {
                    let _ = read_test_request(&mut stream);
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                        Connection: close\r\n\r\n{}",
                        body.len(), body
                    );
                    let _ = stream.flush();
                }
            });

            let client = RemoteClient::new(&url, None).unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            runtime.block_on(client.fetch_info())
        };

        let hash = "ab".repeat(32);
        assert!(serve(hash.clone(), hash.clone()).is_ok(), "a canonical head must pass");

        let error = serve(hash.to_uppercase(), hash.clone()).err().expect("an uppercase head must be refused");
        assert!(error.contains("not an object hash"), "{}", error);
        assert!(!error.contains(&hash.to_uppercase()), "the remote's value is not repeated: {}", error);

        let error = serve(hash.clone(), hash.to_uppercase()).err().expect("an uppercase tombstone must be refused");
        assert!(error.contains("not an object hash"), "{}", error);
    }

    impl BatchOutcome {
        /// The stats of a [`BatchOutcome::Imported`] outcome, or a panic naming `message`.
        fn expect(self, message: &str) -> bundle_utils::ImportStats {
            match self {
                BatchOutcome::Imported(stats) => stats,
                other => panic!("{}: {:?}", message, other),
            }
        }
    }

    /// A remote that answers its `n`th connection with `answers[n]` verbatim (the last one again
    /// once they run out), counting every request it reads.
    fn start_answering_remote(answers: Vec<&'static str>) -> (String, Arc<Mutex<usize>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(0usize));
        let counted = Arc::clone(&requests);

        std::thread::spawn(move || {
            use std::io::Write;

            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                if read_test_request(&mut stream).is_none() {
                    continue;
                }
                let index = {
                    let mut count = counted.lock().unwrap();
                    *count += 1;
                    *count - 1
                };
                let _ = stream.write_all(answers[index.min(answers.len() - 1)].as_bytes());
                let _ = stream.flush();
            }
        });

        (url, requests)
    }

    /// Every route of the hosted API shares one throttle, so every read meets the same `429`s the
    /// loose GETs do: each waits one out and re-sends, and only the answer after it decides (here a
    /// `404`, which each of these reads takes as an answer). Without the retry the first `429` is
    /// the command's error.
    #[test]
    fn the_reads_wait_out_a_throttle() {
        const THROTTLED: &str = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\n\
            Content-Length: 0\r\nConnection: close\r\n\r\n";
        const ABSENT: &str = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

        let (url, requests) = start_answering_remote(vec![THROTTLED, THROTTLED, ABSENT]);
        let client = fast_client(&url);
        let outcome = runtime.block_on(client.fetch_batch(&["a".repeat(64)]))
            .expect("a throttled batch POST is re-sent, not failed");
        assert!(matches!(outcome, BatchOutcome::EndpointAbsent), "{:?}", outcome);
        assert_eq!(*requests.lock().unwrap(), 3, "two throttled POSTs, then the one that answered");

        let (url, requests) = start_answering_remote(vec![THROTTLED, ABSENT]);
        let client = fast_client(&url);
        let outcome = runtime.block_on(client.fetch_subtree(&"p".repeat(64), "src", &"t".repeat(64)))
            .expect("a throttled subtree GET is re-sent, not failed");
        assert!(outcome.is_none(), "the 404 after the throttle is the absence signal");
        assert_eq!(*requests.lock().unwrap(), 2);

        let (url, requests) = start_answering_remote(vec![THROTTLED, ABSENT]);
        let client = fast_client(&url);
        let signature = runtime.block_on(client.fetch_signature(&"p".repeat(64)))
            .expect("a throttled signature GET is re-sent, not failed");
        assert!(signature.is_none(), "the 404 after the throttle is an unsigned parcel");
        assert_eq!(*requests.lock().unwrap(), 2);

        let (url, requests) = start_answering_remote(vec![THROTTLED, ABSENT]);
        let client = fast_client(&url);
        let scratch = Scratch::new("throttled-bundle");
        let fetched = runtime.block_on(download_whole_bundle(&client, &scratch.root.join("bundle")))
            .expect("a throttled bundle GET is re-sent, not failed");
        assert_eq!(fetched, BundleDownload::Absent, "the 404 after the throttle is no bundle");
        assert_eq!(*requests.lock().unwrap(), 2);

        let (url, requests) = start_answering_remote(vec![THROTTLED, ABSENT]);
        let client = fast_client(&url);
        let Err(error) = runtime.block_on(client.fetch_info()) else { panic!("the handshake 404 is an error") };
        assert!(error.contains("404"), "the answer after the throttle decides, not the 429: {}", error);
        assert_eq!(*requests.lock().unwrap(), 2);
    }

    #[test]
    fn origin_relative_path_keeps_path_and_query_and_drops_userinfo() {
        let url = reqwest::Url::parse("http://user:pw@127.0.0.1:9/responses/bundle?sig=a%2Fb").unwrap();
        assert_eq!(origin_relative_path(&url), "/responses/bundle?sig=a%2Fb");
        let url = reqwest::Url::parse("http://127.0.0.1:9/responses/bundle").unwrap();
        assert_eq!(origin_relative_path(&url), "/responses/bundle");
    }

    /// The wait only ever grows by up to half the backoff, and it does vary: a wave of fetches
    /// refused together must not all come back in the same instant.
    #[test]
    fn jittered_lengthens_the_wait_by_a_varying_share_of_half_the_backoff() {
        let wait = std::time::Duration::from_secs(1);
        let backoff = std::time::Duration::from_millis(400);
        let samples: Vec<std::time::Duration> = (0..64).map(|_| jittered(wait, backoff)).collect();

        assert!(samples.iter().all(|sample| *sample >= wait && *sample <= wait + backoff / 2), "{:?}", samples);
        assert!(samples.iter().any(|sample| *sample != samples[0]), "64 identical waits: {:?}", samples);
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

    /// **The absence lane.** A head that predates the batch endpoint answers the `POST` with a
    /// `404`, and `fetch_batch` must turn that into [`BatchOutcome::EndpointAbsent`] and close the
    /// lane — the signal that sends its caller to loose fetches for the rest of the command.
    /// `fetch_batch_degrades_against_a_silent_remote` pins the other degrading outcome.
    ///
    /// The `500` half is what makes this discriminate. An implementation that mapped *any*
    /// non-success status to `Ok(None)` — the plausible over-broad version of this lane — passes
    /// the `404` half alone and fails here, because a server error must surface as an error and
    /// not as "this head is too old".
    ///
    /// A `405` is absence too, as it is for `fetch_subtree`: both go through `endpoint_absent`.
    #[test]
    fn fetch_batch_maps_a_404_to_the_absence_signal_and_a_500_to_an_error() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

        for (status, reason) in [(404u16, "Not Found"), (405, "Method Not Allowed")] {
            let absent = RemoteClient::new(&start_status_only_remote(status, reason), None).unwrap();
            let outcome = runtime.block_on(absent.fetch_batch(&["a".repeat(64)]))
                .unwrap_or_else(|e| panic!("a {} is the absence signal, not a failure: {}", status, e));
            assert!(
                matches!(outcome, BatchOutcome::EndpointAbsent),
                "a {} must produce the absence signal the caller falls back on, not bundle bytes: \
                {:?}", status, outcome
            );
            assert!(!absent.batch_lane_open(), "a {} closes the batch lane", status);
            assert_eq!(
                absent.batch_lane_closure(), Some(BatchLaneClosure::EndpointAbsent),
                "a {} closes the lane for the endpoint's absence, not for a slow head", status
            );
        }

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
        assert!(broken.batch_lane_open(), "a server error is no reason to stop batching");
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
    /// hang forever, against a remote that redirects and then goes silent. This station once rode
    /// an auto-following client with no bound at all, which this fixture would hang against
    /// forever.
    #[test]
    fn fetch_batch_redirect_follow_times_out_against_a_silent_remote() {
        let remote = RedirectThenSilentRemote::start();
        let client = fast_client(&remote.url);
        let hard_ceiling = FAST.connect + FAST.object_read + std::time::Duration::from_secs(20);

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
        let gap = std::time::Duration::from_millis(1_500);
        let _store = Scratch::new("batch-redirect-drip");
        let (chunks, hash) = drip_bundle(4);
        // Four gaps run past the effective loose budget (connect + object read) in total, while
        // no single gap comes near it — the sole property this fixture pins. Each gap does exceed
        // the *tight* [`Posture::BoundedReads`] effective budget (connect + read) — deliberately:
        // that gap width is what makes the BoundedReads mutation fail this test, the
        // discriminating property this fixture exists for.
        let total_duration = gap * (chunks.len() as u32);
        let effective_loose_budget = FAST.connect + FAST.object_read;
        assert!(
            total_duration > effective_loose_budget,
            "the fixture must actually outlast the bound under test"
        );
        assert!(
            gap > FAST.connect + FAST.read && gap * 2 < effective_loose_budget,
            "each gap must outlast the tight budget and stay well inside the loose one"
        );

        let url = start_batch_redirect_then_drip_remote(chunks, gap);
        let client = fast_client(&url);
        let outer_ceiling = total_duration + std::time::Duration::from_secs(20);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(outer_ceiling, client.fetch_batch(&[hash.clone()])).await
        });

        let stats = outcome
            .unwrap_or_else(|_| panic!(
                "fetch_batch hung past its own generous outer ceiling {:?}", outer_ceiling
            ))
            .unwrap_or_else(|e| panic!(
                "a slow-but-steady bundle must succeed — it was never silent, so it must never be \
                treated as a stall: {}", e
            ))
            .expect("a served bundle is not the absence signal");

        assert_eq!(stats.stored_objects, 1, "the full bundle must arrive intact despite the drip");
        assert!(file_utils::does_object_exist(&hash).unwrap(), "the dripped object landed in this store");
    }

    /// The designed transport for chunks (DESIGN.html §9.4b: "clone, lower and scope fetch all fetch
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

    /// A head whose batch endpoint never answers and whose loose object endpoint serves
    /// `objects`, answering each object's first `refusals` GETs with `refusal_status` (and a
    /// `Retry-After` of `retry_after` seconds when given) before serving it. Records each
    /// request's path, so a test can count what the client asked for.
    struct ScriptedObjectRemote {
        url: String,
        paths: Arc<Mutex<Vec<String>>>,
        /// Dropped with the fixture, which is what releases a parked batch handler.
        _park: std::sync::mpsc::Sender<()>,
    }

    impl ScriptedObjectRemote {
        fn start(
            objects: HashMap<String, Vec<u8>>,
            refusal_status: u16,
            refusals: usize,
            retry_after: Option<u64>,
        ) -> ScriptedObjectRemote {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let accepted_paths = Arc::clone(&paths);
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            let park = Arc::new(Mutex::new(rx));
            let objects = Arc::new(objects);

            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let paths = Arc::clone(&accepted_paths);
                    let park = Arc::clone(&park);
                    let objects = Arc::clone(&objects);

                    std::thread::spawn(move || {
                        use std::io::Write;

                        let Some((method, path, _, _)) = read_test_request(&mut stream) else { return };
                        let earlier = {
                            let mut paths = paths.lock().unwrap();
                            let earlier = paths.iter().filter(|seen| **seen == path).count();
                            paths.push(path.clone());
                            earlier
                        };

                        if method == "POST" && path == "/v1/objects/batch" {
                            let _ = park.lock().unwrap().recv();
                            return;
                        }

                        let bytes = path.strip_prefix("/v1/objects/").and_then(|hash| objects.get(hash));
                        match bytes {
                            Some(_) if earlier < refusals => {
                                let retry_after = retry_after
                                    .map(|seconds| format!("Retry-After: {}\r\n", seconds))
                                    .unwrap_or_default();
                                let _ = write!(
                                    stream,
                                    "HTTP/1.1 {} Refused\r\n{}Content-Length: 0\r\nConnection: close\r\n\r\n",
                                    refusal_status, retry_after
                                );
                            }
                            Some(bytes) => {
                                let _ = write!(
                                    stream,
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                                    Content-Length: {}\r\nConnection: close\r\n\r\n",
                                    bytes.len()
                                );
                                let _ = stream.write_all(bytes);
                            }
                            None => {
                                let _ = write!(
                                    stream,
                                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                );
                            }
                        }
                        let _ = stream.flush();
                    });
                }
            });

            ScriptedObjectRemote { url, paths, _park: tx }
        }

        fn hits_for(&self, path: &str) -> usize {
            self.paths.lock().unwrap().iter().filter(|seen| *seen == path).count()
        }
    }

    /// `count` chunk objects, built but not stored, and the map that serves them.
    fn unstored_chunks(tag: &str, count: usize) -> (Vec<String>, HashMap<String, Vec<u8>>) {
        let mut hashes = Vec::new();
        let mut objects = HashMap::new();
        for i in 0..count {
            let chunk = LooseObjectBuilder::build_chunk(&crate::model::chunk::Chunk {
                content: format!("{} chunk {}", tag, i).into_bytes(),
            });
            hashes.push(chunk.hash.clone());
            objects.insert(chunk.hash, chunk.content);
        }
        (hashes, objects)
    }

    /// The lane is sticky across waves: a batch that degrades in the first wave sends that wave
    /// loose, and the second wave of the same client goes straight to loose GETs without asking
    /// the batch endpoint again. Without the lane, every wave of a walk against a slow head would
    /// pay the whole head wait before falling back.
    #[test]
    fn a_degraded_batch_sends_every_later_wave_loose() {
        let _scratch = Scratch::new("degraded-batch-lane");
        let (hashes, objects) = unstored_chunks("lane", 4);
        let remote = ScriptedObjectRemote::start(objects, 429, 0, None);
        let client = fast_client(&remote.url);
        let worker = client.clone();

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let first = runtime.block_on(fetch_missing_objects(&client, &hashes[..2]))
            .expect("a degraded batch falls back to loose GETs");
        assert_eq!(first, 2);
        assert_eq!(remote.hits_for("/v1/objects/batch"), 1, "the first wave asked the batch endpoint");

        let second = runtime.block_on(fetch_missing_objects(&worker, &hashes[2..]))
            .expect("the second wave fetches loose");
        assert_eq!(second, 2);
        assert_eq!(
            remote.hits_for("/v1/objects/batch"), 1,
            "a closed lane is not asked again, by this client or a clone of it"
        );
        for hash in &hashes {
            assert!(file_utils::does_object_exist(hash).unwrap(), "{} landed", hash);
            assert_eq!(remote.hits_for(&format!("/v1/objects/{}", hash)), 1, "{} was fetched loose once", hash);
        }
    }

    /// A throttled object fetch waits the remote's `Retry-After` and then succeeds. The figure is
    /// a second, five times the first backoff step, so a retry that ignored the header would
    /// finish well under it.
    #[test]
    fn fetch_object_waits_out_a_throttle_and_honours_retry_after() {
        let (hashes, objects) = unstored_chunks("throttle", 1);
        let expected = objects[&hashes[0]].clone();
        let remote = ScriptedObjectRemote::start(objects, 429, 1, Some(1));
        let client = fast_client(&remote.url);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let started = std::time::Instant::now();
        let bytes = runtime.block_on(client.fetch_object(&hashes[0]))
            .expect("one 429 is retried past");
        let elapsed = started.elapsed();

        assert_eq!(bytes, expected);
        assert_eq!(remote.hits_for(&format!("/v1/objects/{}", hashes[0])), 2, "one retry");
        assert!(
            elapsed >= std::time::Duration::from_secs(1),
            "elapsed {:?}: the remote's Retry-After of 1s was not waited", elapsed
        );
    }

    /// A remote that keeps throttling is retried, and then the last `429` is the error, before
    /// the throttle patience runs out rather than after it.
    #[test]
    fn fetch_object_gives_up_on_a_persistent_throttle_within_its_patience() {
        let (hashes, objects) = unstored_chunks("persistent-throttle", 1);
        let remote = ScriptedObjectRemote::start(objects, 429, usize::MAX, None);
        let client = fast_client(&remote.url);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let started = std::time::Instant::now();
        let error = runtime.block_on(client.fetch_object(&hashes[0]))
            .expect_err("a throttle that never lifts must fail");
        let elapsed = started.elapsed();

        assert!(error.contains("(429)"), "the error names the remote's answer: {}", error);
        assert!(error.contains("after retrying"), "the error says it retried: {}", error);
        assert!(!is_transient_failure(&error), "a 429 is not reported as transient: {}", error);
        assert!(
            remote.hits_for(&format!("/v1/objects/{}", hashes[0])) > 1,
            "the throttle was retried before giving up"
        );
        assert!(
            elapsed < FAST.throttle_patience,
            "elapsed {:?} overran the throttle patience {:?}", elapsed, FAST.throttle_patience
        );
    }

    /// Only a throttle is retried: a `503` from the object endpoint fails on the first answer, as
    /// it always did.
    #[test]
    fn fetch_object_does_not_retry_a_server_error() {
        let (hashes, objects) = unstored_chunks("server-error", 1);
        let remote = ScriptedObjectRemote::start(objects, 503, 1, Some(0));
        let client = fast_client(&remote.url);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let error = runtime.block_on(client.fetch_object(&hashes[0]))
            .expect_err("a 503 is not retried here");

        assert!(error.contains("(503)"), "{}", error);
        assert_eq!(remote.hits_for(&format!("/v1/objects/{}", hashes[0])), 1);
    }

    /// An unavailable server and a throttle outlasting the patience are a remote that could not
    /// serve the object now; any other refusal, a `500` included, is its answer. Revert: classify
    /// every status as an answer → the `503` and the `429` are refused; every `5xx` as unavailable
    /// → the `500` is not.
    #[test]
    fn fetch_object_classified_tells_a_remote_that_cannot_serve_now_from_one_that_refuses() {
        let classify = |status: u16| {
            let (hashes, objects) = unstored_chunks(&format!("classified-{}", status), 1);
            let remote = ScriptedObjectRemote::start(objects, status, usize::MAX, Some(0));
            let client = fast_client(&remote.url);

            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            runtime.block_on(client.fetch_object_classified(&hashes[0]))
        };

        for status in [503, 429] {
            assert!(matches!(classify(status), ObjectFetch::Unreachable(_)), "a {} cannot serve now", status);
        }

        for status in [500, 404] {
            assert!(matches!(classify(status), ObjectFetch::Refused(_)), "a {} is the remote's answer", status);
        }
    }

    /// The throttle wait is the remote's `Retry-After` when it sent one, never under the backoff.
    #[test]
    fn throttle_wait_floors_retry_after_at_the_backoff() {
        let backoff = std::time::Duration::from_millis(400);
        assert_eq!(throttle_wait(None, backoff), backoff);
        assert_eq!(throttle_wait(Some(std::time::Duration::ZERO), backoff), backoff);
        assert_eq!(throttle_wait(Some(std::time::Duration::from_secs(2)), backoff), std::time::Duration::from_secs(2));
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
        let stats = pack_utils::compact(false, false, false, &crate::util::progress_utils::Progress::NONE).unwrap();
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
    // directly, structurally cover all of them, including the real task body that motivates the
    // fix: `object_utils::store_object_bytes` (see `join_all`'s doc for the exact no-await shape
    // that motivates this). The
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
    /// the real task body `join_all`/`drain_remaining` guard (`object_utils::store_object_bytes`;
    /// see `join_all`'s doc for the exact no-await shape being mirrored here). Sets `started` as soon
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
    // The fixtures that wait a budget out build their client with [`FAST`], whose budgets are
    // milliseconds, so each one fires in about a second rather than a minute. The `TEST_*`
    // constants mirror the production values as literals, [`PRODUCTION_MIRROR`] assembles them,
    // and `production_budgets_are_the_documented_figures` pins `Budgets::production` to it; tests
    // that only compute a production budget, never wait one out, still read them.
    // -----------------------------------------------------------------------------------

    /// Mirrors production `REMOTE_CONNECT_TIMEOUT`, for the reason `TEST_TIGHT_READ_TIMEOUT`'s doc
    /// gives.
    const TEST_DIRECT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    /// Mirrors production `REMOTE_CONNECT_TIMEOUT_TOR`, likewise.
    const TEST_TOR_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
    /// Mirrors production `REMOTE_READ_TIMEOUT` as a literal rather than referring to it, as every
    /// `TEST_*` constant here does for its own production constant: an assertion built on a
    /// mirror keeps meaning what it meant when it was written, so a change to the production
    /// figure has to be re-affirmed here instead of moving both sides of an assertion together.
    const TEST_TIGHT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    /// Mirrors production `FETCH_OBJECT_READ_TIMEOUT`, likewise.
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
    /// Mirrors production `RETRYABLE_HEAD_PATIENCE`, for the reason `TEST_TIGHT_READ_TIMEOUT`'s doc
    /// gives.
    const TEST_RETRYABLE_HEAD_PATIENCE: std::time::Duration = std::time::Duration::from_secs(30);
    /// Mirrors production `COMMIT_DEADLINE`, for the reason `TEST_TIGHT_READ_TIMEOUT`'s doc gives.
    const TEST_COMMIT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);
    /// Mirrors production `COMMIT_ATTEMPT_ALLOWANCE`, for the reason `TEST_TIGHT_READ_TIMEOUT`'s doc
    /// gives.
    const TEST_COMMIT_ATTEMPT_ALLOWANCE: std::time::Duration = std::time::Duration::from_secs(30);
    /// Mirrors production `MIN_COMMIT_ATTEMPT`, for the reason `TEST_TIGHT_READ_TIMEOUT`'s doc gives.
    const TEST_MIN_COMMIT_ATTEMPT: std::time::Duration = std::time::Duration::from_secs(2);
    /// Mirrors production `BATCH_HEAD_PATIENCE` — its own constant, not a reference to the
    /// production one, for the reason `TEST_TIGHT_READ_TIMEOUT`'s doc gives: an assertion built on
    /// a mirror keeps meaning what it meant when it was written, and a change to the production
    /// constant has to be re-affirmed here rather than moving both sides of an assertion together.
    const TEST_BATCH_HEAD_PATIENCE: std::time::Duration = std::time::Duration::from_secs(45);
    /// Mirrors production `FETCH_THROTTLE_PATIENCE`, for the reason `TEST_TIGHT_READ_TIMEOUT`'s doc
    /// gives.
    const TEST_FETCH_THROTTLE_PATIENCE: std::time::Duration = std::time::Duration::from_secs(30);

    /// The budgets a waiting fixture builds its client with. Every value is distinct from every
    /// other one and from every production figure, and so is every sum a budget is armed as
    /// (connect plus one allowance), so an arm site that reads the wrong field, or a production
    /// constant in place of the instance, names a figure the assertion does not expect. The
    /// ratios follow production's where a test depends on them: `batch_head_patience` stays under
    /// `object_read`, `read` under `object_read`, and the Tor connect bound far above the direct
    /// one. A test that widens `read` past `object_read` for its own margin (`WIDE_READ`, the
    /// resolve survive test) gives that up, so it must not touch the object-read path.
    ///
    /// A test that separates an armed budget from a rival by elapsed time needs the two at least a
    /// second apart, so the midpoint leaves half a second or more for a loaded runner's
    /// scheduling. Where [`FAST`]'s own spacing is closer than that for the pair a test separates,
    /// the test widens the budgets it needs rather than every fixture paying for it.
    const FAST: Budgets = Budgets {
        connect: std::time::Duration::from_millis(330),
        read: std::time::Duration::from_millis(670),
        object_read: std::time::Duration::from_millis(4_100),
        batch_head_patience: std::time::Duration::from_millis(3_050),
        error_body_read: std::time::Duration::from_millis(730),
        upload_silence: std::time::Duration::from_millis(2_110),
        retryable_head_patience: std::time::Duration::from_millis(1_690),
        commit_deadline: std::time::Duration::from_millis(5_230),
        commit_attempt_allowance: std::time::Duration::from_millis(1_270),
        min_commit_attempt: std::time::Duration::from_millis(90),
        throttle_patience: std::time::Duration::from_millis(2_590),
    };

    /// [`FAST`]'s connect bound for a client that routes through Tor, standing in for
    /// `REMOTE_CONNECT_TIMEOUT_TOR` the way `FAST.connect` stands in for `REMOTE_CONNECT_TIMEOUT`.
    const FAST_TOR_CONNECT: std::time::Duration = std::time::Duration::from_millis(3_900);

    /// [`FAST`] for a direct dial, or with [`FAST_TOR_CONNECT`] for one through Tor — the
    /// `budgets_for` that [`RemoteClient::with_budgets_for`] takes.
    fn fast_budgets(routes_through_tor: bool) -> Budgets {
        if routes_through_tor { Budgets { connect: FAST_TOR_CONNECT, ..FAST } } else { FAST }
            .checked()
    }

    /// A read budget for the tests that separate the tight silence budget from a nearby figure by
    /// elapsed time, where [`FAST`]'s own `read` leaves too little room. Distinct from every
    /// [`FAST`] figure and every production one.
    const WIDE_READ: std::time::Duration = std::time::Duration::from_millis(3_670);

    /// [`fast_budgets`] with [`WIDE_READ`], for a fixture that must tell a connect-phase stall
    /// from a read-phase one: `read_timeout` is armed before the connector is polled and spans
    /// connect plus read, so the read budget is the room between the two firings.
    fn fast_budgets_wide_read(routes_through_tor: bool) -> Budgets {
        Budgets { read: WIDE_READ, ..fast_budgets(routes_through_tor) }.checked()
    }

    /// The budgets the stalled-error-body tests arm with. [`FAST`] has no error-body budget a
    /// second clear of every rival on both sides (see [`error_body_bracket`]), and a
    /// connect bound as small as [`FAST`]'s leaves the connect-forgotten rival under a second
    /// below whatever allowance is picked, so these widen both. [`error_body_bracket`] checks their
    /// separation from every rival it lists rather than this doc stating it.
    fn stalled_error_body_budgets() -> Budgets {
        Budgets {
            connect: std::time::Duration::from_millis(2_900),
            error_body_read: std::time::Duration::from_millis(8_550),
            ..FAST
        }
        .checked()
    }

    /// The elapsed-time bracket of a timed error-body test on a client carrying `budgets`, computed
    /// before it starts waiting. The lower bound is the armed error-body budget, summed from the
    /// injected budgets rather than through `error_body_read_budget`, so a bug in that helper
    /// cannot move the elapsed time and the bound in lockstep. The upper bound is the midpoint
    /// toward the nearest figure above it that a single misarm could fire at instead: any
    /// allowance of this instance or of [`PRODUCTION_MIRROR`], on its own or folded with any
    /// connect bound (this instance's or a production one); each connect bound alone; and `extra`,
    /// the test's own outer deadlines. Every rival is asserted to be at least a second from the
    /// armed budget on either side (a connect bound equal to a production one lists the armed sum
    /// twice and fails here), so the midpoint leaves half a second for a loaded runner and a misarm
    /// below fires a second before the lower bound. The outer ceiling, which turns a lost bound
    /// into a failure rather than a hang, is asserted to sit above the midpoint, or a misarm
    /// between the two would read as a hang.
    fn error_body_bracket(budgets: Budgets, extra: &[std::time::Duration]) -> Bracket {
        let armed = budgets.connect + budgets.error_body_read;
        // Destructured without `..`, so a field added to `Budgets` has to be placed here.
        let allowances = |b: Budgets| {
            let Budgets {
                connect: _, read, object_read, batch_head_patience, error_body_read, upload_silence,
                retryable_head_patience, commit_deadline, commit_attempt_allowance,
                min_commit_attempt, throttle_patience,
            } = b;
            [read, object_read, batch_head_patience, error_body_read, upload_silence,
                retryable_head_patience, commit_deadline, commit_attempt_allowance,
                min_commit_attempt, throttle_patience]
        };
        let connects = [budgets.connect, PRODUCTION_MIRROR.connect, PRODUCTION_MIRROR_TOR.connect];
        let mut rivals: Vec<std::time::Duration> = allowances(budgets).into_iter()
            .chain(allowances(PRODUCTION_MIRROR))
            .flat_map(|allowance| {
                std::iter::once(allowance).chain(connects.map(|connect| connect + allowance))
            })
            .chain(connects)
            .chain(extra.iter().copied())
            .collect();
        // The armed budget itself is the one entry that is not a rival; any other at the same
        // figure is, and must fail the separation below.
        let own = rivals.iter().position(|&rival| rival == armed).expect("the armed sum is listed");
        rivals.swap_remove(own);
        let second = std::time::Duration::from_secs(1);
        // An exact coincidence first: a connect bound or allowance equal to a production one, or
        // an outer deadline at the armed figure, is the likeliest cause and the least obvious.
        assert!(
            !rivals.contains(&armed),
            "a rival coincides exactly with the armed error-body budget {:?}, so a misarm firing \
            at it cannot be told apart", armed
        );
        let too_close = rivals.iter()
            .find(|&&rival| rival + second > armed && rival < armed + second);
        if let Some(rival) = too_close {
            panic!(
                "the armed error-body budget {:?} is within a second of the rival {:?}, too close \
                to separate by elapsed time", armed, rival
            );
        }
        let nearest = rivals.into_iter().filter(|&rival| rival > armed).min()
            .expect("the production allowances folded with a connect bound exceed any test budget");
        let upper_bound = armed + (nearest - armed) / 2;
        let outer_ceiling = armed + std::time::Duration::from_secs(10);
        assert!(
            upper_bound < outer_ceiling,
            "the midpoint {:?} must sit under the {:?} ceiling, or a misarm between them reads as \
            a hang", upper_bound, outer_ceiling
        );
        Bracket { lower_bound: armed, upper_bound, nearest_rival: nearest, outer_ceiling }
    }

    /// What [`error_body_bracket`] returns, its fields named for the reader: every one is a
    /// duration, so nothing but review stops a test asserting against the wrong one.
    struct Bracket {
        /// The elapsed time must be at least this, the armed error-body budget.
        lower_bound: std::time::Duration,
        /// The elapsed time must be under this.
        upper_bound: std::time::Duration,
        /// The figure `upper_bound` is the midpoint toward, for the failure message.
        nearest_rival: std::time::Duration,
        /// The test's own timeout around the call.
        outer_ceiling: std::time::Duration,
    }

    /// A direct-dial client carrying [`FAST`].
    fn fast_client(url: &str) -> RemoteClient {
        RemoteClient::new_test_with_budgets(url, FAST)
    }

    /// The direct-dial production budgets, built from the mirrors rather than the production
    /// constants, for the reason `TEST_TIGHT_READ_TIMEOUT`'s doc gives.
    const PRODUCTION_MIRROR: Budgets = Budgets {
        connect: TEST_DIRECT_CONNECT_TIMEOUT,
        read: TEST_TIGHT_READ_TIMEOUT,
        object_read: TEST_LOOSE_READ_TIMEOUT,
        batch_head_patience: TEST_BATCH_HEAD_PATIENCE,
        error_body_read: TEST_ERROR_BODY_READ_TIMEOUT,
        upload_silence: TEST_UPLOAD_SILENCE_BUDGET,
        retryable_head_patience: TEST_RETRYABLE_HEAD_PATIENCE,
        commit_deadline: TEST_COMMIT_DEADLINE,
        commit_attempt_allowance: TEST_COMMIT_ATTEMPT_ALLOWANCE,
        min_commit_attempt: TEST_MIN_COMMIT_ATTEMPT,
        throttle_patience: TEST_FETCH_THROTTLE_PATIENCE,
    };

    /// [`PRODUCTION_MIRROR`] for a dial through Tor, which differs in the connect bound alone.
    const PRODUCTION_MIRROR_TOR: Budgets =
        Budgets { connect: TEST_TOR_CONNECT_TIMEOUT, ..PRODUCTION_MIRROR };

    /// The production figures, pinned without waiting any of them out: every fixture that watches
    /// a budget fire does so against [`FAST`], so this is what holds the shipped values.
    #[test]
    fn production_budgets_are_the_documented_figures() {
        assert_eq!(Budgets::production(false), PRODUCTION_MIRROR);
        assert_eq!(
            Budgets::production(true),
            PRODUCTION_MIRROR_TOR,
            "a Tor dial differs from a direct one in the connect bound and nothing else"
        );
        let direct = RemoteClient::new_with_tor("http://127.0.0.1:1", None, TorSettings::default())
            .unwrap();
        assert_eq!(direct.budgets, Budgets::production(false), "new_with_tor must carry the production budgets");
    }

    /// The test budgets keep the ordering production's compile-time assert holds, and
    /// [`Budgets::checked`] refuses a pair that breaks it, so no fixture can run against an
    /// ordering production can never have.
    #[test]
    fn fast_budgets_keep_the_production_ordering() {
        assert_eq!(FAST.checked(), FAST);
        assert_eq!(fast_budgets(true).checked().connect, FAST_TOR_CONNECT);
        let inverted = Budgets { batch_head_patience: FAST.object_read, ..FAST };
        let refused = std::panic::catch_unwind(|| inverted.checked());
        assert!(refused.is_err(), "a head patience equal to the object silence budget must be refused");
        let inverted = Budgets { commit_attempt_allowance: FAST.object_read, ..FAST };
        let refused = std::panic::catch_unwind(|| inverted.checked());
        assert!(refused.is_err(), "a commit attempt allowance equal to the object silence budget must be refused");
    }

    /// Each budget accessor sums the connect bound with *its own* allowance. [`FAST`]'s figures
    /// are pairwise distinct, so an accessor that read a sibling field — the read budget in place
    /// of the error-body one, say, which production cannot tell apart because both are 10s —
    /// returns a figure this test does not expect. No waiting: the arithmetic is the property.
    #[test]
    fn each_budget_accessor_reads_its_own_field() {
        let client = fast_client("http://forklift-budget-accessor-test.invalid");
        assert_eq!(client.resolve_budget(), FAST.connect + FAST.read, "resolve_budget");
        assert_eq!(client.error_body_budget(), FAST.connect + FAST.error_body_read, "error_body_budget");
        assert_eq!(
            client.batch_head_budget(), FAST.connect + FAST.batch_head_patience, "batch_head_budget"
        );
        assert_eq!(
            client.retryable_head_budget(), FAST.connect + FAST.retryable_head_patience,
            "retryable_head_budget"
        );
        assert_eq!(
            client.presence_negotiation_budget(0), FAST.connect + FAST.retryable_head_patience,
            "presence_negotiation_budget"
        );
    }

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

    /// A remote that accepts, reads the request and closes without a byte of answer (a restart)
    /// is a dropped connection worth retrying, not an unknown failure.
    #[test]
    fn fetch_info_against_a_remote_that_closes_without_answering_reads_as_a_dropped_connection() {
        let remote = CommitRemote::start(vec![CommitAnswer::Drop]);
        let client = RemoteClient::new_test_with_budgets(&remote.url, FAST);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

        let message = runtime.block_on(client.fetch_info()).err().expect("no answer is not a success");

        assert!(message.contains(CONNECTION_FAILED), "{}", message);
        assert!(is_transient_failure(&message), "{}", message);
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
        assert!(is_transient_failure(&message), "a refused connection is worth retrying as is: {}", message);
    }

    /// A transport failure that is not a dropped connection repeats when retried unchanged, so it
    /// is never worded as one.
    #[test]
    fn only_a_dropped_connection_reads_as_one() {
        let unbuildable = reqwest::Client::new().get("http://[::1").build().unwrap_err();
        assert_eq!(RemoteClient::connection_failed(&unbuildable), "");
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
    /// a timeout — not hang forever, and not fail for some unrelated reason — and must fail on
    /// this client's own read budget. The message names that budget, but the figure comes from
    /// `describe_transport_error`, not from the client that timed out, so it cannot tell a
    /// `Clients::build` that armed the production [`REMOTE_READ_TIMEOUT`] apart from one that
    /// armed this client's. The elapsed-time bound can: it sits at the midpoint between this
    /// client's budget and that rival's.
    #[test]
    fn fetch_info_times_out_against_a_silent_remote() {
        let remote = SilentRemote::start();
        let client = fast_client(&remote.url);
        let effective_budget = FAST.connect + FAST.read;
        let rival_production_read_budget = FAST.connect + TEST_TIGHT_READ_TIMEOUT;
        let upper_bound =
            effective_budget + (rival_production_read_budget - effective_budget) / 2;
        let hard_ceiling = effective_budget + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let started = std::time::Instant::now();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.fetch_info()).await
        });
        let elapsed = started.elapsed();

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
        assert!(
            elapsed < upper_bound,
            "elapsed {:?} is past {:?} (the midpoint between this client's read budget {:?} and \
            the {:?} the production read timeout would arm) — the tight client is not armed with \
            this client's budgets",
            elapsed, upper_bound, effective_budget, rival_production_read_budget
        );
    }

    /// `fetch_signature` must be bounded exactly like `fetch_info` — pins that it stays wired to
    /// `bounded_reads` and never silently drifts back to the unbounded client (which would
    /// restore the hang).
    #[test]
    fn fetch_signature_times_out_against_a_silent_remote() {
        let remote = SilentRemote::start();
        let client = fast_client(&remote.url);
        let hard_ceiling = FAST.connect + FAST.read
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

    /// `download_bundle` must be bounded exactly like `fetch_info` — same pin as
    /// `fetch_signature`'s, for the same reason.
    #[test]
    fn download_bundle_times_out_against_a_silent_remote() {
        let remote = SilentRemote::start();
        let client = fast_client(&remote.url);
        let dest = std::env::temp_dir().join(format!(
            "forklift-fetch-bundle-to-silent-{}-{}", std::process::id(), line!()
        ));
        let hard_ceiling = FAST.connect + FAST.read
            + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, download_whole_bundle(&client, &dest)).await
        });

        let message = outcome
            .unwrap_or_else(|_| panic!(
                "download_bundle hung past the test's own {:?} ceiling — no timeout fired at all",
                hard_ceiling
            ))
            .expect_err("a silent remote must not appear to succeed");

        assert!(
            message.to_lowercase().contains("timed out"),
            "must fail specifically with a timeout, not some other transport error: {}", message
        );
        // A silent remote never gets past headers, so no file is ever created here — the
        // dedicated test below is what actually exercises the mid-download cleanup.
        assert!(!dest.exists(), "no file should appear at the destination at all in this scenario");

        let _ = std::fs::remove_file(&dest);
    }

    /// `resolve` must **return** — its designed fallback, an empty map — against a remote that
    /// connects and then never writes anything, not hang forever. `resolve` has no `Result` to
    /// inspect the way its siblings above do (it is best-effort by contract: see its own doc), so
    /// this pins the one property their message-content assertions can't stand in for here: that
    /// the call actually completes within its own effective budget at all. Sibling coverage to
    /// `fetch_info`/`fetch_signature`/`download_bundle`'s own `*_times_out_against_a_silent_remote`
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
    /// genuine timeout and the effective connect + read budget that produced it. So this test would
    /// stay green even if `resolve`'s call site rode `Posture::BoundedReads` with no total bound at
    /// all — it is real coverage (a fully-unbounded posture, or a dropped bound entirely, still
    /// fails it), just not the falsifying test for that specific call-site choice.
    /// `resolve_gives_up_on_a_remote_that_never_stops_trickling` below is: it calls `resolve`
    /// itself against a remote that never goes silent at all, the one shape a silence budget
    /// structurally cannot catch and a total deadline must.
    /// (`send_on_applies_a_total_deadline_that_ignores_progress`, also below, uses that same shape
    /// of remote but drives the seam directly rather than calling `resolve` — real coverage of the
    /// seam's own wiring, not of which posture `resolve`'s call site actually names.)
    #[test]
    fn resolve_times_out_against_a_silent_remote() {
        let remote = SilentRemote::start();
        let client = fast_client(&remote.url);
        let effective_budget = FAST.connect + FAST.read;
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
        start_trickling_remote_after(std::time::Duration::ZERO, total_bytes, gap)
    }

    /// [`start_continuously_trickling_remote`] with its headers held back for `head_delay` after
    /// the request is read.
    fn start_trickling_remote_after(head_delay: std::time::Duration,
                                    total_bytes: usize,
                                    gap: std::time::Duration) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());

        std::thread::spawn(move || {
            use std::io::Write;

            if let Ok((mut stream, _)) = listener.accept() {
                let _ = read_test_request(&mut stream);
                std::thread::sleep(head_delay);
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
    /// A short [`Posture::TotalDeadline`] (`FAST.connect + 1s` — deliberately above the client's
    /// own connect bound, the invariant [`clients::check_total_deadline_payload`] enforces on every
    /// `TotalDeadline` payload) against a remote writing one byte every 100ms — never silent for
    /// longer than 100ms, well under every silence budget [`FAST`] carries — must still be cut off
    /// at that deadline, because a `RequestBuilder::timeout` does not reset on anything.
    /// `total_bytes` (100) at that gap makes the full trickle 10s: longer than `hard_ceiling`, so
    /// the fixture is still writing when the ceiling would fire — not literally unbounded, just
    /// sized past this test's own horizon. If the deadline were silently dropped, the outer
    /// `tokio::time::timeout` — not the assertion below it — is what would catch it, loudly, rather
    /// than the test quietly passing on a technicality.
    #[test]
    fn send_on_applies_a_total_deadline_that_ignores_progress() {
        let url = start_continuously_trickling_remote(100, std::time::Duration::from_millis(100));
        let client = fast_client(&url);
        let deadline = FAST.connect + std::time::Duration::from_secs(1);
        let hard_ceiling = deadline + std::time::Duration::from_secs(5);

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

                #[allow(clippy::disallowed_methods)] // probes the transport bound itself, not a production read
                response.bytes().await
            }).await
        });

        let inner = outcome.unwrap_or_else(|_| panic!(
            "send_on(TotalDeadline) hung past the test's own {:?} outer ceiling — the total \
            deadline never fired at all, even though the remote never stopped writing bytes",
            hard_ceiling
        ));

        let error = inner.expect_err(&format!(
            "a remote making continuous progress every 100ms must still be cut off by this \
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
    /// **Drives the seam, not `fetch_batch`.** That is what makes it fast — the connect bound is
    /// injected at 300ms and the head budget is 600ms, so the fixture's ~2s body genuinely
    /// outlasts the bound without paying the production 50s — and it is also its limit: it says
    /// nothing about which posture `fetch_batch`'s own call site names.
    /// `fetch_batch_degrades_against_a_silent_remote` is what pins that.
    ///
    /// The head budget deliberately exceeds `connect_timeout`, the invariant
    /// [`clients::clamp_head_deadline_payload`] enforces on every payload — so the clamp is a
    /// no-op here and the figure under test is the one written below. It also sits far under this
    /// posture's own client-level `read_timeout` (300ms connect + the production 60s), the other fence
    /// [`clients::check_head_deadline_payload`] holds, so neither guard interferes with what this
    /// test measures.
    #[test]
    fn send_on_head_deadline_does_not_bound_a_body_that_arrives_after_it() {
        let body_bytes = 20usize;
        let gap = std::time::Duration::from_millis(100);
        let connect_timeout = std::time::Duration::from_millis(300);
        let head = connect_timeout + std::time::Duration::from_millis(300);
        let body_duration = gap * (body_bytes as u32);
        assert!(
            body_duration > head * 2,
            "the fixture's own body ({:?}) must genuinely outlast the head budget ({:?}), or this \
            test cannot tell a head deadline from a total one at all", body_duration, head
        );

        let url = start_continuously_trickling_remote(body_bytes, gap);
        let client = RemoteClient::new_test_with_budgets(
            &url, Budgets { connect: connect_timeout, ..Budgets::production(false) },
        );
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

                #[allow(clippy::disallowed_methods)] // probes the transport bound itself, not a production read
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
    /// the one producer of a `TotalDeadline` payload (`RemoteClient::resolve_budget`) builds
    /// `self.budgets.connect + <positive constant>`, so it can never construct a violating
    /// payload — the assert holds by construction, and nothing in the suite exercises the branch
    /// where it actually fires. This test bypasses the producer and hand-constructs the violating
    /// payload directly (the same way `send_on_applies_a_total_deadline_that_ignores_progress`
    /// above hand-constructs a valid one).
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
            TEST_LOOSE_READ_TIMEOUT,
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
    /// above the lower fence and it is unambiguously *this* assert that fires. The fence's
    /// silence budget is a 7s sentinel, not the production figure, so a check that read the
    /// production constant in place of the instance's budget would put the fence at 70s, not
    /// fire, and fail this test with "did not panic".
    ///
    /// Ignored outside a debug profile, same as its sibling. The release-side guarantee for the
    /// one producer that exists is not this assert but the const assertion beside
    /// `BATCH_HEAD_PATIENCE`, which fails the *build* rather than a test run.
    #[test]
    #[cfg_attr(not(debug_assertions), ignore)]
    #[should_panic(expected = "must stay strictly under")]
    fn the_head_deadline_payload_check_catches_a_payload_past_the_silence_budget() {
        let connect_timeout = std::time::Duration::from_secs(10);
        let object_read = std::time::Duration::from_secs(7);
        clients::check_head_deadline_payload(
            connect_timeout + object_read,
            connect_timeout,
            object_read,
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
    /// every 200ms — never silent for longer than 200ms, well under every silence budget
    /// [`FAST`] carries), `resolve`'s own `Posture::TotalDeadline` must still cut the
    /// call off at `resolve_budget()` and degrade to the designed fallback (an empty map, see
    /// `resolve`'s own doc). A *silence* budget instead ([`Posture::BoundedReads`]) would reset
    /// its clock on every byte; within this test's observation window such a call never returns
    /// at all, which is why the outer `tokio::time::timeout` below exists as a backstop.
    ///
    /// **A bare "returned before the outer ceiling" check does not pin *when*.** Three call-site
    /// defects ignore this instance's connect bound or its read budget: arming the read budget
    /// alone (connect-blind), arming the production [`REMOTE_CONNECT_TIMEOUT`] in place of
    /// `self.budgets.connect`, and arming the production [`REMOTE_READ_TIMEOUT`] in place of
    /// `self.budgets.read`. The client carries a connect bound neither [`FAST`] nor production
    /// does, chosen so the three rivals and the true budget are strictly ordered with the true
    /// budget second; `lower_bound` and `upper_bound` are the midpoints on either side of it, so
    /// each rival lands outside the window. The test asserts that ordering before it relies on
    /// it, so a change to [`FAST`] that collapses it fails loudly instead of leaving a window that
    /// no longer separates anything.
    ///
    /// This test excludes the rivals named above; it is not a completeness argument over any set
    /// of budgets, and other connect-blind values exist that it does not detect.
    ///
    /// `total_bytes` (100) at the 200ms gap makes the full trickle 20s, longer than
    /// `hard_ceiling` below — the fixture is still writing when the ceiling fires. If the deadline
    /// is silently dropped, or the call site rewired onto `Posture::BoundedReads`, the outer
    /// `tokio::time::timeout` — not the assertion below it — is what catches it, loudly, rather
    /// than the test quietly hanging the suite.
    #[test]
    fn resolve_gives_up_on_a_remote_that_never_stops_trickling() {
        let url = start_continuously_trickling_remote(100, std::time::Duration::from_millis(200));
        // A connect bound neither FAST nor production carries, so each rival a wrong call site
        // could arm lands on its own side of the true budget: dropping the connect term
        // (`rival_flat_budget`) falls below it, and hardcoding the production connect bound or
        // the production read budget in place of this instance's lands above it.
        let connect_timeout = std::time::Duration::from_millis(2_450);
        let client = RemoteClient::new_test_with_budgets(
            &url, Budgets { connect: connect_timeout, ..FAST },
        );
        let true_budget = connect_timeout + FAST.read;
        let rival_flat_budget = FAST.read;
        let rival_production_connect_budget = TEST_DIRECT_CONNECT_TIMEOUT + FAST.read;
        let rival_production_read_budget = connect_timeout + TEST_TIGHT_READ_TIMEOUT;
        assert!(rival_flat_budget < true_budget && true_budget < rival_production_connect_budget
            && rival_production_connect_budget < rival_production_read_budget);
        let lower_bound = rival_flat_budget + (true_budget - rival_flat_budget) / 2;
        let upper_bound = true_budget + (rival_production_connect_budget - true_budget) / 2;
        let hard_ceiling = rival_production_read_budget + std::time::Duration::from_secs(3);

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
            "elapsed {:?} is under {:?} (the midpoint between the flat rival budget {:?} and the \
            true budget {:?}) — resolve returned close to a call site that drops \
            self.budgets.connect rather than folding it in via resolve_budget()",
            elapsed, lower_bound, rival_flat_budget, true_budget
        );
        assert!(
            elapsed < upper_bound,
            "elapsed {:?} is past {:?} (the midpoint between the true budget {:?} and the {:?} \
            a hardcoded REMOTE_CONNECT_TIMEOUT would arm) — resolve_budget() is reading a \
            production constant rather than this instance's budgets",
            elapsed, upper_bound, true_budget, rival_production_connect_budget
        );
    }

    /// A mid-download failure of a download nothing could resume — [`LyingContentLengthRemote`]
    /// sends no tag — must never leave a truncated file at the destination. Unlike the
    /// silent-remote scenario above, it gets past headers and writes a few real bytes before going
    /// quiet, so the file this exercises genuinely exists (and has content) at the moment the
    /// timeout fires and the cleanup runs.
    ///
    /// Revert: drop `RemoveFileOnDrop` from `receive_bundle` → the four bytes stay → red.
    #[test]
    fn download_bundle_leaves_no_truncated_file_on_a_mid_download_timeout() {
        let remote = LyingContentLengthRemote::start();
        let client = fast_client(&remote.url);
        let dest = std::env::temp_dir().join(format!(
            "forklift-fetch-bundle-to-truncated-{}-{}", std::process::id(), line!()
        ));
        let hard_ceiling = FAST.connect + FAST.read
            + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, download_whole_bundle(&client, &dest)).await
        });

        let message = outcome
            .unwrap_or_else(|_| panic!(
                "download_bundle hung past the test's own {:?} ceiling — no timeout fired at all",
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

        let _ = std::fs::remove_file(&dest);
    }

    /// A completed download lands at the destination through the fsync-on path: the bundle is
    /// synced through the handle that wrote it. The integration suites launch their binaries with
    /// durability off, so this in-process test is what still executes `receive_bundle`'s
    /// `sync_all` — the call that fails on Windows if the file is synced through a handle without
    /// write access. It catches a sync that errors, not one removed.
    #[test]
    fn download_bundle_syncs_a_complete_bundle() {
        assert!(
            file_utils::fsync_enabled(),
            "this test exists to run the fsync-on path; unset FORKLIFT_FSYNC to run it"
        );
        let remote = CannedBodyRemote::start(b"a complete bundle");
        let client = fast_client(&remote.url);
        let dest = std::env::temp_dir().join(format!(
            "forklift-fetch-bundle-to-complete-{}-{}", std::process::id(), line!()
        ));

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let fetched = runtime.block_on(download_whole_bundle(&client, &dest));

        assert_eq!(fetched, Ok(BundleDownload::Complete { resumed: false }));
        assert_eq!(std::fs::read(&dest).unwrap(), b"a complete bundle");
        let _ = std::fs::remove_file(&dest);
    }

    /// Download the bundle with nothing kept and no record to checkpoint into.
    async fn download_whole_bundle(client: &RemoteClient, path: &std::path::Path) -> Result<BundleDownload, String> {
        client.download_bundle(path, None, BUNDLE_CHECKPOINT_BYTES, &mut |_| Ok(())).await
    }

    /// A remote that answers its `n`th request with `answers[n]`, a whole raw response after which
    /// it closes the connection, and records each request's head. A request past the script is
    /// answered `500`.
    struct ScriptedBundleRemote {
        url: String,
        heads: Arc<Mutex<Vec<String>>>,
    }

    impl ScriptedBundleRemote {
        fn start(answers: Vec<Vec<u8>>) -> ScriptedBundleRemote {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let heads = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&heads);

            std::thread::spawn(move || {
                use std::io::{Read, Write};

                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];

                    while !head.ends_with(b"\r\n\r\n") {
                        match stream.read(&mut byte) {
                            Ok(1) => head.push(byte[0]),
                            _ => break,
                        }
                    }

                    let index = {
                        let mut heads = recorded.lock().unwrap();
                        heads.push(String::from_utf8_lossy(&head).to_ascii_lowercase());
                        heads.len() - 1
                    };
                    let answer = answers.get(index).cloned().unwrap_or_else(|| {
                        b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                    });
                    let _ = stream.write_all(&answer);
                    let _ = stream.flush();
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            });

            ScriptedBundleRemote { url, heads }
        }

        fn heads(&self) -> Vec<String> {
            self.heads.lock().unwrap().clone()
        }
    }

    /// A `200` for `body` under `tag` (none when `None`), declaring its whole length but sending
    /// only its first `sent` bytes.
    fn whole_bundle_answer(tag: Option<&str>, body: &[u8], sent: usize) -> Vec<u8> {
        let tag = tag.map(|tag| format!("ETag: {}\r\n", tag)).unwrap_or_default();
        let mut answer = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n", body.len(), tag
        ).into_bytes();
        answer.extend_from_slice(&body[..sent]);
        answer
    }

    /// A `206` of `body` from `start` under `tag`, with `content_range` as given.
    fn rest_of_bundle_answer(tag: &str, body: &[u8], start: usize, content_range: &str) -> Vec<u8> {
        let mut answer = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: {}\r\nETag: {}\r\n\
            Connection: close\r\n\r\n", body.len() - start, content_range, tag
        ).into_bytes();
        answer.extend_from_slice(&body[start..]);
        answer
    }

    /// The value of `name` in a recorded (lowercased) request head.
    fn head_value(head: &str, name: &str) -> Option<String> {
        head.lines().find_map(|line| line.strip_prefix(&format!("{}:", name)).map(|value| value.trim().to_string()))
    }

    const BUNDLE_BODY: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCD";

    /// Run one download of `path` against `remote`, collecting every checkpoint.
    fn download_with(remote: &ScriptedBundleRemote,
                     path: &std::path::Path,
                     kept: Option<&PartialBundle>,
                     checkpoint_bytes: u64) -> (Result<BundleDownload, String>, Vec<Option<PartialBundle>>) {
        let client = fast_client(&remote.url);
        let mut checkpoints = Vec::new();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(client.download_bundle(path, kept, checkpoint_bytes, &mut |partial| {
            checkpoints.push(partial.cloned());
            Ok(())
        }));

        (outcome, checkpoints)
    }

    fn kept_bundle(etag: &str, length: u64) -> PartialBundle {
        PartialBundle { etag: etag.to_string(), total: BUNDLE_BODY.len() as u64, length }
    }

    /// A download cut off under a tag keeps what landed and records it, and the next one asks for
    /// only the rest under the same tag and appends it.
    ///
    /// Reverts: no best-effort checkpoint on a failure → the record stops at 0 → red; a plain
    /// request on resume → no `Range` → red; truncate the kept file on a `206` → red.
    #[test]
    fn a_cut_off_download_is_kept_and_resumed_for_the_rest_only() {
        let scratch = Scratch::new("bundle-resume");
        let path = scratch.root.join("partial");
        let tag = "\"v1\"";

        // The interval never comes round: only the checkpoint after the failure records the 21.
        let remote = ScriptedBundleRemote::start(vec![whole_bundle_answer(Some(tag), BUNDLE_BODY, 21)]);
        let (outcome, checkpoints) = download_with(&remote, &path, None, u64::MAX);
        assert!(outcome.is_err(), "{:?}", outcome);
        assert_eq!(checkpoints, vec![Some(kept_bundle(tag, 0)), Some(kept_bundle(tag, 21))]);
        assert_eq!(std::fs::read(&path).unwrap(), &BUNDLE_BODY[..21]);

        // Bytes past what was recorded are not trusted: they are cut before the rest is appended.
        std::io::Write::write_all(&mut std::fs::OpenOptions::new().append(true).open(&path).unwrap(), b"junk").unwrap();
        let rest = rest_of_bundle_answer(tag, BUNDLE_BODY, 21, "bytes 21-39/40");
        let remote = ScriptedBundleRemote::start(vec![rest]);
        let (outcome, checkpoints) = download_with(&remote, &path, Some(&kept_bundle(tag, 21)), 8);
        assert_eq!(outcome, Ok(BundleDownload::Complete { resumed: true }));
        assert_eq!(std::fs::read(&path).unwrap(), BUNDLE_BODY);
        assert_eq!(checkpoints.last(), Some(&Some(kept_bundle(tag, 40))));
        assert!(!checkpoints.contains(&None), "{:?}", checkpoints);

        let heads = remote.heads();
        assert_eq!(heads.len(), 1);
        assert_eq!(head_value(&heads[0], "range").as_deref(), Some("bytes=21-"));
        assert_eq!(head_value(&heads[0], "if-range").as_deref(), Some(tag));
        assert_eq!(head_value(&heads[0], "accept-encoding").as_deref(), Some("identity"));
    }

    /// A kept download is resumed only by the rest of itself: a `206` that starts elsewhere, names
    /// another tag or another total, and a `416`, drop it for one plain request, and a `200`
    /// restarts it under its own tag.
    ///
    /// Revert: accept any `206` → the file is spliced → red.
    #[test]
    fn a_kept_download_restarts_on_any_answer_but_the_rest_it_asked_for() {
        let scratch = Scratch::new("bundle-restart");
        let path = scratch.root.join("partial");
        let tag = "\"v1\"";
        let fresh = whole_bundle_answer(Some("\"v2\""), BUNDLE_BODY, BUNDLE_BODY.len());
        let refused = b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */40\r\nContent-Length: 0\r\n\
            Connection: close\r\n\r\n".to_vec();

        for (label, answer) in [
            ("another start", rest_of_bundle_answer(tag, BUNDLE_BODY, 20, "bytes 20-39/40")),
            ("another tag", rest_of_bundle_answer("\"v0\"", BUNDLE_BODY, 21, "bytes 21-39/40")),
            ("another total", rest_of_bundle_answer(tag, BUNDLE_BODY, 21, "bytes 21-39/41")),
            ("refused", refused),
        ] {
            std::fs::write(&path, &BUNDLE_BODY[..21]).unwrap();
            let remote = ScriptedBundleRemote::start(vec![answer, fresh.clone()]);
            let (outcome, checkpoints) = download_with(&remote, &path, Some(&kept_bundle(tag, 21)), 8);

            assert_eq!(outcome, Ok(BundleDownload::Complete { resumed: false }), "{}", label);
            assert_eq!(std::fs::read(&path).unwrap(), BUNDLE_BODY, "{}", label);
            assert_eq!(checkpoints.first(), Some(&None), "{}: the kept download is dropped first", label);
            assert_eq!(checkpoints.last(), Some(&Some(kept_bundle("\"v2\"", 40))), "{}", label);

            let heads = remote.heads();
            assert_eq!(heads.len(), 2, "{}", label);
            assert_eq!(head_value(&heads[1], "range"), None, "{}: the fallback asks for the whole bundle", label);
        }

        // A rebuilt bundle comes back whole under its new tag, and replaces the kept bytes.
        std::fs::write(&path, b"ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ").unwrap();
        let remote = ScriptedBundleRemote::start(vec![fresh]);
        let (outcome, checkpoints) = download_with(&remote, &path, Some(&kept_bundle(tag, 21)), 8);
        assert_eq!(outcome, Ok(BundleDownload::Complete { resumed: false }));
        assert_eq!(std::fs::read(&path).unwrap(), BUNDLE_BODY);
        assert_eq!(checkpoints.first(), Some(&Some(kept_bundle("\"v2\"", 0))));
        assert_eq!(remote.heads().len(), 1);
    }

    /// A download kept whole goes straight to import: nothing is asked of the remote. One kept at no
    /// bytes is asked for whole.
    ///
    /// Reverts: always request → the remote sees one → red; resume from byte 0 → a `Range` → red.
    #[test]
    fn a_complete_kept_download_needs_no_request() {
        let scratch = Scratch::new("bundle-complete");
        let path = scratch.root.join("partial");
        std::fs::write(&path, BUNDLE_BODY).unwrap();

        let remote = ScriptedBundleRemote::start(Vec::new());
        let (outcome, checkpoints) = download_with(&remote, &path, Some(&kept_bundle("\"v1\"", 40)), 8);
        assert_eq!(outcome, Ok(BundleDownload::Complete { resumed: true }));
        assert!(checkpoints.is_empty());
        assert!(remote.heads().is_empty());

        // One shorter than recorded is not kept: it is dropped and downloaded afresh.
        std::fs::write(&path, &BUNDLE_BODY[..39]).unwrap();
        let remote = ScriptedBundleRemote::start(vec![whole_bundle_answer(Some("\"v1\""), BUNDLE_BODY, 40)]);
        let (outcome, checkpoints) = download_with(&remote, &path, Some(&kept_bundle("\"v1\"", 40)), 8);
        assert_eq!(outcome, Ok(BundleDownload::Complete { resumed: false }));
        assert_eq!(checkpoints.first(), Some(&None));
        assert_eq!(head_value(&remote.heads()[0], "range"), None);

        // Nothing kept yet is no resume: the bundle is asked for whole, and comes in one piece.
        std::fs::write(&path, b"").unwrap();
        let remote = ScriptedBundleRemote::start(vec![whole_bundle_answer(Some("\"v1\""), BUNDLE_BODY, 40)]);
        let (outcome, _) = download_with(&remote, &path, Some(&kept_bundle("\"v1\"", 0)), 8);
        assert_eq!(outcome, Ok(BundleDownload::Complete { resumed: false }));
        assert_eq!(head_value(&remote.heads()[0], "range"), None);
        assert_eq!(std::fs::read(&path).unwrap(), BUNDLE_BODY);
    }

    /// Without a resumable tag and a declared length nothing is kept: a cut-off download leaves no
    /// file and records none, also when an earlier attempt had kept one.
    ///
    /// Reverts: checkpoint without a validator → a `Some` is recorded → red; keep the earlier
    /// download → the file survives → red.
    #[test]
    fn a_download_without_a_validator_is_never_kept() {
        let scratch = Scratch::new("bundle-no-validator");
        let path = scratch.root.join("partial");

        for (label, tag) in [("no tag", None), ("a weak tag", Some("W/\"v1\"")), ("an unquoted tag", Some("v1"))] {
            for kept in [None, Some(kept_bundle("\"v1\"", 21))] {
                if kept.is_some() {
                    std::fs::write(&path, &BUNDLE_BODY[..21]).unwrap();
                }

                let remote = ScriptedBundleRemote::start(vec![whole_bundle_answer(tag, BUNDLE_BODY, 21)]);
                let (outcome, checkpoints) = download_with(&remote, &path, kept.as_ref(), 8);
                assert!(outcome.is_err(), "{}: {:?}", label, outcome);
                assert!(!path.exists(), "{}: a download nothing can resume is not left behind", label);
                assert!(!checkpoints.is_empty() && checkpoints.iter().all(Option::is_none), "{}: {:?}", label, checkpoints);
            }
        }

        // No declared length: the bytes are read to the end of the connection, and not kept.
        let mut undeclared = b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n".to_vec();
        undeclared.extend_from_slice(BUNDLE_BODY);
        let remote = ScriptedBundleRemote::start(vec![undeclared]);
        let (outcome, checkpoints) = download_with(&remote, &path, None, 8);
        assert_eq!(outcome, Ok(BundleDownload::Complete { resumed: false }));
        assert_eq!(std::fs::read(&path).unwrap(), BUNDLE_BODY);
        assert_eq!(checkpoints, vec![None]);
    }

    /// A remote that no longer has a bundle drops the kept download and records none.
    #[test]
    fn a_bundle_gone_from_the_remote_drops_the_kept_download() {
        let scratch = Scratch::new("bundle-gone");
        let path = scratch.root.join("partial");
        std::fs::write(&path, &BUNDLE_BODY[..21]).unwrap();

        let gone = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let remote = ScriptedBundleRemote::start(vec![gone]);
        let (outcome, checkpoints) = download_with(&remote, &path, Some(&kept_bundle("\"v1\"", 21)), 8);
        assert_eq!(outcome, Ok(BundleDownload::Absent));
        assert!(!path.exists());
        assert_eq!(checkpoints, vec![None]);
    }

    /// A download is kept only under a strong, short tag of visible ASCII.
    #[test]
    fn only_strong_short_visible_tags_are_resumable() {
        for tag in ["\"\"", "\"v1\"", "\"0123456789abcdef0123456789abcdef\"", &format!("\"{}\"", "a".repeat(254))] {
            assert!(is_resumable_bundle_tag(tag), "{}", tag);
        }

        for tag in ["", "\"", "v1", "W/\"v1\"", "\"v 1\"", "\"v\"1\"", "\"v\u{e9}\"", "\"v\t\"",
                    &format!("\"{}\"", "a".repeat(255))] {
            assert!(!is_resumable_bundle_tag(tag), "{}", tag);
        }
    }

    /// A remote that answers one request with `200` and the given body, then closes.
    struct CannedBodyRemote {
        url: String,
    }

    impl CannedBodyRemote {
        fn start(body: &'static [u8]) -> CannedBodyRemote {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());

            std::thread::spawn(move || {
                use std::io::Write;

                if let Ok((mut stream, _)) = listener.accept() {
                    let _ = read_test_request(&mut stream);
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(body);
                    let _ = stream.flush();
                }
            });

            CannedBodyRemote { url }
        }
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
    // it, their head-wait sized by `RemoteClient::retryable_head_budget` — the client that never
    // auto-follows a redirect, so the FORK-89 guard neither call ever loses stays in force, and a
    // head-wait that bounds the status line alone, so a late refusal keeps its typed body.
    // -----------------------------------------------------------------------------------

    /// `upload_signature` against a remote that connects and then never writes anything must
    /// fail with a timeout close to its own `retryable_head_budget`, not hang forever (the rival
    /// this excludes: staying on the old unbounded posture, which this outer ceiling would catch
    /// as a hang), not close to a connect-blind flat budget that drops `self.budgets.connect`
    /// (the rival the lower bound excludes), and not close to a budget built from the production
    /// constants in place of this instance's (the rival the upper bound excludes). The production
    /// allowance itself is pinned by `production_budgets_are_the_documented_figures`, without
    /// waiting it out.
    #[test]
    fn upload_signature_times_out_against_a_silent_remote() {
        let remote = SilentRemote::start();
        let client = fast_client(&remote.url);
        // The flat rival drops the connect bound entirely; the production rival reads
        // REMOTE_CONNECT_TIMEOUT in place of this instance's connect bound.
        let effective_budget = FAST.connect + FAST.retryable_head_patience;
        let rival_flat_budget = FAST.retryable_head_patience;
        let rival_production_budget = TEST_DIRECT_CONNECT_TIMEOUT + FAST.retryable_head_patience;
        let lower_bound = rival_flat_budget + (effective_budget - rival_flat_budget) / 2;
        let upper_bound = effective_budget + (rival_production_budget - effective_budget) / 2;
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
                "upload_signature hung past the test's own {:?} ceiling — no head-wait bound \
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
            ignores self.budgets.connect, dropping it entirely, rather than folding it in via \
            retryable_head_budget()",
            elapsed, lower_bound, rival_flat_budget, effective_budget
        );
        assert!(
            elapsed < upper_bound,
            "elapsed {:?} is past {:?} (the midpoint between the true budget {:?} and the {:?} a \
            hardcoded REMOTE_CONNECT_TIMEOUT would arm) — retryable_head_budget() is reading a \
            production constant rather than this instance's budgets",
            elapsed, upper_bound, effective_budget, rival_production_budget
        );
    }

    /// `put_trust` against a remote that connects and then never writes anything — same pin as
    /// `upload_signature_times_out_against_a_silent_remote`, its own call site.
    #[test]
    fn put_trust_times_out_against_a_silent_remote() {
        let remote = SilentRemote::start();
        let client = fast_client(&remote.url);
        let effective_budget = FAST.connect + FAST.retryable_head_patience;
        let rival_flat_budget = FAST.retryable_head_patience;
        let rival_production_budget = TEST_DIRECT_CONNECT_TIMEOUT + FAST.retryable_head_patience;
        let lower_bound = rival_flat_budget + (effective_budget - rival_flat_budget) / 2;
        let upper_bound = effective_budget + (rival_production_budget - effective_budget) / 2;
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
                "put_trust hung past the test's own {:?} ceiling — no head-wait bound fired at \
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
            ignores self.budgets.connect, dropping it entirely, rather than folding it in via \
            retryable_head_budget()",
            elapsed, lower_bound, rival_flat_budget, effective_budget
        );
        assert!(
            elapsed < upper_bound,
            "elapsed {:?} is past {:?} (the midpoint between the true budget {:?} and the {:?} a \
            hardcoded REMOTE_CONNECT_TIMEOUT would arm) — retryable_head_budget() is reading a \
            production constant rather than this instance's budgets",
            elapsed, upper_bound, effective_budget, rival_production_budget
        );
    }

    /// A head that never sends its status line is abandoned, not failed: `fetch_batch`'s `POST`
    /// rides [`Posture::HeadDeadlineNoRedirect`], and an expiry of that head wait is the one
    /// transport failure it turns into [`BatchOutcome::Degraded`], closing the lane so the caller
    /// goes loose. It used to be an error, which failed the whole command against a head that was
    /// merely slow to build its bundle.
    ///
    /// The elapsed time is bracketed so the degrade is the head wait's doing and not some other
    /// budget's: at least [`FAST`]'s `connect + batch_head_patience`, and under the
    /// `connect + object_read` the same client's silence budget would have fired at.
    ///
    /// The seam-level companion is
    /// `send_on_head_deadline_does_not_bound_a_body_that_arrives_after_it` — this test would stay
    /// green under an implementation that armed a *total* deadline of the same size, and that one
    /// is what rules it out.
    #[test]
    fn fetch_batch_degrades_against_a_silent_remote() {
        let remote = SilentRemote::start();
        let client = fast_client(&remote.url);
        let clone = client.clone();
        let effective_budget = FAST.connect + FAST.batch_head_patience;
        let silence_budget = FAST.connect + FAST.object_read;
        let hard_ceiling = effective_budget + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let started = std::time::Instant::now();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.fetch_batch(&["a".repeat(64)])).await
        });
        let elapsed = started.elapsed();

        let outcome = outcome
            .unwrap_or_else(|_| panic!(
                "fetch_batch hung past the test's own {:?} ceiling — the head-wait bound never \
                fired at all", hard_ceiling
            ))
            .unwrap_or_else(|e| panic!("a head that is only slow must degrade, not fail: {}", e));

        assert!(matches!(outcome, BatchOutcome::Degraded), "{:?}", outcome);
        assert!(!client.batch_lane_open(), "a degraded batch closes the lane");
        assert!(!clone.batch_lane_open(), "the lane is shared with every clone of the client");
        assert_eq!(
            clone.batch_lane_closure(), Some(BatchLaneClosure::Degraded),
            "the reason is shared with the lane, and a slow head is not an absent endpoint"
        );
        assert!(
            elapsed >= effective_budget,
            "elapsed {:?} is under the head budget {:?}", elapsed, effective_budget
        );
        assert!(
            elapsed < silence_budget,
            "elapsed {:?} reached the silence budget {:?}: the batch waited on the wrong bound",
            elapsed, silence_budget
        );
    }

    /// `fetch_subtree` rides the same head wait as `fetch_batch`, for the same reason — the server
    /// builds the whole closure before its first byte — but has no lane to degrade to inside it:
    /// its own fallback is for a remote that refuses, not one that never answers. So a silent
    /// remote is a timeout here.
    ///
    /// The message must name **exactly** [`FAST`]'s `connect + batch_head_patience`. [`FAST`]'s
    /// budgets sum to a distinct figure per posture, so a call site rewired onto any other posture
    /// fails on the figure it prints, not merely on how long it took. It must also *not* say "at
    /// least": a non-resetting external timer is a true upper bound, unlike the silence budgets
    /// whose arms in `describe_transport_error` deliberately hedge.
    #[test]
    fn fetch_subtree_times_out_against_a_silent_remote() {
        let remote = SilentRemote::start();
        let client = fast_client(&remote.url);
        let effective_budget = FAST.connect + FAST.batch_head_patience;
        let hard_ceiling = effective_budget + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(
                hard_ceiling, client.fetch_subtree(&"p".repeat(64), "src", &"t".repeat(64))
            ).await
        });

        let message = outcome
            .unwrap_or_else(|_| panic!(
                "fetch_subtree hung past the test's own {:?} ceiling — the head-wait bound never \
                fired at all", hard_ceiling
            ))
            .expect_err("a silent remote must not appear to succeed or to lack the endpoint");

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
        assert!(client.batch_lane_open(), "a subtree timeout says nothing about the batch lane");
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
    /// The budget is [`FAST`]'s `connect + object_read`, and the assertions do the discriminating
    /// rather than the clock. The message must name **exactly** that figure, which separates this
    /// from a mutant wiring the posture onto the tight [`Posture::BoundedReads`] budget
    /// (`connect + read`) or onto no budget at all (hangs past the ceiling).
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
        let client = fast_client(&remote.url);
        let effective_budget = FAST.connect + FAST.object_read;
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
    /// `connect + object_read` budget, so racing them costs one budget rather than two.
    ///
    /// **The mutant this test alone catches, established by running it rather than by reasoning
    /// about it:** give the *`BoundedObjectReads`* arm this arm's wording. Measured — this test
    /// fails on identical `left`/`right`, and
    /// [`fetch_batch_times_out_on_a_stalled_direct_body`] **passes**, because that test asserts on
    /// the direct arm, which the mutant does not touch. Nothing else in the file compares the two
    /// arms against each other, so without this test that mutant ships.
    ///
    /// The mirror mutant — giving *this* posture's arm the `BoundedObjectReads` wording — is
    /// **not** the one that justifies this test's wait, and an earlier version of this doc said it
    /// was. It fails two tests, not one: [`fetch_batch_times_out_on_a_stalled_direct_body`] trips
    /// first, on its `contains("headers")` assertion. Recorded because a coverage claim is exactly
    /// what a later reader consults when deciding a slow test is redundant, and this one was wrong
    /// in the direction that would have got the test deleted.
    #[test]
    fn the_two_batch_stations_do_not_share_one_stall_message() {
        let direct = StalledDirectBodyRemote::start();
        let redirected = RedirectThenStalledBodyRemote::start();
        let direct_client = fast_client(&direct.url);
        let redirect_client = fast_client(&redirected.url);
        let hard_ceiling = FAST.connect + FAST.object_read
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
    /// widened to cover the body kills it at `connect + batch_head_patience`. The tight
    /// [`Posture::BoundedReads`] silence budget (`connect + read`) kills it at the first gap. Only
    /// a *loose, resetting* silence budget survives, which is precisely the mechanism this station
    /// is supposed to carry.
    ///
    /// Reuses `start_steady_drip_remote` — `fetch_object`'s own drip fixture, on the direct path
    /// with no redirect hop, which is exactly the shape the batch `POST` sees from a non-offloading
    /// head.
    #[test]
    fn fetch_batch_survives_a_slow_but_steadily_progressing_direct_body() {
        let gap = std::time::Duration::from_millis(1_500);
        let _store = Scratch::new("batch-direct-drip");
        let (chunks, hash) = drip_bundle(4);
        let total_duration = gap * (chunks.len() as u32);
        let effective_silence_budget = FAST.connect + FAST.object_read;
        let effective_head_budget = FAST.connect + FAST.batch_head_patience;

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
        let client = fast_client(&url);
        let outer_ceiling = total_duration + std::time::Duration::from_secs(20);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(outer_ceiling, client.fetch_batch(&[hash.clone()])).await
        });

        let stats = outcome
            .unwrap_or_else(|_| panic!(
                "fetch_batch hung past its own generous outer ceiling {:?}", outer_ceiling
            ))
            .unwrap_or_else(|e| panic!(
                "a body that was never silent for more than {:?} must never be treated as a \
                stall, however long the whole transfer took: {}", gap, e
            ))
            .expect("a served bundle is not the absence signal");

        // A prefix of the bundle — the other shape a wrongly-scoped bound produces — now fails
        // the import outright, so arriving intact is the object landing.
        assert_eq!(stats.stored_objects, 1, "the full body must arrive intact, not a prefix of it");
        assert!(file_utils::does_object_exist(&hash).unwrap(), "the dripped object landed in this store");
    }

    /// A roster past the server's per-request limit resolves in several requests, each within the
    /// limit and asking about each identifier once — not in one request the server refuses, which
    /// would show every operator pseudonymously.
    #[test]
    fn resolve_splits_a_roster_past_the_server_limit_and_asks_each_identifier_once() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let batches = std::sync::Arc::new(std::sync::Mutex::new(Vec::<usize>::new()));
        let seen = std::sync::Arc::clone(&batches);

        std::thread::spawn(move || {
            use std::io::Write;

            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let Some((_, _, _, body)) = read_test_request(&mut stream) else { continue };
                let request: ResolveRequest = serde_json::from_slice(&body).unwrap();
                seen.lock().unwrap().push(request.identifiers.len());

                let names: BTreeMap<String, String> = request.identifiers.iter()
                    .map(|id| (id.clone(), format!("Name of {}", id))).collect();
                let answer = serde_json::json!({ "names": names }).to_string();
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    answer.len(), answer);
            }
        });

        let distinct = crate::model::remote::MAX_RESOLVE_IDENTIFIERS + 1;
        let mut identifiers: Vec<String> = (0..distinct).map(|i| format!("op-{}", i)).collect();
        identifiers.extend((0..5000).map(|_| "op-0".to_string()));

        let client = RemoteClient::new(&url, None).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let names = runtime.block_on(client.resolve(identifiers));

        assert_eq!(names.len(), distinct, "every distinct identifier resolves");
        let batches = batches.lock().unwrap().clone();
        assert_eq!(batches.iter().sum::<usize>(), distinct, "each identifier is asked about once: {:?}", batches);
        assert!(batches.iter().all(|&n| n <= crate::model::remote::MAX_RESOLVE_IDENTIFIERS),
            "no request exceeds the server's limit: {:?}", batches);
    }

    /// Long identifiers split a roster by bytes, not just by count; exactly
    /// `RESOLVE_REQUESTS_IN_FLIGHT` requests are open at once; and a name an answer gives for an
    /// identifier its request did not ask about is dropped.
    #[test]
    fn resolve_splits_by_bytes_runs_four_requests_at_once_and_keeps_only_asked_names() {
        use std::sync::{Arc, Condvar, Mutex};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let batches = Arc::new(Mutex::new(Vec::<usize>::new()));
        // Requests open now, the most ever open at once, and how many have arrived.
        let open = Arc::new((Mutex::new((0usize, 0usize, 0usize)), Condvar::new()));
        let (seen, gauge) = (Arc::clone(&batches), Arc::clone(&open));

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let (seen, gauge) = (Arc::clone(&seen), Arc::clone(&gauge));

                std::thread::spawn(move || {
                    use std::io::Write;

                    let Some((_, _, _, body)) = read_test_request(&mut stream) else { return };
                    seen.lock().unwrap().push(body.len());
                    let request: ResolveRequest = serde_json::from_slice(&body).unwrap();
                    let mut names: BTreeMap<String, String> = request.identifiers.iter()
                        .map(|id| (id.clone(), "Named".to_string())).collect();
                    names.insert("never-asked".to_string(), "Intruder".to_string());
                    let answer = serde_json::json!({ "names": names }).to_string();

                    // Hold every request until its whole round of the limit's worth has arrived (or a
                    // while passes), then a little longer, so a client exceeding the limit is seen
                    // exceeding it and one under it never fills a round.
                    let (lock, changed) = &*gauge;
                    let mut counts = lock.lock().unwrap();
                    let round_full = (counts.2 / RESOLVE_REQUESTS_IN_FLIGHT + 1) * RESOLVE_REQUESTS_IN_FLIGHT;
                    counts.0 += 1;
                    counts.1 = counts.1.max(counts.0);
                    counts.2 += 1;
                    changed.notify_all();
                    let (mut counts, _) = changed.wait_timeout_while(counts, std::time::Duration::from_secs(10),
                        |counts| counts.2 < round_full).unwrap();
                    drop(counts);
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    counts = lock.lock().unwrap();
                    counts.0 -= 1;
                    drop(counts);

                    let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        answer.len(), answer);
                });
            }
        });

        // 261 thousand-byte identifiers fill a request by bytes (each serialises to 1,003), so 2,088
        // of them are exactly two rounds of four requests, far under the count limit.
        let identifiers: Vec<String> = (0..2088).map(|i| format!("{:04}{}", i, "x".repeat(996))).collect();
        let client = RemoteClient::new(&url, None).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let names = runtime.block_on(client.resolve(identifiers));

        assert_eq!(names.len(), 2088, "every asked identifier resolves, and nothing else");
        assert!(!names.contains_key("never-asked"), "a name for an identifier no request asked about is dropped");
        let batches = batches.lock().unwrap().clone();
        assert_eq!(batches.len(), 2 * RESOLVE_REQUESTS_IN_FLIGHT, "split by bytes: {:?}", batches);
        assert!(batches.iter().all(|&bytes| bytes <= RESOLVE_REQUEST_IDENTIFIER_BYTES + 64), "{:?}", batches);
        assert_eq!(open.0.lock().unwrap().1, RESOLVE_REQUESTS_IN_FLIGHT,
            "the requests run at once, never more of them than the limit");
    }

    /// An importer that refuses the bundle stops the transfer at once: a remote that sends a
    /// refused header and then goes silent must get the refusal back, not a timeout once the
    /// silence budget runs out.
    #[test]
    fn a_refused_batch_bundle_reports_the_refusal_without_waiting_out_a_stalled_remote() {
        let _store = Scratch::new("batch-refused-then-stalled");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());

        std::thread::spawn(move || {
            use std::io::Write;

            if let Ok((mut stream, _)) = listener.accept() {
                let _ = read_test_request(&mut stream);
                let chunk = "forklift-bundle 2026-07-06\n";
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n",
                    chunk.len(), chunk);
                let _ = stream.flush();
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
        });

        let client = fast_client(&url);
        let silence_budget = FAST.connect + FAST.object_read;
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let started = std::time::Instant::now();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(20), client.fetch_batch(&["a".repeat(64)])).await
        }).expect("fetch_batch hung past its outer ceiling");

        let message = match outcome {
            Err(message) => message,
            Ok(_) => panic!("a refused bundle must not import"),
        };
        assert!(message.contains("refusing to import it"), "the importer's refusal is reported: {}", message);
        assert!(started.elapsed() < silence_budget,
            "the refusal must not wait out the silence budget {:?}: took {:?}", silence_budget, started.elapsed());
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
    /// sends must fail with a timeout, not hang waiting for a body that is never coming, and the
    /// message must name this client's own `connect + object_read` silence budget, hedged with
    /// "at least". Sized to the loose budget: `fetch_object` rides `bounded_object_reads`, not the
    /// tight `bounded_reads` the other three calls use. The figure is the only pin on
    /// `describe_transport_error`'s [`Posture::BoundedObjectReads`] arm: [`FAST`]'s figures differ
    /// from production's, so an arm that printed a production constant fails here.
    #[test]
    fn fetch_object_times_out_against_a_content_length_lie() {
        let remote = LyingContentLengthRemote::start();
        let client = fast_client(&remote.url);
        let hash = "a".repeat(64);
        let effective_budget = FAST.connect + FAST.object_read;
        let hard_ceiling = effective_budget + std::time::Duration::from_secs(20);

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
        assert!(
            message.contains(&format!("at least {:?}", effective_budget)),
            "must name this client's effective object-read silence budget {:?}, hedged with \
            \"at least\": {}", effective_budget, message
        );
    }

    // -----------------------------------------------------------------------------------
    // The error-body read: once a non-success status line has arrived, `error_of` reads the body
    // through `read_error_body`, under a byte cap and a time budget. The no-redirect client
    // `update_ref` rides carries only a `connect_timeout`, no `read_timeout` and no per-request
    // deadline at all — so a remote that delivers a
    // full status line and headers and then wedges before writing the body hangs the caller
    // forever. The caller's `error_body_budget` bounds the read itself, in `read_error_body`,
    // rather than trying to fix it with yet another client — no client can help here, since the
    // body-read bound is inherited from whichever client sent the request, and `error_of` serves
    // responses from all of them. `commit_lift` reads its terminal refusals through `error_of` too,
    // inside its own attempt deadline.
    // -----------------------------------------------------------------------------------

    /// A remote that delivers a non-success status line and full headers (claiming a body),
    /// then genuinely goes silent before writing a single byte of that body — the connection
    /// stays open (no FIN), so anything reading the body is left waiting on bytes that never
    /// arrive. Distinct from [`SilentRemote`] (silent *before* the status line even arrives) and
    /// [`LyingContentLengthRemote`] (a `2xx` success path): this is specifically the shape
    /// [`RemoteClient::error_body_budget`] exists for — status and headers fully delivered, only
    /// the error body itself left hanging.
    struct SilentErrorBodyRemote {
        url: String,
        _park: std::sync::mpsc::Sender<()>,
    }

    impl SilentErrorBodyRemote {
        fn start() -> SilentErrorBodyRemote {
            SilentErrorBodyRemote::start_with("HTTP/1.1 500 Internal Server Error")
        }

        /// The same remote answering with `status_line` instead of a `500`.
        fn start_with(status_line: &'static str) -> SilentErrorBodyRemote {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let (tx, rx) = std::sync::mpsc::channel::<()>();

            std::thread::spawn(move || {
                use std::io::Write;

                if let Ok((mut stream, _)) = listener.accept() {
                    let _ = read_test_request(&mut stream);
                    let _ = write!(
                        stream,
                        "{}\r\nContent-Type: application/json\r\nContent-Length: 4096\r\n\r\n",
                        status_line
                    );
                    let _ = stream.flush();
                    let _ = rx.recv();
                    drop(stream);
                }
            });

            SilentErrorBodyRemote { url, _park: tx }
        }
    }

    /// `missing_objects` rides [`Posture::HeadDeadline`]: its head-wait bound covers the status
    /// line only, so a `500` whose body then wedges is bounded by `error_of`'s own
    /// [`RemoteClient::error_body_budget`], counted from the status line, and by nothing else —
    /// the client's silence budget (`connect + object_read`) is set above it here, as production's
    /// is, so it cannot be what fires.
    ///
    /// Before this call had its own outer deadline at all, it rode the auto-following client with
    /// no bound of any kind — a `500` whose body then wedges hung it forever, with the status line
    /// and headers already fully delivered. The outer test-ceiling below is a safety net, not the
    /// property under test: if the bound in `read_error_body` were missing, that ceiling is what
    /// would trip (the test failing instead of hanging the suite) — unambiguous evidence the
    /// internal bound is gone, since status line and headers were fully delivered before the park,
    /// so the only thing left to hang on is the error-body read itself.
    ///
    /// Also asserts a **lower** bound on elapsed time: a red-then-green
    /// suite run alone does not prove `error_of` calls [`error_body_read_budget`] at all — nothing
    /// else in the suite requires that link, so a regression back to the bare
    /// error-body read budget (dropping the folded-in connect bound) still
    /// passes every other assertion here, which only ceiling-checks. `tokio::time::timeout` never
    /// fires early and this fixture never sends a body, so the read burns the *entire* budget —
    /// making the lower bound exact, not approximate.
    ///
    /// On its own this pins only that *some* connect-shaped value gets folded in: a lower bound
    /// cannot tell "reads `self.budgets.connect`" apart from "hardcodes the larger
    /// [`REMOTE_CONNECT_TIMEOUT`]", which only waits longer. That is the gap
    /// `error_body_budget_reads_this_field_not_a_rival_constant` closes, with a connect bound no
    /// production constructor can produce, so a hardcoded rival can never coincidentally match it.
    /// Four tests together pin the property (`commit_lift`'s call site has its own test, after this
    /// one): this one pins call-site-uses-the-budget,
    /// `error_body_read_budget_folds_in_the_connect_timeout` pins the arithmetic the budget
    /// computes, `error_body_budget_reads_this_field_not_a_rival_constant` pins that the accessor
    /// reads *this instance's* connect timeout field rather than hardcoding any constant a real
    /// constructor could produce, and `error_body_budget_is_70s_for_a_real_tor_mode_client` pins
    /// that the value at the one client mode that actually ships a non-default budget is the
    /// correct, uncapped 70s (the field-read property alone does not imply this one: a future cap
    /// on the result would leave it unaffected while breaking this).
    #[test]
    fn missing_objects_bounds_the_error_body_read_after_a_wedged_500() {
        let remote = SilentErrorBodyRemote::start();
        // The silence budget the presence call's client carries (connect + `object_read`) above
        // the error-body budget, as production's is; the shared fixture's is below it.
        let budgets = Budgets { object_read: std::time::Duration::from_millis(15_700), ..stalled_error_body_budgets() }.checked();
        let client = RemoteClient::new_test_with_budgets(&remote.url, budgets);
        // Any real hash string works — the fixture never inspects the body.
        let hashes = vec!["a".repeat(64)];
        let Bracket { lower_bound, upper_bound, nearest_rival, outer_ceiling } =
            error_body_bracket(budgets, &[client.presence_negotiation_budget(hashes.len())]);

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
        // The other side: a misarm that waits longer (the production error-body allowance folded
        // with this connect bound, say, or the call's own head-wait bound) would still clear the
        // lower bound and fit under the outer ceiling, so only an upper bound tells it apart.
        assert!(
            elapsed < upper_bound,
            "elapsed {:?} is past {:?} (the midpoint between this client's error-body budget {:?} \
            and {:?}, the nearest longer budget a misarm could fire at) — the error-body read is \
            not armed with this instance's budget",
            elapsed, upper_bound, lower_bound, nearest_rival
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

    /// A lift commit's terminal refusal whose body stalls fails under the error-body budget, not
    /// the attempt's head-wait bound, not the client's silence budget, and not never. Bracketed
    /// like the `missing_objects` test above: at least the injected budget (no shorter bound
    /// fired), under the midpoint toward the nearest longer budget a misarm could fire at, and
    /// under an outer ceiling. A `422` because a `5xx` is retried without its body being read at
    /// all. The message is the status reason that stands in for an unread body.
    #[test]
    fn commit_lift_bounds_the_error_body_read_after_a_wedged_422() {
        let remote = SilentErrorBodyRemote::start_with("HTTP/1.1 422 Unprocessable Entity");
        // The silence budget the commit's client carries (connect + `object_read`) above the
        // error-body budget, as production's is; the shared fixture's is below it.
        let budgets = Budgets { object_read: std::time::Duration::from_millis(15_700), ..stalled_error_body_budgets() }.checked();
        let client = RemoteClient::new_test_with_budgets(&remote.url, budgets);
        // The attempt's head-wait bound, looser than the error-body budget as production's can be.
        let attempt = std::time::Duration::from_millis(16_300);
        let Bracket { lower_bound, upper_bound, nearest_rival, outer_ceiling } =
            error_body_bracket(budgets, &[attempt]);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let started = std::time::Instant::now();

        let outcome = runtime.block_on(async {
            tokio::time::timeout(outer_ceiling, client.commit_lift("session", &[], &[], false, attempt)).await
        });
        let elapsed = started.elapsed();

        let message = match outcome {
            Ok(Err(message)) => message,
            Ok(Ok(attempt)) => panic!("a 422 is terminal, not {:?}", attempt),
            Err(_) => panic!("the stalled body held the lift past {:?}: its read is unbounded", outer_ceiling),
        };
        assert!(message.contains("(422): Unprocessable Entity"), "the status stands in for the body: {}", message);
        assert!(elapsed >= lower_bound, "the error-body budget fired early ({:?} < {:?})", elapsed, lower_bound);
        assert!(elapsed < upper_bound,
            "elapsed {:?} is past {:?}, the midpoint toward {:?}: the lift commit's error-body read \
            is not armed with this instance's budget", elapsed, upper_bound, nearest_rival);
    }

    /// A Tor-mode fixture (this test's predecessor,
    /// `error_body_budget_reads_this_instances_connect_timeout`) cannot tell
    /// "[`RemoteClient::error_body_budget`] reads `self.budgets.connect`" apart from "hardcodes
    /// [`REMOTE_CONNECT_TIMEOUT_TOR`] directly" — a mutation to the latter still passed, because a
    /// `TorMode::On` client's field genuinely *is* 60s, the same value the hardcoded rival would
    /// return. The direct-client version has the identical problem one level down (5s either way).
    /// No fixture built through a production constructor can separate the two:
    /// [`RemoteClient::new`] only ever yields [`REMOTE_CONNECT_TIMEOUT`] (5s) and
    /// [`RemoteClient::new_with_tor`] under `TorMode::On` only ever yields
    /// [`REMOTE_CONNECT_TIMEOUT_TOR`] (60s) — both are rivals a hardcoded mutant can impersonate.
    /// [`RemoteClient::new_test_with_budgets`] injects a third value, `SENTINEL_CONNECT_TIMEOUT`
    /// (7s), that no production constructor can ever produce, so a mutant hardcoding *either* rival
    /// constant is caught here.
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
    /// stale at once, including the one it wrote itself. What each of these tests actually rests on
    /// is the separation claim — this sentinel collides with nothing — and that claim is
    /// re-checkable in a second by the grep above. The census only ever decays between edits.
    ///
    /// A coincidental *sum* of two unrelated constants would not be a rival even where one exists:
    /// the mutation this test defends against is a hardcoded *connect_timeout* substitution — one
    /// of the two values a production constructor can actually produce — not an arbitrary
    /// recombination of budgets belonging to a different phase, which nothing in this module ever
    /// adds to a connect timeout. What would actually matter
    /// is a rival for this test's asserted *output*, `17s` (`7 + TEST_ERROR_BODY_READ_TIMEOUT`):
    /// the two values a hardcoded production `connect_timeout` would actually produce here instead
    /// of reading `self.budgets.connect` are `15s` (`REMOTE_CONNECT_TIMEOUT` +
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

        let client = RemoteClient::new_test_with_budgets(
            "http://forklift-fork-49-error-body-budget-sentinel-test.invalid",
            Budgets { connect: SENTINEL_CONNECT_TIMEOUT, ..Budgets::production(false) },
        );

        assert_eq!(
            client.error_body_budget(),
            SENTINEL_CONNECT_TIMEOUT + TEST_ERROR_BODY_READ_TIMEOUT,
            "error_body_budget must fold in *this instance's* connect_timeout — a value neither \
            RemoteClient::new (5s) nor RemoteClient::new_with_tor under TorMode::On (60s) can ever \
            produce, so this cannot pass by coincidentally matching a hardcoded rival constant"
        );
    }

    /// The sentinel test above pins *reads-the-field-not-a-rival* but
    /// not what `error_body_budget` actually returns for a real Tor-mode client — the two are
    /// different claims, and the sentinel does not subsume this one: see that test's own doc for
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
    /// `RemoteClient::new`, where `self.budgets.connect` == [`REMOTE_CONNECT_TIMEOUT`] ==
    /// [`TEST_DIRECT_CONNECT_TIMEOUT`] (5s) — so a hardcoded-constant mutant of `resolve_budget`
    /// (`REMOTE_CONNECT_TIMEOUT` in place of `self.budgets.connect`) is extensionally identical to
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
    /// `TEST_TIGHT_READ_TIMEOUT`), and 21s separates from both. This is a separation report against
    /// those two rivals, not a claim that no other combination of this module's constants could
    /// ever sum to 21 — characterising a separation table as complete has cost a fresh defect on
    /// every attempt this module's history has tried it, so that claim is deliberately not made.
    #[test]
    fn resolve_budget_reads_this_field_not_a_rival_constant() {
        const SENTINEL_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(11);

        let client = RemoteClient::new_test_with_budgets(
            "http://forklift-fork-96-resolve-budget-sentinel-test.invalid",
            Budgets { connect: SENTINEL_CONNECT_TIMEOUT, ..Budgets::production(false) },
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
    /// `self.budgets.connect` is [`REMOTE_CONNECT_TIMEOUT`] (5s), so a mutant hardcoding that
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
    /// [`retryable_head_budget_reads_this_field_not_a_rival_constant`]'s own doc gives: written
    /// symbolically as `SENTINEL_CONNECT_TIMEOUT + BATCH_HEAD_PATIENCE`, a change to the patience
    /// moves both sides of the assertion together and this test stays green through it.
    #[test]
    fn batch_head_budget_reads_this_field_not_a_rival_constant() {
        const SENTINEL_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(17);

        let client = RemoteClient::new_test_with_budgets(
            "http://forklift-fork-104-batch-head-budget-sentinel-test.invalid",
            Budgets { connect: SENTINEL_CONNECT_TIMEOUT, ..Budgets::production(false) },
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
            clamp_head_deadline_payload exists to repair, discharged here at this producer"
        );
    }

    // -----------------------------------------------------------------------------------
    // FORK-92 (budget 1): `missing_objects`/`upload_targets` ride `Posture::HeadDeadline`, its
    // head-wait sized by `RemoteClient::presence_negotiation_budget`. The four tests below mirror
    // the `error_body_budget`/`resolve_budget` pattern directly above:
    // reads-n (an implementation ignoring the batch size and returning a flat constant must
    // fail), reads-this-instance's-connect_timeout-not-a-rival (the sentinel pattern), the real
    // Tor-mode value, and that the bound actually fires against a remote that never answers.
    // -----------------------------------------------------------------------------------

    /// A flat-constant implementation of `presence_negotiation_budget` — one that ignores `n`
    /// entirely — passes every other test in this section (the sentinel and Tor-mode tests both
    /// fix `n`). This is the one that would catch it, and the one that pins the rule's shape: the
    /// floor holds unchanged up to the batch whose scaled cost equals it (6,000 hashes at 5ms
    /// each against 30s), and above it the budget grows by exactly the per-hash rate. A sum of
    /// floor and scaled term would fail the first assertion; a flat constant the last two.
    /// [`RemoteClient::presence_body_budget`] at both calls' largest batch, pinned as literals
    /// against the direct production budgets, plus the instance read on [`FAST`]: a figure built
    /// from production constants rather than `self.budgets` names neither.
    #[test]
    fn presence_body_budget_is_the_silence_budget_plus_the_cap_at_the_body_rate() {
        let production = RemoteClient::new_test_with_budgets("http://127.0.0.1:1", PRODUCTION_MIRROR);
        assert_eq!(
            production.presence_body_budget(
                body_utils::per_item_body_cap(MAX_MISSING_BATCH, MISSING_BODY_BYTES_PER_HASH)
            ),
            std::time::Duration::from_secs_f64(65.0 + 20.53125),
            "a full missing batch: 65s of silence budget plus 1,345,536 bytes at 64 KiB/s"
        );
        assert_eq!(
            production.presence_body_budget(
                body_utils::per_item_body_cap(MAX_UPLOAD_TARGETS_BATCH, UPLOAD_TARGETS_BODY_BYTES_PER_HASH)
            ),
            std::time::Duration::from_secs(191),
            "a full upload-targets batch: 65s of silence budget plus 8,257,536 bytes at 64 KiB/s"
        );
        let fast = RemoteClient::new_test_with_budgets("http://127.0.0.1:1", FAST);
        assert_eq!(
            fast.presence_body_budget(64 * 1024),
            FAST.connect + FAST.object_read + std::time::Duration::from_secs(1),
            "presence_body_budget must fold in this instance's connect and object-read budgets"
        );
    }

    #[test]
    fn presence_negotiation_budget_reads_n() {
        let client = RemoteClient::new_test_with_budgets(
            "http://forklift-fork-92-presence-budget-n-test.invalid",
            Budgets { connect: TEST_DIRECT_CONNECT_TIMEOUT, ..Budgets::production(false) },
        );
        let per_hash = std::time::Duration::from_secs_f64(PRESENCE_ALLOWANCE_MS_PER_OP / 1000.0);
        let crossover = 6_000;
        assert_eq!(
            per_hash * crossover as u32, TEST_RETRYABLE_HEAD_PATIENCE,
            "sanity: 6,000 hashes at the per-hash rate cost exactly the floor"
        );

        for n in [1, crossover] {
            assert_eq!(
                client.presence_negotiation_budget(n),
                TEST_DIRECT_CONNECT_TIMEOUT + TEST_RETRYABLE_HEAD_PATIENCE,
                "at n={} the scaled cost does not exceed the floor, so the budget is the floor \
                alone — not floor plus scaled cost", n
            );
        }
        assert_eq!(
            client.presence_negotiation_budget(crossover + 1),
            TEST_DIRECT_CONNECT_TIMEOUT + TEST_RETRYABLE_HEAD_PATIENCE + per_hash,
            "one hash past the crossover, the scaled cost lifts the budget by one hash's rate"
        );
        assert_eq!(
            client.presence_negotiation_budget(MAX_MISSING_BATCH),
            TEST_DIRECT_CONNECT_TIMEOUT + std::time::Duration::from_secs(50),
            "at MAX_MISSING_BATCH the budget is connect plus the 50s scaled cost"
        );
    }

    /// Mirrors [`error_body_budget_reads_this_field_not_a_rival_constant`]/
    /// [`resolve_budget_reads_this_field_not_a_rival_constant`] for
    /// [`RemoteClient::presence_negotiation_budget`] — the identical gap for the identical reason:
    /// every existing behavioral test of `missing_objects`/`upload_targets` builds a direct client
    /// via `RemoteClient::new`, where `self.budgets.connect` == [`REMOTE_CONNECT_TIMEOUT`] (5s) —
    /// so a hardcoded-constant mutant (`REMOTE_CONNECT_TIMEOUT` in place of `self.budgets.connect`)
    /// is extensionally identical to the correct implementation everywhere those tests look, and
    /// would still give a Tor client a head-wait sized off a 5s connect budget against a 60s
    /// connect allowance — the bound firing during circuit build, [`REMOTE_CONNECT_TIMEOUT_TOR`]'s
    /// own reason for existing.
    ///
    /// `SENTINEL_CONNECT_TIMEOUT` (13s) is a third value, distinct from `error_body_budget`'s own
    /// sentinel (7s) and `resolve_budget`'s (11s) for the same "read independently, not one case
    /// split in half" reason those two are distinct from each other. The asserted output is a
    /// literal 43s (`13 + 30`, the floor winning at `n=1`), which separates from the two rivals a
    /// hardcoded production `connect_timeout` would produce, 35s and 90s. This is a separation
    /// report against those two rivals, not a claim that no other combination of this module's
    /// constants could sum to 43s.
    #[test]
    fn presence_negotiation_budget_reads_this_field_not_a_rival_constant() {
        const SENTINEL_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(13);

        let client = RemoteClient::new_test_with_budgets(
            "http://forklift-fork-92-presence-budget-sentinel-test.invalid",
            Budgets { connect: SENTINEL_CONNECT_TIMEOUT, ..Budgets::production(false) },
        );

        assert_eq!(
            client.presence_negotiation_budget(1),
            std::time::Duration::from_secs(43),
            "presence_negotiation_budget must fold in *this instance's* connect_timeout — a value \
            neither RemoteClient::new (5s) nor RemoteClient::new_with_tor under TorMode::On (60s) \
            can ever produce, so this cannot pass by coincidentally matching a hardcoded rival \
            constant (35s or 90s)"
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
    /// worst case, where the scaled term wins over the floor.
    #[test]
    fn presence_negotiation_budget_is_110s_for_a_real_tor_mode_client_at_the_missing_batch_cap() {
        let tor = TorSettings { mode: TorMode::On, proxy: DEFAULT_TOR_PROXY.to_string() };
        let client = RemoteClient::new_with_tor(
            "http://forklift-fork-92-presence-budget-tor-endpoint-test.invalid", None, tor,
        ).unwrap();

        assert_eq!(
            client.presence_negotiation_budget(MAX_MISSING_BATCH),
            std::time::Duration::from_secs(110),
            "a real TorMode::On client's presence_negotiation_budget at the largest batch \
            missing_objects/upload_targets ever send must be exactly 60s connect + max(30s floor, \
            10,000 * 5ms) — otherwise the bound fires during Tor circuit build on the very calls \
            it exists to protect"
        );
    }

    /// Mirrors [`error_body_budget_reads_this_field_not_a_rival_constant`]/
    /// [`resolve_budget_reads_this_field_not_a_rival_constant`]/
    /// [`presence_negotiation_budget_reads_this_field_not_a_rival_constant`] for
    /// [`RemoteClient::retryable_head_budget`] — the identical gap: every end-to-end test of
    /// `upload_signature`/`put_trust` builds a direct client via `RemoteClient::new`, where
    /// `self.budgets.connect` == [`REMOTE_CONNECT_TIMEOUT`] (5s), so a hardcoded-constant mutant
    /// (`REMOTE_CONNECT_TIMEOUT` in place of `self.budgets.connect`) is extensionally identical to
    /// the correct implementation everywhere those tests look — `5 + RETRYABLE_HEAD_PATIENCE`
    /// either way — and neither elapsed-time end-to-end test above can catch it either, for the
    /// same reason `resolve_gives_up_on_a_remote_that_never_stops_trickling`'s own doc names for
    /// the identical rival: at production `connect_timeout`, the two implementations produce the
    /// exact same `Duration`.
    ///
    /// `SENTINEL_CONNECT_TIMEOUT` (9s) is distinct from the two production connect-timeout
    /// constants this test needs to separate from — [`REMOTE_CONNECT_TIMEOUT`] (5s) and
    /// [`REMOTE_CONNECT_TIMEOUT_TOR`] (60s). This test's asserted output, 39s (`9 + 30`),
    /// separates from the two rivals a hardcoded production `connect_timeout` would produce
    /// instead of reading `self.budgets.connect`: 35s and 90s. This is a separation report
    /// against those two rivals, not a claim that no other combination of this module's constants
    /// could ever sum to 39 — characterising a separation table as complete has cost a fresh
    /// defect on every attempt this module's history has tried it, so that claim is deliberately
    /// not made.
    #[test]
    fn retryable_head_budget_reads_this_field_not_a_rival_constant() {
        const SENTINEL_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(9);

        let client = RemoteClient::new_test_with_budgets(
            "http://forklift-fork-49-single-write-budget-sentinel-test.invalid",
            Budgets { connect: SENTINEL_CONNECT_TIMEOUT, ..Budgets::production(false) },
        );

        assert_eq!(
            client.retryable_head_budget(),
            std::time::Duration::from_secs(39),
            "retryable_head_budget must be this instance's 9s connect_timeout plus the 30s \
            retryable floor. The expected value is a literal, deliberately, and not \
            `SENTINEL_CONNECT_TIMEOUT + RETRYABLE_HEAD_PATIENCE`: written symbolically, a change \
            to the floor's own arithmetic moves both sides of this assertion together and every \
            budget test in this module stays green — including a halving of the floor, which is \
            the under-pricing direction that turns a healthy slow write into an unresolvable \
            uncertain-outcome error. 39s also separates from the two rivals a hardcoded \
            production connect_timeout would produce, 35s and 90s"
        );
    }

    /// `missing_objects` must terminate against a remote that connects, reads the request in
    /// full, and then genuinely goes silent — never answering — rather than hang forever. It once
    /// rode an auto-following client with no bound of any kind; this is the falsifying test for
    /// that defect, the same shape as `fetch_info_times_out_against_a_silent_remote` and
    /// siblings above, applied to the head-deadline posture this call now rides.
    ///
    /// A merely-generous outer ceiling alone does not separate the right implementation from a
    /// plausible wrong one: `hard_ceiling` here is `effective_budget` plus generous slack, and
    /// `Posture::BoundedReads`'s own effective silence budget against a fully silent remote
    /// (`connect + read`) sits well inside it — so a mutant that rewired this call site onto
    /// `BoundedReads` instead of `HeadDeadline` would still return an `Err` inside that ceiling,
    /// and the test would still pass, certifying nothing about which posture actually fired.
    /// `upper_bound` below is the assertion that actually distinguishes them, set at the *midpoint*
    /// between this call's own budget and that rival's effective silence budget (not an arbitrary
    /// margin over the true value): that split spends exactly as much slack on absorbing
    /// scheduling/dispatch noise as it keeps in reserve to still catch the rewiring, and makes the
    /// trade a reader who widens this margin later is making explicit — how much closer to the
    /// rival's own budget they are choosing to get.
    #[test]
    fn missing_objects_times_out_against_a_silent_remote() {
        let remote = SilentRemote::start();
        // A read budget seconds past this call's own deadline, so the midpoint between the two
        // leaves room for a loaded runner rather than the quarter-second FAST's spacing gives.
        let budgets = Budgets { read: WIDE_READ, ..FAST };
        let client = RemoteClient::new_test_with_budgets(&remote.url, budgets);
        let hashes = vec!["a".repeat(64)];
        let effective_budget = client.presence_negotiation_budget(hashes.len());
        let rival_bounded_reads_budget = budgets.connect + budgets.read;
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
                "missing_objects hung past the test's own {:?} ceiling — no head-wait bound fired \
                at all", hard_ceiling
            ))
            .expect_err("a silent remote must not appear to succeed");

        assert!(
            elapsed <= upper_bound,
            "elapsed {:?} exceeds {:?} (the midpoint between effective_budget {:?} and \
            BoundedReads' own {:?} silence budget) — missing_objects took closer to a \
            Posture::BoundedReads-shaped silence budget than its own presence_negotiation_budget \
            head-wait; a rewiring onto BoundedReads would still return an Err inside the \
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
        // A read budget seconds past this call's own deadline, so the midpoint between the two
        // leaves room for a loaded runner rather than the quarter-second FAST's spacing gives.
        let budgets = Budgets { read: WIDE_READ, ..FAST };
        let client = RemoteClient::new_test_with_budgets(&remote.url, budgets);
        let hashes = vec!["a".repeat(64)];
        let effective_budget = client.presence_negotiation_budget(hashes.len());
        let rival_bounded_reads_budget = budgets.connect + budgets.read;
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
            "upload_targets hung past the test's own {:?} ceiling — no head-wait bound fired \
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
            head-wait; a rewiring onto BoundedReads would still return an Err inside the \
            generous hard_ceiling above, so this tighter bound is what actually separates the two",
            elapsed, upper_bound, effective_budget, rival_bounded_reads_budget
        );
    }

    // -----------------------------------------------------------------------------------
    // Transport-error composer posture: `missing_objects`/`upload_targets` used to map a
    // `.send()`/`.json()` failure with a bare `format!`, so a genuine deadline expiry read to an
    // operator as a raw reqwest Debug string, never told it was a timeout at all. Both now route
    // through the composers, which take the exact `Posture` a call was armed with: a head-wait
    // expiry reports the armed head-wait figure verbatim (an exact figure, not "at least"), and a
    // stall after the headers reports the silence budget.
    // -----------------------------------------------------------------------------------

    /// The falsifying test for `missing_objects`'s send-error branch: before this fix it composed
    /// a bare `format!("Error while negotiating with the remote: {}", e)` on `.send()` failure —
    /// no timeout wording, no figure — so an operator staring at a stuck negotiation had no way to
    /// tell "the remote is genuinely gone" from "the bound this call itself armed just expired."
    /// This pins that the composed message names *that exact armed head-wait*, read from
    /// [`RemoteClient::presence_negotiation_budget`] itself rather than a hand-written duration
    /// literal — a hand-written literal could silently drift from the real arithmetic and this
    /// test would never notice.
    ///
    /// **Why a 440ms sentinel connect bound over [`FAST`].** No production constructor can produce
    /// it, and it is none of [`FAST`]'s own figures, so a rival implementation that hardcodes a
    /// production constant, or reads a different field of the instance, cannot coincidentally
    /// match this test's expected figure.
    ///
    /// At `n=1` the true budget is `440ms + max(FAST.retryable_head_patience (1.69s), 1 *
    /// PRESENCE_ALLOWANCE_MS_PER_OP (5ms)) = 2.13s`. The rivals below, at this test's own
    /// `S=440ms, n=1`, print (computed from the arithmetic, not run as mutants):
    ///
    /// | Rival | Prints | Separated from 2.13s |
    /// |---|---|---|
    /// | status-quo bare `format!` | no figure, no timeout wording | yes |
    /// | floor plus scaled term, not the larger | 2.135s | yes |
    /// | double-counting (`connect + head budget`) | 2.57s | yes |
    /// | connect-blind (drops `self.budgets.connect`) | 1.69s | yes |
    /// | silence wording (`connect + object_read`) | 4.54s | yes |
    /// | hardcoded production `RETRYABLE_HEAD_PATIENCE` | 30.44s | yes |
    /// | hardcoded production connect (5s / 60s) | 6.69s / 61.69s | yes |
    #[test]
    fn missing_objects_deadline_message_names_the_armed_head_wait() {
        const SENTINEL_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(440);

        let remote = SilentRemote::start();
        let client = RemoteClient::new_test_with_budgets(
            &remote.url, Budgets { connect: SENTINEL_CONNECT_TIMEOUT, ..FAST },
        );
        let hashes = vec!["a".repeat(64)];
        let expected_budget = client.presence_negotiation_budget(hashes.len());
        assert_eq!(
            expected_budget,
            std::time::Duration::from_millis(2_130),
            "sanity: this test's own doc promises 2.13s — 440ms connect + the 1.69s \
            FAST.retryable_head_patience, which outweighs 1 * 5ms PRESENCE_ALLOWANCE_MS_PER_OP"
        );
        let hard_ceiling = expected_budget + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.missing_objects(&hashes)).await
        });

        let message = outcome
            .unwrap_or_else(|_| panic!(
                "missing_objects hung past the test's own {:?} ceiling — no head-wait bound fired \
                at all", hard_ceiling
            ))
            .expect_err("a silent remote must not appear to succeed");

        assert!(
            message.contains(&format!("sent no response headers within {:?}", expected_budget)),
            "must name the exact armed head-wait {:?} — read from presence_negotiation_budget \
            itself, not a hand-written duration literal: {}", expected_budget, message
        );
        assert!(
            message.to_lowercase().contains("timed out"),
            "must carry the timeout wording, not the status-quo bare format!: {}", message
        );
        assert!(
            !message.to_lowercase().contains("could not connect"),
            "must not carry the connect-timeout wording — the head-wait expired after the \
            connection was accepted: {}", message
        );
    }

    /// The `upload_targets` counterpart of
    /// [`missing_objects_deadline_message_names_the_armed_head_wait`] — a separate code path (its
    /// own `.send()` map_err site), so it gets its own test rather than being parameterised into
    /// that one. Same defect, same fix, same rival table (that test's own doc carries it; not
    /// repeated here).
    #[test]
    fn upload_targets_deadline_message_names_the_armed_head_wait() {
        const SENTINEL_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(440);

        let remote = SilentRemote::start();
        let client = RemoteClient::new_test_with_budgets(
            &remote.url, Budgets { connect: SENTINEL_CONNECT_TIMEOUT, ..FAST },
        );
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
            "upload_targets hung past the test's own {:?} ceiling — no head-wait bound fired \
            at all", hard_ceiling
        ));
        let message = match inner {
            Err(message) => message,
            Ok(_) => panic!("a silent remote must not appear to succeed"),
        };

        assert!(
            message.contains(&format!("sent no response headers within {:?}", expected_budget)),
            "must name the exact armed head-wait {:?} — read from presence_negotiation_budget \
            itself, not a hand-written duration literal: {}", expected_budget, message
        );
        assert!(
            message.to_lowercase().contains("timed out"),
            "must carry the timeout wording, not the status-quo bare format!: {}", message
        );
        assert!(
            !message.to_lowercase().contains("could not connect"),
            "must not carry the connect-timeout wording — the head-wait expired after the \
            connection was accepted: {}", message
        );
    }

    /// A remote that sends a full `200 OK` with JSON headers and a `Content-Length` far larger
    /// than the partial JSON fragment it actually writes, then genuinely goes silent — same
    /// parked-connection shape as [`LyingContentLengthRemote`], but for a JSON body instead of raw
    /// bytes, and generic to any request path (via [`read_test_request`]) rather than one
    /// endpoint. Exists for the tests below: a fully [`SilentRemote`] never gets past headers, so
    /// it cannot distinguish `read_json`'s transport-failure branch for `missing_objects`/
    /// `upload_targets` (routed through the composer) from its parse-failure branch (the "not valid
    /// JSON" message) — both look identical against total silence, because neither branch is
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
                    // never arrives and the connection is never closed, so the body read is
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

    /// Distinguishes the body-phase fix from a cosmetic one: reporting every failed read in
    /// `read_json` as "not valid JSON" (dropping its transport-failure branch) would still pass
    /// every silent-remote test above — a fully silent remote never sends headers at all, so it
    /// never reaches the body read's error arm in the first place. Against
    /// [`PartialJsonThenSilentRemote`] it does: the head-wait bound is spent once the headers
    /// arrive, so what ends the stalled body is the client's silence budget (the body's total,
    /// `presence_body_budget`, sits strictly above it) — and the message must be the silence
    /// message, not the JSON-parse-failure one, since the remote never sent garbage: it sent a
    /// valid-so-far prefix and then nothing further.
    #[test]
    fn missing_objects_deadline_message_survives_a_partial_json_body() {
        const SENTINEL_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(440);

        let remote = PartialJsonThenSilentRemote::start();
        let client = RemoteClient::new_test_with_budgets(
            &remote.url, Budgets { connect: SENTINEL_CONNECT_TIMEOUT, ..FAST },
        );
        let hashes = vec!["a".repeat(64)];
        let silence = SENTINEL_CONNECT_TIMEOUT + FAST.object_read;
        let body_budget =
            client.presence_body_budget(body_utils::per_item_body_cap(1, MISSING_BODY_BYTES_PER_HASH));
        assert!(body_budget > silence, "the silence budget fires before the body's total");
        let hard_ceiling = body_budget + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.missing_objects(&hashes)).await
        });

        let message = outcome
            .unwrap_or_else(|_| panic!(
                "missing_objects hung past the test's own {:?} ceiling — the presence answer's \
                body read is unbounded", hard_ceiling
            ))
            .expect_err("a body that never completes must not appear to succeed");

        assert!(
            message.contains(&format!("no further bytes for at least {:?}", silence)),
            "a partial JSON body followed by silence must compose the silence message naming the \
            armed {:?}, not fall through to the unconditional \"not valid JSON\" message: {}",
            silence, message
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

    /// Pins `read_json`'s parse-failure branch for `missing_objects`, which the two partial-body
    /// tests above cannot reach: their fixture only ever produces a transport failure, so routing
    /// *every* failed read through the composer — parse failures included — leaves both of them
    /// green.
    ///
    /// Measured under that mutant, against a proxy or captive portal answering `200` with an HTML
    /// interstitial: the message becomes `"Error while negotiating with the remote: expected value
    /// at line 1 column 1"`. A decode error is neither a connect failure nor a timeout, so
    /// `classify` routes it to [`TransportFailure::Other`] and it degrades to the generic
    /// transport wrapper — it does **not** fabricate a deadline figure. So the wording assertion
    /// is the one doing the pinning here, and it is the only one: the elapsed check below is a
    /// fixture-health check, not a rival-excluding assertion, since any run that reached the armed
    /// budget ended via a timeout and so could not have produced this wording anyway. It
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
        let client = RemoteClient::new_test_with_budgets(
            &remote.url, Budgets { connect: SENTINEL_CONNECT_TIMEOUT, ..Budgets::production(false) },
        );
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
        let client = RemoteClient::new_test_with_budgets(
            &remote.url, Budgets { connect: SENTINEL_CONNECT_TIMEOUT, ..Budgets::production(false) },
        );
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

    /// Serve one canned answer to whatever request arrives first, on any path: `head` (status line
    /// and headers, without the blank line) then `body`.
    fn serve_one_answer(head: String, body: String) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());

        std::thread::spawn(move || {
            use std::io::Write;

            if let Ok((mut stream, _)) = listener.accept() {
                let _ = read_test_request(&mut stream);
                let _ = write!(stream, "{}\r\nConnection: close\r\n\r\n{}", head, body);
                let _ = stream.flush();
            }
        });

        url
    }

    /// The handshake lists every pallet, and a warehouse may have any number of them: a cap sized
    /// for a "small" answer would refuse a real warehouse outright (750 pallets already pass
    /// 64 KiB). Five thousand pallets, about 450 KB, must handshake.
    #[test]
    fn a_handshake_listing_thousands_of_pallets_is_read() {
        let pallets: BTreeMap<String, String> = (0..5000)
            .map(|i| (format!("feature/branch-number-{}", i), "a".repeat(64)))
            .collect();
        let body = serde_json::json!({
            "protocol": PROTOCOL_VERSION, "default_pallet": "main", "pallets": pallets,
            "trust": null, "chunking": false,
        }).to_string();
        assert!(body.len() > 400_000, "the fixture must be far past a small-body cap");

        let url = serve_one_answer(
            format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}", body.len()),
            body,
        );
        let client = RemoteClient::new(&url, None).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

        let info = runtime.block_on(client.fetch_info()).unwrap_or_else(|e| panic!("{}", e));
        assert_eq!(info.pallets.len(), 5000);
    }

    /// The other side of the handshake cap: an answer declaring more than it is refused before a
    /// byte of it is read, with a message naming what was refused and the cap.
    #[test]
    fn a_handshake_over_its_cap_is_refused_naming_the_cap() {
        let url = serve_one_answer(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}",
                HANDSHAKE_BODY_CAP + 1
            ),
            String::new(),
        );
        let client = RemoteClient::new(&url, None).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

        let message = match runtime.block_on(client.fetch_info()) {
            Err(message) => message,
            Ok(_) => panic!("an over-cap handshake must not appear to succeed"),
        };
        assert!(message.contains("handshake") && message.contains(&HANDSHAKE_BODY_CAP.to_string()),
            "the refusal names what was refused and the cap: {}", message);
    }

    /// A signature sidecar over its cap is refused in words that read: the remote's signature of
    /// the parcel, named once.
    #[test]
    fn an_over_cap_signature_is_refused_naming_it_once() {
        let url = serve_one_answer(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}", body_utils::SMALL_BODY_CAP + 1),
            String::new(),
        );
        let client = RemoteClient::new(&url, None).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();

        let message = runtime.block_on(client.fetch_signature("abc")).expect_err("an over-cap sidecar is refused");
        assert!(message.contains("The answer to the remote's signature of abc is larger than"), "{}", message);
    }

    /// A lift-commit refusal whose description runs past the small-answer cap is still read (it
    /// may list many blobs), and one past the error-body cap says its description went unread
    /// instead of passing for the bare status.
    #[test]
    fn a_long_lift_commit_refusal_is_read_and_an_over_cap_one_says_so() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let refusal = |length: u64| {
            let url = serve_one_answer(
                format!("HTTP/1.1 422 Unprocessable Entity\r\nContent-Type: application/json\r\nContent-Length: {}", length),
                if length <= ERROR_BODY_CAP {
                    let error = "b".repeat(length as usize - r#"{"error":""}"#.len());
                    serde_json::json!({ "error": error }).to_string()
                } else {
                    String::new()
                },
            );
            let client = RemoteClient::new(&url, None).unwrap();
            match runtime.block_on(client.commit_lift("session", &[], &[], false, std::time::Duration::from_secs(30))) {
                Err(message) => message,
                Ok(_) => panic!("a 422 is refused"),
            }
        };

        let long = refusal(body_utils::SMALL_BODY_CAP * 2);
        assert!(long.contains(&"b".repeat(1000)), "the long description is read: {:.200}", long);

        let unread = refusal(ERROR_BODY_CAP + 1);
        assert!(unread.contains("was not read"), "{}", unread);
    }

    // -----------------------------------------------------------------------------------
    // The lift-commit retry contract: each attempt is classified from its status line and headers,
    // and one deadline per batch bounds the retrying. A scripted remote answers each connection
    // with the next answer of its script, repeating the last.
    // -----------------------------------------------------------------------------------

    /// What a [`CommitRemote`] does with one connection.
    #[derive(Clone)]
    enum CommitAnswer {
        /// Write this status line and these headers (no blank line), an empty body, and close.
        Reply(&'static str),
        /// Write these raw bytes, then close: a response cut off mid-head or mid-body.
        CutOff(&'static str),
        /// Read the request and never answer, holding the connection open.
        Wedge,
        /// Read the request and close the connection without a byte of answer.
        Drop,
        /// Write this status line and these headers after `head_after`, then this body after a
        /// further `body_after`, and close.
        Slow {
            head: &'static str,
            head_after: std::time::Duration,
            body: &'static str,
            body_after: std::time::Duration,
        },
    }

    /// A remote answering lift commits from a script, counting the connections it accepts.
    struct CommitRemote {
        url: String,
        connections: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CommitRemote {
        fn start(script: Vec<CommitAnswer>) -> CommitRemote {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            CommitRemote::serve(listener, script)
        }

        fn serve(listener: std::net::TcpListener, script: Vec<CommitAnswer>) -> CommitRemote {
            let url = format!("http://{}", listener.local_addr().unwrap());
            let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counter = Arc::clone(&connections);

            std::thread::spawn(move || {
                use std::io::Write;

                let mut held = Vec::new();

                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let index = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let answer = script.get(index).or(script.last()).cloned().unwrap();
                    let _ = read_test_request(&mut stream);

                    match answer {
                        CommitAnswer::Reply(head) => {
                            let _ = write!(stream, "{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", head);
                        }
                        CommitAnswer::CutOff(bytes) => {
                            let _ = stream.write_all(bytes.as_bytes());
                        }
                        CommitAnswer::Wedge => held.push(stream),
                        CommitAnswer::Drop => {}
                        CommitAnswer::Slow { head, head_after, body, body_after } => {
                            std::thread::sleep(head_after);
                            let _ = write!(stream, "{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", head, body.len());
                            let _ = stream.flush();
                            std::thread::sleep(body_after);
                            let _ = stream.write_all(body.as_bytes());
                        }
                    }
                }
            });

            CommitRemote { url, connections }
        }

        fn connections(&self) -> usize {
            self.connections.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    const NOT_READY: &str = "HTTP/1.1 503 Service Unavailable\r\nForklift-Commit: not-ready\r\nRetry-After: 1";
    const COMMITTED: &str = "HTTP/1.1 200 OK";
    const BARE_500: &str = "HTTP/1.1 500 Internal Server Error";

    /// [`FAST`] with a commit deadline of `deadline` milliseconds.
    fn commit_budgets(deadline: u64) -> Budgets {
        Budgets { commit_deadline: std::time::Duration::from_millis(deadline), ..FAST }
    }

    /// Commit one batch against `url` with `budgets`, under a ceiling that turns a lost bound into
    /// a failure rather than a hang. Returns the outcome and how long it took.
    fn commit_batch(url: &str, budgets: Budgets) -> (Result<(), String>, std::time::Duration) {
        let client = RemoteClient::new_test_with_budgets(url, budgets);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let ceiling = (budgets.connect + budgets.commit_deadline) * 3;
        let started = std::time::Instant::now();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(ceiling, commit_one_batch(&client, "session", &[], &[], false)).await
        });

        (outcome.unwrap_or_else(|_| panic!("the commit outlived {:?}", ceiling)), started.elapsed())
    }

    /// The not-ready signal is the header: a not-ready answer whose body is cut off mid-read is
    /// still retried, and the batch commits when a later attempt answers `200`.
    #[test]
    fn a_not_ready_answer_with_a_cut_off_body_is_retried_until_it_commits() {
        let remote = CommitRemote::start(vec![
            CommitAnswer::CutOff("HTTP/1.1 503 Service Unavailable\r\nForklift-Commit: not-ready\r\n\
                Content-Length: 400\r\n\r\n{\"error\":\"Blob"),
            CommitAnswer::Reply(COMMITTED),
        ]);

        let (outcome, _) = commit_batch(&remote.url, commit_budgets(5_230));

        assert_eq!(outcome, Ok(()));
        assert_eq!(remote.connections(), 2, "one not-ready attempt, then the commit");
    }

    /// A `422` without the header is terminal on its first answer, and a framed refusal code on it
    /// survives as that code.
    #[test]
    fn a_422_without_the_header_is_terminal_at_once() {
        let body = r#"{"error":"Blob x is gone.","code":"staged_upload_discarded","next_step":"Run it again."}"#;
        let url = serve_one_answer(
            format!("HTTP/1.1 422 Unprocessable Entity\r\nContent-Type: application/json\r\nContent-Length: {}", body.len()),
            body.to_string(),
        );

        let (outcome, elapsed) = commit_batch(&url, commit_budgets(5_230));

        let message = outcome.expect_err("a 422 is refused");
        assert!(elapsed < std::time::Duration::from_secs(1), "a terminal answer is not retried: {:?}", elapsed);
        match CoreError::from(message.clone()) {
            CoreError::Refusal { code, next_step, .. } => {
                assert_eq!(code, RefusalCode::StagedUploadDiscarded);
                assert_eq!(next_step, "Run it again.");
            }
            CoreError::Other(_) => panic!("the framed code was lost: {}", message),
        }
    }

    /// Against a remote that wedges every attempt, the batch fails within its deadline: an attempt
    /// that would outlive it is cut to what remains, so the attempts' own allowances never add up.
    #[test]
    fn a_remote_that_wedges_every_attempt_fails_within_the_deadline() {
        let remote = CommitRemote::start(vec![CommitAnswer::Wedge]);
        let budgets = Budgets {
            commit_deadline: std::time::Duration::from_millis(3_170),
            commit_attempt_allowance: std::time::Duration::from_millis(1_870),
            ..FAST
        };
        let window = budgets.connect + budgets.commit_deadline;
        // Two whole attempts and the backoff between them: what an attempt not cut to the
        // remaining time would take.
        let uncut = (budgets.connect + budgets.commit_attempt_allowance) * 2 + RETRY_BACKOFF_START;

        let (outcome, elapsed) = commit_batch(&remote.url, budgets);

        let message = outcome.expect_err("a wedged remote never commits");
        assert!(message.starts_with("Timed out while committing the lift session: no attempt got an answer"), "{}", message);
        assert!(message.contains("no answer arrived within"), "{}", message);
        assert!(is_transient_failure(&message), "{}", message);
        assert!(remote.connections() >= 2, "the time left after the first attempt is used");
        assert!(elapsed >= window - budgets.connect - budgets.min_commit_attempt,
            "the deadline was not used ({:?})", elapsed);
        assert!(elapsed < (window + uncut) / 2,
            "elapsed {:?} is past the midpoint between the deadline {:?} and uncut attempts {:?}",
            elapsed, window, uncut);
    }

    /// A `303` is refused as a redirect and not followed: the remote sees one request.
    #[test]
    fn a_redirected_commit_is_refused_and_not_followed() {
        let remote = CommitRemote::start(vec![
            CommitAnswer::Reply("HTTP/1.1 303 See Other\r\nLocation: /landing"),
            CommitAnswer::Reply(COMMITTED),
        ]);

        let (outcome, _) = commit_batch(&remote.url, commit_budgets(5_230));

        let message = outcome.expect_err("a redirect is refused");
        assert!(message.contains("The remote redirected committing the lift session (303) to /landing"), "{}", message);
        assert_eq!(remote.connections(), 1, "the redirect was followed");
    }

    /// A `429` waits out its `Retry-After`, then commits.
    #[test]
    fn a_throttled_commit_waits_out_its_retry_after() {
        let remote = CommitRemote::start(vec![
            CommitAnswer::Reply("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1"),
            CommitAnswer::Reply(COMMITTED),
        ]);

        let (outcome, elapsed) = commit_batch(&remote.url, commit_budgets(5_230));

        assert_eq!(outcome, Ok(()));
        assert!(elapsed >= std::time::Duration::from_secs(1), "the Retry-After was not waited ({:?})", elapsed);
    }

    /// A `Retry-After` longer than the deadline leaves is not waited: the batch fails at once,
    /// headed by the throttling.
    #[test]
    fn a_retry_after_past_the_deadline_fails_at_once() {
        let remote = CommitRemote::start(vec![
            CommitAnswer::Reply("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 30"),
        ]);

        let (outcome, elapsed) = commit_batch(&remote.url, commit_budgets(5_230));

        let message = outcome.expect_err("the wait does not fit");
        assert!(message.contains("last answer was 429 (rate limited), asking for a wait of 30s"), "{}", message);
        assert!(elapsed < std::time::Duration::from_secs(1), "the wait was taken ({:?})", elapsed);
        assert_eq!(remote.connections(), 1);
    }

    /// A `429` without a readable `Retry-After` falls back to the backoff.
    #[test]
    fn a_throttled_commit_without_retry_after_backs_off() {
        let remote = CommitRemote::start(vec![
            CommitAnswer::Reply("HTTP/1.1 429 Too Many Requests\r\nRetry-After: Wed, 21 Oct 2015 07:28:00 GMT"),
            CommitAnswer::Reply(COMMITTED),
        ]);

        let (outcome, elapsed) = commit_batch(&remote.url, commit_budgets(5_230));

        assert_eq!(outcome, Ok(()));
        assert!(elapsed >= RETRY_BACKOFF_START, "no backoff was taken ({:?})", elapsed);
        assert!(elapsed < std::time::Duration::from_secs(1), "a date was read as a wait ({:?})", elapsed);
    }

    /// A refused connect sent nothing and is retried: the batch commits once the remote listens.
    #[test]
    fn a_refused_connect_is_retried_until_the_remote_listens() {
        let address = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap()
        };
        let url = format!("http://{}", address);
        // Listening only after the first attempt was refused, and before the backoff ends. The
        // bind is retried briefly rather than unwrapped, so a slow release of the port cannot
        // fail the test for a reason unrelated to the commit.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            std::thread::sleep(RETRY_BACKOFF_START / 2);
            let started = std::time::Instant::now();
            let listener = loop {
                match std::net::TcpListener::bind(address) {
                    Ok(listener) => break listener,
                    Err(e) if started.elapsed() < std::time::Duration::from_secs(1) => {
                        let _ = e;
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(e) => panic!("could not listen on {} again: {}", address, e),
                }
            };
            let _ = tx.send(CommitRemote::serve(listener, vec![CommitAnswer::Reply(COMMITTED)]));
        });

        let (outcome, _) = commit_batch(&url, commit_budgets(5_230));
        let remote = rx.recv().expect("the remote listened again");

        assert_eq!(outcome, Ok(()));
        assert_eq!(remote.connections(), 1, "the refused attempt never reached the listener");
    }

    /// Unanswered attempts word what is known: a connect that never succeeded sent nothing, and a
    /// connection dropped after the request was sent says no such thing.
    #[test]
    fn unanswered_attempts_say_whether_anything_was_sent() {
        let address = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap()
        };
        let (refused, _) = commit_batch(&format!("http://{}", address), commit_budgets(900));
        let refused = refused.expect_err("nothing listens");
        assert!(refused.contains("could not connect to the remote") && refused.contains("nothing was sent"), "{}", refused);

        let remote = CommitRemote::start(vec![CommitAnswer::Drop]);
        let (dropped, _) = commit_batch(&remote.url, commit_budgets(900));
        let dropped = dropped.expect_err("the remote never answers");
        assert!(dropped.contains("failed before an answer arrived"), "{}", dropped);
        assert!(!dropped.contains("nothing was sent"), "{}", dropped);
        assert!(remote.connections() >= 2, "a dropped connection is retried");
    }

    /// No attempt starts unless more than connect + `min_commit_attempt` remains.
    #[test]
    fn no_attempt_starts_without_room_to_outlive_its_connect_phase() {
        let remote = CommitRemote::start(vec![CommitAnswer::Reply(COMMITTED)]);
        let budgets = Budgets { commit_deadline: FAST.min_commit_attempt, ..FAST };

        let (outcome, _) = commit_batch(&remote.url, budgets);

        let message = outcome.expect_err("no attempt fits");
        assert!(message.contains("no attempt could start"), "{}", message);
        assert_eq!(remote.connections(), 0);
    }

    /// The headline is the last answered attempt's, whatever the unanswered ones after it, and
    /// never claims the upload is safe.
    #[test]
    fn the_deadline_headline_is_the_last_answer() {
        let headline = |script: Vec<CommitAnswer>| {
            let remote = CommitRemote::start(script);
            commit_batch(&remote.url, commit_budgets(1_870)).0.expect_err("never commits")
        };

        let cut_short = headline(vec![CommitAnswer::Reply(NOT_READY), CommitAnswer::Wedge]);
        assert!(cut_short.contains("still holds a staged upload"), "{}", cut_short);
        assert!(cut_short.contains("got no answer"), "the unanswered attempts are counted: {}", cut_short);

        let then_not_ready = headline(vec![CommitAnswer::Reply(BARE_500), CommitAnswer::Reply(NOT_READY)]);
        assert!(then_not_ready.contains("still holds a staged upload"), "{}", then_not_ready);

        let then_500 = headline(vec![CommitAnswer::Reply(NOT_READY), CommitAnswer::Reply(BARE_500)]);
        assert!(then_500.contains("last answer was 500"), "{}", then_500);

        for message in [&cut_short, &then_not_ready, &then_500] {
            assert!(message.contains("The remote's pallet was not moved"), "{}", message);
            assert!(!message.contains("upload is safe") && !message.contains("uploads are safe"), "{}", message);
            assert!(is_transient_failure(message), "{}", message);
        }
    }

    /// A `408` is an intermediary giving up before the remote answered: retried, not terminal.
    #[test]
    fn a_408_is_retried() {
        let remote = CommitRemote::start(vec![
            CommitAnswer::Reply("HTTP/1.1 408 Request Timeout"),
            CommitAnswer::Reply(COMMITTED),
        ]);

        let (outcome, _) = commit_batch(&remote.url, commit_budgets(5_230));

        assert_eq!(outcome, Ok(()));
        assert_eq!(remote.connections(), 2);
    }

    /// A `Retry-After: 0` waits the backoff anyway, so a remote that keeps asking for no wait does
    /// not get the commit resent in a tight loop.
    #[test]
    fn a_zero_retry_after_still_backs_off() {
        let remote = CommitRemote::start(vec![CommitAnswer::Reply("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0")]);

        let (outcome, _) = commit_batch(&remote.url, commit_budgets(1_870));

        let message = outcome.expect_err("never commits");
        assert!(message.contains("last answer was 429 (rate limited). "), "a zero wait is not quoted: {}", message);
        // About 1.8 s of room under a backoff of 200 ms, 400 ms, 800 ms: four attempts at most.
        assert!(remote.connections() <= 5, "{} attempts: the backoff was skipped", remote.connections());
    }

    /// An answer the client cannot parse as HTTP repeats on a retry, so it ends the commit at
    /// once instead of at the deadline.
    #[test]
    fn a_garbled_answer_ends_the_commit_at_once() {
        let remote = CommitRemote::start(vec![CommitAnswer::CutOff("this is not HTTP at all\r\n\r\n")]);

        let (outcome, elapsed) = commit_batch(&remote.url, commit_budgets(5_230));

        let message = outcome.expect_err("garbage never commits");
        assert!(message.starts_with("Error while committing the lift session:"), "{}", message);
        assert!(!is_transient_failure(&message), "{}", message);
        assert_eq!(remote.connections(), 1);
        assert!(elapsed < std::time::Duration::from_secs(1), "{:?}", elapsed);
    }

    /// A connect that fails before sending anything is retried whatever its cause, here a name
    /// that does not resolve: the lift talked to the remote moments ago, so such a failure at
    /// commit time is far likelier passing than permanent.
    #[test]
    fn a_connect_that_fails_for_any_reason_is_retried() {
        let (outcome, _) = commit_batch("http://forklift-commit-test.invalid", commit_budgets(900));

        let message = outcome.expect_err("the name never resolves");
        assert!(message.contains("could not connect to the remote") && message.contains("nothing was sent"), "{}", message);
        assert!(is_transient_failure(&message), "{}", message);
    }

    /// A TLS handshake the client rejects repeats on a retry, so it ends the commit at once.
    #[test]
    fn a_rejected_tls_handshake_ends_the_commit_at_once() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = connections.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // Read the ClientHello before answering and drain until the client hangs up: a
                // socket closed with unread data is reset, and on Windows the reset can overtake
                // the answer, so the client would see an aborted connection, not a bad handshake.
                let mut buffer = [0u8; 4096];
                let _ = stream.read(&mut buffer);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                while matches!(stream.read(&mut buffer), Ok(n) if n > 0) {}
            }
        });

        let (outcome, elapsed) = commit_batch(&format!("https://{}", address), commit_budgets(5_230));

        let message = outcome.expect_err("plain HTTP is no TLS handshake");
        assert!(message.starts_with("Error while committing the lift session:"), "{}", message);
        assert!(!is_transient_failure(&message), "{}", message);
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(elapsed < std::time::Duration::from_secs(1), "{:?}", elapsed);
    }

    /// A refusal whose status line arrives late in an attempt still has its body read whole, under
    /// the error-body budget rather than what was left of the attempt, so its code survives.
    #[test]
    fn a_refusal_arriving_late_in_an_attempt_keeps_its_code() {
        const BODY: &str = r#"{"error":"Blob x is gone.","code":"staged_upload_discarded","next_step":"Run it again."}"#;
        let remote = CommitRemote::start(vec![CommitAnswer::Slow {
            head: "HTTP/1.1 422 Unprocessable Entity\r\nContent-Type: application/json",
            head_after: std::time::Duration::from_millis(600),
            body: BODY,
            body_after: std::time::Duration::from_millis(600),
        }]);
        // One attempt of connect + 570 ms = 900 ms: the status line lands inside it, and the body
        // only after it, though well inside the error-body budget counted from the status line.
        let budgets = commit_budgets(570);
        assert!(budgets.connect + budgets.commit_deadline < std::time::Duration::from_millis(1_200));
        assert!(error_body_read_budget(&budgets) > std::time::Duration::from_millis(900));

        let (outcome, _) = commit_batch(&remote.url, budgets);

        let message = outcome.expect_err("a refusal");
        match CoreError::from(message.clone()) {
            CoreError::Refusal { code, .. } => assert_eq!(code, RefusalCode::StagedUploadDiscarded),
            CoreError::Other(_) => panic!("the code was cut off with the body: {}", message),
        }
    }

    /// A refusal whose status line lands just inside a retryable call's head-wait, and whose body
    /// lands only after that bound, keeps its code at every call on the retryable floor: the
    /// head-wait bounds the status line alone, and the body is read under the error-body budget
    /// counted from it. A total deadline of the same figure, which these calls once rode, cuts the
    /// body off and leaves the bare status reason.
    #[test]
    fn a_late_refusal_keeps_its_code_at_every_retryable_call() {
        const BODY: &str = r#"{"error":"Blob x is gone.","code":"staged_upload_discarded","next_step":"Run it again."}"#;
        let answer = CommitAnswer::Slow {
            head: "HTTP/1.1 422 Unprocessable Entity\r\nContent-Type: application/json",
            head_after: std::time::Duration::from_millis(600),
            body: BODY,
            body_after: std::time::Duration::from_millis(600),
        };
        // A head-wait of connect + 570 ms = 900 ms at every call: the status line lands inside
        // it, and the body only after it, though well inside the error-body budget counted from
        // the status line.
        let budgets = Budgets { retryable_head_patience: std::time::Duration::from_millis(570), ..FAST }.checked();
        assert!(error_body_read_budget(&budgets) > std::time::Duration::from_millis(900));
        let anchor = TrustAnchorDto {
            genesis: "g".repeat(64),
            enabled_at: 0,
            boundary: Vec::new(),
            prior_genesis: None,
            adopts: None,
        };
        let hashes = vec!["a".repeat(64)];

        for call in ["missing_objects", "upload_targets", "upload_signature", "put_trust"] {
            let remote = CommitRemote::start(vec![answer.clone()]);
            let client = RemoteClient::new_test_with_budgets(&remote.url, budgets);
            assert_eq!(client.presence_negotiation_budget(1), std::time::Duration::from_millis(900));
            assert_eq!(client.retryable_head_budget(), std::time::Duration::from_millis(900));
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let outcome = runtime.block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(15), async {
                    match call {
                        "missing_objects" => client.missing_objects(&hashes).await.map(|_| ()),
                        "upload_targets" => client.upload_targets("session", &hashes).await.map(|_| ()),
                        "upload_signature" => client.upload_signature(&"c".repeat(64), vec![1u8; 64]).await,
                        _ => client.put_trust(&anchor).await,
                    }
                }).await
            });

            let message = outcome
                .unwrap_or_else(|_| panic!("{} hung on a refusal whose body arrives", call))
                .expect_err("a refusal");
            match CoreError::from(message.clone()) {
                CoreError::Refusal { code, .. } => assert_eq!(code, RefusalCode::StagedUploadDiscarded, "{}", call),
                CoreError::Other(_) => panic!("{}: the code was cut off with the body: {}", call, message),
            }
        }
    }

    /// An answer lost after its status line began is retried, and the retry's `200` commits.
    #[test]
    fn a_lost_200_is_retried_and_commits() {
        let remote = CommitRemote::start(vec![
            CommitAnswer::CutOff("HTTP/1.1 200 OK\r\n"),
            CommitAnswer::Reply(COMMITTED),
        ]);

        let (outcome, _) = commit_batch(&remote.url, commit_budgets(5_230));

        assert_eq!(outcome, Ok(()));
        assert_eq!(remote.connections(), 2);
    }

    /// The handshake's read in `fetch_info` gets the same treatment as the `missing_objects` and
    /// `upload_targets` parse-failure tests. It was the *least* covered of those three reads:
    /// routing its parse failures through the composer left the entire crate suite green, because
    /// every other `fetch_info` test drives either a refused connect or header-phase silence,
    /// neither of which ever reaches the body read's error arm.
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
        const SENTINEL_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(440);

        let remote = PartialJsonThenSilentRemote::start();
        let client = RemoteClient::new_test_with_budgets(
            &remote.url, Budgets { connect: SENTINEL_CONNECT_TIMEOUT, ..FAST },
        );
        let hashes = vec!["a".repeat(64)];
        let silence = SENTINEL_CONNECT_TIMEOUT + FAST.object_read;
        let body_budget =
            client.presence_body_budget(body_utils::per_item_body_cap(1, UPLOAD_TARGETS_BODY_BYTES_PER_HASH));
        assert!(body_budget > silence, "the silence budget fires before the body's total");
        let hard_ceiling = body_budget + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(
                hard_ceiling, client.upload_targets("session-composer-posture-json-test", &hashes)
            ).await
        });

        let inner = outcome.unwrap_or_else(|_| panic!(
            "upload_targets hung past the test's own {:?} ceiling — the upload-target answer's \
            body read is unbounded", hard_ceiling
        ));
        let message = match inner {
            Err(message) => message,
            Ok(_) => panic!("a body that never completes must not appear to succeed"),
        };

        assert!(
            message.contains(&format!("no further bytes for at least {:?}", silence)),
            "a partial JSON body followed by silence must compose the silence message naming the \
            armed {:?}, not fall through to the unconditional \"not valid JSON\" message: {}",
            silence, message
        );
        assert!(
            !message.to_lowercase().contains("not valid json"),
            "must not be the JSON-parse-failure message — the remote never finished sending, it \
            did not send garbage: {}", message
        );
    }

    /// The presence answers' body bound against a remote that never goes silent: headers after
    /// `HEAD_DELAY`, inside the head-wait, then one byte every 200ms, never long enough for the
    /// client's silence budget to fire. Only the body's own total ends it, at
    /// `presence_body_budget`'s figure for that call's cap; without it both calls wait out the
    /// whole 20s trickle and the ceiling trips. The lower bound pins that the total is counted
    /// from the status line: counted from the request, it would fire `HEAD_DELAY` early.
    #[test]
    fn presence_answers_give_up_on_a_remote_that_never_stops_trickling() {
        const HEAD_DELAY: std::time::Duration = std::time::Duration::from_millis(1_200);

        let hashes = vec!["a".repeat(64)];
        assert!(HEAD_DELAY < FAST.connect + FAST.retryable_head_patience, "the headers land inside the head-wait");
        for (call, per_hash) in [
            ("missing_objects", MISSING_BODY_BYTES_PER_HASH),
            ("upload_targets", UPLOAD_TARGETS_BODY_BYTES_PER_HASH),
        ] {
            let url = start_trickling_remote_after(HEAD_DELAY, 100, std::time::Duration::from_millis(200));
            let client = RemoteClient::new_test_with_budgets(&url, FAST);
            let budget = client.presence_body_budget(body_utils::per_item_body_cap(hashes.len(), per_hash));
            let hard_ceiling = HEAD_DELAY + budget + std::time::Duration::from_secs(8);
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let started = std::time::Instant::now();
            let outcome = runtime.block_on(async {
                tokio::time::timeout(hard_ceiling, async {
                    match call {
                        "missing_objects" => client.missing_objects(&hashes).await.map(|_| ()),
                        _ => client.upload_targets("session", &hashes).await.map(|_| ()),
                    }
                }).await
            });

            let message = outcome
                .unwrap_or_else(|_| panic!("{}: a trickled answer held the call past {:?}", call, hard_ceiling))
                .expect_err("an answer that never finishes");
            let elapsed = started.elapsed();
            assert!(message.contains(&format!("did not finish its answer within {:?}", budget)), "{}: {}", call, message);
            assert!(
                elapsed >= HEAD_DELAY + budget,
                "{}: gave up after {:?}, before the headers' {:?} plus the body's {:?} — the total \
                is not counted from the status line", call, elapsed, HEAD_DELAY, budget
            );
        }
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
    /// comfortably under the client's object-read silence budget, the total transfer comfortably
    /// past it — must **succeed**,
    /// never be treated as a stall. A total-deadline bound (rejected in favor of
    /// `read_timeout`-on-its-own-client, see `REMOTE_READ_TIMEOUT`'s doc) fails this test: it
    /// would kill the transfer partway through even though every single gap was healthy.
    #[test]
    fn fetch_object_survives_a_slow_but_steadily_progressing_body() {
        let gap = std::time::Duration::from_millis(1_500);
        let chunks: Vec<Vec<u8>> = (0..4).map(|i| format!("chunk-{}-of-the-drip", i).into_bytes()).collect();
        let expected: Vec<u8> = chunks.concat();
        // Four gaps run past the effective loose budget (connect + object read) in total, while no
        // single gap comes near it.
        let total_duration = gap * (chunks.len() as u32);
        let effective_loose_budget = FAST.connect + FAST.object_read;
        assert!(
            total_duration > effective_loose_budget,
            "the fixture must actually outlast the bound under test"
        );

        let url = start_steady_drip_remote(chunks, gap);
        let client = fast_client(&url);
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
        let past_read_timeout = FAST.connect + FAST.read
            + std::time::Duration::from_secs(1);
        let url = start_slow_answering_remote(past_read_timeout, "");
        let client = fast_client(&url);
        let outer_ceiling = past_read_timeout + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(
                outer_ceiling,
                client.update_ref("main", None, &"a".repeat(64), None),
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
    /// fixture answers after `connect + read + 1s`, composed from the *tight* budget —
    /// inside the `connect + object_read` a loose-scaled budget would allow, so the
    /// wrongly-bounded client still answers in time and that test stays green. This one is sized
    /// against the **loose** budget precisely to close that gap.
    ///
    /// The "unbounded direction" shape (see `assert_still_running`): a silent remote must not be
    /// enough on its own to fail this call, checked past the loose budget with slack. It also
    /// stands as the standing evidence that the head-wait client bounds only its own consumers
    /// (`fetch_batch`'s `POST`, `fetch_subtree`, `commit_lift`) and not `update_ref`, the one
    /// no-redirect call that stays unbounded.
    #[test]
    fn update_ref_stays_unbounded_against_the_loose_silence_budget() {
        let remote = SilentRemote::start();
        let client = fast_client(&remote.url);
        let check_after = FAST.connect + FAST.object_read
            + std::time::Duration::from_secs(1);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(assert_still_running(
            "update_ref", check_after, client.update_ref("main", None, &"a".repeat(64), None),
        ));
    }

    /// The discriminating fixture for `resolve`'s own [`Posture::TotalDeadline`]: a remote that
    /// connects instantly (loopback) and then stays silent for a fixed delay before answering
    /// correctly — it *does* eventually answer, which is exactly why this test alone cannot pin
    /// termination (see the note on that below). The test splits its two halves. The *figure* is
    /// checked without waiting: a production client's [`RemoteClient::resolve_budget`] must be
    /// `TEST_DIRECT_CONNECT_TIMEOUT + TEST_TIGHT_READ_TIMEOUT` and must sit more than a second past
    /// the *old* flat bound `resolve` used to carry (a plain `RequestBuilder::timeout(5s)`, not
    /// mirrored as a `TEST_*` constant since the production value it mirrored no longer exists to
    /// drift against). The *behaviour* is checked on a client whose read budget puts its total
    /// past that old bound: a remote that answers after the old bound and well inside the budget
    /// must yield the real mapping. That costs a wait just past 5s, the one figure this test
    /// cannot shrink, because a call site reverted to the old flat bound must fire before the
    /// answer arrives. Before the fix, a healthy remote answering past 5s made `send` return
    /// `Err` and `resolve` fall back to an empty map — pseudonyms. Asserts the returned mapping
    /// itself, not merely that the call returned promptly — a timing-only assertion is green in
    /// both worlds, so it would pin nothing.
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
        let production_budget = TEST_DIRECT_CONNECT_TIMEOUT + TEST_TIGHT_READ_TIMEOUT;
        assert_eq!(
            RemoteClient::new_with_tor("http://127.0.0.1:1", None, TorSettings::default())
                .unwrap()
                .resolve_budget(),
            production_budget,
            "resolve's production budget must be the connect bound plus the read budget"
        );
        assert!(
            production_budget > old_flat_bound + std::time::Duration::from_secs(1),
            "resolve's production budget {:?} must stay past the old flat bound {:?}, or a remote \
            that answered in between would fall back to pseudonyms again",
            production_budget, old_flat_bound
        );

        // The behaviour itself: an answer that arrives past the old bound and well inside this
        // client's budget is returned, not dropped for pseudonyms. The read budget is one no
        // production constructor emits, sized so the budget clears the delay by more than a
        // second; the connect bound stays FAST's, under the old bound, so a call site reverted
        // to that bound still arms a legal deadline and fails on the behaviour, not on the
        // payload check.
        let budgets = Budgets { read: std::time::Duration::from_millis(6_670), ..FAST };
        let new_budget = budgets.connect + budgets.read;
        let delay = old_flat_bound + std::time::Duration::from_millis(500);
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
        let client = RemoteClient::new_test_with_budgets(&url, budgets);
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
            inside the connect+read budget — must return the real mapping, not fall back to \
            pseudonyms: got {:?}", delay, names
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
    /// budget. Built through `with_budgets_for`/`TorSettings` (`TorMode::On`, pointed at
    /// a fixture-controlled fake SOCKS proxy, with [`FAST`]'s budgets) — no live Tor daemon, no
    /// config file needed. With a flat read budget as the `read_timeout`, fired regardless of the
    /// connect bound (`read_timeout` is armed and checked before the connector is even polled),
    /// this call would fail at the read budget even though the Tor connect bound allows far
    /// longer; with the configured
    /// `read_timeout` accommodating whichever connect budget applies, the call must still be
    /// running well past both that flat tight budget and the direct connect budget.
    #[test]
    fn tor_routed_read_budget_does_not_preempt_the_tor_connect_budget() {
        let proxy = ParkingSocksProxy::start();
        let tor = TorSettings { mode: TorMode::On, proxy: format!("socks5h://{}", proxy.addr) };
        let client = RemoteClient::with_budgets_for(
            "http://forklift-fork-49-tor-test.invalid", None, &tor, fast_budgets,
        ).unwrap();

        // Comfortably past both the old flat tight-budget bug and the direct connect budget — if
        // either were still preempting the Tor connect allowance, the call would already have
        // failed well before this fires — and still inside the Tor connect bound itself.
        let check_after = FAST.connect + FAST.read + std::time::Duration::from_millis(1_500);
        assert!(check_after < FAST_TOR_CONNECT, "the check must land before the Tor connect bound fires");

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(assert_still_running("fetch_info (Tor-routed)", check_after, client.fetch_info()));
    }

    /// A connect that never completes must fail on this client's own connect bound, and the
    /// message must name that bound as a connect failure. [`ParkingSocksProxy`] never finishes the
    /// SOCKS handshake, so the call stalls inside the connector; a Tor-routed [`FAST`] client
    /// carries [`FAST_TOR_CONNECT`], a figure neither production connect bound shares, so an arm
    /// that printed a production constant, or a connector armed with one, fails here. The client
    /// carries [`WIDE_READ`], so the connect bound fires seconds before the read timeout that also
    /// spans the connect phase.
    #[test]
    fn a_stalled_connect_names_this_clients_connect_bound() {
        let proxy = ParkingSocksProxy::start();
        let tor = TorSettings { mode: TorMode::On, proxy: format!("socks5h://{}", proxy.addr) };
        let client = RemoteClient::with_budgets_for(
            "http://forklift-connect-message-test.invalid", None, &tor, fast_budgets_wide_read,
        ).unwrap();
        let hard_ceiling = FAST_TOR_CONNECT + std::time::Duration::from_secs(15);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(async {
            tokio::time::timeout(hard_ceiling, client.fetch_info()).await
        });
        let message = match outcome.unwrap_or_else(|_| panic!(
            "fetch_info hung past the test's own {:?} ceiling — the connect bound never fired",
            hard_ceiling
        )) {
            Err(message) => message,
            Ok(_) => panic!("a connect that never completes must not appear to succeed"),
        };

        assert!(
            message.contains(&format!("could not connect to the remote within {:?}", FAST_TOR_CONNECT)),
            "must fail as a connect timeout naming this client's connect bound {:?}: {}",
            FAST_TOR_CONNECT, message
        );
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
    /// governs the client — `bounded_read_timeout` computes what the client is actually configured
    /// with, and `describe_transport_error` must report that same figure, not the raw silence
    /// constant alone. The gap is widest, and nothing before this test checked it, on a Tor-routed
    /// remote: the effective bound is the Tor connect bound plus the read silence budget (70s in
    /// production), not the bare silence budget an earlier version of the message printed
    /// regardless of transport. Uses [`HandshakeCompletingSocksProxy`], not [`ParkingSocksProxy`]:
    /// this test needs the failure to land specifically in the read-silence branch, which requires
    /// connect to have genuinely succeeded first.
    #[test]
    fn fetch_info_message_names_the_effective_tor_bound() {
        let proxy = HandshakeCompletingSocksProxy::start();
        let tor = TorSettings { mode: TorMode::On, proxy: format!("socks5h://{}", proxy.addr) };
        let client = RemoteClient::with_budgets_for(
            "http://forklift-fork-49-tor-message-test.invalid", None, &tor, fast_budgets,
        ).unwrap();

        let effective_budget = FAST_TOR_CONNECT + FAST.read;
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
            "must name the effective Tor bound {:?} (Tor connect + read silence), not some other \
            figure — an operator on an onion remote needs the real number to decide whether to \
            retry or raise a bound: {}", effective_budget, message
        );
    }

    /// Pins the one link a live Tor-routed upload test would otherwise need ~72s of real wall
    /// time to prove (60s Tor connect + 10s silence + verify) — that `new_with_tor` with
    /// `TorMode::On` selects [`REMOTE_CONNECT_TIMEOUT_TOR`] (60s) as this instance's
    /// `connect_timeout`, not the direct 5s [`REMOTE_CONNECT_TIMEOUT`]. Everything downstream of
    /// that selection — that `send_with_watchdog`'s phase-2 budget folds `self.budgets.connect`
    /// back in, and that the composed message names whatever `connect_timeout` this instance
    /// carries — is already pinned without a live Tor circuit at all: see
    /// `upload_object_times_out_after_a_fully_received_body_that_never_responds`, which exercises
    /// the identical mechanism on a direct client. This test is the only remaining
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
            client.budgets.connect, TEST_TOR_CONNECT_TIMEOUT,
            "TorMode::On must select the 60s Tor connect budget, not the direct 5s one"
        );
    }

    // -----------------------------------------------------------------------------------
    // FORK-49 slice 2: the upload path. `upload_object` and `put_presigned` used to hang forever
    // against a remote that accepted the connection and then simply stopped reading — no error,
    // no exit. `read_timeout` (the read-path fix) cannot be reused here: it is a flat,
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
    /// (above) — dropped, never signaled mid-test, only once `_park` goes out of scope
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
        let client = fast_client(&remote.url);
        let hash = "a".repeat(64);
        let action = format!("uploading object {}", hash);
        let effective_budget = FAST.connect + FAST.upload_silence;
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
        let client = fast_client(&remote.url);
        let action = "uploading to a staging URL";
        let effective_budget = FAST.connect + FAST.upload_silence;
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
    // The post-send phase: a remote that reads the request headers *and*
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
    // The *first* fix was too tight: `post_send_verify_budget`
    // alone ignores connect latency and the in-flight tail hyper/the OS kernel can still be
    // flushing the instant `is_exhausted()` flips — see `send_with_watchdog`'s doc for the full
    // reasoning. The budget these tests now expect is `phase2_budget`
    // (`connect + UPLOAD_SILENCE_BUDGET + post_send_verify_budget(body_len)`), not
    // `post_send_verify_budget(body_len)` alone.
    //
    // `SilentRemote` (above) is exactly the fixture this needs: it already reads a
    // request in full via `read_test_request` (headers *and* body, draining to `Content-Length`)
    // and then parks without ever writing a response — a wedge that happens *after* a complete
    // receive, never during one. That's what makes it different from `WedgedUploadRemote`, which
    // stops reading at the header terminator and never touches the body: the two fixtures probe
    // the two phases this watchdog now separately bounds.
    // -----------------------------------------------------------------------------------

    /// `upload_object` against a remote that reads the whole request and then goes silent — never
    /// writing a response — must fail with a timeout scaled to the body size, not hang forever.
    /// The send-phase watchdog alone does not cover this gap,
    /// since the stream is genuinely exhausted (the whole body really was delivered) by the time
    /// this remote stops responding.
    #[test]
    fn upload_object_times_out_after_a_fully_received_body_that_never_responds() {
        let remote = SilentRemote::start();
        let client = fast_client(&remote.url);
        let hash = "e".repeat(64);
        let action = format!("uploading object {}", hash);
        // Deliberately small — the whole point is that this bound is tight for an ordinary-sized
        // object, not that it needs a huge body to exercise (that's the *send*-phase tests' job).
        let body = vec![9u8; 64 * 1024];
        let phase2_budget = FAST.connect + FAST.upload_silence
            + post_send_verify_budget(FAST.retryable_head_patience, body.len());
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
            overstate it (claiming the bytes are certainly \"on the wire\"): {}", message
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
        let client = fast_client(&remote.url);
        let action = "uploading to a staging URL";
        let body = vec![9u8; 64 * 1024];
        let phase2_budget = FAST.connect + FAST.upload_silence
            + post_send_verify_budget(FAST.retryable_head_patience, body.len());
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
        let gap = std::time::Duration::from_millis(400);
        let iterations = 10u32;
        let body_len = read_size * iterations as usize;
        let effective_budget = FAST.connect + FAST.upload_silence;
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
        let client = fast_client(&url);
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
    // `is_exhausted()` means every chunk was handed to *hyper*, not that
    // every byte reached the *peer*. `start_delayed_read_remote` below is the deterministic,
    // cross-platform way to exercise the gap between those two facts without depending on any
    // particular OS/hyper buffering threshold (which the FORK-49 spike already established is
    // non-deterministic even on one machine — see `WEDGED_UPLOAD_BODY_LEN`'s doc): a body small
    // enough to fit inside *any* platform's default TCP send/receive window (a couple KB, far
    // under even the smallest realistic default) gets fully accepted into the outgoing pipeline —
    // and so reported "exhausted" by the client's own stream — almost instantly, regardless of
    // whether the peer's *application* has read a single byte. The fixture's deliberate pause
    // before it ever calls `read()` stands in for exactly the invisible, still-genuinely-
    // happening work (an in-flight tail draining slowly) phase 2 must wait out — the client
    // cannot tell it apart from having actually reached the remote already, and must not be too
    // impatient about it.
    // -----------------------------------------------------------------------------------

    /// Starts a remote that reads only the request headers immediately, then pauses for `delay`
    /// *before touching the body at all*, then drains the body fully and answers `200 OK`. See
    /// this section's own comment for why a small body plus this pause is a deterministic stand-in
    /// for the real failure (a slow-draining in-flight tail the client cannot observe).
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
    /// deterministic stand-in for a slow-draining in-flight tail (see this section's own
    /// comment). A verify-only phase-2 budget (`post_send_verify_budget` alone — the first, too
    /// tight, fix) is far shorter than `delay` here and would fail this test; the real
    /// `phase2_budget` (which folds the full send-phase allowance back in as flush margin, see
    /// `send_with_watchdog`'s doc) comfortably covers it. This is exactly the test a fix that
    /// only re-tightened `post_send_verify_budget`'s own constants — without folding the
    /// send-phase allowance back in — would still fail.
    #[test]
    fn upload_object_survives_a_remote_that_pauses_before_reading_a_small_body() {
        let body_len = 2 * 1024;
        let verify_only_budget = post_send_verify_budget(FAST.retryable_head_patience, body_len);
        let phase2_budget = FAST.connect + FAST.upload_silence + verify_only_budget;
        // Comfortably past the (too-tight) verify-only budget, comfortably short of the real
        // phase-2 budget — the gap this test exists to probe.
        let delay = verify_only_budget + (FAST.connect + FAST.upload_silence) / 2;
        assert!(
            delay > verify_only_budget,
            "delay must exceed the too-tight verify-only budget, or this doesn't prove the \
            flush-time margin is needed at all"
        );
        assert!(
            delay < phase2_budget,
            "delay must stay under the real phase-2 budget, or this is a timeout test in \
            disguise rather than a \"must survive\" one"
        );

        let url = start_delayed_read_remote(delay);
        let client = fast_client(&url);
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
    // `wrap_stream`'s body cannot be cloned to replay a redirect (verified
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
    /// (307)" that gives no hint a redirect was even involved. This is the live case: an S3
    /// bucket reached via the wrong regional endpoint answers `PUT` with
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
    /// target — this module's own control plane — is not the S3 live case above; a lighter
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
        let error = runtime.block_on(client.update_ref("main", None, &"a".repeat(64), None))
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
        let error = runtime.block_on(client.update_ref("main", None, &"a".repeat(64), None))
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
    /// its own client selection (`send_on(Posture::HeadDeadlineNoRedirect { head: self
    /// .retryable_head_budget() }, ...)`, the never-auto-follow client).
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
    /// path's own, non-mutation wording) — only that retrying converges. Must also *not* overclaim:
    /// `silent_for()` is measured from before `send()` is even called, so
    /// this can fire having pulled zero chunks — "the request was sent" would assert more than is
    /// actually known.
    #[test]
    fn mutation_read_timeout_message_carries_the_uncertainty_wording() {
        let message = RemoteClient::mutation_read_timeout_message("uploading object aaaa");

        assert!(
            message.contains("may or may not have fully reached the remote"),
            "must carry the mutation-uncertainty wording, spanning the whole zero-to-full range, \
            not claim nothing happened: {}", message
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
            "must not overclaim full delivery when at most partial delivery is known: {}",
            message
        );
    }

    /// Pins the exact wording [`RemoteClient::mutation_post_send_timeout_message`] produces — the
    /// message the upload watchdog manufactures on a *post-send* stall.
    /// Distinct from the mid-body composer above: here every chunk really was handed to hyper, so
    /// the message can (and must) say the remote may already have received, verified, and stored
    /// the bytes — but it must still say only what's *locally observable* ("finished streaming …
    /// the request body" — handed to hyper) rather than assert delivery ("the bytes are on the
    /// wire" — `is_exhausted()` does not mean that), and must name the actual budget
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
            wedged, which is exactly what `is_exhausted()` cannot rule out: {}", message
        );
        assert!(
            message.contains(&format!("{:?}", budget)),
            "must name the actual budget that governed, not a generic phrase: {}", message
        );
    }

    /// The scoped fetch's ledger record and `scope fetch`'s engine, in a sparse warehouse whose objects
    /// are all local and signed, so the walks never need the remote; the client points where
    /// nothing listens, so a walk that does reach for it fails.
    mod scoped_fetch {
        use super::*;
        use crate::enums::dir_entry_type::DirEntryType;
        use crate::model::blob::Blob;
        use crate::model::tree_item::TreeItem;
        use crate::util::fetch_ledger::{self, LedgerEntry};
        use crate::util::pallet_utils::RefUpdateError;

        fn unreachable() -> RemoteClient {
            RemoteClient::new("http://127.0.0.1:1", None).unwrap()
        }

        fn runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
        }

        fn blob(content: &str) -> String {
            let mut object = LooseObjectBuilder::build_blob(&Blob { content: content.as_bytes().to_vec() });
            object.store().unwrap();
            object.hash
        }

        fn tree(entries: &[(&str, &str, DirEntryType)]) -> String {
            let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
            for (name, hash, item_type) in entries {
                tree.add_child(TreeItem::new(name.to_string(), hash.to_string(), *item_type));
            }
            let mut object = LooseObjectBuilder::build_tree(&tree);
            object.store().unwrap();
            object.hash
        }

        /// A tree with `src/a` and `web/b`, each holding `content`. With the scope `src`, `web` is
        /// out of scope.
        fn content_tree(content: &str) -> String {
            let src = tree(&[("a", &blob(&format!("a {}", content)), DirEntryType::Normal)]);
            let web = tree(&[("b", &blob(&format!("b {}", content)), DirEntryType::Normal)]);
            tree(&[("src", &src, DirEntryType::Tree), ("web", &web, DirEntryType::Tree)])
        }

        /// A signed parcel over `content_tree(content)`: the signature sidecar is what keeps the
        /// walk from asking the remote for one.
        fn parcel(content: &str, parents: Vec<String>) -> String {
            let parcel = crate::model::parcel::Parcel {
                tree_hash: content_tree(content),
                parents,
                actions: Vec::new(),
                description: Some(content.to_string()),
            };
            let mut object = LooseObjectBuilder::build_parcel(&parcel);
            object.store().unwrap();
            sign_utils::store_parcel_signature(&object.hash, &sign_utils::ParcelSignature {
                key_id: "k".repeat(64),
                signature: vec![0; 64],
            }).unwrap();
            object.hash
        }

        /// A sparse warehouse scoped to `src` whose `main` holds one parcel. Returns that parcel.
        fn sparse_main() -> String {
            crate::util::warehouse_utils::mark_for_test(crate::util::warehouse_utils::WarehouseMode::Sparse);
            scope_utils::set_fetch_scope(&PathScope::from_prefixes(["src"])).unwrap();
            let head = parcel("main", Vec::new());
            pallet_utils::update_pallet_head("main", None, &head, &RefPreconditions::NONE).unwrap();
            head
        }

        fn record(head: &str, pallet: &str) {
            let lock = RefLock::acquire().unwrap();
            fetch_ledger::record_fetch(&lock, head, pallet).unwrap();
        }

        fn ledger_heads() -> Vec<String> {
            fetch_ledger::read_ledger().unwrap().into_iter().map(|entry| entry.head).collect()
        }

        fn expand(base: &str, added: &[&str]) -> Expansion {
            Expansion::new("main", base, scope_utils::read_fetch_scope().unwrap(),
                           added.iter().map(|prefix| prefix.to_string()).collect())
        }

        // T11. Revert: record the head only after the transfer succeeds → the ledger stays empty.
        #[test]
        fn a_scoped_fetch_that_fails_leaves_its_head_in_the_ledger() {
            let _scratch = Scratch::new("ledger-on-failure");
            sparse_main();
            let head = "c".repeat(64);

            let error = runtime().block_on(fetch_history_scoped(&unreachable(), &head, "other", &Progress::NONE));

            assert!(error.is_err(), "nothing serves the head");
            assert_eq!(fetch_ledger::read_ledger().unwrap(), vec![LedgerEntry { head, pallet: "other".to_string() }]);
        }

        #[test]
        fn a_full_warehouse_records_nothing() {
            let _scratch = Scratch::new("ledger-full");

            let _ = runtime().block_on(fetch_history_scoped(&unreachable(), &"c".repeat(64), "main", &Progress::NONE));

            assert!(!globals::forklift_root().join("fetched-heads").exists());
        }

        // T17. Revert: return after the first transfer → one transfer, and the head the expand
        // dropped is not recorded again.
        #[test]
        fn a_scope_widened_mid_fetch_is_fetched_again_and_the_head_recorded_again() {
            let _scratch = Scratch::new("recheck");
            sparse_main();
            let head = "c".repeat(64);
            let mut scopes: Vec<Vec<String>> = Vec::new();

            runtime().block_on(fetch_recorded(&head, "other", async |scope: &PathScope| {
                scopes.push(scope.prefixes().to_vec());

                if scopes.len() == 1 {
                    // An expand commits while this transfer runs, dropping the head it found absent.
                    let lock = RefLock::acquire().unwrap();
                    fetch_ledger::write_ledger(&lock, &[]).unwrap();
                    crate::util::warehouse_utils::mark_for_test(crate::util::warehouse_utils::WarehouseMode::Sparse);
                    scope_utils::set_fetch_scope(&PathScope::from_prefixes(["src", "web"])).unwrap();
                }

                Ok(FetchStats::default())
            })).unwrap();

            assert_eq!(scopes, vec![vec!["src".to_string()], vec!["src".to_string(), "web".to_string()]]);
            assert_eq!(ledger_heads(), vec![head]);
        }

        #[test]
        fn expand_walks_every_ref_head_and_writes_the_widened_scope() {
            let _scratch = Scratch::new("expand-refs");
            let base = sparse_main();
            let other = parcel("other", Vec::new());
            pallet_utils::update_pallet_head("other", None, &other, &RefPreconditions::NONE).unwrap();

            let mut expansion = expand(&base, &["web"]);
            runtime().block_on(expansion.walk(&unreachable())).unwrap();
            expansion.walk_new_ref_heads_locally().unwrap();

            assert!(expansion.walked_heads.contains(&base) && expansion.walked_heads.contains(&other));
            assert!(matches!(expansion.commit().unwrap(), ExpandCommit::Written(_)));
            assert_eq!(scope_utils::read_fetch_scope().unwrap().prefixes(), ["src", "web"]);
        }

        // T12. Revert: drop the commit's check for unwalked ledger heads → it writes at once.
        #[test]
        fn a_ledger_head_recorded_after_the_walk_is_walked_before_the_commit() {
            let _scratch = Scratch::new("expand-late-ledger");
            let base = sparse_main();
            let mut expansion = expand(&base, &["web"]);
            runtime().block_on(expansion.walk(&unreachable())).unwrap();

            let late = parcel("late", vec![base.clone()]);
            record(&late, "late");

            assert!(matches!(expansion.commit().unwrap(), ExpandCommit::Retry));
            assert_eq!(scope_utils::read_fetch_scope().unwrap().prefixes(), ["src"], "nothing written");

            runtime().block_on(expansion.walk(&unreachable())).unwrap();
            assert!(expansion.walked_heads.contains(&late));
            assert!(matches!(expansion.commit().unwrap(), ExpandCommit::Written(_)));
            assert_eq!(ledger_heads(), vec![late]);
        }

        // T13. Revert: drop the commit's scope compare → it writes `src, web` over `src, docs`.
        #[test]
        fn a_scope_another_expand_widened_is_widened_again_not_overwritten() {
            let _scratch = Scratch::new("expand-concurrent");
            let base = sparse_main();
            let mut expansion = expand(&base, &["web"]);
            runtime().block_on(expansion.walk(&unreachable())).unwrap();

            crate::util::warehouse_utils::mark_for_test(crate::util::warehouse_utils::WarehouseMode::Sparse);
            scope_utils::set_fetch_scope(&PathScope::from_prefixes(["src", "docs"])).unwrap();

            assert!(matches!(expansion.commit().unwrap(), ExpandCommit::Retry));
            runtime().block_on(expansion.walk(&unreachable())).unwrap();
            assert!(matches!(expansion.commit().unwrap(), ExpandCommit::Written(_)));
            assert_eq!(scope_utils::read_fetch_scope().unwrap().prefixes(), ["docs", "src", "web"]);
        }

        #[test]
        fn a_prefix_another_expand_already_added_writes_nothing() {
            let _scratch = Scratch::new("expand-covered");
            let base = sparse_main();
            let mut expansion = expand(&base, &["web"]);
            runtime().block_on(expansion.walk(&unreachable())).unwrap();

            crate::util::warehouse_utils::mark_for_test(crate::util::warehouse_utils::WarehouseMode::Sparse);
            scope_utils::set_fetch_scope(&PathScope::from_prefixes(["src", "web"])).unwrap();

            assert!(matches!(expansion.commit().unwrap(), ExpandCommit::Covered(_)));
        }

        // T15. Revert: walk absent ledger heads → the walk reaches for the remote and marks it.
        #[test]
        fn an_absent_ledger_head_is_dropped_without_a_walk() {
            let _scratch = Scratch::new("expand-absent");
            let base = sparse_main();
            let absent = "e".repeat(64);
            record(&absent, "gone");

            let mut expansion = expand(&base, &["web"]);
            runtime().block_on(expansion.walk(&unreachable())).unwrap();

            assert!(!expansion.walked_heads.contains(&absent) && !expansion.unwalkable.contains(&absent));
            assert!(matches!(expansion.commit().unwrap(), ExpandCommit::Written(_)));
            assert!(ledger_heads().is_empty());
            assert!(!file_utils::does_object_exist(&absent).unwrap(), "nothing fetched it");
        }

        /// A ledger head whose parcel is here but whose tree is not: a fetch that stopped
        /// part-way, which the remote can no longer complete.
        fn part_way(base: &str) -> String {
            let head = parcel("part-way", vec![base.to_string()]);
            let tree = object_utils::load_parcel(&head).unwrap().tree_hash;
            let (folder, file) = file_utils::get_path_for_object(&tree).unwrap();
            std::fs::remove_file(std::path::Path::new(&folder).join(file)).unwrap();
            record(&head, "part-way");
            head
        }

        /// A ledger head whose `src` is here but whose `web` blob is not: complete at the scope
        /// `src`, so a ref may be written at it, but not at a scope that adds `web`, which the
        /// remote can no longer serve.
        fn web_missing(base: &str) -> String {
            let head = parcel("web-missing", vec![base.to_string()]);
            let blob = blob("b web-missing");
            let (folder, file) = file_utils::get_path_for_object(&blob).unwrap();
            std::fs::remove_file(std::path::Path::new(&folder).join(file)).unwrap();
            record(&head, "web-missing");
            head
        }

        // Revert: skip the closure check at the head compare-and-set → the pallet create lands.
        #[test]
        fn an_unservable_ledger_head_is_dropped_and_refused_as_a_new_head() {
            let _scratch = Scratch::new("expand-unwalkable");
            let base = sparse_main();
            let head = part_way(&base);

            let mut expansion = expand(&base, &["web"]);
            runtime().block_on(expansion.walk(&unreachable())).unwrap();
            expansion.walk_new_ref_heads_locally().unwrap();

            assert!(matches!(expansion.commit().unwrap(), ExpandCommit::Written(_)));
            assert!(ledger_heads().is_empty());

            let refused = pallet_utils::update_pallet_head("x", None, &head, &RefPreconditions::NONE);
            assert!(matches!(refused, Err(RefUpdateError::Incomplete { .. })), "{:?}", refused.map_err(|e| e.to_string()));
        }

        // Revert: skip the closure check at the head compare-and-set → the part-way head is
        // made a pallet before its fetch completes; or refuse every new head → it stays refused
        // after.
        #[test]
        fn a_part_way_head_is_refused_until_its_fetch_completes() {
            let _scratch = Scratch::new("part-way-refused");
            let base = sparse_main();
            let head = part_way(&base);

            let refused = pallet_utils::update_pallet_head("x", None, &head, &RefPreconditions::NONE);
            assert!(matches!(refused, Err(RefUpdateError::Incomplete { .. })), "{:?}", refused.map_err(|e| e.to_string()));
            assert_eq!(pallet_utils::get_pallet_head("x").unwrap(), None, "nothing was written");

            // The fetch completes: the missing tree arrives.
            content_tree("part-way");
            pallet_utils::update_pallet_head("x", None, &head, &RefPreconditions::NONE).unwrap();
        }

        // Revert: drop the ref write's ledger record → the head `main` left is in no record, so
        // the expand never walks it, and an undo could write it back unwidened.
        #[test]
        fn a_ref_write_that_leaves_a_head_unreached_records_it() {
            let _scratch = Scratch::new("ref-leaves");
            let base = sparse_main();
            let side = parcel("side", Vec::new());
            pallet_utils::update_pallet_head("main", Some(&base), &side, &RefPreconditions::NONE).unwrap();
            assert_eq!(ledger_heads(), vec![base.clone()], "the write left `base` unreached");

            let child = parcel("child", vec![side.clone()]);
            pallet_utils::update_pallet_head("main", Some(&side), &child, &RefPreconditions::NONE).unwrap();
            assert_eq!(ledger_heads(), vec![base.clone()], "a fast-forward leaves `side` reached");

            let mut expansion = expand(&child, &["web"]);
            runtime().block_on(expansion.walk(&unreachable())).unwrap();
            assert!(expansion.walked_heads.contains(&base));
        }

        #[test]
        fn a_ref_write_in_a_full_warehouse_records_nothing() {
            let _scratch = Scratch::new("ref-leaves-full");
            let base = parcel("main", Vec::new());
            pallet_utils::update_pallet_head("main", None, &base, &RefPreconditions::NONE).unwrap();
            let side = parcel("side", Vec::new());
            pallet_utils::update_pallet_head("main", Some(&base), &side, &RefPreconditions::NONE).unwrap();

            assert!(!globals::forklift_root().join("fetched-heads").exists());
        }

        // Revert: never prune → `kept` and `dropped` both stay; or prune by a ref that moved since
        // its walk → `kept` is dropped while nothing reaches it.
        #[test]
        fn a_ledger_head_a_standing_ref_reaches_is_dropped() {
            let _scratch = Scratch::new("ledger-prune");
            let base = sparse_main();
            let dropped = parcel("dropped", vec![base.clone()]);
            let kept = parcel("kept", vec![base.clone()]);
            record(&dropped, "y");
            record(&kept, "z");
            pallet_utils::update_pallet_head("y", None, &dropped, &RefPreconditions::NONE).unwrap();
            pallet_utils::update_pallet_head("z", None, &kept, &RefPreconditions::NONE).unwrap();

            let mut expansion = expand(&base, &["web"]);
            runtime().block_on(expansion.walk(&unreachable())).unwrap();

            let elsewhere = parcel("elsewhere", Vec::new());
            pallet_utils::update_pallet_head("z", Some(&kept), &elsewhere, &RefPreconditions::NONE).unwrap();
            expansion.walk_new_ref_heads_locally().unwrap();

            assert!(matches!(expansion.commit().unwrap(), ExpandCommit::Written(_)));
            assert_eq!(ledger_heads(), vec![kept]);
        }

        // Revert: leave the base walk out of `reached` → `main`'s own walk adds nothing after
        // it, so the ledger head in the current pallet's history is kept.
        #[test]
        fn a_ledger_head_in_the_bay_base_history_is_dropped() {
            let _scratch = Scratch::new("ledger-prune-base");
            let base = sparse_main();
            let child = parcel("child", vec![base.clone()]);
            pallet_utils::update_pallet_head("main", Some(&base), &child, &RefPreconditions::NONE).unwrap();
            record(&base, "main");

            let mut expansion = expand(&child, &["web"]);
            runtime().block_on(expansion.walk(&unreachable())).unwrap();
            expansion.walk_new_ref_heads_locally().unwrap();

            assert!(matches!(expansion.commit().unwrap(), ExpandCommit::Written(_)));
            assert!(ledger_heads().is_empty());
        }

        // Revert: drop the local walk's check of unwalkable heads → the commit widens the scope
        // over a ref whose history is incomplete at the widened scope.
        #[test]
        fn a_ref_written_at_an_unwalkable_head_refuses_the_expand() {
            let _scratch = Scratch::new("expand-unwalkable-ref");
            let base = sparse_main();
            let head = web_missing(&base);

            let mut expansion = expand(&base, &["web"]);
            runtime().block_on(expansion.walk(&unreachable())).unwrap();
            assert!(expansion.unwalkable.contains(&head));

            // Complete at the scope still on disk, so the ref write is accepted.
            pallet_utils::update_pallet_head("x", None, &head, &RefPreconditions::NONE).unwrap();

            // The first time, the head gets another walk rather than a refusal.
            expansion.walk_new_ref_heads_locally().unwrap();
            assert!(matches!(expansion.commit().unwrap(), ExpandCommit::Retry));

            runtime().block_on(expansion.walk(&unreachable())).unwrap();
            let error = expansion.walk_new_ref_heads_locally().unwrap_err();
            assert!(error.contains("\"x\"") && error.contains("part-way"), "{}", error);
        }

        // Revert: refuse at once when a new ref reaches an unwalkable head → the expand refuses
        // although the fetch behind the ref completed the head.
        #[test]
        fn an_unwalkable_head_that_a_ref_now_holds_is_walked_again() {
            let _scratch = Scratch::new("expand-unwalkable-rewalk");
            let base = sparse_main();
            let head = part_way(&base);

            let mut expansion = expand(&base, &["web"]);
            runtime().block_on(expansion.walk(&unreachable())).unwrap();
            assert!(expansion.unwalkable.contains(&head));

            // A lower completes the head and makes a pallet of it meanwhile.
            content_tree("part-way");
            pallet_utils::update_pallet_head("x", None, &head, &RefPreconditions::NONE).unwrap();

            expansion.walk_new_ref_heads_locally().unwrap();
            assert!(matches!(expansion.commit().unwrap(), ExpandCommit::Retry));

            runtime().block_on(expansion.walk(&unreachable())).unwrap();
            expansion.walk_new_ref_heads_locally().unwrap();
            assert!(matches!(expansion.commit().unwrap(), ExpandCommit::Written(_)));
        }
    }
}
