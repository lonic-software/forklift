//! Applying and refusing redactions on the AWS head (FORK-411).
//!
//! A redaction recorded on `@redactions` applies once no pallet head, tombstone or meta pallet's
//! history names its content. The AWS head has no warehouse-wide gate over its content routes, so
//! it settles each redaction on its own, where its content would otherwise move: a route asked
//! for a recorded hash judges that redaction first, from strongly consistent reads, and applies
//! it before answering when nothing holds it any more.
//!
//! # Holder verdicts
//!
//! Deciding from scratch would walk every pallet's tree per request. Instead a scan, started
//! only after reading a `@redactions` head that records the redaction, lists the pallet items
//! once and walks each pallet once, recording which pallets name the content and at which head.
//! The scan's listing is made after the redaction was recorded, and no head written after that
//! may name the content anew (the ref update's redaction check), so the verdicts stay true:
//!
//! - a user pallet holds the redaction exactly while its item still carries, live or as a
//!   tombstone, the head it was seen naming the content at;
//! - a pallet created after the listing holds nothing;
//! - a meta pallet seen naming it holds it while its head descends from the head seen. One whose
//!   head does not descend from it is scanned again.
//!
//! A walk can span invocations: its frontier and visited set are saved as a work object (under
//! `redaction-work/{wh}/` on S3), keyed by the redaction and the pallet's head seen, together
//! with the list of pallets the scan has still to walk. Once every pallet has a verdict, a judge
//! costs one consistent `Query` of the pallet items, plus an ancestry walk for a moved meta
//! holder.
//!
//! # The item
//!
//! The scan's verdicts and the apply live in the redaction's item (`redaction#<hash>`), a
//! versioned record every write of which is conditioned on the exact bytes its writer read. A
//! record of a version this build does not know fails every read of that redaction, loudly: an
//! applied redaction must never read as pending, and must never be written over.
//!
//! A redaction stays pending, and is served, while a cell of it cannot be read: what it says of a
//! recipe's chunk list cannot be scanned for. A hash the store holds as a tree, a parcel or a
//! chunk is no file's content, so its redaction never applies, as core's scan never applies it:
//! it stays pending, and is served.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use forklift_core::enums::object_type::ObjectType;
use forklift_core::model::remote::{ServedRedaction, ServedRedactionState};
use forklift_core::util::applied_redactions;
use forklift_core::util::pallet_utils::{PalletNamespace, PalletRef};
use forklift_core::util::redaction_utils::{self, HoldingWalk, Redaction, WalkObjects, WalkProgress, REDACTIONS_PALLET_NAME};
use forklift_core::util::{file_utils, object_utils};

use super::Head;
use crate::error::{HeadError, HeadResult};
use crate::scratch::{materialize_state, MirroredObjects, Mirror, Scratch};
use crate::store::{ItemKey, ItemWrite, ObjectStore, RedactionWrite, RefListing, RefStore};

/// The version of the redaction item's record this build writes and reads.
const RECORD_VERSION: u64 = 2;

/// The version of the work objects this build writes and reads.
const WORK_VERSION: u64 = 1;

/// How many of one judge's item writes may lose a race to another request, or end with their fate
/// unknown, before it answers the redaction undecided. A write that lands is the judge's own
/// progress, and is never counted.
const WRITE_ATTEMPTS: usize = 8;

/// The `Retry-After` an undecided redaction's `503` carries, in seconds.
pub const UNDECIDED_RETRY_AFTER_SECONDS: u32 = 2;

/// The redactions each `@redactions` head records, by head. A head is content-addressed and its
/// cells never change, so an entry is never stale.
static RECORDED: OnceLock<Mutex<HashMap<String, Arc<Recorded>>>> = OnceLock::new();

/// What `@redactions` records at one head: each redaction this build can read, and the hash of
/// each cell it cannot.
#[derive(Debug, Default)]
pub struct Recorded {
    readable: HashMap<String, Redaction>,
    unreadable: HashSet<String>,
}

impl Recorded {
    /// Whether `hash` is recorded, readable or not.
    fn contains(&self, hash: &str) -> bool {
        self.readable.contains_key(hash) || self.unreadable.contains(hash)
    }

    /// Every recorded hash, sorted.
    fn hashes(&self) -> Vec<String> {
        let mut hashes: Vec<String> = self.readable.keys().chain(self.unreadable.iter()).cloned().collect();
        hashes.sort();
        hashes
    }
}

/// The redaction item's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    /// [`RECORD_VERSION`].
    version: u64,

    /// Whether every pallet of the scan's listing has a verdict.
    scan_complete: bool,

    /// Each pallet the scan saw naming the content, with the head it saw.
    holders: Vec<Seen>,

    /// When the redaction applied, in seconds since the Unix epoch; `None` while pending.
    applied_at: Option<u64>,

    /// Whether the store holds the hash as no file's content, so the redaction never applies.
    /// Written only once true: a record without it is one where that was never found.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    never_applies: bool,

    /// Which scan of the redaction the record's verdicts come from: every rescan of a moved meta
    /// holder starts the next. The list of pallets a scan has still to walk carries it, so a list
    /// written for another scan, by a request that read an older record, is never walked: the
    /// pallets are listed again instead. Written only once past the first scan.
    #[serde(default, skip_serializing_if = "is_first_scan")]
    scan: u64,

    /// Whether the copies of the content the store keeps have been deleted since it applied: every
    /// staged copy, and a blob's canonical object. Written only once true.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    swept: bool,

    /// Whether the store held the content as a recipe when it applied or was swept. A recipe's
    /// object is its chunk list, so it is kept until the chunk ledger has deleted its chunks
    /// (`chunks.rs`); every write that sets this journals it there in the same transaction.
    /// Written only once true.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    recipe: bool,

    /// Whether the recipe's object has been deleted, after its chunks. Written only once true.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    recipe_deleted: bool,

    /// Whether the late sweep has deleted every copy of the content the bucket holds: what the
    /// first sweep deletes, again, every response bundle that may hold the content, and the scan's
    /// work objects. For a recipe, its deletion is done once its object is deleted too. Written
    /// only once true.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    deleted: bool,

    /// Until when, in seconds since the Unix epoch, one request holds the deletion: no other
    /// starts it before then. Written only while held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lease_until: Option<u64>,
}

impl Record {
    /// Whether the deletion is done: the late sweep has run, and a recipe's object is gone too.
    fn done(&self) -> bool {
        self.deleted && (!self.recipe || self.recipe_deleted)
    }
}

/// Whether `scan` is the first scan, which a record leaves unwritten.
fn is_first_scan(scan: &u64) -> bool {
    *scan == 0
}

/// One pallet, by its wire reference, and the head a scan saw it at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Seen {
    pallet: String,
    head: String,
}

/// The work object listing the pallets a scan has still to walk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Remaining {
    version: u64,

    /// The scan this list belongs to: [`Record::scan`] of the record it was written for.
    scan: u64,

    pallets: Vec<Seen>,
}

/// The work object holding one pallet's walk part way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedWalk {
    version: u64,
    walk: HoldingWalk,
}

/// Where one redaction stands, as a route judges it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Applied: refused on every route. `deleted` once the late sweep has deleted every copy of
    /// the content the bucket holds.
    Applied { deleted: bool },

    /// Pending: something still holds it, a cell of it cannot be read, or it names no file's
    /// content. Served.
    Pending,

    /// Not decided within this request, for this reason. Not served.
    Undecided(String),
}

/// What one request may still spend advancing scans: object reads, when a test bounds them, and
/// the invocation's time less `REDACTION_ANSWER_RESERVE`.
pub(super) struct Budget {
    steps: Option<usize>,
    pub(super) deadline: Option<SystemTime>,

    /// How many item writes this request has lost to another, or left with their fate unknown.
    lost: usize,
}

impl Budget {
    /// Whether the invocation's time, less `REDACTION_ANSWER_RESERVE`, is not yet spent.
    pub(super) fn timely(&self) -> bool {
        self.deadline.is_none_or(|deadline| {
            deadline.duration_since(SystemTime::now()).unwrap_or_default() > super::REDACTION_ANSWER_RESERVE
        })
    }

    /// Whether another step is affordable; spends it when it is.
    pub(super) fn spend(&mut self) -> bool {
        let timely = self.timely();

        match &mut self.steps {
            _ if !timely => false,
            Some(0) => false,
            Some(steps) => {
                *steps -= 1;
                true
            }
            None => true,
        }
    }
}

/// How far one call of [`Head::advance_scan`] got.
enum Advance {
    /// It wrote the item, or found it moved: read it again.
    Moved,

    /// It ran out of budget, or a walk could not read an object, with this reason.
    Stopped(String),
}

/// What a complete scan's holders say now.
enum Holding {
    /// A holder still holds the redaction.
    Held,

    /// This meta holder still holds it, at `head`, which descends from the head seen: record
    /// `head` as seen, so the next judge walks only the parents added after it.
    Advanced { holder: Seen, head: String },

    /// Nothing holds it.
    Free,

    /// This meta holder's head no longer descends from the head seen: scan it again at `head`.
    Rescan { holder: Seen, head: String },

    /// The check could not finish, for this reason.
    Stopped(String),
}

/// The prefix every work object of the redaction of `hash` is named under.
fn work_prefix(hash: &str) -> String {
    format!("{}/", hash)
}

/// The name of the work object listing the pallets the scan of `hash` has still to walk.
fn remaining_name(hash: &str) -> String {
    format!("{}pallets", work_prefix(hash))
}

/// The name of the work object holding the walk of `seen` for the redaction of `hash`.
fn walk_name(hash: &str, seen: &Seen) -> String {
    format!("{}walk-{}", work_prefix(hash), object_utils::hash_object_bytes(format!("{}\n{}", seen.pallet, seen.head).as_bytes()))
}

/// The record `stored` holds, for the redaction of `hash`.
///
/// # Returns
/// * `Ok(Record)`  - The record.
/// * `Err(String)` - The record is of a version this build does not read, or does not parse. Never
///                   read as pending: the redaction may have applied.
fn decode_record(hash: &str, stored: &str) -> Result<Record, String> {
    let version = serde_json::from_str::<serde_json::Value>(stored)
        .ok()
        .and_then(|value| value.get("version").and_then(serde_json::Value::as_u64));

    if version != Some(RECORD_VERSION) {
        return Err(format!(
            "The redaction item for {} is of a version this build does not read ({}), so whether \
            that redaction applied cannot be told. Update the deployment to a build that reads it.",
            hash,
            version.map_or("none".to_string(), |version| version.to_string())
        ));
    }

    serde_json::from_str(stored).map_err(|error| format!("The redaction item for {} cannot be read: {}", hash, error))
}

/// The record as the JSON the item stores.
fn encode_record(record: &Record) -> HeadResult<String> {
    serde_json::to_string(record).map_err(|error| HeadError::internal(format!("encoding a redaction item failed: {}", error)))
}

/// A work object's bytes as `T`, checked for [`WORK_VERSION`]. A work object holds no verdict, so
/// one this build cannot read is dropped and its work done again.
fn decode_work<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Option<T> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;

    match value.get("version").and_then(serde_json::Value::as_u64) {
        Some(WORK_VERSION) => serde_json::from_value(value).ok(),
        _ => None,
    }
}

/// How long a deletion's lease runs past the request's deadline, in seconds.
pub(super) const SWEEP_LEASE_MARGIN: u64 = 60;

/// A Lambda invocation's longest, in seconds: how long a deletion's lease runs from its start when
/// the request has no deadline, and how long after a redaction applied a request that judged it
/// pending first may still write a response bundle holding its content.
pub(super) const INVOCATION_CEILING: u64 = 900;

/// How much later than its bound each of [`response_cutoff`] and [`late_sweep_due`] falls, in
/// seconds: `applied_at` is rounded down and taken before the apply's write lands, S3 stamps a
/// response on its own clock, and a URL is presigned a moment after its object is written.
pub(super) const LATE_SWEEP_MARGIN: u64 = 60;

/// When, in seconds since the Unix epoch, a response bundle last written may still hold the
/// content of a redaction that applied at `applied_at`: a request that judged the redaction
/// pending before it applied ends within [`INVOCATION_CEILING`], and every later one leaves the
/// content out. [`LATE_SWEEP_MARGIN`] later still, for the clocks.
fn response_cutoff(applied_at: u64) -> u64 {
    applied_at.saturating_add(INVOCATION_CEILING).saturating_add(LATE_SWEEP_MARGIN)
}

/// When the late sweep of a redaction that applied at `applied_at` is due, in seconds since the
/// Unix epoch: once every presigned URL that could reach a copy has expired. A response written
/// before [`response_cutoff`] was presigned then, and a staged `PUT` presigned before the apply
/// lands within the same window, so nothing the sweep deletes is still reachable, and nothing it
/// should delete can still arrive.
pub(super) fn late_sweep_due(applied_at: u64) -> u64 {
    response_cutoff(applied_at)
        .saturating_add(crate::aws::PRESIGN_TTL.as_secs())
        .saturating_add(LATE_SWEEP_MARGIN)
}

/// `time` in whole seconds since the Unix epoch.
pub(super) fn epoch_seconds(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |since| since.as_secs())
}

/// The type `objects` holds `hash` as, from its header; `None` when it holds no object.
///
/// # Returns
/// * `Err(String)` - The object could not be read, or its header could not be parsed.
fn stored_type<O: ObjectStore + ?Sized>(objects: &O, hash: &str) -> Result<Option<ObjectType>, String> {
    let Some(bytes) = objects.get(hash)? else {
        return Ok(None);
    };

    object_utils::peek_object_header(&bytes)
        .map(|(object_type, _)| Some(object_type))
        .map_err(|error| format!("Object {} cannot be read: {:?}", hash, error))
}

/// What [`delete_if_applied`] found, and did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppliedCopies {
    /// The redaction has not applied: nothing was deleted.
    Unapplied,
    /// The redaction has applied: the session's staged copy and the canonical object were
    /// deleted.
    Deleted,
    /// The redaction has applied, and the store holds the content as a recipe, kept until its
    /// chunks are deleted: only the session's staged copy was deleted.
    RecipeKept,
}

impl AppliedCopies {
    /// Whether the redaction has applied: the content must not be promoted, and a lift commit
    /// naming it is refused.
    pub fn applied(self) -> bool {
        self != AppliedCopies::Unapplied
    }
}

/// If the redaction of `hash` has applied, delete the copies a promotion from `session` may have
/// left or be about to promote: that session's staged copy, and the canonical object unless the
/// store holds it as a recipe whose chunks are not deleted yet. What the staging verifier and a lift commit check before they
/// promote, and again after: a redaction that applied between the first check and the
/// promotion's write may already have been swept, finding no canonical object to delete. This
/// reads one key, so a client retrying applied content costs no listing. Other sessions' copies
/// are not touched: one staged before the sweep is the sweep's, one staged after it is deleted by
/// its own verifier event or the lift commit naming it, and one neither reaches is left to the
/// `staging/` lifecycle rule.
///
/// # Returns
/// * `Ok(AppliedCopies)` - Whether the redaction has applied, and what was deleted.
/// * `Err(String)`       - The item or the object could not be read, or a delete failed.
pub fn delete_if_applied<O: ObjectStore + ?Sized, R: RefStore + ?Sized>(
    objects: &O,
    refs: &R,
    session: &str,
    hash: &str,
) -> Result<AppliedCopies, String> {
    let Some(record) = applied_record(refs, hash)? else {
        return Ok(AppliedCopies::Unapplied);
    };

    objects.delete_staged_copy(session, hash)?;

    // A recipe is the list its chunks are found by, so it goes only once they are done; a copy
    // promoted after its deletion is deleted as a blob's is.
    if stored_type(objects, hash)? == Some(ObjectType::Recipe)
        && !record.recipe_deleted
        && !super::chunks::recipe_may_go(refs, hash)?
    {
        return Ok(AppliedCopies::RecipeKept);
    }

    objects.delete_canonical(hash)?;

    Ok(AppliedCopies::Deleted)
}

/// Whether the redaction of `hash` has applied, by its item in `refs`: what the staging verifier
/// and a lift commit read before they promote a staged copy.
///
/// # Returns
/// * `Ok(bool)`    - Whether the item records it applied; `false` when there is no item.
/// * `Err(String)` - The item could not be read, or is of a version this build does not read.
pub fn is_applied<R: RefStore + ?Sized>(refs: &R, hash: &str) -> Result<bool, String> {
    Ok(applied_record(refs, hash)?.is_some())
}

/// The record of the redaction of `hash`, when its item records it applied.
fn applied_record<R: RefStore + ?Sized>(refs: &R, hash: &str) -> Result<Option<Record>, String> {
    match refs.get_redaction(hash)? {
        Some(stored) => Ok(Some(decode_record(hash, &stored)?).filter(|record| record.applied_at.is_some())),
        None => Ok(None),
    }
}

/// The item state of a record of this shape, exactly as the judge writes it: what the AWS format
/// fixture freezes. `holders` are `(pallet, head)` pairs, the pallet by its wire reference.
pub fn record_state(scan_complete: bool, holders: &[(String, String)], applied_at: Option<u64>) -> String {
    let record = Record {
        version: RECORD_VERSION,
        scan_complete,
        holders: holders.iter().map(|(pallet, head)| Seen { pallet: pallet.clone(), head: head.clone() }).collect(),
        applied_at,
        never_applies: false,
        scan: 0,
        swept: false,
        recipe: false,
        recipe_deleted: false,
        deleted: false,
        lease_until: None,
    };

    serde_json::to_string(&record).expect("a record encodes")
}

impl<O: ObjectStore, R: RefStore> Head<O, R> {
    /// The same head, with every request's scans bounded to `steps` object reads in all, so a test
    /// can make a scan span requests.
    pub fn with_scan_step_limit(self, steps: usize) -> Head<O, R> {
        Head { scan_step_limit: Some(steps), ..self }
    }

    /// A fresh budget for one request.
    pub(super) fn scan_budget(&self) -> Budget {
        Budget { steps: self.scan_step_limit, deadline: self.deadline, lost: 0 }
    }

    /// What `@redactions` records at its live head; nothing while it is unborn.
    pub(super) fn recorded(&self) -> HeadResult<Arc<Recorded>> {
        let Some(head) = self
            .refs
            .get_head(PalletNamespace::Meta, REDACTIONS_PALLET_NAME)
            .map_err(HeadError::internal)?
            .live()
            .map(str::to_string)
        else {
            return Ok(Arc::new(Recorded::default()));
        };

        let cache = RECORDED.get_or_init(|| Mutex::new(HashMap::new()));

        if let Some(recorded) = cache.lock().map_err(|e| HeadError::internal(e.to_string()))?.get(&head) {
            return Ok(Arc::clone(recorded));
        }

        let scratch = self.scratch().map_err(HeadError::internal)?;
        let read = scratch
            .scoped(|| {
                materialize_state(&self.objects, &head, &mut Mirror::default())?;
                redaction_utils::read_redactions_at(&head)
            })
            .map_err(HeadError::internal)?;

        let recorded = Arc::new(Recorded {
            readable: read.redactions.into_iter()
                .map(|attributed| (attributed.redaction.hash.clone(), attributed.redaction))
                .collect(),
            unreadable: read.unreadable.into_iter().map(|unreadable| unreadable.hash).collect(),
        });

        cache.lock().map_err(|e| HeadError::internal(e.to_string()))?.insert(head, Arc::clone(&recorded));

        Ok(recorded)
    }

    /// Every hash `@redactions` records whose item says it applied: the file content a push gate
    /// tolerates the absence of. Read only when a closure walk has already found something
    /// missing, so its one item read per recorded redaction is never paid by a complete push.
    pub(super) fn applied_set(&self) -> Result<applied_redactions::AppliedSet, String> {
        let recorded = self.recorded().map_err(|error| error.to_string())?;
        let mut applied = Vec::new();

        // Only a redaction this build reads can have applied here: the judge never judges another.
        for hash in recorded.readable.keys() {
            if is_applied(&self.refs, hash)? {
                applied.push(hash.clone());
            }
        }

        Ok(applied_redactions::AppliedSet::of(applied))
    }

    /// Judge each of `hashes` that `@redactions` records, within one request's budget, and take
    /// every applied one out of this container's scratch. A hash it does not record is absent from
    /// the answer.
    pub(super) fn judge_hashes(&self, hashes: &[String]) -> HeadResult<HashMap<String, Verdict>> {
        let recorded = self.recorded()?;
        let mut budget = self.scan_budget();
        let mut listing = None;
        let mut verdicts = HashMap::new();

        for hash in hashes {
            if !recorded.contains(hash) || verdicts.contains_key(hash) {
                continue;
            }

            let verdict = match recorded.readable.get(hash) {
                Some(redaction) => self.judge(redaction, &mut budget, &mut listing)?,
                None => Verdict::Pending,
            };

            if matches!(verdict, Verdict::Applied { .. }) {
                self.evict(hash)?;
            }

            verdicts.insert(hash.clone(), verdict);
        }

        Ok(verdicts)
    }

    /// Refuse a request naming any of `hashes` whose redaction applied (`410`), or is undecided
    /// (`503`, with `Retry-After`). An applied one is refused ahead of an undecided one.
    pub(super) fn refuse_redacted(&self, hashes: &[String]) -> HeadResult<()> {
        let verdicts = self.judge_hashes(hashes)?;

        if let Some(hash) = hashes.iter().find(|hash| matches!(verdicts.get(*hash), Some(Verdict::Applied { .. }))) {
            return Err(HeadError::gone(applied_redactions::served_refusal(hash)));
        }

        for hash in hashes {
            if let Some(Verdict::Undecided(why)) = verdicts.get(hash) {
                return Err(undecided(hash, why));
            }
        }

        Ok(())
    }

    /// Refuse a lift commit naming a hash whose redaction applied, deleting the session's staged
    /// copy of each such content first, and its canonical object ([`delete_if_applied`]): it is
    /// never promoted, and a copy an earlier attempt of the commit promoted after the redaction's
    /// sweep is deleted too. Reads only the item, and applies nothing.
    pub(super) fn refuse_applied_staged(&self, session: &str, hashes: &[String]) -> HeadResult<()> {
        let recorded = self.recorded()?;
        let mut refused = None;

        for hash in hashes.iter().filter(|hash| recorded.contains(hash)) {
            if delete_if_applied(&self.objects, &self.refs, session, hash).map_err(HeadError::internal)?.applied() {
                self.evict(hash)?;
                refused.get_or_insert(hash);
            }
        }

        match refused {
            Some(hash) => Err(HeadError::gone(applied_redactions::served_refusal(hash))),
            None => Ok(()),
        }
    }

    /// After a lift commit promoted `promoted`, delete each one whose redaction applied while it
    /// was promoted, and refuse the commit as
    /// [`refuse_applied_staged`](Self::refuse_applied_staged) does. `promoted` includes what was
    /// already present: an earlier attempt of the same commit may have written it and ended
    /// before this check. `@redactions` is read after the promotions landed, so a redaction it
    /// does not record yet applies only after them, and its own deletion finds their objects.
    pub(super) fn refuse_applied_promoted(&self, session: &str, promoted: &[String]) -> HeadResult<()> {
        let recorded = self.recorded()?;
        let mut refused = None;

        for hash in promoted.iter().filter(|hash| recorded.contains(hash)) {
            if delete_if_applied(&self.objects, &self.refs, session, hash).map_err(HeadError::internal)?.applied() {
                self.evict(hash)?;
                refused.get_or_insert(hash);
            }
        }

        match refused {
            Some(hash) => Err(HeadError::gone(applied_redactions::served_refusal(hash))),
            None => Ok(()),
        }
    }

    /// Where every redaction `@redactions` records stands, for the handshake: each one judged
    /// first, within the request's budget, so an unapplied one may apply and an applied one runs
    /// the sweep it is owed.
    pub(super) fn served_redactions(&self) -> HeadResult<Vec<ServedRedaction>> {
        let recorded = self.recorded()?;
        let hashes = recorded.hashes();
        let verdicts = self.judge_hashes(&hashes)?;

        // A recipe the judges journaled starts its chunk round, and a running round goes on. Only a
        // recorded redaction can have journaled one.
        if !hashes.is_empty() {
            self.advance_round_logged();
        }

        Ok(hashes.into_iter().map(|hash| {
            let state = match verdicts.get(&hash) {
                Some(Verdict::Applied { deleted: true }) => ServedRedactionState::Deleted,
                Some(Verdict::Applied { deleted: false }) => ServedRedactionState::DeletionOwed,
                _ => ServedRedactionState::Pending,
            };

            ServedRedaction { hash, state }
        }).collect())
    }

    /// After a committed write: judge every recorded redaction, as best effort and within the
    /// request's budget, so one the write let apply applies now, one the write recorded starts its
    /// scan, and an applied one runs the sweep it is owed. A failure here is logged, never answered: the write committed,
    /// and the next request for the content judges it anyway.
    pub(super) fn judge_after_write(&self) {
        if let Err(error) = self.served_redactions() {
            eprintln!("forklift-aws-lambda: judging redactions after a write failed: {}", error.message);
        }
    }

    /// Take `hash` out of this container's scratch, where an earlier `batch` may have mirrored it.
    fn evict(&self, hash: &str) -> HeadResult<()> {
        if matches!(self.scratch, super::ScratchSource::Ephemeral) {
            return Ok(());
        }

        let scratch = self.scratch().map_err(HeadError::internal)?;

        scratch
            .scoped(|| {
                let (folder, file_name) = file_utils::get_path_for_object(hash)?;

                match std::fs::remove_file(std::path::Path::new(&folder).join(file_name)) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(format!("Error while evicting redacted object {} from the scratch: {}", hash, error)),
                }
            })
            .map_err(HeadError::internal)
    }

    /// Where `redaction` stands: read its item, start or advance its scan, check its holders, and
    /// apply it when nothing holds it.
    fn judge(&self, redaction: &Redaction, budget: &mut Budget, listing: &mut Option<RefListing>) -> HeadResult<Verdict> {
        let hash = &redaction.hash;

        let lost = budget.lost;

        while budget.lost - lost < WRITE_ATTEMPTS {
            let stored = self.refs.get_redaction(hash).map_err(HeadError::internal)?;
            let record = match &stored {
                Some(bytes) => Some(decode_record(hash, bytes).map_err(HeadError::internal)?),
                None => None,
            };

            let Some(record) = record else {
                // No item, so not applied: a request out of time leaves the start to another.
                if !budget.timely() {
                    return Ok(Verdict::Undecided(format!(
                        "The redaction of {} could not be judged within this request.",
                        hash
                    )));
                }

                self.start_scan(hash, budget)?;
                continue;
            };

            if let Some(applied_at) = record.applied_at {
                // The deletion is owed by whichever request reads it applied first and has the
                // time, and the late sweep by the first after it is due; a failed one leaves it
                // owed, and never changes the verdict.
                let late = epoch_seconds(SystemTime::now()) >= late_sweep_due(applied_at);
                let owed = !record.swept || (late && !record.deleted);

                if owed && budget.timely() {
                    match self.sweep(hash, stored.as_deref().unwrap_or_default(), &record, late, budget) {
                        Ok(deleted) => return Ok(Verdict::Applied { deleted }),
                        Err(error) => {
                            eprintln!("forklift-aws-lambda: the copies of applied redaction {} were not deleted: {}", hash, error);
                        }
                    }
                }

                return Ok(Verdict::Applied { deleted: record.done() });
            }

            if record.never_applies {
                return Ok(Verdict::Pending);
            }

            // The item read above is cheap and decides an applied redaction; only the scan and
            // the holder checks below wait on the time left.
            if !budget.timely() {
                return Ok(Verdict::Undecided(format!(
                    "The redaction of {} could not be judged within this request.",
                    hash
                )));
            }

            let stored = stored.unwrap_or_default();

            if !record.scan_complete {
                match self.advance_scan(redaction, &stored, &record, budget)? {
                    Advance::Moved => continue,
                    Advance::Stopped(why) => return Ok(Verdict::Undecided(why)),
                }
            }

            let pallets = match listing {
                Some(pallets) => pallets,
                None => listing.insert(self.refs.list_refs().map_err(HeadError::internal)?),
            };

            match self.holding(&record, pallets, budget)? {
                Holding::Held => return Ok(Verdict::Pending),
                Holding::Advanced { holder, head } => {
                    let advanced = Seen { pallet: holder.pallet.clone(), head };
                    let next = Record {
                        holders: record.holders.iter()
                            .map(|seen| if *seen == holder { advanced.clone() } else { seen.clone() })
                            .collect(),
                        ..record
                    };
                    // Only a saving for later judges: one lost to another request changes nothing
                    // about this verdict.
                    self.write_record(hash, &next, Some(&stored), budget)?;

                    return Ok(Verdict::Pending);
                }
                Holding::Stopped(why) => return Ok(Verdict::Undecided(why)),
                Holding::Rescan { holder, head } => {
                    let again = Seen { pallet: holder.pallet.clone(), head };
                    let scan = record.scan + 1;

                    let next = Record {
                        scan,
                        scan_complete: false,
                        holders: record.holders.iter().filter(|seen| **seen != holder).cloned().collect(),
                        ..record
                    };

                    // The list follows the record it belongs to, and only once that record has
                    // landed: a request whose rescan lost computed the same next scan, so a list
                    // it saved would pass for the winner's. One lost to a crash is listed again.
                    if self.write_record(hash, &next, Some(&stored), budget)? {
                        self.put_remaining(hash, scan, &[again])?;
                    }
                }
                Holding::Free => {
                    let kept = match self.kept_as(hash)? {
                        Ok(kept) => kept,
                        Err(verdict) => {
                            if verdict == Verdict::Pending {
                                // Kept, so no later judge lists the pallets or reads the object
                                // again; one lost to another request changes nothing about this
                                // verdict.
                                let next = Record { never_applies: true, ..record };
                                self.write_record(hash, &next, Some(&stored), budget)?;
                            }

                            return Ok(verdict);
                        }
                    };

                    let applied_at = epoch_seconds(SystemTime::now());
                    let recipe = kept == Some(ObjectType::Recipe);
                    let next = Record { applied_at: Some(applied_at), recipe, ..record };
                    self.write_record_journaling(hash, &next, Some(&stored), recipe, budget)?;
                }
            }
        }

        Ok(Verdict::Undecided(format!(
            "Other requests kept changing the redaction item for {} while this one judged it.",
            hash
        )))
    }

    /// Start the scan of `hash`: list every pallet item, save the list as the pallets to walk, and
    /// create the item. The caller has read a `@redactions` head recording it, so every head the
    /// listing shows was written after the redaction could refuse it, or is judged by the walk.
    fn start_scan(&self, hash: &str, budget: &mut Budget) -> HeadResult<()> {
        let listing = self.refs.list_refs().map_err(HeadError::internal)?;
        let pallets: Vec<Seen> = listing.live.iter().chain(listing.tombstones.iter())
            .map(|(pallet, head)| Seen { pallet: pallet.to_wire(), head: head.clone() })
            .collect();

        self.put_remaining(hash, 0, &pallets)?;

        let record = Record { version: RECORD_VERSION, scan_complete: false, holders: Vec::new(), applied_at: None, never_applies: false, scan: 0, swept: false, recipe: false, recipe_deleted: false, deleted: false, lease_until: None };
        // Lost to another request's start: the judge reads its item instead.
        self.write_record(hash, &record, None, budget).map(|_| ())
    }

    /// Walk the scan of `redaction` on from where it stopped, within `budget`.
    fn advance_scan(&self, redaction: &Redaction, stored: &str, record: &Record, budget: &mut Budget) -> HeadResult<Advance> {
        let hash = &redaction.hash;

        let remaining = match self.objects.work_get(&remaining_name(hash)).map_err(HeadError::internal)? {
            Some(bytes) => decode_work::<Remaining>(&bytes).filter(|remaining| remaining.scan == record.scan),
            None => None,
        };

        // A list is missing while a rescan's record has landed and its list is not yet saved, or
        // once lost to a crash; one of another scan was saved over this one's by a request that
        // read an older record. Either way, list again: every pallet the listing adds was written
        // after the redaction was recorded, so walking it is only extra work.
        let Some(mut remaining) = remaining else {
            let listing = self.refs.list_refs().map_err(HeadError::internal)?;
            let pallets: Vec<Seen> = listing.live.iter().chain(listing.tombstones.iter())
                .map(|(pallet, head)| Seen { pallet: pallet.to_wire(), head: head.clone() })
                .collect();
            self.put_remaining(hash, record.scan, &pallets)?;

            return Ok(Advance::Moved);
        };

        let scratch = self.scratch().map_err(HeadError::internal)?;

        // Pallets walked off `remaining` since it was last saved. The list is saved again once,
        // on every way out of the walk, an error's included, so what this request walked is kept.
        let mut walked = false;
        let outcome = self.walk_remaining(&scratch, redaction, stored, record, budget, &mut remaining.pallets, &mut walked);

        if walked {
            let saved = self.put_remaining(hash, record.scan, &remaining.pallets);
            return outcome.and_then(|advance| saved.map(|()| advance));
        }

        outcome
    }

    /// Walk the pallets of `remaining` in turn, taking each walked one off it and setting
    /// `walked`, until one names the content, the walks stop, or none is left and the scan is
    /// recorded complete. The caller saves `remaining` when `walked` is set.
    #[allow(clippy::too_many_arguments)]
    fn walk_remaining(
        &self,
        scratch: &Scratch,
        redaction: &Redaction,
        stored: &str,
        record: &Record,
        budget: &mut Budget,
        remaining: &mut Vec<Seen>,
        walked: &mut bool,
    ) -> HeadResult<Advance> {
        let hash = &redaction.hash;

        loop {
            let Some(seen) = remaining.first().cloned() else {
                let next = Record { scan_complete: true, ..record.clone() };

                if self.write_record(hash, &next, Some(stored), budget)? {
                    // The list goes with the scan it served; nothing is left to save.
                    *walked = false;
                    self.objects.work_delete(&remaining_name(hash)).map_err(HeadError::internal)?;
                }

                return Ok(Advance::Moved);
            };

            let (walk, resumed) = self.walk_pallet(scratch, redaction, &seen, budget)?;

            let names = match walk {
                Ok(Some(names)) => names,
                Ok(None) => return Ok(Advance::Stopped(format!(
                    "The scan of the pallets that may hold the redaction of {} has not finished yet.",
                    hash
                ))),
                Err(why) => return Ok(Advance::Stopped(why)),
            };

            if names {
                let mut holders = record.holders.clone();
                if !holders.contains(&seen) {
                    holders.push(seen.clone());
                }

                let next = Record { holders, ..record.clone() };

                if !self.write_record(hash, &next, Some(stored), budget)? {
                    return Ok(Advance::Moved);
                }
            }

            remaining.retain(|pallet| *pallet != seen);
            *walked = true;

            if resumed {
                self.objects.work_delete(&walk_name(hash, &seen)).map_err(HeadError::internal)?;
            }

            if names {
                return Ok(Advance::Moved);
            }
        }
    }

    /// Walk `seen` for `redaction` from its saved walk, within `budget`, saving the walk again
    /// when it stops part way.
    ///
    /// # Returns
    /// How the walk went, beside whether it resumed a saved walk:
    /// * `Ok(Some(names))`, the walk finished: whether the pallet names the content.
    /// * `Ok(None)`, the budget ran out first.
    /// * `Err(why)`, an object the walk needs could not be read.
    ///
    /// `Err(HeadError)` when a work object could not be read or written.
    fn walk_pallet(
        &self,
        scratch: &Scratch,
        redaction: &Redaction,
        seen: &Seen,
        budget: &mut Budget,
    ) -> HeadResult<(Result<Option<bool>, String>, bool)> {
        let name = walk_name(&redaction.hash, seen);
        let meta = PalletRef::parse(&seen.pallet).map_err(HeadError::internal)?.namespace == PalletNamespace::Meta;

        let saved = self.objects.work_get(&name).map_err(HeadError::internal)?;
        let resumed = saved.is_some();

        let mut walk = saved
            .and_then(|bytes| decode_work::<SavedWalk>(&bytes).map(|saved| saved.walk))
            .unwrap_or_else(|| match meta {
            true => HoldingWalk::of_history(&seen.head),
            false => HoldingWalk::of_tree(&seen.head),
        });

        let stepped: Result<Option<bool>, String> = scratch.scoped(|| {
            let objects = MirroredObjects::new(&self.objects);

            loop {
                if !budget.spend() {
                    return Ok(None);
                }

                match walk.step(redaction, &objects)? {
                    WalkProgress::Names => return Ok(Some(true)),
                    WalkProgress::Clear => return Ok(Some(false)),
                    WalkProgress::Going => {}
                }
            }
        });

        match stepped {
            Ok(Some(names)) => Ok((Ok(Some(names)), resumed)),
            stopped => {
                let saved = serde_json::to_vec(&SavedWalk { version: WORK_VERSION, walk })
                    .map_err(|error| HeadError::internal(format!("encoding a redaction walk failed: {}", error)))?;
                self.objects.work_put(&name, &saved).map_err(HeadError::internal)?;

                let walk = stopped.map(|_| None).map_err(|why| format!(
                    "Whether pallet {} names the content of redaction {} cannot be told yet: {}",
                    seen.pallet, redaction.hash, why
                ));

                Ok((walk, true))
            }
        }
    }

    /// What a complete scan's holders say against `listing`, the pallet items now.
    fn holding(&self, record: &Record, listing: &RefListing, budget: &mut Budget) -> HeadResult<Holding> {
        for holder in &record.holders {
            let pallet = PalletRef::parse(&holder.pallet).map_err(HeadError::internal)?;
            let carries = |heads: &[(PalletRef, String)]| heads.iter().any(|(each, head)| *each == pallet && *head == holder.head);

            if pallet.namespace == PalletNamespace::User {
                if carries(&listing.live) || carries(&listing.tombstones) {
                    return Ok(Holding::Held);
                }
                continue;
            }

            let Some((_, current)) = listing.live.iter().find(|(each, _)| *each == pallet) else {
                continue;
            };

            if *current == holder.head {
                return Ok(Holding::Held);
            }

            match self.descends(current, &holder.head, budget)? {
                Ok(Some(true)) => return Ok(Holding::Advanced { holder: holder.clone(), head: current.clone() }),
                Ok(Some(false)) => return Ok(Holding::Rescan { holder: holder.clone(), head: current.clone() }),
                Ok(None) => return Ok(Holding::Stopped(format!(
                    "Whether meta pallet {} still holds a redaction has not been checked yet.",
                    holder.pallet
                ))),
                Err(why) => return Ok(Holding::Stopped(why)),
            }
        }

        Ok(Holding::Free)
    }

    /// Whether `head` descends from `ancestor`, walking parents within `budget`.
    ///
    /// # Returns
    /// * `Ok(Ok(Some(bool)))` - Whether it does.
    /// * `Ok(Ok(None))`       - The budget ran out first.
    /// * `Ok(Err(why))`       - A parcel could not be read.
    fn descends(&self, head: &str, ancestor: &str, budget: &mut Budget) -> HeadResult<Result<Option<bool>, String>> {
        let scratch = self.scratch().map_err(HeadError::internal)?;

        Ok(scratch.scoped(|| {
            let objects = MirroredObjects::new(&self.objects);
            let mut seen: HashSet<String> = HashSet::new();
            let mut pending = vec![head.to_string()];

            while let Some(parcel) = pending.pop() {
                if parcel == ancestor {
                    return Ok(Some(true));
                }

                if !seen.insert(parcel.clone()) {
                    continue;
                }

                if !budget.spend() {
                    return Ok(None);
                }

                pending.extend(objects.load_parcel(&parcel)?.parents);
            }

            Ok(Some(false))
        }))
    }

    /// How the store keeps `hash`, which nothing holds, when its redaction may apply: as a blob, a
    /// recipe, or not at all. A blob is below the chunking threshold, and a recipe a list of
    /// chunks, so reading the whole object stays small.
    ///
    /// # Returns
    /// * `Ok(Err(Verdict))` - What keeps it from applying: `Pending` when the store holds it as no
    ///                        file's content (a tree, a parcel or a chunk, which other content may
    ///                        share), so it never applies; `Undecided` when its header cannot be
    ///                        read.
    fn kept_as(&self, hash: &str) -> HeadResult<Result<Option<ObjectType>, Verdict>> {
        match stored_type(&self.objects, hash) {
            Ok(kept @ (None | Some(ObjectType::Blob | ObjectType::Recipe))) => Ok(Ok(kept)),
            Ok(Some(_)) => Ok(Err(Verdict::Pending)),
            Err(error) => Ok(Err(Verdict::Undecided(error))),
        }
    }

    /// Delete the copies the store keeps of `hash`, applied as `record` (stored as `stored`):
    /// every staged copy, and a blob's canonical object. A recipe's object stays until the chunk
    /// ledger has deleted its chunks, and the write that first finds it a recipe journals it
    /// there. The `late` sweep, once [`late_sweep_due`], deletes them again, catching a staged
    /// upload presigned before the apply that landed after the first sweep, and also every
    /// response bundle written before [`response_cutoff`] and the scan's work objects; for a blob,
    /// it ends the deletion, and for a recipe, so does deleting its object. Run under a lease the item holds, so two
    /// requests reading the redaction applied at once delete once; the lease outlasts this
    /// request, so one that dies holding it leaves the deletion to the first request after it
    /// ends.
    ///
    /// # Returns
    /// * `Ok(bool)`       - Whether the deletion is done; `false` too while another request holds
    ///                      the lease.
    /// * `Err(HeadError)` - A read or a delete failed: the deletion stays owed.
    fn sweep(&self, hash: &str, stored: &str, record: &Record, late: bool, budget: &mut Budget) -> HeadResult<bool> {
        let now = epoch_seconds(SystemTime::now());

        if record.lease_until.is_some_and(|until| until > now) {
            return Ok(record.deleted);
        }

        let until = budget.deadline.map_or(now + INVOCATION_CEILING, epoch_seconds) + SWEEP_LEASE_MARGIN;
        let leased = Record { lease_until: Some(until), ..record.clone() };

        if !self.write_record(hash, &leased, Some(stored), budget)? {
            return Ok(record.deleted);
        }
        let stored = encode_record(&leased)?;

        let swept = self.delete_copies(hash, late.then_some(leased.applied_at.unwrap_or_default()));
        let found_recipe = matches!(swept, Ok(Some(ObjectType::Recipe))) && !leased.recipe;
        let next = match &swept {
            Ok(_) => Record {
                swept: true,
                recipe: leased.recipe || found_recipe,
                deleted: late,
                lease_until: None,
                ..leased
            },
            Err(_) => Record { lease_until: None, ..leased },
        };
        // Lost, the lease simply runs out and the next request after it deletes again. Until
        // then the item says what it said, so this request reports that too.
        let written = self.write_record_journaling(hash, &next, Some(&stored), found_recipe, budget)?;

        swept.map(|_| if written { next.done() } else { record.done() })
    }

    /// Delete every staged copy of `hash`, and its canonical object unless the store holds it as
    /// a recipe. An object that cannot be read is kept: the deletion stays owed. For the late
    /// sweep of a redaction that applied at `late`, delete too every response bundle written
    /// before [`response_cutoff`], and the scan's work objects: no request still walking for it
    /// can outlive the wait before the late sweep.
    ///
    /// # Returns
    /// * `Ok(Option<ObjectType>)` - The type the store held the canonical object as before the
    ///                              deletion; `None` when it held none.
    fn delete_copies(&self, hash: &str, late: Option<u64>) -> HeadResult<Option<ObjectType>> {
        self.objects.delete_staged(hash).map_err(HeadError::internal)?;

        let stored = stored_type(&self.objects, hash).map_err(HeadError::internal)?;
        if stored != Some(ObjectType::Recipe) {
            self.objects.delete_canonical(hash).map_err(HeadError::internal)?;
        }

        if let Some(applied_at) = late {
            let cutoff = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(response_cutoff(applied_at));
            self.objects.delete_responses_before(cutoff).map_err(HeadError::internal)?;
            self.objects.work_delete_under(&work_prefix(hash)).map_err(HeadError::internal)?;
        }

        Ok(stored)
    }

    /// Save `pallets` as the pallets the scan of `hash` has still to walk.
    fn put_remaining(&self, hash: &str, scan: u64, pallets: &[Seen]) -> HeadResult<()> {
        let bytes = serde_json::to_vec(&Remaining { version: WORK_VERSION, scan, pallets: pallets.to_vec() })
            .map_err(|error| HeadError::internal(format!("encoding a redaction scan failed: {}", error)))?;

        self.objects.work_put(&remaining_name(hash), &bytes).map_err(HeadError::internal)
    }

    /// Write `record` as the item of `hash`, on condition that it still holds `expected`.
    ///
    /// # Returns
    /// * `Ok(true)`       - It was written.
    /// * `Ok(false)`      - The item moved, or the write's fate is unknown: read it again.
    /// * `Err(HeadError)` - The write failed, or the warehouse's format moved to one this build
    ///                      does not serve.
    fn write_record(&self, hash: &str, record: &Record, expected: Option<&str>, budget: &mut Budget) -> HeadResult<bool> {
        self.write_record_journaling(hash, record, expected, false, budget)
    }

    /// [`write_record`](Self::write_record), journaling the content on the chunk ledger in the
    /// same transaction when `journal` is set: the write that first records the content is a
    /// recipe, so no record says so while the ledger lacks it.
    fn write_record_journaling(
        &self,
        hash: &str,
        record: &Record,
        expected: Option<&str>,
        journal: bool,
        budget: &mut Budget,
    ) -> HeadResult<bool> {
        let state = encode_record(record)?;
        let mut writes = vec![ItemWrite { key: ItemKey::Redaction(hash), state: &state, expected }];

        let ledger = match journal {
            true => Some(super::chunks::journal(&self.refs, hash, record.applied_at.unwrap_or_default()).map_err(HeadError::internal)?),
            false => None,
        };
        if let Some((expected, state)) = &ledger {
            writes.push(ItemWrite { key: ItemKey::ChunkLedger, state, expected: expected.as_deref() });
        }

        self.write_items(&writes, budget)
    }

    /// Write every one of `writes` in one transaction, each on condition that its item still
    /// holds what it expects.
    ///
    /// # Returns
    /// * `Ok(true)`       - They were written.
    /// * `Ok(false)`      - An item moved, or the write's fate is unknown: read again.
    /// * `Err(HeadError)` - The write failed, or the warehouse's format moved to one this build
    ///                      does not serve.
    pub(super) fn write_items(&self, writes: &[ItemWrite<'_>], budget: &mut Budget) -> HeadResult<bool> {
        let outcome = crate::format::fenced(
            &self.refs,
            |fence| self.refs.put_items(writes, fence).map_err(HeadError::internal),
            |outcome| *outcome == RedactionWrite::FormatMoved,
        )?;

        let written = outcome == RedactionWrite::Written;
        if !written {
            budget.lost += 1;
        }

        Ok(written)
    }

    /// Record on the item of `hash` that its recipe object has been deleted.
    ///
    /// # Returns
    /// * `Ok(bool)` - Whether the item records it now.
    pub(super) fn mark_recipe_deleted(&self, hash: &str, budget: &mut Budget) -> HeadResult<bool> {
        for _ in 0..WRITE_ATTEMPTS {
            let Some(stored) = self.refs.get_redaction(hash).map_err(HeadError::internal)? else {
                return Err(HeadError::internal(format!("The redaction item for journaled recipe {} is missing.", hash)));
            };
            let record = decode_record(hash, &stored).map_err(HeadError::internal)?;

            if record.recipe_deleted {
                return Ok(true);
            }

            let next = Record { recipe_deleted: true, ..record };
            if self.write_record(hash, &next, Some(&stored), budget)? {
                return Ok(true);
            }
        }

        Ok(false)
    }
}

/// The `503` a request for `hash` answers while its redaction is undecided, for `why`.
fn undecided(hash: &str, why: &str) -> HeadError {
    HeadError::retry_after(
        format!(
            "Whether object {} is redacted in this warehouse is not decided yet ({}), so it is not \
            served. Try again shortly.",
            hash,
            why.trim_end_matches('.')
        ),
        UNDECIDED_RETRY_AFTER_SECONDS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bundle written up to the Lambda ceiling after the apply, and a moment past it on a clock
    /// that disagrees, is swept; the sweep waits a URL's lifetime past that, and a moment more.
    // Revert: drop the margin from the cutoff → red.
    #[test]
    fn the_late_sweeps_bounds_keep_a_margin_past_each_wait() {
        let applied_at = 1_000_000;
        // The Lambda ceiling and the presigned URL TTL are 900 s each, with 60 s past each.
        assert!(response_cutoff(applied_at) >= applied_at + 900 + 60);
        assert!(late_sweep_due(applied_at) >= response_cutoff(applied_at) + 900 + 60);
    }

    #[test]
    fn a_record_of_another_version_is_refused_never_read_as_pending() {
        let hash = "a".repeat(64);

        for stored in [
            r#"{"version":3,"scan_complete":true,"holders":[],"applied_at":null}"#,
            r#"{"version":1,"scan_complete":true,"holders":[],"applied_at":null}"#,
            r#"{"scan_complete":true,"holders":[],"applied_at":null}"#,
            "not json",
        ] {
            assert!(decode_record(&hash, stored).is_err(), "{} was read", stored);
        }

        let current = r#"{"version":2,"scan_complete":true,"holders":[],"applied_at":7}"#;
        assert_eq!(decode_record(&hash, current).unwrap().applied_at, Some(7));
    }

    #[test]
    fn a_record_with_a_field_this_build_does_not_know_is_refused() {
        let stored = r#"{"version":2,"scan_complete":true,"holders":[],"applied_at":null,"purged":true}"#;

        assert!(decode_record(&"a".repeat(64), stored).is_err());
    }

    #[test]
    fn a_work_object_of_another_version_is_dropped() {
        let other = br#"{"version":9,"scan":0,"pallets":[]}"#;
        let current = br#"{"version":1,"scan":0,"pallets":[{"pallet":"main","head":"h"}]}"#;

        assert!(decode_work::<Remaining>(other).is_none());
        assert_eq!(decode_work::<Remaining>(current).unwrap().pallets.len(), 1);
    }

    #[test]
    fn a_walk_is_keyed_by_its_pallet_and_its_head() {
        let hash = "a".repeat(64);
        let a = Seen { pallet: "main".into(), head: "h1".into() };
        let b = Seen { pallet: "other".into(), head: "h1".into() };
        let c = Seen { pallet: "main".into(), head: "h2".into() };

        let names: HashSet<String> = [&a, &b, &c].iter().map(|seen| walk_name(&hash, seen)).collect();
        assert_eq!(names.len(), 3);
        assert!(names.iter().all(|name| name.starts_with(&format!("{}/walk-", hash))));
    }
}
