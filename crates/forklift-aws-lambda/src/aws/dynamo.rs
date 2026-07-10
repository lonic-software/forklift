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
//! write, never a read-then-write: an `UpdateItem` whose `ConditionExpression` encodes the
//! caller's `expected`. When the condition fails, DynamoDB returns the current item (via
//! `ReturnValuesOnConditionCheckFailure=ALL_OLD`), so the store reports the actual head in
//! the `Conflict` without a second round trip — and the whole check-and-set is atomic, which
//! is the CAS that lets the serverless head scale horizontally where the server head needs a
//! mutex. [`put_trust_if_absent`](RefStore::put_trust_if_absent) is the same shape: a
//! conditional `PutItem` guarding the one-way trust door.

use std::collections::HashMap;

use aws_sdk_dynamodb::operation::put_item::PutItemError;
use aws_sdk_dynamodb::operation::update_item::UpdateItemError;
use aws_sdk_dynamodb::types::{AttributeValue, ReturnValuesOnConditionCheckFailure};

use forklift_core::model::remote::TrustAnchorDto;
use forklift_core::util::office_utils::TrustAnchor;
use forklift_core::util::pallet_utils::{PalletNamespace, PalletRef};

use crate::aws::sdk::describe;
use crate::blocking::AsyncBridge;
use crate::store::{CasOutcome, RefStore, TrustOutcome};

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
    client: aws_sdk_dynamodb::Client,
    table: String,
    warehouse: String,
    default_pallet: String,
    bridge: AsyncBridge,
}

impl DynamoRefStore {
    /// Build the store over a DynamoDB `client` addressing `table`, scoped to `warehouse`
    /// (the partition key) and serving `default_pallet`, driving async calls through `bridge`.
    pub fn new(
        client: aws_sdk_dynamodb::Client,
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
    /// It does **not** close the other half. `ref_update`'s audit-then-CAS shape reads the
    /// office head, audits against it, and only *afterwards* CASes the target pallet's own
    /// head — the CAS is conditioned on the target pallet's `old_head`, not on the office head
    /// staying put across those two round trips. A concurrent office re-key between the read
    /// and the CAS is still possible, consistent read or not; closing that would mean
    /// conditioning the pallet CAS on the office head too (a DynamoDB transaction across both
    /// items), which changes what the CAS commits and is a design question for `Head`
    /// (`head.rs`), not something this store can decide unilaterally. This is pre-existing —
    /// [`crate::memory::MemoryRefStore`] has the identical property, since nothing in the
    /// trait ties the two reads to the CAS either.
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
    ) -> Result<CasOutcome, String> {
        let entity = pallet_entity(namespace, name);

        self.bridge.block_on(async {
            // Always `SET head = :new`; the condition encodes `expected`. `#h`/`#e` alias the
            // attribute names so a reserved word could never break the expression.
            let mut request = self
                .client
                .update_item()
                .table_name(&self.table)
                .set_key(Some(self.key(&entity)))
                .update_expression("SET #h = :new")
                .expression_attribute_names("#h", ATTR_HEAD)
                .expression_attribute_values(":new", s(new))
                .return_values_on_condition_check_failure(
                    ReturnValuesOnConditionCheckFailure::AllOld,
                );

            request = match expected {
                // The pallet must currently hold exactly `old`. A missing item fails this too
                // (there is no `head` to equal `old`), reported as a conflict with no current
                // head — the fake's `current (None) != expected (Some)` branch.
                Some(old) => request
                    .condition_expression("#h = :old")
                    .expression_attribute_values(":old", s(old)),
                // The pallet must not exist yet.
                None => request
                    .condition_expression("attribute_not_exists(#e)")
                    .expression_attribute_names("#e", ATTR_ENTITY),
            };

            match request.send().await {
                Ok(_) => Ok(CasOutcome::Committed),
                Err(err) => match err.as_service_error() {
                    Some(UpdateItemError::ConditionalCheckFailedException(failure)) => {
                        // ALL_OLD carries the item that failed the condition; its `head` is the
                        // actual current head (absent when the item did not exist).
                        let current = failure.item().and_then(head_of);

                        Ok(CasOutcome::Conflict { current })
                    }
                    _ => Err(describe("DynamoDB update_item", err)),
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

    fn get_trust(&self) -> Result<Option<TrustAnchor>, String> {
        self.bridge.block_on(async {
            let Some(item) = self.get_item(ENTITY_TRUST).await? else {
                return Ok(None);
            };

            let Some(json) = item.get(ATTR_ANCHOR).and_then(|value| value.as_s().ok()) else {
                return Ok(None);
            };

            let dto: TrustAnchorDto = serde_json::from_str(json)
                .map_err(|err| format!("decoding the stored trust anchor failed: {}", err))?;

            Ok(Some(dto.to_anchor()))
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
}
