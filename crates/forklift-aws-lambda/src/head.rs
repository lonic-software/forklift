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
//! Authentication and the transport-level role/grant checks are the adapter's
//! concern (the API Gateway authorizer decides *who* the caller is, then the same office
//! roles the server head consults gate *what* they may move). This type enforces the
//! provider-independent content invariants: hash-verified objects, a fast-forward-only CAS,
//! and — on a trusted warehouse — the full offline audit before a ref moves.
//!
//! **Every method here is synchronous and must be called from a blocking thread**
//! (`tokio::task::spawn_blocking`), exactly as `forklift-server` runs its handlers' storage
//! work. It mirrors objects into a thread-local-scoped scratch and runs `forklift_core`'s
//! audit inside that scope, so the call must never migrate between threads mid-flight; and
//! tokio refuses to let a runtime worker block on the futures an SDK-backed store bridges.
//! See `blocking.rs`.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use forklift_core::model::remote::{
    RefUpdateRequest, TrustAnchorDto, UploadTargetsResponse, WarehouseInfo,
    LIFT_SESSION_BLOB_NOT_READY, MAX_MISSING_BATCH, PROTOCOL_VERSION,
};
use forklift_core::model::tree_item::TreeItem;
use forklift_core::util::office_utils::OFFICE_PALLET_NAME;
use forklift_core::util::pallet_utils::{PalletNamespace, PalletRef};
use forklift_core::util::{
    audit_utils, bundle_utils, file_utils, merge_utils, object_utils, sign_utils,
};

use crate::error::{HeadError, HeadResult};
use crate::scratch::{materialize, Mirror, Scratch};
use crate::store::{
    CasOutcome, ObjectAccess, ObjectStore, OfficePrecondition, PromoteOutcome, PutTarget,
    RefStore, SignatureOutcome, TrustOutcome,
};

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
}

impl<O: ObjectStore, R: RefStore> Head<O, R> {
    /// Assemble a head over the two stores, mirroring into a fresh scratch per request.
    pub fn new(objects: O, refs: R) -> Head<O, R> {
        Head { objects, refs, scratch: ScratchSource::Ephemeral }
    }

    /// Assemble a head that reuses one scratch warehouse across warm invocations, so the
    /// audit mirror is paid once per container rather than once per request.
    ///
    /// `warehouse_id` must identify the warehouse these stores serve, and nothing else: an
    /// object found in the scratch is treated as present in the object store, and that
    /// inference is only sound within one warehouse (see [`Scratch::shared`]).
    pub fn pooled(objects: O, refs: R, warehouse_id: impl Into<String>) -> Head<O, R> {
        Head { objects, refs, scratch: ScratchSource::Pooled(warehouse_id.into()) }
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
        self.reject_oversized_batch(hashes.len())?;

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
        self.reject_oversized_batch(hashes.len())?;

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
        self.objects
            .get_signature(parcel_hash)
            .map_err(HeadError::internal)?
            .ok_or_else(|| HeadError::not_found("The parcel carries no signature.".to_string()))
    }

    /// `PUT /v1/signatures/{hash}` — store a signature sidecar. The structure is validated
    /// here (`422` when malformed); whether it *verifies* is decided at ref-update time.
    /// A conflicting sidecar for an already-signed parcel is refused (`409`).
    pub fn signature_put(&self, parcel_hash: &str, bytes: &[u8]) -> HeadResult<SignatureOutcome> {
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
        let existing = self.refs.get_trust().map_err(HeadError::internal)?;

        // The stored bytes travel with `get_trust` for the ref-update commit's anchor
        // precondition (FORK-95, claim C13); this read-validate-write path is untouched this
        // slice — its own conditional write is claim C22, deferred to a later slice — so the
        // bytes go unused here.
        let Some((existing, _existing_bytes)) = existing else {
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
            return Err(HeadError::unprocessable(format!(
                "The re-genesis anchor adopts office head {}, but this warehouse's office \
                head is {}. The reset would drop history; re-run the re-genesis from a \
                warehouse in sync with this one.",
                anchor.adopts.as_deref().unwrap_or("(none)"),
                office_head.as_deref().unwrap_or("(unborn)")
            )));
        }

        self.refs.replace_trust(&anchor.to_anchor()).map_err(HeadError::internal)?;

        Ok(TrustResult::Established)
    }

    /// `POST /v1/pallets/{name}` — the CAS ref update, the commit point of a lift and the
    /// place the head enforces everything (DESIGN.html §4.2 step 6): closure presence,
    /// fast-forward-ness, and — on a trusted warehouse — the same audit the CLI runs
    /// offline. The audit runs against a scratch warehouse mirrored from the object store;
    /// the atomic CAS is the DynamoDB conditional write of [`RefStore::compare_and_set_head`].
    pub fn ref_update(&self, name: &str, request: &RefUpdateRequest) -> HeadResult<()> {
        let pallet_ref = PalletRef::parse(name).map_err(HeadError::unprocessable)?;
        let namespace = pallet_ref.namespace;
        let bare = pallet_ref.name.clone();
        let is_meta = namespace == PalletNamespace::Meta;
        let is_office = is_meta && bare == OFFICE_PALLET_NAME;

        // Fail fast on an obviously stale request; the conditional write below is the
        // authoritative gate that also catches a head that moves during the audit.
        let current = self.refs.get_head(namespace, &bare).map_err(HeadError::internal)?;

        if current.as_deref() != request.old_head.as_deref() {
            return Err(self.moved(&current, &request.old_head));
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

            // 3. Trust: a trusted warehouse accepts nothing a local audit would reject.
            if let Some(anchor) = &anchor {
                if is_office {
                    // The office chain carries the keys; verify it against the anchor, and
                    // that every new parcel stayed within its signer's privileges.
                    audit_utils::verify_office_chain_memoized(anchor, &request.new_head)
                        .map_err(HeadError::unprocessable)?;

                    audit_utils::verify_office_privileges(
                        anchor,
                        request.old_head.as_deref(),
                        &request.new_head,
                    )
                    .map_err(HeadError::forbidden)?;
                } else {
                    // A user pallet: audit its new history against the office state.
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
        // instantiations of one invariant" / claim C13).
        match self
            .refs
            .compare_and_set_head(
                namespace,
                &bare,
                request.old_head.as_deref(),
                &request.new_head,
                office_head_for_commit,
                anchor_bytes.as_deref(),
            )
            .map_err(HeadError::internal)?
        {
            CasOutcome::Committed => Ok(()),
            CasOutcome::Conflict { current } => Err(self.moved(&current, &request.old_head)),
            CasOutcome::OfficeMoved { current } => Err(self.office_moved(&current)),
            CasOutcome::AnchorMoved => Err(self.anchor_moved()),
            CasOutcome::Transient => Err(self.transient_contention()),
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
        self.reject_oversized_batch(hashes.len())?;

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
    /// same `Blake3(bytes) == hash` check. A blob still sitting in staging simply reads as
    /// absent, and the client retries once the verifier has caught up.
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
                    return Err(HeadError::unprocessable(format!(
                        "Object {} was not uploaded; the lift session is not ready to commit.",
                        hash
                    )))
                }
                PromoteOutcome::Corrupt { actual } => {
                    return Err(HeadError::unprocessable(format!(
                        "Staged object {} is corrupt (it hashes to {}); it was discarded, not \
                        promoted, and the lift will not commit.",
                        hash, actual
                    )))
                }
            }
        }

        // One bulk probe instead of one `exists` per blob: a large chunked commit batch can name
        // thousands of blobs, and a serial walk of them is exactly the pattern that blows API
        // Gateway's 29 s ceiling. Still reports only the *first* not-yet-promoted blob in `blobs`
        // order, with the identical message — a bulk probe means every blob is checked (not just
        // the ones before the first miss), but the response is unchanged: the same one hash named,
        // in the same case.
        let not_ready: HashSet<String> =
            self.objects.objects_missing(blobs).map_err(HeadError::internal)?.into_iter().collect();

        if let Some(hash) = blobs.iter().find(|hash| not_ready.contains(hash.as_str())) {
            // The one *transient* commit failure: the staging verifier has not promoted this
            // blob to its canonical key yet. The message embeds `LIFT_SESSION_BLOB_NOT_READY`
            // so the client can tell this retriable case apart from a terminal one (a
            // control-plane object never uploaded, or a corrupt staged object) and back off.
            return Err(HeadError::unprocessable(format!(
                "Blob {} is {}; the lift session is not ready to commit.",
                hash, LIFT_SESSION_BLOB_NOT_READY
            )));
        }

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

    /// The `422` for a batch request over the protocol cap.
    fn reject_oversized_batch(&self, count: usize) -> HeadResult<()> {
        if count > MAX_MISSING_BATCH {
            return Err(HeadError::unprocessable(format!(
                "At most {} hashes per request; batch larger sets.",
                MAX_MISSING_BATCH
            )));
        }

        Ok(())
    }

    /// The `409` for a ref that is not where the client expected it.
    fn moved(&self, current: &Option<String>, expected: &Option<String>) -> HeadError {
        HeadError::conflict(format!(
            "The pallet moved: its head is {}, not {}. Lower and retry.",
            current.as_deref().unwrap_or("unborn"),
            expected.as_deref().unwrap_or("unborn")
        ))
    }

    /// The `409` for an office lift landing between the audit and the commit: the pallet
    /// itself did not move, but the office state the audit trusted did.
    fn office_moved(&self, current: &Option<String>) -> HeadError {
        HeadError::conflict(format!(
            "The office pallet moved during the audit: its head is now {}. Lower and retry.",
            current.as_deref().unwrap_or("unborn")
        ))
    }

    /// The `409` for the trust anchor changing between the audit and the commit (established,
    /// or replaced by a re-genesis).
    fn anchor_moved(&self) -> HeadError {
        HeadError::conflict(
            "The trust anchor changed during the audit; re-read the warehouse and retry."
                .to_string(),
        )
    }

    /// The `503` for a commit cancelled by DynamoDB's own concurrency control
    /// (`TransactionConflict`) rather than by any of the three preconditions moving — most
    /// often another push racing this one for the shared office item. Unlike `moved`/
    /// `office_moved`/`anchor_moved`, this does not claim any value changed, so it must never
    /// be worded as one of those: a retry may succeed with nothing having moved at all.
    ///
    /// **The message must not promise automation this crate does not have** (PR #116 review,
    /// finding 4). No client-side retry loop or classifier recognizes this status yet — the
    /// client's `update_ref` (`forklift-core`'s `remote_utils.rs`) has no retry path at all to
    /// extend, unlike `commit_lift`'s blob-not-ready case, which already owns one
    /// (`is_transient_commit_failure` plus `commit_one_batch`'s bounded backoff loop); building
    /// an equivalent for ref updates is new retry machinery this design gives to FORK-95 slice
    /// 3 (arm two, claim C14), not this one. So the wording below says "run the push again",
    /// never "retry" bare — an instruction a human (or a script) can act on without implying a
    /// client-side mechanism that does not exist.
    fn transient_contention(&self) -> HeadError {
        HeadError::service_unavailable(
            "The commit could not complete: another push is concurrently touching a shared \
            item on this warehouse, likely its office pallet. No input is known to have \
            moved. Run the push again once the other push has settled — this client does not \
            retry ref updates automatically."
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
