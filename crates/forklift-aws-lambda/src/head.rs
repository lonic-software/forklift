//! The protocol handler: one method per `REMOTE_PROTOCOL.md` endpoint, generic over the
//! two stores. This is the "real handler logic" the whole protocol suite exercises — the
//! same code the AWS Lambda control-plane function calls, only over S3 + DynamoDB instead
//! of the in-memory fakes.
//!
//! The methods return provider-agnostic outcomes ([`Status`](crate::Status) via
//! [`HeadError`](crate::HeadError), or a redirect URL); the runtime adapter — `lambda_http`
//! for AWS — maps them onto HTTP. Byte-plane endpoints (`GET`/`PUT` object) can answer
//! with a `307` redirect to a presigned storage URL when the [`ObjectStore`] is S3-backed,
//! exactly as the protocol's redirect room allows.
//!
//! Authentication is the adapter's concern (`entrypoint::authenticate` — an optional API
//! Gateway authorizer on top of it, if configured). There is no transport-level role/grant
//! check anywhere in this crate to be the adapter's concern *of*: this type has no caller-
//! identity concept at all, so it cannot consult an office role for who is calling. It is not
//! a uniform "role for who signed" either: the office pallet's own parcels are checked against
//! their signer's role, as of that parcel's own signing (`verify_office_chain`); every
//! other pallet's parcels are accepted when they are either validly signed by a key the
//! office currently tracks and has not revoked, or unsigned/untracked-key but reachable from
//! the trust boundary (tolerated as legacy), or signed by a revoked key but reachable from that
//! revocation's own distrust boundary — no role, no grant, in any of the three arms
//! (`verify_pallet_history`). What it enforces is exactly that — the provider-independent
//! content invariants: hash-verified objects, a
//! fast-forward-only CAS, and — on a trusted warehouse — the full offline audit (the signed
//! office chain and, for any non-office pallet, only its pushed history's own signature
//! validity) before a ref moves. See [`Head::ref_update`]'s own doc comment and
//! `docs/DEPLOYMENT.md`'s "What the bearer token does — and does not — control" for the
//! caller-privilege consequence: every caller holding the transport bearer gets identical
//! privileges over every pallet.
//!
//! **Every method here is synchronous and must be called from a blocking thread**
//! (`tokio::task::spawn_blocking`), exactly as `forklift-server` runs its handlers' storage
//! work. It mirrors objects into a thread-local-scoped scratch and runs `forklift_core`'s
//! audit inside that scope, so the call must never migrate between threads mid-flight; and
//! tokio refuses to let a runtime worker block on the futures an SDK-backed store bridges.
//! See `blocking.rs`.

use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::SystemTime;

use forklift_core::error::{CoreError, RefusalCode};
use forklift_core::model::remote::{
    RefUpdateRequest, TrustAnchorDto, UploadTargetsResponse, WarehouseInfo, MAX_MISSING_BATCH,
    PROTOCOL_VERSION, REFUSAL_NOT_AN_OBJECT_HASH,
};
use forklift_core::model::tree_item::TreeItem;
use forklift_core::util::office_utils::OFFICE_PALLET_NAME;
use forklift_core::util::pallet_utils::{PalletNamespace, PalletRef};
use forklift_core::util::{
    audit_utils, bundle_utils, file_utils, merge_utils, meta_tree, object_utils, sign_utils,
};

use crate::error::{HeadError, HeadResult};
use crate::reissue::reissue;
use crate::scratch::{materialize, materialize_state, Mirror, Scratch};
use crate::store::{
    CasOutcome, ObjectAccess, ObjectStore, OfficePrecondition, PromoteOutcome, PutTarget,
    RefStore, SignatureOutcome, TransientCause, TrustOutcome, TrustWriteOutcome,
};

/// The `genesis` of a stored anchor's bytes.
///
/// Used only to name the incumbent in a refusal message. Bytes that will not decode yield `None`
/// rather than an error: the refusal is already decided by the time this is called, so a
/// malformed incumbent must degrade the message, never the outcome.
fn genesis_of(json: &str) -> Option<String> {
    serde_json::from_str::<TrustAnchorDto>(json).ok().map(|dto| dto.genesis)
}

/// How the head answers a byte read: the bytes, or a redirect to a storage URL.
#[derive(Debug)]
pub enum ObjectReadResult {
    /// The object bytes (the head serves them itself).
    Bytes(Vec<u8>),
    /// Follow this presigned URL for the bytes (`307`).
    Redirect(String),
}

/// How the head answers an object upload: it was stored, or the bytes go to a storage URL.
#[derive(Debug)]
pub enum ObjectWriteResult {
    /// The head stored the bytes directly; `true` if newly created.
    Stored { created: bool },
    /// Upload the bytes to this presigned URL (`307`).
    Redirect(String),
}

/// How the head answers a `batch` request: the bundle bytes, or a redirect to a presigned
/// `GET` for them.
#[derive(Debug)]
pub enum BatchResult {
    /// The bundle-format stream (the head serves it itself).
    Bundle(Vec<u8>),
    /// Follow this presigned URL for the bundle. The router answers `303` (not `307`/`308`):
    /// this request was a `POST`, but the target is always a presigned `GET`, so the client
    /// must switch methods rather than replay the `POST` — which would fail signature
    /// verification against a `GET`-only presigned URL.
    Redirect(String),
}

/// The outcome of establishing (or resetting) trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustResult {
    /// A new anchor was planted, or a re-genesis replaced the old one (`201`).
    Established,
    /// The identical anchor was already present (`200`).
    Unchanged,
}

/// Where a request's scratch warehouse comes from.
enum ScratchSource {
    /// A fresh directory per request, removed when the request ends.
    Ephemeral,
    /// The process-global scratch for this warehouse, reused across warm invocations.
    Pooled(String),
}

/// The serverless protocol handler over an [`ObjectStore`] and a [`RefStore`].
pub struct Head<O: ObjectStore, R: RefStore> {
    pub objects: O,
    pub refs: R,
    scratch: ScratchSource,
    /// When the invocation this head serves must have answered by. A transient refusal is
    /// re-issued only while what remains covers the wait and one more attempt (see
    /// `reissue.rs`); `None` keeps the attempt cap and skips that check.
    deadline: Option<SystemTime>,
}

impl<O: ObjectStore, R: RefStore> Head<O, R> {
    /// Assemble a head over the two stores, mirroring into a fresh scratch per request.
    ///
    /// It has no invocation deadline, so a transient refusal is re-issued up to the attempt cap
    /// without a window check; [`with_deadline`](Self::with_deadline) adds one.
    pub fn new(objects: O, refs: R) -> Head<O, R> {
        Head { objects, refs, scratch: ScratchSource::Ephemeral, deadline: None }
    }

    /// Assemble a head that reuses one scratch warehouse across warm invocations, so the
    /// audit mirror is paid once per container rather than once per request.
    ///
    /// `warehouse_id` must identify the warehouse these stores serve, and nothing else: an
    /// object found in the scratch is treated as present in the object store, and that
    /// inference is only sound within one warehouse (see [`Scratch::shared`]).
    ///
    /// `deadline` is when the invocation must have answered by — on Lambda, the runtime
    /// context's deadline. It is required here, unlike on [`new`](Self::new), because this is the
    /// constructor the deployed function uses, and a head re-issuing transactions blind to its
    /// window could spend it and leave the gateway to answer `504` instead of this head a `503`.
    pub fn pooled(
        objects: O,
        refs: R,
        warehouse_id: impl Into<String>,
        deadline: SystemTime,
    ) -> Head<O, R> {
        Head {
            objects,
            refs,
            scratch: ScratchSource::Pooled(warehouse_id.into()),
            deadline: Some(deadline),
        }
    }

    /// The same head, bounded by an invocation deadline.
    pub fn with_deadline(self, deadline: SystemTime) -> Head<O, R> {
        Head { deadline: Some(deadline), ..self }
    }

    /// The scratch warehouse this request mirrors into.
    fn scratch(&self) -> Result<Arc<Scratch>, String> {
        match &self.scratch {
            ScratchSource::Ephemeral => Scratch::new().map(Arc::new),
            ScratchSource::Pooled(warehouse_id) => Scratch::shared(warehouse_id),
        }
    }

    /// `GET /v1/warehouse` — the handshake: protocol version, refs and trust.
    pub fn handshake(&self) -> HeadResult<WarehouseInfo> {
        let mut pallets = BTreeMap::new();

        for (pallet_ref, head) in self.refs.list_refs().map_err(HeadError::internal)? {
            pallets.insert(pallet_ref.to_wire(), head);
        }

        Ok(WarehouseInfo {
            protocol: PROTOCOL_VERSION.to_string(),
            default_pallet: self.refs.default_pallet().map_err(HeadError::internal)?,
            pallets,
            trust: self
                .refs
                .get_trust()
                .map_err(HeadError::internal)?
                .map(|(anchor, _bytes)| TrustAnchorDto::from(&anchor)),
            // This head serves and stores chunked large files: chunks and recipes ride the byte
            // plane as ordinary content-addressed objects, and the commit-gate closure audit
            // (`ref_update`) descends a recipe to presence-check its chunks before a ref moves. A
            // chunk-aware client reads this to know it may lift chunked content here.
            chunking: true,
        })
    }

    /// `POST /v1/objects/missing` — which of these hashes does the store lack?
    ///
    /// One bulk probe (`ObjectStore::objects_missing`) rather than one `exists` per hash: on
    /// the AWS head this is the bounded-concurrency S3 `HeadObject` batch, not up to
    /// [`MAX_MISSING_BATCH`] (10,000) serial round trips behind API Gateway's 29 s ceiling —
    /// the same fix the ref-update chunk descent needed. The bulk probe's answer may come
    /// back in any order (a parallel backend finishes out of order) and may not preserve
    /// duplicates, so the result is a membership test: the response is built by walking the
    /// *input* `hashes` in order and keeping every one the probe reported absent — identical
    /// to what the old per-hash loop returned (order and duplicate hashes both preserved).
    pub fn missing(&self, hashes: &[String]) -> HeadResult<Vec<String>> {
        self.reject_malformed_batch(hashes)?;

        let absent: HashSet<String> =
            self.objects.objects_missing(hashes).map_err(HeadError::internal)?.into_iter().collect();

        Ok(hashes.iter().filter(|hash| absent.contains(hash.as_str())).cloned().collect())
    }

    /// `POST /v1/objects/upload-targets` — the body-less upload negotiation (additive).
    ///
    /// One round trip, no object bodies: for every hash the client learns whether the
    /// remote already has it (`present`), where to `PUT` it straight into storage
    /// (`targets`, a presigned staging URL), or that it must go through the control plane
    /// (`direct`). Without it a client uploading to a staging head has to send each body to
    /// the control plane only to be told `307` — paying for the bytes twice, and on Lambda
    /// paying for them through a request-size limit the byte plane exists to avoid.
    ///
    /// A direct head answers with every missing hash in `direct`, so one client code path
    /// serves both heads.
    pub fn upload_targets(
        &self,
        session: &str,
        hashes: &[String],
    ) -> HeadResult<UploadTargetsResponse> {
        self.reject_malformed_batch(hashes)?;

        let mut response = UploadTargetsResponse {
            present: Vec::new(),
            targets: BTreeMap::new(),
            direct: Vec::new(),
        };

        // Dedupe, keeping first-occurrence order — the order `present`/`direct` are built in
        // below, unchanged from the old per-hash loop.
        let mut seen: HashSet<&str> = HashSet::new();
        let unique: Vec<String> =
            hashes.iter().filter(|hash| seen.insert(hash.as_str())).cloned().collect();

        // One bulk probe instead of one `exists` per unique hash: on the AWS head this is the
        // bounded-concurrency S3 `HeadObject` batch, not up to a thousand serial round trips for
        // a large negotiation. `put_target` (a local presign, no network round trip) still runs
        // once per missing hash, exactly as before — only the presence check is batched.
        let absent: HashSet<String> = self
            .objects
            .objects_missing(&unique)
            .map_err(HeadError::internal)?
            .into_iter()
            .collect();

        for hash in &unique {
            if !absent.contains(hash.as_str()) {
                response.present.push(hash.clone());
                continue;
            }

            match self.objects.put_target(Some(session), hash).map_err(HeadError::internal)? {
                PutTarget::Staged(url) => {
                    response.targets.insert(hash.clone(), url);
                }
                PutTarget::Direct => response.direct.push(hash.clone()),
                // The session was named, so a store that still demands one is broken.
                PutTarget::SessionRequired => {
                    return Err(HeadError::internal(format!(
                        "The object store refused a staging target for {} despite a named lift \
                        session.",
                        hash
                    )))
                }
            }
        }

        Ok(response)
    }

    /// `GET /v1/objects/{hash}` — the raw object bytes, or a redirect to storage.
    pub fn object_get(&self, hash: &str) -> HeadResult<ObjectReadResult> {
        require_object_hash(hash)?;

        match self.objects.access(hash).map_err(HeadError::internal)? {
            Some(ObjectAccess::Direct(bytes)) => Ok(ObjectReadResult::Bytes(bytes)),
            Some(ObjectAccess::Redirect(url)) => Ok(ObjectReadResult::Redirect(url)),
            None => Err(HeadError::not_found(format!("No object {} exists.", hash))),
        }
    }

    /// `PUT /v1/objects/{hash}?session={id}` — store the object (hash-verified before it is
    /// fetchable), or redirect the upload to a presigned staging URL.
    ///
    /// On a direct head the bytes are verified inline and `session` is irrelevant. On a
    /// staging head the bytes never pass through here: the `307` sends them to a staging
    /// key under `session`, from which `commit_lift` promotes them. Such a head refuses a
    /// session-less upload (`422`) — bytes staged under no session could never be promoted,
    /// and the alternative (staging at the hash key) is exactly the invariant-1 hole.
    pub fn object_put(
        &self,
        session: Option<&str>,
        hash: &str,
        bytes: &[u8],
    ) -> HeadResult<ObjectWriteResult> {
        require_object_hash(hash)?;

        match self.objects.put_target(session, hash).map_err(HeadError::internal)? {
            PutTarget::Staged(url) => Ok(ObjectWriteResult::Redirect(url)),
            PutTarget::SessionRequired => Err(HeadError::unprocessable(
                "This warehouse stages uploads in object storage; name the lift session \
                (`?session=…`) so the upload can be verified and promoted to its hash key."
                    .to_string(),
            )),
            PutTarget::Direct => {
                // A hash mismatch is a client error (`422`): nothing unverified is stored.
                let outcome =
                    self.objects.put_verified(hash, bytes).map_err(HeadError::unprocessable)?;

                Ok(ObjectWriteResult::Stored {
                    created: outcome == crate::store::PutOutcome::Created,
                })
            }
        }
    }

    /// `GET /v1/signatures/{hash}` — a parcel's signature sidecar.
    pub fn signature_get(&self, parcel_hash: &str) -> HeadResult<Vec<u8>> {
        require_object_hash(parcel_hash)?;

        self.objects
            .get_signature(parcel_hash)
            .map_err(HeadError::internal)?
            .ok_or_else(|| HeadError::not_found("The parcel carries no signature.".to_string()))
    }

    /// `PUT /v1/signatures/{hash}` — store a signature sidecar. The structure is validated
    /// here (`422` when malformed); whether it *verifies* is decided at ref-update time.
    /// A conflicting sidecar for an already-signed parcel is refused (`409`).
    pub fn signature_put(&self, parcel_hash: &str, bytes: &[u8]) -> HeadResult<SignatureOutcome> {
        require_object_hash(parcel_hash)?;

        sign_utils::validate_raw_parcel_signature(bytes, parcel_hash)
            .map_err(HeadError::unprocessable)?;

        let outcome =
            self.objects.put_signature(parcel_hash, bytes).map_err(HeadError::internal)?;

        if outcome == SignatureOutcome::Conflict {
            return Err(HeadError::conflict(format!(
                "Parcel {} already carries a different signature; signatures are immutable.",
                parcel_hash
            )));
        }

        Ok(outcome)
    }

    /// `PUT /v1/trust` — establish the trust anchor (the same one-way door it is locally),
    /// with the one sanctioned replacement: a re-genesis (§8.7). The static-token
    /// authority a re-genesis requires is enforced by the runtime adapter, exactly as the
    /// server head restricts it to its static token.
    pub fn put_trust(&self, anchor: &TrustAnchorDto) -> HeadResult<TrustResult> {
        // Clients refuse a malformed anchor at the handshake, and trust here is a one-way door:
        // one stored would lock every client out of this warehouse for good.
        anchor.validate().map_err(HeadError::unprocessable)?;

        let existing = self.refs.get_trust().map_err(HeadError::internal)?;

        // The stored bytes travel with `get_trust` so a write can condition on exactly what was
        // read (FORK-95, claim C13). This path is a read-validate-write, and validating is not
        // holding: the incumbent read here is the value the chain-of-custody test below runs
        // against, and `replace_trust` conditions the write on it being unmoved.
        let Some((existing, existing_bytes)) = existing else {
            // No anchor but an office: the anchor was lost, and whichever client lifts first
            // would otherwise plant its own over a chain it never verified. No head lifts an
            // office without an anchor, so the office cannot appear after this read.
            let office_head = self
                .refs
                .get_head(PalletNamespace::Meta, OFFICE_PALLET_NAME)
                .map_err(HeadError::internal)?;
            if office_head.is_some() {
                return Err(Self::anchor_lost());
            }
            self.require_adopted_history(&anchor)?;

            return match self
                .refs
                .put_trust_if_absent(&anchor.to_anchor())
                .map_err(HeadError::internal)?
            {
                TrustOutcome::Established => Ok(TrustResult::Established),
                TrustOutcome::AlreadyIdentical => Ok(TrustResult::Unchanged),
                TrustOutcome::Conflict => Err(self.trust_one_way_door()),
            };
        };

        let existing_dto = TrustAnchorDto::from(&existing);

        if existing_dto == *anchor {
            return Ok(TrustResult::Unchanged);
        }

        // A re-genesis anchor names the current genesis as its prior and adopts the office
        // head exactly as it stands — nothing of the old chain may be silently dropped.
        let is_regenesis = anchor.prior_genesis.as_deref() == Some(existing.genesis.as_str());

        if !is_regenesis {
            return Err(self.trust_one_way_door());
        }

        let office_head = self
            .refs
            .get_head(PalletNamespace::Meta, OFFICE_PALLET_NAME)
            .map_err(HeadError::internal)?;

        if anchor.adopts.as_deref() != office_head.as_deref() {
            return Err(self.adopts_mismatch(anchor.adopts.as_deref(), office_head.as_deref()));
        }

        // The two tests above ran against values that can move underneath them, so the write
        // conditions on both and this match is where those refusals land. Before FORK-95 the
        // write was a bare unconditional `put_item`: two concurrent re-geneses could each pass
        // the `prior_genesis` test and both write, one genesis silently overwriting another
        // through a door this code calls one-way; and the office head could move between the
        // `adopts` test and the write, planting an anchor that drops precisely the history that
        // test protects.
        //
        // A transient refusal is re-issued, alone and under one request token, before it is
        // answered (see `reissue.rs`); what reaches this match is the last attempt's outcome.
        // Whether *any* attempt may have landed is kept apart from it: under one token, an
        // earlier attempt of unknown fate can still commit after a later one is refused.
        let replacement = anchor.to_anchor();
        let uncertain = Cell::new(false);
        let outcome = reissue(
            self.deadline,
            |token| {
                self.refs.replace_trust(&replacement, &existing_bytes, office_head.as_deref(), token)
            },
            |outcome| match outcome {
                TrustWriteOutcome::Transient(cause) => Some(noting_uncertainty(&uncertain, *cause)),
                _ => None,
            },
        )
        .map_err(HeadError::internal)?;

        match outcome {
            TrustWriteOutcome::Replaced => Ok(TrustResult::Established),
            // Someone else planted the identical anchor inside this request's window. The
            // read-side `existing_dto == *anchor` check above answers exactly this case with
            // `Unchanged` when the anchor was already identical when read; answering it any
            // differently here would make the documented idempotency of `PUT /v1/trust` hold
            // only when no one else is asking for the same thing at the same time.
            TrustWriteOutcome::AlreadyIdentical => Ok(TrustResult::Unchanged),
            // The same `409` the read-side test gives, but *not* the same message. This arm is
            // reached by a re-genesis that was authorized and validated correctly and merely lost
            // a race; `trust_one_way_door` says the replacement is impossible, which would tell
            // an operator to stop when the remedy is to re-read and re-run.
            TrustWriteOutcome::AnchorMoved { current } => {
                Err(self.regenesis_lost_the_race(current.as_deref()))
            }
            // Same `422`, from the same helper, as the read-side `adopts` test — a caller cannot
            // tell whether the mismatch was there when it arrived or opened underneath it, and
            // does not need to: the remedy is identical.
            TrustWriteOutcome::OfficeMoved { current } => {
                Err(self.adopts_mismatch(anchor.adopts.as_deref(), current.as_deref()))
            }
            TrustWriteOutcome::Transient(_) => Err(self.anchor_write_contention(uncertain.get())),
        }
    }

    /// A re-genesis refused because another one replaced the incumbent it validated against.
    ///
    /// Deliberately not [`trust_one_way_door`](Self::trust_one_way_door), which this arm used to
    /// share. That message says trust "cannot be replaced" — true of an unsanctioned anchor
    /// arriving at an established warehouse, and false here: this caller *was* entitled to
    /// replace, and the only thing wrong with its request is that it names an incumbent that is
    /// one re-genesis out of date. Telling it the door is closed points at giving up; the
    /// actionable answer is to re-read and re-run.
    /// `current` is the incumbent's stored bytes as the refused write found them, or `None` when
    /// no anchor is present at all.
    ///
    /// Three states, three **remedies** — which is the point of separating them, and the reason
    /// they cannot share a tail. Only the replaced case can be answered by re-running: an absent
    /// anchor has nothing to re-read, and an undecodable one cannot even be read, because both
    /// stores' `get_trust` returns `Err` on bytes that will not decode, so `put_trust` would map
    /// a re-run to a `500` rather than reaching this path again. Telling an operator to re-run in
    /// either case is advice that provably cannot be followed.
    fn regenesis_lost_the_race(&self, current: Option<&str>) -> HeadError {
        let Some(bytes) = current else {
            return HeadError::conflict(
                "This warehouse's trust anchor is no longer the one this re-genesis was \
                validated against, and no anchor is present now. Nothing was changed. \
                Establish trust rather than re-running the re-genesis."
                    .to_string(),
            );
        };

        let Some(genesis) = genesis_of(bytes) else {
            return HeadError::conflict(
                "This warehouse's trust anchor changed while this re-genesis was being \
                validated, and the anchor now stored cannot be decoded. Nothing was changed. \
                Re-running will not help — reading a trust anchor that will not decode fails \
                outright — so the warehouse's stored trust needs repairing before any \
                re-genesis can proceed."
                    .to_string(),
            );
        };

        HeadError::conflict(format!(
            "Another re-genesis replaced this warehouse's trust anchor while this one was \
            being validated; its genesis is now {}. Nothing was changed. Re-read the anchor \
            and re-run the re-genesis against the new incumbent if it should still be \
            replaced.",
            genesis
        ))
    }

    /// The transient refusal of the *anchor write*, as opposed to a ref-update commit.
    ///
    /// Separate from [`transient_contention`](Self::transient_contention) because that message
    /// names a push, a commit and a ref update, and this path is none of the three — a client
    /// told to "run the push again" for a re-genesis it issued would be looking for an operation
    /// it never performed.
    ///
    /// Reached only once the head's own re-issues are spent or the invocation window cannot
    /// afford another, so the message says the head already retried rather than inviting the
    /// operator to wonder whether it did.
    ///
    /// When `uncertain` — some attempt of this request may yet land — the message claims nothing
    /// about what was changed and sends the operator to read the anchor first.
    fn anchor_write_contention(&self, uncertain: bool) -> HeadError {
        if uncertain {
            return HeadError::service_unavailable(format!(
                "The trust anchor write could not be confirmed: {} Read the warehouse's trust \
                anchor before running the re-genesis again — if it is already the new anchor, the \
                re-genesis succeeded.",
                UNCONFIRMED
            ));
        }

        HeadError::service_unavailable(
            "The trust anchor could not be replaced: the warehouse's storage stayed busy \
            through every attempt this request could afford, most likely another request \
            touching its office pallet at the same time. Neither the anchor nor the office head \
            is known to have moved, and nothing was changed. Run the re-genesis again once the \
            other request has settled."
                .to_string(),
        )
    }

    /// The re-genesis refusal for an anchor whose `adopts` does not name the warehouse's office
    /// head — `422`, since the reset would silently drop history.
    ///
    /// One helper for both the read-side test and the write's own `OfficeMoved` refusal. They are
    /// the same refusal about the same mismatch, and two hand-written copies of a message this
    /// specific would drift.
    fn adopts_mismatch(&self, adopts: Option<&str>, office_head: Option<&str>) -> HeadError {
        HeadError::unprocessable(format!(
            "The re-genesis anchor adopts office head {}, but this warehouse's office \
            head is {}. The reset would drop history; re-run the re-genesis from a \
            warehouse in sync with this one.",
            adopts.unwrap_or("(none)"),
            office_head.unwrap_or("(unborn)")
        ))
    }

    /// `POST /v1/pallets/{name}` — the CAS ref update, the commit point of a lift and the
    /// place the head enforces everything (DESIGN.html §4.2 step 6): closure presence,
    /// fast-forward-ness, and — on a trusted warehouse — the same audit the CLI runs
    /// offline. "Everything" is content-level only, exactly what the CLI's own offline audit
    /// checks, and it differs by pallet: for the office pallet, every office-modifying parcel's
    /// *signer* is checked against the office role it held as of that parcel's own signing
    /// (`verify_office_chain`); for a meta pallet (`meta_tree::meta_rules`),
    /// every new parcel passes `verify_meta_content` against the verified office, and the pallet
    /// is refused outright without one; for any other pallet, a parcel is accepted when it is
    /// either validly signed by a key the office currently tracks and has not revoked, or
    /// unsigned/untracked-key but reachable from the trust boundary (tolerated as legacy), or
    /// signed by a revoked key but reachable from that revocation's own distrust boundary — no
    /// role, no grant, in any of the three arms
    /// (`verify_pallet_history` → `classify_signature_trust`). There is no check on who
    /// transported this request either way — this type takes no caller parameter at all — so a
    /// caller who can produce a validly-signed history for a pallet can move it regardless of
    /// what an office role would say about *that caller*, and — outside the office pallet —
    /// regardless of what it would say about the *signer* too: an operator enrolled as
    /// `reader`, or a `writer` granted only some other pallet, can sign and push this pallet's
    /// history because no transport rule here consults `may_write_pallet` (only the `@haul`
    /// content rule does, for a merged-state cell). The
    /// audit runs against a scratch warehouse mirrored from the object store; the atomic CAS is
    /// the DynamoDB conditional write of [`RefStore::compare_and_set_head`].
    pub fn ref_update(&self, name: &str, request: &RefUpdateRequest) -> HeadResult<()> {
        let pallet_ref = PalletRef::parse(name).map_err(HeadError::unprocessable)?;
        let namespace = pallet_ref.namespace;
        let bare = pallet_ref.name.clone();
        let is_meta = namespace == PalletNamespace::Meta;
        let is_office = is_meta && bare == OFFICE_PALLET_NAME;

        // A meta name this head does not know is refused outright, never stored unchecked.
        if is_meta {
            meta_tree::check_meta_pallet_name(&bare).map_err(HeadError::unprocessable)?;
        }

        let pallet_rules = if is_meta { meta_tree::meta_rules(&bare) } else { None };

        // Fail fast on an obviously stale request; the conditional write below is the
        // authoritative gate that also catches a head that moves during the audit.
        let current = self.refs.get_head(namespace, &bare).map_err(HeadError::internal)?;

        if current.as_deref() != request.old_head.as_deref() {
            return Err(self.moved(&current, &request.old_head));
        }

        // The head is stored as given; refuse a spelling the ref reader would refuse, the same
        // answer the self-hosted server gives, before presence decides it by S3's key casing.
        if !object_utils::is_object_hash(&request.new_head) {
            return Err(HeadError::unprocessable(
                "The new head is not an object hash (64 lowercase hex characters)."
            ));
        }

        // The new head must be present — a ref never points at a missing parcel.
        if !self.objects.exists(&request.new_head).map_err(HeadError::internal)? {
            return Err(HeadError::unprocessable(format!(
                "The new head {} has not been uploaded.",
                request.new_head
            )));
        }

        // The anchor's stored bytes travel alongside the decoded value: the commit below
        // conditions on exactly what was read here, never a re-serialization of it (FORK-95
        // design memo, claim C13) — a `serde` field reorder must never fail a push's anchor
        // check for no anchor that actually moved.
        let (anchor, anchor_bytes) = match self.refs.get_trust().map_err(HeadError::internal)? {
            Some((anchor, bytes)) => (Some(anchor), Some(bytes)),
            None => (None, None),
        };

        // An office is the chain an anchor verifies; one lifted without an anchor is a state
        // clients read as a stripped anchor, so it is never created. Same as the server, and
        // decided before any audit work. The commit conditions on the anchor still being absent.
        if is_office && anchor.is_none() {
            return Err(HeadError::unprocessable(
                "Establish the trust anchor (PUT /v1/trust) before lifting the office.".to_string(),
            ));
        }

        let office_head = self
            .refs
            .get_head(PalletNamespace::Meta, OFFICE_PALLET_NAME)
            .map_err(HeadError::internal)?;

        // Mirror the objects the audit reads into a scratch `.forklift`, then run the exact
        // `forklift_core` audit inside its scope. Working blobs are never mirrored — their
        // presence is answered by the object store (an S3 HEAD).
        let scratch = self.scratch().map_err(HeadError::internal)?;

        scratch.scoped(|| -> HeadResult<()> {
            let mut mirror = Mirror::default();

            // On a trusted warehouse the office chain must be readable (it carries the keys
            // that verify every other pallet). Its "blobs" are tracked-metadata records the
            // audit reads, so they are mirrored too — and never bounded, because
            // `verify_office_chain` walks from the head to the genesis every time.
            if anchor.is_some() {
                if let Some(office_head) = &office_head {
                    materialize(&self.objects, office_head, true, None, &mut mirror)
                        .map_err(HeadError::internal)?;
                }
            }

            // Everything reachable from the pallet's current head was audited when that head
            // was committed, so the target mirror stops expanding trees there. The office is
            // the exception (see above); a creation has no bound at all.
            let known_complete = if is_office { None } else { request.old_head.as_deref() };

            // The target closure: a meta pallet's blobs are records (mirror them); a
            // working pallet's blobs are file content (leave them in the object store).
            materialize(&self.objects, &request.new_head, is_meta, known_complete, &mut mirror)
                .map_err(HeadError::internal)?;

            // A meta pallet's content check diffs each new parcel against its parent, so
            // the current head's state is read too.
            if let (Some(_), Some(old_head)) = (pallet_rules, &request.old_head) {
                materialize_state(&self.objects, old_head, &mut mirror).map_err(HeadError::internal)?;
            }

            // 1. Closure presence. A working blob and a chunked file's recipe are presence-checked
            // one hash at a time via `blob_exists`; a chunked file's recipe is read from the object
            // store too (it is a file-entry object, so `materialize` above never mirrored it into
            // the scratch), and its chunks are presence-checked non-tolerantly — a ref must never
            // advance over a chunked file whose chunks never reached storage (§9.4b W4).
            let blob_exists = |hash: &str| self.objects.exists(hash);
            // The chunk-presence seam (§9.4b W4): a changed chunked file's *whole* chunk list is
            // probed in one bulk call, which the S3 store answers with a bounded-concurrency batch
            // of HeadObjects. Walked serially (one HEAD per chunk), a 1–2 GiB file's ~1–2k chunks
            // already crowd API Gateway's 29 s integration ceiling; in parallel they clear in a
            // second or two. Still non-tolerant: any missing chunk refuses the ref.
            let chunks_missing = |chunks: &[String]| self.objects.objects_missing(chunks);
            let load_recipe_chunks = |hash: &str| -> Result<Vec<String>, String> {
                let bytes = self
                    .objects
                    .get(hash)?
                    .ok_or_else(|| format!("Recipe {} is missing.", hash))?;

                Ok(object_utils::parse_recipe_bytes(hash, &bytes)?
                    .chunks
                    .into_iter()
                    .map(|chunk| chunk.hash)
                    .collect())
            };
            // The subtree prune (§9.4b W1) reads a candidate base's trees to skip subtrees some
            // candidate already carries at that path. The candidates are the prior head's tree and
            // each parcel's own immediate parents' — the first already-audited history, the second
            // audited earlier in the same call by the parents-first order.
            //
            // The prior head's tree is *not* in the audit scratch, so that read has to come from
            // the object store. `materialize` clears `full` at the bound *hash*, not at the fresh
            // frontier — so it stops expanding along paths through the bound and nowhere else. A
            // merge's second parent is below no bound, so it and its whole ancestry are expanded
            // full; the property this seam rests on is only ever about `known_complete` itself.
            //
            // An in-segment parent's trees, by contrast, are mirrored (such a parcel is expanded
            // `full`), so those reads re-fetch bytes already on local disk. That waste is accepted
            // rather than special-cased: reading a parent's base locally and the bound's remotely
            // would make this one seam two, and the local one would be correct only for as long as
            // `materialize` keeps expanding every in-segment parent. One seam, one rule.
            //
            // The prune's point, either way: an unchanged large chunked file below such a subtree
            // is skipped whole, sparing the ~million per-chunk S3 `HEAD`s its recipe descent would
            // otherwise cost per push. Reads must stay on this seam: a direct
            // object_utils::load_tree would pass every self-host test, and pass or fail here by
            // container history rather than by anything a test can name — the pool is keyed per
            // warehouse and per process, so whether the bound's tree happens to be on disk depends
            // on which pushes this container served before, not on "cold" versus "warm". The prune
            // then turns the failures it does hit into a silent full walk, reopening on this head
            // the very asymmetry the prune closes. There is no environment class that reliably
            // catches it, which is why the rule is the seam and not a test.
            let load_base_tree = |hash: &str| -> Result<TreeItem, String> {
                let bytes = self
                    .objects
                    .get(hash)?
                    .ok_or_else(|| format!("Tree {} is missing.", hash))?;

                object_utils::parse_tree_bytes(hash, &bytes)
            };
            audit_utils::verify_parcel_closure_with(
                &request.new_head,
                request.old_head.as_deref(),
                &blob_exists,
                &chunks_missing,
                &load_recipe_chunks,
                &load_base_tree,
            )
            .map_err(HeadError::unprocessable)?;

            // 2. Fast-forward, with the one sanctioned exception: the office lift right
            // after a re-genesis, moving away from the anchor's adopted pin (§8.7).
            if let Some(old_head) = &request.old_head {
                let adopted_reset = is_office
                    && anchor.as_ref().and_then(|a| a.adopts.as_deref()) == Some(old_head.as_str());

                if !adopted_reset
                    && !merge_utils::is_ancestor(old_head, &request.new_head)
                        .map_err(HeadError::internal)?
                {
                    return Err(HeadError::conflict(
                        "The update is not a fast-forward; the protocol has no force push. \
                        Lower, consolidate, and lift the merge."
                            .to_string(),
                    ));
                }
            }

            // 3. Trust: a trusted warehouse accepts nothing a local audit would reject. A
            // meta pallet exists only under an office, so it is checked even without
            // an anchor — and refused for the lack of one. Its content check subsumes the
            // signature audit: every new parcel is signed by an office key, stamps only its own
            // cells, and keeps the pallet's rules.
            if let Some(rules) = pallet_rules {
                meta_tree::verify_meta_update(
                    &bare,
                    rules,
                    meta_tree::Admission::Admit,
                    anchor.as_ref(),
                    office_head.as_deref(),
                    request.old_head.as_deref(),
                    &request.new_head,
                )
                .map_err(HeadError::unprocessable)?;
            } else if let Some(anchor) = &anchor {
                if is_office {
                    // The office chain carries the keys; verify it against the anchor, and
                    // that every parcel stayed within its signer's privileges.
                    audit_utils::verify_office_chain_memoized(anchor, &request.new_head)
                        .map_err(|error| HeadError::office_lift(error, office_head.as_deref()))?;
                } else {
                    // Any non-office pallet: audit its new history against the office state.
                    let office_head = office_head.as_deref().ok_or_else(|| {
                        HeadError::unprocessable(
                            "Trust is established but the office pallet is missing; lift the \
                            office first."
                                .to_string(),
                        )
                    })?;

                    let office_state = audit_utils::verify_office_chain_memoized(anchor, office_head)
                        .map_err(HeadError::unprocessable)?;

                    audit_utils::verify_pallet_history(
                        &request.new_head,
                        anchor,
                        &office_state,
                        request.old_head.as_deref(),
                    )
                    .map_err(HeadError::unprocessable)?;
                }
            }

            Ok(())
        })?;

        // The commit conditions on the office head only when the audit above actually
        // consumed it — an untrusted warehouse's audit never reads it (both the office-chain
        // mirror and the whole trust block are gated on `anchor.is_some()`), so conditioning
        // on it unconditionally would refuse a push for an office move the audit never
        // depended on, or worse: an untrusted office pallet may carry a real, unaudited head,
        // and treating "not consumed" as "must be unborn" would refuse *every* such push, race
        // or not. `OfficePrecondition`'s own docs (`store.rs`) record why `NotConsumed` and
        // "the office pallet is unborn" cannot be the same value.
        let office_head_for_commit = match (anchor.is_some(), office_head.as_deref()) {
            (true, Some(head)) => OfficePrecondition::At(head),
            _ => OfficePrecondition::NotConsumed,
        };

        // The commit: one DynamoDB transaction conditioned on every movable input the audit
        // above consumed — the pallet's own head, the office head (when read), and the trust
        // anchor — so a concurrent office lift or re-genesis refuses this update exactly as a
        // concurrent lift to this same pallet always has (FORK-95 design memo, "Two
        // instantiations of one invariant" / claim C13). A refusal that shows no input moved is
        // re-issued — the transaction alone, never the audit — under one request token and one
        // bounded budget before it is answered (claim C14, arm two; see `reissue.rs`). As for the
        // anchor write, whether any attempt may have landed is tracked across all of them.
        let uncertain = Cell::new(false);
        let outcome = reissue(
            self.deadline,
            |token| {
                self.refs.compare_and_set_head(
                    namespace,
                    &bare,
                    request.old_head.as_deref(),
                    &request.new_head,
                    office_head_for_commit,
                    anchor_bytes.as_deref(),
                    token,
                )
            },
            |outcome| match outcome {
                CasOutcome::Transient(cause) => Some(noting_uncertainty(&uncertain, *cause)),
                _ => None,
            },
        )
        .map_err(HeadError::internal)?;

        match outcome {
            CasOutcome::Committed => Ok(()),
            CasOutcome::Conflict { current } => Err(self.moved(&current, &request.old_head)),
            CasOutcome::OfficeMoved { current } => Err(self.office_moved(&current)),
            CasOutcome::AnchorMoved => Err(self.anchor_moved()),
            CasOutcome::Transient(_) => Err(self.transient_contention(uncertain.get())),
        }
    }

    /// `POST /v1/objects/batch` — many objects in one bundle-format stream (forklift's
    /// packfile moment). Objects the store lacks are simply absent; the client falls back
    /// to loose fetches. Built by mirroring the requested objects into a scratch and
    /// reusing `bundle_utils::build_partial_bundle`.
    ///
    /// The bundle is the one *response* that has no small bound — it is as large as the
    /// objects asked for — so a store that can offload it hands back a presigned `GET`
    /// rather than squeezing megabytes back through the control plane. Same medicine as the
    /// upload path, in the other direction — except the router answers `303`, not `307`, since
    /// this request is a `POST` and the presigned target only ever accepts `GET`.
    pub fn batch(&self, hashes: &[String]) -> HeadResult<BatchResult> {
        self.reject_malformed_batch(hashes)?;

        let scratch = self.scratch().map_err(HeadError::internal)?;

        let bundle = scratch.scoped(|| -> HeadResult<Vec<u8>> {
            let mut mirrored: HashSet<String> = HashSet::new();

            for hash in hashes {
                if !mirrored.insert(hash.clone()) {
                    continue;
                }

                // A warm scratch may already hold it, mirrored from this same warehouse.
                if !file_utils::does_object_exist(hash).map_err(HeadError::internal)? {
                    if let Some(bytes) = self.objects.get(hash).map_err(HeadError::internal)? {
                        object_utils::store_object_bytes(hash, &bytes)
                            .map_err(HeadError::internal)?;
                    }
                }

                if sign_utils::load_raw_parcel_signature(hash).map_err(HeadError::internal)?.is_none()
                {
                    if let Some(sidecar) =
                        self.objects.get_signature(hash).map_err(HeadError::internal)?
                    {
                        sign_utils::store_raw_parcel_signature(hash, &sidecar)
                            .map_err(HeadError::internal)?;
                    }
                }
            }

            // `build_partial_bundle` skips whatever is absent from the scratch, so a hash the
            // store lacks is simply not in the bundle — the documented contract.
            bundle_utils::build_partial_bundle(hashes).map_err(HeadError::internal)
        })?;

        match self.objects.offload_response(&bundle).map_err(HeadError::internal)? {
            Some(url) => Ok(BatchResult::Redirect(url)),
            None => Ok(BatchResult::Bundle(bundle)),
        }
    }

    /// `GET /v1/bundles/latest` — the whole-warehouse bundle. Building it is periodic ECS
    /// work in the hosted deployment (DESIGN.html §4.3/§4.6); until a builder runs, this
    /// is a spec-compliant `404` and clients fall back to loose/batch fetches.
    pub fn bundle_latest(&self) -> HeadResult<Vec<u8>> {
        Err(HeadError::not_found("No bundle has been built.".to_string()))
    }

    /// `POST /lift/{session}/commit` — the additive session-commit step (the split-verify
    /// decision, 2026-07-06). In the AWS deployment the client uploads straight to a staging
    /// prefix, so the control plane never saw the bytes. This is where the small
    /// control-plane objects of `session` are **verified and promoted** to their hash keys
    /// (synchronously, via [`ObjectStore::verify_and_promote`]) — until it runs they are not
    /// fetchable, and a corrupt one is discarded and stops the lift rather than ever
    /// becoming visible.
    ///
    /// Large blobs are only checked for *presence at their canonical key*, which is itself
    /// the proof they were verified: the staging verifier promotes a blob only after the
    /// same `Blake3(bytes) == hash` check. A blob absent there is probed in the session's
    /// staging area: still staged means the verifier has not caught up, answered as a `503`
    /// with `Forklift-Commit: not-ready` that the client retries; gone from both places means
    /// the upload was discarded (the verifier rejected it) or never sent, a terminal `422`
    /// whose next step is running the lift again.
    ///
    /// Promotion is idempotent, so a retried commit is safe. Once everything is promoted the
    /// session's staging prefix is swept — but only on the **final** batch (`more == false`). A
    /// lift touching a maximal chunked file lists too many chunk hashes for one request, so the
    /// client paginates and sets `more: true` on every batch but the last; an early batch verifies
    /// and presence-checks its slice without sweeping, because the sweep is session-wide and would
    /// otherwise discard chunks a later batch still needs staged. An old client never sets `more`
    /// (it defaults to `false`), so a single-shot lift verifies, presence-checks, and sweeps
    /// exactly as before.
    pub fn commit_lift(
        &self,
        session: &str,
        control_plane: &[String],
        blobs: &[String],
        more: bool,
    ) -> HeadResult<()> {
        for hash in control_plane {
            let outcome =
                self.objects.verify_and_promote(session, hash).map_err(HeadError::internal)?;

            match outcome {
                PromoteOutcome::Promoted | PromoteOutcome::AlreadyPresent => {}
                PromoteOutcome::Missing => {
                    // Not "never arrived": an earlier attempt of this commit whose answer was
                    // lost may have discarded it as corrupt or too large.
                    return Err(upload_discarded(format!(
                        "Object {} is not on the remote: its upload was discarded (rejected as \
                        corrupt or too large) or never arrived. The lift session cannot commit.",
                        hash
                    )))
                }
                PromoteOutcome::Corrupt { actual } => {
                    return Err(upload_discarded(format!(
                        "Staged object {} is corrupt (it hashes to {}); it was discarded, not \
                        promoted, and the lift session cannot commit.",
                        hash, actual
                    )))
                }
            }
        }

        self.check_blobs_promoted(session, blobs)?;

        // Sweep the session's staging prefix only when this is the final batch. An intermediate
        // batch (`more`) leaves staging intact so a later batch's still-staged chunks survive to
        // be presence-checked; the final batch (or the only batch of a single-shot lift) sweeps.
        //
        // A client that never sends a final (`more: false`) batch — crashed, killed, or simply
        // abandoned mid-lift — leaves its staged objects unswept forever: no code path here ever
        // revisits a session it was not explicitly told is done, and paginating widens the window
        // (§9.4b Stage 3, W2). This is an operational gap, not a code one: the deploying operator
        // MUST configure an S3 lifecycle rule expiring the `staging/` prefix (see the "Operational
        // requirement" section of `aws::s3`'s module docs for the full reasoning and the suggested
        // age).
        if !more {
            self.objects.discard_session(session).map_err(HeadError::internal)?;
        }

        Ok(())
    }

    /// Whether every one of a lift commit's `blobs` is at its canonical key, and if not, which
    /// answer the commit gets: a terminal `422` (`staged_upload_discarded`) when some blob is in
    /// neither place, else the retriable `503` not-ready while some blob is still staged. Each
    /// names the first such blob in `blobs` order.
    ///
    /// The probes run canonical, then staged, then canonical again for the blobs found in neither,
    /// and the last probe is what makes "in neither" sound: a blob promoted between the first two
    /// probes has left staging, and would otherwise read as discarded. That needs promotion to
    /// write the canonical key before it drops the staged copy, which S3's `verify_and_promote`
    /// does on every promoting path; the in-memory store does both under the `objects` lock.
    ///
    /// Bulk probes, not one `exists` per blob: a large chunked commit batch can name thousands of
    /// blobs, and a serial walk of them is exactly the pattern that blows API Gateway's 29 s
    /// ceiling. The staging and re-probe passes cost nothing unless some blob is missing.
    fn check_blobs_promoted(&self, session: &str, blobs: &[String]) -> HeadResult<()> {
        let missing = self.objects.objects_missing(blobs).map_err(HeadError::internal)?;

        if missing.is_empty() {
            return Ok(());
        }

        let staged: HashSet<String> = self
            .objects
            .objects_staged(session, &missing)
            .map_err(HeadError::internal)?
            .into_iter()
            .collect();

        let unstaged: Vec<String> =
            missing.into_iter().filter(|hash| !staged.contains(hash)).collect();

        let gone: HashSet<String> =
            self.objects.objects_missing(&unstaged).map_err(HeadError::internal)?.into_iter().collect();

        if let Some(hash) = blobs.iter().find(|hash| gone.contains(hash.as_str())) {
            return Err(upload_discarded(format!(
                "Blob {} is not on the remote: its upload was discarded (rejected as corrupt or too \
                large) or never arrived. The lift session cannot commit.",
                hash
            )));
        }

        if let Some(hash) = blobs.iter().find(|hash| staged.contains(hash.as_str())) {
            return Err(HeadError::commit_not_ready(format!(
                "Blob {} is uploaded but the remote has not accepted it into the warehouse yet; \
                the lift session is not ready to commit.",
                hash
            )));
        }

        // Every blob missing at the first probe was promoted before the re-probe.
        Ok(())
    }

    /// The `422` for a batch request over the protocol cap, or naming anything but object hashes
    /// ([`require_object_hash`]).
    fn reject_malformed_batch(&self, hashes: &[String]) -> HeadResult<()> {
        if hashes.len() > MAX_MISSING_BATCH {
            return Err(HeadError::unprocessable(format!(
                "At most {} hashes per request; batch larger sets.",
                MAX_MISSING_BATCH
            )));
        }

        hashes.iter().try_for_each(|hash| require_object_hash(hash))
    }

    /// The `409` for a ref that is not where the client expected it.
    fn moved(&self, current: &Option<String>, expected: &Option<String>) -> HeadError {
        HeadError::conflict(String::from(
            forklift_core::util::pallet_utils::pallet_moved_refusal(current.as_deref(), expected.as_deref())
        ))
    }

    /// The `409` for an office lift landing between the audit and the commit: the pallet
    /// itself did not move, but the office state the audit trusted did.
    fn office_moved(&self, current: &Option<String>) -> HeadError {
        HeadError::conflict(String::from(forklift_core::util::pallet_utils::remote_ahead_refusal(format!(
            "The office pallet moved during the audit: its head is now {}. Lower and retry.",
            current.as_deref().unwrap_or("unborn")
        ))))
    }

    /// The `409` for the trust anchor changing between the audit and the commit (established,
    /// or replaced by a re-genesis).
    fn anchor_moved(&self) -> HeadError {
        HeadError::conflict(
            "The trust anchor changed during the audit; re-read the warehouse and retry."
                .to_string(),
        )
    }

    /// The `503` for a commit refused for a reason that shows no input moved — most often
    /// DynamoDB's own concurrency control (`TransactionConflict`) with another push racing this
    /// one for the shared office item. Unlike `moved`/`office_moved`/`anchor_moved`, this does not
    /// claim any value changed, so it must never be worded as one of those: a later attempt may
    /// succeed with nothing having moved at all.
    ///
    /// Reached only after the head has re-issued the commit itself as many times as one budget
    /// and the invocation window allow (claim C14, arm two; `reissue.rs`). The client's
    /// `update_ref` has no retry path of its own for this status, so the wording says "run the
    /// push again" — an instruction a person or a script can act on — and never "retry" bare,
    /// which would imply a client-side mechanism that does not exist.
    ///
    /// When `uncertain` — some attempt of this request may yet land — running the identical push
    /// again could be refused as the pallet having moved, to the client's own new head. So that
    /// message says the outcome is unknown and how to tell, rather than "nothing moved".
    fn transient_contention(&self, uncertain: bool) -> HeadError {
        if uncertain {
            return HeadError::service_unavailable(format!(
                "The commit could not be confirmed: {} Fetch the warehouse before pushing again — \
                if the pallet is already at the pushed head, the push succeeded.",
                UNCONFIRMED
            ));
        }

        HeadError::service_unavailable(
            "The commit could not complete: the warehouse's storage stayed busy through every \
            attempt this request could afford, most likely another push touching its office \
            pallet at the same time. No input is known to have moved. Run the push again once \
            the other push has settled."
                .to_string(),
        )
    }

    /// Refuse a first anchor whose `adopts` names history this warehouse does not completely
    /// hold. The replacement path needs no such check — it requires `adopts` to be the office
    /// head — but a first anchor arrives over an unborn office, and every warehouse that adopts it
    /// later fetches that history from here. The client uploads it before the anchor.
    ///
    /// The whole closure is walked, as the lift audit walks it: the adopted chain is an office
    /// chain, so its parcels, trees and record blobs are mirrored into the scratch and checked
    /// there, with the chunk and recipe seams answered by the object store.
    fn require_adopted_history(&self, anchor: &TrustAnchorDto) -> Result<(), HeadError> {
        let Some(adopts) = anchor.adopts.as_deref() else {
            return Ok(());
        };

        let scratch = self.scratch().map_err(HeadError::internal)?;

        scratch.scoped(|| -> HeadResult<()> {
            let mut mirror = Mirror::default();
            materialize(&self.objects, adopts, true, None, &mut mirror).map_err(HeadError::internal)?;

            let verify_closure = |head: &str| {
                audit_utils::verify_parcel_closure_with(
                    head,
                    None,
                    &|hash| self.objects.exists(hash),
                    &|chunks| self.objects.objects_missing(chunks),
                    &|hash| {
                        let bytes = self
                            .objects
                            .get(hash)?
                            .ok_or_else(|| format!("Recipe {} is missing.", hash))?;

                        Ok(object_utils::parse_recipe_bytes(hash, &bytes)?
                            .chunks
                            .into_iter()
                            .map(|chunk| chunk.hash)
                            .collect())
                    },
                    // A creation (`known_complete: None`) takes no prune candidates.
                    &|hash| Err(format!("Tree {} was read as a prune base, but nothing bounds this walk.", hash)),
                )
            };

            audit_utils::require_adopted_history(Some(adopts), &verify_closure)
                .map_err(HeadError::unprocessable)
        })
    }

    /// The `409` for planting an anchor over an office whose own anchor was lost: the chain was
    /// never verified against the anchor offered, so no client may supply one.
    fn anchor_lost() -> HeadError {
        HeadError::conflict(
            "This warehouse holds an office but no trust anchor: its anchor was lost. The \
            server operator must restore it; a client cannot re-establish it."
                .to_string(),
        )
    }

    /// The `409` for trying to replace an established trust anchor.
    fn trust_one_way_door(&self) -> HeadError {
        HeadError::conflict(
            "This warehouse already has a different trust anchor; trust is a one-way door \
            and cannot be replaced."
                .to_string(),
        )
    }
}

/// The `422` for a client-sent hash spelled any way but the store's own, 64 lowercase hex
/// characters: the same refusal the self-hosted server gives, rather than a miss decided by S3's
/// key casing. The value is not repeated back.
fn require_object_hash(hash: &str) -> HeadResult<()> {
    if object_utils::is_object_hash(hash) {
        return Ok(());
    }

    Err(HeadError::unprocessable(REFUSAL_NOT_AN_OBJECT_HASH))
}

/// A lift commit's terminal refusal for an upload the remote does not hold: discarded by
/// verification, or never received. Retrying the commit cannot help; running the lift again
/// uploads it afresh.
fn upload_discarded(message: String) -> HeadError {
    HeadError::unprocessable(String::from(CoreError::refusal(
        RefusalCode::StagedUploadDiscarded,
        message,
        "Run the lift again; it uploads the object afresh.",
    )))
}

/// Why an uncertain transaction's `503` cannot say what happened, shared by both of its messages.
const UNCONFIRMED: &str = "an attempt timed out, failed inside the warehouse's storage, or was \
    still being applied when this request ran out of attempts, so it may or may not have been \
    applied.";

/// Pass `cause` through to the re-issue loop, first noting in `uncertain` whether this attempt
/// may yet land. Only a refusal for contention shows the attempt did not: an unknown fate may
/// have committed, and an attempt in progress is one still being applied.
fn noting_uncertainty(uncertain: &Cell<bool>, cause: TransientCause) -> TransientCause {
    if cause != TransientCause::Contended {
        uncertain.set(true);
    }

    cause
}
