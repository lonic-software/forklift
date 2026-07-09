//! The two narrow traits the head's protocol logic runs over.
//!
//! [`ObjectStore`] is the byte plane (canonical objects and signature sidecars); on AWS
//! it is S3, and object bytes reach it directly from the client via presigned URLs. Its
//! reads and existence checks funnel the same access `forklift_core::util::file_utils`
//! makes on a local warehouse (`retrieve_object_by_hash`, `does_object_exist`,
//! `store_object_bytes`); its `access`/`put_target` hooks decide whether the head serves
//! bytes itself (the fake, the self-host equivalent) or hands out a `307` to a storage
//! URL (the S3-backed deployment).
//!
//! When bytes bypass the head they land in a **staging prefix** keyed by the lift session,
//! not at the object's hash key. Only [`ObjectStore::verify_and_promote`] — which checks
//! `Blake3(bytes) == hash` — moves them to the canonical key, and that is the moment they
//! become fetchable. So invariant 1 ("nothing unverified is ever fetchable") holds for the
//! presigned path structurally, not by a later audit: there is no window in which a client
//! can write arbitrary bytes to a hash key. The staging round trip is the whole reason
//! `put_target` needs a session.
//!
//! [`RefStore`] is the single consistency point — pallet heads and the trust anchor. On
//! AWS it is DynamoDB, and its one non-idempotent operation, [`RefStore::compare_and_set_head`],
//! is a conditional write: exactly the CAS that lets the serverless head scale
//! horizontally where the server head needs an in-process mutex (§4.5, §4.6). Unlike the
//! filesystem, object storage has no directory walk, so ref enumeration is an explicit
//! primitive ([`RefStore::list_refs`]).
//!
//! Every method returns `Result<_, String>`: storage failures are opaque strings that the
//! [`Head`](crate::Head) turns into a `500`. The verification and CAS *semantics* live in
//! the head, not the stores — a store only persists and reports.
//!
//! Both traits are **synchronous, deliberately** (R4, decided 2026-07-09), even though S3
//! and DynamoDB are async: the audit these stores feed *is* `forklift_core`, which is
//! synchronous to its roots and scoped by a thread-local that must never cross an `.await`.
//! The async boundary therefore lives in the runtime adapter, which runs the whole `Head`
//! call on a blocking thread; an SDK-backed store bridges each future with
//! [`AsyncBridge`](crate::blocking::AsyncBridge). See `blocking.rs` for the full argument.

use forklift_core::util::office_utils::TrustAnchor;
use forklift_core::util::pallet_utils::{PalletNamespace, PalletRef};

/// The outcome of storing an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    /// The object was newly stored.
    Created,
    /// The object was already present (immutable, so equal hash means equal content).
    AlreadyPresent,
}

/// The outcome of storing a signature sidecar (immutable, like an object, but a
/// *conflicting* sidecar for an already-signed parcel is refused rather than deduplicated).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureOutcome {
    /// The sidecar was newly stored.
    Created,
    /// The identical sidecar was already present.
    AlreadyPresent,
    /// A *different* sidecar already exists — signatures are immutable (`409`).
    Conflict,
}

/// How the head answers a request for an object's bytes.
pub enum ObjectAccess {
    /// The store served the bytes; the head returns them directly (self-host / fake).
    Direct(Vec<u8>),
    /// Follow this URL (a presigned S3 GET) for the bytes — the head answers `307`.
    Redirect(String),
}

/// Where an object upload should go.
pub enum PutTarget {
    /// The head accepts and verifies the bytes itself (self-host / fake).
    Direct,
    /// Upload the bytes to this URL (a presigned S3 PUT); the head answers `307`. The URL
    /// addresses a **staging key**, never the object's hash key: bytes written there are
    /// invisible to [`ObjectStore::exists`]/[`get`](ObjectStore::get) and become fetchable
    /// only once [`ObjectStore::verify_and_promote`] has checked `Blake3(bytes) == hash`
    /// and copied them to the canonical key. That ordering is what upholds invariant 1
    /// against a client that uploads straight to storage (DESIGN.html §4.2 / §4.6).
    Staged(String),
    /// The store stages uploads, but the request named no lift session to stage under —
    /// the head answers `422`. A session-less upload has nowhere to be promoted from.
    SessionRequired,
}

/// The outcome of verifying a staged object and promoting it to its canonical hash key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromoteOutcome {
    /// The staged bytes hashed to `hash` and now live at the canonical key: fetchable.
    Promoted,
    /// The canonical object was already present, so the staged copy (if any) was dropped.
    /// Promotion is idempotent — a retried commit lands here.
    AlreadyPresent,
    /// Nothing is staged under `(session, hash)` and no canonical object exists: the
    /// client never uploaded it.
    Missing,
    /// The staged bytes do **not** hash to `hash`. They are discarded, never promoted;
    /// nothing unverified becomes fetchable. Carries the hash the bytes actually have.
    Corrupt {
        /// What the staged bytes actually hash to.
        actual: String,
    },
}

/// The outcome of a compare-and-set on a pallet head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CasOutcome {
    /// The head moved from `expected` to the new value.
    Committed,
    /// The head was not `expected`; nothing moved. Carries the actual current head so
    /// the client can report the divergence.
    Conflict { current: Option<String> },
}

/// The outcome of establishing the trust anchor (a one-way door, §4.4 / §8.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustOutcome {
    /// No anchor existed; this one was planted.
    Established,
    /// The identical anchor was already present (idempotent).
    AlreadyIdentical,
    /// A *different* anchor already exists; trust cannot be replaced silently (`409`).
    /// The one sanctioned replacement, a re-genesis, goes through
    /// [`RefStore::replace_trust`].
    Conflict,
}

/// The byte plane: canonical objects and signature sidecars, addressed by hash.
///
/// Object bytes are the *uncompressed* wire form — the verifiable form the hash covers
/// (`REMOTE_PROTOCOL.md` invariant 3). What the backend does at rest (the fake keeps
/// bytes as-is; a filesystem mirror re-compresses via `store_object_bytes`; S3 stores
/// what it is given) is its own business.
pub trait ObjectStore {
    /// Whether an object is present at its hash. On S3 this is a `HEAD`; it is the seam
    /// the closure walk uses for the (large, many) working blobs it never reads.
    fn exists(&self, hash: &str) -> Result<bool, String>;

    /// The uncompressed object bytes, or `None` if absent.
    fn get(&self, hash: &str) -> Result<Option<Vec<u8>>, String>;

    /// Verify `Blake3(bytes) == hash` and store the object; nothing unverified may ever
    /// become fetchable (invariant 1). Storing an already-present hash is a no-op.
    fn put_verified(&self, hash: &str, bytes: &[u8]) -> Result<PutOutcome, String>;

    /// A parcel's signature sidecar bytes, or `None` for an unsigned parcel.
    fn get_signature(&self, parcel_hash: &str) -> Result<Option<Vec<u8>>, String>;

    /// Store a signature sidecar. The bytes are assumed already structurally validated by
    /// the caller (the head, via `sign_utils::validate_raw_parcel_signature`). Immutable:
    /// identical re-store is a no-op, a conflicting one is refused.
    fn put_signature(&self, parcel_hash: &str, bytes: &[u8]) -> Result<SignatureOutcome, String>;

    /// How to answer a byte read for `hash`. The default serves the bytes directly (the
    /// self-host / in-memory behaviour); an S3-backed store overrides this to answer with
    /// a presigned-GET [`ObjectAccess::Redirect`].
    fn access(&self, hash: &str) -> Result<Option<ObjectAccess>, String> {
        Ok(self.get(hash)?.map(ObjectAccess::Direct))
    }

    /// Where an upload of `hash` belonging to lift `session` should go. The default
    /// accepts the bytes directly and ignores the session (the head verifies them inline,
    /// so it needs no staging area); an S3-backed store overrides this to hand out a
    /// presigned PUT to a [`PutTarget::Staged`] key under the session.
    fn put_target(&self, _session: Option<&str>, _hash: &str) -> Result<PutTarget, String> {
        Ok(PutTarget::Direct)
    }

    /// Verify a staged upload and, only if `Blake3(bytes) == hash`, promote it to its
    /// canonical hash key — the single moment an uploaded object becomes fetchable
    /// (invariant 1). Corrupt staged bytes are discarded, never promoted.
    ///
    /// Both callers of the AWS deployment funnel through here: the control plane runs it
    /// synchronously for the small objects at `POST /lift/{session}/commit`, and the
    /// staging verifier (an S3-event Lambda) runs it asynchronously for large blobs. It is
    /// idempotent, so the two racing on one hash is safe.
    ///
    /// The default is the direct store's: nothing is ever staged, and whatever is present
    /// was already hash-verified by [`put_verified`](ObjectStore::put_verified).
    fn verify_and_promote(&self, _session: &str, hash: &str) -> Result<PromoteOutcome, String> {
        if self.exists(hash)? {
            Ok(PromoteOutcome::AlreadyPresent)
        } else {
            Ok(PromoteOutcome::Missing)
        }
    }

    /// Drop everything still staged under `session` — the sweep after a committed lift
    /// (whose objects have been promoted) or an abandoned one. Never touches canonical
    /// objects. The default store stages nothing, so this is a no-op.
    fn discard_session(&self, _session: &str) -> Result<(), String> {
        Ok(())
    }

    /// Park a large *response* body (a `batch` bundle) in storage and return a presigned
    /// `GET` for it, so the bytes never travel through the control plane — which on Lambda
    /// cannot return more than a few megabytes anyway. `None` means "serve it inline"
    /// (the self-host / fake behaviour).
    ///
    /// The bytes land under an ephemeral, content-addressed prefix that is *not* the
    /// canonical object namespace, so nothing here is reachable as an object at a hash key
    /// and invariant 1 is not in play. Clients verify bundle records on import regardless.
    fn offload_response(&self, _bytes: &[u8]) -> Result<Option<String>, String> {
        Ok(None)
    }
}

/// The consistency point: pallet heads and the trust anchor.
pub trait RefStore {
    /// The current head of a pallet, or `None` if it is unborn.
    fn get_head(&self, namespace: PalletNamespace, name: &str) -> Result<Option<String>, String>;

    /// Atomically move a pallet head from `expected` to `new` (a DynamoDB conditional
    /// write). `expected: None` means "the pallet must not exist yet". A mismatch commits
    /// nothing and reports the actual head — the CAS that catches concurrent lifts.
    fn compare_and_set_head(
        &self,
        namespace: PalletNamespace,
        name: &str,
        expected: Option<&str>,
        new: &str,
    ) -> Result<CasOutcome, String>;

    /// Every pallet with something stacked, across both namespaces. Explicit because
    /// object storage has no directory walk (unlike `pallet_utils::all_pallet_refs`).
    fn list_refs(&self) -> Result<Vec<(PalletRef, String)>, String>;

    /// The pallet a franchise checks out when the user does not choose (git's `HEAD`).
    fn default_pallet(&self) -> Result<String, String>;

    /// The warehouse's trust anchor, or `None` before trust is established.
    fn get_trust(&self) -> Result<Option<TrustAnchor>, String>;

    /// Plant the trust anchor iff none exists yet (the one-way door). Idempotent for an
    /// identical anchor; a conflicting one is refused. Atomic, like the head CAS.
    fn put_trust_if_absent(&self, anchor: &TrustAnchor) -> Result<TrustOutcome, String>;

    /// Replace the trust anchor — the one sanctioned overwrite, a re-genesis (§8.7). The
    /// head validates the chain-of-custody before calling this.
    fn replace_trust(&self, anchor: &TrustAnchor) -> Result<(), String>;
}
