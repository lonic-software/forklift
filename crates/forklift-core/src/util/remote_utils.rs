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
    ErrorResponse, MissingObjectsRequest, MissingObjectsResponse, RefUpdateRequest,
    ResolveRequest, ResolveResponse, TrustAnchorDto, WarehouseInfo, MAX_MISSING_BATCH,
    PROTOCOL_VERSION,
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

    /// Upload one object's raw bytes.
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

    let missing = client.missing_objects(&candidates).await?;

    upload_objects(client, &missing).await?;

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
        uploaded_objects: missing.len(),
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
