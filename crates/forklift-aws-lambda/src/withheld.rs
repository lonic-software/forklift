//! What the host of an AWS warehouse withholds: one DynamoDB item per hash, `withheld#<hash>`, in
//! the warehouse's partition ([`ItemKey::Withheld`]).
//!
//! Withholding is a takedown on the host's own authority, as on a served root
//! (`forklift_core::util::withholding`): the host lists a file's content hash with a statement,
//! and the head stops serving and taking it, whatever names it. Each item is a versioned JSON
//! record of the statement, when the hash was withheld, and how far withholding it has got. The
//! operator writes it with `forklift-aws-withhold` ([`add`]); the head lists every item on the
//! requests it gates, with one strongly consistent `Query` ([`list`]), so the next request after a
//! withhold sees it, with nothing to poll.
//!
//! An item of a version this build does not read, or that does not parse, fails every listing,
//! loudly, and is never written over: a head that cannot tell what its host withholds must never
//! serve as if it withheld nothing.
//!
//! The operator's role reads no object, so [`add`] cannot tell what the store holds the hash as.
//! The head settles that from the store before it answers, and an item whose hash the store holds
//! as a tree, a parcel or a chunk, which is no file's content, never applies.

use std::time::SystemTime;

use forklift_core::util::{object_utils, withholding};
use serde::{Deserialize, Serialize};

use crate::format;
use crate::store::{FormatFence, ItemKey, ItemWrite, RedactionWrite, RefStore};

/// The version of the withheld item this build writes and reads.
pub const ITEM_VERSION: u64 = 1;

/// How many of [`add`]'s writes may lose a race, or end with their fate unknown, before it gives
/// up.
const WRITE_ATTEMPTS: usize = 8;

/// A withheld item's record. The fields past the statement are written by the head as it carries
/// the withholding out, each only once true (or, for the lease, while held), so the record an
/// operator adds holds the statement and the time alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WithheldItem {
    /// [`ITEM_VERSION`].
    pub version: u64,

    /// The host's statement: one plain line of at most
    /// [`withholding::STATEMENT_MAX_CHARS`] characters, carried by every refusal and handshake.
    pub statement: String,

    /// When the hash was withheld, in seconds since the Unix epoch.
    pub withheld_at: u64,

    /// Whether a request has read what the store holds the hash as, and recorded it below.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub settled: bool,

    /// Whether the store holds the hash as no file's content (a tree, a parcel or a chunk), so the
    /// withholding never applies and the object stays served.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub never_applies: bool,

    /// Whether the store held the content as a recipe when it was settled. The write that sets it
    /// journals the recipe in the chunk ledger in the same transaction.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recipe: bool,

    /// Whether a blob's canonical copy is kept, because meta history or the adopted history
    /// names it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub kept: bool,

    /// Whether the copies of the content the store keeps have been deleted: every staged copy,
    /// and a blob's canonical object unless it is kept.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub swept: bool,

    /// Whether the recipe's object has been deleted, after its chunks.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recipe_deleted: bool,

    /// Whether the late sweep has deleted every copy the bucket holds.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,

    /// Until when, in seconds since the Unix epoch, one request holds the deletion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_until: Option<u64>,
}

impl WithheldItem {
    /// The record an operator adds: `statement`, withheld at `withheld_at`, nothing yet done.
    pub fn new(statement: &str, withheld_at: u64) -> WithheldItem {
        WithheldItem {
            version: ITEM_VERSION,
            statement: statement.to_string(),
            withheld_at,
            settled: false,
            never_applies: false,
            recipe: false,
            kept: false,
            swept: false,
            recipe_deleted: false,
            deleted: false,
            lease_until: None,
        }
    }
}

/// One withheld item as [`list`] reads it: its hash, the exact bytes it is stored as (what a
/// write conditions on), and its record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub hash: String,
    pub stored: String,
    pub item: WithheldItem,
}

/// The record `stored` holds, for the withheld item of `hash`.
///
/// # Returns
/// * `Ok(WithheldItem)` - The record.
/// * `Err(String)`      - The record is of a version this build does not read, or does not parse.
pub fn decode(hash: &str, stored: &str) -> Result<WithheldItem, String> {
    let version = serde_json::from_str::<serde_json::Value>(stored)
        .ok()
        .and_then(|value| value.get("version").and_then(serde_json::Value::as_u64));

    if version != Some(ITEM_VERSION) {
        return Err(format!(
            "The withheld item for {} is of a version this build does not read ({}), so what this \
            warehouse's host withholds cannot be told. Update the deployment to a build that reads \
            it; nothing was written over it.",
            hash,
            version.map_or("none".to_string(), |version| version.to_string())
        ));
    }

    serde_json::from_str(stored).map_err(|error| format!(
        "The withheld item for {} cannot be read ({}), so what this warehouse's host withholds \
        cannot be told. Nothing was written over it.",
        hash, error
    ))
}

/// The record as the JSON the item stores.
pub fn encode(item: &WithheldItem) -> String {
    serde_json::to_string(item).expect("a withheld item always encodes")
}

/// Every withheld item of the warehouse behind `refs`, sorted by hash.
///
/// # Returns
/// * `Ok(Vec<Listed>)` - Each item and its record.
/// * `Err(String)`     - The items could not be listed, or one is keyed by no object hash, or its
///                       record cannot be read ([`decode`]). Never a partial listing.
pub fn list<R: RefStore + ?Sized>(refs: &R) -> Result<Vec<Listed>, String> {
    let mut listed = Vec::new();

    for (hash, stored) in refs.list_withheld()? {
        if !object_utils::is_object_hash(&hash) {
            return Err(format!(
                "A withheld item is keyed by \"{}\", which is not an object hash, so what this \
                warehouse's host withholds cannot be told. Nothing was written over it.",
                hash.escape_debug()
            ));
        }

        let item = decode(&hash, &stored)?;
        listed.push(Listed { hash, stored, item });
    }

    listed.sort_by(|a, b| a.hash.cmp(&b.hash));
    Ok(listed)
}

/// What [`add`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Added {
    /// The hash was not withheld, and now is.
    Added,
    /// The hash was withheld already; its statement now is the one given, and nothing else of its
    /// record changed.
    Restated,
    /// The hash was withheld already, with this statement; nothing was written.
    Unchanged,
}

/// Withhold `hash` in the warehouse behind `refs`, with the host's `statement`.
///
/// The item is written conditioned on what was read of it, its absence for a new one, and on the
/// warehouse's format item, so it is never written over an item another writer changed, and never
/// into a warehouse this build does not serve. A warehouse holding nothing yet is refused, so a
/// misspelt id never makes one.
///
/// # Returns
/// * `Ok(Added)`   - What changed.
/// * `Err(String)` - `hash` is not an object hash, `statement` is not one plain line of at most
///                   [`withholding::STATEMENT_MAX_CHARS`], the warehouse is new or in a format
///                   this build does not serve, the item held already cannot be read, or the
///                   write kept losing races or failing.
pub fn add<R: RefStore + ?Sized>(refs: &R, hash: &str, statement: &str) -> Result<Added, String> {
    add_at(refs, hash, statement, epoch_seconds(SystemTime::now()))
}

/// [`add`], withheld at `now`.
fn add_at<R: RefStore + ?Sized>(refs: &R, hash: &str, statement: &str, now: u64) -> Result<Added, String> {
    if !object_utils::is_object_hash(hash) {
        return Err(format!("\"{}\" is not an object hash.", hash));
    }

    let statement = withholding::checked_statement(statement)?;

    for _ in 0..WRITE_ATTEMPTS {
        let stored = refs.get_item(ItemKey::Withheld(hash))?;

        let (added, item) = match &stored {
            None => (Added::Added, WithheldItem::new(statement, now)),
            Some(stored) => {
                let held = decode(hash, stored)?;

                if held.statement == statement {
                    return Ok(Added::Unchanged);
                }

                (Added::Restated, WithheldItem { statement: statement.to_string(), ..held })
            }
        };

        let state = encode(&item);
        let write = |fence: &FormatFence| match fence {
            FormatFence::Stamp(_) => Err(crate::HeadError::conflict(
                "This warehouse holds nothing yet, so nothing was withheld in it: check its id."
                    .to_string(),
            )),
            FormatFence::At(_) => refs
                .put_items(&[ItemWrite { key: ItemKey::Withheld(hash), state: &state, expected: stored.as_deref() }], fence)
                .map_err(crate::HeadError::internal),
        };

        match format::fenced(refs, write, |outcome| *outcome == RedactionWrite::FormatMoved)
            .map_err(|error| error.message)?
        {
            RedactionWrite::Written => return Ok(added),
            // Read the item again: another writer changed it, or this write's fate is unknown.
            RedactionWrite::Moved | RedactionWrite::Transient(_) => continue,
            RedactionWrite::FormatMoved => unreachable!("fenced never returns an outcome `moved` names"),
        }
    }

    Err(format!(
        "The withheld item for {} kept changing, or its writes kept failing, while it was being \
        written. Run the command again; `list` shows whether it landed.",
        hash
    ))
}

/// `time` in whole seconds since the Unix epoch.
fn epoch_seconds(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryRefStore;

    /// A store of a warehouse in the format this build serves, holding one pallet.
    fn served() -> MemoryRefStore {
        let items = [
            ("format".to_string(), "state".to_string(), format::AWS_FORMAT.to_string()),
            ("pallet#main".to_string(), "head".to_string(), "a".repeat(64)),
        ];
        MemoryRefStore::from_items(&items).expect("a served store")
    }

    // Revert: write the item unconditioned → the restate overwrites the head's progress, and the
    // add over an unchanged statement writes. Revert: take the stamp fence → the empty warehouse is
    // made by the add.
    #[test]
    fn an_add_writes_a_new_item_restates_one_and_never_makes_a_warehouse() {
        let refs = served();
        let hash = "b".repeat(64);

        assert_eq!(add_at(&refs, &hash, "  court order 1 ", 7).unwrap(), Added::Added);
        let listed = list(&refs).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].hash, hash);
        assert_eq!(listed[0].item, WithheldItem::new("court order 1", 7));

        assert_eq!(add_at(&refs, &hash, "court order 1", 9).unwrap(), Added::Unchanged);
        assert_eq!(list(&refs).unwrap()[0].item.withheld_at, 7, "nothing was written");

        // A record the head has carried forward keeps its progress when restated.
        let progressed = WithheldItem { settled: true, swept: true, ..WithheldItem::new("court order 1", 7) };
        let fence = FormatFence::At(format::AWS_FORMAT.to_string());
        let write = ItemWrite { key: ItemKey::Withheld(&hash), state: &encode(&progressed), expected: Some(&listed[0].stored) };
        assert_eq!(refs.put_items(&[write], &fence).unwrap(), RedactionWrite::Written);
        assert_eq!(add_at(&refs, &hash, "court order 2", 9).unwrap(), Added::Restated);
        assert_eq!(
            list(&refs).unwrap()[0].item,
            WithheldItem { statement: "court order 2".to_string(), ..progressed }
        );

        let empty = MemoryRefStore::new();
        let refused = add_at(&empty, &hash, "court order 3", 7).unwrap_err();
        assert!(refused.contains("holds nothing yet"), "{}", refused);
        assert!(empty.items().is_empty(), "nothing was written: {:?}", empty.items());
    }

    // Revert: skip the statement check → the two-line statement is written.
    #[test]
    fn an_add_refuses_what_is_not_a_hash_or_not_a_statement() {
        let refs = served();
        assert!(add_at(&refs, "not-a-hash", "court order", 7).unwrap_err().contains("not an object hash"));
        assert!(add_at(&refs, &"c".repeat(64), "two\nlines", 7).unwrap_err().contains("one plain line"));
        assert!(add_at(&refs, &"c".repeat(64), &"x".repeat(withholding::STATEMENT_MAX_CHARS + 1), 7).is_err());
        assert!(list(&refs).unwrap().is_empty());
    }

    // Revert: skip an item of an unknown version in the listing → the listing reads as withholding
    // nothing. Revert: accept an unknown field → the newer record is read.
    #[test]
    fn a_listing_fails_loudly_on_an_item_this_build_cannot_read() {
        let hash = "d".repeat(64);
        let with = |state: &str| {
            let items = [
                ("format".to_string(), "state".to_string(), format::AWS_FORMAT.to_string()),
                (format!("withheld#{}", hash), "state".to_string(), state.to_string()),
            ];
            MemoryRefStore::from_items(&items).expect("a store")
        };

        let newer = with(r#"{"version":2,"statement":"s","withheld_at":1}"#);
        assert!(list(&newer).unwrap_err().contains("of a version this build does not read (2)"));
        let unknown = with(r#"{"version":1,"statement":"s","withheld_at":1,"later":true}"#);
        assert!(list(&unknown).unwrap_err().contains("cannot be read"));
        assert!(add_at(&unknown, &hash, "other", 7).is_err(), "never written over");
        let bare = with("{}");
        assert!(list(&bare).unwrap_err().contains("(none)"));

        let readable = with(r#"{"version":1,"statement":"s","withheld_at":1}"#);
        assert_eq!(list(&readable).unwrap()[0].item, WithheldItem::new("s", 1));
    }
}
