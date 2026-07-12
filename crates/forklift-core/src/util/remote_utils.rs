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
use crate::util::office_utils::OFFICE_PALLET_NAME;
use crate::util::scope_utils::{self, MaterializationScope, ScopeClass};
use crate::util::{
    bundle_utils, config_utils, file_utils, merge_utils, object_utils, office_utils,
    pallet_utils, sign_utils,
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

/// The remote endpoint: base URL, optional bearer token, and the HTTP client.
#[derive(Clone)]
pub struct RemoteClient {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
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
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| format!("Error while creating the HTTP client: {}", e))?;

        Ok(RemoteClient {
            http,
            base: url.trim_end_matches('/').to_string(),
            token,
        })
    }

    /// Create the client for the configured remote of the current warehouse
    /// (`remote.url`, plus `remote.token` when set).
    ///
    /// # Returns
    /// * `Ok(RemoteClient)` - The client.
    /// * `Err(String)`      - If no remote is configured.
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

    /// The base URL of the remote.
    pub fn url(&self) -> &str {
        &self.base
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut builder = self.http.request(method, format!("{}{}", self.base, path));

        if let Some(token) = &self.token {
            builder = builder.bearer_auth(token);
        }

        builder
    }

    /// Turn a non-success response into the server's error message.
    async fn error_of(response: reqwest::Response, action: &str) -> String {
        let status = response.status();

        let message = match response.json::<ErrorResponse>().await {
            Ok(body) => body.error,
            Err(_) => status.canonical_reason().unwrap_or("unknown error").to_string(),
        };

        format!("The remote refused {} ({}): {}", action, status.as_u16(), message)
    }

    /// Fetch the warehouse handshake and check the protocol version.
    pub async fn fetch_info(&self) -> Result<WarehouseInfo, String> {
        let response = self.request(reqwest::Method::GET, "/v1/warehouse")
            .send()
            .await
            .map_err(|e| format!("Could not reach the remote {}: {}", self.base, e))?;

        if !response.status().is_success() {
            return Err(Self::error_of(response, "the handshake").await);
        }

        let info: WarehouseInfo = response.json()
            .await
            .map_err(|e| format!("The remote's handshake is not valid JSON: {}", e))?;

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
    pub async fn missing_objects(&self, hashes: &[String]) -> Result<Vec<String>, String> {
        let mut missing: Vec<String> = Vec::new();

        for batch in hashes.chunks(MAX_MISSING_BATCH) {
            let response = self.request(reqwest::Method::POST, "/v1/objects/missing")
                .json(&MissingObjectsRequest { hashes: batch.to_vec() })
                .send()
                .await
                .map_err(|e| format!("Error while negotiating with the remote: {}", e))?;

            if !response.status().is_success() {
                return Err(Self::error_of(response, "the negotiation").await);
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
    pub async fn fetch_batch(&self, hashes: &[String]) -> Result<Option<Vec<u8>>, String> {
        let response = self.request(reqwest::Method::POST, "/v1/objects/batch")
            .json(&MissingObjectsRequest { hashes: hashes.to_vec() })
            .send()
            .await
            .map_err(|e| format!("Error while batch-fetching from the remote: {}", e))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !response.status().is_success() {
            return Err(Self::error_of(response, "the batch fetch").await);
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
            return Err(Self::error_of(response, &format!("the subtree fetch for \"{}\"", path)).await);
        }

        response.bytes()
            .await
            .map(|bytes| Some(bytes.to_vec()))
            .map_err(|e| format!("Error while reading the subtree response: {}", e))
    }

    /// Fetch one object's raw bytes.
    pub async fn fetch_object(&self, hash: &str) -> Result<Vec<u8>, String> {
        let response = self.request(reqwest::Method::GET, &format!("/v1/objects/{}", hash))
            .send()
            .await
            .map_err(|e| format!("Error while fetching object {}: {}", hash, e))?;

        if !response.status().is_success() {
            return Err(Self::error_of(response, &format!("object {}", hash)).await);
        }

        response.bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|e| format!("Error while reading object {}: {}", hash, e))
    }

    /// Upload one object's raw bytes to the control plane (`PUT /v1/objects/{hash}`), where the
    /// remote verifies the hash inline before the object becomes fetchable. This is the direct
    /// path — for the objects `upload-targets` returns in `direct`, and the whole missing set on
    /// the legacy fallback.
    pub async fn upload_object(&self, hash: &str, bytes: Vec<u8>) -> Result<(), String> {
        let response = self.request(reqwest::Method::PUT, &format!("/v1/objects/{}", hash))
            .body(bytes)
            .send()
            .await
            .map_err(|e| format!("Error while uploading object {}: {}", hash, e))?;

        if !response.status().is_success() {
            return Err(Self::error_of(response, &format!("object {}", hash)).await);
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
                return Err(Self::error_of(response, "the upload negotiation").await);
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
    /// token — and because the bearer is attached per request (in `request`, never as a client
    /// default header), a plain `self.http.put(url)` cannot leak it to the storage host, even
    /// were the storage host the remote itself.
    async fn put_presigned(&self, url: &str, bytes: Vec<u8>) -> Result<(), String> {
        let response = self.http.put(url)
            .body(bytes)
            .send()
            .await
            .map_err(|e| format!("Error while uploading to a staging URL: {}", e))?;

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
            .map_err(|e| format!("Error while committing the lift session: {}", e))?;

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
        let response = self.request(reqwest::Method::GET, &format!("/v1/signatures/{}", parcel_hash))
            .send()
            .await
            .map_err(|e| format!("Error while fetching the signature of {}: {}", parcel_hash, e))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !response.status().is_success() {
            return Err(Self::error_of(response, &format!("the signature of {}", parcel_hash)).await);
        }

        response.bytes()
            .await
            .map(|bytes| Some(bytes.to_vec()))
            .map_err(|e| format!("Error while reading the signature of {}: {}", parcel_hash, e))
    }

    /// Upload a parcel's signature sidecar.
    pub async fn upload_signature(&self, parcel_hash: &str, bytes: Vec<u8>) -> Result<(), String> {
        let response = self.request(reqwest::Method::PUT, &format!("/v1/signatures/{}", parcel_hash))
            .body(bytes)
            .send()
            .await
            .map_err(|e| format!("Error while uploading the signature of {}: {}", parcel_hash, e))?;

        if !response.status().is_success() {
            return Err(Self::error_of(response, &format!("the signature of {}", parcel_hash)).await);
        }

        Ok(())
    }

    /// Establish the trust anchor on the remote (idempotent for an identical anchor).
    pub async fn put_trust(&self, anchor: &TrustAnchorDto) -> Result<(), String> {
        let response = self.request(reqwest::Method::PUT, "/v1/trust")
            .json(anchor)
            .send()
            .await
            .map_err(|e| format!("Error while uploading the trust anchor: {}", e))?;

        if !response.status().is_success() {
            return Err(Self::error_of(response, "the trust anchor").await);
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
            .map_err(|e| format!("Error while moving the remote pallet \"{}\": {}", pallet, e))?;

        if !response.status().is_success() {
            return Err(Self::error_of(response, &format!("moving pallet \"{}\"", pallet)).await);
        }

        Ok(())
    }

    /// Download the remote's latest bundle into a file.
    ///
    /// # Returns
    /// * `Ok(true)`    - The bundle was downloaded.
    /// * `Ok(false)`   - The remote has no bundle.
    /// * `Err(String)` - On any other failure.
    pub async fn fetch_bundle_to(&self, path: &std::path::Path) -> Result<bool, String> {
        let mut response = self.request(reqwest::Method::GET, "/v1/bundles/latest")
            .send()
            .await
            .map_err(|e| format!("Error while fetching the bundle: {}", e))?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }

        if !response.status().is_success() {
            return Err(Self::error_of(response, "the bundle").await);
        }

        let mut file = std::fs::File::create(path)
            .map_err(|e| format!("Error while creating the bundle file: {}", e))?;

        while let Some(chunk) = response.chunk()
            .await
            .map_err(|e| format!("Error while downloading the bundle: {}", e))?
        {
            std::io::Write::write_all(&mut file, &chunk)
                .map_err(|e| format!("Error while writing the bundle file: {}", e))?;
        }

        Ok(true)
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
/// # Returns
/// * `Ok(usize)`   - How many objects were fetched.
/// * `Err(String)` - If a transfer or verification failed.
async fn fetch_missing_objects(client: &RemoteClient, hashes: &[String]) -> Result<usize, String> {
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

    fetch_missing_objects(client, &chunk_hashes).await
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

    let mut fetched = 0usize;

    while let Some(result) = tasks.join_next().await {
        fetched += result.map_err(|e| format!("A transfer task failed: {}", e))??;
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
                return Err(scope_utils::chunked_remote_refusal(&join_path(path_prefix, name)));
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
fn refuse_if_over_ceiling_for_upload(hash: &str, bytes: &[u8]) -> Result<(), String> {
    scope_utils::refuse_if_over_object_ceiling(&format!("object {}", hash), bytes.len())
}

/// Upload (concurrently) the objects of the given hashes.
async fn upload_objects(client: &RemoteClient, hashes: &[String]) -> Result<(), String> {
    if hashes.is_empty() {
        return Ok(());
    }

    let semaphore = Arc::new(Semaphore::new(CONCURRENT_TRANSFERS));
    let mut tasks: JoinSet<Result<(), String>> = JoinSet::new();

    for hash in hashes {
        let client = client.clone();
        let hash = hash.clone();
        let semaphore = Arc::clone(&semaphore);

        tasks.spawn(async move {
            let _permit = semaphore.acquire().await
                .map_err(|_| "The transfer pool was closed unexpectedly.".to_string())?;

            let bytes = file_utils::retrieve_object_by_hash(&hash)?;
            refuse_if_over_ceiling_for_upload(&hash, &bytes)?;

            client.upload_object(&hash, bytes).await
        });
    }

    join_all(tasks).await
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
/// * `Ok(())`      - One batch suffices, or the remote understands pagination either way.
/// * `Err(String)` - The `commit_pagination_unsupported` refusal.
fn refuse_if_commit_pagination_unsupported(staged_count: usize,
                                           chunking_supported: bool) -> Result<(), String> {
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

    let semaphore = Arc::new(Semaphore::new(CONCURRENT_TRANSFERS));
    let mut tasks: JoinSet<Result<(), String>> = JoinSet::new();

    for (hash, url) in targets {
        let client = client.clone();
        let hash = hash.clone();
        let url = url.clone();
        let semaphore = Arc::clone(&semaphore);

        tasks.spawn(async move {
            let _permit = semaphore.acquire().await
                .map_err(|_| "The transfer pool was closed unexpectedly.".to_string())?;

            let bytes = file_utils::retrieve_object_by_hash(&hash)?;
            refuse_if_over_ceiling_for_upload(&hash, &bytes)?;

            client.put_presigned(&url, bytes).await
        });
    }

    join_all(tasks).await
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

/// Await every task of a set, surfacing the first failure.
async fn join_all(mut tasks: JoinSet<Result<(), String>>) -> Result<(), String> {
    while let Some(result) = tasks.join_next().await {
        result.map_err(|e| format!("A transfer task failed: {}", e))??;
    }

    Ok(())
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
    use crate::builder::object::loose_object_builder::LooseObjectBuilder;
    use crate::globals::StorageRootScope;

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
    /// a non-chunking remote is refused, naming the exact staged count.
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
        let (code, message, next_step) = scope_utils::decode_refusal(&error)
            .expect("the refusal must decode via the shared sentinel framing");

        assert_eq!(code, scope_utils::CODE_COMMIT_PAGINATION_UNSUPPORTED);
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

        let Some((path, body)) = read_test_request(&mut stream) else { return };

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

    /// Read one HTTP/1.1 request (start line + content-length body only; no header inspection is
    /// needed by this fake). Returns `(path, body)`.
    fn read_test_request(stream: &mut std::net::TcpStream) -> Option<(String, Vec<u8>)> {
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
        let path = head.lines().next()?.split_whitespace().nth(1)?.to_string();

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

        Some((path, body))
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

    /// The same oversized negotiation against a remote that DOES advertise chunking never triggers
    /// the pagination gate — proven by the *kind* of failure, not by completing a real 10,000+
    /// object upload (too slow for a unit test; the pure boundary test above already nails the
    /// exact cap arithmetic with zero cost). The candidates are placeholder hashes naming no real
    /// object, so `upload_to_targets`'s per-object read fails downstream — but that failure is an
    /// ordinary read error, not a scope refusal at all, which is exactly the proof: had the gate
    /// fired, the error would decode as `CODE_COMMIT_PAGINATION_UNSUPPORTED` before a single object
    /// read was even attempted (as the sibling refusal test demonstrates). Its absence here means
    /// execution passed the gate and reached the real upload attempt.
    #[test]
    fn a_large_lift_to_a_chunking_remote_does_not_trigger_the_pagination_gate() {
        let remote = FakeStagingRemote::start(true);
        let client = RemoteClient::new(&remote.url, None).unwrap();
        let candidates = oversized_candidate_set();
        let control_plane: HashSet<String> = HashSet::new();

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let error = runtime.block_on(negotiate_and_upload(
            &client, "session-2", &candidates, &control_plane, true,
        )).expect_err("the placeholder hashes name no real object, so the upload attempt fails");

        assert!(
            scope_utils::decode_refusal(&error).is_none(),
            "a chunking-capable remote must never see the pagination refusal, only the (expected, \
            unrelated) downstream read failure: {}", error
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
}
