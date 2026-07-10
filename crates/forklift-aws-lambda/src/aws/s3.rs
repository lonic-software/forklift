//! [`S3ObjectStore`]: the byte plane on `aws-sdk-s3`.
//!
//! # Key layout
//!
//! One bucket, four prefixes, and the boundary between them *is* invariant 1:
//!
//! * `objects/{hash}` — the **canonical** namespace. Everything here is hash-verified, and
//!   the only two write paths into it are [`put_verified`](S3ObjectStore::put_verified)
//!   (which hashes the bytes first) and [`verify_and_promote`](S3ObjectStore::verify_and_promote)
//!   (which hashes the staged bytes first). Nothing else can put a key here — in particular,
//!   no presigned `PUT` ever addresses this prefix.
//! * `staging/{session}/{hash}` — where a client's presigned `PUT` lands. Bytes here are
//!   invisible to [`exists`](S3ObjectStore::exists)/[`get`](S3ObjectStore::get); they become
//!   fetchable only when `verify_and_promote` copies them to `objects/{hash}` after checking
//!   the hash. This is the whole reason a `PUT` target needs a lift session.
//! * `signatures/{parcel_hash}` — signature sidecars, immutable like objects. A distinct
//!   prefix, never `objects/`, because a parcel's sidecar shares the parcel's hash and must
//!   not collide with the parcel object.
//! * `responses/{content_hash}` — offloaded `batch` bundles, ephemeral and content-addressed,
//!   deliberately outside `objects/` so nothing here is reachable as an object (invariant 1
//!   is not in play; the client verifies every record on import regardless).
//!
//! # The conditional-write CAS
//!
//! S3 has no lock, so the atomicity the fake gets from its `objects` mutex comes here from an
//! `If-None-Match: *` conditional `PUT`: the write succeeds only if the key does not yet
//! exist, else S3 answers `412`. That single primitive gives `put_verified` its
//! `Created`/`AlreadyPresent` split and lets two promoters race onto one canonical key with
//! exactly one winner — the atomicity the fake's module docs say "an S3 + DynamoDB backend
//! owes."
//!
//! # Bounded memory on the promote path (§ review, C1)
//!
//! [`verify_and_promote`](S3ObjectStore::verify_and_promote) hash-verifies a **staged**
//! object a client uploaded straight to storage — bytes this process has never seen. An
//! authenticated lifter can presign-PUT an object of any size to staging and name it in
//! `commit_lift`; a naive "download the whole thing, then hash it" implementation buffers an
//! attacker-controlled number of bytes in Lambda RAM before it ever gets to decide anything,
//! which is an OOM DoS with no signature or trust required. Two mechanisms close it:
//!
//! * **Below [`STREAMING_THRESHOLD_BYTES`]** (small control-plane objects — parcels, trees,
//!   signatures — which are the common case `commit_lift` handles synchronously): the simple
//!   buffer-then-hash-then-`put_if_absent` path, bounded by the threshold itself, so there is
//!   no unbounded allocation to begin with.
//! * **At or above it**: the object is never buffered. [`stream_hash_capped`] reads it in
//!   chunks through an incremental Blake3 hasher — memory use is bounded by one chunk, not by
//!   the object's size — aborting the moment the running total passes [`MAX_STAGED_OBJECT_BYTES`],
//!   the hard ceiling regardless of path. Once the hash is known good, promotion moves the
//!   bytes with a server-side `CopyObject` rather than re-uploading them from a buffer this
//!   process would otherwise have had to hold.
//!
//! Streaming introduces its own hazard: the presigned staging URL is still valid for the rest
//! of its TTL, so a client can `PUT` **different** bytes to the same staged key between the
//! `GET` this store just streamed and the `CopyObject` that promotes it (a TOCTOU). The copy
//! is pinned against exactly that hazard with `copy_source_if_match(etag)`, where `etag` is
//! captured from the same `GetObject` response the hash was streamed from — the copy then
//! either transfers the *exact* bytes just hashed, or fails, and `verify_and_promote` loops
//! (bounded by [`MAX_PROMOTE_ATTEMPTS`]) to re-hash whatever is there now. See
//! [`S3ObjectStore::copy_staged_to_canonical`] for how the destination side of that copy also
//! gets a conditional write, and what happens when a backend does not honor it.
//!
//! # Presigned URLs
//!
//! Reads and staged writes are handed to the client as presigned URLs (a `307` from the
//! head), so object bytes never traverse the control plane. Every presigned URL is valid for
//! [`PRESIGN_TTL`] — short, constant, and the same for reads and writes. A presigned `PUT` is
//! constructible by exactly one private helper, [`S3ObjectStore::presign_staging_put`], which
//! hardcodes the `staging/` prefix; there is no code path that presigns a `PUT` to a hash key.
//!
//! A presigned `PUT` (SigV4 query-string auth) cannot itself carry a size **range**
//! condition — that is a presigned-**POST** policy feature (`content-length-range`), which
//! this SDK does not expose a builder for, and which is orthogonal to the query-string
//! signing every other endpoint here uses. So [`MAX_STAGED_OBJECT_BYTES`] is not, and cannot
//! be, enforced by the URL; it is enforced where it actually can be — the streaming read
//! above — which is a stronger guarantee anyway: it bounds every staged object regardless of
//! how or whether the client's `Content-Length` header matched anything.

use std::time::Duration;

use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;

use forklift_core::util::object_utils;

use crate::aws::sdk::{describe, is_head_object_not_found, is_no_such_key, is_precondition_failed};
use crate::blocking::AsyncBridge;
use crate::store::{ObjectAccess, ObjectStore, PromoteOutcome, PutOutcome, PutTarget, SignatureOutcome};

/// How long a presigned URL is valid. Short and constant, the same for a read and a staged
/// write: long enough for a client to upload or download one object over a slow link, short
/// enough that a leaked URL is a small window. 15 minutes matches the SDK's own default and
/// the hosted deployment's assumption of a live, actively-lifting client.
pub const PRESIGN_TTL: Duration = Duration::from_secs(15 * 60);

/// The boundary between the two promotion strategies in [`ObjectStore::verify_and_promote`]:
/// below this, buffer the whole staged object and hash it in one shot (bounded, so safe, and
/// simpler); at or above it, never buffer — stream-hash and promote via `CopyObject`. 8 MiB
/// comfortably covers real control-plane objects (parcels, trees, signature sidecars are all
/// small by construction) while being far too small for even many concurrent promotions to
/// meaningfully pressure Lambda's memory.
///
/// `pub` (and re-exported from `aws::`) so the LocalStack integration suite can size a staged
/// object relative to the real boundary instead of duplicating the number.
pub const STREAMING_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024;

/// The hard ceiling on a staged object's size, enforced while streaming (see the module
/// docs). 5 GiB is S3's own single-`PUT` maximum — generous enough that no legitimate blob
/// ever hits it, since anything larger would not have fit through a single presigned `PUT` in
/// the first place.
const MAX_STAGED_OBJECT_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// How many times [`ObjectStore::verify_and_promote`] retries the streaming path when a
/// client's re-`PUT` to the staging URL invalidates the `CopyObject` source pin mid-promotion.
/// Each attempt is a full re-`GET`+re-hash, so this bounds the cost a churning client can
/// impose to a small constant rather than an unbounded retry loop.
const MAX_PROMOTE_ATTEMPTS: u32 = 3;

/// The canonical key of an object — the only prefix `exists`/`get`/`access` read, and the
/// only prefix hash-verified writes target.
fn object_key(hash: &str) -> String {
    format!("objects/{}", hash)
}

/// The staging key an upload for `hash` lands at under lift `session`. Never `objects/{hash}`:
/// bytes here are unverified and unfetchable until promotion (invariant 1).
fn staging_key(session: &str, hash: &str) -> String {
    format!("staging/{}/{}", session, hash)
}

/// The prefix sweeping a whole session's staged uploads.
fn staging_prefix(session: &str) -> String {
    format!("staging/{}/", session)
}

/// A parcel's signature-sidecar key. A distinct prefix so it never collides with the parcel
/// object at `objects/{parcel_hash}`.
fn signature_key(parcel_hash: &str) -> String {
    format!("signatures/{}", parcel_hash)
}

/// The ephemeral, content-addressed key of an offloaded response body. Outside `objects/`, so
/// nothing here is ever an object at a hash key.
fn response_key(content_hash: &str) -> String {
    format!("responses/{}", content_hash)
}

/// Percent-encode a key for the key portion of a `CopySource` value (`bucket/key`). AWS
/// requires the source path URL-encoded; unlike a request path, the `/` segment separators
/// must stay literal, or the source would resolve to the wrong object entirely.
fn percent_encode_copy_source_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());

    for byte in key.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }

    out
}

/// The S3-backed [`ObjectStore`]: the byte plane of an AWS serverless head.
///
/// Every method is synchronous (the settled seam, R4) and drives the async SDK through the
/// [`AsyncBridge`]. It must be built inside a multi-thread runtime and its methods called
/// from a blocking thread, exactly as the `Head` contract requires.
pub struct S3ObjectStore {
    client: aws_sdk_s3::Client,
    bucket: String,
    bridge: AsyncBridge,
}

/// The outcome of a conditional (`If-None-Match: *`) write: it landed, or the key already
/// existed. Every other failure is a genuine error.
enum Conditional {
    /// The write succeeded — the key was free and now holds these bytes.
    Written,
    /// The key already existed; nothing was written (`412`).
    AlreadyExists,
}

/// The outcome of hashing a staged object's bytes without buffering them (see
/// [`stream_hash_capped`]).
enum StreamHash {
    /// The object's Blake3 hash, computed incrementally.
    Hash(String),
    /// The object exceeded [`MAX_STAGED_OBJECT_BYTES`] before it finished streaming; hashing
    /// was abandoned rather than let the transfer keep going indefinitely.
    TooLarge,
}

/// The outcome of the ETag-pinned server-side copy in [`S3ObjectStore::copy_staged_to_canonical`].
enum CopyOutcome {
    /// The copy landed at the canonical key.
    Copied,
    /// The destination already held this hash — a racing promoter won (see the module docs'
    /// note on the destination condition).
    AlreadyPresent,
    /// The *source* changed since it was hashed (the `copy_source_if_match` pin failed): a
    /// client re-`PUT` the staging key while this promotion was in flight. The caller should
    /// re-fetch and re-hash.
    SourceChanged,
}

/// Stream-hash a staged object's body without ever buffering it whole: read it in chunks,
/// feed each into an incremental Blake3 hasher, and abort once `cap` is exceeded. Blake3's
/// incremental hasher (`update` called any number of times) produces exactly the hash a
/// one-shot `blake3::hash` over the same bytes would — the same identity
/// `object_utils::hash_object_bytes` checks — so this is not a different verification, only a
/// bounded-memory way to run it.
///
/// A free function (not a method) so a unit test can drive it with a small `cap` and a small
/// `ByteStream`, without any AWS credentials or a multi-gigabyte payload.
async fn stream_hash_capped(mut body: ByteStream, cap: u64) -> Result<StreamHash, String> {
    let mut hasher = blake3::Hasher::new();
    let mut total: u64 = 0;

    while let Some(chunk) =
        body.try_next().await.map_err(|err| format!("reading the staged object failed: {}", err))?
    {
        total += chunk.len() as u64;

        if total > cap {
            return Ok(StreamHash::TooLarge);
        }

        hasher.update(&chunk);
    }

    Ok(StreamHash::Hash(hasher.finalize().to_hex().to_string()))
}

impl S3ObjectStore {
    /// Build the store over an S3 `client` addressing `bucket`, driving its async calls
    /// through `bridge`. Capture the bridge on the runtime thread (see `aws::config`).
    pub fn new(client: aws_sdk_s3::Client, bucket: String, bridge: AsyncBridge) -> S3ObjectStore {
        S3ObjectStore { client, bucket, bridge }
    }

    /// Whether a key exists (an S3 `HEAD`). A `404` is `Ok(false)`, not an error. See
    /// [`is_head_object_not_found`]'s docs for the residual bucket-vs-key ambiguity this call
    /// cannot resolve — a limitation of `HeadObject` itself, not of this mapping.
    async fn key_exists(&self, key: &str) -> Result<bool, String> {
        match self.client.head_object().bucket(&self.bucket).key(key).send().await {
            Ok(_) => Ok(true),
            Err(err) if is_head_object_not_found(&err) => Ok(false),
            Err(err) => Err(describe("S3 head_object", err)),
        }
    }

    /// The bytes at a key, or `None` when it is absent (`NoSuchKey` is `Ok(None)`; any other
    /// failure — including `NoSuchBucket` — is a genuine error, per [`is_no_such_key`]).
    async fn key_bytes(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        match self.client.get_object().bucket(&self.bucket).key(key).send().await {
            Ok(output) => {
                let bytes = output
                    .body
                    .collect()
                    .await
                    .map_err(|err| format!("S3 read of {} failed: {}", key, err))?;

                Ok(Some(bytes.to_vec()))
            }
            Err(err) if is_no_such_key(&err) => Ok(None),
            Err(err) => Err(describe("S3 get_object", err)),
        }
    }

    /// A conditional `PUT`: write `bytes` at `key` only if the key does not already exist.
    /// The `If-None-Match: *` header is the byte plane's CAS — it is how immutability and
    /// racing-promoter serialization are enforced without a lock.
    async fn put_if_absent(&self, key: &str, bytes: Vec<u8>) -> Result<Conditional, String> {
        match self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .if_none_match("*")
            .body(ByteStream::from(bytes))
            .send()
            .await
        {
            Ok(_) => Ok(Conditional::Written),
            Err(err) if is_precondition_failed(&err) => Ok(Conditional::AlreadyExists),
            Err(err) => Err(describe("S3 put_object", err)),
        }
    }

    /// An unconditional `PUT` — for the ephemeral response prefix, where a repeated offload of
    /// the same content-addressed bundle harmlessly overwrites itself.
    async fn put_overwrite(&self, key: &str, bytes: Vec<u8>) -> Result<(), String> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(bytes))
            .send()
            .await
            .map(|_| ())
            .map_err(|err| describe("S3 put_object", err))
    }

    /// Best-effort delete of a staged key. A failure here never fails the caller: a leftover
    /// staged object is unfetchable (it is not at a hash key) and is swept by
    /// [`discard_session`](S3ObjectStore::discard_session) later.
    async fn drop_staged(&self, key: &str) {
        let _ = self.client.delete_object().bucket(&self.bucket).key(key).send().await;
    }

    /// Presign a `GET` for `key`, valid for [`PRESIGN_TTL`].
    async fn presign_get(&self, key: &str) -> Result<String, String> {
        let config = PresigningConfig::expires_in(PRESIGN_TTL)
            .map_err(|err| format!("building the presigning config failed: {}", err))?;

        let request = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(config)
            .await
            .map_err(|err| describe("S3 presign get_object", err))?;

        Ok(request.uri().to_string())
    }

    /// Presign a `PUT` into the session's **staging** prefix, valid for [`PRESIGN_TTL`]. This
    /// is the *only* presigned-`PUT` path in the store, and it hardcodes `staging/{session}/`:
    /// there is no way to hand a client a presigned `PUT` to a canonical hash key. That is the
    /// structural half of invariant 1.
    ///
    /// Does **not** carry a size condition — see the module docs on why a presigned `PUT`
    /// cannot express one, and where [`MAX_STAGED_OBJECT_BYTES`] is actually enforced instead.
    async fn presign_staging_put(&self, session: &str, hash: &str) -> Result<String, String> {
        let config = PresigningConfig::expires_in(PRESIGN_TTL)
            .map_err(|err| format!("building the presigning config failed: {}", err))?;

        let request = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(staging_key(session, hash))
            .presigned(config)
            .await
            .map_err(|err| describe("S3 presign put_object", err))?;

        Ok(request.uri().to_string())
    }

    /// Promote a staged object already known to hash-verify, by copying it server-side rather
    /// than re-uploading bytes this process would otherwise have had to buffer.
    ///
    /// Two independent conditions guard the copy:
    ///
    /// * `copy_source_if_match(etag)` (when `source_etag` is known) pins the **source**: the
    ///   copy transfers exactly the bytes this store already streamed and hashed, or fails —
    ///   closing the TOCTOU where a client re-`PUT`s different bytes to the still-valid
    ///   staging URL between the hash pass and this call.
    /// * `if_none_match("*")` pins the **destination**: the copy lands only if the canonical
    ///   key does not yet exist, the same CAS [`put_if_absent`](S3ObjectStore::put_if_absent)
    ///   gets from a direct conditional `PUT`. AWS S3 has supported a conditional `CopyObject`
    ///   destination since 2024; a `412` here (with the source pin intact) means another
    ///   promoter already won.
    ///
    /// A `412` is ambiguous between those two conditions — S3 reports the same status for
    /// either — so it is resolved with one more `HEAD`: if the canonical key exists now, the
    /// destination condition is what failed ([`CopyOutcome::AlreadyPresent`]); otherwise the
    /// source changed ([`CopyOutcome::SourceChanged`], and the caller re-fetches and retries).
    ///
    /// If a backend does not honor the destination condition (older S3-compatible services —
    /// real AWS S3 does), the worst case is a **benign race**: two promoters' copies can both
    /// "succeed", and both report [`CopyOutcome::Copied`]. That is still correct, not merely
    /// harmless: both sources were independently `copy_source_if_match`-pinned to bytes *this
    /// process itself* verified hash to the target hash, so whichever copy lands last writes
    /// exactly the same bytes as the one before it. The only thing lost is precision in the
    /// loser's report (`Promoted` instead of `AlreadyPresent`) — an observability nicety, not
    /// a safety property. This is a deliberate departure from the small-object path's exact
    /// single-winner semantics (which real AWS still gets, via the destination condition).
    async fn copy_staged_to_canonical(
        &self,
        staged_key: &str,
        canonical_key: &str,
        source_etag: Option<&str>,
    ) -> Result<CopyOutcome, String> {
        let mut request = self
            .client
            .copy_object()
            .bucket(&self.bucket)
            .copy_source(format!(
                "{}/{}",
                self.bucket,
                percent_encode_copy_source_key(staged_key)
            ))
            .key(canonical_key)
            .if_none_match("*");

        if let Some(etag) = source_etag {
            request = request.copy_source_if_match(etag);
        }

        match request.send().await {
            Ok(_) => Ok(CopyOutcome::Copied),
            Err(err) if is_precondition_failed(&err) => {
                if self.key_exists(canonical_key).await? {
                    Ok(CopyOutcome::AlreadyPresent)
                } else {
                    Ok(CopyOutcome::SourceChanged)
                }
            }
            Err(err) => Err(describe("S3 copy_object", err)),
        }
    }
}

impl ObjectStore for S3ObjectStore {
    fn exists(&self, hash: &str) -> Result<bool, String> {
        self.bridge.block_on(self.key_exists(&object_key(hash)))
    }

    fn get(&self, hash: &str) -> Result<Option<Vec<u8>>, String> {
        self.bridge.block_on(self.key_bytes(&object_key(hash)))
    }

    fn put_verified(&self, hash: &str, bytes: &[u8]) -> Result<PutOutcome, String> {
        // Verify before the network call: nothing unverified is ever handed to the canonical
        // namespace, and the message matches the fake so the head's `422` reads identically.
        let actual = object_utils::hash_object_bytes(bytes);

        if actual != hash {
            return Err(format!(
                "Object content does not match its claimed hash {} (actual: {}); refusing to store it.",
                hash, actual
            ));
        }

        self.bridge.block_on(async {
            match self.put_if_absent(&object_key(hash), bytes.to_vec()).await? {
                Conditional::Written => Ok(PutOutcome::Created),
                Conditional::AlreadyExists => Ok(PutOutcome::AlreadyPresent),
            }
        })
    }

    fn get_signature(&self, parcel_hash: &str) -> Result<Option<Vec<u8>>, String> {
        self.bridge.block_on(self.key_bytes(&signature_key(parcel_hash)))
    }

    fn put_signature(&self, parcel_hash: &str, bytes: &[u8]) -> Result<SignatureOutcome, String> {
        let key = signature_key(parcel_hash);

        self.bridge.block_on(async {
            match self.put_if_absent(&key, bytes.to_vec()).await? {
                Conditional::Written => Ok(SignatureOutcome::Created),
                // A sidecar already exists. Immutable: identical bytes are a no-op, different
                // bytes are refused — so read the incumbent and compare, exactly as the fake.
                Conditional::AlreadyExists => match self.key_bytes(&key).await? {
                    Some(existing) if existing == bytes => Ok(SignatureOutcome::AlreadyPresent),
                    Some(_) => Ok(SignatureOutcome::Conflict),
                    // Raced with a delete that cannot happen (sidecars are never deleted); treat
                    // the vanished incumbent as absent-and-now-different rather than claim it.
                    None => Ok(SignatureOutcome::Conflict),
                },
            }
        })
    }

    fn access(&self, hash: &str) -> Result<Option<ObjectAccess>, String> {
        let key = object_key(hash);

        self.bridge.block_on(async {
            // Mirror the fake: absent is `None` (the head's `404`), present is a presigned GET.
            // The HEAD before the presign keeps a redirect from ever pointing at a missing key.
            if !self.key_exists(&key).await? {
                return Ok(None);
            }

            let url = self.presign_get(&key).await?;

            Ok(Some(ObjectAccess::Redirect(url)))
        })
    }

    fn put_target(&self, session: Option<&str>, hash: &str) -> Result<PutTarget, String> {
        // The S3 head always stages: the bytes go straight to storage and are promoted at
        // commit. A session-less upload has nowhere to be promoted from, so it is refused —
        // never by handing out a presigned PUT to the hash key (the invariant-1 hole).
        match session {
            Some(session) => {
                let url = self.bridge.block_on(self.presign_staging_put(session, hash))?;

                Ok(PutTarget::Staged(url))
            }
            None => Ok(PutTarget::SessionRequired),
        }
    }

    fn verify_and_promote(&self, session: &str, hash: &str) -> Result<PromoteOutcome, String> {
        let canonical = object_key(hash);
        let staged = staging_key(session, hash);

        self.bridge.block_on(async {
            for _ in 0..MAX_PROMOTE_ATTEMPTS {
                // 1. Already canonical? Mirror the fake's first check: the object was
                //    verified once and is immutable, so sweep the now-redundant staged copy
                //    and report it. Also the cheap idempotent-retry path — one HEAD, no read.
                if self.key_exists(&canonical).await? {
                    self.drop_staged(&staged).await;

                    return Ok(PromoteOutcome::AlreadyPresent);
                }

                // 2. Fetch the staged object's metadata and body. The body is *not* consumed
                //    yet — nothing is buffered until we decide which strategy applies.
                let output =
                    match self.client.get_object().bucket(&self.bucket).key(&staged).send().await
                    {
                        Ok(output) => output,
                        Err(err) if is_no_such_key(&err) => {
                            // Absent means either "never uploaded" or "a racing promoter
                            // already promoted and swept it" — recheck canonical so the loser
                            // of a race never spuriously reports `Missing`.
                            if self.key_exists(&canonical).await? {
                                return Ok(PromoteOutcome::AlreadyPresent);
                            }

                            return Ok(PromoteOutcome::Missing);
                        }
                        Err(err) => return Err(describe("S3 get_object (staged)", err)),
                    };

                let etag = output.e_tag().map(str::to_string);
                let declared_len =
                    output.content_length().filter(|&len| len >= 0).map(|len| len as u64);

                if declared_len.is_some_and(|len| len < STREAMING_THRESHOLD_BYTES) {
                    // Small: buffer the whole body (bounded by the threshold, so no unbounded
                    // allocation) and hash it in one shot — no ETag pin is needed because the
                    // exact bytes this process verified are the exact bytes it then writes.
                    let bytes = output
                        .body
                        .collect()
                        .await
                        .map_err(|err| format!("reading the staged object failed: {}", err))?
                        .to_vec();

                    let actual = object_utils::hash_object_bytes(&bytes);

                    if actual != hash {
                        self.drop_staged(&staged).await;

                        return Ok(PromoteOutcome::Corrupt { actual });
                    }

                    return match self.put_if_absent(&canonical, bytes).await? {
                        Conditional::Written => {
                            self.drop_staged(&staged).await;

                            Ok(PromoteOutcome::Promoted)
                        }
                        Conditional::AlreadyExists => {
                            self.drop_staged(&staged).await;

                            Ok(PromoteOutcome::AlreadyPresent)
                        }
                    };
                }

                // Large, or a length S3 did not declare: never buffer. Stream-hash with a hard
                // cap, so memory use is bounded by chunk size — not by whatever a client
                // claims or actually uploaded.
                match stream_hash_capped(output.body, MAX_STAGED_OBJECT_BYTES).await? {
                    StreamHash::TooLarge => {
                        self.drop_staged(&staged).await;

                        return Err(format!(
                            "Staged object {} exceeds the {}-byte cap; refusing to promote it.",
                            hash, MAX_STAGED_OBJECT_BYTES
                        ));
                    }
                    StreamHash::Hash(actual) if actual != hash => {
                        self.drop_staged(&staged).await;

                        return Ok(PromoteOutcome::Corrupt { actual });
                    }
                    StreamHash::Hash(_) => {
                        // Verified without ever buffering the body. Promote via a server-side
                        // copy pinned to exactly the bytes just hashed — see
                        // `copy_staged_to_canonical` for the two conditions and the race it
                        // does and does not resolve.
                        match self
                            .copy_staged_to_canonical(&staged, &canonical, etag.as_deref())
                            .await?
                        {
                            CopyOutcome::Copied => {
                                self.drop_staged(&staged).await;

                                return Ok(PromoteOutcome::Promoted);
                            }
                            CopyOutcome::AlreadyPresent => {
                                self.drop_staged(&staged).await;

                                return Ok(PromoteOutcome::AlreadyPresent);
                            }
                            // The staged object changed since it was hashed (a client re-PUT
                            // during this promotion). Loop: re-fetch, re-hash, re-pin.
                            CopyOutcome::SourceChanged => continue,
                        }
                    }
                }
            }

            Err(format!(
                "Staged object {} under session {} kept changing during promotion; refusing \
                after {} attempts.",
                hash, session, MAX_PROMOTE_ATTEMPTS
            ))
        })
    }

    fn discard_session(&self, session: &str) -> Result<(), String> {
        let prefix = staging_prefix(session);

        self.bridge.block_on(async {
            let mut continuation: Option<String> = None;

            loop {
                let mut request =
                    self.client.list_objects_v2().bucket(&self.bucket).prefix(&prefix);

                if let Some(token) = continuation.take() {
                    request = request.continuation_token(token);
                }

                let page = request
                    .send()
                    .await
                    .map_err(|err| describe("S3 list_objects_v2", err))?;

                for object in page.contents() {
                    if let Some(key) = object.key() {
                        self.client
                            .delete_object()
                            .bucket(&self.bucket)
                            .key(key)
                            .send()
                            .await
                            .map_err(|err| describe("S3 delete_object", err))?;
                    }
                }

                match page.next_continuation_token() {
                    Some(token) => continuation = Some(token.to_string()),
                    None => break,
                }
            }

            Ok(())
        })
    }

    fn offload_response(&self, bytes: &[u8]) -> Result<Option<String>, String> {
        // A content-addressed key in the ephemeral response prefix — never an object. Two
        // identical bundles land on the same key, so the write is an idempotent overwrite.
        let key = response_key(&object_utils::hash_object_bytes(bytes));

        self.bridge.block_on(async {
            self.put_overwrite(&key, bytes.to_vec()).await?;

            let url = self.presign_get(&key).await?;

            Ok(Some(url))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_canonical_and_staging_prefixes_never_collide() {
        let hash = "a".repeat(64);

        assert_eq!(object_key(&hash), format!("objects/{}", hash));
        assert_eq!(staging_key("lift-1", &hash), format!("staging/lift-1/{}", hash));

        // The one property invariant 1 rests on at the key layout: a staged key is never the
        // canonical key, so bytes uploaded to it are not fetchable at the hash.
        assert_ne!(object_key(&hash), staging_key("lift-1", &hash));
        assert!(!staging_key("lift-1", &hash).starts_with("objects/"));
    }

    #[test]
    fn signature_and_response_prefixes_are_outside_the_object_namespace() {
        let hash = "b".repeat(64);

        // A sidecar shares the parcel's hash but must not collide with the parcel object.
        assert_eq!(signature_key(&hash), format!("signatures/{}", hash));
        assert_ne!(signature_key(&hash), object_key(&hash));

        // A response body is never reachable as an object at a hash key.
        assert_eq!(response_key(&hash), format!("responses/{}", hash));
        assert!(!response_key(&hash).starts_with("objects/"));
    }

    #[test]
    fn a_session_prefix_bounds_exactly_its_staged_keys() {
        let prefix = staging_prefix("lift-7");

        assert_eq!(prefix, "staging/lift-7/");
        assert!(staging_key("lift-7", &"c".repeat(64)).starts_with(&prefix));
        // The prefix of one session never captures another's keys.
        assert!(!staging_key("lift-8", &"c".repeat(64)).starts_with(&prefix));
    }

    #[test]
    fn the_presign_ttl_is_short_and_constant() {
        assert_eq!(PRESIGN_TTL, Duration::from_secs(900));
    }

    // Both sides are `const`, so clippy sees a compile-time-constant comparison; that is the
    // point — this pins the documented relationship between the two constants (and the hard
    // cap's documented value) so an edit to either constant that breaks the relationship the
    // module docs describe fails a test, not just a comment.
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn the_streaming_threshold_is_well_under_the_hard_cap() {
        assert!(STREAMING_THRESHOLD_BYTES < MAX_STAGED_OBJECT_BYTES);
        // The hard cap matches S3's own single-PUT maximum (documented in the module docs).
        assert_eq!(MAX_STAGED_OBJECT_BYTES, 5 * 1024 * 1024 * 1024);
    }

    #[test]
    fn copy_source_keys_are_percent_encoded_but_keep_literal_slashes() {
        let key = staging_key("lift 1", &"d".repeat(64)); // a space: needs encoding
        let encoded = percent_encode_copy_source_key(&key);

        assert!(encoded.contains("%20"), "{}", encoded);
        assert_eq!(encoded.matches('/').count(), key.matches('/').count(), "slashes stay literal");

        // Already-safe characters (hex hashes, the fixed prefixes) round-trip unchanged.
        let hash = "e".repeat(64);
        assert_eq!(percent_encode_copy_source_key(&object_key(&hash)), object_key(&hash));
    }

    /// The soundness property the whole streaming fix rests on: hashing a body in chunks
    /// through the incremental hasher must equal hashing the same bytes in one shot — the
    /// identity `object_utils::hash_object_bytes` (and the fake) checks. `ByteStream::from`
    /// yields the bytes in one chunk, which already exercises the real `try_next` + `update`
    /// wiring end to end; multi-chunk framing is Blake3's own tested guarantee (`update` is
    /// defined to be order-preserving and chunk-boundary-independent).
    #[tokio::test]
    async fn streaming_hash_matches_the_one_shot_hash() {
        let payload = b"the same bytes, hashed two different ways".to_vec();
        let expected = object_utils::hash_object_bytes(&payload);

        let body = ByteStream::from(payload);
        let outcome = stream_hash_capped(body, MAX_STAGED_OBJECT_BYTES).await.expect("stream");

        match outcome {
            StreamHash::Hash(actual) => assert_eq!(actual, expected),
            StreamHash::TooLarge => panic!("well under the cap"),
        }
    }

    /// The DoS defense itself: a body that exceeds a (small, test-only) cap is abandoned
    /// before it finishes streaming — this is what stands between an attacker-controlled
    /// staged-object size and unbounded memory use in `verify_and_promote`, exercised here
    /// without allocating anything close to the real 5 GiB production cap.
    #[tokio::test]
    async fn streaming_hash_aborts_once_the_cap_is_exceeded() {
        let payload = vec![0u8; 64];
        let body = ByteStream::from(payload);

        let outcome = stream_hash_capped(body, 16).await.expect("stream");

        assert!(matches!(outcome, StreamHash::TooLarge));
    }

    /// A body at exactly the cap is not "too large" — the check is a strict `>`, matching the
    /// module docs' "at or above [the streaming threshold], below [the hard cap]" framing for
    /// where an object is still accepted.
    #[tokio::test]
    async fn streaming_hash_accepts_a_body_exactly_at_the_cap() {
        let payload = vec![1u8; 32];
        let expected = object_utils::hash_object_bytes(&payload);
        let body = ByteStream::from(payload);

        let outcome = stream_hash_capped(body, 32).await.expect("stream");

        match outcome {
            StreamHash::Hash(actual) => assert_eq!(actual, expected),
            StreamHash::TooLarge => panic!("exactly at the cap must still be accepted"),
        }
    }
}
