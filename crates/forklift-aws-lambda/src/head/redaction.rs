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
use crate::store::{ObjectStore, RedactionWrite, RefListing, RefStore};

/// The version of the redaction item's record this build writes and reads.
const RECORD_VERSION: u64 = 1;

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
    /// Applied: refused on every route.
    Applied,

    /// Pending: something still holds it, a cell of it cannot be read, or it names no file's
    /// content. Served.
    Pending,

    /// Not decided within this request, for this reason. Not served.
    Undecided(String),
}

/// What one request may still spend advancing scans: object reads, when a test bounds them, and
/// the invocation's time less `REDACTION_ANSWER_RESERVE`.
struct Budget {
    steps: Option<usize>,
    deadline: Option<SystemTime>,

    /// How many item writes this request has lost to another, or left with their fate unknown.
    lost: usize,
}

impl Budget {
    /// Whether the invocation's time, less `REDACTION_ANSWER_RESERVE`, is not yet spent.
    fn timely(&self) -> bool {
        self.deadline.is_none_or(|deadline| {
            deadline.duration_since(SystemTime::now()).unwrap_or_default() > super::REDACTION_ANSWER_RESERVE
        })
    }

    /// Whether another step is affordable; spends it when it is.
    fn spend(&mut self) -> bool {
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

/// The name of the work object listing the pallets the scan of `hash` has still to walk.
fn remaining_name(hash: &str) -> String {
    format!("{}/pallets", hash)
}

/// The name of the work object holding the walk of `seen` for the redaction of `hash`.
fn walk_name(hash: &str, seen: &Seen) -> String {
    format!("{}/walk-{}", hash, object_utils::hash_object_bytes(format!("{}\n{}", seen.pallet, seen.head).as_bytes()))
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

/// Whether the redaction of `hash` has applied, by its item in `refs`: what the staging verifier
/// and a lift commit read before they promote a staged copy.
///
/// # Returns
/// * `Ok(bool)`    - Whether the item records it applied; `false` when there is no item.
/// * `Err(String)` - The item could not be read, or is of a version this build does not read.
pub fn is_applied<R: RefStore + ?Sized>(refs: &R, hash: &str) -> Result<bool, String> {
    match refs.get_redaction(hash)? {
        Some(stored) => Ok(decode_record(hash, &stored)?.applied_at.is_some()),
        None => Ok(false),
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
    fn scan_budget(&self) -> Budget {
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

            if verdict == Verdict::Applied {
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

        if let Some(hash) = hashes.iter().find(|hash| verdicts.get(*hash) == Some(&Verdict::Applied)) {
            return Err(HeadError::gone(applied_redactions::served_refusal(hash)));
        }

        for hash in hashes {
            if let Some(Verdict::Undecided(why)) = verdicts.get(hash) {
                return Err(undecided(hash, why));
            }
        }

        Ok(())
    }

    /// Refuse a lift commit naming a hash whose redaction applied, dropping each such staged copy
    /// first: it is never promoted. Reads only the item, and applies nothing.
    pub(super) fn refuse_applied_staged(&self, session: &str, hashes: &[String]) -> HeadResult<()> {
        let recorded = self.recorded()?;
        let mut refused = None;

        for hash in hashes.iter().filter(|hash| recorded.contains(hash)) {
            if is_applied(&self.refs, hash).map_err(HeadError::internal)? {
                self.objects.drop_staged_copy(session, hash).map_err(HeadError::internal)?;
                self.evict(hash)?;
                refused.get_or_insert(hash);
            }
        }

        match refused {
            Some(hash) => Err(HeadError::gone(applied_redactions::served_refusal(hash))),
            None => Ok(()),
        }
    }

    /// Where every redaction `@redactions` records stands, for the handshake: each unapplied one
    /// judged first, within the request's budget.
    pub(super) fn served_redactions(&self) -> HeadResult<Vec<ServedRedaction>> {
        let recorded = self.recorded()?;
        let hashes = recorded.hashes();
        let verdicts = self.judge_hashes(&hashes)?;

        Ok(hashes.into_iter().map(|hash| {
            let state = match verdicts.get(&hash) {
                Some(Verdict::Applied) => ServedRedactionState::DeletionOwed,
                _ => ServedRedactionState::Pending,
            };

            ServedRedaction { hash, state }
        }).collect())
    }

    /// After a committed write: judge every recorded, unapplied redaction, as best effort and
    /// within the request's budget, so one the write let apply applies now, and one the write
    /// recorded starts its scan. A failure here is logged, never answered: the write committed,
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
                self.start_scan(hash, budget)?;
                continue;
            };

            if record.applied_at.is_some() {
                return Ok(Verdict::Applied);
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
                    self.put_remaining(hash, &[again])?;

                    let next = Record {
                        scan_complete: false,
                        holders: record.holders.iter().filter(|seen| **seen != holder).cloned().collect(),
                        ..record
                    };
                    self.write_record(hash, &next, Some(&stored), budget)?;
                }
                Holding::Free => {
                    if let Some(verdict) = self.never_applies(hash)? {
                        if verdict == Verdict::Pending {
                            // Kept, so no later judge lists the pallets or reads the object again;
                            // one lost to another request changes nothing about this verdict.
                            let next = Record { never_applies: true, ..record };
                            self.write_record(hash, &next, Some(&stored), budget)?;
                        }

                        return Ok(verdict);
                    }

                    let applied_at = SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .map_or(0, |since| since.as_secs());
                    let next = Record { applied_at: Some(applied_at), ..record };
                    self.write_record(hash, &next, Some(&stored), budget)?;
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

        self.put_remaining(hash, &pallets)?;

        let record = Record { version: RECORD_VERSION, scan_complete: false, holders: Vec::new(), applied_at: None, never_applies: false };
        // Lost to another request's start: the judge reads its item instead.
        self.write_record(hash, &record, None, budget).map(|_| ())
    }

    /// Walk the scan of `redaction` on from where it stopped, within `budget`.
    fn advance_scan(&self, redaction: &Redaction, stored: &str, record: &Record, budget: &mut Budget) -> HeadResult<Advance> {
        let hash = &redaction.hash;

        let remaining = match self.objects.work_get(&remaining_name(hash)).map_err(HeadError::internal)? {
            Some(bytes) => decode_work::<Remaining>(&bytes),
            None => None,
        };

        // The list is written before the item that needs it, and removed only after the item
        // records the scan complete, so a missing one was lost to a crash or a race: list again.
        // Every pallet it adds was written after the redaction was recorded, so walking it is
        // only extra work.
        let Some(mut remaining) = remaining else {
            let listing = self.refs.list_refs().map_err(HeadError::internal)?;
            let pallets: Vec<Seen> = listing.live.iter().chain(listing.tombstones.iter())
                .map(|(pallet, head)| Seen { pallet: pallet.to_wire(), head: head.clone() })
                .collect();
            self.put_remaining(hash, &pallets)?;

            return Ok(Advance::Moved);
        };

        let scratch = self.scratch().map_err(HeadError::internal)?;

        // Pallets walked off `remaining` since it was last saved. The list is saved again once,
        // on every way out of the walk, an error's included, so what this request walked is kept.
        let mut walked = false;
        let outcome = self.walk_remaining(&scratch, redaction, stored, record, budget, &mut remaining.pallets, &mut walked);

        if walked {
            let saved = self.put_remaining(hash, &remaining.pallets);
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

    /// What keeps the redaction of `hash` from applying though nothing holds it: `Pending` when the
    /// store holds it as no file's content (a tree, a parcel or a chunk, which other content may
    /// share), so it never applies; `Undecided` when its header cannot be read. `None` when the
    /// store holds it as a blob or a recipe, or not at all. A blob is below the chunking
    /// threshold, and a recipe a list of chunks, so reading the whole object stays small.
    fn never_applies(&self, hash: &str) -> HeadResult<Option<Verdict>> {
        let Some(bytes) = self.objects.get(hash).map_err(HeadError::internal)? else {
            return Ok(None);
        };

        match object_utils::peek_object_header(&bytes) {
            Ok((ObjectType::Blob | ObjectType::Recipe, _)) => Ok(None),
            Ok(_) => Ok(Some(Verdict::Pending)),
            Err(error) => Ok(Some(Verdict::Undecided(format!("Object {} cannot be read: {:?}", hash, error)))),
        }
    }

    /// Save `pallets` as the pallets the scan of `hash` has still to walk.
    fn put_remaining(&self, hash: &str, pallets: &[Seen]) -> HeadResult<()> {
        let bytes = serde_json::to_vec(&Remaining { version: WORK_VERSION, pallets: pallets.to_vec() })
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
        let state = encode_record(record)?;

        let outcome = crate::format::fenced(
            &self.refs,
            |fence| self.refs.put_redaction(hash, &state, expected, fence).map_err(HeadError::internal),
            |outcome| *outcome == RedactionWrite::FormatMoved,
        )?;

        let written = outcome == RedactionWrite::Written;
        if !written {
            budget.lost += 1;
        }

        Ok(written)
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

    #[test]
    fn a_record_of_another_version_is_refused_never_read_as_pending() {
        let hash = "a".repeat(64);

        for stored in [
            r#"{"version":2,"scan_complete":true,"holders":[],"applied_at":null}"#,
            r#"{"scan_complete":true,"holders":[],"applied_at":null}"#,
            "not json",
        ] {
            assert!(decode_record(&hash, stored).is_err(), "{} was read", stored);
        }

        let current = r#"{"version":1,"scan_complete":true,"holders":[],"applied_at":7}"#;
        assert_eq!(decode_record(&hash, current).unwrap().applied_at, Some(7));
    }

    #[test]
    fn a_record_with_a_field_this_build_does_not_know_is_refused() {
        let stored = r#"{"version":1,"scan_complete":true,"holders":[],"applied_at":null,"deleted":true}"#;

        assert!(decode_record(&"a".repeat(64), stored).is_err());
    }

    #[test]
    fn a_work_object_of_another_version_is_dropped() {
        let other = br#"{"version":9,"pallets":[]}"#;
        let current = br#"{"version":1,"pallets":[{"pallet":"main","head":"h"}]}"#;

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
