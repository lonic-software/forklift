//! [`DynamoRefStore`]: the consistency point on `aws-sdk-dynamodb`.
//!
//! # Item layout
//!
//! One table serves many warehouses. Each item is keyed by a partition key (`wh`, the
//! warehouse id) and a sort key (`entity`), so a warehouse's refs sit together in one
//! partition and never collide with another warehouse's:
//!
//! | `wh`          | `entity`         | payload            | what it is                     |
//! |---------------|------------------|--------------------|--------------------------------|
//! | `<warehouse>` | `pallet#main`    | `head = <hash>`    | a user pallet head             |
//! | `<warehouse>` | `pallet#@office` | `head = <hash>`    | a meta pallet head             |
//! | `<warehouse>` | `pallet#old`     | `deleted_head = …` | a deleted pallet's tombstone   |
//! | `<warehouse>` | `trust`          | `anchor = <json>`  | the trust anchor               |
//! | `<warehouse>` | `format`         | `state = <state>`  | the warehouse's format         |
//! | `<warehouse>` | `redaction#<hash>` | `state = <json>` | where one redaction stands     |
//! | `<warehouse>` | `withheld#<hash>` | `state = <json>` | content its host withholds     |
//! | `<warehouse>` | `chunk-fence`    | `state = <json>`   | the round of chunk deletion    |
//! | `<warehouse>` | `chunk-ledger`   | `state = <json>`   | the recipes chunk rounds owe   |
//!
//! The format item's `state` is the format number (`1`), or `N>M owner step` while an upgrade
//! from `N` to `M` holds it (`crate::format`). Every ref and trust write carries a fence on it as
//! the first action of its transaction: a `ConditionCheck` on the state the request's gate read,
//! which never rewrites the item and so can never erase an upgrade's mark, or, in a new
//! warehouse's empty partition, a `Put` stamping it conditioned on its absence ([`FormatFence`]).
//! Only the upgrade operator rewrites it ([`FormatStore`]).
//!
//! The pallet sort key is `pallet#{qualified-ref}` — the same wire form the fake keys on
//! (`main`, `@office`), unique across the two namespaces. Partitioning by warehouse makes
//! [`list_refs`](RefStore::list_refs) a single `Query` (`wh = … AND begins_with(entity,
//! "pallet#")`) rather than a full-table `Scan` — the explicit ref enumeration the trait
//! calls for, since object storage has no directory walk.
//!
//! A pallet item carries `head` while the pallet is live and `deleted_head` once it is deleted —
//! never both. A delete keeps the item rather than removing it, so a creation conditioned on the
//! item not existing (`attribute_not_exists(entity)`) refuses a deleted pallet exactly as it
//! refuses a live one, and only an acknowledged re-creation, conditioned on the tombstone, lifts
//! it back.
//!
//! A redaction item exists for each redaction recorded on `@redactions` that the head has begun
//! deciding (`crate::redaction`): its `state` is a versioned JSON record of the scan's verdicts
//! and whether the redaction has applied. It is written by a `Put` conditioned on the exact
//! bytes the writer read, behind the format fence, so two judges never overwrite each other's
//! verdicts. A record of a version this build does not know fails every read of it. The chunk
//! fence and the chunk ledger (`crate::head::chunks`) are written the same way, and a recipe's
//! apply writes its redaction item and the ledger in one transaction ([`RefStore::put_items`]).
//!
//! A withheld item exists for each hash the warehouse's host withholds (`crate::withheld`): its
//! `state` is a versioned JSON record of the host's statement and how far withholding the content
//! has got. It is written the same way, and listed with one strongly consistent `Query` on
//! `withheld#`, as pallets are ([`RefStore::list_withheld`]).
//!
//! The table's key schema must be `wh` (S, partition) and `entity` (S, sort); a deployment
//! provisions it that way (the integration test creates exactly this table).
//!
//! # The CAS
//!
//! [`compare_and_set_head`](RefStore::compare_and_set_head) — and its sibling
//! [`delete_head`](RefStore::delete_head), the same transaction with a different pallet action —
//! is a real DynamoDB conditional
//! write, never a read-then-write — and, as of FORK-95, never a single-item one either. It is
//! a `TransactWriteItems` of up to four actions: the format fence above, an `Update` on the
//! target pallet's own item
//! whose `ConditionExpression` encodes the caller's `expected` (exactly the prior single-item
//! shape), a `ConditionCheck` on the office pallet's item at the office head the caller's
//! audit consumed (or at its absence, for a delete whose authority check found the office
//! unborn), and a `ConditionCheck` on the trust item at the anchor's stored serialization. The
//! office check is skipped for two independent reasons: the target pallet *is* the office pallet
//! (the `Update` above already pins that item; DynamoDB refuses two actions on one item), or the
//! caller's audit never read an office head at all (an untrusted warehouse's audit never touches
//! it) — see `RefStore::compare_and_set_head`'s docs. DynamoDB
//! either applies the whole transaction with every condition holding or applies nothing; on a
//! condition failure `ReturnValuesOnConditionCheckFailure=ALL_OLD` hands back the failed item,
//! so the store reports which precondition moved without a second round trip.
//! [`put_trust_if_absent`](RefStore::put_trust_if_absent) is a transaction too, of the format
//! fence and a `Put` of the trust item conditioned on its absence: the one-way trust door.
//!
//! # The anchor write
//!
//! [`replace_trust`](RefStore::replace_trust) — the one sanctioned overwrite of the anchor, a
//! re-genesis — is the store's *second* transaction, and for the same reason: `put_trust`
//! validates the chain of custody against values that can move before the write lands. It is a
//! `TransactWriteItems` of the format fence and two more actions: a `Put` on the trust item conditioned on the
//! incumbent anchor's stored bytes, and a `ConditionCheck` on the office item at the head the
//! `adopts` test consumed. Unlike the CAS above, neither action is ever dropped — the trust item
//! and the office item are always distinct, so the collapse-to-one-item case cannot arise. It
//! was a bare unconditional `PutItem` until FORK-95 slice 4, which is how two concurrent
//! re-geneses could both pass the same `prior_genesis` test and both write.
//!
//! Cancellation reasons on a `TransactionCanceledException` are ordered by the order actions
//! were requested (AWS API Reference and the pinned `aws-sdk-dynamodb` model agree on this), so
//! the index an action is pushed at is the index its refusal comes back at. Every transaction
//! above depends on that, and they order their preconditions *differently* — after the format
//! fence, the target pallet's head comes first in a ref-update commit and the trust anchor in an
//! anchor write. Neither
//! restates the mapping: each action is pushed together with the precondition it conditions on,
//! and the refusal is read back off that same layout. See `ConditionedTransaction`.

use std::collections::HashMap;
use std::time::Duration;

use aws_sdk_dynamodb::config::retry::RetryConfig;
use aws_sdk_dynamodb::config::timeout::TimeoutConfig;
use aws_sdk_dynamodb::operation::transact_write_items::TransactWriteItemsError;
use aws_smithy_runtime_api::client::result::SdkError;
use aws_sdk_dynamodb::types::{
    AttributeValue, CancellationReason, ConditionCheck, Put, ReturnValuesOnConditionCheckFailure,
    TransactWriteItem, Update,
};

use forklift_core::model::remote::TrustAnchorDto;
use forklift_core::util::office_utils::{TrustAnchor, OFFICE_PALLET_NAME};
use forklift_core::util::pallet_utils::{PalletNamespace, PalletRef};
use forklift_core::util::redaction_utils::REDACTIONS_PALLET_NAME;

use crate::aws::dynamo_ops::DynamoOps;
use crate::aws::sdk::describe;
use crate::blocking::AsyncBridge;
use crate::reissue::request_token;
use crate::store::{
    CasOutcome, ExpectedHead, FormatFence, FormatStore, ItemKey, ItemWrite, OfficePrecondition, RedactionWrite, RedactionsPrecondition,
    RefHead, RefListing,
    RefStore, TransientCause, TrustOutcome, TrustWriteOutcome, TRANSACTION_ATTEMPT_TIMEOUT,
};

/// The partition-key attribute: the warehouse id.
pub(crate) const ATTR_WAREHOUSE: &str = "wh";
/// The sort-key attribute: the item kind (`pallet#…` or `trust`).
pub(crate) const ATTR_ENTITY: &str = "entity";
/// A live pallet item's head hash.
pub(crate) const ATTR_HEAD: &str = "head";
/// A deleted pallet item's tombstone: the head the pallet held when it was deleted.
pub(crate) const ATTR_DELETED_HEAD: &str = "deleted_head";
/// A trust item's anchor, as a JSON string.
pub(crate) const ATTR_ANCHOR: &str = "anchor";
/// The format item's state.
pub(crate) const ATTR_STATE: &str = "state";

/// The sort-key prefix of every pallet head; `begins_with` on it is the ref enumeration.
pub(crate) const ENTITY_PALLET_PREFIX: &str = "pallet#";
/// The sort key of the single trust item.
pub(crate) const ENTITY_TRUST: &str = "trust";
/// The sort key of the single format item.
pub(crate) const ENTITY_FORMAT: &str = "format";
/// The sort-key prefix of a redaction item: `redaction#<hash>`.
pub(crate) const ENTITY_REDACTION_PREFIX: &str = "redaction#";
/// The sort-key prefix of a withheld item: `withheld#<hash>`.
pub(crate) const ENTITY_WITHHELD_PREFIX: &str = "withheld#";
/// The sort key of the single chunk fence item.
pub(crate) const ENTITY_CHUNK_FENCE: &str = "chunk-fence";
/// The sort key of the single chunk ledger item.
pub(crate) const ENTITY_CHUNK_LEDGER: &str = "chunk-ledger";

/// The sort key of the item at `key`.
pub(crate) fn item_entity(key: ItemKey<'_>) -> String {
    match key {
        ItemKey::Redaction(hash) => format!("{}{}", ENTITY_REDACTION_PREFIX, hash),
        ItemKey::Withheld(hash) => format!("{}{}", ENTITY_WITHHELD_PREFIX, hash),
        ItemKey::ChunkFence => ENTITY_CHUNK_FENCE.to_string(),
        ItemKey::ChunkLedger => ENTITY_CHUNK_LEDGER.to_string(),
    }
}

/// The sort key of a pallet head — `pallet#{wire}`, the qualified reference the fake keys on.
fn pallet_entity(namespace: PalletNamespace, name: &str) -> String {
    let wire = PalletRef { namespace, name: name.to_string() }.to_wire();

    format!("{}{}", ENTITY_PALLET_PREFIX, wire)
}

/// A DynamoDB string attribute.
fn s(value: impl Into<String>) -> AttributeValue {
    AttributeValue::S(value.into())
}

/// The `head` string of an item, if present.
fn head_of(item: &HashMap<String, AttributeValue>) -> Option<String> {
    item.get(ATTR_HEAD).and_then(|value| value.as_s().ok()).cloned()
}

/// What a pallet item holds: live at its `head`, a tombstone of its `deleted_head`, or nothing.
/// `head` wins if an item somehow carries both — nothing this store writes ever does, and a live
/// head is the state a write conditioned on `head` would see.
fn ref_head_of(item: &HashMap<String, AttributeValue>) -> RefHead {
    if let Some(head) = head_of(item) {
        return RefHead::Live(head);
    }

    match item.get(ATTR_DELETED_HEAD).and_then(|value| value.as_s().ok()) {
        Some(deleted) => RefHead::Deleted(deleted.clone()),
        None => RefHead::Absent,
    }
}

/// What the action on the target pallet's own item writes.
enum PalletWrite<'a> {
    /// Leave the pallet live at this head, from whatever `ExpectedHead` names.
    Set { expected: ExpectedHead<'a>, new: &'a str },
    /// Turn the pallet live at this head into its tombstone.
    Delete { old: &'a str },
}

/// The `anchor` string of an item — its *stored bytes*, not a decoded anchor — if present. Reads
/// the same way `get_trust` does, so an item carrying no readable `anchor` string is
/// indistinguishable here from a missing item, exactly as it is there.
fn anchor_of(item: &HashMap<String, AttributeValue>) -> Option<String> {
    item.get(ATTR_ANCHOR).and_then(|value| value.as_s().ok()).cloned()
}

/// The DynamoDB-backed [`RefStore`]: pallet heads and the trust anchor, with an atomic head
/// CAS and a one-way trust door.
///
/// Every method is synchronous and drives the async SDK through the [`AsyncBridge`]. The
/// default pallet is held here rather than read per call — it is set once when the warehouse
/// is registered, exactly as the fake holds it — so `default_pallet` costs no round trip.
pub struct DynamoRefStore {
    client: DynamoOps,
    table: String,
    warehouse: String,
    default_pallet: String,
    bridge: AsyncBridge,
}

impl DynamoRefStore {
    /// Build the store over a DynamoDB `client` addressing `table`, scoped to `warehouse`
    /// (the partition key) and serving `default_pallet`, driving async calls through `bridge`.
    pub fn new(
        client: DynamoOps,
        table: String,
        warehouse: String,
        default_pallet: String,
        bridge: AsyncBridge,
    ) -> DynamoRefStore {
        DynamoRefStore { client, table, warehouse, default_pallet, bridge }
    }

    /// The full primary key of an item in this warehouse's partition.
    fn key(&self, entity: &str) -> HashMap<String, AttributeValue> {
        HashMap::from([
            (ATTR_WAREHOUSE.to_string(), s(self.warehouse.clone())),
            (ATTR_ENTITY.to_string(), s(entity)),
        ])
    }

    /// Read one item by its sort key, strongly consistent.
    ///
    /// DynamoDB's default eventually-consistent read can serve a replica that has not yet
    /// absorbed the last write; `consistent_read(true)` pins this read to the same partition
    /// the CAS writes land on, at roughly double the read cost. That closes the *read* half of
    /// a staleness window that otherwise exists wherever `get_head`/`get_trust` feed a
    /// decision: `ref_update` reads the office head and the trust anchor once per request to
    /// decide what to audit against, and an eventually-consistent read could hand back an
    /// office head DynamoDB had already moved past.
    ///
    /// The other half — a concurrent office re-key or re-genesis landing between that read and
    /// the commit — used to be an open window at this commit's ancestor: `compare_and_set_head`
    /// conditioned on the target pallet's own head alone, so the office head and the anchor
    /// could move freely underneath a running audit. FORK-95 closed it by folding both into the
    /// same `TransactWriteItems` the pallet head CAS already needed (see this module's own
    /// docs), so the commit now conditions on everything the audit consumed, not only the one
    /// input the caller happened to be updating.
    /// Every item of the warehouse's partition whose sort key begins with `prefix`, read with one
    /// strongly consistent `Query`, page by page to the end.
    async fn query_prefix(&self, prefix: &str) -> Result<Vec<HashMap<String, AttributeValue>>, String> {
        let mut items = Vec::new();
        let mut start_key: Option<HashMap<String, AttributeValue>> = None;

        loop {
            let mut request = self
                .client
                .query()
                .table_name(&self.table)
                .key_condition_expression("#wh = :wh AND begins_with(#e, :prefix)")
                .expression_attribute_names("#wh", ATTR_WAREHOUSE)
                .expression_attribute_names("#e", ATTR_ENTITY)
                .expression_attribute_values(":wh", s(self.warehouse.clone()))
                .expression_attribute_values(":prefix", s(prefix))
                .consistent_read(true);

            if let Some(key) = start_key.take() {
                request = request.set_exclusive_start_key(Some(key));
            }

            let page = request.send().await.map_err(|err| describe("DynamoDB query", err))?;
            items.extend(page.items().iter().cloned());

            match page.last_evaluated_key() {
                Some(key) if !key.is_empty() => start_key = Some(key.clone()),
                _ => break,
            }
        }

        Ok(items)
    }

    async fn read_item(
        &self,
        entity: &str,
    ) -> Result<Option<HashMap<String, AttributeValue>>, String> {
        let output = self
            .client
            .get_item()
            .table_name(&self.table)
            .set_key(Some(self.key(entity)))
            .consistent_read(true)
            .send()
            .await
            .map_err(|err| describe("DynamoDB get_item", err))?;

        Ok(output.item)
    }

    /// The format fence's action, the first of every transaction a request writes with: a
    /// `ConditionCheck` that the format item still holds the state the gate read, or a `Put`
    /// stamping a new warehouse's item, conditioned on there still being none.
    fn format_action(&self, fence: &FormatFence) -> TransactWriteItem {
        match fence {
            FormatFence::At(state) => {
                let check = ConditionCheck::builder()
                    .set_key(Some(self.key(ENTITY_FORMAT)))
                    .table_name(&self.table)
                    .condition_expression("#s = :s")
                    .expression_attribute_names("#s", ATTR_STATE)
                    .expression_attribute_values(":s", s(state.clone()))
                    .build()
                    .expect("every required ConditionCheck field is set");

                TransactWriteItem::builder().condition_check(check).build()
            }
            FormatFence::Stamp(state) => {
                let mut item = self.key(ENTITY_FORMAT);
                item.insert(ATTR_STATE.to_string(), s(state.clone()));

                let put = Put::builder()
                    .set_item(Some(item))
                    .table_name(&self.table)
                    .condition_expression("attribute_not_exists(#e)")
                    .expression_attribute_names("#e", ATTR_ENTITY)
                    .build()
                    .expect("every required Put field is set");

                TransactWriteItem::builder().put(put).build()
            }
        }
    }

    /// The `Update` action against the target pallet's own item, guarded by what the caller
    /// expects it to hold. Always right after the format fence in the transaction
    /// [`Self::commit`] builds.
    ///
    /// DynamoDB refuses an expression attribute name or value the expressions never use, so each
    /// arm names exactly the attributes its own two expressions mention.
    fn pallet_update(&self, entity: &str, write: PalletWrite<'_>) -> TransactWriteItem {
        let builder = Update::builder()
            .set_key(Some(self.key(entity)))
            .table_name(&self.table)
            .expression_attribute_names("#h", ATTR_HEAD)
            .return_values_on_condition_check_failure(ReturnValuesOnConditionCheckFailure::AllOld);

        let builder = match write {
            // The pallet must currently be live at exactly `old`. A missing item or a tombstone
            // fails this too (there is no `head` to equal `old`), reported as a conflict carrying
            // what the item does hold.
            PalletWrite::Set { expected: ExpectedHead::At(old), new } => builder
                .update_expression("SET #h = :new")
                .condition_expression("#h = :old")
                .expression_attribute_values(":new", s(new))
                .expression_attribute_values(":old", s(old)),
            // The pallet must have no item at all: neither a head nor a tombstone.
            PalletWrite::Set { expected: ExpectedHead::Unborn, new } => builder
                .update_expression("SET #h = :new")
                .condition_expression("attribute_not_exists(#e)")
                .expression_attribute_names("#e", ATTR_ENTITY)
                .expression_attribute_values(":new", s(new)),
            // The pallet must be a tombstone of exactly `tombstone`; lifting it back clears the
            // tombstone in the same write.
            PalletWrite::Set { expected: ExpectedHead::Deleted(tombstone), new } => builder
                .update_expression("SET #h = :new REMOVE #d")
                .condition_expression("attribute_not_exists(#h) AND #d = :t")
                .expression_attribute_names("#d", ATTR_DELETED_HEAD)
                .expression_attribute_values(":new", s(new))
                .expression_attribute_values(":t", s(tombstone)),
            // The pallet must be live at exactly `old`; it becomes a tombstone of `old`.
            PalletWrite::Delete { old } => builder
                .update_expression("SET #d = :old REMOVE #h")
                .condition_expression("#h = :old")
                .expression_attribute_names("#d", ATTR_DELETED_HEAD)
                .expression_attribute_values(":old", s(old)),
        };

        TransactWriteItem::builder()
            .update(builder.build().expect("every required Update field is set"))
            .build()
    }

    /// The `ConditionCheck` on a meta pallet's item, the office's or `@redactions`': its head must
    /// still equal `head` exactly, `None` meaning "the pallet must be unborn". Built only when the
    /// caller actually consumed that head — see [`Self::commit`], which skips this action both
    /// when the target *is* that pallet itself (the `Update` above already pins that exact item;
    /// AWS refuses two actions on one item in the same transaction) and when the caller's
    /// precondition is `NotConsumed`, meaning it never read one.
    ///
    /// For the office, the `None` arm is reached from two callers, each of which genuinely
    /// consumed "the office is unborn". `replace_trust`: `put_trust` reads the office head
    /// unconditionally to run its `adopts` check, so an anchor adopting nothing is one whose
    /// validation depended on the office still being unborn. A pallet delete, through
    /// `OfficePrecondition::Unborn`, on every warehouse whose office is unborn: in the
    /// trust-bootstrap window its authority check admits the caller because no office head exists
    /// yet, so an office lift landing before the commit must refuse it. A ref update reaches it for
    /// `@redactions` alone, whenever its redaction check found no redaction recorded: one recorded
    /// before the commit must refuse it. The distinction lives in the callers' parameter types,
    /// not in this method, which only builds what it is told.
    fn head_condition_check(
        &self,
        entity: &str,
        head: Option<&str>,
    ) -> TransactWriteItem {
        let mut builder = ConditionCheck::builder()
            .set_key(Some(self.key(entity)))
            .table_name(&self.table)
            .return_values_on_condition_check_failure(ReturnValuesOnConditionCheckFailure::AllOld);

        builder = match head {
            Some(head) => builder
                .condition_expression("#h = :h")
                .expression_attribute_names("#h", ATTR_HEAD)
                .expression_attribute_values(":h", s(head)),
            // `attribute_not_exists(#h)` names the *head attribute*, not the item — matching
            // `head_of`, which reads an item carrying no readable `head` string exactly as it
            // reads a missing item. The unborn condition must test what the reader tests.
            None => builder
                .condition_expression("attribute_not_exists(#h)")
                .expression_attribute_names("#h", ATTR_HEAD),
        };

        TransactWriteItem::builder()
            .condition_check(builder.build().expect("every required ConditionCheck field is set"))
            .build()
    }

    /// A `ConditionCheck` that the state item at `entity` still holds exactly `state`, or, for
    /// `None`, that there is no such item.
    fn state_condition_check(&self, entity: &str, state: Option<&str>) -> TransactWriteItem {
        let builder = ConditionCheck::builder()
            .set_key(Some(self.key(entity)))
            .table_name(&self.table)
            .return_values_on_condition_check_failure(ReturnValuesOnConditionCheckFailure::AllOld);

        let builder = match state {
            Some(state) => builder
                .condition_expression("#s = :s")
                .expression_attribute_names("#s", ATTR_STATE)
                .expression_attribute_values(":s", s(state)),
            None => builder
                .condition_expression("attribute_not_exists(#e)")
                .expression_attribute_names("#e", ATTR_ENTITY),
        };

        TransactWriteItem::builder()
            .condition_check(builder.build().expect("every required ConditionCheck field is set"))
            .build()
    }

    /// The `Put` action that replaces the trust item, guarded on the incumbent anchor's **stored
    /// bytes** being exactly what the caller read. Always right after the format fence in the
    /// transaction `replace_trust` builds.
    ///
    /// A `Put` rather than an `Update` because the anchor item is wholly replaced by a
    /// re-genesis, and the condition is string equality on `anchor` for the same reason the
    /// ref-update commit's anchor check is (claim C13): a `serde` field reorder must never refuse
    /// a re-genesis over an anchor that never actually moved.
    fn trust_put(&self, json: &str, expected_anchor: &str) -> TransactWriteItem {
        let mut item = self.key(ENTITY_TRUST);
        item.insert(ATTR_ANCHOR.to_string(), s(json));

        let put = Put::builder()
            .set_item(Some(item))
            .table_name(&self.table)
            .return_values_on_condition_check_failure(ReturnValuesOnConditionCheckFailure::AllOld)
            .condition_expression("#a = :a")
            .expression_attribute_names("#a", ATTR_ANCHOR)
            .expression_attribute_values(":a", s(expected_anchor))
            .build()
            .expect("every required Put field is set");

        TransactWriteItem::builder().put(put).build()
    }

    /// The `ConditionCheck` on the trust item: its *stored bytes* must still equal `anchor`,
    /// `None` meaning "no anchor exists". Built from the bytes the head read, never a
    /// re-serialization of the decoded value (this module's docs / `RefStore::get_trust`).
    /// `attribute_not_exists(#a)` names the anchor *attribute*, not the item's existence —
    /// `get_trust` treats an item with no readable `ATTR_ANCHOR` string identically to a
    /// missing item (below), so the absent-anchor condition must test the same thing it does.
    fn anchor_condition_check(&self, anchor: Option<&str>) -> TransactWriteItem {
        let mut builder = ConditionCheck::builder()
            .set_key(Some(self.key(ENTITY_TRUST)))
            .table_name(&self.table)
            .return_values_on_condition_check_failure(ReturnValuesOnConditionCheckFailure::AllOld);

        builder = match anchor {
            Some(bytes) => builder
                .condition_expression("#a = :a")
                .expression_attribute_names("#a", ATTR_ANCHOR)
                .expression_attribute_values(":a", s(bytes)),
            None => builder
                .condition_expression("attribute_not_exists(#a)")
                .expression_attribute_names("#a", ATTR_ANCHOR),
        };

        TransactWriteItem::builder()
            .condition_check(builder.build().expect("every required ConditionCheck field is set"))
            .build()
    }
}

/// Which movable input the action at one position of a transaction conditions on.
///
/// Naming the positions is what lets a single classifier serve both of this store's
/// transactions. They condition on overlapping but different inputs — the ref-update commit on
/// the target pallet's head, the office head and the anchor; the anchor's own write on the
/// incumbent anchor and the office head — and a classifier that hard-coded either one's index
/// literals could not serve the other without a second copy of the same positional reasoning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Precondition {
    /// The format item's state, as the request's gate read it.
    Format,
    /// The target pallet's own head, as the caller's `expected` names it.
    PalletHead,
    /// The office pallet's head, as the caller's audit or validation consumed it.
    OfficeHead,
    /// The `@redactions` head, as the caller's redaction check read it.
    RedactionsHead,
    /// The trust anchor's stored bytes, as [`RefStore::get_trust`] returned them.
    Anchor,
    /// A state item's stored state, as [`RefStore::get_item`] returned it.
    Item,
    /// The chunk fence's stored state, as the ref update read it before its audit.
    ChunkFence,
}

/// A `TransactWriteItems` under construction, together with the positional map of what each of
/// its actions conditions on.
///
/// The two are pushed as one and read back as one, and that is the whole point of the type.
/// `TransactionCanceledException::cancellation_reasons` is documented ordered by the order
/// actions were requested (AWS API Reference, corroborated by the pinned `aws-sdk-dynamodb`
/// model), so **the index an action is pushed at IS the index its refusal comes back at**.
/// Keeping the build order and the attribution in separate places — a `push` sequence here, a run
/// of index literals there — makes that correspondence something two pieces of code have to agree
/// about, and agreements drift. Here there is nothing to drift: the layout is a by-product of
/// building the actions, and [`classify`] reads it rather than restating it. The length check
/// falls out for free, as `layout.len()`.
struct ConditionedTransaction {
    items: Vec<TransactWriteItem>,
    layout: Vec<Precondition>,
}

/// What a cancelled transaction was refused *for*, before either call site turns it into the
/// outcome type its own caller reads ([`CasOutcome`], [`TrustWriteOutcome`]).
enum Refusal {
    /// One position's condition failed: that input no longer holds the value the caller consumed.
    /// Carries the item's old image (`ALL_OLD`), from which each call site reads whatever
    /// attribute it reports — a head, or the anchor's bytes.
    Moved(Precondition, Option<HashMap<String, AttributeValue>>),
    /// Refused for a reason that establishes nothing about whether any input moved.
    Transient(TransientCause),
}

impl ConditionedTransaction {
    fn new() -> ConditionedTransaction {
        ConditionedTransaction { items: Vec::new(), layout: Vec::new() }
    }

    /// Add an action and record, in the same call, which precondition it conditions on.
    fn push(&mut self, item: TransactWriteItem, conditions_on: Precondition) {
        self.items.push(item);
        self.layout.push(conditions_on);
    }

    /// Issue one attempt of the transaction under `request_token`. `Ok(None)` committed;
    /// `Ok(Some(_))` was refused and attributed; `Err` is a fault.
    ///
    /// The partition is over the whole `SdkError` surface, not only its modelled branch (FORK-95
    /// design memo, claim C14):
    ///
    /// * `ConstructionFailure` was never dispatched, so nothing can have committed — a fault.
    /// * `TimeoutError`, `DispatchFailure` and `ResponseError` leave the attempt's fate unknown:
    ///   it may have committed. That is the case the request token exists for, so they are
    ///   transient and the head re-issues under the same token.
    /// * `ServiceError` is partitioned over `TransactWriteItemsError`'s variants by name, in
    ///   [`classify_service_error`].
    ///
    /// Anything else (`SdkError` is `#[non_exhaustive]`) is a fault rather than a guess.
    ///
    /// The SDK's own retry is disabled for this one call, and the attempt is bounded by
    /// [`TRANSACTION_ATTEMPT_TIMEOUT`]. Left on the shared default (`RetryConfig::standard()`,
    /// three attempts with 1 s and 2 s waits) one store call could be three billed transactions
    /// and several seconds the head's remaining-window check never saw. With it off, one call is
    /// one transaction and the head's budget is exact. Both transactions this store issues go
    /// through here, and nothing else in the crate overrides the client's configuration.
    ///
    /// The token is set explicitly because the SDK's own fill-in is per `send()`: it would keep
    /// the token across its internal retries but mint a fresh one for every re-issue the head
    /// makes, and a fresh token is what turns a re-issue of an attempt that already landed into
    /// a second transaction refused by the state the first one wrote.
    async fn send(
        self,
        client: &DynamoOps,
        label: &str,
        request_token: &str,
    ) -> Result<Option<Refusal>, String> {
        let request = client
            .transact_write_items()
            .set_transact_items(Some(self.items))
            .client_request_token(request_token)
            .customize()
            .config_override(
                aws_sdk_dynamodb::Config::builder()
                    .retry_config(RetryConfig::disabled())
                    .timeout_config(
                        TimeoutConfig::builder()
                            .operation_attempt_timeout(TRANSACTION_ATTEMPT_TIMEOUT)
                            .build(),
                    ),
            );

        match request.send().await {
            Ok(_) => Ok(None),
            Err(SdkError::ServiceError(context)) => {
                match classify_service_error(&self.layout, context.err()) {
                    Some(classified) => classified.map(Some),
                    None => Err(describe(label, SdkError::ServiceError(context))),
                }
            }
            Err(
                SdkError::TimeoutError(_) | SdkError::DispatchFailure(_) | SdkError::ResponseError(_),
            ) => Ok(Some(Refusal::Transient(TransientCause::FateUnknown))),
            Err(err) => Err(describe(label, err)),
        }
    }
}

/// Route a modelled `TransactWriteItems` error to an arm, naming every variant the pinned SDK
/// declares (FORK-95 design memo, claim C14). `None` is arm three — a fault, described by the
/// caller from the error itself — and is reached only by the variants listed for it, plus
/// whatever a later SDK adds (the enum is `#[non_exhaustive]`, and an unknown variant is not
/// guessed into a retry).
///
/// Every transient variant here was retried inside the SDK before its retry was scoped off at
/// this call, so naming each one is what keeps disabling that retry from silently turning any of
/// them into a `500`.
fn classify_service_error(
    layout: &[Precondition],
    err: &TransactWriteItemsError,
) -> Option<Result<Refusal, String>> {
    use TransactWriteItemsError as E;

    let cause = match err {
        E::TransactionCanceledException(failure) => {
            return Some(classify(layout, failure.cancellation_reasons()));
        }
        E::ProvisionedThroughputExceededException(_)
        | E::ThrottlingException(_)
        | E::RequestLimitExceeded(_) => TransientCause::Contended,
        // What a same-token re-issue meets while an earlier attempt is still running.
        E::TransactionInProgressException(_) => TransientCause::InProgress,
        // "An error occurred on the server side": nothing says whether the attempt applied.
        E::InternalServerError(_) => TransientCause::FateUnknown,
        // `IdempotentParameterMismatchException` means this request's token was reused with
        // different actions, which the head never does; `InvalidEndpointException`,
        // `ResourceNotFoundException` and anything unmodelled are configuration or service
        // faults that a re-issue cannot fix.
        _ => return None,
    };

    Some(Ok(Refusal::Transient(cause)))
}

/// Attribute a cancellation to the precondition it names, by position in `layout`.
///
/// A cancellation that names no moved precondition — every position reads "no error" (both the
/// literal string `"None"` and an absent code mean that; the pinned SDK model allows either, and
/// real DynamoDB sends `"None"`) — is reported as a fault rather than guessed at: attributing it
/// to one of the inputs would be wrong. This is not a corner. The reason list is positional, so
/// every cancellation either transaction can produce contains at least one no-error position, and
/// a three-action transaction refused on one condition contains two. What is forbidden is the
/// inverse — reading a no-error position as if it named the failure — not the recognition of an
/// absent code.
///
/// A list whose length does not match the number of actions submitted names no position reliably
/// and lands in the same fault arm, as does an absent or empty one.
///
/// `ConditionalCheckFailed` takes precedence over any transient code: a transaction that was
/// going to be refused for a moved input would be refused again on re-issue, so reporting the
/// transient code first would spend a retry budget on a `409` wearing a `503`'s clothes. Only
/// once no position failed a condition is the list read for contention — and then *every*
/// position that names a code must name a transient one (`TransactionConflict`,
/// `ThrottlingError`, `ProvisionedThroughputExceeded`). A list mixing a transient code with any
/// other, such as a `ValidationError`, would fail the same way on every re-issue, so it is a
/// fault, not a reason to spend the budget.
///
/// `TransactionConflict` is the transient cause these transactions most expect, because it is
/// contention they themselves introduce: both of them `ConditionCheck` the single `@office` item,
/// which every trusted lift in the warehouse also touches, so an office lift racing any other
/// pallet's push — or a re-genesis merely running alongside an ordinary push — can cancel one of
/// them for a reason that establishes nothing about whether it was ever actually wrong.
fn classify(layout: &[Precondition], reasons: &[CancellationReason]) -> Result<Refusal, String> {
    if reasons.len() != layout.len() {
        return Err(format!(
            "DynamoDB transact_write_items was cancelled with {} cancellation reason(s), \
            expected {}: the cancellation cannot be attributed to a specific precondition.",
            reasons.len(),
            layout.len()
        ));
    }

    for (precondition, reason) in layout.iter().zip(reasons) {
        if reason.code() == Some("ConditionalCheckFailed") {
            return Ok(Refusal::Moved(*precondition, reason.item().cloned()));
        }
    }

    let named: Vec<&str> =
        reasons.iter().filter_map(CancellationReason::code).filter(|code| *code != "None").collect();
    let transient = |code: &&str| {
        matches!(*code, "TransactionConflict" | "ThrottlingError" | "ProvisionedThroughputExceeded")
    };

    if !named.is_empty() && named.iter().all(transient) {
        return Ok(Refusal::Transient(TransientCause::Contended));
    }

    Err(format!(
        "DynamoDB transact_write_items was cancelled for a reason that names no moved \
        precondition: {:?}",
        reasons.iter().map(CancellationReason::code).collect::<Vec<_>>()
    ))
}

impl DynamoRefStore {
    /// The one transaction behind both pallet writers — [`RefStore::compare_and_set_head`] and
    /// [`RefStore::delete_head`]: the format fence, the action on the pallet's own item, plus the
    /// office and anchor `ConditionCheck`s.
    #[allow(clippy::too_many_arguments)]
    async fn commit(
        &self,
        entity: &str,
        write: PalletWrite<'_>,
        office_head: OfficePrecondition<'_>,
        redactions: RedactionsPrecondition<'_>,
        anchor: Option<&str>,
        chunk_fence: Option<Option<&str>>,
        format: &FormatFence,
        label: &str,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        let office_entity = pallet_entity(PalletNamespace::Meta, OFFICE_PALLET_NAME);
        // The office `ConditionCheck` is built only when neither of two independent reasons to
        // skip it holds. Structural: DynamoDB refuses a transaction where two actions target the
        // same item ("you cannot both `ConditionCheck` and `Update` the same item" — AWS API
        // Reference), and writing `@office` itself is that case — the office precondition IS the
        // pallet precondition the `Update` action already encodes. Semantic:
        // `OfficePrecondition::NotConsumed` means the caller never consumed an office head at all
        // (an untrusted warehouse never reads one — see `RefStore::compare_and_set_head`'s docs),
        // and conditioning on it anyway would refuse a commit over a state it never depended on.
        // `Unborn` is consumed, and conditions on the office still having no head. The match
        // names every variant, so a new one cannot silently fall into "not consumed".
        let office_condition = if entity == office_entity {
            None
        } else {
            match office_head {
                OfficePrecondition::NotConsumed => None,
                OfficePrecondition::Unborn => Some(None),
                OfficePrecondition::At(head) => Some(Some(head)),
            }
        };

        // The `@redactions` check is built by the same two rules: a write of `@redactions` itself
        // already conditions on its own item, and a check that read no `@redactions` head leaves
        // nothing to condition on.
        let redactions_entity = pallet_entity(PalletNamespace::Meta, REDACTIONS_PALLET_NAME);
        let redactions_condition = if entity == redactions_entity {
            None
        } else {
            match redactions {
                RedactionsPrecondition::NotConsumed => None,
                RedactionsPrecondition::Unborn => Some(None),
                RedactionsPrecondition::At(head) => Some(Some(head)),
            }
        };

        // Each action is pushed with the precondition it conditions on, so the layout `classify`
        // later reads is built here rather than restated there — see `ConditionedTransaction`.
        let mut transaction = ConditionedTransaction::new();

        transaction.push(self.format_action(format), Precondition::Format);
        transaction.push(self.pallet_update(entity, write), Precondition::PalletHead);

        if let Some(office_head) = office_condition {
            transaction.push(
                self.head_condition_check(&office_entity, office_head),
                Precondition::OfficeHead,
            );
        }

        if let Some(redactions_head) = redactions_condition {
            transaction.push(
                self.head_condition_check(&redactions_entity, redactions_head),
                Precondition::RedactionsHead,
            );
        }

        transaction.push(self.anchor_condition_check(anchor), Precondition::Anchor);

        if let Some(fence) = chunk_fence {
            transaction.push(self.state_condition_check(ENTITY_CHUNK_FENCE, fence), Precondition::ChunkFence);
        }

        let refusal = transaction.send(&self.client, label, request_token).await?;

        Ok(match refusal {
            None => CasOutcome::Committed,
            Some(Refusal::Transient(cause)) => CasOutcome::Transient(cause),
            Some(Refusal::Moved(Precondition::PalletHead, item)) => CasOutcome::Conflict {
                current: item.as_ref().map(ref_head_of).unwrap_or(RefHead::Absent),
            },
            Some(Refusal::Moved(Precondition::OfficeHead, item)) => {
                CasOutcome::OfficeMoved { current: item.as_ref().and_then(head_of) }
            }
            Some(Refusal::Moved(Precondition::RedactionsHead, _)) => CasOutcome::RedactionsMoved,
            Some(Refusal::Moved(Precondition::Anchor, _)) => CasOutcome::AnchorMoved,
            Some(Refusal::Moved(Precondition::Format, _)) => CasOutcome::FormatMoved,
            Some(Refusal::Moved(Precondition::ChunkFence, _)) => CasOutcome::ChunkFenceMoved,
            // No action of a ref update writes a state item.
            Some(Refusal::Moved(Precondition::Item, _)) => {
                return Err(format!("{} was refused on a state-item precondition it never built.", label))
            }
        })
    }
}

impl RefStore for DynamoRefStore {
    fn get_head(&self, namespace: PalletNamespace, name: &str) -> Result<RefHead, String> {
        let entity = pallet_entity(namespace, name);

        self.bridge.block_on(async {
            Ok(self.read_item(&entity).await?.as_ref().map(ref_head_of).unwrap_or(RefHead::Absent))
        })
    }

    fn read_format(&self) -> Result<Option<String>, String> {
        self.bridge.block_on(async {
            Ok(self
                .read_item(ENTITY_FORMAT)
                .await?
                .and_then(|item| item.get(ATTR_STATE).and_then(|value| value.as_s().ok()).cloned()))
        })
    }

    fn partition_has_items(&self) -> Result<bool, String> {
        self.bridge.block_on(async {
            let page = self
                .client
                .query()
                .table_name(&self.table)
                .key_condition_expression("#wh = :wh")
                .expression_attribute_names("#wh", ATTR_WAREHOUSE)
                .expression_attribute_values(":wh", s(self.warehouse.clone()))
                .consistent_read(true)
                .limit(1)
                .send()
                .await
                .map_err(|err| describe("DynamoDB query", err))?;

            Ok(!page.items().is_empty())
        })
    }

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
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        let entity = pallet_entity(namespace, name);
        let write = PalletWrite::Set { expected, new };

        self.bridge.block_on(self.commit(
            &entity,
            write,
            office_head,
            redactions,
            anchor,
            Some(chunk_fence),
            format,
            "DynamoDB ref-update commit",
            request_token,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn delete_head(
        &self,
        namespace: PalletNamespace,
        name: &str,
        old: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        format: &FormatFence,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        let entity = pallet_entity(namespace, name);
        let write = PalletWrite::Delete { old };

        // A delete adds no content, so it checks nothing against the redactions, and names nothing
        // a round of chunk deletion could delete: the census counts a tombstone as a live head.
        self.bridge.block_on(self.commit(
            &entity,
            write,
            office_head,
            RedactionsPrecondition::NotConsumed,
            anchor,
            None,
            format,
            "DynamoDB pallet delete",
            request_token,
        ))
    }

    fn list_refs(&self) -> Result<RefListing, String> {
        let mut listing = RefListing::default();

        for item in self.bridge.block_on(self.query_prefix(ENTITY_PALLET_PREFIX))? {
            let entity = item.get(ATTR_ENTITY).and_then(|value| value.as_s().ok());
            let Some(wire) = entity.and_then(|entity| entity.strip_prefix(ENTITY_PALLET_PREFIX)) else {
                continue;
            };

            match ref_head_of(&item) {
                RefHead::Live(head) => listing.live.push((PalletRef::parse(wire)?, head)),
                RefHead::Deleted(head) => listing.tombstones.push((PalletRef::parse(wire)?, head)),
                RefHead::Absent => {}
            }
        }

        Ok(listing)
    }

    fn list_withheld(&self) -> Result<Vec<(String, String)>, String> {
        let mut withheld = Vec::new();

        for item in self.bridge.block_on(self.query_prefix(ENTITY_WITHHELD_PREFIX))? {
            let entity = item.get(ATTR_ENTITY).and_then(|value| value.as_s().ok());
            let Some(hash) = entity.and_then(|entity| entity.strip_prefix(ENTITY_WITHHELD_PREFIX)) else {
                continue;
            };

            // An item with no readable state is not one this build wrote; listing it as absent
            // would serve content its host withholds.
            match item.get(ATTR_STATE).and_then(|value| value.as_s().ok()) {
                Some(state) => withheld.push((hash.to_string(), state.clone())),
                None => return Err(format!(
                    "The item {}{} carries no state this build can read.",
                    ENTITY_WITHHELD_PREFIX, hash
                )),
            }
        }

        Ok(withheld)
    }

    fn default_pallet(&self) -> Result<String, String> {
        Ok(self.default_pallet.clone())
    }

    fn get_item(&self, key: ItemKey<'_>) -> Result<Option<String>, String> {
        let entity = item_entity(key);

        self.bridge.block_on(async {
            let Some(item) = self.read_item(&entity).await? else {
                return Ok(None);
            };

            // An item with no readable state is not one this build wrote; reading it as absent
            // would start a redaction over as pending, or a round of chunk deletion over.
            match item.get(ATTR_STATE).and_then(|value| value.as_s().ok()) {
                Some(state) => Ok(Some(state.clone())),
                None => Err(format!("The item {} carries no state this build can read.", entity)),
            }
        })
    }

    fn put_items(&self, writes: &[ItemWrite<'_>], format: &FormatFence) -> Result<RedactionWrite, String> {
        let mut transaction = ConditionedTransaction::new();

        transaction.push(self.format_action(format), Precondition::Format);

        for write in writes {
            let mut item = self.key(&item_entity(write.key));
            item.insert(ATTR_STATE.to_string(), s(write.state));

            let builder = Put::builder()
                .set_item(Some(item))
                .table_name(&self.table)
                .return_values_on_condition_check_failure(ReturnValuesOnConditionCheckFailure::AllOld);

            // String equality on the stored bytes, as the anchor's condition is: whatever moved
            // them, the writer's decisions were read from something else.
            let put = match write.expected {
                Some(bytes) => builder
                    .condition_expression("#s = :s")
                    .expression_attribute_names("#s", ATTR_STATE)
                    .expression_attribute_values(":s", s(bytes)),
                None => builder
                    .condition_expression("attribute_not_exists(#e)")
                    .expression_attribute_names("#e", ATTR_ENTITY),
            }
            .build()
            .expect("every required Put field is set");

            transaction.push(TransactWriteItem::builder().put(put).build(), Precondition::Item);
        }

        self.bridge.block_on(async {
            let refusal = transaction.send(&self.client, "DynamoDB state-item write", &request_token()).await?;

            match refusal {
                None => Ok(RedactionWrite::Written),
                Some(Refusal::Transient(cause)) => Ok(RedactionWrite::Transient(cause)),
                Some(Refusal::Moved(Precondition::Format, _)) => Ok(RedactionWrite::FormatMoved),
                Some(Refusal::Moved(Precondition::Item, _)) => Ok(RedactionWrite::Moved),
                Some(Refusal::Moved(precondition, _)) => Err(format!(
                    "DynamoDB state-item write was refused on a {:?} precondition it never built.",
                    precondition
                )),
            }
        })
    }

    fn get_trust(&self) -> Result<Option<(TrustAnchor, String)>, String> {
        self.bridge.block_on(async {
            let Some(item) = self.read_item(ENTITY_TRUST).await? else {
                return Ok(None);
            };

            let Some(json) = item.get(ATTR_ANCHOR).and_then(|value| value.as_s().ok()) else {
                return Ok(None);
            };

            let dto: TrustAnchorDto = serde_json::from_str(json)
                .map_err(|err| format!("decoding the stored trust anchor failed: {}", err))?;

            // The decoded anchor is what an audit consumes; `json` — the exact bytes this item
            // holds — is what `compare_and_set_head`'s anchor precondition later compares by
            // string equality (this module's docs / claim C13). Never re-serialize `dto` here
            // in its place: a `serde` field reorder must not fail a commit for no anchor that
            // actually moved.
            Ok(Some((dto.to_anchor(), json.clone())))
        })
    }

    fn put_trust_if_absent(
        &self,
        anchor: &TrustAnchor,
        format: &FormatFence,
        request_token: &str,
    ) -> Result<TrustOutcome, String> {
        let dto = TrustAnchorDto::from(anchor);
        let json = serde_json::to_string(&dto)
            .map_err(|err| format!("encoding the trust anchor failed: {}", err))?;

        self.bridge.block_on(async {
            let mut item = self.key(ENTITY_TRUST);
            item.insert(ATTR_ANCHOR.to_string(), s(json));

            // The one-way door: plant the anchor only when none exists.
            let put = Put::builder()
                .set_item(Some(item))
                .table_name(&self.table)
                .condition_expression("attribute_not_exists(#e)")
                .expression_attribute_names("#e", ATTR_ENTITY)
                .return_values_on_condition_check_failure(ReturnValuesOnConditionCheckFailure::AllOld)
                .build()
                .expect("every required Put field is set");

            let mut transaction = ConditionedTransaction::new();

            transaction.push(self.format_action(format), Precondition::Format);
            transaction.push(TransactWriteItem::builder().put(put).build(), Precondition::Anchor);

            let refusal = transaction.send(&self.client, "DynamoDB trust plant", request_token).await?;

            match refusal {
                None => Ok(TrustOutcome::Established),
                Some(Refusal::Transient(cause)) => Ok(TrustOutcome::Transient(cause)),
                Some(Refusal::Moved(Precondition::Format, _)) => Ok(TrustOutcome::FormatMoved),
                // An anchor already exists. Idempotent for the identical one, refused for a
                // different one — the fake's exact split, decided by comparing the incumbent DTO
                // with the incoming one.
                Some(Refusal::Moved(Precondition::Anchor, item)) => match item.as_ref().and_then(anchor_of) {
                    Some(existing_json) => {
                        let existing_dto: TrustAnchorDto = serde_json::from_str(&existing_json)
                            .map_err(|err| format!("decoding the stored trust anchor failed: {}", err))?;

                        if existing_dto == dto {
                            Ok(TrustOutcome::AlreadyIdentical)
                        } else {
                            Ok(TrustOutcome::Conflict)
                        }
                    }
                    None => Ok(TrustOutcome::Conflict),
                },
                Some(Refusal::Moved(precondition, _)) => Err(format!(
                    "DynamoDB trust plant was refused on a {:?} precondition it never built.",
                    precondition
                )),
            }
        })
    }

    fn replace_trust(
        &self,
        anchor: &TrustAnchor,
        expected_anchor: &str,
        office_head: Option<&str>,
        format: &FormatFence,
        request_token: &str,
    ) -> Result<TrustWriteOutcome, String> {
        let dto = TrustAnchorDto::from(anchor);
        let json = serde_json::to_string(&dto)
            .map_err(|err| format!("encoding the trust anchor failed: {}", err))?;
        let office_entity = pallet_entity(PalletNamespace::Meta, OFFICE_PALLET_NAME);

        self.bridge.block_on(async {
            // The format fence and two actions, and no fourth: this write conditions on the two inputs `put_trust`'s
            // validation consumed, and on nothing else. Unlike the ref-update commit there is no
            // case that collapses to one item — the trust item and the office item are always
            // distinct — so the office check is never dropped here.
            let mut transaction = ConditionedTransaction::new();

            transaction.push(self.format_action(format), Precondition::Format);
            transaction.push(self.trust_put(&json, expected_anchor), Precondition::Anchor);
            transaction.push(
                self.head_condition_check(&office_entity, office_head),
                Precondition::OfficeHead,
            );

            let refusal = transaction.send(&self.client, "DynamoDB anchor write", request_token).await?;

            Ok(match refusal {
                None => TrustWriteOutcome::Replaced,
                Some(Refusal::Transient(cause)) => TrustWriteOutcome::Transient(cause),
                Some(Refusal::Moved(Precondition::Anchor, item)) => {
                    let current = item.as_ref().and_then(anchor_of);

                    // The incumbent moved — but if it moved *to what this call was going to
                    // write*, the caller's desired state holds and there is no conflict to
                    // report. See `TrustWriteOutcome::AlreadyIdentical`.
                    //
                    // Compared as decoded values, not as bytes, because that is the comparison
                    // its three siblings make for the same question — `put_trust_if_absent` in
                    // both stores, and `put_trust`'s own read-side check. A byte comparison is
                    // strictly narrower and would answer `409` for an incumbent that differs
                    // only in encoding, which is the same spurious refusal this variant exists
                    // to remove. An incumbent that will not decode falls through to
                    // `AnchorMoved` rather than erroring: this is a refusal already decided, and
                    // a `?` here would turn today's `409` into a `500`.
                    let identical = current
                        .as_deref()
                        .and_then(|bytes| serde_json::from_str::<TrustAnchorDto>(bytes).ok())
                        .is_some_and(|incumbent| incumbent == dto);

                    if identical {
                        TrustWriteOutcome::AlreadyIdentical
                    } else {
                        TrustWriteOutcome::AnchorMoved { current }
                    }
                }
                Some(Refusal::Moved(Precondition::OfficeHead, item)) => {
                    TrustWriteOutcome::OfficeMoved { current: item.as_ref().and_then(head_of) }
                }
                Some(Refusal::Moved(Precondition::Format, _)) => TrustWriteOutcome::FormatMoved,
                // No action of this transaction conditions on a pallet head, so `classify` cannot
                // name one: the layout it reads is the one pushed above.
                Some(Refusal::Moved(Precondition::PalletHead, _)) => {
                    return Err("DynamoDB anchor write was refused on a pallet-head \
                        precondition it never built."
                        .to_string())
                }
                Some(Refusal::Moved(Precondition::RedactionsHead, _)) => {
                    return Err("DynamoDB anchor write was refused on a @redactions \
                        precondition it never built."
                        .to_string())
                }
                Some(Refusal::Moved(precondition @ (Precondition::Item | Precondition::ChunkFence), _)) => {
                    return Err(format!(
                        "DynamoDB anchor write was refused on a {:?} precondition it never built.",
                        precondition
                    ))
                }
            })
        })
    }
}

/// The waits before each attempt of a format swap: the first at once, then backing off past what
/// a burst of pushes holds the format item for.
const FORMAT_SWAP_WAITS: [Duration; 6] = [
    Duration::ZERO,
    Duration::from_millis(100),
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];

impl FormatStore for DynamoRefStore {
    /// One conditional write of the format item, as a single-action transaction: this crate
    /// issues every conditional write through `TransactWriteItems`. Every ref and trust write
    /// checks the same item, so a busy warehouse refuses the swap transiently; it is re-issued
    /// under the same token, which makes an attempt that landed answer success rather than a
    /// moved state. Refused through every attempt, it is an error.
    fn swap_format(&self, expected: &str, new: &str) -> Result<bool, String> {
        let update = Update::builder()
            .set_key(Some(self.key(ENTITY_FORMAT)))
            .table_name(&self.table)
            .update_expression("SET #s = :new")
            .condition_expression("#s = :old")
            .expression_attribute_names("#s", ATTR_STATE)
            .expression_attribute_values(":new", s(new))
            .expression_attribute_values(":old", s(expected))
            .build()
            .expect("every required Update field is set");
        let token = request_token();

        self.bridge.block_on(async {
            // Once an attempt's fate is unknown, or one is still running, the swap may land
            // however the rest end: what the operator is told next has to say so.
            let mut uncertain = false;
            let may_have_landed = |message: String| {
                format!(
                    "{} The write may still have landed, leaving the format item holding \"{}\". \
                    Run the upgrade again: it reads the state on record and says what is left.",
                    message, new
                )
            };

            for wait in FORMAT_SWAP_WAITS {
                tokio::time::sleep(wait).await;

                let mut transaction = ConditionedTransaction::new();
                transaction.push(TransactWriteItem::builder().update(update.clone()).build(), Precondition::Format);

                match transaction.send(&self.client, "DynamoDB format swap", &token).await {
                    Ok(None) => return Ok(true),
                    Ok(Some(Refusal::Moved(..))) => return Ok(false),
                    Ok(Some(Refusal::Transient(TransientCause::Contended))) => {}
                    Ok(Some(Refusal::Transient(TransientCause::InProgress | TransientCause::FateUnknown))) => {
                        uncertain = true
                    }
                    Err(error) if uncertain => return Err(may_have_landed(error)),
                    Err(error) => return Err(error),
                }
            }

            match uncertain {
                true => Err(may_have_landed(
                    "DynamoDB could not confirm the format item's write in any attempt.".to_string(),
                )),
                false => Err("DynamoDB refused the format item's write through every attempt: the \
                    warehouse stayed busy. Nothing was changed; run the upgrade again."
                    .to_string()),
            }
        })
    }
}

/// Every warehouse id with an item in `table`: a full `Scan`, projecting only the partition key.
/// Only the upgrade operator's `--all` calls it, under a role that grants `Scan`, which neither
/// Lambda's does.
pub fn list_warehouses(client: &DynamoOps, table: &str, bridge: &AsyncBridge) -> Result<Vec<String>, String> {
    bridge.block_on(async {
        let mut warehouses = std::collections::BTreeSet::new();
        let mut start_key: Option<HashMap<String, AttributeValue>> = None;

        loop {
            let mut request = client
                .scan()
                .table_name(table)
                .consistent_read(true)
                .projection_expression("#wh")
                .expression_attribute_names("#wh", ATTR_WAREHOUSE);

            if let Some(key) = start_key.take() {
                request = request.set_exclusive_start_key(Some(key));
            }

            let page = request.send().await.map_err(|err| describe("DynamoDB scan", err))?;

            for item in page.items() {
                if let Some(id) = item.get(ATTR_WAREHOUSE).and_then(|value| value.as_s().ok()) {
                    warehouses.insert(id.clone());
                }
            }

            match page.last_evaluated_key() {
                Some(key) if !key.is_empty() => start_key = Some(key.clone()),
                _ => break,
            }
        }

        Ok(warehouses.into_iter().collect())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table's key attributes are the deployment's key schema, which no upgrade step can change:
    /// a new one is a new table. The items under them are frozen per AWS format by
    /// `tests/integration/aws_format.rs`, on the memory store, which keeps them under these same
    /// names.
    #[test]
    fn the_key_schema_is_the_deployments() {
        assert_eq!((ATTR_WAREHOUSE, ATTR_ENTITY), ("wh", "entity"));
    }

    #[test]
    fn pallet_entities_qualify_the_wire_form_and_stay_distinct_across_namespaces() {
        assert_eq!(pallet_entity(PalletNamespace::User, "main"), "pallet#main");
        assert_eq!(pallet_entity(PalletNamespace::Meta, "office"), "pallet#@office");

        // A user pallet and a meta pallet of the same bare name never collide.
        assert_ne!(
            pallet_entity(PalletNamespace::User, "office"),
            pallet_entity(PalletNamespace::Meta, "office"),
        );

        // Every pallet entity is caught by the enumeration prefix; the trust item is not.
        assert!(pallet_entity(PalletNamespace::User, "main").starts_with(ENTITY_PALLET_PREFIX));
        assert!(!ENTITY_TRUST.starts_with(ENTITY_PALLET_PREFIX));
    }

    #[test]
    fn a_pallet_entity_round_trips_through_the_enumeration_prefix() {
        for (namespace, name) in
            [(PalletNamespace::User, "feature/x"), (PalletNamespace::Meta, "office")]
        {
            let entity = pallet_entity(namespace, name);
            let wire = entity.strip_prefix(ENTITY_PALLET_PREFIX).expect("the prefix");
            let parsed = PalletRef::parse(wire).expect("a valid ref");

            assert_eq!(parsed.namespace, namespace);
            assert_eq!(parsed.name, name);
        }
    }

    #[test]
    fn head_of_reads_the_head_attribute_and_tolerates_its_absence() {
        let with_head =
            HashMap::from([(ATTR_HEAD.to_string(), s("abc123"))]);
        assert_eq!(head_of(&with_head).as_deref(), Some("abc123"));

        let without_head =
            HashMap::from([(ATTR_ANCHOR.to_string(), s("{}"))]);
        assert_eq!(head_of(&without_head), None);
    }

    /// A synthetic cancellation reason carrying `code` and nothing else — no SDK call, no
    /// network. Mirrors `aws/s3.rs`'s `synthetic_head_object_error` pattern for the same
    /// purpose: exercising a classifier against constructed SDK types.
    fn reason(code: &str) -> CancellationReason {
        CancellationReason::builder().code(code).build()
    }

    /// The ref-update commit's layout, as `compare_and_set_head` pushes it when every action is
    /// built.
    fn ref_update_layout() -> Vec<Precondition> {
        vec![Precondition::PalletHead, Precondition::OfficeHead, Precondition::Anchor]
    }

    /// The anchor write's layout, as `replace_trust` pushes it.
    fn anchor_write_layout() -> Vec<Precondition> {
        vec![Precondition::Anchor, Precondition::OfficeHead]
    }

    /// PR #116 review, finding 1: `TransactionConflict` — DynamoDB's own concurrency control
    /// when another transaction concurrently touches one of this commit's items (the shared
    /// `@office` item, principally) — must classify as `Transient`, not fall to the generic
    /// fault this function's `Err` branch answers as a `500`. No position failed a condition,
    /// so nothing here is attributable to a moved value.
    #[test]
    fn a_transaction_conflict_with_no_moved_precondition_is_transient() {
        let reasons = vec![reason("None"), reason("TransactionConflict"), reason("None")];

        assert!(matches!(
            classify(&ref_update_layout(), &reasons).expect("a transient cause, not a fault"),
            Refusal::Transient(TransientCause::Contended)
        ));
    }

    /// Throttling arrives as a cancellation code too, not only as a top-level variant, and is
    /// re-issued like a conflict rather than answered as a fault.
    #[test]
    fn throttle_cancellation_codes_are_transient() {
        for code in ["ThrottlingError", "ProvisionedThroughputExceeded"] {
            let reasons = vec![reason("None"), reason(code), reason("TransactionConflict")];

            assert!(
                matches!(
                    classify(&ref_update_layout(), &reasons),
                    Ok(Refusal::Transient(TransientCause::Contended))
                ),
                "{code}"
            );
        }
    }

    /// A transient code alongside one that would fail every re-issue identically is a fault: the
    /// re-issue loop would only spend the budget reaching the same refusal.
    #[test]
    fn a_transient_code_beside_a_permanent_one_is_a_fault_not_a_retry() {
        let reasons =
            vec![reason("TransactionConflict"), reason("ValidationError"), reason("None")];

        assert!(classify(&ref_update_layout(), &reasons).is_err());
    }

    /// Every modelled variant the pinned SDK declares is routed by name: the five transient ones
    /// to the cause that sets their wait, and the rest to the fault arm. Disabling the SDK's own
    /// retry at the call site is safe only because none of the five falls through to a `500`.
    #[test]
    fn every_modelled_transact_write_items_error_is_routed_by_name() {
        use aws_sdk_dynamodb::types::error::{
            IdempotentParameterMismatchException, InternalServerError, InvalidEndpointException,
            ProvisionedThroughputExceededException, RequestLimitExceeded,
            ResourceNotFoundException, ThrottlingException, TransactionInProgressException,
        };
        use TransactWriteItemsError as E;

        let transient = [
            (
                E::ProvisionedThroughputExceededException(
                    ProvisionedThroughputExceededException::builder().build(),
                ),
                TransientCause::Contended,
            ),
            (E::ThrottlingException(ThrottlingException::builder().build()), TransientCause::Contended),
            (E::RequestLimitExceeded(RequestLimitExceeded::builder().build()), TransientCause::Contended),
            (
                E::TransactionInProgressException(TransactionInProgressException::builder().build()),
                TransientCause::InProgress,
            ),
            (E::InternalServerError(InternalServerError::builder().build()), TransientCause::FateUnknown),
        ];

        for (err, expected) in transient {
            match classify_service_error(&ref_update_layout(), &err) {
                Some(Ok(Refusal::Transient(cause))) => assert_eq!(cause, expected, "{err:?}"),
                _ => panic!("{err:?} must be transient"),
            }
        }

        let faults = [
            E::IdempotentParameterMismatchException(
                IdempotentParameterMismatchException::builder().build(),
            ),
            E::InvalidEndpointException(InvalidEndpointException::builder().build()),
            E::ResourceNotFoundException(ResourceNotFoundException::builder().build()),
            E::generic(aws_smithy_types::error::ErrorMetadata::builder().code("Mystery").build()),
        ];

        for err in faults {
            assert!(classify_service_error(&ref_update_layout(), &err).is_none(), "{err:?}");
        }
    }

    /// `ConditionalCheckFailed` takes precedence over a `TransactionConflict` elsewhere in the
    /// list: a transaction that was going to be refused for a moved input would be refused
    /// again on re-issue, so reporting the transient code instead would answer `503` for what
    /// is really a `409`.
    #[test]
    fn a_moved_precondition_is_attributed_even_alongside_a_transaction_conflict() {
        let reasons = vec![
            reason("ConditionalCheckFailed"),
            reason("TransactionConflict"),
            reason("None"),
        ];

        assert!(matches!(
            classify(&ref_update_layout(), &reasons).expect("attributed to the pallet"),
            Refusal::Moved(Precondition::PalletHead, None)
        ));
    }

    /// The classifier reads the layout it is given rather than any fixed positional map, so the
    /// *same* index means a different input in the two transactions this store builds.
    ///
    /// This is the property that lets one classifier serve both, and it is the one a second
    /// hand-written copy would have got wrong: position 0 is the pallet's head in a ref-update
    /// commit and the trust anchor in an anchor write. A classifier hard-coding the ref-update
    /// map would report a moved *pallet* for a re-genesis that lost its race — a `409` about a
    /// pallet the request never named.
    #[test]
    fn the_same_position_names_a_different_input_in_each_transaction() {
        let refused_at_zero = vec![reason("ConditionalCheckFailed"), reason("None")];

        assert!(matches!(
            classify(&anchor_write_layout(), &refused_at_zero).expect("attributed"),
            Refusal::Moved(Precondition::Anchor, None)
        ));

        let refused_at_one = vec![reason("None"), reason("ConditionalCheckFailed")];

        assert!(matches!(
            classify(&anchor_write_layout(), &refused_at_one).expect("attributed"),
            Refusal::Moved(Precondition::OfficeHead, None)
        ));
    }

    /// A reason list whose length does not match the actions submitted names no position
    /// reliably, so it is a fault rather than a guess. The length comes from the layout, so this
    /// check cannot drift from the transaction it guards.
    #[test]
    fn a_reason_list_of_the_wrong_length_is_a_fault() {
        let reasons = vec![reason("ConditionalCheckFailed"), reason("None"), reason("None")];

        assert!(classify(&anchor_write_layout(), &reasons).is_err());
    }
}
