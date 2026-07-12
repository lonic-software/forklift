//! The wire types of the remote protocol (see `docs/format/REMOTE_PROTOCOL.md`).
//! Shared by the client engine (`util::remote_utils`) and every server implementation
//! (the reference `forklift-server`, and the hosted control plane).

use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};
use crate::util::office_utils::TrustAnchor;

/// The protocol version spoken by this build. A client refuses a remote whose version
/// it does not know; the version only changes when the wire format changes.
pub const PROTOCOL_VERSION: &str = "2026-07-05";

/// The largest number of hashes accepted by one `POST /v1/objects/missing` request;
/// clients batch larger sets.
pub const MAX_MISSING_BATCH: usize = 10_000;

/// The largest number of hashes accepted by one `POST /v1/objects/upload-targets` request —
/// smaller than [`MAX_MISSING_BATCH`] because, unlike `missing`'s bare-hash answer, this
/// endpoint answers with a presigned URL per requested hash. A `MAX_MISSING_BATCH`-sized
/// request would answer with several megabytes of JSON (~500-byte presigned URLs × 10 000
/// keys) — at or over a Lambda synchronous response's ~6 MB limit. 1 000 keeps a max-size
/// response comfortably under 1 MB while staying well above what one lift session needs in
/// practice. Clients batch larger sets, and both heads reject an over-cap request; the client
/// (`remote_utils`), the storage-backed head (`forklift-aws-lambda`) and the direct head
/// (`forklift-server`) all read this one number so they can never drift.
pub const MAX_UPLOAD_TARGETS_BATCH: usize = 1_000;

/// The stable, refactor-safe marker every storage-backed head embeds in the `422` it answers
/// when a lift session's blob is still being verified and promoted out of band (the staging
/// verifier has not caught up yet). It is the one *transient* commit failure — distinct from a
/// control-plane object that was never uploaded, or a corrupt staged object, both of which are
/// terminal — so the client keys its bounded commit retry on this phrase rather than the exact
/// wording of the whole message. The head builds its message around this constant; the client
/// (`remote_utils::commit_lift`) matches on it. Changing it changes both sides at once.
pub const LIFT_SESSION_BLOB_NOT_READY: &str = "not yet verified and promoted";

/// The `GET /v1/warehouse` handshake: protocol version, refs and trust in one round trip.
#[derive(Serialize, Deserialize)]
pub struct WarehouseInfo {
    pub protocol: String,

    /// The pallet a franchise (clone) checks out when the user does not choose.
    pub default_pallet: String,

    /// Every pallet with something stacked, mapped to its head parcel hash.
    pub pallets: BTreeMap<String, String>,

    /// The trust anchor, when signing is established on the remote.
    pub trust: Option<TrustAnchorDto>,

    /// Whether this head supports chunked large files (§9.4b): it serves and stores the
    /// per-object recipe/chunk closure of a chunked file, and a `gc` that will not silently
    /// collect a chunked file's recipe-only-reachable chunks. An **additive** capability field,
    /// deliberately *not* a [`PROTOCOL_VERSION`] bump — the version check is exact-string
    /// equality, so bumping it would refuse every old×new pairing outright (a flag day), whereas
    /// a chunk-aware client reads this field to refuse only the one thing an old head cannot
    /// safely hold: a chunked file's lift. Absent (an old head that never wrote the field) reads
    /// as `false` via `#[serde(default)]`, so a new client refuses to lift chunked content there;
    /// an old client ignores the unknown field and is wholly unaffected.
    #[serde(default)]
    pub chunking: bool,
}

/// The trust anchor on the wire (the TOML file's fields as JSON). The re-genesis
/// fields (§8.7) are absent for an original enrollment.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct TrustAnchorDto {
    pub genesis: String,
    pub enabled_at: i64,
    pub boundary: Vec<String>,

    /// The genesis of the chain this anchor replaced (re-genesis chain of custody).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_genesis: Option<String>,

    /// The office head of the replaced chain, pinned as attested history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopts: Option<String>,
}

impl From<&TrustAnchor> for TrustAnchorDto {
    fn from(anchor: &TrustAnchor) -> Self {
        TrustAnchorDto {
            genesis: anchor.genesis.clone(),
            enabled_at: anchor.enabled_at,
            boundary: anchor.boundary.clone(),
            prior_genesis: anchor.prior_genesis.clone(),
            adopts: anchor.adopts.clone(),
        }
    }
}

impl TrustAnchorDto {
    /// Convert the wire form back into the local anchor type.
    pub fn to_anchor(&self) -> TrustAnchor {
        TrustAnchor {
            genesis: self.genesis.clone(),
            enabled_at: self.enabled_at,
            boundary: self.boundary.clone(),
            prior_genesis: self.prior_genesis.clone(),
            adopts: self.adopts.clone(),
        }
    }
}

/// The body of `POST /v1/objects/missing`.
#[derive(Serialize, Deserialize)]
pub struct MissingObjectsRequest {
    pub hashes: Vec<String>,
}

/// The response of `POST /v1/objects/missing`: the subset the remote does not have.
#[derive(Serialize, Deserialize)]
pub struct MissingObjectsResponse {
    pub missing: Vec<String>,
}

/// The body of `POST /v1/objects/upload-targets` (additive; a head whose byte plane is
/// object storage). Asks, without sending a single object body, where each of these
/// objects should be uploaded for lift `session`.
#[derive(Serialize, Deserialize)]
pub struct UploadTargetsRequest {
    /// The lift session the uploads belong to; it scopes the staging keys.
    pub session: String,

    pub hashes: Vec<String>,
}

/// The response of `POST /v1/objects/upload-targets`: one verdict per requested hash, so a
/// client learns in a single body-less round trip what to skip, what to send straight to
/// storage, and what to hand the control plane. It subsumes `POST /v1/objects/missing` for
/// the upload path (`present` is the complement of `missing`).
#[derive(Serialize, Deserialize)]
pub struct UploadTargetsResponse {
    /// Objects the remote already has at their canonical key. Do not upload them.
    pub present: Vec<String>,

    /// Presigned `PUT` URLs by hash — upload the bytes straight to storage, bypassing the
    /// control plane. Each URL addresses a *staging* key: the object is not fetchable until
    /// `POST /lift/{session}/commit` (or the staging verifier) promotes it.
    pub targets: BTreeMap<String, String>,

    /// Objects with no presigned target: `PUT` their bytes to `/v1/objects/{hash}` as usual
    /// and the head verifies them inline. A direct head answers with every missing hash here.
    pub direct: Vec<String>,
}

/// The body of `POST /v1/pallets/{name}` — the CAS ref update.
#[derive(Serialize, Deserialize)]
pub struct RefUpdateRequest {
    /// The head the remote is expected to have right now (`None`: the pallet must not
    /// exist yet). A mismatch is a `409` and nothing moves.
    pub old_head: Option<String>,

    /// The parcel the pallet head moves to.
    pub new_head: String,
}

/// The body of `POST /v1/lift/{session}/commit` (additive; the serverless head). After a
/// client has `PUT` its objects straight to storage via presigned staging URLs, it asks the
/// head to verify and promote the session's uploads before the ref update. The head promotes
/// the small `control_plane` objects synchronously and only presence-checks the large `blobs`
/// (the staging verifier promotes those out of band). A direct head verifies every `PUT`
/// inline and never needs this call.
#[derive(Serialize, Deserialize)]
pub struct CommitLiftRequest {
    /// Small objects — parcels, trees, recipes, signature sidecars — the head verifies and
    /// promotes synchronously: it reads the staged bytes, checks `Blake3(bytes) == hash`, and
    /// only then copies them to the canonical hash key. A corrupt one refuses the commit.
    pub control_plane: Vec<String>,

    /// Large working blobs and a chunked file's chunks, checked for presence at their canonical
    /// key only — which is the proof the staging verifier already hash-checked them. One still in
    /// staging simply reads as not-yet-ready, and the client retries.
    pub blobs: Vec<String>,

    /// Whether more commit batches follow for this same lift session. A lift touching a maximal
    /// chunked file lists too many chunk hashes for one request (Lambda's ~6 MB synchronous body),
    /// so the client paginates `control_plane`/`blobs` at [`MAX_MISSING_BATCH`] and sends
    /// `more: true` on every batch but the last. The head verifies/presence-checks each batch, but
    /// gates its end-of-session staging sweep (`discard_session`) on the **final** batch only
    /// (`more: false`) — otherwise an early batch's sweep would delete chunks a later batch still
    /// needs staged. Additive: an old client omits it (`#[serde(default)]` → `false`), which is
    /// exactly today's single-shot "verify, presence-check, then sweep" behaviour.
    #[serde(default)]
    pub more: bool,
}

/// The body of `POST /v1/resolve` — operator identifiers to resolve to display
/// names. Resolution is server-mediated on purpose (DESIGN.html §8.12): the client
/// never talks to the resolution service directly, so the policy that decides *which*
/// names a caller may see is enforced, not advisory. The server answers from its
/// resolution hook (`docs/format/HOOK_PROTOCOL.md`), knowing who is asking.
#[derive(Serialize, Deserialize)]
pub struct ResolveRequest {
    pub identifiers: Vec<String>,
}

/// The response of `POST /v1/resolve`: the names the caller is allowed to see (the
/// policy withholds the rest, and a server with no resolution hook returns none —
/// the client shows pseudonyms either way).
#[derive(Serialize, Deserialize)]
pub struct ResolveResponse {
    pub names: BTreeMap<String, String>,
}

/// The JSON body every error status carries.
#[derive(Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// New client × **old** server: an old head's handshake has no `chunking` field, so a
    /// chunk-aware client must read it as `false` (and then refuse to lift chunked content there).
    #[test]
    fn warehouse_info_without_chunking_reads_as_false() {
        let json = r#"{
            "protocol": "x",
            "default_pallet": "main",
            "pallets": {},
            "trust": null
        }"#;

        let info: WarehouseInfo = serde_json::from_str(json).expect("old-server handshake parses");
        assert!(!info.chunking, "an absent chunking field defaults to false");
    }

    /// A new head advertises `chunking: true`, and a chunk-aware client reads it.
    #[test]
    fn warehouse_info_with_chunking_reads_true_and_round_trips() {
        let info = WarehouseInfo {
            protocol: "x".to_string(),
            default_pallet: "main".to_string(),
            pallets: BTreeMap::new(),
            trust: None,
            chunking: true,
        };

        let wire = serde_json::to_string(&info).expect("serialize");
        assert!(wire.contains("\"chunking\":true"), "the field is on the wire: {}", wire);

        let back: WarehouseInfo = serde_json::from_str(&wire).expect("round trip");
        assert!(back.chunking);
    }

    /// **Old** client × new server: an old client's `WarehouseInfo` has fewer fields than a new
    /// server sends, so deserialization must ignore unknown fields (no `deny_unknown_fields`) —
    /// otherwise a new server's handshake would break every old client. A future extra field
    /// stands in for exactly that "server is newer than client" direction.
    #[test]
    fn warehouse_info_tolerates_an_unknown_field() {
        let json = r#"{
            "protocol": "x",
            "default_pallet": "main",
            "pallets": {},
            "trust": null,
            "chunking": true,
            "some_future_capability": ["v9"]
        }"#;

        let info: WarehouseInfo = serde_json::from_str(json).expect("unknown fields are ignored");
        assert!(info.chunking);
    }

    /// **Old** client → new server, or the single-shot path: a commit request with no `more` field
    /// must read as `false` (verify, presence-check, then sweep — today's exact behaviour).
    #[test]
    fn commit_lift_request_without_more_reads_as_false() {
        let json = r#"{ "control_plane": [], "blobs": [] }"#;

        let request: CommitLiftRequest = serde_json::from_str(json).expect("old-client commit parses");
        assert!(!request.more, "an absent more field defaults to false (single-shot, sweeps)");
    }

    /// A paginating client sets `more: true`; it round-trips, and an unknown extra field (a newer
    /// client than this server) is tolerated.
    #[test]
    fn commit_lift_request_more_round_trips_and_tolerates_unknown_fields() {
        let request = CommitLiftRequest {
            control_plane: vec!["a".repeat(64)],
            blobs: vec!["b".repeat(64)],
            more: true,
        };

        let wire = serde_json::to_string(&request).expect("serialize");
        assert!(wire.contains("\"more\":true"), "the field is on the wire: {}", wire);

        let json = r#"{ "control_plane": [], "blobs": [], "more": true, "future": 1 }"#;
        let back: CommitLiftRequest = serde_json::from_str(json).expect("unknown fields ignored");
        assert!(back.more);
    }
}
