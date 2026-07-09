//! In-memory fakes of the two stores, so the whole protocol suite runs in CI against the
//! real [`Head`](crate::Head) logic without AWS. They implement the same semantics the S3
//! and DynamoDB backends must — hash-verified object writes, immutable signatures, an
//! atomic head CAS, a one-way trust door — in a `HashMap` behind a `Mutex`.
//!
//! [`MemoryObjectStore`] can also be put in *staging mode* to exercise the presigned-URL
//! branch of the head without a real S3, and offers [`MemoryObjectStore::stage`] to seed a
//! staged upload as if a client had `PUT` it straight to the staging prefix, bypassing the
//! control plane — the case `verify_and_promote` guards.
//!
//! There is deliberately **no way to put unverified bytes at a canonical hash key**: the
//! only paths into `objects` are `put_verified` and `verify_and_promote`, both of which
//! check `Blake3(bytes) == hash` first. The fake cannot express the state invariant 1
//! forbids, so a test cannot accidentally assert it is reachable.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use forklift_core::model::remote::TrustAnchorDto;
use forklift_core::util::object_utils;
use forklift_core::util::office_utils::TrustAnchor;
use forklift_core::util::pallet_utils::{PalletNamespace, PalletRef, DEFAULT_PALLET_NAME};

use crate::store::{
    CasOutcome, ObjectAccess, ObjectStore, PromoteOutcome, PutOutcome, PutTarget, RefStore,
    SignatureOutcome, TrustOutcome,
};

/// An in-memory [`ObjectStore`]. Object bytes are the uncompressed wire form, keyed by hash.
#[derive(Default)]
pub struct MemoryObjectStore {
    objects: Mutex<HashMap<String, Vec<u8>>>,
    signatures: Mutex<HashMap<String, Vec<u8>>>,
    /// Uploads that bypassed the head, keyed by `(session, hash)` — the in-memory stand-in
    /// for an S3 staging prefix. Invisible to `exists`/`get` until promoted.
    staged: Mutex<HashMap<(String, String), Vec<u8>>>,
    /// Offloaded response bodies (`batch` bundles), keyed by their content hash — the
    /// stand-in for an ephemeral S3 prefix served by presigned `GET`. Never an object.
    responses: Mutex<HashMap<String, Vec<u8>>>,
    /// How many object bodies have been read out of the store — each one an S3 `GET` in the
    /// real backend, so tests can assert what the audit mirror does *not* fetch.
    reads: AtomicUsize,
    /// When set, `access`/`put_target` answer with a presigned-style URL under this base
    /// instead of serving bytes directly — the AWS deployment's behaviour.
    redirect_base: Option<String>,
}

impl MemoryObjectStore {
    /// A direct-serving store (the self-host equivalent).
    pub fn new() -> MemoryObjectStore {
        MemoryObjectStore::default()
    }

    /// A store that hands out presigned-style staging URLs under `base`, so tests can
    /// exercise the head's `307` + verify-and-promote branch without S3.
    pub fn with_redirect(base: impl Into<String>) -> MemoryObjectStore {
        MemoryObjectStore { redirect_base: Some(base.into()), ..Default::default() }
    }

    /// Seed a *staged* upload, as if the client had `PUT` these bytes to the presigned
    /// staging URL for `(session, hash)` without the head ever seeing them. The bytes are
    /// not verified and not fetchable; only `verify_and_promote` can make them so.
    pub fn stage(&self, session: &str, hash: &str, bytes: Vec<u8>) {
        self.staged.lock().unwrap().insert((session.to_string(), hash.to_string()), bytes);
    }

    /// How many objects are stored at their canonical key (for test assertions).
    pub fn object_count(&self) -> usize {
        self.objects.lock().unwrap().len()
    }

    /// How many uploads are still sitting in staging (for test assertions).
    pub fn staged_count(&self) -> usize {
        self.staged.lock().unwrap().len()
    }

    /// How many object bodies have been read from the store — an S3 `GET` apiece.
    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }

    /// Forget the read count, to measure one operation in isolation.
    pub fn reset_reads(&self) {
        self.reads.store(0, Ordering::Relaxed);
    }

    /// The bytes behind an offloaded response URL, as a presigned `GET` would serve them.
    pub fn offloaded_response(&self, url: &str) -> Option<Vec<u8>> {
        let key = url.rsplit('/').next()?;

        self.responses.lock().unwrap().get(key).cloned()
    }
}

impl ObjectStore for MemoryObjectStore {
    fn exists(&self, hash: &str) -> Result<bool, String> {
        Ok(self.objects.lock().unwrap().contains_key(hash))
    }

    fn get(&self, hash: &str) -> Result<Option<Vec<u8>>, String> {
        self.reads.fetch_add(1, Ordering::Relaxed);

        Ok(self.objects.lock().unwrap().get(hash).cloned())
    }

    fn put_verified(&self, hash: &str, bytes: &[u8]) -> Result<PutOutcome, String> {
        let actual = object_utils::hash_object_bytes(bytes);

        if actual != hash {
            return Err(format!(
                "Object content does not match its claimed hash {} (actual: {}); refusing to store it.",
                hash, actual
            ));
        }

        let mut objects = self.objects.lock().unwrap();

        if objects.contains_key(hash) {
            return Ok(PutOutcome::AlreadyPresent);
        }

        objects.insert(hash.to_string(), bytes.to_vec());

        Ok(PutOutcome::Created)
    }

    fn get_signature(&self, parcel_hash: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(self.signatures.lock().unwrap().get(parcel_hash).cloned())
    }

    fn put_signature(&self, parcel_hash: &str, bytes: &[u8]) -> Result<SignatureOutcome, String> {
        let mut signatures = self.signatures.lock().unwrap();

        match signatures.get(parcel_hash) {
            Some(existing) if existing == bytes => Ok(SignatureOutcome::AlreadyPresent),
            Some(_) => Ok(SignatureOutcome::Conflict),
            None => {
                signatures.insert(parcel_hash.to_string(), bytes.to_vec());
                Ok(SignatureOutcome::Created)
            }
        }
    }

    fn access(&self, hash: &str) -> Result<Option<ObjectAccess>, String> {
        match &self.redirect_base {
            Some(base) => {
                if self.objects.lock().unwrap().contains_key(hash) {
                    Ok(Some(ObjectAccess::Redirect(format!("{}/objects/{}", base, hash))))
                } else {
                    Ok(None)
                }
            }
            None => Ok(self.get(hash)?.map(ObjectAccess::Direct)),
        }
    }

    fn put_target(&self, session: Option<&str>, hash: &str) -> Result<PutTarget, String> {
        match (&self.redirect_base, session) {
            // A staging key under the session — never `objects/{hash}`, which is the
            // canonical key `get`/`exists` read.
            (Some(base), Some(session)) => {
                Ok(PutTarget::Staged(format!("{}/staging/{}/{}", base, session, hash)))
            }
            (Some(_), None) => Ok(PutTarget::SessionRequired),
            (None, _) => Ok(PutTarget::Direct),
        }
    }

    /// Take the staged bytes (so a corrupt upload is *discarded* by the same act that
    /// rejects it), and promote them only if they hash to `hash`.
    ///
    /// The whole check-and-promote runs under the `objects` lock, so the control plane and
    /// the staging verifier racing on one hash serialize: the loser observes the winner's
    /// canonical object and reports `AlreadyPresent`, never a spurious `Missing` because it
    /// found the staged copy already taken. An S3 + DynamoDB backend owes the same
    /// atomicity (a conditional write on the canonical key).
    fn verify_and_promote(&self, session: &str, hash: &str) -> Result<PromoteOutcome, String> {
        let key = (session.to_string(), hash.to_string());

        // Lock order is always `objects` before `staged`; nothing takes them the other way.
        let mut objects = self.objects.lock().unwrap();

        // Already canonical: the object was verified once, and objects are immutable. Sweep
        // the now-redundant staged copy.
        if objects.contains_key(hash) {
            self.staged.lock().unwrap().remove(&key);

            return Ok(PromoteOutcome::AlreadyPresent);
        }

        let Some(bytes) = self.staged.lock().unwrap().remove(&key) else {
            return Ok(PromoteOutcome::Missing);
        };

        let actual = object_utils::hash_object_bytes(&bytes);

        if actual != hash {
            return Ok(PromoteOutcome::Corrupt { actual });
        }

        objects.insert(hash.to_string(), bytes);

        Ok(PromoteOutcome::Promoted)
    }

    fn discard_session(&self, session: &str) -> Result<(), String> {
        self.staged.lock().unwrap().retain(|(staged_session, _), _| staged_session != session);

        Ok(())
    }

    fn offload_response(&self, bytes: &[u8]) -> Result<Option<String>, String> {
        let Some(base) = &self.redirect_base else {
            return Ok(None);
        };

        // A content-addressed *response* key, deliberately outside the `objects/` namespace.
        let key = object_utils::hash_object_bytes(bytes);

        self.responses.lock().unwrap().insert(key.clone(), bytes.to_vec());

        Ok(Some(format!("{}/responses/{}", base, key)))
    }
}

/// An in-memory [`RefStore`]. Pallet heads are keyed by their qualified wire reference
/// (`main`, `@office`), which is unique across the two namespaces.
pub struct MemoryRefStore {
    heads: Mutex<HashMap<String, String>>,
    trust: Mutex<Option<TrustAnchorDto>>,
    default_pallet: String,
}

impl Default for MemoryRefStore {
    fn default() -> MemoryRefStore {
        MemoryRefStore {
            heads: Mutex::new(HashMap::new()),
            trust: Mutex::new(None),
            default_pallet: DEFAULT_PALLET_NAME.to_string(),
        }
    }
}

impl MemoryRefStore {
    /// A fresh ref store with the default pallet (`main`).
    pub fn new() -> MemoryRefStore {
        MemoryRefStore::default()
    }

    fn key(namespace: PalletNamespace, name: &str) -> String {
        PalletRef { namespace, name: name.to_string() }.to_wire()
    }
}

impl RefStore for MemoryRefStore {
    fn get_head(&self, namespace: PalletNamespace, name: &str) -> Result<Option<String>, String> {
        Ok(self.heads.lock().unwrap().get(&Self::key(namespace, name)).cloned())
    }

    fn compare_and_set_head(
        &self,
        namespace: PalletNamespace,
        name: &str,
        expected: Option<&str>,
        new: &str,
    ) -> Result<CasOutcome, String> {
        let mut heads = self.heads.lock().unwrap();
        let key = Self::key(namespace, name);
        let current = heads.get(&key).cloned();

        if current.as_deref() != expected {
            return Ok(CasOutcome::Conflict { current });
        }

        heads.insert(key, new.to_string());

        Ok(CasOutcome::Committed)
    }

    fn list_refs(&self) -> Result<Vec<(PalletRef, String)>, String> {
        self.heads
            .lock()
            .unwrap()
            .iter()
            .map(|(wire, head)| Ok((PalletRef::parse(wire)?, head.clone())))
            .collect()
    }

    fn default_pallet(&self) -> Result<String, String> {
        Ok(self.default_pallet.clone())
    }

    fn get_trust(&self) -> Result<Option<TrustAnchor>, String> {
        Ok(self.trust.lock().unwrap().as_ref().map(|dto| dto.to_anchor()))
    }

    fn put_trust_if_absent(&self, anchor: &TrustAnchor) -> Result<TrustOutcome, String> {
        let incoming = TrustAnchorDto::from(anchor);
        let mut trust = self.trust.lock().unwrap();

        match trust.as_ref() {
            Some(existing) if *existing == incoming => Ok(TrustOutcome::AlreadyIdentical),
            Some(_) => Ok(TrustOutcome::Conflict),
            None => {
                *trust = Some(incoming);
                Ok(TrustOutcome::Established)
            }
        }
    }

    fn replace_trust(&self, anchor: &TrustAnchor) -> Result<(), String> {
        *self.trust.lock().unwrap() = Some(TrustAnchorDto::from(anchor));
        Ok(())
    }
}
