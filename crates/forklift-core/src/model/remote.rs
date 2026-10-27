//! The wire types of the remote protocol (see `docs/format/REMOTE_PROTOCOL.md`).
//! Shared by the client engine (`util::remote_utils`) and every server implementation
//! (the reference `forklift-server`, and the hosted control plane).

use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};
use crate::util::office_utils::TrustAnchor;

/// The protocol version spoken by this build. A client refuses a remote whose version
/// it does not know; the version only changes when the wire format changes.
pub const PROTOCOL_VERSION: &str = "2026-07-05";

/// A head's `422` for a client-sent hash spelled any way but the object store's own, 64 lowercase
/// hex characters. Both heads give it word for word, and never repeat the value.
pub const REFUSAL_NOT_AN_OBJECT_HASH: &str =
    "A hash in the request is not an object hash (64 lowercase hex characters).";

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

/// The response header a storage-backed head sets, with the value [`LIFT_COMMIT_NOT_READY`], on the
/// `503` it answers a lift commit while a blob the batch names is still staged and not yet promoted
/// (the staging verifier has not caught up). It is the remote's own word that retrying can help,
/// and it lives in the header section rather than the body so that a body cut short or rewritten
/// in transit cannot turn a retriable answer into a terminal one. A client classifies a commit from
/// the status line and headers alone: a `503` without this header is still retried, but as an
/// answer of undetermined meaning. Header names are case-insensitive; this is the lower-case form.
pub const LIFT_COMMIT_HEADER: &str = "forklift-commit";

/// The value of [`LIFT_COMMIT_HEADER`] meaning "a staged copy exists and is not promoted yet". It
/// promises only that the copy exists, not that it will ever be promoted.
pub const LIFT_COMMIT_NOT_READY: &str = "not-ready";

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

    /// The pallets deleted on this head whose tombstones still stand, each mapped to the head it
    /// was deleted at, which the head keeps and serves until the tombstone is cleared or dropped.
    /// `Some` (possibly empty) also says the head deletes pallets (`DELETE /v1/pallets/{name}`);
    /// absent or `None` says it does not. An **additive** capability field like
    /// [`Self::chunking`]: an older head never writes it, and an older client ignores it.
    #[serde(default)]
    pub tombstones: Option<BTreeMap<String, String>>,
}

impl WarehouseInfo {
    /// The head this remote holds history from for each pallet name: a listed pallet's head, else
    /// the head of its tombstone, which the remote keeps rooted and serves until it is dropped. A
    /// name listed with a tombstone too is taken at its live head, as the holds route walks it.
    pub fn held_pallet_heads(&self) -> BTreeMap<String, String> {
        let mut heads = self.pallets.clone();

        for (name, head) in self.tombstones.iter().flatten() {
            heads.entry(name.clone()).or_insert_with(|| head.clone());
        }

        heads
    }
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

impl TrustAnchorDto {
    /// Refuse an anchor whose hashes are not canonical object hashes. Every one of them is shown
    /// to a human to verify out of band, so a value that could carry terminal escapes or
    /// look-alike characters is rejected where an anchor is read — the client's handshake, the
    /// server's `PUT /v1/trust` — rather than sanitized at each place it is printed.
    pub fn validate(&self) -> Result<(), String> {
        // The label is built only for the field that fails: a boundary holds one entry per pallet.
        let fields = std::iter::once(("genesis", None, &self.genesis))
            .chain(self.boundary.iter().enumerate().map(|(i, hash)| ("boundary", Some(i), hash)))
            .chain(self.prior_genesis.iter().map(|hash| ("prior_genesis", None, hash)))
            .chain(self.adopts.iter().map(|hash| ("adopts", None, hash)));

        // The problem names the field and what is wrong with it, never the value: the value came
        // from a remote that is misbehaving, and repeating it would hand that remote the message.
        for (field, index, hash) in fields {
            if crate::util::object_utils::is_object_hash(hash) {
                continue;
            }

            let problem = match hash.bytes().position(|b| !matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                Some(position) if position < 64 => {
                    format!("byte {} is not a lowercase hex digit", position)
                }
                _ => format!("it is {} bytes long, not 64", hash.len()),
            };

            return Err(format!(
                "The trust anchor is malformed: its {} is not an object hash ({}).",
                index.map_or(field.to_string(), |i| format!("{}[{}]", field, i)), problem
            ));
        }

        Ok(())
    }
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

/// The most candidates one `POST /v1/pallets/{name}/holds` request may ask about. Both heads
/// refuse a longer list with a `422`, since a client's own cap does not bind a hostile client.
/// It matches the held-heads record's per-pallet cap, so one request covers a pallet's record.
pub const MAX_HOLDS_CANDIDATES: usize = 16;

/// The body of `POST /v1/pallets/{name}/holds`: parcel hashes the client once saw this pallet
/// hold, asking whether the pallet's current head still reaches each one.
#[derive(Serialize, Deserialize)]
pub struct HoldsRequest {
    pub candidates: Vec<String>,
}

/// One candidate's answer in a [`HoldsResponse`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HoldAnswer {
    /// The pallet's current head reaches the candidate, so the remote holds its history.
    Held,
    /// The walk from the current head ended without reaching the candidate (or the pallet does
    /// not exist).
    NotHeld,
    /// The head could not decide within its bounds, or could not read a parcel on the way. An
    /// answer this build does not know reads as this one too: it is the answer that frees
    /// nothing.
    #[serde(other)]
    Unknown,
}

/// The response of `POST /v1/pallets/{name}/holds`: one answer per distinct candidate asked.
#[derive(Serialize, Deserialize)]
pub struct HoldsResponse {
    pub holds: BTreeMap<String, HoldAnswer>,
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

    /// On a creation (`old_head` `None`) of a pallet deleted on this head, the head its tombstone
    /// holds: the writer acknowledges the deletion. A head refuses re-creating a deleted pallet
    /// without it, or from history that does not descend from that head. Additive: absent reads as
    /// `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tombstone: Option<String>,
}

/// The body of `DELETE /v1/pallets/{name}`: the compare-and-set removal of a user pallet's ref,
/// which leaves a tombstone (see [`WarehouseInfo::tombstones`]).
#[derive(Serialize, Deserialize)]
pub struct RefDeleteRequest {
    /// The head the remote is expected to have right now. A mismatch is a `409` and nothing
    /// moves.
    pub old_head: String,
}

/// The body of `POST /v1/lift/{session}/commit` (additive; the serverless head). After a
/// client has `PUT` its objects straight to storage via presigned staging URLs, it asks the
/// head to verify and promote the session's uploads before the ref update. The head promotes
/// the small `control_plane` objects synchronously and only presence-checks the large `blobs`
/// (the staging verifier promotes those out of band). A direct head verifies every `PUT`
/// inline and never needs this call.
#[derive(Serialize, Deserialize)]
pub struct CommitLiftRequest {
    /// Small objects — parcels, trees, recipes — the head verifies and promotes synchronously:
    /// it reads the staged bytes, checks `Blake3(bytes) == hash`, and only then copies them to
    /// the canonical hash key. A corrupt one refuses the commit.
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
///
/// The identifiers are borrowed when sending, so a client need not copy a batch to ask about it,
/// and owned when received.
#[derive(Serialize, Deserialize)]
pub struct ResolveRequest<'a> {
    pub identifiers: std::borrow::Cow<'a, [String]>,
}

/// The most distinct identifiers one resolve request may name. A server refuses more; a client
/// with more splits them across requests.
pub const MAX_RESOLVE_IDENTIFIERS: usize = 10_000;

/// A resolve request's identifiers, each once and sorted, or the refusal every head gives a
/// request naming more than [`MAX_RESOLVE_IDENTIFIERS`] distinct ones (which says how many it
/// named). Borrowed, so a head that only checks the limit copies nothing.
pub fn distinct_resolve_identifiers(identifiers: &[String])
                                    -> Result<std::collections::BTreeSet<&String>, String> {
    let distinct: std::collections::BTreeSet<&String> = identifiers.iter().collect();

    if distinct.len() > MAX_RESOLVE_IDENTIFIERS {
        return Err(format!(
            "This resolve request names {} distinct identifiers; at most {} are allowed per \
            request, so batch larger sets.",
            distinct.len(), MAX_RESOLVE_IDENTIFIERS
        ));
    }

    Ok(distinct)
}

/// Per-identifier allowance in a resolve answer, beyond the identifier itself: the name it
/// resolves to and the JSON around the pair.
pub const RESOLVE_ANSWER_BYTES_PER_IDENTIFIER: u64 = 1024;

/// The cap for an answer resolving `identifiers`, at the directory and again at the client: each
/// identifier as the answer spells it (a JSON-escaped key, up to six times its raw length) plus
/// [`RESOLVE_ANSWER_BYTES_PER_IDENTIFIER`], plus the envelope. Sized from the identifiers' bytes,
/// not their count alone, so a long one cannot overflow the answer for the whole request.
pub fn resolve_answer_cap(identifiers: &[String]) -> u64 {
    let identifier_bytes = identifiers.iter()
        .fold(0u64, |sum, id| sum.saturating_add((id.len() as u64).saturating_mul(6)));

    crate::util::body_utils::per_item_body_cap(identifiers.len(), RESOLVE_ANSWER_BYTES_PER_IDENTIFIER)
        .saturating_add(identifier_bytes)
}

/// The response of `POST /v1/resolve`: the names the caller is allowed to see (the
/// policy withholds the rest, and a server with no resolution hook returns none —
/// the client shows pseudonyms either way).
#[derive(Serialize, Deserialize)]
pub struct ResolveResponse {
    pub names: BTreeMap<String, String>,
}

/// The JSON body every error status carries.
///
/// `error` is the human message. `code` and `next_step` are **additive** (§7.4 error taxonomy):
/// when a server-side operation fails with a classified refusal, the head sets the stable refusal
/// `code` and its recovery `next_step` here so the client can classify it with the same code and
/// exit code as a local refusal. Both are absent for an unclassified error and on an older head
/// that predates them (`#[serde(default)]` reads them as `None`); a client that does not understand
/// them ignores them. See `docs/format/REMOTE_PROTOCOL.md`.
#[derive(Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,

    /// The stable refusal code (a `RefusalCode` string), when the failure is a classified refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,

    /// The machine-actionable recovery step that accompanies `code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Revert: skip the tombstones → "gone" has no head; let them override → "both" reads at its
    /// tombstone.
    #[test]
    fn a_tombstone_stands_in_for_a_pallet_the_remote_no_longer_lists() {
        let mut info: WarehouseInfo = serde_json::from_str(
            r#"{"protocol": "1", "pallets": {"live": "l", "both": "b"}, "default_pallet": "live"}"#
        ).unwrap();
        assert_eq!(info.held_pallet_heads(), info.pallets, "a head that cannot delete adds nothing");

        info.tombstones = Some(BTreeMap::from([("gone".to_string(), "g".to_string()), ("both".to_string(), "t".to_string())]));
        let held = info.held_pallet_heads();
        assert_eq!(held.get("gone").map(String::as_str), Some("g"));
        assert_eq!(held.get("both").map(String::as_str), Some("b"), "a live ref is taken at its head");
        assert_eq!(held.len(), 3);
    }

    /// New client × **old** server: an old error body has no `code`/`next_step`, so a new client
    /// must read them as absent (`#[serde(default)]`) rather than fail to parse.
    #[test]
    fn error_response_without_code_reads_as_none() {
        let body: ErrorResponse = serde_json::from_str(r#"{ "error": "boom" }"#)
            .expect("an old-shape error body parses");
        assert_eq!(body.error, "boom");
        assert!(body.code.is_none());
        assert!(body.next_step.is_none());
    }

    /// An uncoded error serializes to the **byte-identical** old shape — the additive fields are
    /// skipped when absent, so an old client sees exactly the body it always did.
    #[test]
    fn an_uncoded_error_serializes_to_the_old_shape() {
        let body = ErrorResponse { error: "boom".to_string(), code: None, next_step: None };
        assert_eq!(serde_json::to_string(&body).unwrap(), r#"{"error":"boom"}"#);
    }

    /// A coded error round-trips its additive fields.
    #[test]
    fn a_coded_error_round_trips() {
        let body = ErrorResponse {
            error: "outside scope".to_string(),
            code: Some("out_of_scope".to_string()),
            next_step: Some("widen it".to_string()),
        };
        let json = serde_json::to_string(&body).unwrap();
        let back: ErrorResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.code.as_deref(), Some("out_of_scope"));
        assert_eq!(back.next_step.as_deref(), Some("widen it"));
    }

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
            tombstones: None,
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

    #[test]
    fn a_trust_anchor_whose_hashes_are_not_hashes_is_refused() {
        let hash = "ab".repeat(32);
        let anchor = |genesis: &str, boundary: Vec<String>, prior: Option<&str>| TrustAnchorDto {
            genesis: genesis.to_string(),
            enabled_at: 0,
            boundary,
            prior_genesis: prior.map(str::to_string),
            adopts: None,
        };

        assert_eq!(anchor(&hash, vec![hash.clone()], Some(&hash)).validate(), Ok(()));

        // Terminal escapes, a bidi override that would make a hash read as another, a short
        // value, in each field a human is shown.
        let escape = format!("{}\u{1b}]0;x\u{7}", &hash[..56]);
        let bidi = format!("{}\u{202e}{}", &hash[..32], &hash[..31]);
        assert!(anchor(&escape, vec![], None).validate().is_err());
        assert!(anchor(&hash, vec![bidi.clone()], None).validate().is_err());
        assert!(anchor(&hash, vec![], Some("abc")).validate().is_err());

        // Uppercase is a hash in another spelling, which every byte-for-byte comparison misses.
        assert!(anchor(&hash.to_uppercase(), vec![], None).validate().is_err());

        // The problem names the field and the defect, never the remote's value.
        let problem = anchor(&hash, vec![bidi.clone()], None).validate().unwrap_err();
        assert!(problem.contains("boundary[0]") && problem.contains("byte 32"), "{}", problem);
        assert!(!problem.contains('\u{202e}') && !problem.contains(&hash[..32]), "{}", problem);
        let problem = anchor(&escape, vec![], None).validate().unwrap_err();
        assert!(!problem.contains('\u{1b}'), "{}", problem);
        let problem = anchor(&hash, vec![], Some("abc")).validate().unwrap_err();
        assert!(problem.contains("prior_genesis") && problem.contains("3 bytes"), "{}", problem);
    }
}
