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
//! The cost: one strongly consistent `Query` per such request, reading every item whole. An item
//! is its statement (at most 1000 characters) and a few flags, so a strongly consistent read
//! unit (4 KB) covers about four; a warehouse whose host withholds nothing pays the smallest
//! `Query` there is. Kept as one listing rather than split into a statement item and a progress
//! item, because the split saves bytes, not the round trip, and only pays once a host withholds
//! hundreds of hashes.

use forklift_core::model::remote::{ServedWithholding, ServedWithholdingState};
use forklift_core::enums::object_type::ObjectType;
use forklift_core::util::object_utils;
use forklift_core::util::withholding::{self, WithheldSet};

use super::redaction::{epoch_seconds, Budget, UNDECIDED_RETRY_AFTER_SECONDS, WRITE_ATTEMPTS};
use super::Head;
use crate::error::{HeadError, HeadResult};
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
        let mut items = Vec::new();

        for listed in self.withheld_listing()? {
            if listed.item.settled {
                items.push(listed);
                continue;
            }

            match self.settle(listed, &mut budget)? {
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
                None | Some(ObjectType::Blob) => WithheldItem { settled: true, ..listed.item.clone() },
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
