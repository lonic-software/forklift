//! The wire types of the hook protocol (see `docs/format/HOOK_PROTOCOL.md`): the
//! typed seam between a Forklift server head and a hosting provider (§8.13). Hooks
//! cover exactly the *soft* surface — authentication, admission policy, event
//! notification, identity resolution. Signature, sigchain and revocation
//! verification, content authorization and signing are never hookable: they are
//! Forklift-owned, deterministic and offline.

use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};

/// The hook protocol version. Carried on every request in the
/// `x-forklift-hook-version` header; a receiver refuses versions it does not know.
/// It only changes when the wire format changes.
pub const HOOK_PROTOCOL_VERSION: &str = "2026-07-05";

/// The flat request timeout the server head arms on the `reqwest::Client` it calls every
/// hook through (`forklift-server`'s `serve` builds it from this constant rather than a
/// second, unlinked literal). It bounds *every* hook that client carries — authentication,
/// admission, events and resolution — individually, not collectively: a request running two
/// of them may wait twice this long.
///
/// Which hooks a given request actually runs varies, and callers sizing a budget against this
/// must not assume a fixed count: a mutating request may run authentication alone (`put_trust`)
/// or authentication and admission (`put_signature`, `put_object`). The authentication and
/// admission hooks fail closed on a transport failure, so a hook endpoint that never answers
/// must not wedge the request it gates forever — but 10s is generous enough that a hook doing
/// real work (a directory lookup, a policy check) never trips it under normal load.
///
/// `pub`, not module-private, because `forklift-core`'s own remote client derives its
/// single-write budget from this value (`remote_utils::single_write_budget`) — the
/// server's real per-hook allowance, not a mirrored guess, since the two crates cannot
/// share a module-private item across the boundary. A change here is therefore load-bearing
/// on both sides: it moves the server's own hook budget *and* the client-side deadline that
/// waits out however many of these hooks a single write's server side runs.
pub const HOOK_CLIENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The header naming which hook a request speaks (`authentication`, `admission`,
/// `event`, `resolution`).
pub const HEADER_HOOK: &str = "x-forklift-hook";

/// The header carrying `HOOK_PROTOCOL_VERSION`.
pub const HEADER_HOOK_VERSION: &str = "x-forklift-hook-version";

/// The header carrying the request's Unix timestamp (seconds, decimal). Signed
/// together with the body; receivers refuse stale requests (replay protection).
pub const HEADER_HOOK_TIMESTAMP: &str = "x-forklift-hook-timestamp";

/// The header carrying the request MAC: Blake3 keyed hash (hex) over
/// `"<timestamp>\n" + body`, keyed by the shared hook secret (see
/// `util::hook_utils`). A hook endpoint must verify it before acting — a spoofable
/// authentication hook is game over.
pub const HEADER_HOOK_SIGNATURE: &str = "x-forklift-hook-signature";

/// `authentication` (hot path, fail closed): credential → operator identifier.
/// Called for a bearer token the server does not know locally.
#[derive(Serialize, Deserialize)]
pub struct AuthenticationHookRequest {
    /// The presented bearer token, verbatim.
    pub token: String,
}

/// The authentication hook's positive answer (HTTP 200). Any non-200 answer means
/// the credential is not valid; a transport failure means the request is refused
/// (fail closed), never waved through.
#[derive(Serialize, Deserialize)]
pub struct AuthenticationHookResponse {
    /// The office identifier the credential belongs to. Content authorization still
    /// derives from this identifier's role in the warehouse office — the hook
    /// authenticates, it never authorizes content.
    pub identifier: String,
}

/// `admission` (hot path, fail closed): may this request proceed, as a matter of
/// *soft* policy (quota, plan limits, suspended accounts)? A denial here is an
/// access decision; it can never make invalid content valid or forge anything.
#[derive(Serialize, Deserialize)]
pub struct AdmissionHookRequest {
    /// What is being attempted: `upload`, `ref_update` or `warehouse_create`.
    pub action: String,

    /// The warehouse id (multi-warehouse servers; absent in single mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warehouse: Option<String>,

    /// The acting operator's office identifier, when the principal has one (absent
    /// for the static token and open servers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,

    /// The pallet a `ref_update` targets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pallet: Option<String>,
}

/// The admission hook's verdict (HTTP 200 either way; non-200 or a transport
/// failure counts as a denial — fail closed).
#[derive(Serialize, Deserialize)]
pub struct AdmissionHookResponse {
    pub allow: bool,

    /// Shown to the refused client, when given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `event` (cold path, queue + retry): a side-effect notification. Delivery is
/// at-least-once with retries; the response body is ignored (any 2xx = delivered).
#[derive(Clone, Serialize, Deserialize)]
pub struct HookEvent {
    /// What happened: one of the `EVENT_*` constants.
    pub event: String,

    /// The warehouse id (multi-warehouse servers; absent in single mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warehouse: Option<String>,

    /// The acting operator's office identifier, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,

    /// The pallet a ref event concerns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pallet: Option<String>,

    /// The pallet head before the move (absent when the pallet was unborn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_head: Option<String>,

    /// The pallet head after the move.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_head: Option<String>,

    /// The key a `key_revoked` event concerns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,

    /// The recorded revocation reason of a `key_revoked` event
    /// (`retirement`/`compromise`), or the genesis hash of a `trust_*` event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl HookEvent {
    pub fn new(event: &str) -> HookEvent {
        HookEvent {
            event: event.to_string(),
            warehouse: None,
            operator: None,
            pallet: None,
            old_head: None,
            new_head: None,
            key_id: None,
            detail: None,
        }
    }
}

/// A pallet head moved (a lift was accepted; consolidations arrive as this too —
/// the merge parcel is the new head).
pub const EVENT_PALLET_UPDATED: &str = "pallet_updated";

/// A key was revoked in the office (`detail` carries the reason).
pub const EVENT_KEY_REVOKED: &str = "key_revoked";

/// Trust was established on a warehouse (`detail` carries the genesis hash).
pub const EVENT_TRUST_ESTABLISHED: &str = "trust_established";

/// The trust anchor was replaced by a re-genesis (`detail` carries the new genesis).
pub const EVENT_TRUST_RESET: &str = "trust_reset";

/// A warehouse was created (multi-warehouse servers).
pub const EVENT_WAREHOUSE_CREATED: &str = "warehouse_created";

/// `resolution` (cold path, fall back to pseudonyms): operator identifiers → display
/// names. The chain stores zero PII (§8.12); names exist only at display time,
/// server-mediated and policy-gated — the *server* invokes this hook on behalf of a
/// client's `POST /v1/resolve`, never the client directly, so the policy that decides
/// which names a caller may see is enforced rather than advisory. A failure or a
/// missing entry simply leaves the pseudonymous identifier on screen — never an error.
#[derive(Serialize, Deserialize)]
pub struct ResolutionHookRequest {
    /// Who is asking: the authenticated caller's operator identifier, when they have
    /// one (absent for the server's own static token). A policy-aware directory tiers
    /// its answer by this (§8.12: guests resolve nothing, members only shared-warehouse
    /// peers, admins everyone); a dumb directory may ignore it and let the server
    /// pre-filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,

    pub identifiers: Vec<String>,
}

/// The resolution hook's answer: whatever the caller may resolve. Identifiers the
/// policy withholds (or the directory does not know) are simply absent.
#[derive(Serialize, Deserialize)]
pub struct ResolutionHookResponse {
    pub names: BTreeMap<String, String>,
}
