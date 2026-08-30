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
//! | `<warehouse>` | `trust`          | `anchor = <json>`  | the trust anchor               |
//!
//! The pallet sort key is `pallet#{qualified-ref}` — the same wire form the fake keys on
//! (`main`, `@office`), unique across the two namespaces. Partitioning by warehouse makes
//! [`list_refs`](RefStore::list_refs) a single `Query` (`wh = … AND begins_with(entity,
//! "pallet#")`) rather than a full-table `Scan` — the explicit ref enumeration the trait
//! calls for, since object storage has no directory walk.
//!
//! The table's key schema must be `wh` (S, partition) and `entity` (S, sort); a deployment
//! provisions it that way (the integration test creates exactly this table).
//!
//! # The CAS
//!
//! [`compare_and_set_head`](RefStore::compare_and_set_head) is a real DynamoDB conditional
//! write, never a read-then-write — and, as of FORK-95, never a single-item one either. It is
//! a `TransactWriteItems` of up to three actions: an `Update` on the target pallet's own item
//! whose `ConditionExpression` encodes the caller's `expected` (exactly the prior single-item
//! shape), a `ConditionCheck` on the office pallet's item at the office head the caller's
//! audit consumed, and a `ConditionCheck` on the trust item at the anchor's stored
//! serialization. The office check is skipped for two independent reasons: the target pallet
//! *is* the office pallet (the `Update` above already pins that item; DynamoDB refuses two
//! actions on one item), or the caller's audit never read an office head at all (an untrusted
//! warehouse's audit never touches it) — see `RefStore::compare_and_set_head`'s docs. DynamoDB
//! either applies the whole transaction with every condition holding or applies nothing; on a
//! condition failure `ReturnValuesOnConditionCheckFailure=ALL_OLD` hands back the failed item,
//! so the store reports which precondition moved without a second round trip.
//! [`put_trust_if_absent`](RefStore::put_trust_if_absent) is still the single-item shape: a
//! conditional `PutItem` guarding the one-way trust door.
//!
//! # The anchor write
//!
//! [`replace_trust`](RefStore::replace_trust) — the one sanctioned overwrite of the anchor, a
//! re-genesis — is the store's *second* transaction, and for the same reason: `put_trust`
//! validates the chain of custody against values that can move before the write lands. It is a
//! `TransactWriteItems` of exactly two actions: a `Put` on the trust item conditioned on the
//! incumbent anchor's stored bytes, and a `ConditionCheck` on the office item at the head the
//! `adopts` test consumed. Unlike the CAS above, neither action is ever dropped — the trust item
//! and the office item are always distinct, so the collapse-to-one-item case cannot arise. It
//! was a bare unconditional `PutItem` until FORK-95 slice 4, which is how two concurrent
//! re-geneses could both pass the same `prior_genesis` test and both write.
//!
//! Cancellation reasons on a `TransactionCanceledException` are ordered by the order actions
//! were requested (AWS API Reference and the pinned `aws-sdk-dynamodb` model agree on this),
//! so the index an action is pushed at in [`DynamoRefStore::compare_and_set_head`] is the
//! index its refusal comes back at — see that method's own comment for exactly where that
//! ordering is encoded.

use std::collections::HashMap;

use aws_sdk_dynamodb::operation::put_item::PutItemError;
use aws_sdk_dynamodb::operation::transact_write_items::TransactWriteItemsError;
use aws_sdk_dynamodb::types::{
    AttributeValue, CancellationReason, ConditionCheck, Put, ReturnValuesOnConditionCheckFailure,
    TransactWriteItem, Update,
};

use forklift_core::model::remote::TrustAnchorDto;
use forklift_core::util::office_utils::{TrustAnchor, OFFICE_PALLET_NAME};
use forklift_core::util::pallet_utils::{PalletNamespace, PalletRef};

use crate::aws::dynamo_ops::DynamoOps;
use crate::aws::sdk::describe;
use crate::blocking::AsyncBridge;
use crate::store::{
    CasOutcome, OfficePrecondition, RefStore, TrustOutcome, TrustWriteOutcome,
};

/// The partition-key attribute: the warehouse id.
const ATTR_WAREHOUSE: &str = "wh";
/// The sort-key attribute: the item kind (`pallet#…` or `trust`).
const ATTR_ENTITY: &str = "entity";
/// A pallet item's head hash.
const ATTR_HEAD: &str = "head";
/// A trust item's anchor, as a JSON string.
const ATTR_ANCHOR: &str = "anchor";

/// The sort-key prefix of every pallet head; `begins_with` on it is the ref enumeration.
const ENTITY_PALLET_PREFIX: &str = "pallet#";
/// The sort key of the single trust item.
const ENTITY_TRUST: &str = "trust";

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
    async fn get_item(
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

    /// The `Update` action against the target pallet's own item: `SET head = :new`, guarded by
    /// `expected` exactly as the pre-FORK-95 single-item `UpdateItem` was. Always position 0 of
    /// the transaction `compare_and_set_head` builds.
    fn pallet_update(&self, entity: &str, expected: Option<&str>, new: &str) -> TransactWriteItem {
        let mut builder = Update::builder()
            .set_key(Some(self.key(entity)))
            .table_name(&self.table)
            .update_expression("SET #h = :new")
            .expression_attribute_names("#h", ATTR_HEAD)
            .expression_attribute_values(":new", s(new))
            .return_values_on_condition_check_failure(ReturnValuesOnConditionCheckFailure::AllOld);

        builder = match expected {
            // The pallet must currently hold exactly `old`. A missing item fails this too
            // (there is no `head` to equal `old`), reported as a conflict with no current
            // head — the fake's `current (None) != expected (Some)` branch.
            Some(old) => builder
                .condition_expression("#h = :old")
                .expression_attribute_values(":old", s(old)),
            // The pallet must not exist yet.
            None => builder
                .condition_expression("attribute_not_exists(#e)")
                .expression_attribute_names("#e", ATTR_ENTITY),
        };

        TransactWriteItem::builder()
            .update(builder.build().expect("every required Update field is set"))
            .build()
    }

    /// The `ConditionCheck` on the office pallet's item: its head must still equal
    /// `office_head` exactly. Built only when the caller's audit actually consumed an office
    /// head — see `compare_and_set_head`, which skips this action both when the target *is*
    /// `@office` itself (the `Update` above already pins that exact item; AWS refuses two
    /// actions on one item in the same transaction) and when the caller passed no office head
    /// at all, meaning its audit never read one. There is deliberately no "office pallet must
    /// be unborn" mode here: the only reachable caller of this method already holds a real
    /// head (an audit that found the office pallet missing refuses before reaching the
    /// commit — see `head.rs::ref_update`), so a fabricated `attribute_not_exists` condition
    /// would test a state no audit this store serves ever actually consumed.
    /// The anchor's own write is the caller that *does* reach the `None` arm, and legitimately:
    /// `put_trust` reads the office head unconditionally to run its `adopts` check, so an anchor
    /// adopting nothing is one whose validation genuinely consumed "the office is unborn" and
    /// must condition on it. The distinction is carried in the two callers' own parameter types —
    /// [`OfficePrecondition`] where a head may go unconsumed, a plain `Option` where it never
    /// does — not in this method, which only builds what it is told.
    fn office_condition_check(
        &self,
        office_entity: &str,
        office_head: Option<&str>,
    ) -> TransactWriteItem {
        let mut builder = ConditionCheck::builder()
            .set_key(Some(self.key(office_entity)))
            .table_name(&self.table)
            .return_values_on_condition_check_failure(ReturnValuesOnConditionCheckFailure::AllOld);

        builder = match office_head {
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

    /// The `Put` action that replaces the trust item, guarded on the incumbent anchor's **stored
    /// bytes** being exactly what the caller read. Always position 0 of the transaction
    /// `replace_trust` builds.
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
    /// The target pallet's own head, as the caller's `expected` names it.
    PalletHead,
    /// The office pallet's head, as the caller's audit or validation consumed it.
    OfficeHead,
    /// The trust anchor's stored bytes, as [`RefStore::get_trust`] returned them.
    Anchor,
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
    Transient,
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

    /// Issue the transaction. `Ok(None)` committed; `Ok(Some(_))` was cancelled and attributed;
    /// `Err` is a fault.
    ///
    /// Only `SdkError::ServiceError` carries a modelled error at all, so the four other
    /// `SdkError` variants — construction, timeout, dispatch and response failures — reach the
    /// fault arm here. Three of them leave the transaction's fate genuinely unknown to the caller
    /// and belong on a transient arm with an idempotency token behind it, which is FORK-95's
    /// retry slice; routing them there before that machinery exists would answer `503` to a
    /// client with nothing able to act on it, widening a debt this crate already carries rather
    /// than paying it.
    async fn send(self, client: &DynamoOps, label: &str) -> Result<Option<Refusal>, String> {
        let request = client.transact_write_items().set_transact_items(Some(self.items));

        match request.send().await {
            Ok(_) => Ok(None),
            Err(err) => match err.as_service_error() {
                Some(TransactWriteItemsError::TransactionCanceledException(failure)) => {
                    classify(&self.layout, failure.cancellation_reasons()).map(Some)
                }
                _ => Err(describe(label, err)),
            },
        }
    }
}

/// Attribute a cancellation to the precondition it names, by position in `layout`.
///
/// A cancellation that names no moved precondition — every position reads "no error" (both the
/// literal string `"None"` and an absent code mean that; the API Reference and the pinned SDK
/// model disagree on which one DynamoDB actually sends, so both must read the same way here) — is
/// reported as a fault rather than guessed at: attributing it to one of the inputs would be
/// wrong. This is not a corner. The reason list is positional, so every cancellation either
/// transaction can produce contains at least one no-error position, and a three-action
/// transaction refused on one condition contains two. What is forbidden is the inverse — reading
/// a no-error position as if it named the failure — not the recognition of an absent code.
///
/// A list whose length does not match the number of actions submitted names no position reliably
/// and lands in the same fault arm, as does an absent or empty one.
///
/// `ConditionalCheckFailed` takes precedence over any transient code: a transaction that was
/// going to be refused for a moved input would be refused again on re-issue, so reporting the
/// transient code first would spend a retry budget on a `409` wearing a `503`'s clothes. Only
/// once no position failed a condition does a `TransactionConflict` anywhere in the list read as
/// contention rather than a moved value.
///
/// `TransactionConflict` is the one transient cause classified explicitly rather than falling
/// into the fault arm, because it is not merely inherited risk but **contention these
/// transactions themselves introduce**: both of them `ConditionCheck` the single `@office` item,
/// which every trusted lift in the warehouse also touches, so an office lift racing any other
/// pallet's push — or a re-genesis merely running alongside an ordinary push — can cancel one of
/// them for a reason that establishes nothing about whether it was ever actually wrong. See
/// [`CasOutcome::Transient`] for why that is answered as a retryable refusal rather than a fault.
/// Every other transient cause still falls to the fault arm: FORK-95's retry slice inherits the
/// loop that answers all of them uniformly and gives it a request token to de-duplicate against.
///
/// **Recorded gap, not built here**: neither transaction carries a `ClientRequestToken`.
/// `TransactWriteItems` is the one DynamoDB call AWS documents an idempotency token for; without
/// one, an SDK-level retry of a request that actually committed re-issues it as a *new*
/// transaction whose own conditions then fail against the state it just wrote, and the client is
/// told a value moved for a write that succeeded. This is **not a regression** — the single-item
/// writes these replaced had the identical hazard for the identical reason — but it is real, and
/// giving it a token per sub-cause is exactly the policy FORK-95's retry slice owns.
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

    if reasons.iter().any(|reason| reason.code() == Some("TransactionConflict")) {
        return Ok(Refusal::Transient);
    }

    Err(format!(
        "DynamoDB transact_write_items was cancelled for a reason that names no moved \
        precondition: {:?}",
        reasons.iter().map(CancellationReason::code).collect::<Vec<_>>()
    ))
}

impl RefStore for DynamoRefStore {
    fn get_head(&self, namespace: PalletNamespace, name: &str) -> Result<Option<String>, String> {
        let entity = pallet_entity(namespace, name);

        self.bridge.block_on(async {
            Ok(self.get_item(&entity).await?.as_ref().and_then(head_of))
        })
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
        let entity = pallet_entity(namespace, name);
        let office_entity = pallet_entity(PalletNamespace::Meta, OFFICE_PALLET_NAME);
        // The office `ConditionCheck` is built for two independent reasons at once, both of
        // which must hold. Structural: DynamoDB refuses a transaction where two actions target
        // the same item ("you cannot both `ConditionCheck` and `Update` the same item" — AWS
        // API Reference), and lifting `@office` itself is that case — the office precondition
        // IS the pallet precondition the `Update` action already encodes. Semantic:
        // `OfficePrecondition::NotConsumed` means the caller's own audit never consumed an
        // office head at all (an untrusted warehouse never reads one — see
        // `RefStore::compare_and_set_head`'s docs), and conditioning on it anyway would refuse
        // a commit over a state the audit never depended on.
        let office_head_value = match office_head {
            OfficePrecondition::At(head) if entity != office_entity => Some(head),
            _ => None,
        };
        self.bridge.block_on(async {
            // Each action is pushed with the precondition it conditions on, so the layout
            // `classify` later reads is built here rather than restated there — see
            // `ConditionedTransaction`.
            let mut transaction = ConditionedTransaction::new();

            transaction.push(self.pallet_update(&entity, expected, new), Precondition::PalletHead);

            if let Some(office_head) = office_head_value {
                transaction.push(
                    self.office_condition_check(&office_entity, Some(office_head)),
                    Precondition::OfficeHead,
                );
            }

            transaction.push(self.anchor_condition_check(anchor), Precondition::Anchor);

            let refusal = transaction.send(&self.client, "DynamoDB ref-update commit").await?;

            Ok(match refusal {
                None => CasOutcome::Committed,
                Some(Refusal::Transient) => CasOutcome::Transient,
                Some(Refusal::Moved(Precondition::PalletHead, item)) => {
                    CasOutcome::Conflict { current: item.as_ref().and_then(head_of) }
                }
                Some(Refusal::Moved(Precondition::OfficeHead, item)) => {
                    CasOutcome::OfficeMoved { current: item.as_ref().and_then(head_of) }
                }
                Some(Refusal::Moved(Precondition::Anchor, _)) => CasOutcome::AnchorMoved,
            })
        })
    }

    fn list_refs(&self) -> Result<Vec<(PalletRef, String)>, String> {
        self.bridge.block_on(async {
            let mut refs = Vec::new();
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
                    .expression_attribute_values(":prefix", s(ENTITY_PALLET_PREFIX));

                if let Some(key) = start_key.take() {
                    request = request.set_exclusive_start_key(Some(key));
                }

                let page =
                    request.send().await.map_err(|err| describe("DynamoDB query", err))?;

                for item in page.items() {
                    let entity = item.get(ATTR_ENTITY).and_then(|value| value.as_s().ok());
                    let head = head_of(item);

                    if let (Some(entity), Some(head)) = (entity, head) {
                        if let Some(wire) = entity.strip_prefix(ENTITY_PALLET_PREFIX) {
                            refs.push((PalletRef::parse(wire)?, head));
                        }
                    }
                }

                match page.last_evaluated_key() {
                    Some(key) if !key.is_empty() => start_key = Some(key.clone()),
                    _ => break,
                }
            }

            Ok(refs)
        })
    }

    fn default_pallet(&self) -> Result<String, String> {
        Ok(self.default_pallet.clone())
    }

    fn get_trust(&self) -> Result<Option<(TrustAnchor, String)>, String> {
        self.bridge.block_on(async {
            let Some(item) = self.get_item(ENTITY_TRUST).await? else {
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

    fn put_trust_if_absent(&self, anchor: &TrustAnchor) -> Result<TrustOutcome, String> {
        let dto = TrustAnchorDto::from(anchor);
        let json = serde_json::to_string(&dto)
            .map_err(|err| format!("encoding the trust anchor failed: {}", err))?;

        self.bridge.block_on(async {
            let mut item = self.key(ENTITY_TRUST);
            item.insert(ATTR_ANCHOR.to_string(), s(json));

            let result = self
                .client
                .put_item()
                .table_name(&self.table)
                .set_item(Some(item))
                // The one-way door: plant the anchor only when none exists.
                .condition_expression("attribute_not_exists(#e)")
                .expression_attribute_names("#e", ATTR_ENTITY)
                .return_values_on_condition_check_failure(
                    ReturnValuesOnConditionCheckFailure::AllOld,
                )
                .send()
                .await;

            match result {
                Ok(_) => Ok(TrustOutcome::Established),
                Err(err) => match err.as_service_error() {
                    Some(PutItemError::ConditionalCheckFailedException(failure)) => {
                        // An anchor already exists. Idempotent for the identical one, refused
                        // for a different one — the fake's exact split, decided by comparing the
                        // incumbent DTO with the incoming one.
                        let existing = failure
                            .item()
                            .and_then(|item| item.get(ATTR_ANCHOR))
                            .and_then(|value| value.as_s().ok());

                        match existing {
                            Some(existing_json) => {
                                let existing_dto: TrustAnchorDto = serde_json::from_str(existing_json)
                                    .map_err(|err| {
                                        format!("decoding the stored trust anchor failed: {}", err)
                                    })?;

                                if existing_dto == dto {
                                    Ok(TrustOutcome::AlreadyIdentical)
                                } else {
                                    Ok(TrustOutcome::Conflict)
                                }
                            }
                            None => Ok(TrustOutcome::Conflict),
                        }
                    }
                    _ => Err(describe("DynamoDB put_item", err)),
                },
            }
        })
    }

    fn replace_trust(
        &self,
        anchor: &TrustAnchor,
        expected_anchor: &str,
        office_head: Option<&str>,
    ) -> Result<TrustWriteOutcome, String> {
        let json = serde_json::to_string(&TrustAnchorDto::from(anchor))
            .map_err(|err| format!("encoding the trust anchor failed: {}", err))?;
        let office_entity = pallet_entity(PalletNamespace::Meta, OFFICE_PALLET_NAME);

        self.bridge.block_on(async {
            // Two actions, and no third: this write conditions on the two inputs `put_trust`'s
            // validation consumed, and on nothing else. Unlike the ref-update commit there is no
            // case that collapses to one item — the trust item and the office item are always
            // distinct — so the office check is never dropped here.
            let mut transaction = ConditionedTransaction::new();

            transaction.push(self.trust_put(&json, expected_anchor), Precondition::Anchor);
            transaction.push(
                self.office_condition_check(&office_entity, office_head),
                Precondition::OfficeHead,
            );

            let refusal = transaction.send(&self.client, "DynamoDB anchor write").await?;

            Ok(match refusal {
                None => TrustWriteOutcome::Replaced,
                Some(Refusal::Transient) => TrustWriteOutcome::Transient,
                Some(Refusal::Moved(Precondition::Anchor, item)) => {
                    TrustWriteOutcome::AnchorMoved { current: item.as_ref().and_then(anchor_of) }
                }
                Some(Refusal::Moved(Precondition::OfficeHead, item)) => {
                    TrustWriteOutcome::OfficeMoved { current: item.as_ref().and_then(head_of) }
                }
                // No action of this transaction conditions on a pallet head, so `classify` cannot
                // name one: the layout it reads is the one pushed above.
                Some(Refusal::Moved(Precondition::PalletHead, _)) => {
                    return Err("DynamoDB anchor write was refused on a pallet-head \
                        precondition it never built."
                        .to_string())
                }
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            Refusal::Transient
        ));
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
