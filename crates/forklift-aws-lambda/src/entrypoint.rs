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

use std::collections::HashMap;

use http::{header, Method, Request, Response, StatusCode};
use percent_encoding::percent_decode_str;
use serde::Serialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use forklift_core::model::remote::{
    CommitLiftRequest, ErrorResponse, HoldsRequest, HoldsResponse, MissingObjectsRequest,
    MissingObjectsResponse, RefUpdateRequest,
    ResolveRequest, ResolveResponse, TrustAnchorDto, UploadTargetsRequest, MAX_MISSING_BATCH,
    MAX_UPLOAD_TARGETS_BATCH, distinct_resolve_identifiers, LIFT_COMMIT_HEADER, LIFT_COMMIT_NOT_READY,
};
use forklift_core::util::authority_utils::Principal;
use forklift_core::util::object_utils::is_object_hash;
use forklift_core::util::pallet_utils::{validate_pallet_name, DEFAULT_PALLET_NAME};
use forklift_core::util::sign_utils;
use forklift_core::util::warehouse_utils;

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
    config_from(|name| std::env::var(name).ok())
}

/// [`config_from_env`] over any variable source. The two names a client must also accept, the
/// single-routing warehouse id and the default pallet, are validated here, so a misconfigured head
/// refuses to start rather than serving names every client refuses.
fn config_from(var: impl Fn(&str) -> Option<String>) -> Result<(AwsConfig, Routing), String> {
    let bucket = require_var(&var, "FORKLIFT_S3_BUCKET")?;
    let table = require_var(&var, "FORKLIFT_DYNAMODB_TABLE")?;

    let (warehouse_id, routing) = match var("FORKLIFT_WAREHOUSE_ID") {
        Some(id) if !id.is_empty() => {
            warehouse_utils::validate_warehouse_id(&id)
                .map_err(|e| format!("The FORKLIFT_WAREHOUSE_ID environment variable is invalid: {}", e))?;
            (id.clone(), Routing::Single(id))
        }
        // No fixed id: the id travels in the path, so the config carries a placeholder the
        // per-request head overrides. The shared object store and the client builders never
        // read it (only the per-warehouse ref store and scratch pool do, and both are keyed
        // by the resolved id).
        _ => (String::new(), Routing::Multi),
    };

    let mut config = AwsConfig::new(bucket, table, warehouse_id);

    if let Some(endpoint) = var("FORKLIFT_AWS_ENDPOINT_URL") {
        if !endpoint.is_empty() {
            config = config.with_endpoint_url(endpoint);
        }
    }

    let default_pallet = var("FORKLIFT_DEFAULT_PALLET")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_PALLET_NAME.to_string());
    validate_pallet_name(&default_pallet)
        .map_err(|e| format!("The FORKLIFT_DEFAULT_PALLET environment variable is invalid: {}", e))?;
    config = config.with_default_pallet(default_pallet);

    Ok((config, routing))
}

/// Read a required variable, or explain which one is missing.
fn require_var(var: &impl Fn(&str) -> Option<String>, name: &str) -> Result<String, String> {
    match var(name) {
        None => Err(format!("The {} environment variable must be set.", name)),
        Some(value) if value.is_empty() => Err(format!("The {} environment variable must not be empty.", name)),
        Some(value) => Ok(value),
    }
}

/// The SHA-256 digest of a bearer token: the only form in which this head holds a credential.
///
/// A Lambda's environment is part of the function's configuration, readable by whoever may read
/// that configuration, so the function never holds a plaintext token to compare against. A
/// deployer computes the digest with any common tool (`shasum -a 256`, `openssl dgst -sha256`,
/// terraform's `sha256()`); tokens are high-entropy, so an unsalted digest gives nothing away.
///
/// Neither constructor yields the digest of the empty string: a configuration holding it would
/// admit a request with a blank bearer, were [`authenticate`] ever to hash one.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenDigest([u8; 32]);

impl TokenDigest {
    /// Parse a configured digest: exactly 64 lowercase hex characters, never the digest of the
    /// empty string.
    pub fn parse_hex(hex: &str) -> Result<TokenDigest, String> {
        // An object hash's spelling is exactly a SHA-256 digest's: 64 lowercase hex characters.
        let malformed = || "a token digest must be 64 lowercase hex characters (a SHA-256 digest)".to_string();

        if !is_object_hash(hex) {
            return Err(malformed());
        }

        let bytes = sign_utils::from_hex(hex).map_err(|()| malformed())?;
        let digest = TokenDigest(bytes.try_into().map_err(|_| malformed())?);
        if digest == TokenDigest::hash("") {
            return Err("a token digest must not be the SHA-256 of the empty string".to_string());
        }

        Ok(digest)
    }

    /// The digest of a token, refused for an empty or whitespace-only token: the same rule
    /// [`authenticate`] applies to a presented bearer, and `forklift-server` to its own static
    /// token.
    pub fn of_token(token: &str) -> Result<TokenDigest, String> {
        if token.trim().is_empty() {
            return Err("a bearer token must not be empty or whitespace-only".to_string());
        }

        Ok(TokenDigest::hash(token))
    }

    fn hash(token: &str) -> TokenDigest {
        TokenDigest(Sha256::digest(token.as_bytes()).into())
    }
}

impl std::fmt::Debug for TokenDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TokenDigest(<redacted>)")
    }
}

/// The credentials a deployment accepts: the shared bearer, per-operator tokens, or both — never
/// neither, which is what [`Credentials::new`] refuses.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    /// The shared bearer's digest. Its holder is [`Principal::Static`]: the deployment's own
    /// administrator credential, full access under every office rule.
    static_token: Option<TokenDigest>,

    /// Per-operator token digests, each bound to an office identifier. Identifiers are kept
    /// untrimmed: the office rules match them by exact equality against the roster.
    operators: HashMap<TokenDigest, String>,
}

impl Credentials {
    /// Assemble the accepted credentials. Refused when there are none at all, when an
    /// identifier is blank, and when one digest is bound twice — among the operators, or as both
    /// the shared bearer and an operator, where the shared bearer's match would silently make
    /// that operator an administrator.
    pub fn new(static_token: Option<TokenDigest>,
               operators: Vec<(TokenDigest, String)>) -> Result<Credentials, String> {
        if static_token.is_none() && operators.is_empty() {
            return Err("no credential is configured".to_string());
        }

        let mut bound = HashMap::new();
        for (digest, identifier) in operators {
            if identifier.trim().is_empty() {
                return Err("an operator token is bound to a blank identifier".to_string());
            }

            if static_token == Some(digest) {
                return Err("an operator token's digest is also the shared bearer's".to_string());
            }

            if bound.insert(digest, identifier).is_some() {
                return Err("one operator token digest is bound more than once".to_string());
            }
        }

        Ok(Credentials { static_token, operators: bound })
    }
}

/// The transport-authentication configuration, resolved once at cold start ([`auth_from_env`])
/// and threaded into every request by [`handle`]. `Debug` is hand-written so a digest or an
/// identifier never reaches a log line.
#[derive(Clone, PartialEq, Eq)]
pub enum AuthConfig {
    /// Credentials are configured; every request must present one via
    /// `Authorization: Bearer <token>`.
    Credentials(Credentials),

    /// No credential is configured and the operator has explicitly opted out
    /// (`FORKLIFT_OPEN_ACCESS=1`): every request passes as [`Principal::Open`], mirroring
    /// `forklift-server` with no token configured. Meant for LocalStack and local development.
    Open,

    /// No credential is configured and there is no explicit opt-out: the safe default. Every
    /// request is refused (`401`), so a forgotten credential fails closed instead of serving the
    /// world from a public endpoint.
    Closed,
}

impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthConfig::Credentials(credentials) => f
                .debug_struct("Credentials")
                .field("static_token", &credentials.static_token.is_some())
                .field("operators", &credentials.operators.len())
                .finish(),
            AuthConfig::Open => write!(f, "Open"),
            AuthConfig::Closed => write!(f, "Closed"),
        }
    }
}

/// Read the auth configuration from the environment. A misconfiguration is an `Err`, which
/// fails the cold start: a head never runs on a credential set it half understood.
/// * `FORKLIFT_TOKEN_SHA256` (optional) — the shared bearer's digest ([`TokenDigest`]).
/// * `FORKLIFT_OPERATOR_TOKENS` (optional) — a JSON object mapping each per-operator token's
///   digest to its office identifier.
/// * `FORKLIFT_OPEN_ACCESS` (optional) — `1` runs with no credential at all. Refused beside
///   any credential.
/// * `FORKLIFT_TOKEN` — never read as a credential: its presence is refused, so a plaintext
///   token left over from an older configuration is never silently ignored.
pub fn auth_from_env() -> Result<AuthConfig, String> {
    auth_from(|name| std::env::var(name).ok())
}

/// Refuse credentials the routing can never use: in [`Routing::Multi`] only the shared bearer
/// (or an open head) sets a warehouse up (see [`handle`]), so operator tokens alone would leave
/// every warehouse id read-only for good. Checked at cold start, beside the two readers.
pub fn check_auth_fits_routing(auth: &AuthConfig, routing: &Routing) -> Result<(), String> {
    match (auth, routing) {
        (AuthConfig::Credentials(credentials), Routing::Multi) if credentials.static_token.is_none() => Err(
            "Multi-warehouse routing (no FORKLIFT_WAREHOUSE_ID) needs FORKLIFT_TOKEN_SHA256: only the \
            shared bearer sets a warehouse up, so operator tokens alone could never write anywhere."
                .to_string(),
        ),
        _ => Ok(()),
    }
}

/// The pure decision behind [`auth_from_env`], over any variable source.
fn auth_from(var: impl Fn(&str) -> Option<String>) -> Result<AuthConfig, String> {
    if var("FORKLIFT_TOKEN").is_some() {
        return Err("The FORKLIFT_TOKEN environment variable is set, but this function holds no \
            plaintext credential: set FORKLIFT_TOKEN_SHA256 to the token's SHA-256 digest \
            (64 lowercase hex characters) and remove FORKLIFT_TOKEN."
            .to_string());
    }

    let static_token = var("FORKLIFT_TOKEN_SHA256")
        .map(|hex| TokenDigest::parse_hex(&hex))
        .transpose()
        .map_err(|e| format!("The FORKLIFT_TOKEN_SHA256 environment variable is invalid: {}.", e))?;

    let operators = match var("FORKLIFT_OPERATOR_TOKENS") {
        Some(json) => parse_operator_tokens(&json)
            .map_err(|e| format!("The FORKLIFT_OPERATOR_TOKENS environment variable is invalid: {}.", e))?,
        None => Vec::new(),
    };

    let open_access = var("FORKLIFT_OPEN_ACCESS").as_deref() == Some("1");

    if static_token.is_none() && operators.is_empty() {
        return Ok(if open_access { AuthConfig::Open } else { AuthConfig::Closed });
    }

    if open_access {
        return Err("FORKLIFT_OPEN_ACCESS=1 is set beside a configured credential. Open access \
            is only for a deployment with no credential at all; unset one or the other."
            .to_string());
    }

    Credentials::new(static_token, operators)
        .map(AuthConfig::Credentials)
        .map_err(|e| format!("The configured credentials are invalid: {}.", e))
}

/// Parse `FORKLIFT_OPERATOR_TOKENS`: a JSON object of digest → identifier. The entries are read
/// in order, duplicates kept, so [`Credentials::new`] can refuse a digest bound twice rather
/// than a JSON parser silently keeping the last binding.
fn parse_operator_tokens(json: &str) -> Result<Vec<(TokenDigest, String)>, String> {
    struct Entries(Vec<(String, String)>);

    impl<'de> serde::Deserialize<'de> for Entries {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Entries, D::Error> {
            struct Visitor;

            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = Entries;

                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    write!(f, "an object mapping token digests to office identifiers")
                }

                fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Entries, A::Error> {
                    let mut entries = Vec::new();
                    while let Some(entry) = map.next_entry::<String, String>()? {
                        entries.push(entry);
                    }
                    Ok(Entries(entries))
                }
            }

            deserializer.deserialize_map(Visitor)
        }
    }

    let Entries(entries) = serde_json::from_str(json).map_err(|e| format!("not a JSON object of strings ({})", e))?;

    entries
        .into_iter()
        .map(|(digest, identifier)| Ok((TokenDigest::parse_hex(&digest)?, identifier)))
        .collect()
}

/// Strip the `Bearer` auth-scheme token case-insensitively (RFC 7235 §2.1: `auth-scheme` is a
/// `token`, and the scheme name itself is case-insensitive — `bearer`, `Bearer`, `BEARER` all
/// name the same scheme), leaving the credentials (the token after the scheme and its one
/// separating space) untouched. Only the 7-byte scheme prefix is matched loosely; a
/// non-`Bearer` scheme (`Basic …`) still fails to match at all.
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
/// blank or unknown token, or no credential configured at all. The message is identical in
/// every case: a probing client must not be able to tell "you sent nothing" from "you sent the
/// wrong thing" from "this deployment isn't configured for auth".
fn unauthorized() -> HeadError {
    HeadError::unauthorized("A valid bearer token is required.")
}

/// The transport-authentication seam: who is calling. [`AuthConfig::Open`] passes everything as
/// [`Principal::Open`]; [`AuthConfig::Closed`] refuses everything. With credentials configured,
/// a blank bearer is refused before anything is hashed; otherwise the bearer's digest is
/// matched against the shared bearer's in constant time first ([`Principal::Static`]), then
/// looked up among the operator tokens ([`Principal::Operator`]), the same order
/// `forklift-server` resolves its tokens in. A lookup leaks timing only about the digest.
///
/// What the principal may do is not decided here. The office authority rules
/// (`forklift_core::util::authority_utils`) decide it at each mutating [`Head`] method, against
/// the office state that method reads; the content-level audit in [`Head::ref_update`] runs on
/// top of them for every principal.
fn authenticate<B>(auth: &AuthConfig, request: &Request<B>) -> HeadResult<Principal> {
    let credentials = match auth {
        AuthConfig::Open => return Ok(Principal::Open),
        AuthConfig::Closed => return Err(unauthorized()),
        AuthConfig::Credentials(credentials) => credentials,
    };

    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(strip_bearer_prefix)
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(unauthorized)?;

    let digest = TokenDigest::hash(provided);

    if let Some(expected) = &credentials.static_token {
        if bool::from(digest.0.ct_eq(&expected.0)) {
            return Ok(Principal::Static);
        }
    }

    match credentials.operators.get(&digest) {
        Some(identifier) => Ok(Principal::Operator(identifier.clone())),
        None => Err(unauthorized()),
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
    let caller = match authenticate(auth, &request) {
        Ok(caller) => caller,
        Err(error) => return error_response(error),
    };

    let (warehouse_id, route) = match resolve_route(routing, &request) {
        Ok(resolved) => resolved,
        Err(error) => return error_response(error),
    };

    let head = match build_head(&warehouse_id) {
        Ok(head) => head,
        Err(message) => return error_response(HeadError::internal(message)),
    };

    if let Err(error) = require_set_up_for(routing, &head, &caller, &route) {
        return error_response(error);
    }

    match dispatch(&head, &caller, route, request.into_body()) {
        Ok(response) => response,
        Err(error) => error_response(error),
    }
}

/// In multi-warehouse routing every well-formed id answers, so a warehouse with no trust anchor
/// is one nobody has set up. An operator token may not write to it — nor establish its trust,
/// which would hand the id to whoever held any token. Setting a warehouse up is the shared
/// bearer's (or an open head's), as creating one is the static token's on `forklift-server`.
/// A single-warehouse head serves the one warehouse its deployer configured, and there the
/// office rules alone decide. An anchor, once present, is never removed (a re-genesis replaces
/// it), so the warehouse cannot fall back out of this check before the route commits.
fn require_set_up_for<O, R>(routing: &Routing, head: &Head<O, R>, caller: &Principal, route: &Route)
    -> HeadResult<()>
where
    O: ObjectStore,
    R: RefStore,
{
    let writes = matches!(
        route,
        Route::UploadTargets
            | Route::ObjectPut { .. }
            | Route::SignaturePut(_)
            | Route::PutTrust
            | Route::RefUpdate(_)
            | Route::CommitLift(_)
    );

    if !matches!(routing, Routing::Multi) || !writes || !matches!(caller, Principal::Operator(_)) {
        return Ok(());
    }

    if head.refs.get_trust().map_err(HeadError::internal)?.is_none() {
        return Err(HeadError::forbidden(
            "This warehouse has not been set up: the deployment's administrator establishes its \
            trust first, and only then may an operator token write to it.",
        ));
    }

    Ok(())
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
    Holds(String),
    Resolve,
    BundleLatest,
    CommitLift(String),
}

/// Resolve the warehouse id and the endpoint of a request, stripping the `/warehouses/{id}`
/// prefix in multi mode. A path that matches nothing is a `404`; an invalid warehouse id is a
/// `422` (the same shape `forklift-server` gives), as is a captured segment that does not
/// percent-decode to UTF-8 (see [`param`]).
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
                    let id = param(id)?;
                    validate_warehouse_id(&id)?;
                    (id, endpoint)
                }
                None => return Err(not_found(path)),
            },
            _ => return Err(not_found(path)),
        },
    };

    let route = match_endpoint(method, rest, request.uri().query())?.ok_or_else(|| not_found(path))?;

    Ok((warehouse_id, route))
}

/// Match the protocol endpoint (everything after the optional `/warehouses/{id}` prefix).
/// `None` for an unknown path/method pair, which the caller turns into a `404`; a captured
/// segment or query value that does not percent-decode to UTF-8 is a `422`.
fn match_endpoint(method: &Method, segments: &[&str], query: Option<&str>) -> HeadResult<Option<Route>> {
    // The session-commit endpoint is the one the spec writes without a `/v1` prefix
    // (`POST /lift/{session}/commit`), while its Transport section says every endpoint lives
    // under `/v1`. The client posts the `/v1` form; the bare one is accepted as the spec writes it.
    if method == Method::POST {
        if let ["lift", session, "commit"] | ["v1", "lift", session, "commit"] = segments {
            return Ok(Some(Route::CommitLift(session_id(&param(session)?)?)));
        }
    }

    // Every other endpoint lives under `/v1`.
    let ["v1", rest @ ..] = segments else {
        return Ok(None);
    };

    Ok(match (method, rest) {
        (&Method::GET, ["warehouse"]) => Some(Route::Handshake),
        (&Method::POST, ["objects", "missing"]) => Some(Route::Missing),
        (&Method::POST, ["objects", "upload-targets"]) => Some(Route::UploadTargets),
        (&Method::POST, ["objects", "batch"]) => Some(Route::Batch),
        (&Method::GET, ["objects", hash]) => Some(Route::ObjectGet(param(hash)?)),
        (&Method::PUT, ["objects", hash]) => Some(Route::ObjectPut {
            hash: param(hash)?,
            session: query_param(query, "session")?.as_deref().map(session_id).transpose()?,
        }),
        (&Method::GET, ["signatures", hash]) => Some(Route::SignatureGet(param(hash)?)),
        (&Method::PUT, ["signatures", hash]) => Some(Route::SignaturePut(param(hash)?)),
        (&Method::PUT, ["trust"]) => Some(Route::PutTrust),
        (&Method::POST, ["pallets", name]) => Some(Route::RefUpdate(param(name)?)),
        (&Method::POST, ["pallets", name, "holds"]) => Some(Route::Holds(param(name)?)),
        (&Method::POST, ["resolve"]) => Some(Route::Resolve),
        (&Method::GET, ["bundles", "latest"]) => Some(Route::BundleLatest),
        _ => None,
    })
}

/// A captured path parameter, percent-decoded as `forklift-server`'s `Path` extractor decodes
/// it. The path is split into segments first, so an encoded `/` stays inside its segment: a
/// pallet named `feature/x` arrives as the one segment `feature%2Fx`. A segment that does not
/// decode to UTF-8 is a `422`, this head's status for a request it cannot process
/// (`forklift-server` answers it `400`).
fn param(segment: &str) -> HeadResult<String> {
    percent_decode_str(segment).decode_utf8().map(|value| value.into_owned()).map_err(|_| {
        HeadError::unprocessable(format!(
            "The path segment {} is not UTF-8 once percent-decoded.", segment))
    })
}

/// A lift session id, as decoded from the path or the query: ASCII alphanumerics and `-` only,
/// as the client's hyphenated UUIDs are. The id becomes the staging prefix `staging/{session}/`,
/// so a `/` in one would put another session's keys under its sweep; anything else is a `422`.
fn session_id(session: &str) -> HeadResult<String> {
    if session.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-') {
        Ok(session.to_string())
    } else {
        Err(HeadError::unprocessable(format!(
            "The lift session {:?} is not a session id (ASCII letters, digits and '-').", session
        )))
    }
}

/// The `422` for an `upload-targets` request over [`MAX_UPLOAD_TARGETS_BATCH`] — reads exactly
/// like `Head::reject_malformed_batch`, just against the smaller, response-shaped ceiling this
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
/// `reject_malformed_batch`), so the router enforces it here — combined across both lists,
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
fn dispatch<O, R>(head: &Head<O, R>, caller: &Principal, route: Route, body: Vec<u8>)
    -> HeadResult<Response<Vec<u8>>>
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
            let response = head.upload_targets(caller, &request.session, &request.hashes)?;
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
            match head.object_put(caller, session.as_deref(), &hash, &body)? {
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
            Ok(empty(signature_put_status(head.signature_put(caller, &hash, &body)?)))
        }

        Route::PutTrust => {
            let anchor: TrustAnchorDto = parse_json(&body)?;
            match head.put_trust(caller, &anchor)? {
                TrustResult::Established => Ok(empty(StatusCode::CREATED)),
                TrustResult::Unchanged => Ok(empty(StatusCode::OK)),
            }
        }

        Route::RefUpdate(name) => {
            let request: RefUpdateRequest = parse_json(&body)?;
            head.ref_update(caller, &name, &request)?;
            Ok(empty(StatusCode::OK))
        }

        Route::Holds(name) => {
            let request: HoldsRequest = parse_json(&body)?;
            let holds = head.holds(&name, &request.candidates)?;
            Ok(json_response(StatusCode::OK, &HoldsResponse { holds }))
        }

        // Resolution is a display-only, server-mediated directory lookup (DESIGN.html §8.12).
        // The hosted service tiers names behind its own (privately tracked) policy; this head runs no
        // resolution hook, so — exactly as the protocol prescribes for a head without one — it
        // answers an empty map, and the client shows pseudonyms. The identifier limit is part of
        // the request's contract, so it holds here exactly as on a head with a hook.
        Route::Resolve => {
            let request: ResolveRequest<'static> = parse_json(&body)?;
            distinct_resolve_identifiers(&request.identifiers).map_err(HeadError::unprocessable)?;

            Ok(json_response(StatusCode::OK, &ResolveResponse { names: std::collections::BTreeMap::new() }))
        }

        // A bundle builder is periodic ECS work (DESIGN.html §4.3/§4.6); until one runs
        // `bundle_latest` is a spec-compliant `404` (the `?` returns it) and clients fall back
        // to loose/batch fetches. When a builder ships, the bundle is a zstd stream, so it is
        // served `identity` like `batch`.
        Route::BundleLatest => Ok(octet_stream(StatusCode::OK, head.bundle_latest()?, true)),

        Route::CommitLift(session) => {
            let request: CommitLiftRequest = parse_json(&body)?;
            reject_oversized_commit(request.control_plane.len(), request.blobs.len())?;
            head.commit_lift(caller, &session, &request.control_plane, &request.blobs, request.more)?;
            Ok(empty(StatusCode::OK))
        }
    }
}

/// Validate a warehouse id (the one shared rule, which forklift-server applies too); a refusal is
/// unprocessable.
fn validate_warehouse_id(id: &str) -> HeadResult<()> {
    warehouse_utils::validate_warehouse_id(id).map_err(HeadError::unprocessable)
}

/// Pull one `key=value` out of a raw query string, percent-decoded as a path parameter is (see
/// [`param`]), so the session a staged upload names reads the same as the one its commit path
/// names.
///
/// An empty or whitespace-only value (`?session=`) is treated the same as the parameter being
/// absent entirely, rather than as a real, empty session id: a blank session could never be
/// committed against (`staging//{hash}` is not a key `commit_lift` can promote), so a client
/// that sends one is routed to the same `422 SessionRequired` a missing parameter gets, instead
/// of a `307` that stages the upload somewhere it can never be promoted from.
fn query_param(query: Option<&str>, key: &str) -> HeadResult<Option<String>> {
    let Some(value) = query.and_then(|query| query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then_some(value)
    })) else {
        return Ok(None);
    };

    let value = param(value)?;
    if value.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
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

/// The `Retry-After` a not-ready lift commit carries, in seconds: about how long a staged blob takes
/// the verifier, and short enough that a generic retrying proxy does not stall a lift.
const COMMIT_NOT_READY_RETRY_AFTER_SECONDS: &str = "1";

/// A failed request as the protocol's JSON error body, at the status the head chose.
///
/// A raw `500`'s message is the one the client never sees as written. `Head`'s internal errors wrap
/// raw storage failures (an SDK `DisplayErrorContext` can carry a request id, a bucket or table
/// name), and this router sits behind the hosted service's public edge. `forklift-server`
/// forwards its `500` messages verbatim, and that is fine there — a self-host head only ever
/// runs on the operator's own infrastructure — but this head must not hand a stranger's bucket
/// name to whoever happens to be asking. The detail is logged instead (Lambda ships stderr to
/// CloudWatch) and the client gets a generic message. Every other status is the protocol's own
/// diagnostic (a bad hash, a stale ref, a malformed body) and stays as-is — it is meant to be
/// read. A classified refusal (§7.4) is decoded regardless of status — see [`error_body`].
fn error_response(error: HeadError) -> Response<Vec<u8>> {
    // The head's [`Status`] numeric values *are* the protocol's HTTP status codes, so this is
    // the single point its taxonomy meets `http`.
    let status = StatusCode::from_u16(error.status.as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let commit_not_ready = error.commit_not_ready;

    let body = serde_json::to_vec(&error_body(status, error.message))
        .unwrap_or_else(|_| b"{\"error\":\"internal error\"}".to_vec());

    let mut builder =
        Response::builder().status(status).header(header::CONTENT_TYPE, "application/json");

    if status == StatusCode::UNAUTHORIZED {
        // RFC 6750 §3: a resource server refusing a bearer-authenticated request challenges
        // with this header so a well-behaved client knows which scheme to retry.
        builder = builder.header(header::WWW_AUTHENTICATE, "Bearer");
    }

    if commit_not_ready {
        // The lift commit's retry signal travels in the header section, which a client reads
        // before (and without) the body. `Retry-After` is a hint for generic tooling; the client
        // paces not-ready retries with its own backoff.
        builder = builder
            .header(header::RETRY_AFTER, COMMIT_NOT_READY_RETRY_AFTER_SECONDS)
            .header(LIFT_COMMIT_HEADER, LIFT_COMMIT_NOT_READY);
    }

    builder.body(body).expect("a valid error response")
}

/// Compose the JSON error body from the chosen status and message.
///
/// A **classified refusal** (§7.4) reaches this head as a sentinel-framed message a core operation
/// produced; it is decoded *regardless of status* so the body carries the stable `code` and
/// `next_step` machine-readably (additive fields) with the de-framed human text in `error` — the
/// raw `\u{1f}` frame never leaks, and the refusal message is a controlled taxonomy diagnostic that
/// carries no infrastructure detail, so exposing it is safe even at `500`. A **raw** `500` (not a
/// framed refusal) is the case that may wrap infra detail: it is logged and replaced with a generic
/// message and no code. Any other plain message passes through with no code.
///
/// Trust-boundary note: `deframe` decodes *any* message that happens to start with the sentinel
/// prefix, at any status, regardless of where the `String` originated. Deciding that is safe even
/// at `500` (unlike the rest of that message, which is hidden) rests on every framed message being
/// refusal text this codebase's own sanitized constructors produced, sent back only to the
/// requester whose action triggered it — never third-party or attacker-supplied content. Keep it
/// that way: a future error path that let outside-influenced text reach byte zero of a handler's
/// message could spoof a `code`/`next_step` pair here.
fn error_body(status: StatusCode, message: String) -> ErrorResponse {
    if let Some((code, human, next_step)) = forklift_core::error::deframe(&message) {
        return ErrorResponse {
            error: human.to_string(),
            code: Some(code.to_string()),
            next_step: Some(next_step.to_string()),
        };
    }

    if status == StatusCode::INTERNAL_SERVER_ERROR {
        eprintln!("forklift-aws-lambda: internal error: {}", message);
        return ErrorResponse {
            error: "An internal error occurred.".to_string(),
            code: None,
            next_step: None,
        };
    }

    ErrorResponse { error: message, code: None, next_step: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forklift_core::util::scope_utils;

    /// A classified refusal (§7.4) is decoded onto the wire regardless of status — its human message
    /// is a controlled taxonomy diagnostic, safe to expose even at `500`, so both heads carry the
    /// code consistently. The raw frame never leaks.
    #[test]
    fn error_body_threads_a_refusal_code_at_any_status() {
        for status in [StatusCode::UNPROCESSABLE_ENTITY, StatusCode::INTERNAL_SERVER_ERROR] {
            let framed: String = scope_utils::chunked_transport_refusal("big.bin").into();
            let body = error_body(status, framed);

            assert_eq!(body.code.as_deref(), Some(scope_utils::CODE_CHUNKED_TRANSPORT_UNSUPPORTED),
                "code carried at {}", status);
            assert!(body.next_step.is_some());
            assert!(body.error.contains("big.bin"), "{}", body.error);
            assert!(!body.error.contains('\u{1f}'), "no frame leaks: {:?}", body.error);
        }
    }

    /// A **raw** `500` (not a framed refusal) still hides its message — an SDK error can carry a
    /// bucket/request-id, which must never reach a stranger — and carries no code.
    #[test]
    fn error_body_hides_a_raw_internal_error() {
        let body = error_body(
            StatusCode::INTERNAL_SERVER_ERROR,
            "DynamoDB error in table forklift-prod-abc123: ...".to_string(),
        );
        assert_eq!(body.error, "An internal error occurred.");
        assert!(body.code.is_none());
        assert!(!body.error.contains("forklift-prod-abc123"), "infra detail hidden: {}", body.error);
    }

    /// A variable source holding `overrides` on top of the two required names.
    fn env_with(overrides: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let mut vars: std::collections::HashMap<String, String> = [
            ("FORKLIFT_S3_BUCKET", "bucket"), ("FORKLIFT_DYNAMODB_TABLE", "table"),
        ].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        for (key, value) in overrides {
            vars.insert(key.to_string(), value.to_string());
        }
        move |name| vars.get(name).cloned()
    }

    /// FORK-77: a head configured with a name every client refuses must not start. A mixed-case
    /// warehouse id or default pallet is refused, naming the variable; the defaults, a portable
    /// id, and multi routing (an empty id is the placeholder, never validated) all start.
    #[test]
    fn config_refuses_names_outside_the_portable_grammar() {
        let error = config_from(env_with(&[("FORKLIFT_WAREHOUSE_ID", "Acme")])).err().unwrap();
        assert!(error.contains("FORKLIFT_WAREHOUSE_ID"), "{}", error);
        assert!(error.contains("\"Acme\" is not a valid warehouse id"), "{}", error);

        let error = config_from(env_with(&[("FORKLIFT_DEFAULT_PALLET", "Main")])).err().unwrap();
        assert!(error.contains("FORKLIFT_DEFAULT_PALLET"), "{}", error);
        assert!(error.contains("\"Main\" is not a valid pallet name"), "{}", error);

        let (_, routing) = config_from(env_with(&[])).unwrap();
        assert!(matches!(routing, Routing::Multi));

        let (_, routing) = config_from(env_with(&[("FORKLIFT_WAREHOUSE_ID", "")])).unwrap();
        assert!(matches!(routing, Routing::Multi), "an empty id selects multi routing");

        let (_, routing) = config_from(env_with(&[
            ("FORKLIFT_WAREHOUSE_ID", "acme"), ("FORKLIFT_DEFAULT_PALLET", "trunk"),
        ])).unwrap();
        assert!(matches!(routing, Routing::Single(ref id) if id == "acme"));
    }

    /// A plain non-500 error passes through with no code (an older client ignores the absent field).
    #[test]
    fn error_body_passes_a_plain_error_through() {
        let body = error_body(StatusCode::UNPROCESSABLE_ENTITY, "a bad hash".to_string());
        assert_eq!(body.error, "a bad hash");
        assert!(body.code.is_none());
    }

    /// `query_param` treats a missing, empty, or whitespace-only value as absent, so `?session=`
    /// never presigns a `staging//{hash}` key nothing could ever commit against. It decodes as a
    /// path parameter does, so emptiness is judged on the decoded value.
    #[test]
    fn query_param_treats_empty_and_whitespace_values_as_absent() {
        let read = |query: Option<&str>| query_param(query, "session").unwrap();
        assert_eq!(read(Some("session=abc")), Some("abc".to_string()));
        assert_eq!(read(Some("session=")), None, "an empty value is absent");
        assert_eq!(read(Some("session=   ")), None, "a whitespace-only value is absent");
        assert_eq!(read(Some("session=%20")), None, "decoded, an encoded space is whitespace too");
        assert_eq!(read(Some("session=%61bc")), Some("abc".to_string()), "decoded as a path parameter");
        assert!(query_param(Some("session=%FF"), "session").is_err(), "not UTF-8 once decoded");
        assert_eq!(read(Some("other=abc")), None, "a different key");
        assert_eq!(read(Some("session=abc&other=x")), Some("abc".to_string()));
        assert_eq!(read(None), None, "no query string at all");
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

    /// The SHA-256 of `token`, as a deployer writes it into the environment.
    fn hex_digest(token: &str) -> String {
        sign_utils::to_hex(&Sha256::digest(token.as_bytes()))
    }

    /// A variable source holding exactly `vars`.
    fn vars(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: HashMap<String, String> =
            vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |name| vars.get(name).cloned()
    }

    /// Credentials of a shared bearer `token` alone.
    fn static_auth(token: &str) -> AuthConfig {
        AuthConfig::Credentials(Credentials::new(Some(TokenDigest::of_token(token).unwrap()), Vec::new()).unwrap())
    }

    /// Credentials of a shared bearer and one operator token per `(token, identifier)`.
    fn operator_auth(static_token: Option<&str>, operators: &[(&str, &str)]) -> AuthConfig {
        let operators = operators
            .iter()
            .map(|(token, identifier)| (TokenDigest::of_token(token).unwrap(), identifier.to_string()))
            .collect();
        AuthConfig::Credentials(
            Credentials::new(static_token.map(|token| TokenDigest::of_token(token).unwrap()), operators).unwrap(),
        )
    }

    /// A configured digest is 64 lowercase hex characters, and never the digest of the empty
    /// string: a configuration holding that would admit a blank bearer.
    #[test]
    fn a_token_digest_is_64_lowercase_hex_and_never_the_empty_strings() {
        assert_eq!(TokenDigest::parse_hex(&hex_digest("secret")).unwrap(), TokenDigest::of_token("secret").unwrap());

        assert!(TokenDigest::parse_hex(&hex_digest("")).is_err(), "the empty string's digest");
        assert!(TokenDigest::parse_hex(&hex_digest("secret").to_uppercase()).is_err(), "uppercase");
        assert!(TokenDigest::parse_hex(&hex_digest("secret")[1..]).is_err(), "63 characters");
        assert!(TokenDigest::parse_hex(&format!("{}0", hex_digest("secret"))).is_err(), "65 characters");
        assert!(TokenDigest::parse_hex(&format!("g{}", &hex_digest("secret")[1..])).is_err(), "a non-hex digit");
        assert!(TokenDigest::parse_hex("").is_err(), "empty");
        assert!(TokenDigest::parse_hex("secret").is_err(), "a plaintext token");

        assert!(TokenDigest::of_token("").is_err(), "an empty token");
        assert!(TokenDigest::of_token("   ").is_err(), "a whitespace-only token");
        assert_eq!(
            TokenDigest::of_token(" has spaces ").unwrap(),
            TokenDigest::parse_hex(&hex_digest(" has spaces ")).unwrap(),
            "a real token is hashed untrimmed"
        );
    }

    /// The cold-start matrix: a credential configured closes the head to everything else; no
    /// credential is closed unless the literal opt-out says open; and every misconfiguration is
    /// an error that fails the cold start rather than a quieter configuration.
    #[test]
    fn auth_from_env_resolves_credentials_and_refuses_every_misconfiguration() {
        let digest = hex_digest("secret");
        let ada = hex_digest("ada-token");

        assert_eq!(auth_from(vars(&[("FORKLIFT_TOKEN_SHA256", &digest)])).unwrap(), static_auth("secret"));
        assert_eq!(
            auth_from(vars(&[("FORKLIFT_OPERATOR_TOKENS", &format!(r#"{{"{}":"ada@example"}}"#, ada))])).unwrap(),
            operator_auth(None, &[("ada-token", "ada@example")])
        );
        assert_eq!(
            auth_from(vars(&[
                ("FORKLIFT_TOKEN_SHA256", &digest),
                ("FORKLIFT_OPERATOR_TOKENS", &format!(r#"{{"{}":"ada@example"}}"#, ada)),
            ]))
            .unwrap(),
            operator_auth(Some("secret"), &[("ada-token", "ada@example")])
        );

        assert_eq!(auth_from(vars(&[])).unwrap(), AuthConfig::Closed, "nothing configured is closed");
        assert_eq!(auth_from(vars(&[("FORKLIFT_OPEN_ACCESS", "1")])).unwrap(), AuthConfig::Open);
        assert_eq!(auth_from(vars(&[("FORKLIFT_OPEN_ACCESS", "yes")])).unwrap(), AuthConfig::Closed, "only \"1\"");
        assert_eq!(
            auth_from(vars(&[("FORKLIFT_OPERATOR_TOKENS", "{}")])).unwrap(),
            AuthConfig::Closed,
            "an empty map is no credential"
        );

        let refused = |source: &[(&str, &str)], why: &str| {
            let error = auth_from(vars(source)).expect_err(why);
            assert!(!error.contains("secret"), "{}: the error must not echo a credential: {}", why, error);
            error
        };

        let leftover = refused(&[("FORKLIFT_TOKEN", "secret")], "a plaintext token");
        assert!(leftover.contains("FORKLIFT_TOKEN_SHA256"), "{}", leftover);
        refused(&[("FORKLIFT_TOKEN", "secret"), ("FORKLIFT_TOKEN_SHA256", &digest)], "a plaintext token beside its digest");
        refused(&[("FORKLIFT_TOKEN", "")], "even an empty plaintext token");

        let open = refused(&[("FORKLIFT_TOKEN_SHA256", &digest), ("FORKLIFT_OPEN_ACCESS", "1")], "open beside a digest");
        assert!(open.contains("FORKLIFT_OPEN_ACCESS"), "{}", open);
        refused(
            &[("FORKLIFT_OPERATOR_TOKENS", &format!(r#"{{"{}":"ada@example"}}"#, ada)), ("FORKLIFT_OPEN_ACCESS", "1")],
            "open beside operator tokens",
        );

        refused(&[("FORKLIFT_TOKEN_SHA256", "")], "an empty digest");
        refused(&[("FORKLIFT_TOKEN_SHA256", &hex_digest(""))], "the empty string's digest");
        refused(&[("FORKLIFT_TOKEN_SHA256", "secret")], "a plaintext token in the digest variable");

        refused(&[("FORKLIFT_OPERATOR_TOKENS", "not json")], "malformed JSON");
        refused(&[("FORKLIFT_OPERATOR_TOKENS", &format!(r#"["{}"]"#, ada))], "not an object");
        refused(&[("FORKLIFT_OPERATOR_TOKENS", &format!(r#"{{"{}":7}}"#, ada))], "a non-string identifier");
        refused(&[("FORKLIFT_OPERATOR_TOKENS", r#"{"secret":"ada@example"}"#)], "a key that is not a digest");
        refused(
            &[("FORKLIFT_OPERATOR_TOKENS", &format!(r#"{{"{}":"ada@example"}}"#, hex_digest("")))],
            "the empty string's digest as a key",
        );
        refused(&[("FORKLIFT_OPERATOR_TOKENS", &format!(r#"{{"{}":"  "}}"#, ada))], "a blank identifier");
        refused(
            &[("FORKLIFT_OPERATOR_TOKENS", &format!(r#"{{"{0}":"ada@example","{0}":"eve@example"}}"#, ada))],
            "one digest bound twice",
        );
        refused(
            &[
                ("FORKLIFT_TOKEN_SHA256", &hex_digest("shared-secret")),
                ("FORKLIFT_OPERATOR_TOKENS", &format!(r#"{{"{}":"ada@example"}}"#, hex_digest("shared-secret"))),
            ],
            "one digest bound as the shared bearer and as an operator",
        );
    }

    /// Operator tokens alone suit one warehouse, never many: in multi routing nobody could set a
    /// warehouse up. The shared bearer, or open access, suits either.
    #[test]
    fn operator_tokens_alone_are_refused_for_multi_warehouse_routing() {
        let operators_only = auth_from(vars(&[(
            "FORKLIFT_OPERATOR_TOKENS",
            &format!(r#"{{"{}":"ada@example"}}"#, hex_digest("ada-token")),
        )]))
        .unwrap();
        let with_bearer = auth_from(vars(&[("FORKLIFT_TOKEN_SHA256", &hex_digest("shared-secret"))])).unwrap();
        let single = Routing::Single("wh".to_string());

        assert!(check_auth_fits_routing(&operators_only, &Routing::Multi).unwrap_err().contains("FORKLIFT_TOKEN_SHA256"));
        assert_eq!(check_auth_fits_routing(&operators_only, &single), Ok(()));
        assert_eq!(check_auth_fits_routing(&with_bearer, &Routing::Multi), Ok(()));
        assert_eq!(check_auth_fits_routing(&AuthConfig::Open, &Routing::Multi), Ok(()));
    }

    /// An identifier is bound exactly as configured: the office matches identifiers by exact
    /// equality, so trimming one here would hand its token to a different user.
    #[test]
    fn an_operator_identifier_is_kept_untrimmed() {
        let config = auth_from(vars(&[(
            "FORKLIFT_OPERATOR_TOKENS",
            &format!(r#"{{"{}":" ada@example"}}"#, hex_digest("ada-token")),
        )]))
        .unwrap();

        assert_eq!(
            authenticate(&config, &request_with_bearer("ada-token")).unwrap(),
            Principal::Operator(" ada@example".to_string())
        );
    }

    /// `Debug` never prints a digest or an identifier, only what is configured.
    #[test]
    fn auth_config_debug_never_prints_a_credential() {
        let config = operator_auth(Some("s3cr3t-value"), &[("ada-token", "ada@example")]);
        let formatted = format!("{:?}", config);

        assert!(!formatted.contains(&hex_digest("s3cr3t-value")), "{}", formatted);
        assert!(!formatted.contains(&hex_digest("ada-token")), "{}", formatted);
        assert!(!formatted.contains("ada@example"), "{}", formatted);
        assert!(formatted.contains("Credentials"), "{}", formatted);

        let digest = format!("{:?}", TokenDigest::of_token("s3cr3t-value").unwrap());
        assert!(digest.contains("redacted"), "{}", digest);
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
    fn authenticate_open_passes_every_request_as_open() {
        let auth = AuthConfig::Open;
        assert_eq!(authenticate(&auth, &request_with_no_header()).unwrap(), Principal::Open);
        assert_eq!(authenticate(&auth, &request_with_bearer("anything")).unwrap(), Principal::Open);
    }

    /// The shared bearer is [`Principal::Static`], an operator token is that operator, and
    /// anything else — no header, a wrong or near-miss token, another scheme — is refused.
    #[test]
    fn authenticate_resolves_each_token_to_its_principal() {
        let auth = operator_auth(Some("secret"), &[("ada-token", "ada@example")]);

        assert_eq!(authenticate(&auth, &request_with_bearer("secret")).unwrap(), Principal::Static);
        assert_eq!(
            authenticate(&auth, &request_with_bearer("ada-token")).unwrap(),
            Principal::Operator("ada@example".to_string())
        );
        assert!(authenticate(&auth, &request_with_no_header()).is_err());
        assert!(authenticate(&auth, &request_with_bearer("wrong")).is_err());
        assert!(authenticate(&auth, &request_with_bearer("secrft")).is_err(), "an equal-length near miss");
        assert!(authenticate(&auth, &request_with_bearer("ada@example")).is_err(), "an identifier is not a token");

        let non_bearer =
            Request::builder().header(header::AUTHORIZATION, "Basic secret").body(()).unwrap();
        assert!(authenticate(&auth, &non_bearer).is_err(), "a non-Bearer scheme must refuse");

        let operators_only = operator_auth(None, &[("ada-token", "ada@example")]);
        assert!(authenticate(&operators_only, &request_with_bearer("secret")).is_err(), "no shared bearer");
    }

    /// A blank bearer is refused before anything is hashed, so no configured digest can ever
    /// match one — not even a digest of that very blank string, which [`TokenDigest`]'s
    /// constructors refuse but which is built here directly to show the check does not lean on
    /// them.
    #[test]
    fn authenticate_refuses_a_blank_bearer_before_hashing_it() {
        for blank in ["", "   "] {
            let auth = AuthConfig::Credentials(Credentials {
                static_token: Some(TokenDigest::hash(blank)),
                operators: [(TokenDigest::hash(blank), "ada@example".to_string())].into_iter().collect(),
            });

            let error = authenticate(&auth, &request_with_bearer(blank)).unwrap_err();
            assert_eq!(error.status, crate::error::Status::Unauthorized, "{:?}", blank);
        }
    }

    /// RFC 7235 §2.1: `auth-scheme` is case-insensitive, so `bearer`/`BEARER`/mixed-case must
    /// authenticate exactly like `Bearer`. The credential (token) half of the header is a
    /// different story — it stays case-sensitive, so a same-length, wrong-case token must still
    /// refuse.
    #[test]
    fn authenticate_accepts_the_bearer_scheme_case_insensitively() {
        let auth = static_auth("secret");

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
            &static_auth("secret"),
            &request_with_bearer("wrong"),
        ).unwrap_err();

        assert_eq!(closed_error.status, wrong_token_error.status);
        assert_eq!(closed_error.message, wrong_token_error.message);
    }
}
