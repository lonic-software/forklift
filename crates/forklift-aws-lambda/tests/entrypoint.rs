//! The HTTP edge suite: the whole protocol walk through the pure router
//! [`forklift_aws_lambda::handle`], asserting the status codes and bodies of
//! `REMOTE_PROTOCOL.md` against the in-memory fakes — the same warehouse `protocol.rs`
//! exercises at the `Head` level, one layer up at the wire.
//!
//! No AWS, no Lambda runtime: `handle` speaks the plain `http` crate, so a synthetic
//! `http::Request` *is* the request an API Gateway event would decode into. This runs on every
//! push (no `--features lambda`), exactly as CI drives it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use http::{Request, Response};
use serde::de::DeserializeOwned;
use serde::Serialize;

use forklift_aws_lambda::memory::{MemoryObjectStore, MemoryRefStore};
use forklift_aws_lambda::store::{
    CasOutcome, ObjectAccess, ObjectStore, PromoteOutcome, PutOutcome, PutTarget, RefStore,
    SignatureOutcome, TrustOutcome,
};
use forklift_aws_lambda::{handle, AuthConfig, Head, Routing};

use forklift_core::globals::StorageRootScope;
use forklift_core::model::remote::{
    CommitLiftRequest, MissingObjectsRequest, MissingObjectsResponse, RefUpdateRequest,
    ResolveResponse, TrustAnchorDto, UploadTargetsRequest, UploadTargetsResponse, WarehouseInfo,
    MAX_MISSING_BATCH,
};
use forklift_core::util::office_utils::{self, OFFICE_PALLET_NAME};
use forklift_core::util::pallet_utils::{self, PalletNamespace};
use forklift_core::util::{file_utils, object_utils, sign_utils};

// -------------------------------------------------------------------------------------------
// Shared-state store wrappers. `handle` owns a fresh `Head` per call (a per-request head is
// how multi-warehouse serving works), so a multi-request walk shares one store through an
// `Arc`. These forward *every* trait method — including the presigned overrides the fakes
// carry — so the wrapped store behaves identically to the store it wraps.
// -------------------------------------------------------------------------------------------

#[derive(Clone)]
struct SharedObjects(Arc<MemoryObjectStore>);

impl ObjectStore for SharedObjects {
    fn exists(&self, hash: &str) -> Result<bool, String> {
        self.0.exists(hash)
    }
    fn get(&self, hash: &str) -> Result<Option<Vec<u8>>, String> {
        self.0.get(hash)
    }
    fn put_verified(&self, hash: &str, bytes: &[u8]) -> Result<PutOutcome, String> {
        self.0.put_verified(hash, bytes)
    }
    fn get_signature(&self, parcel_hash: &str) -> Result<Option<Vec<u8>>, String> {
        self.0.get_signature(parcel_hash)
    }
    fn put_signature(&self, parcel_hash: &str, bytes: &[u8]) -> Result<SignatureOutcome, String> {
        self.0.put_signature(parcel_hash, bytes)
    }
    fn access(&self, hash: &str) -> Result<Option<ObjectAccess>, String> {
        self.0.access(hash)
    }
    fn put_target(&self, session: Option<&str>, hash: &str) -> Result<PutTarget, String> {
        self.0.put_target(session, hash)
    }
    fn verify_and_promote(&self, session: &str, hash: &str) -> Result<PromoteOutcome, String> {
        self.0.verify_and_promote(session, hash)
    }
    fn discard_session(&self, session: &str) -> Result<(), String> {
        self.0.discard_session(session)
    }
    fn offload_response(&self, bytes: &[u8]) -> Result<Option<String>, String> {
        self.0.offload_response(bytes)
    }
}

#[derive(Clone)]
struct SharedRefs(Arc<MemoryRefStore>);

impl RefStore for SharedRefs {
    fn get_head(&self, namespace: PalletNamespace, name: &str) -> Result<Option<String>, String> {
        self.0.get_head(namespace, name)
    }
    fn compare_and_set_head(
        &self,
        namespace: PalletNamespace,
        name: &str,
        expected: Option<&str>,
        new: &str,
    ) -> Result<CasOutcome, String> {
        self.0.compare_and_set_head(namespace, name, expected, new)
    }
    fn list_refs(&self) -> Result<Vec<(pallet_utils::PalletRef, String)>, String> {
        self.0.list_refs()
    }
    fn default_pallet(&self) -> Result<String, String> {
        self.0.default_pallet()
    }
    fn get_trust(&self) -> Result<Option<office_utils::TrustAnchor>, String> {
        self.0.get_trust()
    }
    fn put_trust_if_absent(
        &self,
        anchor: &office_utils::TrustAnchor,
    ) -> Result<TrustOutcome, String> {
        self.0.put_trust_if_absent(anchor)
    }
    fn replace_trust(&self, anchor: &office_utils::TrustAnchor) -> Result<(), String> {
        self.0.replace_trust(anchor)
    }
}

/// A head over one shared warehouse, driven through `handle` request by request.
///
/// `auth` defaults to [`AuthConfig::Open`] in every constructor below — this suite drives the
/// protocol walk, not the auth seam (which has its own dedicated tests further down), so every
/// call site is deliberately pre-authenticated in this one place rather than per test.
struct Fixture {
    objects: Arc<MemoryObjectStore>,
    refs: Arc<MemoryRefStore>,
    routing: Routing,
    auth: AuthConfig,
}

impl Fixture {
    /// Single-warehouse serving over a direct (bytes-inline) store.
    fn direct() -> Fixture {
        Fixture {
            objects: Arc::new(MemoryObjectStore::new()),
            refs: Arc::new(MemoryRefStore::new()),
            routing: Routing::Single("wh".to_string()),
            auth: AuthConfig::Open,
        }
    }

    /// Single-warehouse serving over a staging (presigned-redirect) store.
    fn staging() -> Fixture {
        Fixture {
            objects: Arc::new(MemoryObjectStore::with_redirect("https://s3.example/bucket")),
            refs: Arc::new(MemoryRefStore::new()),
            routing: Routing::Single("wh".to_string()),
            auth: AuthConfig::Open,
        }
    }

    /// Multi-warehouse serving: the id travels in the path.
    fn multi() -> Fixture {
        Fixture {
            objects: Arc::new(MemoryObjectStore::new()),
            refs: Arc::new(MemoryRefStore::new()),
            routing: Routing::Multi,
            auth: AuthConfig::Open,
        }
    }

    /// Route one request against the shared store.
    fn call(&self, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
        let (objects, refs) = (self.objects.clone(), self.refs.clone());

        handle(
            &self.routing,
            &self.auth,
            move |_warehouse| Ok(Head::new(SharedObjects(objects), SharedRefs(refs))),
            request,
        )
    }

    /// Route one request and report which warehouse id the router resolved for it.
    fn call_capturing(&self, request: Request<Vec<u8>>) -> (Response<Vec<u8>>, Option<String>) {
        let (objects, refs) = (self.objects.clone(), self.refs.clone());
        let seen = Rc::new(RefCell::new(None));
        let sink = Rc::clone(&seen);

        let response = handle(
            &self.routing,
            &self.auth,
            move |warehouse| {
                *sink.borrow_mut() = Some(warehouse.to_string());
                Ok(Head::new(SharedObjects(objects), SharedRefs(refs)))
            },
            request,
        );

        let resolved = seen.borrow().clone();
        (response, resolved)
    }
}

// -------------------------------------------------------------------------------------------
// Request/response helpers.
// -------------------------------------------------------------------------------------------

fn get(uri: &str) -> Request<Vec<u8>> {
    Request::builder().method("GET").uri(uri).body(Vec::new()).unwrap()
}

fn post_json<T: Serialize>(uri: &str, body: &T) -> Request<Vec<u8>> {
    Request::builder().method("POST").uri(uri).body(serde_json::to_vec(body).unwrap()).unwrap()
}

fn put_bytes(uri: &str, body: Vec<u8>) -> Request<Vec<u8>> {
    Request::builder().method("PUT").uri(uri).body(body).unwrap()
}

fn put_json<T: Serialize>(uri: &str, body: &T) -> Request<Vec<u8>> {
    Request::builder().method("PUT").uri(uri).body(serde_json::to_vec(body).unwrap()).unwrap()
}

fn status(response: &Response<Vec<u8>>) -> u16 {
    response.status().as_u16()
}

fn location(response: &Response<Vec<u8>>) -> String {
    response.headers().get(http::header::LOCATION).expect("a Location header").to_str().unwrap().to_string()
}

fn body_json<T: DeserializeOwned>(response: &Response<Vec<u8>>) -> T {
    serde_json::from_slice(response.body()).expect("a JSON body")
}

// -------------------------------------------------------------------------------------------
// The untrusted lift walk, end to end through the HTTP edge.
// -------------------------------------------------------------------------------------------

#[test]
fn the_untrusted_lift_walk_maps_every_status() {
    let area = Area::new("untrusted");
    prepare(&area, "wh");
    area.write_file("wh/readme.txt", "hello\n");
    area.write_file("wh/src/main.txt", "fn main\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "first"]);

    let harvest = harvest(&area.path("wh"));
    let main_head = harvest.head_of("main").expect("main has a head");

    let fixture = Fixture::direct();

    // Handshake: an empty warehouse.
    let response = fixture.call(get("/v1/warehouse"));
    assert_eq!(status(&response), 200);
    let info: WarehouseInfo = body_json(&response);
    assert!(info.pallets.is_empty());
    assert!(info.trust.is_none());
    assert_eq!(info.default_pallet, "main");

    // Negotiation: everything is missing.
    let all: Vec<String> = harvest.objects.keys().cloned().collect();
    let response = fixture.call(post_json("/v1/objects/missing", &MissingObjectsRequest { hashes: all.clone() }));
    assert_eq!(status(&response), 200);
    let missing: MissingObjectsResponse = body_json(&response);
    assert_eq!(missing.missing.len(), all.len());

    // Upload each object: a create is 201, an idempotent re-upload is 200.
    let mut first: Option<String> = None;
    for (hash, bytes) in &harvest.objects {
        let response = fixture.call(put_bytes(&format!("/v1/objects/{}", hash), bytes.clone()));
        assert_eq!(status(&response), 201, "a new object is created");
        first.get_or_insert_with(|| hash.clone());
    }
    let first = first.expect("at least one object");
    let bytes = harvest.objects[&first].clone();
    let response = fixture.call(put_bytes(&format!("/v1/objects/{}", first), bytes));
    assert_eq!(status(&response), 200, "an already-present object is an idempotent 200");

    // Nothing missing now.
    let response = fixture.call(post_json("/v1/objects/missing", &MissingObjectsRequest { hashes: all }));
    let missing: MissingObjectsResponse = body_json(&response);
    assert!(missing.missing.is_empty());

    // The lift commits.
    let commit = RefUpdateRequest { old_head: None, new_head: main_head.clone() };
    let response = fixture.call(post_json("/v1/pallets/main", &commit));
    assert_eq!(status(&response), 200);

    // The handshake reflects it.
    let info: WarehouseInfo = body_json(&fixture.call(get("/v1/warehouse")));
    assert_eq!(info.pallets.get("main"), Some(&main_head));

    // A stale replay (the pallet now exists) is a 409, and the body is the protocol error.
    let response = fixture.call(post_json("/v1/pallets/main", &commit));
    assert_eq!(status(&response), 409);
    let error: serde_json::Value = body_json(&response);
    assert!(error.get("error").and_then(|value| value.as_str()).is_some(), "a JSON error body");
}

/// A wrong-hash upload is refused with a 422 — nothing unverified enters the store.
#[test]
fn a_tampered_upload_is_a_422() {
    let fixture = Fixture::direct();

    let response = fixture.call(put_bytes(&format!("/v1/objects/{}", "a".repeat(64)), b"not that content".to_vec()));
    assert_eq!(status(&response), 422);
}

/// Signatures: created (201), idempotent (200), and a conflicting sidecar is a 409.
#[test]
fn signature_endpoints_map_created_idempotent_and_conflict() {
    let area = Area::new("signatures");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "signed"]);

    let harvest = harvest(&area.path("wh"));
    let (parcel, sidecar) = harvest.signatures.iter().next().expect("a signed parcel");

    let fixture = Fixture::direct();

    let path = format!("/v1/signatures/{}", parcel);
    assert_eq!(status(&fixture.call(put_bytes(&path, sidecar.clone()))), 201);
    assert_eq!(status(&fixture.call(put_bytes(&path, sidecar.clone()))), 200);

    // A GET returns the sidecar bytes.
    let response = fixture.call(get(&path));
    assert_eq!(status(&response), 200);
    assert_eq!(response.body(), sidecar);

    // A GET for an unsigned parcel is a 404.
    assert_eq!(status(&fixture.call(get(&format!("/v1/signatures/{}", "b".repeat(64))))), 404);

    // A conflicting sidecar for the same parcel is a 409.
    if let Some((_, other)) = harvest.signatures.iter().find(|(hash, _)| *hash != parcel) {
        assert_eq!(status(&fixture.call(put_bytes(&path, other.clone()))), 409);
    }
}

// -------------------------------------------------------------------------------------------
// The presigned byte plane: object reads/writes answer 307 (same method replayed at the
// target); the batch bundle answers 303 instead, since its POST must switch to a GET.
// -------------------------------------------------------------------------------------------

/// A staging store redirects object reads to the canonical key, refuses a session-less upload,
/// and redirects a session upload to the staging key — never the hash key.
#[test]
fn the_staging_byte_plane_answers_307_and_refuses_a_session_less_put() {
    let fixture = Fixture::staging();

    // Seed one object as if already promoted into S3.
    let bytes = b"an object".to_vec();
    let hash = object_utils::hash_object_bytes(&bytes);
    fixture.objects.put_verified(&hash, &bytes).expect("seed");

    // A read redirects to the canonical key.
    let response = fixture.call(get(&format!("/v1/objects/{}", hash)));
    assert_eq!(status(&response), 307);
    assert_eq!(location(&response), format!("https://s3.example/bucket/objects/{}", hash));

    // A session upload redirects to the staging key, never `/objects/`.
    let response = fixture.call(put_bytes(&format!("/v1/objects/{}?session=lift-1", hash), b"ignored".to_vec()));
    assert_eq!(status(&response), 307);
    let target = location(&response);
    assert_eq!(target, format!("https://s3.example/bucket/staging/lift-1/{}", hash));
    assert!(!target.contains("/objects/"), "an upload must never target the hash key");

    // A session-less upload has nowhere to stage: 422.
    let response = fixture.call(put_bytes(&format!("/v1/objects/{}", hash), b"ignored".to_vec()));
    assert_eq!(status(&response), 422);

    // An absent object is a 404 (the redirect never points at a missing key).
    assert_eq!(status(&fixture.call(get(&format!("/v1/objects/{}", "c".repeat(64))))), 404);
}

/// `?session=` with an empty value must not be treated as a real session — that
/// would presign a `staging//{hash}` key `commit_lift` could never promote, stranding the
/// object. It is routed exactly like a request with no `session` parameter: `422
/// SessionRequired`, never a `307` to a doubled-slash staging key.
#[test]
fn an_empty_session_query_value_is_treated_as_absent() {
    let fixture = Fixture::staging();

    let hash = object_utils::hash_object_bytes(b"an object");

    let response =
        fixture.call(put_bytes(&format!("/v1/objects/{}?session=", hash), b"ignored".to_vec()));
    assert_eq!(status(&response), 422, "an empty session is treated as absent, not a real one");

    // Confirm it is genuinely the "no session" path, not some other 422: the identical request
    // with a real session redirects, and one with no `?session` at all fails the same way.
    let with_real_session =
        fixture.call(put_bytes(&format!("/v1/objects/{}?session=lift-1", hash), b"ignored".to_vec()));
    assert_eq!(status(&with_real_session), 307);

    let with_no_session = fixture.call(put_bytes(&format!("/v1/objects/{}", hash), b"ignored".to_vec()));
    assert_eq!(status(&with_no_session), status(&response), "empty and absent answer identically");
}

/// The body-less upload negotiation sorts hashes into present/targets/direct.
#[test]
fn upload_targets_negotiates_over_http() {
    let fixture = Fixture::staging();

    let present = b"already here".to_vec();
    let present_hash = object_utils::hash_object_bytes(&present);
    fixture.objects.put_verified(&present_hash, &present).expect("seed");
    let wanted_hash = object_utils::hash_object_bytes(b"not here yet");

    let request = UploadTargetsRequest {
        session: "lift-1".to_string(),
        hashes: vec![present_hash.clone(), wanted_hash.clone()],
    };
    let response = fixture.call(post_json("/v1/objects/upload-targets", &request));
    assert_eq!(status(&response), 200);

    let answer: UploadTargetsResponse = body_json(&response);
    assert_eq!(answer.present, vec![present_hash]);
    assert!(answer.direct.is_empty());
    assert_eq!(
        answer.targets.get(&wanted_hash).map(String::as_str),
        Some(format!("https://s3.example/bucket/staging/lift-1/{}", wanted_hash).as_str())
    );
}

/// `upload-targets` has its own, smaller batch cap than the protocol's shared
/// `MAX_MISSING_BATCH` (10 000) — each response entry carries a presigned URL, not a bare hash,
/// so a `MAX_MISSING_BATCH`-sized request would answer with several megabytes of JSON, at or
/// over a Lambda synchronous response's limit. A request over the router's cap is refused
/// before `Head::upload_targets` ever runs, and the error names the cap.
#[test]
fn upload_targets_over_the_router_cap_is_a_422_naming_the_cap() {
    let fixture = Fixture::staging();

    // One over the documented cap (1000; see `entrypoint::MAX_UPLOAD_TARGETS_BATCH`).
    let hashes: Vec<String> = (0..1001).map(|i| format!("{:064x}", i)).collect();
    let request = UploadTargetsRequest { session: "lift-1".to_string(), hashes };

    let response = fixture.call(post_json("/v1/objects/upload-targets", &request));
    assert_eq!(status(&response), 422);

    let error: serde_json::Value = body_json(&response);
    let message = error["error"].as_str().expect("a JSON error body");
    assert!(message.contains("1000"), "the error should name the cap: {}", message);

    // Exactly at the cap still succeeds.
    let hashes: Vec<String> = (0..1000).map(|i| format!("{:064x}", i)).collect();
    let request = UploadTargetsRequest { session: "lift-1".to_string(), hashes };
    let response = fixture.call(post_json("/v1/objects/upload-targets", &request));
    assert_eq!(status(&response), 200, "exactly at the cap is still accepted");
}

/// The full staged-commit path over HTTP: a corrupt object refuses the commit (422), a clean
/// one promotes (200) and only then becomes fetchable. Also proves both spelling of the commit
/// route (`/v1/lift/...` and `/lift/...`) reach the handler.
#[test]
fn commit_lift_verifies_and_promotes_over_http() {
    let fixture = Fixture::staging();

    let good = b"a good control-plane object".to_vec();
    let good_hash = object_utils::hash_object_bytes(&good);
    let corrupt_hash = object_utils::hash_object_bytes(b"the declared content");

    fixture.objects.stage("lift-1", &good_hash, good);
    fixture.objects.stage("lift-1", &corrupt_hash, b"tampered".to_vec());

    // Neither is fetchable while merely staged.
    for hash in [&good_hash, &corrupt_hash] {
        assert_eq!(status(&fixture.call(get(&format!("/v1/objects/{}", hash)))), 404);
    }

    // A commit naming the corrupt object is refused.
    let commit = CommitLiftRequest {
        control_plane: vec![good_hash.clone(), corrupt_hash.clone()],
        blobs: vec![],
        more: false,
    };
    assert_eq!(status(&fixture.call(post_json("/v1/lift/lift-1/commit", &commit))), 422);

    // A commit over only the good object promotes it — via the un-versioned spelling.
    let commit = CommitLiftRequest { control_plane: vec![good_hash.clone()], blobs: vec![], more: false };
    assert_eq!(status(&fixture.call(post_json("/lift/lift-1/commit", &commit))), 200);

    // Now — and only now — it is fetchable (as a redirect to the canonical key).
    let response = fixture.call(get(&format!("/v1/objects/{}", good_hash)));
    assert_eq!(status(&response), 307);
    assert_eq!(location(&response), format!("https://s3.example/bucket/objects/{}", good_hash));
}

/// `commit_lift` had no batch cap at all (every other list-taking route enforces the
/// protocol's shared `MAX_MISSING_BATCH` inside `Head`). A request whose `control_plane` and
/// `blobs` lists together exceed the cap is refused with a 422 before `Head::commit_lift` runs.
#[test]
fn commit_lift_over_the_shared_cap_is_a_422() {
    let fixture = Fixture::staging();

    // The combined length is one over `MAX_MISSING_BATCH`; the two lists individually stay
    // under it, proving the cap applies to their sum, not either list alone.
    let control_plane: Vec<String> =
        (0..MAX_MISSING_BATCH / 2).map(|i| format!("{:064x}", i)).collect();
    let blobs: Vec<String> = (0..(MAX_MISSING_BATCH / 2 + 1))
        .map(|i| format!("{:064x}", i + MAX_MISSING_BATCH))
        .collect();
    assert!(control_plane.len() + blobs.len() > MAX_MISSING_BATCH);

    let commit = CommitLiftRequest { control_plane, blobs, more: false };
    let response = fixture.call(post_json("/v1/lift/lift-1/commit", &commit));
    assert_eq!(status(&response), 422);

    let error: serde_json::Value = body_json(&response);
    let message = error["error"].as_str().expect("a JSON error body");
    assert!(message.contains(&MAX_MISSING_BATCH.to_string()), "{}", message);
}

/// A `batch` over a store that offloads answers 303 (not 307 — this request is a POST, and the
/// presigned response URL only ever accepts GET, so the client must switch methods rather than
/// replay the POST there) to a presigned response URL, outside the object namespace; a direct
/// store streams the bundle inline as `application/octet-stream`.
#[test]
fn batch_offloads_or_streams_by_store() {
    let area = Area::new("batch");
    prepare(&area, "wh");
    area.write_file("wh/a.txt", "alpha\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "one"]);

    let harvest = harvest(&area.path("wh"));
    let hashes: Vec<String> = harvest.objects.keys().cloned().collect();

    // Direct store: the bundle streams inline.
    let direct = Fixture::direct();
    for (hash, bytes) in &harvest.objects {
        direct.objects.put_verified(hash, bytes).expect("seed");
    }
    let response = direct.call(post_json("/v1/objects/batch", &MissingObjectsRequest { hashes: hashes.clone() }));
    assert_eq!(status(&response), 200);
    assert_eq!(
        response.headers().get(http::header::CONTENT_TYPE).unwrap(),
        "application/octet-stream"
    );
    assert_eq!(response.headers().get(http::header::CONTENT_ENCODING).unwrap(), "identity");
    assert!(!response.body().is_empty());

    // Staging store: the bundle offloads to a presigned response URL.
    let staging = Fixture::staging();
    for (hash, bytes) in &harvest.objects {
        staging.objects.put_verified(hash, bytes).expect("seed");
    }
    let response = staging.call(post_json("/v1/objects/batch", &MissingObjectsRequest { hashes }));
    assert_eq!(status(&response), 303, "a POST redirecting to a GET-only target answers 303, not 307");
    let url = location(&response);
    assert!(url.starts_with("https://s3.example/bucket/responses/"), "{}", url);
    assert!(!url.contains("/objects/"), "a response body is never an object");
}

// -------------------------------------------------------------------------------------------
// The trusted lift, driven through the HTTP edge (the audit still runs).
// -------------------------------------------------------------------------------------------

#[test]
fn a_trusted_lift_runs_the_audit_through_http() {
    let area = Area::new("trusted");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "signed one"]);

    let harvest = harvest(&area.path("wh"));
    let anchor = harvest.trust.clone().expect("trust established");
    let office_head = harvest.head_of(&format!("@{}", OFFICE_PALLET_NAME)).expect("office head");
    let main_head = harvest.head_of("main").expect("main head");

    let fixture = Fixture::direct();

    // Seed the objects and signatures directly (bulk); the interesting paths are trust + the
    // audited ref updates, driven through `handle`.
    for (hash, bytes) in &harvest.objects {
        fixture.objects.put_verified(hash, bytes).expect("seed object");
    }
    for (hash, sidecar) in &harvest.signatures {
        fixture.objects.put_signature(hash, sidecar).expect("seed signature");
    }

    // Trust: established (201), then idempotent (200).
    assert_eq!(status(&fixture.call(put_json("/v1/trust", &anchor))), 201);
    assert_eq!(status(&fixture.call(put_json("/v1/trust", &anchor))), 200);

    // A user lift before the office is refused: the audit has no keys (422).
    let premature = RefUpdateRequest { old_head: None, new_head: main_head.clone() };
    assert_eq!(status(&fixture.call(post_json("/v1/pallets/main", &premature))), 422);

    // The office lifts to its meta path, then the working pallet audits clean.
    let office = RefUpdateRequest { old_head: None, new_head: office_head.clone() };
    assert_eq!(
        status(&fixture.call(post_json(&format!("/v1/pallets/@{}", OFFICE_PALLET_NAME), &office))),
        200
    );
    let main = RefUpdateRequest { old_head: None, new_head: main_head.clone() };
    assert_eq!(status(&fixture.call(post_json("/v1/pallets/main", &main))), 200);

    let info: WarehouseInfo = body_json(&fixture.call(get("/v1/warehouse")));
    assert_eq!(info.pallets.get("main"), Some(&main_head));
    assert_eq!(info.pallets.get(&format!("@{}", OFFICE_PALLET_NAME)), Some(&office_head));
    assert!(info.trust.is_some());
}

// -------------------------------------------------------------------------------------------
// The additive/degrading endpoints and the routing surface.
// -------------------------------------------------------------------------------------------

/// `bundles/latest` is a spec-compliant 404 until a builder ships; `resolve` is an empty map.
#[test]
fn bundle_latest_is_404_and_resolve_is_an_empty_map() {
    let fixture = Fixture::direct();

    assert_eq!(status(&fixture.call(get("/v1/bundles/latest"))), 404);

    let response = fixture.call(post_json(
        "/v1/resolve",
        &serde_json::json!({ "identifiers": ["someone@forklift"] }),
    ));
    assert_eq!(status(&response), 200);
    let resolved: ResolveResponse = body_json(&response);
    assert!(resolved.names.is_empty());
}

/// Multi-warehouse routing: the id in the path reaches the head builder, a bad id is a 422, and
/// a missing `/warehouses/{id}` prefix is a 404.
#[test]
fn multi_warehouse_routing_resolves_the_path_id() {
    let fixture = Fixture::multi();

    // A valid id routes and reaches the head builder.
    let (response, resolved) = fixture.call_capturing(get("/warehouses/acme/v1/warehouse"));
    assert_eq!(status(&response), 200);
    assert_eq!(resolved.as_deref(), Some("acme"));

    // A well-formed but never-used warehouse simply hands back an empty handshake (the AWS head
    // does not track existence; that is the hosting registry's job).
    let info: WarehouseInfo = body_json(&response);
    assert!(info.pallets.is_empty());

    // An invalid id is a 422, and the head builder is never reached.
    let (response, resolved) = fixture.call_capturing(get("/warehouses/..%2Fescape/v1/warehouse"));
    assert_eq!(status(&response), 422);
    assert!(resolved.is_none());
    // A dotted-leading id is likewise rejected.
    assert_eq!(status(&fixture.call(get("/warehouses/.hidden/v1/warehouse"))), 422);

    // Without the prefix, multi mode has no warehouse to serve: 404.
    assert_eq!(status(&fixture.call(get("/v1/warehouse"))), 404);
}

/// An unknown path or a wrong method on a known resource is a 404.
#[test]
fn unknown_routes_are_404() {
    let fixture = Fixture::direct();

    assert_eq!(status(&fixture.call(get("/v1/nonsense"))), 404);
    assert_eq!(status(&fixture.call(get("/"))), 404);
    // `warehouse` is a GET-only resource; a POST does not match.
    assert_eq!(status(&fixture.call(post_json("/v1/warehouse", &serde_json::json!({})))), 404);
    // The version prefix is mandatory for the versioned endpoints.
    assert_eq!(status(&fixture.call(get("/warehouse"))), 404);
}

/// A `500` must not forward `Head`'s internal message verbatim — that message can
/// wrap a raw SDK failure carrying a request id, a bucket or table name, which this hosted,
/// multi-tenant edge must not hand to whoever is asking (unlike `forklift-server`, which
/// forwards its `500`s because that self-host head only ever runs on the operator's own
/// infrastructure). Force one via a failing `build_head` (the same path a real client-building
/// failure takes) and assert the body is generic, not the detailed message.
#[test]
fn an_internal_error_is_redacted_at_the_edge_not_forwarded_verbatim() {
    let routing = Routing::Single("wh".to_string());
    let request = get("/v1/warehouse");

    let sensitive = "S3 bucket forklift-prod-customer-42 (request-id 8f1c2b, role AKIAEXAMPLE) \
        denied HeadObject";

    let response = handle(
        &routing,
        &AuthConfig::Open,
        |_warehouse_id: &str| -> Result<Head<SharedObjects, SharedRefs>, String> {
            Err(sensitive.to_string())
        },
        request,
    );

    assert_eq!(status(&response), 500);

    let body: serde_json::Value = body_json(&response);
    let message = body["error"].as_str().expect("a JSON error body");
    assert_ne!(message, sensitive, "the detailed message must not be forwarded verbatim");
    assert!(!message.contains("forklift-prod-customer-42"), "{}", message);
    assert!(!message.contains("AKIAEXAMPLE"), "{}", message);
    assert!(!message.contains("8f1c2b"), "{}", message);
}

// -------------------------------------------------------------------------------------------
// Auth: the fail-closed default, the explicit opt-out, and a configured bearer token — the
// seam every other test in this file bypasses via `Fixture`'s `AuthConfig::Open` default.
// -------------------------------------------------------------------------------------------

/// Route one request through `handle` with an explicit `AuthConfig`, over a fresh empty store.
/// Every case here is decided by `authenticate` before the route even resolves, so which store
/// backs it is irrelevant.
fn call_with_auth(auth: &AuthConfig, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
    let routing = Routing::Single("wh".to_string());

    handle(
        &routing,
        auth,
        |_warehouse| {
            Ok(Head::new(
                SharedObjects(Arc::new(MemoryObjectStore::new())),
                SharedRefs(Arc::new(MemoryRefStore::new())),
            ))
        },
        request,
    )
}

/// The safe default: no token configured, no opt-out — every request is refused, and the
/// answer carries the `WWW-Authenticate` challenge RFC 6750 prescribes.
#[test]
fn closed_by_default_refuses_every_request_with_a_challenge() {
    let response = call_with_auth(&AuthConfig::Closed, get("/v1/warehouse"));

    assert_eq!(status(&response), 401);
    assert_eq!(
        response.headers().get(http::header::WWW_AUTHENTICATE).map(|v| v.to_str().unwrap()),
        Some("Bearer")
    );

    let body: serde_json::Value = body_json(&response);
    assert!(body.get("error").and_then(|v| v.as_str()).is_some(), "a JSON error body");
}

/// The explicit opt-out: no token, but `AuthConfig::Open` — everything passes untouched.
#[test]
fn the_open_opt_out_passes_every_request() {
    let response = call_with_auth(&AuthConfig::Open, get("/v1/warehouse"));
    assert_eq!(status(&response), 200);
}

/// A configured token: the correct bearer passes; a missing header, a wrong token, and an
/// equal-length near-miss all refuse identically.
#[test]
fn a_configured_token_gates_every_request() {
    let auth = AuthConfig::Token("secret".to_string());

    assert_eq!(
        status(&call_with_auth(&auth, get("/v1/warehouse"))),
        401,
        "no Authorization header at all"
    );

    let bearer = |token: &str| {
        Request::builder()
            .method("GET")
            .uri("/v1/warehouse")
            .header(http::header::AUTHORIZATION, format!("Bearer {}", token))
            .body(Vec::new())
            .unwrap()
    };

    assert_eq!(status(&call_with_auth(&auth, bearer("wrong"))), 401);
    assert_eq!(
        status(&call_with_auth(&auth, bearer("secrft"))),
        401,
        "an equal-length near miss must refuse"
    );
    assert_eq!(status(&call_with_auth(&auth, bearer("secret"))), 200);
}

// -------------------------------------------------------------------------------------------
// Harness: build a warehouse with the CLI, harvest its objects/refs. (A trimmed copy of
// protocol.rs's harness, which cannot be shared across test binaries.)
// -------------------------------------------------------------------------------------------

static AREA_COUNTER: AtomicU64 = AtomicU64::new(0);

struct Area {
    root: PathBuf,
}

impl Area {
    fn new(name: &str) -> Area {
        let unique = format!(
            "forklift-edge-test-{}-{}-{}",
            name,
            std::process::id(),
            AREA_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let root = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&root).expect("create the test area");
        Area { root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn forklift(&self, dir: &str, args: &[&str]) {
        let working = self.path(dir);
        std::fs::create_dir_all(&working).expect("create the working directory");

        let output = Command::new(forklift_binary())
            .args(args)
            .current_dir(&working)
            .env("FORKLIFT_GLOBAL_CONFIG", self.path("global.toml"))
            .env("FORKLIFT_KEYS_DIR", self.path("keys"))
            .output()
            .expect("run forklift");

        assert!(
            output.status.success(),
            "forklift {:?} failed: {}{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write_file(&self, relative: &str, content: &str) {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(path, content).expect("write file");
    }
}

impl Drop for Area {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn forklift_binary() -> PathBuf {
    let mut dir = std::env::current_exe().expect("the test binary has a path");
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let binary = dir.join(format!("forklift{}", std::env::consts::EXE_SUFFIX));
    assert!(
        binary.exists(),
        "forklift is not built at {}; run the suite via a workspace `cargo test`.",
        binary.display()
    );
    binary
}

struct Harvest {
    objects: HashMap<String, Vec<u8>>,
    signatures: HashMap<String, Vec<u8>>,
    refs: Vec<(pallet_utils::PalletRef, String)>,
    trust: Option<TrustAnchorDto>,
}

impl Harvest {
    fn head_of(&self, wire: &str) -> Option<String> {
        self.refs
            .iter()
            .find(|(pallet_ref, _)| pallet_ref.to_wire() == wire)
            .map(|(_, head)| head.clone())
    }
}

fn harvest(warehouse: &Path) -> Harvest {
    let objects_dir = warehouse.join(".forklift").join("objects");
    let mut object_hashes: Vec<String> = Vec::new();
    let mut signature_hashes: Vec<String> = Vec::new();

    for fan in std::fs::read_dir(&objects_dir).expect("read the objects dir") {
        let fan = fan.expect("read a fan entry");
        if !fan.file_type().expect("fan file type").is_dir() {
            continue;
        }
        let prefix = fan.file_name().to_string_lossy().to_string();

        for object in std::fs::read_dir(fan.path()).expect("read a fan folder") {
            let object = object.expect("read an object entry");
            let name = object.file_name().to_string_lossy().to_string();
            match name.strip_suffix(".sig") {
                Some(rest) => signature_hashes.push(format!("{}{}", prefix, rest)),
                None => object_hashes.push(format!("{}{}", prefix, name)),
            }
        }
    }

    let _scope = StorageRootScope::enter(warehouse);

    let mut objects = HashMap::new();
    for hash in object_hashes {
        objects.insert(hash.clone(), file_utils::retrieve_object_by_hash(&hash).expect("object"));
    }

    let mut signatures = HashMap::new();
    for hash in signature_hashes {
        let sidecar = sign_utils::load_raw_parcel_signature(&hash)
            .expect("load signature")
            .expect("signature present");
        signatures.insert(hash, sidecar);
    }

    let refs = pallet_utils::all_pallet_refs().expect("read refs");
    let trust = office_utils::read_trust_anchor()
        .expect("read trust")
        .map(|anchor| TrustAnchorDto::from(&anchor));

    Harvest { objects, signatures, refs, trust }
}

fn prepare(area: &Area, dir: &str) {
    area.forklift(dir, &["prepare"]);
    area.forklift(dir, &["config", "--global", "operator.name", "AWS Edge Tester"]);
    area.forklift(dir, &["config", "--global", "operator.identifier", "tester@forklift"]);
}
