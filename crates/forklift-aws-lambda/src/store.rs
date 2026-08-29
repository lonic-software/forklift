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
//! AWS it is DynamoDB, and its one non-idempotent operation,
//! [`RefStore::compare_and_set_head`], is a conditional write: exactly the CAS that lets the
//! serverless head scale horizontally where the server head needs an in-process mutex
//! (§4.5, §4.6). It conditions on all three of a ref update's movable inputs at once — the
//! pallet's own head, the office head, and the trust anchor — as one DynamoDB transaction,
//! so nothing this store cannot see move can make a commit outlive the audit it followed
//! (FORK-95 design memo, "The invariant" / claim C6). Unlike the filesystem, object storage
//! has no directory walk, so ref enumeration is an explicit primitive
//! ([`RefStore::list_refs`]).
//!
//! Every method returns `Result<_, String>`: storage failures are opaque strings that the
//! [`Head`](crate::Head) turns into a `500`. The verification and CAS *semantics* live in
//! the head, not the stores — a store only persists and reports.
//!
//! Both traits are **synchronous, deliberately**, even though S3
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

/// The outcome of a commit that conditions on every movable input a ref-update audit
/// consumed — the pallet's own head, the office head, and the trust anchor (FORK-95 design
/// memo, "The movable inputs: exactly three" / claim C6). A caller needs to know *which*
/// precondition failed, not just that one did: a moved office head means the pallet itself
/// never budged but something it was audited against did, and the client-facing refusal
/// differs accordingly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CasOutcome {
    /// Every precondition held; the head moved from `expected` to the new value.
    Committed,
    /// The pallet's own head was not `expected`; nothing moved. Carries the actual current
    /// head so the client can report the divergence.
    Conflict { current: Option<String> },
    /// The office head the audit consumed has since moved (or the office pallet was born or
    /// retired underneath it). Carries the office pallet's actual current head.
    OfficeMoved { current: Option<String> },
    /// The trust anchor the audit consumed has since moved: established, replaced by a
    /// re-genesis, or (unreachable today, since nothing deletes it) removed. Nothing at this
    /// call site needs the anchor's new bytes — the caller re-reads and re-decides — so
    /// unlike its two siblings this variant carries no payload.
    AnchorMoved,
    /// The commit was refused for a reason that establishes nothing about whether any of the
    /// three inputs actually moved — on DynamoDB, `TransactionConflict` (another transaction
    /// concurrently touching one of this commit's items, the shared office item principally).
    /// A retry may succeed with nothing having changed, unlike the three variants above, each
    /// of which asserts a specific value differs from what was consumed. `MemoryRefStore`
    /// never produces this: its single mutex makes every commit either succeed or lose to a
    /// real, attributable mismatch, never a transaction-level conflict with no value to blame.
    Transient,
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

    /// Which of `hashes` the store lacks — the bulk presence probe the commit-gate closure audit
    /// hands a chunked file's whole chunk list to (`audit_utils`'s recipe descent). Returns the
    /// absent subset in an unspecified order (a parallel backend finishes its probes out of order).
    ///
    /// The default answers it serially with [`exists`](ObjectStore::exists), which is right for a
    /// store whose presence check is cheap — the in-memory fake, a filesystem mirror. The S3 backend
    /// overrides it with a bounded-concurrency batch of `HEAD`s: there a probe is a network round
    /// trip and a changed multi-gigabyte file lists thousands of chunks, so a serial walk of them
    /// would blow API Gateway's hard 29-second integration timeout while a parallel one clears in a
    /// second or two.
    fn objects_missing(&self, hashes: &[String]) -> Result<Vec<String>, String> {
        let mut missing = Vec::new();

        for hash in hashes {
            if !self.exists(hash)? {
                missing.push(hash.clone());
            }
        }

        Ok(missing)
    }

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

    /// Atomically move a pallet head from `expected` to `new`, conditioned on all three of
    /// the invariant's movable inputs still holding what a ref-update audit consumed (a
    /// DynamoDB `TransactWriteItems`; FORK-95 design memo, claim C13):
    ///
    /// * `expected` — the pallet's own head. `None` means "the pallet must not exist yet".
    /// * `office_head` — the office pallet's head the audit read, precisely when the audit
    ///   read one at all — `None` means "the audit never consumed the office head", not
    ///   "the office pallet is unborn": an untrusted warehouse's audit never reads it (every
    ///   office-touching step is gated on trust being established), so conditioning on
    ///   "unborn" there would refuse a push over an office state the audit never depended on,
    ///   possibly *every* push, since an untrusted office pallet may well carry a real,
    ///   unaudited head. The caller must pass `None` here whenever its own audit did not read
    ///   the office head, never merely because the office pallet happens to be unborn. Also
    ///   skipped (regardless of value) when `namespace`/`name` name the office pallet itself —
    ///   the `expected` condition above already pins that exact item.
    /// * `anchor` — the trust anchor's *stored serialization*, exactly as
    ///   [`get_trust`](RefStore::get_trust) returned it, or `None` if untrusted. Never a
    ///   re-serialization of the decoded value: a `serde` field reorder must not fail every
    ///   push's anchor check for no anchor that actually moved.
    ///
    /// A mismatch on any of the three commits nothing and reports *which* input moved via
    /// [`CasOutcome`] — the caller cannot tell a moved pallet head from a moved office head
    /// or a moved anchor without that distinction, and the client-facing refusal differs
    /// per case.
    fn compare_and_set_head(
        &self,
        namespace: PalletNamespace,
        name: &str,
        expected: Option<&str>,
        new: &str,
        office_head: Option<&str>,
        anchor: Option<&str>,
    ) -> Result<CasOutcome, String>;

    /// Every pallet with something stacked, across both namespaces. Explicit because
    /// object storage has no directory walk (unlike `pallet_utils::all_pallet_refs`).
    fn list_refs(&self) -> Result<Vec<(PalletRef, String)>, String>;

    /// The pallet a franchise checks out when the user does not choose (git's `HEAD`).
    fn default_pallet(&self) -> Result<String, String>;

    /// The warehouse's trust anchor, decoded — alongside the exact bytes it is stored as, or
    /// `None` before trust is established. The decoded value is what an audit consumes; the
    /// bytes are what [`compare_and_set_head`](RefStore::compare_and_set_head)'s anchor
    /// precondition later compares by string equality, so a caller that needs to condition a
    /// commit on "the anchor I just read" must carry the bytes forward rather than re-encode
    /// the decoded value (FORK-95 design memo, claim C13).
    fn get_trust(&self) -> Result<Option<(TrustAnchor, String)>, String>;

    /// Plant the trust anchor iff none exists yet (the one-way door). Idempotent for an
    /// identical anchor; a conflicting one is refused. Atomic, like the head CAS.
    fn put_trust_if_absent(&self, anchor: &TrustAnchor) -> Result<TrustOutcome, String>;

    /// Replace the trust anchor — the one sanctioned overwrite, a re-genesis (§8.7). The
    /// head validates the chain-of-custody before calling this.
    fn replace_trust(&self, anchor: &TrustAnchor) -> Result<(), String>;
}
