//! The HTTP surface of the server head: one handler per protocol endpoint.
//!
//! Every handler does its filesystem work inside `spawn_blocking` (the storage code is
//! synchronous by design) under the resolved warehouse's storage-root scope, and the
//! mutations with ordering requirements — the ref-update CAS and the one-way door of
//! trust establishment — are serialized by a per-warehouse mutex, so they can never
//! interleave between two lifts.
//!
//! Two serving modes share every handler: `--root` serves a single warehouse at
//! `/v1/…`, `--warehouses` serves each subdirectory of a base folder at
//! `/warehouses/{id}/v1/…` (the id travels inside `remote.url`, so clients need
//! nothing new). Warehouse creation is explicit: `PUT /warehouses/{id}` on a
//! token-protected multi-warehouse server (never a side effect of a lift).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use forklift_core::globals::{StorageRootScope, FOLDER_NAME_FORKLIFT_ROOT};
use forklift_core::model::hooks::{
    AdmissionHookRequest, AdmissionHookResponse, AuthenticationHookRequest,
    AuthenticationHookResponse, HookEvent, ResolutionHookRequest, ResolutionHookResponse,
    EVENT_KEY_REVOKED, EVENT_PALLET_UPDATED, EVENT_TRUST_ESTABLISHED, EVENT_TRUST_RESET,
    EVENT_WAREHOUSE_CREATED,
};
use forklift_core::model::remote::{
    ErrorResponse, MissingObjectsRequest, MissingObjectsResponse, RefUpdateRequest,
    ResolveRequest, ResolveResponse, TrustAnchorDto, UploadTargetsRequest, UploadTargetsResponse,
    WarehouseInfo, MAX_MISSING_BATCH, MAX_UPLOAD_TARGETS_BATCH, PROTOCOL_VERSION,
};
use forklift_core::util::office_utils::OFFICE_PALLET_NAME;
use forklift_core::util::lock_utils::ServeLock;
use forklift_core::util::{
    audit_utils, bundle_utils, file_utils, hook_utils, merge_utils, object_utils,
    office_utils, pallet_utils, sign_utils, warehouse_utils,
};

/// One served warehouse: its storage root and its write mutex.
struct WarehouseHandle {
    /// Serializes warehouse mutations with ordering requirements: the CAS
    /// read-check-write of a ref update and the one-way door of trust establishment
    /// must never interleave.
    writes: Mutex<()>,

    /// The warehouse root; every blocking storage closure enters it as a storage-root
    /// scope (the process never changes its working directory, which is what lets one
    /// process serve several roots).
    root: PathBuf,

    /// Accepted lifts since the bundle was last (re)built — the auto-rebuild trigger.
    lifts_since_bundle: std::sync::atomic::AtomicU32,

    /// Whether a bundle rebuild is running right now (never two at once).
    bundling: std::sync::atomic::AtomicBool,

    /// The serve lock for this root, held for as long as the handle is served. `None` for
    /// transient handles that are not a live served warehouse (e.g. the throwaway handle
    /// `put_warehouse` uses only to run `prepare` in a storage scope). Held in the handle so the
    /// lock releases exactly when the served warehouse stops being served (server shutdown).
    _serve_lock: Option<ServeLock>,
}

impl WarehouseHandle {
    /// A transient handle with no serve lock — for work that is not a live served warehouse
    /// (warehouse creation). A served warehouse is built with [`WarehouseHandle::serving`].
    fn new(root: PathBuf) -> WarehouseHandle {
        WarehouseHandle {
            writes: Mutex::new(()),
            root,
            lifts_since_bundle: std::sync::atomic::AtomicU32::new(0),
            bundling: std::sync::atomic::AtomicBool::new(false),
            _serve_lock: None,
        }
    }

    /// A handle for a warehouse this process is about to serve: acquires the serve lock at `root`
    /// and holds it in the handle for the handle's lifetime. Errors if the root is already
    /// served by another process, or a `gc`/`bundle` is running against it — in either case
    /// serving it would be unsafe.
    fn serving(root: PathBuf) -> Result<WarehouseHandle, String> {
        // Acquire inside the root's storage scope so the lock lands at this warehouse's store
        // root (the scope is synchronous and never held across an `.await`; the guard keeps an
        // absolute path, so it stays valid once the scope drops).
        let serve_lock = {
            let _scope = StorageRootScope::enter(&root);
            ServeLock::acquire()?
        };
        let mut handle = WarehouseHandle::new(root);
        handle._serve_lock = Some(serve_lock);
        Ok(handle)
    }
}

/// What the process serves.
enum ServeMode {
    /// One warehouse at `/v1/…`.
    Single(Arc<WarehouseHandle>),

    /// Every prepared subdirectory of a base folder, at `/warehouses/{id}/v1/…`.
    Multi { base: PathBuf },
}

/// The shared server state.
struct AppState {
    mode: ServeMode,

    /// The static bearer token (full access), when configured.
    token: Option<String>,

    /// Per-operator bearer tokens: token → office identifier. What the
    /// operator may do derives from their role in the target warehouse's office —
    /// the tracked, signed metadata — not from the token itself.
    operator_tokens: HashMap<String, String>,

    /// Rebuild a warehouse's bundle after this many accepted lifts (`None` = never;
    /// `forklift-server bundle` stays the manual path).
    rebuild_after_lifts: Option<u32>,

    /// The handles of the warehouses seen so far (multi mode). Exactly one handle may
    /// ever exist per warehouse id — a second handle would mean a second write mutex.
    warehouses: Mutex<HashMap<String, Arc<WarehouseHandle>>>,

    /// The configured hooks (`docs/format/HOOK_PROTOCOL.md`) — the typed seam a
    /// hosting provider plugs into. Only the soft surface is hookable; signature and
    /// privilege verification never are.
    authentication_hook: Option<HookEndpoint>,
    admission_hook: Option<HookEndpoint>,
    events_hook: Option<HookEndpoint>,
    resolution_hook: Option<HookEndpoint>,

    /// The outbound HTTP client of the hooks.
    http: reqwest::Client,

    /// Successful authentication-hook answers, cached per token (hot path). A revoked
    /// credential outlives its revocation by at most the TTL.
    authentication_cache: Mutex<HashMap<String, (String, std::time::Instant)>>,
    authentication_cache_ttl: std::time::Duration,
}

/// Who a request is: the transport-level identity. Content-level authorization (roles,
/// pallet grants) is decided against the warehouse's office state.
#[derive(Clone, PartialEq, Eq)]
enum Principal {
    /// No authentication is configured on this server: full access.
    Open,

    /// The static token: full access (the operator of the server itself).
    Static,

    /// A per-operator token, bound to this office identifier.
    Operator(String),
}

/// A handler error: a status code and the message for the JSON error body.
type HandlerError = (StatusCode, String);

/// The path parameters of a request; which keys exist depends on the route and the
/// serving mode (multi mode adds `warehouse` to every route).
type PathParams = HashMap<String, String>;

/// One configured hook endpoint. The secret is mandatory: every hook request is
/// signed (Blake3 keyed MAC over timestamp + body), because a spoofable
/// authentication hook is game over (§8.13).
#[derive(Clone)]
pub struct HookEndpoint {
    pub url: String,
    pub secret: String,
}

/// What to serve and how (the merged flags/config of the `serve` subcommand).
pub struct ServeOptions {
    /// The single warehouse root to serve at `/v1` (mutually exclusive with
    /// `warehouses`).
    pub root: Option<String>,

    /// The base folder whose subdirectories are served at `/warehouses/{id}/v1`.
    pub warehouses: Option<String>,

    /// The address to bind (port 0 picks a free port).
    pub addr: String,

    /// The static bearer token (full access), if any.
    pub token: Option<String>,

    /// The path of the per-operator token file, if any.
    pub tokens: Option<String>,

    /// Refuse request bodies over this size (MiB); `None` = unlimited.
    pub max_body_mb: Option<u64>,

    /// Rebuild a warehouse's bundle after this many accepted lifts; `None` = never.
    pub rebuild_after_lifts: Option<u32>,

    /// `authentication` hook: credential → operator identifier (hot, fail closed).
    pub authentication_hook: Option<HookEndpoint>,

    /// `admission` hook: soft policy gate on uploads/ref updates/creation (hot,
    /// fail closed).
    pub admission_hook: Option<HookEndpoint>,

    /// `event` hook: lift/trust/revocation webhooks (cold, retried).
    pub events_hook: Option<HookEndpoint>,

    /// `resolution` hook: operator id → display name, behind `POST /v1/resolve`
    /// (cold, degrade to pseudonyms).
    pub resolution_hook: Option<HookEndpoint>,

    /// How long a positive authentication-hook answer is cached (seconds; default 60).
    pub authentication_cache_secs: Option<u64>,
}

/// Serve one warehouse root, or every warehouse under a base folder.
///
/// # Returns
/// * `Err(String)` - If the root is not a warehouse or the address cannot be bound
///                   (serving itself runs until the process is stopped or receives
///                   SIGINT/SIGTERM, which drains gracefully).
pub async fn serve(options: ServeOptions) -> Result<(), String> {
    // Structured request logs; RUST_LOG overrides (default: info).
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
        )
        .with_writer(std::io::stderr)
        .try_init();

    let operator_tokens = match options.tokens {
        Some(path) => parse_operator_tokens(&path)?,
        None => HashMap::new(),
    };

    let mode = match (options.root, options.warehouses) {
        (Some(root), None) => {
            let root = std::fs::canonicalize(&root)
                .map_err(|e| format!("Error while resolving \"{}\": {}", root, e))?;

            if !root.join(FOLDER_NAME_FORKLIFT_ROOT).is_dir() {
                return Err(format!(
                    "\"{}\" is not a forklift warehouse. Prepare it first: \
                    forklift-server prepare --root {}",
                    root.to_string_lossy(), root.to_string_lossy()
                ));
            }

            // Hold the serve lock for the served root for the process lifetime: a second
            // server on the same root is refused here rather than silently breaking the CAS.
            ServeMode::Single(Arc::new(WarehouseHandle::serving(root)?))
        }
        (None, Some(base)) => {
            let base = std::fs::canonicalize(&base)
                .map_err(|e| format!("Error while resolving \"{}\": {}", base, e))?;

            if !base.is_dir() {
                return Err(format!("\"{}\" is not a folder.", base.to_string_lossy()));
            }

            ServeMode::Multi { base }
        }
        _ => return Err("Pass exactly one of --root and --warehouses.".to_string()),
    };

    let is_multi = matches!(mode, ServeMode::Multi { .. });

    for (name, hook) in [
        ("authentication", &options.authentication_hook),
        ("admission", &options.admission_hook),
        ("events", &options.events_hook),
        ("resolution", &options.resolution_hook),
    ] {
        if let Some(hook) = hook {
            if hook.url.is_empty() || hook.secret.is_empty() {
                return Err(format!(
                    "The {} hook needs both a URL and a secret: hook requests are \
                    signed, and an unsigned hook would be spoofable.",
                    name
                ));
            }
        }
    }

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("Error while building the hook HTTP client: {}", e))?;

    let state = Arc::new(AppState {
        mode,
        token: options.token,
        operator_tokens,
        rebuild_after_lifts: options.rebuild_after_lifts,
        warehouses: Mutex::new(HashMap::new()),
        authentication_hook: options.authentication_hook,
        admission_hook: options.admission_hook,
        events_hook: options.events_hook,
        resolution_hook: options.resolution_hook,
        http,
        authentication_cache: Mutex::new(HashMap::new()),
        authentication_cache_ttl: std::time::Duration::from_secs(
            options.authentication_cache_secs.unwrap_or(60)
        ),
    });

    let protocol = Router::new()
        .route("/warehouse", get(get_warehouse))
        .route("/objects/missing", post(post_missing))
        .route("/objects/upload-targets", post(post_upload_targets))
        .route("/objects/batch", post(post_objects_batch))
        .route("/objects/{hash}", get(get_object).put(put_object))
        .route("/signatures/{hash}", get(get_signature).put(put_signature))
        .route("/trust", put(put_trust))
        .route("/pallets/{name}", post(post_ref_update))
        .route("/resolve", post(post_resolve))
        .route("/bundles/latest", get(get_bundle));

    let app = if is_multi {
        Router::new()
            .route("/warehouses/{warehouse}", put(put_warehouse))
            .nest("/warehouses/{warehouse}/v1", protocol)
    } else {
        Router::new().nest("/v1", protocol)
    };

    // The hash check gates correctness; the (optional) body cap gates disk-fill abuse.
    let body_limit = match options.max_body_mb {
        Some(mb) => DefaultBodyLimit::max((mb as usize) * 1024 * 1024),
        None => DefaultBodyLimit::disable(),
    };

    let app = app
        // Liveness only — deliberately unauthenticated and warehouse-free.
        .route("/healthz", get(|| async { "ok" }))
        .layer(body_limit)
        // Objects travel uncompressed on the wire by format (the hash covers the
        // uncompressed form, §4.4), so transport compression is nearly free wins;
        // bundle/batch responses are already zstd streams and mark themselves
        // `content-encoding: identity`, which the layer respects.
        .layer(tower_http::compression::CompressionLayer::new())
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&options.addr)
        .await
        .map_err(|e| format!("Error while binding \"{}\": {}", options.addr, e))?;

    let bound = listener.local_addr()
        .map_err(|e| format!("Error while reading the bound address: {}", e))?;

    // The single startup line is machine-readable on purpose: tools (and the tests)
    // parse the port out of it, which is what makes `--addr 127.0.0.1:0` usable.
    println!("forklift-server listening on http://{}", bound);

    use std::io::Write;
    std::io::stdout().flush().ok();

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| format!("The server failed: {}", e))
}

/// Resolve when the process should shut down (SIGINT/ctrl-c, or SIGTERM on Unix), so
/// in-flight transfers drain instead of being severed.
async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => { signal.recv().await; }
            Err(_) => std::future::pending().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = interrupt => {}
        _ = terminate => {}
    }
}

/// Parse the operator-token file: a TOML `[operators]` table of `"<token>" =
/// "<office identifier>"` entries. Tokens are transport secrets and live only here,
/// server-side — never in the tracked office metadata.
fn parse_operator_tokens(path: &str) -> Result<HashMap<String, String>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Error while reading the token file \"{}\": {}", path, e))?;

    let doc: toml_edit::DocumentMut = content.parse()
        .map_err(|e| format!("The token file \"{}\" is not valid TOML: {}", path, e))?;

    let Some(operators) = doc.get("operators").and_then(|item| item.as_table()) else {
        return Err(format!(
            "The token file \"{}\" has no [operators] table (\"<token>\" = \"<identifier>\").",
            path
        ));
    };

    let mut tokens = HashMap::new();

    for (token, identifier) in operators.iter() {
        let identifier = identifier.as_str().ok_or(format!(
            "The token file \"{}\" maps a token to a non-string value.", path
        ))?;

        tokens.insert(token.to_string(), identifier.to_string());
    }

    Ok(tokens)
}

/// Build the JSON error response of a handler error.
fn error_response(error: HandlerError) -> Response {
    (error.0, Json(ErrorResponse { error: error.1 })).into_response()
}

/// Map an internal storage error to a 500.
fn internal(message: String) -> HandlerError {
    (StatusCode::INTERNAL_SERVER_ERROR, message)
}

/// Map a verification failure to a 422.
fn unprocessable(message: String) -> HandlerError {
    (StatusCode::UNPROCESSABLE_ENTITY, message)
}

/// Authenticate a request: who is this? Reads are open to every authenticated
/// principal; writes are further authorized against the office (see
/// `require_uploader` and the ref-update handler). Tokens unknown locally are asked
/// of the authentication hook, when one is configured (fail closed: a hook failure
/// refuses the request, it never waves it through).
async fn check_auth(state: &AppState, headers: &HeaderMap) -> Result<Principal, HandlerError> {
    if state.token.is_none()
        && state.operator_tokens.is_empty()
        && state.authentication_hook.is_none() {
        return Ok(Principal::Open);
    }

    let provided = headers.get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));

    let unauthorized = || (
        StatusCode::UNAUTHORIZED,
        "A valid bearer token is required.".to_string()
    );

    match provided {
        Some(token) if state.token.as_deref() == Some(token) => Ok(Principal::Static),
        Some(token) => {
            if let Some(identifier) = state.operator_tokens.get(token) {
                return Ok(Principal::Operator(identifier.clone()));
            }

            match &state.authentication_hook {
                Some(hook) => authenticate_via_hook(state, hook, token).await,
                None => Err(unauthorized()),
            }
        }
        None => Err(unauthorized()),
    }
}

/// Ask the authentication hook who a token is; positive answers are cached for the
/// configured TTL. Failure policy: fail closed — a hook the server cannot reach
/// refuses the request (503), it never becomes an open door.
async fn authenticate_via_hook(state: &AppState,
                               hook: &HookEndpoint,
                               token: &str) -> Result<Principal, HandlerError> {
    if let Ok(cache) = state.authentication_cache.lock() {
        if let Some((identifier, at)) = cache.get(token) {
            if at.elapsed() < state.authentication_cache_ttl {
                return Ok(Principal::Operator(identifier.clone()));
            }
        }
    }

    let request = AuthenticationHookRequest { token: token.to_string() };

    let response = post_hook(state, hook, "authentication", &request).await
        .map_err(|reason| {
            tracing::warn!(reason, "the authentication hook is unreachable; failing closed");

            (
                StatusCode::SERVICE_UNAVAILABLE,
                "The authentication service is unavailable; try again later.".to_string()
            )
        })?;

    if !response.status().is_success() {
        return Err((
            StatusCode::UNAUTHORIZED,
            "A valid bearer token is required.".to_string()
        ));
    }

    let answer: AuthenticationHookResponse = response.json().await.map_err(|e| {
        tracing::warn!(error = %e, "the authentication hook answered malformed JSON");

        (
            StatusCode::SERVICE_UNAVAILABLE,
            "The authentication service is unavailable; try again later.".to_string()
        )
    })?;

    if let Ok(mut cache) = state.authentication_cache.lock() {
        // The cache is bounded by the set of live tokens; expired entries are
        // dropped opportunistically so failed brute-force tokens cannot pile up
        // (they are never inserted — only successes are cached).
        cache.retain(|_, (_, at)| at.elapsed() < state.authentication_cache_ttl);
        cache.insert(token.to_string(), (answer.identifier.clone(), std::time::Instant::now()));
    }

    Ok(Principal::Operator(answer.identifier))
}

/// POST one signed hook request (see `hook_utils`: Blake3 keyed MAC over
/// timestamp + body).
async fn post_hook<T: serde::Serialize>(state: &AppState,
                                        hook: &HookEndpoint,
                                        kind: &str,
                                        payload: &T) -> Result<reqwest::Response, String> {
    let body = serde_json::to_vec(payload)
        .map_err(|e| format!("Error while encoding the {} hook request: {}", kind, e))?;

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut request = state.http.post(&hook.url)
        .header("content-type", "application/json")
        .body(body.clone());

    for (name, value) in hook_utils::hook_request_headers(kind, &hook.secret, timestamp, &body) {
        request = request.header(name, value);
    }

    request.send().await
        .map_err(|e| format!("The {} hook at \"{}\" failed: {}", kind, hook.url, e))
}

/// Ask the admission hook whether a request may proceed (soft policy: quotas, plan
/// limits, suspensions). Fail closed on hook failure; a denial carries the hook's
/// reason. No hook configured = everything admitted (the default server).
async fn check_admission(state: &AppState,
                         params: &PathParams,
                         principal: &Principal,
                         action: &str,
                         pallet: Option<&str>) -> Result<(), HandlerError> {
    let Some(hook) = &state.admission_hook else {
        return Ok(());
    };

    let request = AdmissionHookRequest {
        action: action.to_string(),
        warehouse: params.get("warehouse").cloned(),
        operator: match principal {
            Principal::Operator(identifier) => Some(identifier.clone()),
            _ => None,
        },
        pallet: pallet.map(|name| name.to_string()),
    };

    let response = post_hook(state, hook, "admission", &request).await
        .map_err(|reason| {
            tracing::warn!(reason, "the admission hook is unreachable; failing closed");

            (
                StatusCode::SERVICE_UNAVAILABLE,
                "The admission service is unavailable; try again later.".to_string()
            )
        })?;

    if !response.status().is_success() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "The admission service is unavailable; try again later.".to_string()
        ));
    }

    let verdict: AdmissionHookResponse = response.json().await.map_err(|e| {
        tracing::warn!(error = %e, "the admission hook answered malformed JSON");

        (
            StatusCode::SERVICE_UNAVAILABLE,
            "The admission service is unavailable; try again later.".to_string()
        )
    })?;

    if verdict.allow {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            verdict.reason.unwrap_or("The request was refused by the server's admission policy.".to_string())
        ))
    }
}

/// Deliver an event to the events hook, when one is configured: fire-and-forget
/// from the handler's perspective, at-least-once towards the hook — retried with
/// backoff, logged (never failed-on) when the hook stays down.
fn emit_event(state: &Arc<AppState>, event: HookEvent) {
    let Some(hook) = state.events_hook.clone() else {
        return;
    };

    let state = Arc::clone(state);

    tokio::spawn(async move {
        // ~1 + 5 + 25 + 125 seconds of patience before giving up.
        let mut delay = std::time::Duration::from_secs(1);

        for attempt in 1..=5 {
            match post_hook(&state, &hook, "event", &event).await {
                Ok(response) if response.status().is_success() => return,
                Ok(response) => tracing::warn!(
                    event = event.event,
                    status = %response.status(),
                    attempt,
                    "the events hook refused the event"
                ),
                Err(reason) => tracing::warn!(
                    event = event.event,
                    reason,
                    attempt,
                    "the events hook is unreachable"
                ),
            }

            if attempt < 5 {
                tokio::time::sleep(delay).await;
                delay *= 5;
            }
        }

        tracing::error!(
            event = event.event,
            "an event was dropped after 5 delivery attempts"
        );
    });
}

/// Look up an operator's user record in this warehouse's office (runs under the
/// warehouse's storage-root scope). `None` when the warehouse has no trust yet — no
/// office means no roles, and the transport token is the whole gate.
fn office_user_of(identifier: &str) -> Result<Option<office_utils::UserRecord>, HandlerError> {
    if office_utils::read_trust_anchor().map_err(internal)?.is_none() {
        return Ok(None);
    }

    let state = office_utils::read_office_state().map_err(internal)?;

    // The bootstrap window: the anchor is set but the office itself has not been
    // lifted yet (a lift PUTs trust before it uploads the office chain). With no
    // roster there are no roles to enforce — the token stays the gate, and the
    // office ref update still verifies the chain against the anchor, so only the
    // enrolling operator's office can land.
    if state.users.is_empty() {
        return Ok(None);
    }

    let user = state.users.into_iter()
        .find(|user| user.identifier == identifier)
        .ok_or((
            StatusCode::FORBIDDEN,
            format!("\"{}\" is not enrolled in this warehouse's office.", identifier)
        ))?;

    Ok(Some(user))
}

/// Authorize an upload (objects, signatures) — runs under the warehouse's storage-root
/// scope. Uploads are not pallet-scoped, so any non-reader may upload; the ref update
/// is where pallet grants bite.
fn require_uploader(principal: &Principal) -> Result<(), HandlerError> {
    let Principal::Operator(identifier) = principal else {
        return Ok(());
    };

    match office_user_of(identifier)? {
        Some(user) if user.role == office_utils::Role::Reader => Err((
            StatusCode::FORBIDDEN,
            format!("\"{}\" is a reader; readers cannot upload.", identifier)
        )),
        _ => Ok(()),
    }
}

/// Get a path parameter of the matched route.
fn param(params: &PathParams, key: &str) -> Result<String, HandlerError> {
    params.get(key)
        .cloned()
        .ok_or_else(|| internal(format!("The \"{}\" path parameter is missing.", key)))
}

/// Validate a warehouse id: a single safe path component. Stricter than pallet names
/// (no `/`, no leading `.`): the id names a folder directly under the served base.
fn validate_warehouse_id(id: &str) -> Result<(), HandlerError> {
    let error = |reason: &str| Err(unprocessable(format!(
        "\"{}\" is not a valid warehouse id: {}.", id, reason
    )));

    if id.is_empty() {
        return error("it is empty");
    }

    if id.len() > 100 {
        return error("it is longer than 100 characters");
    }

    if id.starts_with('.') || id.starts_with('-') {
        return error("it must not start with \".\" or \"-\"");
    }

    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-') {
        return error("only ASCII letters, digits, \".\", \"_\" and \"-\" are allowed");
    }

    Ok(())
}

/// Resolve the warehouse a request addresses: the served one in single mode, the one
/// named by the `warehouse` path parameter in multi mode. A warehouse that does not
/// exist on disk is a `404` — creation is explicit (`PUT /warehouses/{id}`), never a
/// side effect.
fn resolve_warehouse(state: &AppState,
                     params: &PathParams) -> Result<Arc<WarehouseHandle>, HandlerError> {
    let base = match &state.mode {
        ServeMode::Single(handle) => return Ok(Arc::clone(handle)),
        ServeMode::Multi { base } => base,
    };

    let id = param(params, "warehouse")?;
    validate_warehouse_id(&id)?;

    let mut registry = state.warehouses.lock()
        .map_err(|_| internal("The warehouse registry lock is poisoned.".to_string()))?;

    if let Some(handle) = registry.get(&id) {
        return Ok(Arc::clone(handle));
    }

    let root = base.join(&id);

    if !root.join(FOLDER_NAME_FORKLIFT_ROOT).is_dir() {
        return Err((
            StatusCode::NOT_FOUND,
            format!(
                "No warehouse \"{}\" exists on this server. Create it first: \
                PUT /warehouses/{}.",
                id, id
            )
        ));
    }

    // Acquire this warehouse's serve lock as it is first served, and hold it in the cached handle
    // for the server's lifetime. Refuses (503) if a `gc`/`bundle` is running against this
    // warehouse, or another server already serves it — serving it then would be unsafe.
    let handle = Arc::new(
        WarehouseHandle::serving(root).map_err(|e| (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Warehouse \"{}\" is temporarily unavailable (under maintenance): {}", id, e),
        ))?
    );
    registry.insert(id, Arc::clone(&handle));

    Ok(handle)
}

/// Run blocking storage work on the blocking pool, inside the warehouse's storage-root
/// scope. The scope is thread-local and the closure runs synchronously on one blocking
/// thread, so it can never leak into another request.
async fn blocking<T: Send + 'static>(
    warehouse: Arc<WarehouseHandle>,
    work: impl FnOnce() -> Result<T, HandlerError> + Send + 'static
) -> Result<T, HandlerError> {
    tokio::task::spawn_blocking(move || {
        let _scope = StorageRootScope::enter(&warehouse.root);

        work()
    })
        .await
        .map_err(|e| internal(format!("A storage task failed: {}", e)))?
}

/// `PUT /warehouses/{id}` — create a warehouse (multi mode only). Explicitly gated:
/// only token-protected servers accept creation (an open server would be a junk farm),
/// and a lift can never create a warehouse by accident. Idempotent: `201` when
/// created, `200` when the warehouse already exists.
async fn put_warehouse(State(state): State<Arc<AppState>>,
                       headers: HeaderMap,
                       Path(params): Path<PathParams>) -> Response {
    let principal = match check_auth(&state, &headers).await {
        Ok(principal) => principal,
        Err(error) => return error_response(error),
    };

    let ServeMode::Multi { base } = &state.mode else {
        return error_response((
            StatusCode::NOT_FOUND,
            "This server serves a single warehouse; there is nothing to create.".to_string()
        ));
    };

    // Creation is a server-administration act: only the static token qualifies (an
    // open server would be a junk farm, and operator tokens are warehouse-scoped
    // identities, not server admins).
    if principal != Principal::Static {
        return error_response((
            StatusCode::FORBIDDEN,
            "Warehouse creation requires the server's static token (start the server \
            with --token).".to_string()
        ));
    }

    let id = match param(&params, "warehouse").and_then(|id| {
        validate_warehouse_id(&id).map(|_| id)
    }) {
        Ok(id) => id,
        Err(error) => return error_response(error),
    };

    if let Err(error) = check_admission(&state, &params, &principal, "warehouse_create", None).await {
        return error_response(error);
    }

    let root = base.join(&id);
    let handle = Arc::new(WarehouseHandle::new(root.clone()));

    let result = blocking(Arc::clone(&handle), move || {
        let existed = root.join(FOLDER_NAME_FORKLIFT_ROOT).is_dir();

        std::fs::create_dir_all(&root)
            .map_err(|e| internal(format!("Error while creating the warehouse folder: {}", e)))?;

        warehouse_utils::prepare_warehouse().map_err(internal)?;

        Ok(!existed)
    }).await;

    match result {
        Ok(created) => {
            // Register the handle unless a concurrent request already did: exactly one
            // handle (one write mutex) may ever exist per warehouse.
            match state.warehouses.lock() {
                Ok(mut registry) => { registry.entry(id.clone()).or_insert(handle); }
                Err(_) => return error_response(internal(
                    "The warehouse registry lock is poisoned.".to_string()
                )),
            }

            if created {
                let mut event = HookEvent::new(EVENT_WAREHOUSE_CREATED);
                event.warehouse = Some(id);
                emit_event(&state, event);

                StatusCode::CREATED.into_response()
            } else {
                StatusCode::OK.into_response()
            }
        }
        Err(error) => error_response(error),
    }
}

/// `GET /v1/warehouse` — the handshake: protocol version, refs and trust.
async fn get_warehouse(State(state): State<Arc<AppState>>,
                       headers: HeaderMap,
                       Path(params): Path<PathParams>) -> Response {
    if let Err(error) = check_auth(&state, &headers).await {
        return error_response(error);
    }

    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    let result = blocking(warehouse, move || {
        let mut pallets = std::collections::BTreeMap::new();

        // Both namespaces travel in one map, keyed by the qualified reference form: user
        // pallets bare, meta pallets as `@office` — so clients (and future meta
        // pallets) route by namespace, never by a hard-coded name.
        for (pallet_ref, head) in pallet_utils::all_pallet_refs().map_err(internal)? {
            pallets.insert(pallet_ref.to_wire(), head);
        }

        Ok(WarehouseInfo {
            protocol: PROTOCOL_VERSION.to_string(),
            default_pallet: pallet_utils::get_current_pallet_name().map_err(internal)?,
            pallets,
            trust: office_utils::read_trust_anchor()
                .map_err(internal)?
                .map(|anchor| TrustAnchorDto::from(&anchor)),
        })
    }).await;

    match result {
        Ok(info) => Json(info).into_response(),
        Err(error) => error_response(error),
    }
}

/// `POST /v1/objects/missing` — which of these objects does the remote lack?
async fn post_missing(State(state): State<Arc<AppState>>,
                      headers: HeaderMap,
                      Path(params): Path<PathParams>,
                      Json(request): Json<MissingObjectsRequest>) -> Response {
    if let Err(error) = check_auth(&state, &headers).await {
        return error_response(error);
    }

    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    if request.hashes.len() > MAX_MISSING_BATCH {
        return error_response(unprocessable(format!(
            "At most {} hashes per request; batch larger sets.",
            MAX_MISSING_BATCH
        )));
    }

    let result = blocking(warehouse, move || {
        let mut missing: Vec<String> = Vec::new();

        for hash in request.hashes {
            if !file_utils::does_object_exist(&hash).map_err(internal)? {
                missing.push(hash);
            }
        }

        Ok(MissingObjectsResponse { missing })
    }).await;

    match result {
        Ok(body) => Json(body).into_response(),
        Err(error) => error_response(error),
    }
}

/// `POST /v1/objects/upload-targets` — the body-less upload negotiation. The direct head
/// serves object bytes itself and verifies every `PUT` inline, so it has no staging prefix and
/// no presigned targets: it answers with what it already has (`present`) and every missing hash
/// in `direct` (upload those to `/v1/objects/{hash}` for inline verification), with `targets`
/// empty. That is exactly what lets one client code path serve both this head and a storage-
/// backed one (`REMOTE_PROTOCOL.md`), and it subsumes `missing` on the upload path.
async fn post_upload_targets(State(state): State<Arc<AppState>>,
                             headers: HeaderMap,
                             Path(params): Path<PathParams>,
                             Json(request): Json<UploadTargetsRequest>) -> Response {
    if let Err(error) = check_auth(&state, &headers).await {
        return error_response(error);
    }

    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    if request.hashes.len() > MAX_UPLOAD_TARGETS_BATCH {
        return error_response(unprocessable(format!(
            "At most {} hashes per upload-targets request; batch larger sets.",
            MAX_UPLOAD_TARGETS_BATCH
        )));
    }

    let result = blocking(warehouse, move || {
        let mut response = UploadTargetsResponse {
            present: Vec::new(),
            targets: std::collections::BTreeMap::new(),
            direct: Vec::new(),
        };

        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        for hash in request.hashes {
            if !seen.insert(hash.clone()) {
                continue;
            }

            if file_utils::does_object_exist(&hash).map_err(unprocessable)? {
                response.present.push(hash);
            } else {
                // No staging prefix here — the client PUTs the body and the head verifies it
                // inline (invariant 1 is upheld by the inline hash check, not a staging copy).
                response.direct.push(hash);
            }
        }

        Ok(response)
    }).await;

    match result {
        Ok(body) => Json(body).into_response(),
        Err(error) => error_response(error),
    }
}

/// `POST /v1/objects/batch` — many objects in one round trip, as a bundle-format
/// stream (the incremental counterpart of `GET /v1/bundles/latest`). Objects the
/// warehouse lacks are simply absent from the stream; the client notices and falls
/// back to loose fetches.
async fn post_objects_batch(State(state): State<Arc<AppState>>,
                            headers: HeaderMap,
                            Path(params): Path<PathParams>,
                            Json(request): Json<MissingObjectsRequest>) -> Response {
    if let Err(error) = check_auth(&state, &headers).await {
        return error_response(error);
    }

    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    if request.hashes.len() > MAX_MISSING_BATCH {
        return error_response(unprocessable(format!(
            "At most {} hashes per request; batch larger sets.",
            MAX_MISSING_BATCH
        )));
    }

    let result = blocking(warehouse, move || {
        bundle_utils::build_partial_bundle(&request.hashes).map_err(internal)
    }).await;

    match result {
        Ok(bytes) => (
            [
                (axum::http::header::CONTENT_TYPE, "application/octet-stream"),
                // The payload is already a zstd stream; the compression layer must
                // not wrap it again.
                (axum::http::header::CONTENT_ENCODING, "identity"),
            ],
            bytes,
        ).into_response(),
        Err(error) => error_response(error),
    }
}

/// `GET /v1/objects/{hash}` — the raw object bytes.
async fn get_object(State(state): State<Arc<AppState>>,
                    headers: HeaderMap,
                    Path(params): Path<PathParams>) -> Response {
    if let Err(error) = check_auth(&state, &headers).await {
        return error_response(error);
    }

    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    let hash = match param(&params, "hash") {
        Ok(hash) => hash,
        Err(error) => return error_response(error),
    };

    let result = blocking(warehouse, move || {
        if !file_utils::does_object_exist(&hash).map_err(unprocessable)? {
            return Err((StatusCode::NOT_FOUND, format!("No object {} exists.", hash)));
        }

        file_utils::retrieve_object_by_hash(&hash).map_err(internal)
    }).await;

    match result {
        Ok(bytes) => bytes.into_response(),
        Err(error) => error_response(error),
    }
}

/// `PUT /v1/objects/{hash}` — store an object; the hash is verified before anything
/// becomes fetchable (the non-negotiable invariant of the protocol).
async fn put_object(State(state): State<Arc<AppState>>,
                    headers: HeaderMap,
                    Path(params): Path<PathParams>,
                    body: Bytes) -> Response {
    let principal = match check_auth(&state, &headers).await {
        Ok(principal) => principal,
        Err(error) => return error_response(error),
    };

    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    let hash = match param(&params, "hash") {
        Ok(hash) => hash,
        Err(error) => return error_response(error),
    };

    if let Err(error) = check_admission(&state, &params, &principal, "upload", None).await {
        return error_response(error);
    }

    let result = blocking(warehouse, move || {
        require_uploader(&principal)?;

        object_utils::store_object_bytes(&hash, &body).map_err(unprocessable)
    }).await;

    match result {
        Ok(true) => StatusCode::CREATED.into_response(),
        Ok(false) => StatusCode::OK.into_response(),
        Err(error) => error_response(error),
    }
}

/// `GET /v1/signatures/{hash}` — a parcel's signature sidecar.
async fn get_signature(State(state): State<Arc<AppState>>,
                       headers: HeaderMap,
                       Path(params): Path<PathParams>) -> Response {
    if let Err(error) = check_auth(&state, &headers).await {
        return error_response(error);
    }

    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    let hash = match param(&params, "hash") {
        Ok(hash) => hash,
        Err(error) => return error_response(error),
    };

    let result = blocking(warehouse, move || {
        sign_utils::load_raw_parcel_signature(&hash).map_err(internal)
    }).await;

    match result {
        Ok(Some(bytes)) => bytes.into_response(),
        Ok(None) => error_response((StatusCode::NOT_FOUND, "The parcel carries no signature.".to_string())),
        Err(error) => error_response(error),
    }
}

/// `PUT /v1/signatures/{hash}` — store a signature sidecar. Structure is validated
/// here; whether the signature *verifies* is decided at ref update time. A conflicting
/// sidecar for an already-signed parcel is refused (signatures are immutable).
async fn put_signature(State(state): State<Arc<AppState>>,
                       headers: HeaderMap,
                       Path(params): Path<PathParams>,
                       body: Bytes) -> Response {
    let principal = match check_auth(&state, &headers).await {
        Ok(principal) => principal,
        Err(error) => return error_response(error),
    };

    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    let hash = match param(&params, "hash") {
        Ok(hash) => hash,
        Err(error) => return error_response(error),
    };

    if let Err(error) = check_admission(&state, &params, &principal, "upload", None).await {
        return error_response(error);
    }

    let result = blocking(warehouse, move || {
        require_uploader(&principal)?;

        let existed = sign_utils::load_raw_parcel_signature(&hash).map_err(internal)?;

        match sign_utils::store_raw_parcel_signature(&hash, &body) {
            Ok(()) => Ok(existed.is_none()),
            Err(message) if message.contains("immutable") => Err((StatusCode::CONFLICT, message)),
            Err(message) => Err(unprocessable(message)),
        }
    }).await;

    match result {
        Ok(true) => StatusCode::CREATED.into_response(),
        Ok(false) => StatusCode::OK.into_response(),
        Err(error) => error_response(error),
    }
}

/// `PUT /v1/trust` — establish the trust anchor: the same one-way door it is locally.
/// The one sanctioned way through the door is a re-genesis anchor (§8.7), and only
/// the server's own operator authority (the static token — the authority *outside*
/// the dead chain) may sanction it; per-operator tokens may not, since their roles
/// come from exactly the chain being replaced.
async fn put_trust(State(state): State<Arc<AppState>>,
                   headers: HeaderMap,
                   Path(params): Path<PathParams>,
                   Json(anchor): Json<TrustAnchorDto>) -> Response {
    let principal = match check_auth(&state, &headers).await {
        Ok(principal) => principal,
        Err(error) => return error_response(error),
    };

    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    let warehouse_id = params.get("warehouse").cloned();

    let result = blocking(Arc::clone(&warehouse), move || {
        // Trust establishment is a one-way door: without the write lock, two first
        // contacts could both read "no anchor" and race their differing geneses.
        let _guard = warehouse.writes.lock()
            .map_err(|_| internal("The write lock is poisoned.".to_string()))?;

        match office_utils::read_trust_anchor().map_err(internal)? {
            Some(existing) => {
                if TrustAnchorDto::from(&existing) == anchor {
                    return Ok(None);
                }

                // A re-genesis anchor: it must name the current genesis as its prior
                // and adopt the office head exactly as it stands — nothing of the old
                // chain may be silently dropped.
                let is_regenesis = anchor.prior_genesis.as_deref() == Some(existing.genesis.as_str());

                if !is_regenesis {
                    return Err((
                        StatusCode::CONFLICT,
                        "This warehouse already has a different trust anchor; trust is \
                        a one-way door and cannot be replaced.".to_string()
                    ));
                }

                if !matches!(principal, Principal::Static | Principal::Open) {
                    return Err((
                        StatusCode::FORBIDDEN,
                        "A trust reset (re-genesis) must be sanctioned by the server \
                        operator: only the static token may replace the anchor. \
                        Per-operator tokens derive their authority from the chain \
                        being replaced.".to_string()
                    ));
                }

                let office_head = pallet_utils::get_meta_pallet_head(office_utils::OFFICE_PALLET_NAME)
                    .map_err(internal)?;

                if anchor.adopts.as_deref() != office_head.as_deref() {
                    return Err(unprocessable(format!(
                        "The re-genesis anchor adopts office head {}, but this \
                        warehouse's office head is {}. The reset would drop history; \
                        re-run the re-genesis from a warehouse in sync with this one.",
                        anchor.adopts.as_deref().unwrap_or("(none)"),
                        office_head.as_deref().unwrap_or("(unborn)")
                    )));
                }

                tracing::warn!(
                    old_genesis = existing.genesis,
                    new_genesis = anchor.genesis,
                    adopts = anchor.adopts.as_deref().unwrap_or(""),
                    "TRUST RESET: the trust anchor was replaced by a re-genesis"
                );

                office_utils::replace_trust_anchor(&anchor.to_anchor()).map_err(internal)?;

                let mut event = HookEvent::new(EVENT_TRUST_RESET);
                event.detail = Some(anchor.genesis.clone());
                Ok(Some(event))
            }
            None => {
                office_utils::write_trust_anchor(&anchor.to_anchor()).map_err(internal)?;

                let mut event = HookEvent::new(EVENT_TRUST_ESTABLISHED);
                event.detail = Some(anchor.genesis.clone());
                Ok(Some(event))
            }
        }
    }).await;

    match result {
        Ok(Some(mut event)) => {
            event.warehouse = warehouse_id;
            emit_event(&state, event);

            StatusCode::CREATED.into_response()
        }
        Ok(None) => StatusCode::OK.into_response(),
        Err(error) => error_response(error),
    }
}

/// `POST /v1/pallets/{name}` — the CAS ref update: the commit point of a lift, and the
/// place where the server enforces everything (DESIGN.html §4.2 step 6): presence of
/// the full closure, fast-forward-ness, and — on trusted warehouses — the same audit
/// the CLI runs offline.
async fn post_ref_update(State(state): State<Arc<AppState>>,
                         headers: HeaderMap,
                         Path(params): Path<PathParams>,
                         Json(request): Json<RefUpdateRequest>) -> Response {
    let principal = match check_auth(&state, &headers).await {
        Ok(principal) => principal,
        Err(error) => return error_response(error),
    };

    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    let name = match param(&params, "name") {
        Ok(name) => name,
        Err(error) => return error_response(error),
    };

    if let Err(error) = check_admission(&state, &params, &principal, "ref_update", Some(&name)).await {
        return error_response(error);
    }

    let warehouse_id = params.get("warehouse").cloned();

    let acting_operator = match &principal {
        Principal::Operator(identifier) => Some(identifier.clone()),
        _ => None,
    };

    let handle = Arc::clone(&warehouse);

    let result = blocking(Arc::clone(&warehouse), move || {
        // Parse the qualified reference: a meta pallet arrives as `@office`. The server
        // routes by *namespace*, never by a hard-coded name (DESIGN.html §3.3).
        let pallet_ref = pallet_utils::PalletRef::parse(&name).map_err(unprocessable)?;
        let namespace = pallet_ref.namespace;
        let bare = pallet_ref.name.clone();
        let is_meta = namespace == pallet_utils::PalletNamespace::Meta;
        let is_office = is_meta && bare == OFFICE_PALLET_NAME;

        // Transport authorization: may this principal move this ref? The
        // role and the pallet grants come from the office — signed, tracked metadata.
        if let Principal::Operator(identifier) = &principal {
            if let Some(user) = office_user_of(identifier)? {
                let allowed = if is_meta {
                    // Anyone but a reader may transport a meta pallet's history; whether
                    // its *content* is authorized is verified below, per parcel, against
                    // the signer's role.
                    user.role != office_utils::Role::Reader
                } else {
                    user.may_write_pallet(&bare)
                };

                if !allowed {
                    return Err((
                        StatusCode::FORBIDDEN,
                        format!(
                            "\"{}\" ({}) may not move pallet \"{}\".",
                            identifier, user.role.as_str(), name
                        )
                    ));
                }
            }
        }

        // The CAS read-check-write is one critical section.
        let _guard = handle.writes.lock()
            .map_err(|_| internal("The write lock is poisoned.".to_string()))?;

        let current = pallet_utils::get_pallet_head_in(namespace, &bare).map_err(internal)?;

        if current != request.old_head {
            return Err((
                StatusCode::CONFLICT,
                format!(
                    "The pallet moved: its head is {}, not {}. Lower and retry.",
                    current.as_deref().unwrap_or("unborn"),
                    request.old_head.as_deref().unwrap_or("unborn")
                )
            ));
        }

        if !file_utils::does_object_exist(&request.new_head).map_err(internal)? {
            return Err(unprocessable(format!(
                "The new head {} has not been uploaded.",
                request.new_head
            )));
        }

        // A ref must never point at missing history.
        audit_utils::verify_parcel_closure(&request.new_head, request.old_head.as_deref())
            .map_err(unprocessable)?;

        let anchor = office_utils::read_trust_anchor().map_err(internal)?;

        if let Some(old_head) = &request.old_head {
            // The one sanctioned non-fast-forward: the office lift right after a
            // re-genesis, where the (already replaced) anchor adopts exactly the head
            // being moved away from — the old chain is pinned, not dropped.
            let adopted_reset = is_office
                && anchor.as_ref().and_then(|anchor| anchor.adopts.as_deref()) == Some(old_head.as_str());

            if !adopted_reset && !merge_utils::is_ancestor(old_head, &request.new_head).map_err(internal)? {
                return Err((
                    StatusCode::CONFLICT,
                    "The update is not a fast-forward; the protocol has no force \
                    push. Lower, consolidate, and lift the merge.".to_string()
                ));
            }
        }

        // The events of this update (delivered after the commit): the ref move, plus
        // any revocations an office update carries — the old office state must be
        // read *before* the head moves.
        let mut events: Vec<HookEvent> = Vec::new();

        let mut ref_event = HookEvent::new(EVENT_PALLET_UPDATED);
        ref_event.warehouse = warehouse_id.clone();
        ref_event.operator = acting_operator.clone();
        ref_event.pallet = Some(name.clone());
        ref_event.old_head = request.old_head.clone();
        ref_event.new_head = Some(request.new_head.clone());
        events.push(ref_event);

        if is_office {
            // The office chain carries the keys; it must verify against the anchor.
            let anchor = anchor.ok_or(unprocessable(
                "Establish the trust anchor (PUT /v1/trust) before lifting the office.".to_string()
            ))?;

            let new_office_state = audit_utils::verify_office_chain_memoized(&anchor, &request.new_head)
                .map_err(unprocessable)?;

            // Authentic is not authorized: every new office parcel must stay within
            // its *signer's* privileges (admins change anything; everyone else only
            // their own keys). This is a content invariant — it holds no matter which
            // token transported the chain.
            audit_utils::verify_office_privileges(
                &anchor,
                request.old_head.as_deref(),
                &request.new_head
            ).map_err(|reason| (StatusCode::FORBIDDEN, reason))?;

            // Newly revoked keys (active before, revoked after) become events.
            let old_office_state = match request.old_head.is_some() {
                true => office_utils::read_office_state().map_err(internal)?,
                false => office_utils::OfficeState { users: Vec::new(), keys: Vec::new() },
            };

            for key in &new_office_state.keys {
                let was_active = old_office_state.find_key(&key.key_id)
                    .map(|old| old.is_active())
                    .unwrap_or(true);

                if key.retired_at.is_some() && was_active {
                    let mut event = HookEvent::new(EVENT_KEY_REVOKED);
                    event.warehouse = warehouse_id.clone();
                    event.operator = Some(key.operator.clone());
                    event.key_id = Some(key.key_id.clone());
                    event.detail = key.revocation_reason
                        .map(|reason| reason.as_str().to_string());
                    events.push(event);
                }
            }
        } else if let Some(anchor) = anchor {
            // A trusted warehouse accepts nothing a local audit would reject. This is the
            // user-pallet path (the office took the branch above); a future non-office
            // meta pallet will bring its own verification here.
            let office_head = pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME)
                .map_err(internal)?
                .ok_or(unprocessable(
                    "Trust is established but the office pallet is missing; lift the \
                    office first.".to_string()
                ))?;

            let office_state = audit_utils::verify_office_chain_memoized(&anchor, &office_head)
                .map_err(unprocessable)?;

            // Incremental: everything reachable from old_head was verified when
            // old_head was committed, so only the new slice is audited.
            audit_utils::verify_pallet_history(
                &request.new_head,
                &anchor,
                &office_state,
                request.old_head.as_deref()
            ).map_err(unprocessable)?;
        }

        pallet_utils::set_pallet_head_in(namespace, &bare, &request.new_head).map_err(internal)?;

        Ok(events)
    }).await;

    match result {
        Ok(events) => {
            for event in events {
                emit_event(&state, event);
            }

            maybe_rebuild_bundle(&state, warehouse);

            StatusCode::OK.into_response()
        }
        Err(error) => error_response(error),
    }
}

/// Kick off a background bundle rebuild when the accepted-lift counter reaches the
/// configured threshold. At most one rebuild runs per warehouse at a time; the bundle
/// is written atomically, so serving continues undisturbed throughout.
fn maybe_rebuild_bundle(state: &AppState, warehouse: Arc<WarehouseHandle>) {
    use std::sync::atomic::Ordering;

    let Some(threshold) = state.rebuild_after_lifts else {
        return;
    };

    let lifts = warehouse.lifts_since_bundle.fetch_add(1, Ordering::SeqCst) + 1;

    if lifts < threshold || warehouse.bundling.swap(true, Ordering::SeqCst) {
        return;
    }

    warehouse.lifts_since_bundle.store(0, Ordering::SeqCst);

    tokio::spawn(async move {
        let worker = Arc::clone(&warehouse);

        let result = tokio::task::spawn_blocking(move || {
            let _scope = StorageRootScope::enter(&worker.root);

            bundle_utils::build_bundle()
        }).await;

        match result {
            Ok(Ok(stats)) => tracing::info!(
                objects = stats.objects,
                deltas = stats.deltas,
                signatures = stats.signatures,
                "rebuilt the bundle"
            ),
            Ok(Err(error)) => tracing::error!(error, "the bundle rebuild failed"),
            Err(error) => tracing::error!(%error, "the bundle rebuild task failed"),
        }

        warehouse.bundling.store(false, Ordering::SeqCst);
    });
}

/// `POST /v1/resolve` — operator identifiers → display names (DESIGN.html §8.12).
/// Resolution is server-mediated precisely so its policy is enforced and not
/// advisory: the server authenticates the caller, then asks its resolution hook (when
/// configured) which of the requested names this caller may see. No hook — or nothing
/// the caller may see — is an empty map, and the client shows the pseudonymous
/// identifiers. This only ever feeds display; it is never a verification input.
async fn post_resolve(State(state): State<Arc<AppState>>,
                      headers: HeaderMap,
                      Path(params): Path<PathParams>,
                      Json(request): Json<ResolveRequest>) -> Response {
    let principal = match check_auth(&state, &headers).await {
        Ok(principal) => principal,
        Err(error) => return error_response(error),
    };

    // A request against a warehouse that does not exist is a `404`, like every other
    // endpoint (resolution reads no warehouse state, but the route is warehouse-scoped).
    if let Err(error) = resolve_warehouse(&state, &params) {
        return error_response(error);
    }

    let Some(hook) = &state.resolution_hook else {
        // No directory is configured: the server knows no names. The client degrades
        // to the pseudonymous identifiers — never an error.
        return Json(ResolveResponse { names: BTreeMap::new() }).into_response();
    };

    // The caller travels to the hook so a policy-aware directory can tier its answer
    // (§8.12: guests resolve nothing, members only shared-warehouse peers, admins
    // everyone). This is exactly what a client-held shared secret could never do —
    // the reason resolution is server-mediated.
    let caller = match &principal {
        Principal::Operator(identifier) => Some(identifier.clone()),
        _ => None,
    };

    let hook_request = ResolutionHookRequest { caller, identifiers: request.identifiers };

    let names = match post_hook(&state, hook, "resolution", &hook_request).await {
        Ok(response) if response.status().is_success() => {
            response.json::<ResolutionHookResponse>().await
                .map(|answer| answer.names)
                .unwrap_or_default()
        }
        // The resolution failure policy is "show pseudonyms": a hook that is down or
        // answers badly resolves to nothing, never a failed request (unlike the hot
        // authentication/admission hooks, which fail closed).
        Ok(_) => BTreeMap::new(),
        Err(reason) => {
            tracing::warn!(reason, "the resolution hook is unreachable; showing pseudonyms");
            BTreeMap::new()
        }
    };

    Json(ResolveResponse { names }).into_response()
}

/// `GET /v1/bundles/latest` — the most recent bundle, when one was built. The file is
/// streamed, never read into memory: bundles grow with the warehouse. The open handle
/// keeps serving the old file even if a rebuild replaces it mid-transfer.
async fn get_bundle(State(state): State<Arc<AppState>>,
                    headers: HeaderMap,
                    Path(params): Path<PathParams>) -> Response {
    if let Err(error) = check_auth(&state, &headers).await {
        return error_response(error);
    }

    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    // The path resolves synchronously under the scope; only the streaming is async.
    let path = {
        let _scope = StorageRootScope::enter(&warehouse.root);

        bundle_utils::get_latest_bundle_path()
    };

    match tokio::fs::File::open(&path).await {
        Ok(file) => {
            let stream = tokio_util::io::ReaderStream::new(file);

            (
                [
                    (axum::http::header::CONTENT_TYPE, "application/octet-stream"),
                    // Already a zstd stream; the compression layer must not re-wrap.
                    (axum::http::header::CONTENT_ENCODING, "identity"),
                ],
                Body::from_stream(stream),
            ).into_response()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            error_response((StatusCode::NOT_FOUND, "No bundle has been built.".to_string()))
        }
        Err(e) => error_response(internal(format!("Error while opening the bundle: {}", e))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use forklift_core::globals::{StorageRootScope, FOLDER_NAME_FORKLIFT_ROOT};
    use forklift_core::model::operator::Operator;
    use forklift_core::util::{office_utils, warehouse_utils};
    use forklift_core::util::office_utils::{IdentityClass, OfficeState, Role, TrustAnchor, UserRecord};

    // ---------------------------------------------------------------------------------
    // Shared test plumbing
    // ---------------------------------------------------------------------------------

    /// A fresh `AppState` for a given serving mode, with no auth/hooks configured (tests
    /// override individual fields with struct-update syntax).
    fn base_state(mode: ServeMode) -> AppState {
        AppState {
            mode,
            token: None,
            operator_tokens: HashMap::new(),
            rebuild_after_lifts: None,
            warehouses: Mutex::new(HashMap::new()),
            authentication_hook: None,
            admission_hook: None,
            events_hook: None,
            resolution_hook: None,
            http: reqwest::Client::new(),
            authentication_cache: Mutex::new(HashMap::new()),
            authentication_cache_ttl: std::time::Duration::from_secs(60),
        }
    }

    fn single_mode_state(root: PathBuf) -> AppState {
        base_state(ServeMode::Single(Arc::new(WarehouseHandle::new(root))))
    }

    fn headers_with_bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {}", token).parse().unwrap());
        headers
    }

    fn headers_with_raw(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", value.parse().unwrap());
        headers
    }

    /// A unique scratch directory for one test (never shared across tests, so parallel
    /// tests never collide on disk).
    fn scratch_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);

        let path = std::env::temp_dir().join(format!(
            "forklift-server-test-{}-{}-{}", name, std::process::id(), id
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// Start an in-process HTTP server that answers every POST with a fixed status and
    /// body, recording the most recent request body it received and how many requests it
    /// has handled. The hook protocol is ordinary HTTP, so this is a faithful stand-in for
    /// a hosting provider's hook endpoint — no subprocess needed.
    async fn spawn_hook(status: StatusCode, body: &'static str)
        -> (String, Arc<Mutex<Vec<u8>>>, Arc<AtomicUsize>) {
        let received = Arc::new(Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicUsize::new(0));

        let received_for_handler = Arc::clone(&received);
        let hits_for_handler = Arc::clone(&hits);

        let app = Router::new().route("/hook", post(move |bytes: Bytes| {
            let received = Arc::clone(&received_for_handler);
            let hits = Arc::clone(&hits_for_handler);

            async move {
                if let Ok(mut guard) = received.lock() {
                    *guard = bytes.to_vec();
                }
                hits.fetch_add(1, Ordering::SeqCst);

                (status, body).into_response()
            }
        }));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{}/hook", addr), received, hits)
    }

    /// A URL nothing listens on (bound, then immediately released) — a deterministic way
    /// to exercise the "hook unreachable" fail-closed path without depending on a magic
    /// port number staying free.
    fn unreachable_url() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{}/hook", addr)
    }

    // ---------------------------------------------------------------------------------
    // check_auth
    // ---------------------------------------------------------------------------------

    #[tokio::test]
    async fn no_auth_configured_is_fully_open() {
        let state = single_mode_state(PathBuf::from("/unused"));
        let principal = check_auth(&state, &HeaderMap::new()).await.unwrap();
        assert!(principal == Principal::Open);
    }

    #[tokio::test]
    async fn the_static_token_authenticates_as_static() {
        let state = AppState { token: Some("secret".to_string()), ..single_mode_state(PathBuf::from("/unused")) };
        let principal = check_auth(&state, &headers_with_bearer("secret")).await.unwrap();
        assert!(principal == Principal::Static);
    }

    #[tokio::test]
    async fn a_known_operator_token_authenticates_as_that_operator() {
        let mut operator_tokens = HashMap::new();
        operator_tokens.insert("tok-bob".to_string(), "bob".to_string());
        let state = AppState { operator_tokens, ..single_mode_state(PathBuf::from("/unused")) };

        let principal = check_auth(&state, &headers_with_bearer("tok-bob")).await.unwrap();
        assert!(principal == Principal::Operator("bob".to_string()));
    }

    #[tokio::test]
    async fn an_unknown_token_with_no_hook_is_unauthorized() {
        let state = AppState { token: Some("secret".to_string()), ..single_mode_state(PathBuf::from("/unused")) };
        let error = check_auth(&state, &headers_with_bearer("guess")).await.err().unwrap();
        assert_eq!(error.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_missing_authorization_header_is_unauthorized_when_auth_is_configured() {
        let state = AppState { token: Some("secret".to_string()), ..single_mode_state(PathBuf::from("/unused")) };
        let error = check_auth(&state, &HeaderMap::new()).await.err().unwrap();
        assert_eq!(error.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_non_bearer_scheme_is_unauthorized() {
        let state = AppState { token: Some("secret".to_string()), ..single_mode_state(PathBuf::from("/unused")) };
        let error = check_auth(&state, &headers_with_raw("Basic secret")).await.err().unwrap();
        assert_eq!(error.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_hook_answer_authenticates_and_is_cached() {
        let (url, _received, hits) = spawn_hook(StatusCode::OK, r#"{"identifier":"bob"}"#).await;
        let state = AppState {
            authentication_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let first = check_auth(&state, &headers_with_bearer("tok")).await.unwrap();
        assert!(first == Principal::Operator("bob".to_string()));
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // A second request with the same token must reuse the cached answer, not
        // re-consult the hook (the whole point of the cache on this hot path).
        let second = check_auth(&state, &headers_with_bearer("tok")).await.unwrap();
        assert!(second == Principal::Operator("bob".to_string()));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_zero_ttl_cache_never_serves_a_stale_answer() {
        let (url, _received, hits) = spawn_hook(StatusCode::OK, r#"{"identifier":"bob"}"#).await;
        let state = AppState {
            authentication_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            authentication_cache_ttl: std::time::Duration::from_secs(0),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        check_auth(&state, &headers_with_bearer("tok")).await.unwrap();
        check_auth(&state, &headers_with_bearer("tok")).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2, "a zero-second TTL must re-check every time");
    }

    #[tokio::test]
    async fn a_hook_non_success_status_is_unauthorized() {
        let (url, _received, _hits) = spawn_hook(StatusCode::FORBIDDEN, "nope").await;
        let state = AppState {
            authentication_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let error = check_auth(&state, &headers_with_bearer("tok")).await.err().unwrap();
        assert_eq!(error.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_unreachable_authentication_hook_fails_closed() {
        let state = AppState {
            authentication_hook: Some(HookEndpoint { url: unreachable_url(), secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let error = check_auth(&state, &headers_with_bearer("tok")).await.err().unwrap();
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn a_malformed_authentication_hook_answer_fails_closed() {
        let (url, _received, _hits) = spawn_hook(StatusCode::OK, "not json").await;
        let state = AppState {
            authentication_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let error = check_auth(&state, &headers_with_bearer("tok")).await.err().unwrap();
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    // ---------------------------------------------------------------------------------
    // check_admission
    // ---------------------------------------------------------------------------------

    #[tokio::test]
    async fn no_admission_hook_always_admits() {
        let state = single_mode_state(PathBuf::from("/unused"));
        let result = check_admission(
            &state, &PathParams::new(), &Principal::Operator("bob".to_string()), "upload", None
        ).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn the_admission_hook_can_allow() {
        let (url, _received, _hits) = spawn_hook(StatusCode::OK, r#"{"allow":true}"#).await;
        let state = AppState {
            admission_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let result = check_admission(
            &state, &PathParams::new(), &Principal::Operator("bob".to_string()), "upload", None
        ).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn the_admission_hook_denies_with_its_reason() {
        let (url, _received, _hits) = spawn_hook(
            StatusCode::OK, r#"{"allow":false,"reason":"quota exceeded"}"#
        ).await;
        let state = AppState {
            admission_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let error = check_admission(
            &state, &PathParams::new(), &Principal::Operator("bob".to_string()), "upload", None
        ).await.unwrap_err();
        assert_eq!(error.0, StatusCode::FORBIDDEN);
        assert_eq!(error.1, "quota exceeded");
    }

    #[tokio::test]
    async fn a_denial_without_a_reason_uses_the_default_message() {
        let (url, _received, _hits) = spawn_hook(StatusCode::OK, r#"{"allow":false}"#).await;
        let state = AppState {
            admission_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let error = check_admission(
            &state, &PathParams::new(), &Principal::Operator("bob".to_string()), "upload", None
        ).await.unwrap_err();
        assert_eq!(error.0, StatusCode::FORBIDDEN);
        assert!(error.1.contains("admission policy"), "{}", error.1);
    }

    #[tokio::test]
    async fn an_unreachable_admission_hook_fails_closed() {
        let state = AppState {
            admission_hook: Some(HookEndpoint { url: unreachable_url(), secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let error = check_admission(
            &state, &PathParams::new(), &Principal::Operator("bob".to_string()), "upload", None
        ).await.unwrap_err();
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn a_non_success_admission_status_fails_closed() {
        let (url, _received, _hits) = spawn_hook(StatusCode::INTERNAL_SERVER_ERROR, "boom").await;
        let state = AppState {
            admission_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let error = check_admission(
            &state, &PathParams::new(), &Principal::Operator("bob".to_string()), "upload", None
        ).await.unwrap_err();
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn malformed_admission_json_fails_closed() {
        let (url, _received, _hits) = spawn_hook(StatusCode::OK, "not json").await;
        let state = AppState {
            admission_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let error = check_admission(
            &state, &PathParams::new(), &Principal::Operator("bob".to_string()), "upload", None
        ).await.unwrap_err();
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn the_admission_request_carries_the_tenant_operator_and_pallet() {
        let (url, received, _hits) = spawn_hook(StatusCode::OK, r#"{"allow":true}"#).await;
        let state = AppState {
            admission_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let mut params = PathParams::new();
        params.insert("warehouse".to_string(), "tenant-a".to_string());

        check_admission(
            &state, &params, &Principal::Operator("bob".to_string()), "ref_update", Some("main")
        ).await.unwrap();

        let body = received.lock().unwrap().clone();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["action"], "ref_update");
        assert_eq!(value["warehouse"], "tenant-a");
        assert_eq!(value["operator"], "bob");
        assert_eq!(value["pallet"], "main");
    }

    // ---------------------------------------------------------------------------------
    // validate_warehouse_id / parse_operator_tokens
    // ---------------------------------------------------------------------------------

    #[test]
    fn validate_warehouse_id_rejects_the_obvious_bad_shapes() {
        assert!(validate_warehouse_id("").is_err(), "empty");
        assert!(validate_warehouse_id(&"a".repeat(101)).is_err(), "too long");
        assert!(validate_warehouse_id(".hidden").is_err(), "leading dot");
        assert!(validate_warehouse_id("-flag").is_err(), "leading dash");
        assert!(validate_warehouse_id("tenant/a").is_err(), "path separator");
        assert!(validate_warehouse_id("tenant a").is_err(), "space");
    }

    #[test]
    fn validate_warehouse_id_accepts_ordinary_ids() {
        assert!(validate_warehouse_id("tenant-a").is_ok());
        assert!(validate_warehouse_id("tenant_a.1").is_ok());
        assert!(validate_warehouse_id(&"a".repeat(100)).is_ok(), "exactly 100 is fine");
    }

    #[test]
    fn parses_a_valid_operator_token_file() {
        let dir = scratch_dir("tokens-ok");
        let path = dir.join("tokens.toml");
        std::fs::write(&path, "[operators]\n\"tok-a\" = \"alice\"\n\"tok-b\" = \"bob\"\n").unwrap();

        let tokens = parse_operator_tokens(path.to_str().unwrap()).unwrap();
        assert_eq!(tokens.get("tok-a"), Some(&"alice".to_string()));
        assert_eq!(tokens.get("tok-b"), Some(&"bob".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_a_missing_token_file() {
        let path = std::env::temp_dir().join("forklift-server-test-tokens-does-not-exist.toml");
        let _ = std::fs::remove_file(&path);
        assert!(parse_operator_tokens(path.to_str().unwrap()).is_err());
    }

    #[test]
    fn rejects_a_token_file_that_is_not_valid_toml() {
        let dir = scratch_dir("tokens-bad-toml");
        let path = dir.join("tokens.toml");
        std::fs::write(&path, "not [ valid toml").unwrap();

        assert!(parse_operator_tokens(path.to_str().unwrap()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_a_token_file_with_no_operators_table() {
        let dir = scratch_dir("tokens-no-table");
        let path = dir.join("tokens.toml");
        std::fs::write(&path, "not_operators = 1\n").unwrap();

        let error = parse_operator_tokens(path.to_str().unwrap()).unwrap_err();
        assert!(error.contains("[operators]"), "{}", error);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_a_token_mapped_to_a_non_string_value() {
        let dir = scratch_dir("tokens-non-string");
        let path = dir.join("tokens.toml");
        std::fs::write(&path, "[operators]\n\"tok-a\" = 42\n").unwrap();

        assert!(parse_operator_tokens(path.to_str().unwrap()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------------------------------------------------------------------------
    // resolve_warehouse
    // ---------------------------------------------------------------------------------

    #[test]
    fn single_mode_always_resolves_the_one_served_handle() {
        let root = scratch_dir("resolve-single");
        let state = single_mode_state(root.clone());

        let first = resolve_warehouse(&state, &PathParams::new()).unwrap();
        let second = resolve_warehouse(&state, &PathParams::new()).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.root, root);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn multi_mode_404s_for_an_unknown_warehouse() {
        let base = scratch_dir("resolve-multi-404");
        let state = base_state(ServeMode::Multi { base: base.clone() });

        let mut params = PathParams::new();
        params.insert("warehouse".to_string(), "ghost".to_string());

        let error = resolve_warehouse(&state, &params).err().unwrap();
        assert_eq!(error.0, StatusCode::NOT_FOUND);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn multi_mode_422s_for_an_invalid_warehouse_id() {
        let base = scratch_dir("resolve-multi-422");
        let state = base_state(ServeMode::Multi { base: base.clone() });

        let mut params = PathParams::new();
        params.insert("warehouse".to_string(), "../escape".to_string());

        let error = resolve_warehouse(&state, &params).err().unwrap();
        assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn multi_mode_caches_the_handle_across_requests() {
        let base = scratch_dir("resolve-multi-cache");
        std::fs::create_dir_all(base.join("wh1").join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let state = base_state(ServeMode::Multi { base: base.clone() });

        let mut params = PathParams::new();
        params.insert("warehouse".to_string(), "wh1".to_string());

        let first = resolve_warehouse(&state, &params).unwrap();
        let second = resolve_warehouse(&state, &params).unwrap();
        assert!(Arc::ptr_eq(&first, &second), "the same warehouse must reuse one write mutex");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn multi_mode_keeps_tenants_isolated() {
        let base = scratch_dir("resolve-multi-tenants");
        std::fs::create_dir_all(base.join("wh1").join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        std::fs::create_dir_all(base.join("wh2").join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let state = base_state(ServeMode::Multi { base: base.clone() });

        let mut params1 = PathParams::new();
        params1.insert("warehouse".to_string(), "wh1".to_string());
        let mut params2 = PathParams::new();
        params2.insert("warehouse".to_string(), "wh2".to_string());

        let handle1 = resolve_warehouse(&state, &params1).unwrap();
        let handle2 = resolve_warehouse(&state, &params2).unwrap();

        assert!(!Arc::ptr_eq(&handle1, &handle2), "two tenants must never share a handle");
        assert_ne!(handle1.root, handle2.root);
        assert_eq!(state.warehouses.lock().unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&base);
    }

    // ---------------------------------------------------------------------------------
    // office_user_of / require_uploader (transport authorization)
    // ---------------------------------------------------------------------------------

    /// Serializes every test that touches key generation/signing: `sign_utils` resolves
    /// its private-key directory from the `FORKLIFT_KEYS_DIR` environment variable on
    /// every call, which is process-global (not scoped by `StorageRootScope`). Holding
    /// this lock for such a test's whole body keeps two tests from ever pointing the
    /// same process at different key directories at once. Tests that never sign
    /// anything (most of this file) never take it and run fully in parallel.
    static KEYS_ENV_LOCK: Mutex<()> = Mutex::new(());

    /// A real, cryptographically valid office chain: a genesis admin (self-endorsed
    /// identity root, exactly like `office enroll`), with trust established. Building it
    /// through the same `office_utils`/`sign_utils` calls the CLI uses means the chain
    /// this produces is exactly as verifiable as a real one — not a shortcut that only
    /// happens to satisfy `office_user_of`.
    struct OfficeFixture {
        root: PathBuf,
        _scope: StorageRootScope,
        _keys_lock: std::sync::MutexGuard<'static, ()>,
        admin: Operator,
        admin_key_id: String,
    }

    impl OfficeFixture {
        fn genesis(name: &str) -> OfficeFixture {
            let keys_lock = KEYS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

            let root = scratch_dir(name);
            let keys_dir = root.join("keys");
            std::fs::create_dir_all(&keys_dir).unwrap();
            std::env::set_var("FORKLIFT_KEYS_DIR", &keys_dir);

            let scope = StorageRootScope::enter(&root);
            warehouse_utils::prepare_warehouse().unwrap();

            let admin = Operator { name: "alice".to_string(), identifier: "alice".to_string() };
            let (key_id, public_key) = forklift_core::util::sign_utils::generate_keypair(&admin.identifier).unwrap();
            let pop = office_utils::sign_key_pop(&key_id, &public_key, &admin.identifier).unwrap();
            let root_key = office_utils::endorse_key(&public_key, &admin.identifier, &key_id, &pop, 1_700_000_000).unwrap();

            let state = OfficeState {
                users: vec![UserRecord {
                    identifier: admin.identifier.clone(),
                    enrolled_at: 1_700_000_000,
                    role: Role::Admin,
                    pallets: Vec::new(),
                    identity_root: key_id.clone(),
                    class: IdentityClass::Human,
                    supervisor: None,
                }],
                keys: vec![root_key],
            };

            let genesis = office_utils::stack_office_parcel(
                &state, &admin, "genesis".to_string(), &key_id
            ).unwrap();

            office_utils::write_trust_anchor(&TrustAnchor {
                genesis,
                enabled_at: 1_700_000_000,
                boundary: Vec::new(),
                prior_genesis: None,
                adopts: None,
            }).unwrap();

            OfficeFixture { root, _scope: scope, _keys_lock: keys_lock, admin, admin_key_id: key_id }
        }

        /// Admit an additional user onto the chain (mirrors `office admit`).
        fn admit(&self,
                identifier: &str,
                role: Role,
                pallets: Vec<String>,
                class: IdentityClass,
                supervisor: Option<String>) {
            let mut state = office_utils::read_office_state().unwrap();

            let (key_id, public_key) = forklift_core::util::sign_utils::generate_keypair(identifier).unwrap();
            let pop = office_utils::sign_key_pop(&key_id, &public_key, identifier).unwrap();
            let key = office_utils::endorse_key(
                &public_key, identifier, &self.admin_key_id, &pop, 1_700_000_001
            ).unwrap();

            state.users.push(UserRecord {
                identifier: identifier.to_string(),
                enrolled_at: 1_700_000_001,
                role,
                pallets,
                identity_root: key_id,
                class,
                supervisor,
            });
            state.keys.push(key);

            office_utils::stack_office_parcel(
                &state, &self.admin, format!("admit {}", identifier), &self.admin_key_id
            ).unwrap();
        }
    }

    impl Drop for OfficeFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn no_trust_anchor_means_office_user_of_is_none() {
        let root = scratch_dir("office-none-anchor");
        let _scope = StorageRootScope::enter(&root);
        std::fs::create_dir_all(root.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();

        assert!(office_user_of("anyone").unwrap().is_none());
        assert!(require_uploader(&Principal::Operator("anyone".to_string())).is_ok());

        drop(_scope);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_established_anchor_with_no_office_yet_is_the_bootstrap_window() {
        // Trust set, but the office pallet itself has not been lifted (no key generation
        // involved, so this test needs no `FORKLIFT_KEYS_DIR` scoping).
        let root = scratch_dir("office-bootstrap-window");
        let _scope = StorageRootScope::enter(&root);
        std::fs::create_dir_all(root.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();

        office_utils::write_trust_anchor(&TrustAnchor {
            genesis: "0".repeat(64),
            enabled_at: 0,
            boundary: Vec::new(),
            prior_genesis: None,
            adopts: None,
        }).unwrap();

        assert!(office_user_of("anyone").unwrap().is_none());
        assert!(require_uploader(&Principal::Operator("anyone".to_string())).is_ok());

        drop(_scope);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn require_uploader_bypasses_the_office_for_static_and_open_principals() {
        // No storage scope at all: neither variant ever reaches `office_user_of`.
        assert!(require_uploader(&Principal::Static).is_ok());
        assert!(require_uploader(&Principal::Open).is_ok());
    }

    #[test]
    fn an_enrolled_reader_is_found_with_their_role_and_identity_class() {
        let fixture = OfficeFixture::genesis("office-reader");
        fixture.admit(
            "bob", Role::Reader, Vec::new(), IdentityClass::Agent, Some("alice".to_string())
        );

        let user = office_user_of("bob").unwrap().expect("bob must be enrolled");
        assert_eq!(user.role, Role::Reader);
        assert_eq!(user.class, IdentityClass::Agent);
        assert_eq!(user.supervisor.as_deref(), Some("alice"));

        let error = require_uploader(&Principal::Operator("bob".to_string())).unwrap_err();
        assert_eq!(error.0, StatusCode::FORBIDDEN);
        assert!(error.1.contains("readers cannot upload"), "{}", error.1);
    }

    #[test]
    fn an_unenrolled_operator_is_forbidden_once_an_office_exists() {
        let fixture = OfficeFixture::genesis("office-unenrolled");

        let error = office_user_of("mallory").err().unwrap();
        assert_eq!(error.0, StatusCode::FORBIDDEN);
        assert!(error.1.contains("is not enrolled"), "{}", error.1);

        drop(fixture);
    }

    #[test]
    fn require_uploader_allows_writers_and_admins() {
        let fixture = OfficeFixture::genesis("office-writer-admin");
        fixture.admit(
            "carol", Role::Writer, vec!["main".to_string()], IdentityClass::Bot, None
        );

        assert!(require_uploader(&Principal::Operator("carol".to_string())).is_ok());
        assert!(require_uploader(&Principal::Operator(fixture.admin.identifier.clone())).is_ok());
    }

    // ---------------------------------------------------------------------------------
    // post_ref_update — the transport-authorization gate
    // ---------------------------------------------------------------------------------

    fn ref_update_params(name: &str) -> PathParams {
        let mut params = PathParams::new();
        params.insert("name".to_string(), name.to_string());
        params
    }

    #[tokio::test]
    async fn a_reader_may_not_transport_the_office_meta_pallet() {
        let fixture = OfficeFixture::genesis("post-ref-reader-office");
        fixture.admit("dana", Role::Reader, Vec::new(), IdentityClass::Human, None);

        let state = Arc::new(single_mode_state(fixture.root.clone()));
        let params = ref_update_params("@office");
        let body = RefUpdateRequest { old_head: None, new_head: "a".repeat(64) };

        // The transport layer authenticates via the bearer-token map; give "dana" a
        // token so `check_auth` yields `Principal::Operator("dana")`.
        let mut operator_tokens = HashMap::new();
        operator_tokens.insert("tok-dana".to_string(), "dana".to_string());
        let state = Arc::new(AppState { operator_tokens, ..Arc::try_unwrap(state).ok().unwrap() });

        let response = post_ref_update(
            State(state), headers_with_bearer("tok-dana"), Path(params), Json(body)
        ).await;

        assert_eq!(response.into_response().status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_writer_outside_their_grant_may_not_move_a_pallet() {
        let fixture = OfficeFixture::genesis("post-ref-writer-outside-grant");
        fixture.admit(
            "erin", Role::Writer, vec!["only-this-one".to_string()], IdentityClass::Human, None
        );

        let mut operator_tokens = HashMap::new();
        operator_tokens.insert("tok-erin".to_string(), "erin".to_string());
        let state = Arc::new(AppState { operator_tokens, ..single_mode_state(fixture.root.clone()) });

        let params = ref_update_params("someone-elses-pallet");
        let body = RefUpdateRequest { old_head: None, new_head: "a".repeat(64) };

        let response = post_ref_update(
            State(state), headers_with_bearer("tok-erin"), Path(params), Json(body)
        ).await;

        assert_eq!(response.into_response().status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_writer_inside_their_grant_clears_the_authorization_gate() {
        let fixture = OfficeFixture::genesis("post-ref-writer-inside-grant");
        fixture.admit(
            "frank", Role::Writer, vec!["main".to_string()], IdentityClass::Human, None
        );

        let mut operator_tokens = HashMap::new();
        operator_tokens.insert("tok-frank".to_string(), "frank".to_string());
        let state = Arc::new(AppState { operator_tokens, ..single_mode_state(fixture.root.clone()) });

        let params = ref_update_params("main");
        // A `new_head` that was never uploaded: this must fail *later*, on object
        // presence, never on authorization — the FORBIDDEN branch must not fire for an
        // operator who is allowed to write this pallet.
        let body = RefUpdateRequest { old_head: None, new_head: "b".repeat(64) };

        let response = post_ref_update(
            State(state), headers_with_bearer("tok-frank"), Path(params), Json(body)
        ).await;

        let status = response.into_response().status();
        assert_ne!(status, StatusCode::FORBIDDEN, "an in-grant writer must clear the auth gate");
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "it must instead fail on the missing object");
    }
}
