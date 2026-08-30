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
use forklift_core::util::office_utils::{TrustAnchor, OFFICE_PALLET_NAME};
use forklift_core::util::pallet_utils::{PalletNamespace, PalletRef, DEFAULT_PALLET_NAME};

use crate::store::{
    CasOutcome, ObjectAccess, ObjectStore, OfficePrecondition, PromoteOutcome, PutOutcome,
    PutTarget, RefStore, SignatureOutcome, TrustOutcome,
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

/// The state a [`MemoryRefStore`] guards behind **one** mutex — not two, as an earlier shape
/// of this fake held. A check-and-commit that spans a head and the trust anchor (FORK-95
/// design memo, claim C13) cannot be atomic across two independent locks: a caller taking them
/// in sequence would open a window between the two acquisitions for `replace_trust` to land
/// in — the very race this design exists to close, reproduced inside the reference store the
/// falsifying tests run against, where it would let an anchor precondition pass for the wrong
/// reason. Folding both into one guard is what makes `compare_and_set_head` genuinely atomic
/// here, the same property `DynamoRefStore` gets from a real `TransactWriteItems`.
struct RefState {
    heads: HashMap<String, String>,
    /// The trust anchor's *stored serialization* — the same representation `DynamoRefStore`
    /// keeps (the JSON text of the DTO), not the decoded value. The commit's anchor
    /// precondition is string equality on stored bytes; a fake that kept only a decoded value
    /// could never represent the comparison the real store enforces.
    trust: Option<String>,
}

/// An in-memory [`RefStore`]. Pallet heads are keyed by their qualified wire reference
/// (`main`, `@office`), which is unique across the two namespaces.
pub struct MemoryRefStore {
    state: Mutex<RefState>,
    default_pallet: String,
}

impl Default for MemoryRefStore {
    fn default() -> MemoryRefStore {
        MemoryRefStore {
            state: Mutex::new(RefState { heads: HashMap::new(), trust: None }),
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

    /// Decode a stored anchor serialization, the same way `DynamoRefStore::get_trust` does.
    fn decode(json: &str) -> Result<TrustAnchorDto, String> {
        serde_json::from_str(json)
            .map_err(|err| format!("decoding the stored trust anchor failed: {}", err))
    }

    /// Encode an anchor to the form this store keeps, the same way `DynamoRefStore` does.
    fn encode(anchor: &TrustAnchor) -> Result<String, String> {
        serde_json::to_string(&TrustAnchorDto::from(anchor))
            .map_err(|err| format!("encoding the trust anchor failed: {}", err))
    }
}

impl RefStore for MemoryRefStore {
    fn get_head(&self, namespace: PalletNamespace, name: &str) -> Result<Option<String>, String> {
        Ok(self.state.lock().unwrap().heads.get(&Self::key(namespace, name)).cloned())
    }

    fn compare_and_set_head(
        &self,
        namespace: PalletNamespace,
        name: &str,
        expected: Option<&str>,
        new: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
    ) -> Result<CasOutcome, String> {
        let key = Self::key(namespace, name);
        let office_key = Self::key(PalletNamespace::Meta, OFFICE_PALLET_NAME);
        // The office precondition is checked only when both hold at once. Structural: a lift
        // to `@office` itself needs no separate office check — the pallet-head check below
        // already pins that exact entry, the fake's mirror of the reason `DynamoRefStore`
        // drops the redundant `ConditionCheck` there (two actions on one item is refused by
        // DynamoDB; here it would just be checking the same key twice). Semantic:
        // `OfficePrecondition::NotConsumed` means the caller's audit never consumed an office
        // head at all — not "expect the office pallet to be unborn" — so it must not be
        // compared against whatever the office key currently holds; see
        // `RefStore::compare_and_set_head`'s docs for why treating it as "unborn" here would
        // refuse an untrusted push whose office pallet genuinely (if unaudited) has a real
        // head, every time, not just under a race.
        let office_head = match office_head {
            OfficePrecondition::At(head) if key != office_key => Some(head),
            _ => None,
        };

        let mut state = self.state.lock().unwrap();

        // All three preconditions are evaluated, and the commit made, under this one guard —
        // the property this whole struct exists to provide (see `RefState`'s docs).
        let current = state.heads.get(&key).cloned();

        if current.as_deref() != expected {
            return Ok(CasOutcome::Conflict { current });
        }

        if let Some(office_head) = office_head {
            let current_office = state.heads.get(&office_key).cloned();

            if current_office.as_deref() != Some(office_head) {
                return Ok(CasOutcome::OfficeMoved { current: current_office });
            }
        }

        if state.trust.as_deref() != anchor {
            return Ok(CasOutcome::AnchorMoved);
        }

        state.heads.insert(key, new.to_string());

        Ok(CasOutcome::Committed)
    }

    fn list_refs(&self) -> Result<Vec<(PalletRef, String)>, String> {
        self.state
            .lock()
            .unwrap()
            .heads
            .iter()
            .map(|(wire, head)| Ok((PalletRef::parse(wire)?, head.clone())))
            .collect()
    }

    fn default_pallet(&self) -> Result<String, String> {
        Ok(self.default_pallet.clone())
    }

    fn get_trust(&self) -> Result<Option<(TrustAnchor, String)>, String> {
        let state = self.state.lock().unwrap();

        let Some(json) = state.trust.as_ref() else {
            return Ok(None);
        };

        let dto = Self::decode(json)?;

        Ok(Some((dto.to_anchor(), json.clone())))
    }

    fn put_trust_if_absent(&self, anchor: &TrustAnchor) -> Result<TrustOutcome, String> {
        let incoming = TrustAnchorDto::from(anchor);
        let json = Self::encode(anchor)?;
        let mut state = self.state.lock().unwrap();

        match &state.trust {
            Some(existing_json) => {
                let existing = Self::decode(existing_json)?;

                if existing == incoming {
                    Ok(TrustOutcome::AlreadyIdentical)
                } else {
                    Ok(TrustOutcome::Conflict)
                }
            }
            None => {
                state.trust = Some(json);
                Ok(TrustOutcome::Established)
            }
        }
    }

    fn replace_trust(&self, anchor: &TrustAnchor) -> Result<(), String> {
        let json = Self::encode(anchor)?;

        // Unconditional, exactly as `DynamoRefStore::replace_trust` still is this slice — its
        // conditional write is FORK-95 claim C22, a later slice. Taking the *same* guard
        // `compare_and_set_head` does is what matters here: it is what lets a falsifying test
        // interleave this write into the window between an audit's anchor read and the commit
        // that conditions on it.
        self.state.lock().unwrap().trust = Some(json);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bulk presence probe (`objects_missing`, the default serial impl the fake inherits and the
    /// commit-gate chunk descent leans on) must return exactly the absent subset of a hash list — the
    /// same semantics the S3 backend's bounded-concurrency `HEAD` batch must match. Exercised with
    /// hundreds of hashes and a scattered handful withheld, the scale a changed large file's chunk
    /// list actually reaches (which no content-driven test can afford to materialize).
    #[test]
    fn objects_missing_returns_exactly_the_absent_subset_of_a_large_batch() {
        let store = MemoryObjectStore::new();

        // Hundreds of content-addressed objects; store all but a scattered few.
        let mut all: Vec<String> = Vec::new();
        let mut withheld: Vec<String> = Vec::new();

        for i in 0..400u32 {
            let bytes = format!("chunk-{i}").into_bytes();
            let hash = object_utils::hash_object_bytes(&bytes);
            all.push(hash.clone());

            if i % 97 == 0 {
                withheld.push(hash); // 0, 97, 194, 291, 388 — five absent
            } else {
                store.put_verified(&hash, &bytes).expect("store the object");
            }
        }

        let mut missing = store.objects_missing(&all).expect("bulk probe");
        missing.sort();
        let mut expected = withheld.clone();
        expected.sort();
        assert_eq!(missing, expected, "objects_missing names exactly the withheld objects");

        // A fully-present slice yields nothing missing.
        let present: Vec<String> = all.iter().filter(|h| !withheld.contains(h)).cloned().collect();
        assert!(
            store.objects_missing(&present).expect("bulk probe").is_empty(),
            "a fully-present list has no missing objects"
        );

        // An empty batch is trivially complete.
        assert!(store.objects_missing(&[]).expect("bulk probe").is_empty());
    }
}
