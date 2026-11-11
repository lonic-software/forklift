//! Withholding on the AWS head: what a request reads of the withheld items (`crate::withheld`),
//! and how it answers content its host withholds, as the served root does.
//!
//! - Object `GET` and `PUT` answer `451 content_withheld` with the host's statement. An applied
//!   redaction of the same hash wins, since it is for good, and is answered `410`.
//! - `upload-targets` answers it in its withheld list, `missing` never lists it, and `batch`
//!   leaves it out.
//! - The handshake's withheld list names each one with its statement.
//! - A ref update tolerates its absence as a file's content, and a first anchor's adopted history
//!   does not tolerate it at all; either refusal says the host withholds it.
//! - A lift commit naming it is refused `451`, and its staged copy deleted, never promoted.
//!
//! An item applies from the moment it is added. It never applies when the store holds its hash
//! as no file's content (a tree, a parcel or a chunk); a request learns which by settling it
//! ([`Head::withholding`]), reading the object once and recording what it is; one whose header
//! this build cannot read is never recorded, so it applies and is read again by every request.
//! Settling a recipe journals it in the chunk ledger in the same transaction, which is what gates
//! its chunks (the chunk gate reads only the ledger, and a chunk request names no recipe). So
//! every request that serves or takes objects settles every unsettled item before it answers, and
//! answers `503` only when that does not finish within the request.
//!
//! After settling, such a request with time to spare runs one owed deletion step, never changing
//! its answer ([`Head::withheld_deletion_step`]): staged copies at once, and once the late sweep
//! is due, a blob's canonical copy unless meta or adopted history names it, which a resumable walk
//! decides. Until that walk records it kept, a meta pallet's ref update refuses to newly name it
//! ([`Head::refuse_meta_naming_withheld`]). The handshake reports an item deleted only once
//! every copy is gone, so a kept one reports its deletion owed for good.
//!
//! The cost: one strongly consistent `Query` per such request, reading every item whole. An item
//! is its statement (at most 1000 characters) and a few flags, so a strongly consistent read
//! unit (4 KB) covers about four; a warehouse whose host withholds nothing pays the smallest
//! `Query` there is. Kept as one listing rather than split into a statement item and a progress
//! item, because the split saves bytes, not the round trip, and only pays once a host withholds
//! hundreds of hashes.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use forklift_core::model::remote::{ServedWithholding, ServedWithholdingState};
use forklift_core::enums::object_type::ObjectType;
use forklift_core::util::meta_tree;
use forklift_core::util::object_utils;
use forklift_core::util::pallet_utils::PalletNamespace;
use forklift_core::util::redaction_utils::{HoldingWalk, WalkObjects, WalkProgress};
use forklift_core::util::withholding::{self, WithheldSet};

use super::redaction::{
    decode_work, epoch_seconds, late_sweep_due, response_cutoff, Budget, INVOCATION_CEILING, SWEEP_LEASE_MARGIN,
    UNDECIDED_RETRY_AFTER_SECONDS, WORK_VERSION, WRITE_ATTEMPTS,
};
use super::Head;
use crate::error::{HeadError, HeadResult};
use crate::scratch::MirroredObjects;
use crate::store::{ItemKey, ItemWrite, ObjectStore, RefStore};
use crate::withheld::{self, Listed, WithheldItem};

/// Every withheld item of a warehouse, each settled but for one whose object's header cannot be
/// read: what one request read.
#[derive(Debug, Clone, Default)]
pub(super) struct Withholding {
    items: Vec<Listed>,
}

impl Withholding {
    /// The content this head withholds, each hash beside its statement: every item that applies.
    pub(super) fn applying(&self) -> WithheldSet {
        applying(&self.items)
    }

    /// Whether the host withholds `hash`: the listing holds an item of it that applies.
    pub(super) fn withholds(&self, hash: &str) -> bool {
        self.items.iter().any(|listed| listed.hash == hash && !listed.item.never_applies)
    }

    /// The `451` for the first of `hashes` this head withholds, if any.
    pub(super) fn refusal(&self, hashes: &[String]) -> Option<HeadError> {
        hashes.iter().find_map(|hash| {
            self.items.iter()
                .find(|listed| listed.hash == *hash && !listed.item.never_applies)
                .map(|listed| HeadError::withheld(withholding::served_refusal(hash, &listed.item.statement)))
        })
    }

    /// Whether any item is a recipe not deleted yet, whose chunks the ledger may owe a round.
    pub(super) fn has_recipe(&self) -> bool {
        self.items.iter().any(|listed| listed.item.recipe && !listed.item.recipe_deleted && !listed.item.never_applies)
    }

    /// What the handshake reports: each item that applies, with its statement, its deletion owed
    /// until every copy of the content is deleted.
    pub(super) fn served(&self) -> Vec<ServedWithholding> {
        self.items.iter().filter(|listed| !listed.item.never_applies).map(|listed| ServedWithholding {
            hash: listed.hash.clone(),
            statement: listed.item.statement.clone(),
            state: match deleted(&listed.item) {
                true => ServedWithholdingState::Deleted,
                false => ServedWithholdingState::DeletionOwed,
            },
            // The AWS head records no chunk-list digest yet.
            chunks: None,
        }).collect()
    }
}

/// The content `items` withhold, each hash beside its statement: every item that applies, settled
/// or not.
pub(super) fn applying(items: &[Listed]) -> WithheldSet {
    WithheldSet::of(items.iter()
        .filter(|listed| !listed.item.never_applies)
        .map(|listed| (listed.hash.clone(), listed.item.statement.clone())))
}

/// Whether every copy of the content `item` withholds is deleted: a blob's canonical copy unless
/// kept, every staged copy, and a recipe's object after its chunks.
fn deleted(item: &WithheldItem) -> bool {
    !item.kept && item.swept && item.deleted && (!item.recipe || item.recipe_deleted)
}

/// How long, in seconds, an item whose deletion step failed waits before a request takes it
/// again, leaving the requests between to the other items.
const FAILED_STEP_BACKOFF: u64 = 60;

/// Whether `item` is owed a deletion step at `now`: its staged copies are not swept yet, or its
/// late step is due and not done.
fn owed(item: &WithheldItem, now: u64) -> bool {
    item.settled
        && !item.never_applies
        && item.lease_until.is_none_or(|until| until <= now)
        && (!item.swept || (!item.deleted && now >= late_sweep_due(item.withheld_at)))
}

/// The prefix every work object of the withheld item of `hash` is named under: apart from a
/// redaction's of the same hash, whose late sweep deletes its own prefix.
fn work_prefix(hash: &str) -> String {
    format!("withheld-{}/", hash)
}

/// The name of the work object holding the walk for whether meta or adopted history names the
/// withheld blob `hash`.
fn walk_name(hash: &str) -> String {
    format!("{}keepers", work_prefix(hash))
}

/// The work object holding the walk for a withheld blob's keepers part way: the history heads
/// still to walk after the one `walk` is walking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedKeepers {
    version: u64,
    heads: Vec<String>,
    walk: HoldingWalk,
}

/// What the walk for a withheld blob's keepers found.
enum Keepers {
    /// Meta or adopted history names the blob: its canonical copy stays.
    Named,

    /// Nothing does: its canonical copy may go.
    None,

    /// The walk has not finished within this request, for this reason; it is saved. `failed`
    /// when an object it needs could not be read, rather than the budget running out.
    Stopped { why: String, failed: bool },
}

/// Walk the tree `root` and every tree under it not in `trees`, adding each to `trees`, and
/// offer every file's content hash to `found` until it answers.
///
/// # Returns
/// * `Ok(Some(T))` - What `found` answered.
/// * `Ok(None)`    - It never answered.
/// * `Err(String)` - A tree could not be read.
fn walk_files<T>(
    root: &str,
    objects: &dyn WalkObjects,
    trees: &mut HashSet<String>,
    found: &mut dyn FnMut(&str) -> Option<T>,
) -> Result<Option<T>, String> {
    let mut pending = vec![root.to_string()];

    while let Some(hash) = pending.pop() {
        if !trees.insert(hash.clone()) {
            continue;
        }

        let tree = objects.load_tree(&hash)?;
        for (_, file) in tree.get_files() {
            if let Some(answer) = found(&file.hash) {
                return Ok(Some(answer));
            }
        }
        pending.extend(tree.get_subtrees().map(|(_, subtree)| subtree.hash.clone()));
    }

    Ok(None)
}

/// What settling one item found.
enum Settled {
    /// The item, as recorded settled.
    Item(Listed),
    /// Not settled within this request, for this reason.
    Unsettled(String),
}

impl<O: ObjectStore, R: RefStore> Head<O, R> {
    /// Every withheld item, as listed, without settling any: for what only reads the statements,
    /// or the hashes they withhold.
    pub(super) fn withheld_listing(&self) -> HeadResult<Vec<Listed>> {
        withheld::list(&self.refs).map_err(HeadError::internal)
    }

    /// Every withheld item, each settled first but for one whose object's header cannot be read:
    /// what a request that serves or takes objects reads before it answers.
    ///
    /// # Returns
    /// * `Ok(Withholding)` - Every item, settled, but for one whose object's header cannot be
    ///                       read: that one stays unsettled and applies, refusing only its own
    ///                       hash, and journals no recipe.
    /// * `Err(HeadError)`  - The items could not be listed or one cannot be read (`500`), or one
    ///                       was not settled within this request (`503`, with `Retry-After`).
    pub(super) fn withholding(&self) -> HeadResult<Withholding> {
        let mut budget = self.scan_budget();
        let mut withholding = self.settled_withholding(&mut budget)?;

        self.advance_withheld_deletion(&mut withholding, &mut budget);

        Ok(withholding)
    }

    /// [`withholding`](Self::withholding), without the deletion step: for a request that reads it
    /// inside a ref update's scratch scope, where a walk would open its own.
    pub(super) fn settled_withholding(&self, budget: &mut Budget) -> HeadResult<Withholding> {
        let mut items = Vec::new();

        for listed in self.withheld_listing()? {
            if listed.item.settled {
                items.push(listed);
                continue;
            }

            match self.settle(listed, budget)? {
                Settled::Item(listed) => items.push(listed),
                Settled::Unsettled(why) => return Err(HeadError::retry_after(why, UNDECIDED_RETRY_AFTER_SECONDS)),
            }
        }

        Ok(Withholding { items })
    }

    /// Refuse a request naming any of `hashes` this head withholds, with a `451`.
    pub(super) fn refuse_withheld(&self, withholding: &Withholding, hashes: &[String]) -> HeadResult<()> {
        match withholding.refusal(hashes) {
            Some(refusal) => Err(refusal),
            None => Ok(()),
        }
    }

    /// The `451` for `hash`, with the statement its withheld item holds now: for an item added
    /// after this request's listing. Its item cannot be read, or is gone, answers `500`.
    pub(super) fn withheld_refusal(&self, hash: &str) -> HeadError {
        let item = self.refs.get_item(ItemKey::Withheld(hash))
            .and_then(|stored| stored.ok_or_else(|| format!("The withheld item for {} applies, and cannot be found.", hash)))
            .and_then(|stored| withheld::decode(hash, &stored));

        match item {
            Ok(item) => HeadError::withheld(withholding::served_refusal(hash, &item.statement)),
            Err(error) => HeadError::internal(error),
        }
    }

    /// Settle `listed`: read what the store holds its hash as, and record it, a recipe journaled
    /// in the chunk ledger in the same transaction.
    fn settle(&self, mut listed: Listed, budget: &mut Budget) -> HeadResult<Settled> {
        for _ in 0..WRITE_ATTEMPTS {
            if listed.item.settled {
                return Ok(Settled::Item(listed));
            }

            let hash = listed.hash.clone();

            if !budget.spend() {
                return Ok(Settled::Unsettled(format!(
                    "Whether this warehouse's host withholds {} as a file's content could not be \
                    settled within this request.",
                    hash
                )));
            }

            // A store that cannot be read leaves the item for a later request. One that lacks the
            // object cannot tell what it is, so the item applies, whatever it names, as a
            // redaction's does: a tree or a chunk of that hash a lift uploads later is refused, and
            // the lift fails rather than landing without it. A header this build cannot read is
            // never guessed at: the item stays unsettled, so it applies and only its own hash is
            // refused, and each request reads and logs it again. The verifier checks only that an
            // object's bytes hash to its key, so this is content no client of this build writes,
            // and it stays loud rather than cheap.
            let bytes = match self.objects.get(&hash) {
                Ok(bytes) => bytes,
                Err(error) => return Ok(Settled::Unsettled(format!(
                    "Whether this warehouse's host withholds {} as a file's content could not be \
                    settled: {}",
                    hash, error
                ))),
            };
            let kind = match bytes.as_deref() {
                Some(bytes) => match object_utils::peek_object_header(bytes) {
                    Ok((object_type, _)) => Some(object_type),
                    Err(error) => {
                        eprintln!(
                            "forklift-aws-lambda: object {} has a header this build cannot read ({:?}), so \
                            whether its host withholds it as a file's content cannot be settled; it is \
                            refused until it can be.",
                            hash, error
                        );
                        return Ok(Settled::Item(listed));
                    }
                },
                None => None,
            };

            let next = match kind {
                // A format marker is named by every parcel of its kind, so it is kept from the
                // start, with no walk: no deletion may leave a kind uncreatable.
                None | Some(ObjectType::Blob) => WithheldItem {
                    settled: true,
                    kept: listed.item.kept || meta_tree::format_marker_hashes().contains(&hash),
                    ..listed.item.clone()
                },
                Some(ObjectType::Recipe) => WithheldItem { settled: true, recipe: true, ..listed.item.clone() },
                Some(_) => WithheldItem { settled: true, never_applies: true, ..listed.item.clone() },
            };

            let state = withheld::encode(&next);
            let mut writes = vec![ItemWrite { key: ItemKey::Withheld(&hash), state: &state, expected: Some(&listed.stored) }];

            // Journaled as of now, not as of the add: no request that started before this one
            // gated the recipe's chunks.
            let ledger = match next.recipe {
                true => Some(super::chunks::journal(&self.refs, &hash, epoch_seconds(std::time::SystemTime::now())).map_err(HeadError::internal)?),
                false => None,
            };
            if let Some((expected, state)) = &ledger {
                writes.push(ItemWrite { key: ItemKey::ChunkLedger, state, expected: expected.as_deref() });
            }

            if self.write_items(&writes, budget)? {
                return Ok(Settled::Item(Listed { hash, stored: state, item: next }));
            }

            // Another request settled it, or a write of unknown fate landed: read it again.
            let Some(stored) = self.refs.get_item(ItemKey::Withheld(&hash)).map_err(HeadError::internal)? else {
                return Err(HeadError::internal(format!(
                    "The withheld item for {} was listed, and is gone: withheld items are never \
                    taken off.",
                    hash
                )));
            };
            let item = withheld::decode(&hash, &stored).map_err(HeadError::internal)?;
            listed = Listed { hash, stored, item };
        }

        Ok(Settled::Unsettled(format!(
            "Other requests kept changing the withheld item for {} while this one settled it.",
            listed.hash
        )))
    }

    /// Run one owed deletion step, for one item of `withholding` owed one, within `budget`:
    /// best effort, as redaction's judge sweeps, so a failure is logged and never answered, and a
    /// request without time to spare skips it.
    /// The listing takes the item as the step wrote it, so the answer reports what is done.
    fn advance_withheld_deletion(&self, withholding: &mut Withholding, budget: &mut Budget) {
        let now = epoch_seconds(SystemTime::now());

        // A step that failed leaves its lease to run out ([`FAILED_STEP_BACKOFF`]), so an item whose
        // step keeps failing never keeps the others from theirs.
        let Some(listed) = withholding.items.iter_mut().find(|listed| owed(&listed.item, now)) else {
            return;
        };

        if !budget.timely() {
            return;
        }

        match self.withheld_deletion_step(listed, now, budget) {
            Ok(Some(written)) => *listed = written,
            Ok(None) => {}
            Err(error) => eprintln!(
                "forklift-aws-lambda: a deletion step for withheld content {} failed: {}",
                listed.hash, error.message
            ),
        }
    }

    /// One deletion step for `listed`, under a lease its item holds, so two requests never run
    /// one at once; the lease outlasts the request, so one that dies holding it leaves the step to
    /// the first request after it ends.
    ///
    /// - Before [`late_sweep_due`], delete every staged copy. A blob's canonical copy waits for the
    ///   late step, and a recipe's for the chunk ledger, which settling journaled it in.
    /// - Once it is due, no request begun before the withhold is still running: for a blob not
    ///   kept, walk every meta pallet's history and the adopted history for one naming it, saving
    ///   the walk between requests. Named, it is kept; otherwise its canonical copy is deleted.
    ///   Either is for good: a meta ref update never comes to name content that is neither
    ///   ([`Head::refuse_meta_naming_withheld`]). Then delete every staged copy again, every response
    ///   bundle written before [`response_cutoff`], and the item's work objects.
    ///
    /// # Returns
    /// * `Ok(Some(Listed))` - The item as the step wrote it.
    /// * `Ok(None)`         - Another request holds the step, or the item moved.
    /// * `Err(HeadError)`   - A read, a write or a delete failed: the step stays owed.
    fn withheld_deletion_step(&self, listed: &Listed, now: u64, budget: &mut Budget) -> HeadResult<Option<Listed>> {
        let hash = &listed.hash;
        let until = budget.deadline.map_or(now + INVOCATION_CEILING, epoch_seconds) + SWEEP_LEASE_MARGIN;
        let leased = WithheldItem { lease_until: Some(until), ..listed.item.clone() };
        let leased_state = withheld::encode(&leased);

        if !self.write_items(&[ItemWrite { key: ItemKey::Withheld(hash), state: &leased_state, expected: Some(&listed.stored) }], budget)? {
            return Ok(None);
        }

        let late = now >= late_sweep_due(leased.withheld_at);
        let deleted = self.delete_withheld_copies(hash, &leased, late, budget);
        let next = match &deleted {
            Ok(Some(kept)) => WithheldItem { swept: true, kept: *kept, deleted: late, lease_until: None, ..leased.clone() },
            Ok(None) => WithheldItem { lease_until: None, ..leased.clone() },
            // From the failure, not the request's start: a walk may have run for minutes first.
            Err(_) => WithheldItem { lease_until: Some(epoch_seconds(SystemTime::now()) + FAILED_STEP_BACKOFF), ..leased.clone() },
        };

        // Lost, the lease simply runs out, and the first request after it takes the step again.
        let state = withheld::encode(&next);
        let written = self.write_items(&[ItemWrite { key: ItemKey::Withheld(hash), state: &state, expected: Some(&leased_state) }], budget);

        // A failed delete is the cause worth reporting, whatever became of the write after it.
        deleted?;
        Ok(written?.then(|| Listed { hash: hash.clone(), stored: state, item: next }))
    }

    /// Delete the copies the store keeps of `hash`, withheld as `item`, for the step
    /// [`withheld_deletion_step`](Self::withheld_deletion_step) describes.
    ///
    /// # Returns
    /// * `Ok(Some(kept))` - The step is done: whether the blob's canonical copy is kept.
    /// * `Ok(None)`       - The walk for its keepers stopped part way; it is saved.
    /// * `Err(HeadError)` - A read or a delete failed: the step stays owed.
    fn delete_withheld_copies(&self, hash: &str, item: &WithheldItem, late: bool, budget: &mut Budget) -> HeadResult<Option<bool>> {
        self.objects.delete_staged(hash).map_err(HeadError::internal)?;

        if !late {
            return Ok(Some(item.kept));
        }

        let kept = match item.kept || item.recipe {
            true => item.kept,
            false => match self.walk_keepers(hash, budget)? {
                Keepers::Named => true,
                Keepers::None => {
                    self.objects.delete_canonical(hash).map_err(HeadError::internal)?;
                    false
                }
                Keepers::Stopped { why, failed: true } => return Err(HeadError::internal(why)),
                Keepers::Stopped { why, failed: false } => {
                    eprintln!("forklift-aws-lambda: {}", why);
                    return Ok(None);
                }
            },
        };

        let cutoff = SystemTime::UNIX_EPOCH + Duration::from_secs(response_cutoff(item.withheld_at));
        self.objects.delete_responses_before(cutoff).map_err(HeadError::internal)?;
        self.objects.work_delete_under(&work_prefix(hash)).map_err(HeadError::internal)?;

        Ok(Some(kept))
    }

    /// Whether meta history or the trust anchor's adopted history names the blob `hash`: walk the
    /// history of every meta pallet's head, live or deleted, and of the anchor's adopts pin, from
    /// the walk saved by an earlier request, within `budget`, saving it again when it stops part
    /// way. The heads are read once, when the walk starts after [`late_sweep_due`]: a meta ref
    /// update after that newly names no withheld content that is not kept, and every head it
    /// leaves behind is in the history of the head it writes.
    fn walk_keepers(&self, hash: &str, budget: &mut Budget) -> HeadResult<Keepers> {
        let name = walk_name(hash);
        let saved = self.objects.work_get(&name).map_err(HeadError::internal)?
            .and_then(|bytes| decode_work::<SavedKeepers>(&bytes));

        let (mut heads, mut walk) = match saved {
            Some(saved) => (saved.heads, Some(saved.walk)),
            None => (self.history_heads()?, None),
        };

        let scratch = self.scratch().map_err(HeadError::internal)?;
        let stepped: Result<Option<bool>, String> = scratch.scoped(|| {
            let objects = MirroredObjects::new(&self.objects);

            loop {
                let current = match &mut walk {
                    Some(current) => current,
                    None => match heads.pop() {
                        Some(head) => walk.insert(HoldingWalk::of_history(&head)),
                        None => return Ok(Some(false)),
                    },
                };

                if !budget.spend() {
                    return Ok(None);
                }

                match current.step_for_hash(hash, &objects)? {
                    WalkProgress::Names => return Ok(Some(true)),
                    WalkProgress::Clear => walk = None,
                    WalkProgress::Going => {}
                }
            }
        });

        let (why, failed) = match stepped {
            Ok(Some(true)) => return Ok(Keepers::Named),
            Ok(Some(false)) => return Ok(Keepers::None),
            Ok(None) => (format!("The walk for whether meta history names withheld content {} has not finished yet.", hash), false),
            Err(why) => (format!("Whether meta history names withheld content {} cannot be told yet: {}", hash, why), true),
        };

        // A walk the budget stopped between heads resumes at the next one. A stop is never read
        // as a finished walk: with nothing left to save, the next request walks again.
        let walk = match walk {
            Some(walk) => walk,
            None => match heads.pop() {
                Some(head) => HoldingWalk::of_history(&head),
                None => return Ok(Keepers::Stopped { why, failed }),
            },
        };
        let bytes = serde_json::to_vec(&SavedKeepers { version: WORK_VERSION, heads, walk })
            .map_err(|error| HeadError::internal(format!("encoding a withheld item's walk failed: {}", error)))?;
        self.objects.work_put(&name, &bytes).map_err(HeadError::internal)?;

        Ok(Keepers::Stopped { why, failed })
    }

    /// Every head whose history may name content a meta cell is verified by: each meta pallet's,
    /// live or deleted, and the trust anchor's adopts pin, each once.
    fn history_heads(&self) -> HeadResult<Vec<String>> {
        let listing = self.refs.list_refs().map_err(HeadError::internal)?;
        let mut heads: Vec<String> = listing.live.iter().chain(&listing.tombstones)
            .filter(|(pallet, _)| pallet.namespace == PalletNamespace::Meta)
            .map(|(_, head)| head.clone())
            .collect();

        if let Some((anchor, _)) = self.refs.get_trust().map_err(HeadError::internal)? {
            heads.extend(anchor.adopts);
        }

        heads.sort();
        heads.dedup();

        Ok(heads)
    }

    /// Refuse a meta pallet's update to `new_head` from `old_head` that newly names withheld
    /// content the late step may still delete: content no item records kept, as a file of a tree
    /// in the parcels from `new_head` back to `old_head`, that `old_head`'s own tree does not name.
    /// With no old head, nothing is named. Read before the ref is written, so a meta history never
    /// comes to rest on a copy a sweep deletes. Its old head is meta history the late walk finds,
    /// so the content its tree names stays named. Content only the old head's history names, not
    /// its tree, is refused too if the update names it again, until the walk records it kept;
    /// the update need not, so the pallet is never wedged by what its history once named. A
    /// format marker is never refused: it is kept from settling, settled or not. An item added
    /// after this listing is not late yet, and this request ends before it is.
    ///
    /// # Returns
    /// * `Ok(())`         - The update newly names no such content.
    /// * `Err(HeadError)` - It does (`451`, with the host's statement), or the items or an object
    ///                      could not be read (`500`).
    pub(super) fn refuse_meta_naming_withheld(&self, new_head: &str, old_head: Option<&str>, objects: &dyn WalkObjects) -> HeadResult<()> {
        let listing = self.withheld_listing()?;
        let guarded: HashMap<&str, &str> = listing.iter()
            .filter(|listed| !listed.item.never_applies && !listed.item.kept)
            .filter(|listed| !meta_tree::format_marker_hashes().contains(&listed.hash))
            .map(|listed| (listed.hash.as_str(), listed.item.statement.as_str()))
            .collect();

        if guarded.is_empty() {
            return Ok(());
        }

        let mut trees: HashSet<String> = HashSet::new();
        let mut named: HashSet<String> = HashSet::new();

        if let Some(old_head) = old_head {
            let root = objects.load_parcel(old_head).map_err(HeadError::internal)?.tree_hash;
            walk_files(&root, objects, &mut trees, &mut |file| {
                if guarded.contains_key(file) {
                    named.insert(file.to_string());
                }
                None::<()>
            }).map_err(HeadError::internal)?;
        }

        let mut parcels: HashSet<String> = HashSet::new();
        let mut pending = vec![new_head.to_string()];

        while let Some(parcel) = pending.pop() {
            if Some(parcel.as_str()) == old_head || !parcels.insert(parcel.clone()) {
                continue;
            }

            let parcel = objects.load_parcel(&parcel).map_err(HeadError::internal)?;
            let found = walk_files(&parcel.tree_hash, objects, &mut trees, &mut |file| {
                guarded.get(file).filter(|_| !named.contains(file)).map(|statement| (file.to_string(), statement.to_string()))
            }).map_err(HeadError::internal)?;

            if let Some((hash, statement)) = found {
                return Err(HeadError::withheld(withholding::served_refusal(&hash, &statement)));
            }

            pending.extend(parcel.parents);
        }

        Ok(())
    }

    /// Record on the withheld item of `hash`, if there is one, that its recipe object has been
    /// deleted.
    ///
    /// # Returns
    /// * `Ok(Some(bool))` - There is an item; whether it records it now.
    /// * `Ok(None)`       - There is no withheld item of `hash`.
    pub(super) fn mark_withheld_recipe_deleted(&self, hash: &str, budget: &mut Budget) -> HeadResult<Option<bool>> {
        for _ in 0..WRITE_ATTEMPTS {
            let Some(stored) = self.refs.get_item(ItemKey::Withheld(hash)).map_err(HeadError::internal)? else {
                return Ok(None);
            };
            let item = withheld::decode(hash, &stored).map_err(HeadError::internal)?;

            if item.recipe_deleted {
                return Ok(Some(true));
            }

            let state = withheld::encode(&WithheldItem { recipe_deleted: true, ..item });
            if self.write_items(&[ItemWrite { key: ItemKey::Withheld(hash), state: &state, expected: Some(&stored) }], budget)? {
                return Ok(Some(true));
            }
        }

        Ok(Some(false))
    }
}
