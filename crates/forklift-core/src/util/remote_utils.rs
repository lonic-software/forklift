//! The client side of the remote protocol (`docs/format/REMOTE_PROTOCOL.md`): the HTTP
//! client and the sync engines behind `lift`, `lower` and `franchise`. Everything here
//! returns data — the commands own the words.
//!
//! Transfers are parallel by design (DESIGN.html §4.1): object fetches and uploads fan
//! out over concurrent connections, bounded by [`CONCURRENT_TRANSFERS`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use crate::model::remote::{
    CommitLiftRequest, ErrorResponse, MissingObjectsRequest, MissingObjectsResponse,
    RefUpdateRequest, ResolveRequest, ResolveResponse, TrustAnchorDto, UploadTargetsRequest,
    UploadTargetsResponse, WarehouseInfo, LIFT_SESSION_BLOB_NOT_READY, MAX_MISSING_BATCH,
    MAX_UPLOAD_TARGETS_BATCH, PROTOCOL_VERSION,
};
use crate::util::office_utils::OFFICE_PALLET_NAME;
use crate::util::{
    bundle_utils, config_utils, file_utils, merge_utils, object_utils, office_utils,
    pallet_utils, sign_utils,
};

/// How many object transfers run concurrently.
pub const CONCURRENT_TRANSFERS: usize = 24;

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
                         blobs: &[String]) -> Result<CommitOutcome, String> {
        let body = CommitLiftRequest {
            control_plane: control_plane.to_vec(),
            blobs: blobs.to_vec(),
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
/// transfer-economics half of R5.
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

            for tree_hash in &tree_wave {
                let tree = object_utils::load_tree(tree_hash)?;

                for (_, file) in tree.get_files() {
                    if seen_blobs.insert(file.hash.clone()) {
                        blob_wave.push(file.hash.clone());
                    }
                }

                for (_, subtree) in tree.get_subtrees() {
                    tree_frontier.push(subtree.hash.clone());
                }
            }

            stats.fetched_objects += fetch_missing_objects(client, &blob_wave).await?;
        }
    }

    Ok(stats)
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
                         remote_head: Option<&str>) -> Result<LiftResult, String> {
    lift_pallet_inner(client, pallet, local_head, remote_head, false).await
}

/// `lift_pallet`, allowing one sanctioned non-descendant update: the office lift right
/// after a re-genesis (§8.7), where the new chain replaces — rather than extends — the
/// remote's office head that the local anchor adopted. The server enforces the same
/// exception narrowly on its side.
async fn lift_pallet_inner(client: &RemoteClient,
                           pallet: &str,
                           local_head: &str,
                           remote_head: Option<&str>,
                           adopted_reset: bool) -> Result<LiftResult, String> {
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

    // The new parcels: everything from the local head down to the remote head. The
    // walk stops at the remote head itself — no pre-walk of the shared history — so a
    // linear lift touches O(new parcels). A merge that rejoins below the remote head
    // re-walks the shared slice; the negotiation drops it.
    let mut new_parcels: Vec<String> = Vec::new();
    let mut queue: Vec<String> = vec![local_head.to_string()];
    let mut visited: HashSet<String> = HashSet::new();

    while let Some(hash) = queue.pop() {
        if Some(hash.as_str()) == remote_head || !visited.insert(hash.clone()) {
            continue;
        }

        let parcel = object_utils::load_parcel(&hash)?;

        queue.extend(parcel.parents);
        new_parcels.push(hash);
    }

    // Candidate objects for the negotiation: each new parcel's tree, walked against
    // its first parent's tree — an unchanged subtree (identical hash at the same path)
    // is skipped whole, the same skip the merge walk and the pallet diff use. A
    // one-line change on a 100k-file warehouse thus negotiates the changed path, not
    // the full closure (DESIGN.html §4.5, data-plane item 1).
    let mut candidates: Vec<String> = new_parcels.clone();
    let mut seen_trees: HashSet<String> = HashSet::new();
    let mut seen_blobs: HashSet<String> = HashSet::new();

    // Oldest first: a parcel's first parent is remote-known or already processed, so
    // everything the base "explains" is on the remote or in the candidates already.
    for parcel_hash in new_parcels.iter().rev() {
        let parcel = object_utils::load_parcel(parcel_hash)?;

        let base_tree = match parcel.parents.first() {
            Some(parent) => Some(object_utils::load_parcel(parent)?.tree_hash),
            None => None,
        };

        collect_changed_closure(&parcel.tree_hash, base_tree.as_deref(),
                                &mut seen_trees, &mut seen_blobs, &mut candidates)?;
    }

    // Control-plane objects — parcels and trees — are promoted synchronously when a storage-
    // backed head commits the session; working blobs are promoted out of band by the staging
    // verifier and only presence-checked. Classify from the sets the closure walk already built
    // (`new_parcels` and `seen_trees`), rather than re-deriving each object's type on the wire.
    let mut control_plane: HashSet<String> = new_parcels.iter().cloned().collect();
    control_plane.extend(seen_trees.iter().cloned());

    // One flow serves both heads: negotiate upload targets, PUT the missing objects straight to
    // presigned staging URLs and/or to the control plane, and commit the staged session. Falls
    // back to `missing` + per-object `PUT` against a remote that predates `upload-targets`.
    let session = new_lift_session();
    let uploaded_objects = negotiate_and_upload(client, &session, &candidates, &control_plane).await?;

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

/// Collect the objects of a tree that its base — the tree at the same path in the
/// parent parcel — does not explain: an identical subtree or file is skipped whole,
/// a changed subtree is descended with the base's matching child as its base. `None`
/// collects the full closure (a root parcel has no base).
fn collect_changed_closure(tree_hash: &str,
                           base_tree_hash: Option<&str>,
                           seen_trees: &mut HashSet<String>,
                           seen_blobs: &mut HashSet<String>,
                           candidates: &mut Vec<String>) -> Result<(), String> {
    if base_tree_hash == Some(tree_hash) || !seen_trees.insert(tree_hash.to_string()) {
        return Ok(());
    }

    candidates.push(tree_hash.to_string());

    let tree = object_utils::load_tree(tree_hash)?;

    let base = match base_tree_hash {
        Some(hash) => Some(object_utils::load_tree(hash)?),
        None => None,
    };

    let base_files: HashMap<&String, &String> = base.as_ref()
        .map(|base| base.get_files().map(|(name, file)| (name, &file.hash)).collect())
        .unwrap_or_default();

    let base_subtrees: HashMap<&String, &String> = base.as_ref()
        .map(|base| base.get_subtrees().map(|(name, tree)| (name, &tree.hash)).collect())
        .unwrap_or_default();

    for (name, file) in tree.get_files() {
        if base_files.get(name) == Some(&&file.hash) {
            continue;
        }

        if seen_blobs.insert(file.hash.clone()) {
            candidates.push(file.hash.clone());
        }
    }

    for (name, subtree) in tree.get_subtrees() {
        collect_changed_closure(
            &subtree.hash,
            base_subtrees.get(name).map(|hash| hash.as_str()),
            seen_trees,
            seen_blobs,
            candidates
        )?;
    }

    Ok(())
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

            client.upload_object(&hash, bytes).await
        });
    }

    join_all(tasks).await
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
async fn negotiate_and_upload(client: &RemoteClient,
                              session: &str,
                              candidates: &[String],
                              control_plane: &HashSet<String>) -> Result<usize, String> {
    let Some(negotiation) = client.upload_targets(session, candidates).await? else {
        // An older remote with no `upload-targets`: negotiate the missing set and PUT each body
        // to the control plane, exactly as before.
        let missing = client.missing_objects(candidates).await?;
        upload_objects(client, &missing).await?;
        return Ok(missing.len());
    };

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

            client.put_presigned(&url, bytes).await
        });
    }

    join_all(tasks).await
}

/// Commit a staged lift session, retrying with bounded backoff while a blob is still being
/// promoted out of band by the staging verifier (the one transient failure — every other
/// commit failure surfaces at once). Gives up with a clear, safe-to-retry error rather than
/// hanging on a stuck verifier.
async fn commit_staged_session(client: &RemoteClient,
                               session: &str,
                               control_plane: &[String],
                               blobs: &[String]) -> Result<(), String> {
    let mut delay = COMMIT_BACKOFF_START;

    for attempt in 1..=MAX_COMMIT_ATTEMPTS {
        match client.commit_lift(session, control_plane, blobs).await? {
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
        adopted_reset
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

        let result = lift_pallet(client, &wire, &local_head, remote_head).await?;
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
}
