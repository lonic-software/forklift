//! The HTTP edge of the AWS serverless head: one pure router mapping `REMOTE_PROTOCOL.md`
//! onto [`Head`] methods.
//!
//! [`handle`] is the whole control plane, and it is deliberately **synchronous and provider
//! independent**. It takes an [`http::Request`] whose body is already buffered (the Lambda
//! runtime buffers it before invoking us, exactly as `forklift-server`'s axum layer does),
//! parses the route, calls the matching `Head` method, and turns the provider-agnostic
//! outcome ([`Status`](crate::Status), a redirect URL, or bytes) into an [`http::Response`].
//! Nothing here says `lambda_http` or `axum`: the runtime adapter that owns those types is
//! the thin binary in `src/bin/`, which converts its request into this one, runs [`handle`]
//! on a blocking thread (`spawn_blocking` — every `Head` method blocks on
//! its store's futures), and converts the response back.
//!
//! That split is what makes the control plane testable without AWS: `tests/entrypoint.rs`
//! replays the whole protocol walk through [`handle`] against the in-memory fakes, and
//! `tests/aws_integration.rs` drives the same function against real S3 + DynamoDB. The
//! router never learns which store it is over.
//!
//! # Where the bytes go
//!
//! On an S3-backed head the object and bundle endpoints answer with a redirect to a presigned
//! storage URL: an object `GET` redirects to the canonical key and a staged `PUT` redirects to
//! `staging/{session}/{hash}` (never the hash key — invariant 1), both `307 Temporary Redirect`
//! since the client must replay the same method at the target. A `batch` bundle redirects to an
//! ephemeral response key with `303 See Other` instead — the target is always a presigned `GET`,
//! and the original request was a `POST`, so the redirect must tell the client to *switch*
//! methods rather than replay the `POST` (which a `307`/`308` would, failing signature
//! verification against a `GET`-only presigned URL). The control plane never carries object
//! bytes it can hand off, which is what keeps a Lambda inside its few-megabyte response limit.
//! The self-host equivalent (the fakes) serves those bytes inline, and [`handle`] answers both
//! shapes so one router serves both stores.
//!
//! # Multi-warehouse routing
//!
//! Like `forklift-server`'s `--root`/`--warehouses`, a deployment either pins one warehouse
//! ([`Routing::Single`], serving `/v1/…`) or serves many ([`Routing::Multi`], serving
//! `/warehouses/{id}/v1/…` with the id travelling inside the client's `remote.url`). The id
//! resolved here becomes the warehouse the per-request [`Head`] is built for — the DynamoDB
//! ref partition and the warm-scratch pool key, which must agree (see
//! [`Scratch::shared`](crate::scratch::Scratch::shared) — both must key on the same warehouse
//! id).

use http::{header, Method, Request, Response, StatusCode};
use serde::Serialize;
use subtle::ConstantTimeEq;

use forklift_core::model::remote::{
    CommitLiftRequest, ErrorResponse, MissingObjectsRequest, MissingObjectsResponse,
    RefUpdateRequest, ResolveResponse, TrustAnchorDto, UploadTargetsRequest, MAX_MISSING_BATCH,
    MAX_UPLOAD_TARGETS_BATCH,
};
use forklift_core::util::pallet_utils::DEFAULT_PALLET_NAME;

use crate::aws::AwsConfig;
use crate::error::{HeadError, HeadResult};
use crate::head::{BatchResult, Head, ObjectReadResult, ObjectWriteResult, TrustResult};
use crate::store::{ObjectStore, RefStore, SignatureOutcome};

/// How the deployment addresses warehouses, the serverless twin of `forklift-server`'s
/// `--root` (single) and `--warehouses` (multi) modes.
#[derive(Clone, Debug)]
pub enum Routing {
    /// One warehouse served at `/v1/…`; every request uses this fixed id.
    Single(String),

    /// Many warehouses served at `/warehouses/{id}/v1/…`; the id comes from the path.
    Multi,
}

/// Read the deployment configuration from the environment — the Lambda's only input besides
/// the request. Credentials are never read here: the SDK's default provider chain (the
/// execution role) resolves them, so nothing secret passes through.
///
/// Variables:
/// * `FORKLIFT_S3_BUCKET`     (required) — the object byte plane.
/// * `FORKLIFT_DYNAMODB_TABLE`(required) — the ref/trust consistency point.
/// * `FORKLIFT_WAREHOUSE_ID`  (optional) — set for single-warehouse serving (`/v1/…`);
///   unset selects multi-warehouse serving (`/warehouses/{id}/v1/…`).
/// * `FORKLIFT_AWS_ENDPOINT_URL` (optional) — an endpoint override for LocalStack/MinIO.
/// * `FORKLIFT_DEFAULT_PALLET`(optional) — the franchise default; defaults to `main`.
///
/// The region is deliberately absent: the provider chain reads `AWS_REGION`, so pinning it
/// here would only shadow it.
pub fn config_from_env() -> Result<(AwsConfig, Routing), String> {
    let bucket = require_env("FORKLIFT_S3_BUCKET")?;
    let table = require_env("FORKLIFT_DYNAMODB_TABLE")?;

    let (warehouse_id, routing) = match std::env::var("FORKLIFT_WAREHOUSE_ID") {
        Ok(id) if !id.is_empty() => (id.clone(), Routing::Single(id)),
        // No fixed id: the id travels in the path, so the config carries a placeholder the
        // per-request head overrides. The shared object store and the client builders never
        // read it (only the per-warehouse ref store and scratch pool do, and both are keyed
        // by the resolved id).
        _ => (String::new(), Routing::Multi),
    };

    let mut config = AwsConfig::new(bucket, table, warehouse_id);

    if let Ok(endpoint) = std::env::var("FORKLIFT_AWS_ENDPOINT_URL") {
        if !endpoint.is_empty() {
            config = config.with_endpoint_url(endpoint);
        }
    }

    let default_pallet = std::env::var("FORKLIFT_DEFAULT_PALLET")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_PALLET_NAME.to_string());
    config = config.with_default_pallet(default_pallet);

    Ok((config, routing))
}

/// Read a required environment variable, or explain which one is missing.
fn require_env(name: &str) -> Result<String, String> {
    std::env::var(name)
        .map_err(|_| format!("The {} environment variable must be set.", name))
        .and_then(|value| {
            if value.is_empty() {
                Err(format!("The {} environment variable must not be empty.", name))
            } else {
                Ok(value)
            }
        })
}

/// The transport-authentication configuration, resolved once at cold start ([`auth_from_env`])
/// and threaded into every request by [`handle`]. Full multi-tenant policy (resolving a bearer
/// to a principal, per-warehouse admission) is tracked privately and stays out of scope here —
/// this is the single-tenant seam: is *a* configured token present and correct at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthConfig {
    /// A configured bearer token; every request must present it via
    /// `Authorization: Bearer <token>`.
    Token(String),

    /// No token is configured and the operator has explicitly opted out
    /// (`FORKLIFT_OPEN_ACCESS=1`): every request passes untouched, mirroring
    /// `forklift-server`'s `Principal::Open`. Meant for LocalStack and local development —
    /// a real deployment sets `FORKLIFT_TOKEN` instead.
    Open,

    /// No token is configured and there is no explicit opt-out: the safe default. Every
    /// request is refused (`401`) — a forgotten `FORKLIFT_TOKEN` fails closed instead of
    /// silently serving the world from a public endpoint.
    Closed,
}

/// Read the auth configuration from the environment:
/// * `FORKLIFT_TOKEN` (optional) — the bearer token every request must present. An empty
///   value is treated exactly like an unset one (refuse), never as a valid empty token.
/// * `FORKLIFT_OPEN_ACCESS` (optional) — set to `1` to run with no token at all. Only takes
///   effect when `FORKLIFT_TOKEN` is unset; LocalStack and local dev use this, a real
///   deployment never should.
pub fn auth_from_env() -> AuthConfig {
    auth_from(std::env::var("FORKLIFT_TOKEN").ok(), std::env::var("FORKLIFT_OPEN_ACCESS").ok())
}

/// The pure decision behind [`auth_from_env`], split out so the matrix (empty-is-unset, the
/// opt-out, and token-takes-precedence-over-the-opt-out) is testable without touching real
/// process environment state.
fn auth_from(token: Option<String>, open_access: Option<String>) -> AuthConfig {
    match token.filter(|value| !value.is_empty()) {
        Some(token) => AuthConfig::Token(token),
        None if open_access.as_deref() == Some("1") => AuthConfig::Open,
        None => AuthConfig::Closed,
    }
}

/// Compare two bearer tokens without leaking which byte differs first: a naive `==` on `str`
/// short-circuits at the first mismatch, which is exactly the timing side channel a bearer
/// check must not have. The slice lengths are still compared up front (`subtle`'s own
/// short-circuit) — a token's length is not the secret, only its bytes are.
fn tokens_match(provided: &str, expected: &str) -> bool {
    bool::from(provided.as_bytes().ct_eq(expected.as_bytes()))
}

/// Strip the `Bearer` auth-scheme token case-insensitively (RFC 7235 §2.1: `auth-scheme` is a
/// `token`, and the scheme name itself is case-insensitive — `bearer`, `Bearer`, `BEARER` all
/// name the same scheme), leaving the credentials (the token after the scheme and its one
/// separating space) untouched: still compared case-sensitively, in constant time, by
/// [`tokens_match`]. Only the 7-byte scheme prefix is matched loosely; a non-`Bearer` scheme
/// (`Basic …`) still fails to match at all.
fn strip_bearer_prefix(value: &str) -> Option<&str> {
    const SCHEME_LEN: usize = "bearer ".len();

    let prefix = value.get(..SCHEME_LEN)?;

    if prefix.eq_ignore_ascii_case("bearer ") {
        value.get(SCHEME_LEN..)
    } else {
        None
    }
}

/// The `401` this seam answers on any failure — a missing header, a non-`Bearer` scheme, a
/// wrong token, or no token configured at all. The message is identical in every case: a
/// probing client must not be able to tell "you sent nothing" from "you sent the wrong thing"
/// from "this deployment isn't configured for auth", the same information a timing side
/// channel would otherwise leak by a different route.
fn unauthorized() -> HeadError {
    HeadError::unauthorized("A valid bearer token is required.")
}

/// The transport-authentication seam. Multi-tenant policy is tracked privately and is out
/// of scope here.
///
/// The protocol carries auth as `Authorization: Bearer <token>`; in the hosted deployment the
/// API Gateway authorizer in front of this function is an additional gate, and the office
/// roles the ref-update handler already consults decide *what* an authenticated caller may
/// move. This function decides the one thing prior to both: does the request carry the
/// configured bearer at all. [`AuthConfig::Open`] passes everything (the explicit local/
/// LocalStack opt-out); [`AuthConfig::Closed`] (no token configured, no opt-out) refuses
/// everything; [`AuthConfig::Token`] requires an exact, constant-time match.
fn authenticate<B>(auth: &AuthConfig, request: &Request<B>) -> HeadResult<()> {
    let expected = match auth {
        AuthConfig::Open => return Ok(()),
        AuthConfig::Closed => return Err(unauthorized()),
        AuthConfig::Token(expected) => expected,
    };

    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(strip_bearer_prefix);

    match provided {
        Some(token) if tokens_match(token, expected) => Ok(()),
        _ => Err(unauthorized()),
    }
}

/// Route and answer one request. The whole control plane: pure, synchronous, and generic over
/// the two stores so tests drive it against the fakes and the binary drives it over S3 +
/// DynamoDB.
///
/// `build_head` constructs the [`Head`] for the warehouse the request resolves to — a fixed
/// id in [`Routing::Single`], the path's `{id}` in [`Routing::Multi`]. It is called at most
/// once, only after the route parses, so an unroutable request never touches a store.
///
/// # Blocking contract
///
/// Every `Head` method blocks on its store's futures, so on a real deployment this must
/// run inside `tokio::task::spawn_blocking` — never on a runtime worker, where tokio refuses
/// to let a thread block. The binary honours that; the fakes never block, so tests call it
/// directly.
pub fn handle<O, R>(
    routing: &Routing,
    auth: &AuthConfig,
    build_head: impl FnOnce(&str) -> Result<Head<O, R>, String>,
    request: Request<Vec<u8>>,
) -> Response<Vec<u8>>
where
    O: ObjectStore,
    R: RefStore,
{
    if let Err(error) = authenticate(auth, &request) {
        return error_response(error);
    }

    let (warehouse_id, route) = match resolve_route(routing, &request) {
        Ok(resolved) => resolved,
        Err(error) => return error_response(error),
    };

    let head = match build_head(&warehouse_id) {
        Ok(head) => head,
        Err(message) => return error_response(HeadError::internal(message)),
    };

    match dispatch(&head, route, request.into_body()) {
        Ok(response) => response,
        Err(error) => error_response(error),
    }
}

/// A parsed route: the protocol endpoint plus the path/query pieces it carries. The body is
/// passed to [`dispatch`] separately (it is read only by the endpoints that need it).
enum Route {
    Handshake,
    Missing,
    UploadTargets,
    Batch,
    ObjectGet(String),
    ObjectPut { hash: String, session: Option<String> },
    SignatureGet(String),
    SignaturePut(String),
    PutTrust,
    RefUpdate(String),
    Resolve,
    BundleLatest,
    CommitLift(String),
}

/// Resolve the warehouse id and the endpoint of a request, stripping the `/warehouses/{id}`
/// prefix in multi mode. A path that matches nothing is a `404`; an invalid warehouse id is a
/// `422` (the same shape `forklift-server` gives).
fn resolve_route<B>(
    routing: &Routing,
    request: &Request<B>,
) -> HeadResult<(String, Route)> {
    let path = request.uri().path();
    let method = request.method();

    // Split into non-empty segments; the leading/trailing slashes drop out.
    let segments: Vec<&str> = path.split('/').filter(|segment| !segment.is_empty()).collect();

    let (warehouse_id, rest) = match routing {
        Routing::Single(id) => (id.clone(), segments.as_slice()),
        Routing::Multi => match segments.split_first() {
            Some((&"warehouses", tail)) => match tail.split_first() {
                Some((&id, endpoint)) => {
                    validate_warehouse_id(id)?;
                    (id.to_string(), endpoint)
                }
                None => return Err(not_found(path)),
            },
            _ => return Err(not_found(path)),
        },
    };

    let route = match_endpoint(method, rest, request.uri().query()).ok_or_else(|| not_found(path))?;

    Ok((warehouse_id, route))
}

/// Match the protocol endpoint (everything after the optional `/warehouses/{id}` prefix).
/// `None` for an unknown path/method pair, which the caller turns into a `404`.
fn match_endpoint(method: &Method, segments: &[&str], query: Option<&str>) -> Option<Route> {
    // The session-commit endpoint is the one the spec writes without a `/v1` prefix
    // (`POST /lift/{session}/commit`), while its Transport section says every endpoint lives
    // under `/v1`. The two disagree and no client speaks it yet, so accept both forms.
    if method == Method::POST {
        if let ["lift", session, "commit"] | ["v1", "lift", session, "commit"] = segments {
            return Some(Route::CommitLift((*session).to_string()));
        }
    }

    // Every other endpoint lives under `/v1`.
    let ["v1", rest @ ..] = segments else {
        return None;
    };

    match (method, rest) {
        (&Method::GET, ["warehouse"]) => Some(Route::Handshake),
        (&Method::POST, ["objects", "missing"]) => Some(Route::Missing),
        (&Method::POST, ["objects", "upload-targets"]) => Some(Route::UploadTargets),
        (&Method::POST, ["objects", "batch"]) => Some(Route::Batch),
        (&Method::GET, ["objects", hash]) => Some(Route::ObjectGet((*hash).to_string())),
        (&Method::PUT, ["objects", hash]) => Some(Route::ObjectPut {
            hash: (*hash).to_string(),
            session: query_param(query, "session"),
        }),
        (&Method::GET, ["signatures", hash]) => Some(Route::SignatureGet((*hash).to_string())),
        (&Method::PUT, ["signatures", hash]) => Some(Route::SignaturePut((*hash).to_string())),
        (&Method::PUT, ["trust"]) => Some(Route::PutTrust),
        (&Method::POST, ["pallets", name]) => Some(Route::RefUpdate((*name).to_string())),
        (&Method::POST, ["resolve"]) => Some(Route::Resolve),
        (&Method::GET, ["bundles", "latest"]) => Some(Route::BundleLatest),
        _ => None,
    }
}

/// The `422` for an `upload-targets` request over [`MAX_UPLOAD_TARGETS_BATCH`] — reads exactly
/// like `Head::reject_oversized_batch`, just against the smaller, response-shaped ceiling this
/// endpoint needs (see the constant's docs).
fn reject_oversized_upload_targets(count: usize) -> HeadResult<()> {
    if count > MAX_UPLOAD_TARGETS_BATCH {
        return Err(HeadError::unprocessable(format!(
            "At most {} hashes per upload-targets request (each answer carries a presigned URL, \
            not just a hash); batch larger sets.",
            MAX_UPLOAD_TARGETS_BATCH
        )));
    }

    Ok(())
}

/// The `422` for a `commit_lift` request whose `control_plane` and `blobs` lists together
/// exceed the protocol's shared batch cap. `Head::commit_lift` carries no such guard itself
/// (unlike `missing`/`upload_targets`/`batch`, each capped inside `Head` via its own
/// `reject_oversized_batch`), so the router enforces it here — combined across both lists,
/// since a request naming this many hashes at all is the failure mode worth capping, not
/// either list in isolation.
fn reject_oversized_commit(control_plane: usize, blobs: usize) -> HeadResult<()> {
    let total = control_plane + blobs;

    if total > MAX_MISSING_BATCH {
        return Err(HeadError::unprocessable(format!(
            "At most {} hashes per commit (control-plane objects plus blobs combined); commit \
            the lift session in smaller groups.",
            MAX_MISSING_BATCH
        )));
    }

    Ok(())
}

/// Map a signature-store outcome to its response status — split out from the `SignaturePut`
/// dispatch arm so the mapping is directly unit-testable: `Head::signature_put` currently turns
/// a conflicting sidecar into `Err(HeadError::conflict(..))` before this function ever sees
/// `SignatureOutcome::Conflict` (the `?` at the call site returns early there), so that arm is
/// unreachable through `Head` today. It still maps to `409`, not `200` — an invariant enforced
/// here rather than left to fall out of whichever variants happen to reach this match.
fn signature_put_status(outcome: SignatureOutcome) -> StatusCode {
    match outcome {
        SignatureOutcome::Created => StatusCode::CREATED,
        SignatureOutcome::AlreadyPresent => StatusCode::OK,
        SignatureOutcome::Conflict => StatusCode::CONFLICT,
    }
}

/// Call the `Head` method a route names and shape its outcome into a response. Returning a
/// [`HeadError`] here funnels every failure through the one status/JSON-body mapping.
fn dispatch<O, R>(head: &Head<O, R>, route: Route, body: Vec<u8>) -> HeadResult<Response<Vec<u8>>>
where
    O: ObjectStore,
    R: RefStore,
{
    match route {
        Route::Handshake => Ok(json_response(StatusCode::OK, &head.handshake()?)),

        Route::Missing => {
            let request: MissingObjectsRequest = parse_json(&body)?;
            let missing = head.missing(&request.hashes)?;
            Ok(json_response(StatusCode::OK, &MissingObjectsResponse { missing }))
        }

        Route::UploadTargets => {
            let request: UploadTargetsRequest = parse_json(&body)?;
            reject_oversized_upload_targets(request.hashes.len())?;
            let response = head.upload_targets(&request.session, &request.hashes)?;
            Ok(json_response(StatusCode::OK, &response))
        }

        Route::Batch => {
            let request: MissingObjectsRequest = parse_json(&body)?;
            match head.batch(&request.hashes)? {
                // A bundle is already a zstd stream; mark it `identity` so nothing re-wraps it.
                BatchResult::Bundle(bundle) => Ok(octet_stream(StatusCode::OK, bundle, true)),
                // `303`, not `307`/`308`: the redirect target is a presigned `GET`, but this
                // request was a `POST` — the client must switch methods, not replay it (see
                // `redirect`'s doc comment).
                BatchResult::Redirect(url) => Ok(redirect(&url, StatusCode::SEE_OTHER)),
            }
        }

        Route::ObjectGet(hash) => match head.object_get(&hash)? {
            ObjectReadResult::Bytes(bytes) => Ok(octet_stream(StatusCode::OK, bytes, false)),
            ObjectReadResult::Redirect(url) => Ok(redirect(&url, StatusCode::TEMPORARY_REDIRECT)),
        },

        Route::ObjectPut { hash, session } => {
            match head.object_put(session.as_deref(), &hash, &body)? {
                ObjectWriteResult::Stored { created: true } => Ok(empty(StatusCode::CREATED)),
                ObjectWriteResult::Stored { created: false } => Ok(empty(StatusCode::OK)),
                ObjectWriteResult::Redirect(url) =>
                    Ok(redirect(&url, StatusCode::TEMPORARY_REDIRECT)),
            }
        }

        Route::SignatureGet(hash) => {
            Ok(octet_stream(StatusCode::OK, head.signature_get(&hash)?, false))
        }

        Route::SignaturePut(hash) => {
            Ok(empty(signature_put_status(head.signature_put(&hash, &body)?)))
        }

        Route::PutTrust => {
            let anchor: TrustAnchorDto = parse_json(&body)?;
            match head.put_trust(&anchor)? {
                TrustResult::Established => Ok(empty(StatusCode::CREATED)),
                TrustResult::Unchanged => Ok(empty(StatusCode::OK)),
            }
        }

        Route::RefUpdate(name) => {
            let request: RefUpdateRequest = parse_json(&body)?;
            head.ref_update(&name, &request)?;
            Ok(empty(StatusCode::OK))
        }

        // Resolution is a display-only, server-mediated directory lookup (DESIGN.html §8.12).
        // The hosted service tiers names behind its own (privately tracked) policy; this head runs no
        // resolution hook, so — exactly as the protocol prescribes for a head without one — it
        // answers an empty map, and the client shows pseudonyms.
        Route::Resolve => Ok(json_response(
            StatusCode::OK,
            &ResolveResponse { names: std::collections::BTreeMap::new() },
        )),

        // A bundle builder is periodic ECS work (DESIGN.html §4.3/§4.6); until one runs
        // `bundle_latest` is a spec-compliant `404` (the `?` returns it) and clients fall back
        // to loose/batch fetches. When a builder ships, the bundle is a zstd stream, so it is
        // served `identity` like `batch`.
        Route::BundleLatest => Ok(octet_stream(StatusCode::OK, head.bundle_latest()?, true)),

        Route::CommitLift(session) => {
            let request: CommitLiftRequest = parse_json(&body)?;
            reject_oversized_commit(request.control_plane.len(), request.blobs.len())?;
            head.commit_lift(&session, &request.control_plane, &request.blobs, request.more)?;
            Ok(empty(StatusCode::OK))
        }
    }
}

/// Validate a warehouse id: a single safe path component, exactly `forklift-server`'s rule.
/// Stricter than a pallet name, because the id both keys the ref partition and (hashed) names
/// the warm-scratch directory.
fn validate_warehouse_id(id: &str) -> HeadResult<()> {
    let reject = |reason: &str| {
        Err(HeadError::unprocessable(format!("\"{}\" is not a valid warehouse id: {}.", id, reason)))
    };

    if id.is_empty() {
        return reject("it is empty");
    }

    if id.len() > 100 {
        return reject("it is longer than 100 characters");
    }

    if id.starts_with('.') || id.starts_with('-') {
        return reject("it must not start with \".\" or \"-\"");
    }

    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-') {
        return reject("only ASCII letters, digits, \".\", \"_\" and \"-\" are allowed");
    }

    Ok(())
}

/// Pull one `key=value` out of a raw query string. Values are simple (a lift session id, a
/// hex hash) so no percent-decoding is needed — the client never encodes them.
///
/// An empty or whitespace-only value (`?session=`) is treated the same as the parameter being
/// absent entirely, rather than as a real, empty session id: a blank session could never be
/// committed against (`staging//{hash}` is not a key `commit_lift` can promote), so a client
/// that sends one is routed to the same `422 SessionRequired` a missing parameter gets, instead
/// of a `307` that stages the upload somewhere it can never be promoted from.
fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    let value = query?.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then_some(value)
    })?;

    if value.trim().is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// The `404` for a path this head does not serve (an unknown route, a wrong method, or a
/// missing warehouse prefix in multi mode).
fn not_found(path: &str) -> HeadError {
    HeadError::not_found(format!("No route matches {}.", path))
}

/// Parse a JSON request body, mapping a malformed one to a `422` (the closest status in the
/// head's taxonomy — there is no `400`; a body the server cannot process is unprocessable).
fn parse_json<T: serde::de::DeserializeOwned>(body: &[u8]) -> HeadResult<T> {
    serde_json::from_slice(body)
        .map_err(|e| HeadError::unprocessable(format!("The request body is not valid JSON: {}", e)))
}

// -------------------------------------------------------------------------------------------
// Response shaping. Every response the head emits is one of: a JSON body, raw object/bundle
// bytes, a redirect to a storage URL (`307` or `303`, see `redirect`), an empty status, or a
// JSON error `{"error": …}`.
// -------------------------------------------------------------------------------------------

/// A JSON response with the given status.
fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response<Vec<u8>> {
    match serde_json::to_vec(body) {
        Ok(bytes) => Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(bytes)
            .expect("a valid JSON response"),
        // Serializing our own response types cannot fail in practice; surface it as a 500
        // rather than panicking in a request handler.
        Err(e) => error_response(HeadError::internal(format!(
            "Error while encoding the response: {}",
            e
        ))),
    }
}

/// Raw bytes as `application/octet-stream`. `identity_encoding` marks an already-compressed
/// payload (a bundle stream) so no transport layer re-wraps it — the same guard the server
/// head sets on `batch`/`bundle` responses.
fn octet_stream(status: StatusCode, bytes: Vec<u8>, identity_encoding: bool) -> Response<Vec<u8>> {
    let mut builder =
        Response::builder().status(status).header(header::CONTENT_TYPE, "application/octet-stream");

    if identity_encoding {
        builder = builder.header(header::CONTENT_ENCODING, "identity");
    }

    builder.body(bytes).expect("a valid octet-stream response")
}

/// A redirect to a presigned storage URL. The body is always empty by design — the bytes are
/// behind the `Location` — but the *status* depends on whether the client must replay its
/// original method at that URL or switch to `GET`:
///
/// * `307 Temporary Redirect` for an object `GET`/`PUT`: the target expects the same method,
///   and `307`/`308` are the only redirect statuses standard HTTP clients replay unchanged
///   (body included), which is exactly what a presigned PUT needs.
/// * `303 See Other` for the `batch` bundle: the original request is a `POST`, but the target
///   is always a presigned `GET` — replaying the `POST` there (what `307`/`308` would do) fails
///   signature verification, since SigV4 bakes the method into the signature. `303` is the
///   status built for precisely this "the response to your POST is over there, fetch it with
///   GET" redirect, and every mainstream client (including this protocol's own `reqwest`-based
///   one) follows it that way automatically.
fn redirect(url: &str, status: StatusCode) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(header::LOCATION, url)
        .body(Vec::new())
        .expect("a valid redirect response")
}

/// An empty-bodied status (a created/ok with nothing to return).
fn empty(status: StatusCode) -> Response<Vec<u8>> {
    Response::builder().status(status).body(Vec::new()).expect("a valid empty response")
}

/// A failed request as the protocol's JSON error body, at the status the head chose.
///
/// A `500`'s message is the one the client never sees as written. `Head`'s internal errors wrap
/// raw storage failures (an SDK `DisplayErrorContext` can carry a request id, a bucket or table
/// name), and this router sits behind the hosted service's public edge. `forklift-server`
/// forwards its `500` messages verbatim, and that is fine there — a self-host head only ever
/// runs on the operator's own infrastructure — but this head must not hand a stranger's bucket
/// name to whoever happens to be asking. The detail is logged instead (Lambda ships stderr to
/// CloudWatch) and the client gets a generic message. Every other status is the protocol's own
/// diagnostic (a bad hash, a stale ref, a malformed body) and stays as-is — it is meant to be
/// read.
fn error_response(error: HeadError) -> Response<Vec<u8>> {
    // The head's [`Status`] numeric values *are* the protocol's HTTP status codes, so this is
    // the single point its taxonomy meets `http`.
    let status = StatusCode::from_u16(error.status.as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
        eprintln!("forklift-aws-lambda: internal error: {}", error.message);
        "An internal error occurred.".to_string()
    } else {
        error.message
    };

    let body = serde_json::to_vec(&ErrorResponse { error: message })
        .unwrap_or_else(|_| b"{\"error\":\"internal error\"}".to_vec());

    let mut builder =
        Response::builder().status(status).header(header::CONTENT_TYPE, "application/json");

    if status == StatusCode::UNAUTHORIZED {
        // RFC 6750 §3: a resource server refusing a bearer-authenticated request challenges
        // with this header so a well-behaved client knows which scheme to retry.
        builder = builder.header(header::WWW_AUTHENTICATE, "Bearer");
    }

    builder.body(body).expect("a valid error response")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `query_param` treats a missing, empty, or whitespace-only value as absent — the fix for
    /// the review finding that `?session=` presigned a `staging//{hash}` key nothing could ever
    /// commit against. A non-empty value (including one with embedded whitespace) still round
    /// trips untouched, and no percent-decoding happens (the client never encodes these values).
    #[test]
    fn query_param_treats_empty_and_whitespace_values_as_absent() {
        assert_eq!(query_param(Some("session=abc"), "session"), Some("abc".to_string()));
        assert_eq!(query_param(Some("session="), "session"), None, "an empty value is absent");
        assert_eq!(
            query_param(Some("session=   "), "session"),
            None,
            "a whitespace-only value is absent"
        );
        assert_eq!(
            query_param(Some("session=%20"), "session"),
            Some("%20".to_string()),
            "no percent-decoding: a literal three-character value is not empty"
        );
        assert_eq!(query_param(Some("other=abc"), "session"), None, "a different key");
        assert_eq!(query_param(Some("session=abc&other=x"), "session"), Some("abc".to_string()));
        assert_eq!(query_param(None, "session"), None, "no query string at all");
    }

    /// The status mapping stays a `409` for a conflicting signature even though `Head` never
    /// actually returns `Ok(SignatureOutcome::Conflict)` today (see the function's own doc) —
    /// tested directly since the router can never observe that variant through `Head`.
    #[test]
    fn signature_put_status_maps_conflict_to_409_even_though_head_never_returns_it() {
        assert_eq!(signature_put_status(SignatureOutcome::Created), StatusCode::CREATED);
        assert_eq!(signature_put_status(SignatureOutcome::AlreadyPresent), StatusCode::OK);
        assert_eq!(signature_put_status(SignatureOutcome::Conflict), StatusCode::CONFLICT);
    }

    /// The router-level caps reject at the documented ceilings and accept exactly at them.
    #[test]
    fn the_router_level_batch_caps_are_enforced_at_their_documented_ceilings() {
        assert!(reject_oversized_upload_targets(MAX_UPLOAD_TARGETS_BATCH).is_ok());
        let error = reject_oversized_upload_targets(MAX_UPLOAD_TARGETS_BATCH + 1)
            .expect_err("over the cap");
        assert_eq!(error.status, crate::error::Status::Unprocessable);
        assert!(error.message.contains(&MAX_UPLOAD_TARGETS_BATCH.to_string()), "{}", error.message);

        assert!(reject_oversized_commit(MAX_MISSING_BATCH, 0).is_ok());
        assert!(reject_oversized_commit(MAX_MISSING_BATCH / 2, MAX_MISSING_BATCH / 2).is_ok());
        let error =
            reject_oversized_commit(MAX_MISSING_BATCH, 1).expect_err("over the combined cap");
        assert_eq!(error.status, crate::error::Status::Unprocessable);
        assert!(error.message.contains(&MAX_MISSING_BATCH.to_string()), "{}", error.message);
    }

    // ---------------------------------------------------------------------------------
    // Auth: AuthConfig resolution, constant-time token comparison, the authenticate() seam.
    // ---------------------------------------------------------------------------------

    fn request_with_bearer(token: &str) -> Request<()> {
        Request::builder()
            .header(header::AUTHORIZATION, format!("Bearer {}", token))
            .body(())
            .unwrap()
    }

    fn request_with_no_header() -> Request<()> {
        Request::builder().body(()).unwrap()
    }

    /// An empty `FORKLIFT_TOKEN` is treated exactly like an unset one — never a valid empty
    /// token — and the opt-out only takes effect (and only for the literal `"1"`) when there
    /// is no real token at all: a token, once configured, always wins.
    #[test]
    fn auth_from_env_treats_an_empty_token_as_unset_and_only_the_opt_out_falls_back_to_open() {
        assert_eq!(
            auth_from(Some("secret".to_string()), None),
            AuthConfig::Token("secret".to_string())
        );
        assert_eq!(
            auth_from(Some("".to_string()), None),
            AuthConfig::Closed,
            "an empty token with no opt-out is closed, not a valid empty token"
        );
        assert_eq!(auth_from(None, None), AuthConfig::Closed, "no token, no opt-out: closed by default");
        assert_eq!(auth_from(None, Some("1".to_string())), AuthConfig::Open, "the explicit opt-out");
        assert_eq!(
            auth_from(Some("".to_string()), Some("1".to_string())),
            AuthConfig::Open,
            "an empty token is unset, so the opt-out still applies"
        );
        assert_eq!(
            auth_from(Some("secret".to_string()), Some("1".to_string())),
            AuthConfig::Token("secret".to_string()),
            "a real token always wins over the opt-out"
        );
        assert_eq!(
            auth_from(None, Some("yes".to_string())),
            AuthConfig::Closed,
            "only the literal \"1\" opts out"
        );
    }

    /// Pins the equality semantics a timing side channel can't be asserted in a unit test:
    /// an exact match passes, and near-misses of every length relationship (equal, shorter,
    /// longer) refuse — including the equal-length case, which is exactly what would slip
    /// through a broken constant-time comparison that always agreed on length.
    #[test]
    fn tokens_match_requires_an_exact_byte_for_byte_match() {
        assert!(tokens_match("secret", "secret"));
        assert!(!tokens_match("secret", "secrft"), "a same-length near-miss must refuse");
        assert!(!tokens_match("secret", "secre"), "a shorter guess must refuse");
        assert!(!tokens_match("secret", "secrets"), "a longer guess must refuse");
        assert!(!tokens_match("", "secret"));
        assert!(tokens_match("", ""));
    }

    #[test]
    fn authenticate_closed_refuses_every_request() {
        let auth = AuthConfig::Closed;
        assert_eq!(
            authenticate(&auth, &request_with_no_header()).unwrap_err().status,
            crate::error::Status::Unauthorized
        );
        assert_eq!(
            authenticate(&auth, &request_with_bearer("anything")).unwrap_err().status,
            crate::error::Status::Unauthorized
        );
    }

    #[test]
    fn authenticate_open_passes_every_request() {
        let auth = AuthConfig::Open;
        assert!(authenticate(&auth, &request_with_no_header()).is_ok());
        assert!(authenticate(&auth, &request_with_bearer("anything")).is_ok());
    }

    #[test]
    fn authenticate_token_requires_the_exact_bearer() {
        let auth = AuthConfig::Token("secret".to_string());

        assert!(authenticate(&auth, &request_with_bearer("secret")).is_ok());
        assert!(authenticate(&auth, &request_with_no_header()).is_err());
        assert!(authenticate(&auth, &request_with_bearer("wrong")).is_err());
        assert!(
            authenticate(&auth, &request_with_bearer("secrft")).is_err(),
            "an equal-length near miss must refuse"
        );

        let non_bearer =
            Request::builder().header(header::AUTHORIZATION, "Basic secret").body(()).unwrap();
        assert!(authenticate(&auth, &non_bearer).is_err(), "a non-Bearer scheme must refuse");
    }

    /// RFC 7235 §2.1: `auth-scheme` is case-insensitive, so `bearer`/`BEARER`/mixed-case must
    /// authenticate exactly like `Bearer`. The credential (token) half of the header is a
    /// different story — it stays case-sensitive, so a same-length, wrong-case token must still
    /// refuse.
    #[test]
    fn authenticate_accepts_the_bearer_scheme_case_insensitively() {
        let auth = AuthConfig::Token("secret".to_string());

        for scheme in ["bearer", "Bearer", "BEARER", "BeArEr"] {
            let request = Request::builder()
                .header(header::AUTHORIZATION, format!("{scheme} secret"))
                .body(())
                .unwrap();
            assert!(authenticate(&auth, &request).is_ok(), "scheme {} must authenticate", scheme);
        }

        // The token itself is still case-sensitive.
        let wrong_case_token =
            Request::builder().header(header::AUTHORIZATION, "Bearer SECRET").body(()).unwrap();
        assert!(
            authenticate(&auth, &wrong_case_token).is_err(),
            "the token's case must still matter"
        );
    }

    /// The seam never distinguishes "no token configured" from "wrong token" — same status,
    /// same message — so a client probing a deployment learns nothing either way.
    #[test]
    fn closed_and_a_wrong_token_answer_the_identical_401() {
        let closed_error = authenticate(&AuthConfig::Closed, &request_with_no_header()).unwrap_err();
        let wrong_token_error = authenticate(
            &AuthConfig::Token("secret".to_string()),
            &request_with_bearer("wrong"),
        ).unwrap_err();

        assert_eq!(closed_error.status, wrong_token_error.status);
        assert_eq!(closed_error.message, wrong_token_error.message);
    }
}
