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
//! Cancellation reasons on a `TransactionCanceledException` are ordered by the order actions
//! were requested (AWS API Reference and the pinned `aws-sdk-dynamodb` model agree on this),
//! so the index an action is pushed at in [`DynamoRefStore::compare_and_set_head`] is the
//! index its refusal comes back at — see that method's own comment for exactly where that
//! ordering is encoded.

use std::collections::HashMap;

use aws_sdk_dynamodb::operation::put_item::PutItemError;
use aws_sdk_dynamodb::operation::transact_write_items::TransactWriteItemsError;
use aws_sdk_dynamodb::types::{
    AttributeValue, CancellationReason, ConditionCheck, ReturnValuesOnConditionCheckFailure,
    TransactWriteItem, Update,
};

use forklift_core::model::remote::TrustAnchorDto;
use forklift_core::util::office_utils::{TrustAnchor, OFFICE_PALLET_NAME};
use forklift_core::util::pallet_utils::{PalletNamespace, PalletRef};

use crate::aws::dynamo_ops::DynamoOps;
use crate::aws::sdk::describe;
use crate::blocking::AsyncBridge;
use crate::store::{CasOutcome, OfficePrecondition, RefStore, TrustOutcome};

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
    fn office_condition_check(&self, office_entity: &str, office_head: &str) -> TransactWriteItem {
        let condition_check = ConditionCheck::builder()
            .set_key(Some(self.key(office_entity)))
            .table_name(&self.table)
            .return_values_on_condition_check_failure(ReturnValuesOnConditionCheckFailure::AllOld)
            .condition_expression("#h = :h")
            .expression_attribute_names("#h", ATTR_HEAD)
            .expression_attribute_values(":h", s(office_head))
            .build()
            .expect("every required ConditionCheck field is set");

        TransactWriteItem::builder().condition_check(condition_check).build()
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

/// Attribute a `TransactWriteItems` cancellation to the precondition it names, using the
/// positional mapping [`DynamoRefStore::compare_and_set_head`] builds: position 0 is always
/// the pallet's own head, position 1 is the office `ConditionCheck` when `builds_office_check`
/// (else the anchor's), and the anchor's is always last. That ordering is not incidental —
/// `TransactionCanceledException::cancellation_reasons` is documented ordered by the order
/// actions were requested (AWS API Reference, corroborated by the pinned `aws-sdk-dynamodb`
/// model) — so the index an action is pushed at in `compare_and_set_head` IS the index its
/// refusal comes back at here, and the two must never drift apart independently.
///
/// A cancellation that names no moved precondition — every position reads "no error" (both the
/// literal string `"None"` and an absent code mean that; the API Reference and the pinned SDK
/// model disagree on which one DynamoDB actually sends, so both must read the same way here) —
/// is reported as a fault rather than guessed at: attributing it to one of the three inputs
/// would be wrong. `TransactionConflict` is the one transient cause classified explicitly
/// rather than falling into that fault, because it is not merely inherited risk but a
/// **contention this slice's own change introduces**: every `ConditionCheck` this method now
/// builds shares the single `@office` item across every concurrent ref update in the
/// warehouse, so an office lift racing *any* other pallet's push can cancel one of them for a
/// reason that establishes nothing about whether that pallet's own commit was ever actually
/// wrong — see [`CasOutcome::Transient`]'s docs for why that is answered as a retryable
/// refusal, not a fault. Every other transient cause (a throttle, the client's own request
/// token racing itself) still falls to the fault arm below: FORK-95 slice 3 (arm two, claim
/// C14) *inherits* the retry loop that answers all of them uniformly and gives it a request
/// token to de-duplicate against; this slice does not build that loop, so SDK-level retry is
/// left exactly as it is today, and a same-token re-issue of a cancelled transaction — the
/// premise that retry would need — is untested here (see this crate's own residuals).
///
/// **Recorded gap, not built here**: `compare_and_set_head`'s `TransactWriteItems` carries no
/// `ClientRequestToken`. `TransactWriteItems` is the one DynamoDB call AWS documents an
/// idempotency token for; without one, an SDK-level retry of a request that actually committed
/// re-issues it as a *new* transaction, the pallet `Update`'s own condition then fails (the
/// head it just moved to no longer equals the `expected` it retried with), and the client is
/// told `409 "The pallet moved"` for a push that succeeded. This is **not a regression** — the
/// single-item `UpdateItem` this replaced had the identical hazard, unconditioned on any
/// token, for the identical reason (a `PutItem`/`UpdateItem` call's SDK-level retry is not
/// itself idempotent-by-token either) — but it is real, and giving it a token per sub-cause is
/// exactly the policy FORK-95 slice 3 owns; recording it here is what keeps this note's "SDK
/// retry left exactly as it is today" from reading as though this call site already has the
/// coverage a token would need to give it.
fn classify_cancellation(
    reasons: &[CancellationReason],
    builds_office_check: bool,
) -> Result<CasOutcome, String> {
    let expected_len = if builds_office_check { 3 } else { 2 };

    if reasons.len() != expected_len {
        return Err(format!(
            "DynamoDB transact_write_items was cancelled with {} cancellation reason(s), \
            expected {}: the cancellation cannot be attributed to a specific precondition.",
            reasons.len(),
            expected_len
        ));
    }

    let is_condition_failed =
        |reason: &CancellationReason| reason.code() == Some("ConditionalCheckFailed");

    if is_condition_failed(&reasons[0]) {
        return Ok(CasOutcome::Conflict { current: reasons[0].item().and_then(head_of) });
    }

    if builds_office_check && is_condition_failed(&reasons[1]) {
        return Ok(CasOutcome::OfficeMoved { current: reasons[1].item().and_then(head_of) });
    }

    let anchor_position = if builds_office_check { 2 } else { 1 };

    if is_condition_failed(&reasons[anchor_position]) {
        return Ok(CasOutcome::AnchorMoved);
    }

    // No position names a moved precondition. `ConditionalCheckFailed` takes precedence over
    // any transient code by construction above (checked first, at every position) — a
    // transaction that was going to be refused for a moved input would be refused again on
    // re-issue, so reporting the transient code first would spend a retry budget on a `409`
    // wearing a `503`'s clothes. Only once no position failed a condition does a
    // `TransactionConflict` anywhere in the list read as this slice's own introduced
    // contention rather than a moved value.
    if reasons.iter().any(|reason| reason.code() == Some("TransactionConflict")) {
        return Ok(CasOutcome::Transient);
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
        let builds_office_check = office_head_value.is_some();

        self.bridge.block_on(async {
            // Position 0 is always the pallet's own head; the office `ConditionCheck` (when
            // built) and the anchor `ConditionCheck` follow in exactly this push order. See
            // `classify_cancellation`'s docs for why that ordering is load-bearing.
            let mut items = vec![self.pallet_update(&entity, expected, new)];

            if let Some(office_head) = office_head_value {
                items.push(self.office_condition_check(&office_entity, office_head));
            }

            items.push(self.anchor_condition_check(anchor));

            let request = self.client.transact_write_items().set_transact_items(Some(items));

            match request.send().await {
                Ok(_) => Ok(CasOutcome::Committed),
                Err(err) => match err.as_service_error() {
                    Some(TransactWriteItemsError::TransactionCanceledException(failure)) => {
                        classify_cancellation(failure.cancellation_reasons(), builds_office_check)
                    }
                    _ => Err(describe("DynamoDB transact_write_items", err)),
                },
            }
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

    fn replace_trust(&self, anchor: &TrustAnchor) -> Result<(), String> {
        let json = serde_json::to_string(&TrustAnchorDto::from(anchor))
            .map_err(|err| format!("encoding the trust anchor failed: {}", err))?;

        self.bridge.block_on(async {
            let mut item = self.key(ENTITY_TRUST);
            item.insert(ATTR_ANCHOR.to_string(), s(json));

            // Unconditional: the head has already validated the chain of custody (§8.7); this
            // is the one sanctioned overwrite of the anchor.
            self.client
                .put_item()
                .table_name(&self.table)
                .set_item(Some(item))
                .send()
                .await
                .map(|_| ())
                .map_err(|err| describe("DynamoDB put_item", err))
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

    /// PR #116 review, finding 1: `TransactionConflict` — DynamoDB's own concurrency control
    /// when another transaction concurrently touches one of this commit's items (the shared
    /// `@office` item, principally) — must classify as `Transient`, not fall to the generic
    /// fault this function's `Err` branch answers as a `500`. No position failed a condition,
    /// so nothing here is attributable to a moved value.
    #[test]
    fn a_transaction_conflict_with_no_moved_precondition_is_transient() {
        let reasons = vec![reason("None"), reason("TransactionConflict"), reason("None")];

        assert_eq!(
            classify_cancellation(&reasons, true).expect("a transient cause, not a fault"),
            CasOutcome::Transient
        );
    }

    /// `ConditionalCheckFailed` takes precedence over a `TransactionConflict` elsewhere in the
    /// list: a transaction that was going to be refused for a moved input would be refused
    /// again on re-issue, so reporting the transient code instead would answer `503` for what
    /// is really a `409`.
    #[test]
    fn a_moved_precondition_is_attributed_even_alongside_a_transaction_conflict() {
        let reasons =
            vec![reason("ConditionalCheckFailed"), reason("TransactionConflict"), reason("None")];

        assert_eq!(
            classify_cancellation(&reasons, true).expect("attributed to the pallet"),
            CasOutcome::Conflict { current: None }
        );
    }
}
