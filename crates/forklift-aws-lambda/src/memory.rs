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
use forklift_core::util::redaction_utils::REDACTIONS_PALLET_NAME;
use forklift_core::util::pallet_utils::{PalletNamespace, PalletRef, DEFAULT_PALLET_NAME};

use crate::aws::dynamo::{
    item_entity, ATTR_ANCHOR, ATTR_DELETED_HEAD, ATTR_HEAD, ATTR_STATE, ENTITY_CHUNK_FENCE, ENTITY_CHUNK_LEDGER,
    ENTITY_FORMAT, ENTITY_PALLET_PREFIX, ENTITY_REDACTION_PREFIX, ENTITY_TRUST, ENTITY_WITHHELD_PREFIX,
};
use crate::store::{
    CasOutcome, ExpectedHead, FormatFence, FormatStore, IntakeCopy, ObjectAccess, ObjectStore, OffloadedResponse,
    ItemKey, ItemWrite, OfficePrecondition, PromoteOutcome, PutOutcome, PutTarget, RedactionWrite, RedactionsPrecondition, RefHead, RefListing, RefStore,
    SignatureOutcome, TransientCause, TrustOutcome, TrustWriteOutcome,
};

/// An in-memory [`ObjectStore`]. Object bytes are the uncompressed wire form, keyed by hash.
#[derive(Default)]
pub struct MemoryObjectStore {
    objects: Mutex<HashMap<String, Vec<u8>>>,
    signatures: Mutex<HashMap<String, Vec<u8>>>,
    /// Uploads that bypassed the head, keyed by `(session, hash)` — the in-memory stand-in
    /// for an S3 staging prefix. Invisible to `exists`/`get` until promoted.
    staged: Mutex<HashMap<(String, String), Vec<u8>>>,
    /// Offloaded response bodies (`batch` bundles), keyed by their content hash, with when each
    /// was last written — the stand-in for an ephemeral S3 prefix served by presigned `GET`.
    /// Never an object.
    responses: Mutex<HashMap<String, (Vec<u8>, std::time::SystemTime)>>,
    /// How many object bodies have been read out of the store — each one an S3 `GET` in the
    /// real backend, so tests can assert what the audit mirror does *not* fetch.
    reads: AtomicUsize,
    /// When set, `access`/`put_target` answer with a presigned-style URL under this base
    /// instead of serving bytes directly — the AWS deployment's behaviour.
    redirect_base: Option<String>,
    /// How long each object read takes, standing in for a slow store.
    read_delay: Option<std::time::Duration>,
    /// Redaction work objects, keyed by name — the stand-in for the `redaction-work/{wh}/`
    /// prefix. Never objects.
    work: Mutex<HashMap<String, Vec<u8>>>,
    /// Clients' copies of withheld recipes, keyed by hash — the stand-in for the `intake/{wh}/`
    /// prefix. Never objects, never staged.
    intake: Mutex<HashMap<String, Vec<u8>>>,
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

    /// The same store, taking `delay` over every object read — a slow store, for the tests of
    /// what a head does as its invocation deadline nears.
    pub fn with_read_delay(self, delay: std::time::Duration) -> MemoryObjectStore {
        MemoryObjectStore { read_delay: Some(delay), ..self }
    }

    /// Seed a *staged* upload, as if the client had `PUT` these bytes to the presigned
    /// staging URL for `(session, hash)` without the head ever seeing them. The bytes are
    /// not verified and not fetchable; only `verify_and_promote` can make them so.
    pub fn stage(&self, session: &str, hash: &str, bytes: Vec<u8>) {
        self.staged.lock().unwrap().insert((session.to_string(), hash.to_string()), bytes);
    }

    /// Land `bytes` at the intake key of `hash`, as a client's `PUT` to the URL
    /// [`intake_target`](ObjectStore::intake_target) handed out would.
    pub fn land_intake(&self, hash: &str, bytes: Vec<u8>) {
        self.intake.lock().unwrap().insert(hash.to_string(), bytes);
    }

    /// How many intake copies the store holds (for test assertions).
    pub fn intake_count(&self) -> usize {
        self.intake.lock().unwrap().len()
    }

    /// How many objects are stored at their canonical key (for test assertions).
    pub fn object_count(&self) -> usize {
        self.objects.lock().unwrap().len()
    }

    /// How many uploads are still sitting in staging (for test assertions).
    pub fn staged_count(&self) -> usize {
        self.staged.lock().unwrap().len()
    }

    /// The names of the redaction work objects the store holds, sorted (for test assertions).
    pub fn work_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.work.lock().unwrap().keys().cloned().collect();
        names.sort();
        names
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

        self.responses.lock().unwrap().get(key).map(|(bytes, _)| bytes.clone())
    }

    /// Make every offloaded response read as written `by` earlier than it was, standing in for
    /// the time a redaction's late sweep waits.
    pub fn age_responses(&self, by: std::time::Duration) {
        for (_, written) in self.responses.lock().unwrap().values_mut() {
            *written -= by;
        }
    }

    /// How many offloaded responses the store holds.
    pub fn response_count(&self) -> usize {
        self.responses.lock().unwrap().len()
    }
}

impl ObjectStore for MemoryObjectStore {
    fn exists(&self, hash: &str) -> Result<bool, String> {
        Ok(self.objects.lock().unwrap().contains_key(hash))
    }

    fn objects_staged(&self, session: &str, hashes: &[String]) -> Result<Vec<String>, String> {
        let staged = self.staged.lock().unwrap();

        Ok(hashes
            .iter()
            .filter(|hash| staged.contains_key(&(session.to_string(), hash.to_string())))
            .cloned()
            .collect())
    }

    fn get(&self, hash: &str) -> Result<Option<Vec<u8>>, String> {
        self.reads.fetch_add(1, Ordering::Relaxed);

        if let Some(delay) = self.read_delay {
            std::thread::sleep(delay);
        }

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
            // A staging key under the session — never the canonical key `get`/`exists` read.
            // The fake keys nothing per warehouse: a test that hands one fake to several
            // warehouses shares their objects, as the S3 store no longer does.
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

    fn delete_canonical(&self, hash: &str) -> Result<(), String> {
        self.objects.lock().unwrap().remove(hash);

        Ok(())
    }

    fn delete_staged(&self, hash: &str) -> Result<(), String> {
        self.staged.lock().unwrap().retain(|(_, staged), _| staged != hash);

        Ok(())
    }

    fn delete_staged_copy(&self, session: &str, hash: &str) -> Result<(), String> {
        self.staged.lock().unwrap().remove(&(session.to_string(), hash.to_string()));

        Ok(())
    }

    fn work_get(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(self.work.lock().unwrap().get(name).cloned())
    }

    fn work_put(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        self.work.lock().unwrap().insert(name.to_string(), bytes.to_vec());

        Ok(())
    }

    fn work_create(&self, name: &str, bytes: &[u8]) -> Result<bool, String> {
        let mut work = self.work.lock().unwrap();

        if work.contains_key(name) {
            return Ok(false);
        }
        work.insert(name.to_string(), bytes.to_vec());

        Ok(true)
    }

    fn work_delete(&self, name: &str) -> Result<(), String> {
        self.work.lock().unwrap().remove(name);

        Ok(())
    }

    fn work_delete_under(&self, prefix: &str) -> Result<(), String> {
        self.work.lock().unwrap().retain(|name, _| !name.starts_with(prefix));

        Ok(())
    }

    /// A URL under the redirect base when there is one, else under a base of its own: the fake
    /// takes the bytes only through [`MemoryObjectStore::land_intake`] either way.
    fn intake_target(&self, hash: &str) -> Result<String, String> {
        Ok(format!("{}/intake/{}", self.redirect_base.as_deref().unwrap_or("memory://store"), hash))
    }

    fn take_intake(&self, hash: &str, cap: u64) -> Result<IntakeCopy, String> {
        Ok(match self.intake.lock().unwrap().remove(hash) {
            None => IntakeCopy::Absent,
            Some(bytes) if bytes.len() as u64 > cap => IntakeCopy::Oversized,
            Some(bytes) => IntakeCopy::Bytes(bytes),
        })
    }

    fn delete_responses_before(&self, cutoff: std::time::SystemTime) -> Result<(), String> {
        self.responses.lock().unwrap().retain(|_, (_, written)| *written >= cutoff);

        Ok(())
    }

    fn offload_response(&self, bytes: Vec<u8>) -> Result<OffloadedResponse, String> {
        let Some(base) = &self.redirect_base else {
            return Ok(OffloadedResponse::Inline(bytes));
        };

        // A content-addressed *response* key, deliberately outside the `objects/` namespace.
        let key = object_utils::hash_object_bytes(&bytes);

        self.responses.lock().unwrap().insert(key.clone(), (bytes, std::time::SystemTime::now()));

        Ok(OffloadedResponse::Url(format!("{}/responses/{}", base, key)))
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
    /// Every pallet's entry, live or tombstoned. An entry, not a head: a delete keeps it and
    /// marks it deleted, the mirror of the DynamoDB item a delete keeps, so a creation expecting
    /// no entry at all can be refused over a tombstone exactly as the real store refuses it.
    pallets: HashMap<String, RefHead>,
    /// The trust anchor's *stored serialization* — the same representation `DynamoRefStore`
    /// keeps (the JSON text of the DTO), not the decoded value. The commit's anchor
    /// precondition is string equality on stored bytes; a fake that kept only a decoded value
    /// could never represent the comparison the real store enforces.
    trust: Option<String>,
    /// The format item's state, or `None` while the warehouse has none — the item
    /// `DynamoRefStore` keeps beside the pallets and the anchor.
    format: Option<String>,
    /// The stored state of each item [`RefStore::put_items`] writes (the redaction items and the
    /// chunk fence and ledger), keyed by the sort key `DynamoRefStore` gives it.
    items: HashMap<String, String>,
    /// How many of the next [`RefStore::put_items`] calls that would write land and then answer
    /// as a write of unknown fate, as one whose response DynamoDB lost.
    writes_landing_unknown: usize,
    /// What the next [`RefStore::put_items`] calls answer instead of writing, each a transient
    /// refusal that wrote nothing.
    writes_refused: std::collections::VecDeque<TransientCause>,
    /// An item another writer puts, by sort key and state, just before the next
    /// [`RefStore::put_items`] call checks its conditions.
    racing_write: Option<(String, String)>,
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
            state: Mutex::new(RefState {
                pallets: HashMap::new(),
                trust: None,
                format: None,
                items: HashMap::new(),
                writes_landing_unknown: 0,
                writes_refused: std::collections::VecDeque::new(),
                racing_write: None,
            }),
            default_pallet: DEFAULT_PALLET_NAME.to_string(),
        }
    }
}

impl MemoryRefStore {
    /// A fresh ref store with the default pallet (`main`).
    pub fn new() -> MemoryRefStore {
        MemoryRefStore::default()
    }

    /// Set the format item's state, or remove the item with `None`, bypassing every fence — the
    /// stand-in for an upgrade's mark, a build of another format, or a warehouse written before
    /// formats were recorded.
    pub fn set_format(&self, state: Option<&str>) {
        self.state.lock().unwrap().format = state.map(str::to_string);
    }

    /// Make the next `count` [`RefStore::put_items`] calls that write land, then answer as writes
    /// of unknown fate: the stand-in for a transaction that committed and whose response was lost.
    pub fn land_next_writes_unknown(&self, count: usize) {
        self.state.lock().unwrap().writes_landing_unknown = count;
    }

    /// Make the next [`RefStore::put_items`] calls answer `causes`, in turn, as transient
    /// refusals that wrote nothing.
    ///
    /// Within one call the hooks act in this order: a write [`race_next_write`] armed lands first,
    /// then a refusal armed here answers before any check, and a write that passes its checks
    /// lands before [`land_next_writes_unknown`] answers its fate unknown.
    ///
    /// [`race_next_write`]: MemoryRefStore::race_next_write
    /// [`land_next_writes_unknown`]: MemoryRefStore::land_next_writes_unknown
    pub fn refuse_next_writes(&self, causes: &[TransientCause]) {
        self.state.lock().unwrap().writes_refused.extend(causes.iter().copied());
    }

    /// Have another writer put `state` as the item of sort key `entity` just before the next
    /// [`RefStore::put_items`] call checks its conditions, as a write racing it would.
    pub fn race_next_write(&self, entity: &str, state: &str) {
        self.state.lock().unwrap().racing_write = Some((entity.to_string(), state.to_string()));
    }

    /// This store's state as the DynamoDB items `DynamoRefStore` keeps for it: each item's sort
    /// key, its one payload attribute and that attribute's value, sorted by sort key. What the AWS
    /// format fixture freezes.
    pub fn items(&self) -> Vec<(String, String, String)> {
        let state = self.state.lock().unwrap();
        let mut items = Vec::new();

        if let Some(format) = &state.format {
            items.push((ENTITY_FORMAT.to_string(), ATTR_STATE.to_string(), format.clone()));
        }
        if let Some(trust) = &state.trust {
            items.push((ENTITY_TRUST.to_string(), ATTR_ANCHOR.to_string(), trust.clone()));
        }
        for (entity, item) in &state.items {
            items.push((entity.clone(), ATTR_STATE.to_string(), item.clone()));
        }
        for (wire, head) in &state.pallets {
            let entity = format!("{}{}", ENTITY_PALLET_PREFIX, wire);
            match head {
                RefHead::Live(head) => items.push((entity, ATTR_HEAD.to_string(), head.clone())),
                RefHead::Deleted(head) => items.push((entity, ATTR_DELETED_HEAD.to_string(), head.clone())),
                RefHead::Absent => {}
            }
        }

        items.sort();
        items
    }

    /// A store holding exactly `items`, as [`Self::items`] lists them.
    ///
    /// # Returns
    /// * `Ok(MemoryRefStore)` - The store.
    /// * `Err(String)`        - An item this build does not keep: an unknown sort key, a pallet with
    ///                          no name, an attribute its kind of item does not carry, or a sort key
    ///                          listed twice.
    pub fn from_items(items: &[(String, String, String)]) -> Result<MemoryRefStore, String> {
        let store = MemoryRefStore::new();
        let mut state = store.state.lock().unwrap();

        let mut seen = std::collections::HashSet::new();

        for (entity, attribute, value) in items {
            if !seen.insert(entity) {
                return Err(format!("The item \"{}\" is listed twice.", entity));
            }
            let pallet = entity.strip_prefix(ENTITY_PALLET_PREFIX).filter(|wire| !wire.is_empty());
            let redaction = entity.strip_prefix(ENTITY_REDACTION_PREFIX).filter(|hash| !hash.is_empty());
            let withheld = entity.strip_prefix(ENTITY_WITHHELD_PREFIX).filter(|hash| !hash.is_empty());

            match (entity.as_str(), pallet, attribute.as_str()) {
                (ENTITY_FORMAT, _, ATTR_STATE) => state.format = Some(value.clone()),
                (ENTITY_CHUNK_FENCE | ENTITY_CHUNK_LEDGER, _, ATTR_STATE) => {
                    state.items.insert(entity.clone(), value.clone());
                }
                (_, None, ATTR_STATE) if redaction.is_some() || withheld.is_some() => {
                    state.items.insert(entity.clone(), value.clone());
                }
                (ENTITY_TRUST, _, ATTR_ANCHOR) => state.trust = Some(value.clone()),
                (_, Some(wire), ATTR_HEAD) => { state.pallets.insert(wire.to_string(), RefHead::Live(value.clone())); }
                (_, Some(wire), ATTR_DELETED_HEAD) => {
                    state.pallets.insert(wire.to_string(), RefHead::Deleted(value.clone()));
                }
                _ => return Err(format!("The item \"{}\" carries \"{}\", which this build does not keep.", entity, attribute)),
            }
        }

        drop(state);
        Ok(store)
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

    /// The office precondition to check, if any, for a write to the entry at `key`: `None` when
    /// there is nothing to check, else the office head it must still hold (`None` inside: unborn).
    ///
    /// Nothing is checked in two cases. Structural: a write to `@office` itself needs no separate
    /// office check — the pallet-head check already pins that exact entry, the fake's mirror of
    /// the reason `DynamoRefStore` drops the redundant `ConditionCheck` there (two actions on one
    /// item is refused by DynamoDB; here it would just be checking the same key twice). Semantic:
    /// `OfficePrecondition::NotConsumed` means the caller never consumed an office head at all —
    /// not "expect the office pallet to be unborn" — so it must not be compared against whatever
    /// the office key currently holds; see `RefStore::compare_and_set_head`'s docs for why
    /// treating it as "unborn" here would refuse an untrusted push whose office pallet genuinely
    /// (if unaudited) has a real head, every time, not just under a race. `Unborn` is the
    /// opposite claim and is checked: the office must still have no head.
    fn office_condition<'a>(key: &str, office_head: OfficePrecondition<'a>) -> Option<Option<&'a str>> {
        if key == Self::key(PalletNamespace::Meta, OFFICE_PALLET_NAME) {
            return None;
        }

        match office_head {
            OfficePrecondition::NotConsumed => None,
            OfficePrecondition::Unborn => Some(None),
            OfficePrecondition::At(head) => Some(Some(head)),
        }
    }

    /// The `@redactions` head a commit to `key` conditions on, by the same rule as
    /// [`office_condition`](Self::office_condition): none for a write of `@redactions` itself.
    fn redactions_condition<'a>(key: &str, redactions: RedactionsPrecondition<'a>) -> Option<Option<&'a str>> {
        if key == Self::key(PalletNamespace::Meta, REDACTIONS_PALLET_NAME) {
            return None;
        }

        match redactions {
            RedactionsPrecondition::NotConsumed => None,
            RedactionsPrecondition::Unborn => Some(None),
            RedactionsPrecondition::At(head) => Some(Some(head)),
        }
    }

    /// Check the office, `@redactions` and anchor preconditions under the held guard: `Some`
    /// names the one that failed, `None` means all hold.
    fn moved_inputs(
        state: &RefState,
        office: Option<Option<&str>>,
        redactions: Option<Option<&str>>,
        anchor: Option<&str>,
        chunk_fence: Option<Option<&str>>,
    ) -> Option<CasOutcome> {
        if let Some(office_head) = office {
            let current_office = state.live_head(&Self::key(PalletNamespace::Meta, OFFICE_PALLET_NAME));

            if current_office.as_deref() != office_head {
                return Some(CasOutcome::OfficeMoved { current: current_office });
            }
        }

        if let Some(redactions_head) = redactions {
            let current = state.live_head(&Self::key(PalletNamespace::Meta, REDACTIONS_PALLET_NAME));

            if current.as_deref() != redactions_head {
                return Some(CasOutcome::RedactionsMoved);
            }
        }

        if state.trust.as_deref() != anchor {
            return Some(CasOutcome::AnchorMoved);
        }

        if let Some(fence) = chunk_fence {
            if state.items.get(ENTITY_CHUNK_FENCE).map(String::as_str) != fence {
                return Some(CasOutcome::ChunkFenceMoved);
            }
        }

        None
    }
}

impl RefState {
    /// Whether the format item still holds what `fence` expects: the state it names, or no item
    /// at all for a stamp. Checked before every other precondition, as `DynamoRefStore` places
    /// the format action first in each transaction.
    fn format_holds(&self, fence: &FormatFence) -> bool {
        match fence {
            FormatFence::At(state) => self.format.as_deref() == Some(state),
            FormatFence::Stamp(_) => self.format.is_none(),
        }
    }

    /// Commit the fence's own write, under the guard that checked it: a stamp records its state.
    fn apply_format(&mut self, fence: &FormatFence) {
        if let FormatFence::Stamp(state) = fence {
            self.format = Some(state.clone());
        }
    }

    fn entry(&self, key: &str) -> RefHead {
        self.pallets.get(key).cloned().unwrap_or(RefHead::Absent)
    }

    fn live_head(&self, key: &str) -> Option<String> {
        self.entry(key).live().map(str::to_string)
    }
}

impl RefStore for MemoryRefStore {
    fn get_head(&self, namespace: PalletNamespace, name: &str) -> Result<RefHead, String> {
        Ok(self.state.lock().unwrap().entry(&Self::key(namespace, name)))
    }

    fn read_format(&self) -> Result<Option<String>, String> {
        Ok(self.state.lock().unwrap().format.clone())
    }

    fn partition_has_items(&self) -> Result<bool, String> {
        let state = self.state.lock().unwrap();

        Ok(!state.pallets.is_empty() || state.trust.is_some() || state.format.is_some())
    }

    // One mutex makes every commit atomic in one step, so there is never an attempt whose
    // fate is unknown and nothing for a request token to deduplicate.
    #[allow(clippy::too_many_arguments)]
    fn compare_and_set_head(
        &self,
        namespace: PalletNamespace,
        name: &str,
        expected: ExpectedHead<'_>,
        new: &str,
        office_head: OfficePrecondition<'_>,
        redactions: RedactionsPrecondition<'_>,
        anchor: Option<&str>,
        chunk_fence: Option<&str>,
        format: &FormatFence,
        _request_token: &str,
    ) -> Result<CasOutcome, String> {
        let key = Self::key(namespace, name);
        let office = Self::office_condition(&key, office_head);
        let redactions = Self::redactions_condition(&key, redactions);

        let mut state = self.state.lock().unwrap();

        if !state.format_holds(format) {
            return Ok(CasOutcome::FormatMoved);
        }

        // Every precondition is evaluated, and the commit made, under this one guard —
        // the property this whole struct exists to provide (see `RefState`'s docs).
        let current = state.entry(&key);

        let holds = match (expected, &current) {
            (ExpectedHead::Unborn, RefHead::Absent) => true,
            (ExpectedHead::Deleted(want), RefHead::Deleted(have)) => want == have,
            (ExpectedHead::At(want), RefHead::Live(have)) => want == have,
            _ => false,
        };

        if !holds {
            return Ok(CasOutcome::Conflict { current });
        }

        if let Some(moved) = Self::moved_inputs(&state, office, redactions, anchor, Some(chunk_fence)) {
            return Ok(moved);
        }

        state.apply_format(format);
        state.pallets.insert(key, RefHead::Live(new.to_string()));

        Ok(CasOutcome::Committed)
    }

    fn delete_head(
        &self,
        namespace: PalletNamespace,
        name: &str,
        old: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        format: &FormatFence,
        _request_token: &str,
    ) -> Result<CasOutcome, String> {
        let key = Self::key(namespace, name);
        let office = Self::office_condition(&key, office_head);

        let mut state = self.state.lock().unwrap();

        if !state.format_holds(format) {
            return Ok(CasOutcome::FormatMoved);
        }

        let current = state.entry(&key);

        if current.live() != Some(old) {
            return Ok(CasOutcome::Conflict { current });
        }

        if let Some(moved) = Self::moved_inputs(&state, office, None, anchor, None) {
            return Ok(moved);
        }

        state.apply_format(format);
        state.pallets.insert(key, RefHead::Deleted(old.to_string()));

        Ok(CasOutcome::Committed)
    }

    fn list_refs(&self) -> Result<RefListing, String> {
        let state = self.state.lock().unwrap();
        let mut listing = RefListing::default();

        for (wire, entry) in &state.pallets {
            let pallet_ref = PalletRef::parse(wire)?;

            match entry {
                RefHead::Live(head) => listing.live.push((pallet_ref, head.clone())),
                RefHead::Deleted(head) => listing.tombstones.push((pallet_ref, head.clone())),
                RefHead::Absent => {}
            }
        }

        Ok(listing)
    }

    fn default_pallet(&self) -> Result<String, String> {
        Ok(self.default_pallet.clone())
    }

    fn get_item(&self, key: ItemKey<'_>) -> Result<Option<String>, String> {
        Ok(self.state.lock().unwrap().items.get(&item_entity(key)).cloned())
    }

    fn list_withheld(&self) -> Result<Vec<(String, String)>, String> {
        let state = self.state.lock().unwrap();
        // In no order: `withheld::list` sorts.
        Ok(state
            .items
            .iter()
            .filter_map(|(entity, item)| {
                entity.strip_prefix(ENTITY_WITHHELD_PREFIX).map(|hash| (hash.to_string(), item.clone()))
            })
            .collect())
    }

    fn put_items(&self, writes: &[ItemWrite<'_>], format: &FormatFence) -> Result<RedactionWrite, String> {
        let mut guard = self.state.lock().unwrap();

        if let Some((entity, state)) = guard.racing_write.take() {
            guard.items.insert(entity, state);
        }

        if let Some(cause) = guard.writes_refused.pop_front() {
            return Ok(RedactionWrite::Transient(cause));
        }

        if !guard.format_holds(format) {
            return Ok(RedactionWrite::FormatMoved);
        }

        if writes.iter().any(|write| guard.items.get(&item_entity(write.key)).map(String::as_str) != write.expected) {
            return Ok(RedactionWrite::Moved);
        }

        guard.apply_format(format);
        for write in writes {
            guard.items.insert(item_entity(write.key), write.state.to_string());
        }

        if guard.writes_landing_unknown > 0 {
            guard.writes_landing_unknown -= 1;
            return Ok(RedactionWrite::Transient(TransientCause::FateUnknown));
        }

        Ok(RedactionWrite::Written)
    }

    fn get_trust(&self) -> Result<Option<(TrustAnchor, String)>, String> {
        let state = self.state.lock().unwrap();

        let Some(json) = state.trust.as_ref() else {
            return Ok(None);
        };

        let dto = Self::decode(json)?;

        Ok(Some((dto.to_anchor(), json.clone())))
    }

    fn put_trust_if_absent(
        &self,
        anchor: &TrustAnchor,
        format: &FormatFence,
        _request_token: &str,
    ) -> Result<TrustOutcome, String> {
        let incoming = TrustAnchorDto::from(anchor);
        let json = Self::encode(anchor)?;
        let mut state = self.state.lock().unwrap();

        if !state.format_holds(format) {
            return Ok(TrustOutcome::FormatMoved);
        }

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
                state.apply_format(format);
                state.trust = Some(json);
                Ok(TrustOutcome::Established)
            }
        }
    }

    fn replace_trust(
        &self,
        anchor: &TrustAnchor,
        expected_anchor: &str,
        office_head: Option<&str>,
        format: &FormatFence,
        _request_token: &str,
    ) -> Result<TrustWriteOutcome, String> {
        let incoming = TrustAnchorDto::from(anchor);
        let json = Self::encode(anchor)?;
        let office_key = Self::key(PalletNamespace::Meta, OFFICE_PALLET_NAME);

        let mut state = self.state.lock().unwrap();

        if !state.format_holds(format) {
            return Ok(TrustWriteOutcome::FormatMoved);
        }

        // Both preconditions are evaluated, and the write made, under this one guard — the
        // mirror of the transaction being atomic over both actions. It is also what lets a
        // falsifying test interleave this write into the window between an audit's anchor read
        // and the commit that conditions on it.
        //
        // The incumbent is compared as *stored bytes*, never as a decoded anchor: that is the
        // comparison `DynamoRefStore` makes, and a fake comparing decoded values could not
        // represent it (see `RefState::trust`).
        if state.trust.as_deref() != Some(expected_anchor) {
            // Mirrors `DynamoRefStore`: an incumbent that moved to exactly what this call was
            // going to write is the caller's desired state, not a conflict. Compared as decoded
            // values for the reason given there, and reached from the same branch — the anchor
            // precondition is position 0 in that transaction, so it is what fails first.
            let identical = state
                .trust
                .as_deref()
                .and_then(|bytes| Self::decode(bytes).ok())
                .is_some_and(|incumbent| incumbent == incoming);

            if identical {
                return Ok(TrustWriteOutcome::AlreadyIdentical);
            }

            return Ok(TrustWriteOutcome::AnchorMoved { current: state.trust.clone() });
        }

        // Unlike `compare_and_set_head`'s, this office precondition has no "not consumed"
        // state: `put_trust` reads the office head unconditionally, so `None` here genuinely
        // means the validation consumed an unborn office and the write must condition on it
        // still being unborn.
        let current_office = state.live_head(&office_key);

        if current_office.as_deref() != office_head {
            return Ok(TrustWriteOutcome::OfficeMoved { current: current_office });
        }

        state.apply_format(format);
        state.trust = Some(json);

        Ok(TrustWriteOutcome::Replaced)
    }
}

impl FormatStore for MemoryRefStore {
    fn swap_format(&self, expected: &str, new: &str) -> Result<bool, String> {
        let mut state = self.state.lock().unwrap();

        if state.format.as_deref() != Some(expected) {
            return Ok(false);
        }

        state.format = Some(new.to_string());

        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(entity: &str, attribute: &str, value: &str) -> (String, String, String) {
        (entity.to_string(), attribute.to_string(), value.to_string())
    }

    /// A store loads exactly the items it dumps, and refuses an item it would never write.
    ///
    /// Revert: drop the duplicate check → the twice-listed pallet loads; drop the empty-name check →
    /// the bare prefix loads.
    #[test]
    fn from_items_loads_only_items_this_store_writes() {
        let items = vec![item("format", "state", "1"), item("pallet#main", "head", "h"), item("trust", "anchor", "{}")];
        assert_eq!(MemoryRefStore::from_items(&items).unwrap().items(), items);

        for refused in [
            vec![item("pallet#main", "head", "h"), item("pallet#main", "deleted_head", "h")],
            vec![item("pallet#", "head", "h")],
            vec![item("trust", "head", "h")],
            vec![item("lease", "state", "1")],
        ] {
            assert!(MemoryRefStore::from_items(&refused).is_err(), "{:?}", refused);
        }
    }

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
