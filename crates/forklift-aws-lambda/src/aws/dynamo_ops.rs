//! [`DynamoOps`] — the sanctioned DynamoDB operation surface, and nothing else.
//!
//! The DynamoDB sibling of [`S3Ops`](crate::aws::s3_ops::S3Ops): see that module's docs for the
//! full mechanism (a private client field, `pub(crate)` one-line delegations to the SDK's own
//! fluent builders, and the three checked properties — parse totality, the item-kind allowlist,
//! and the per-method shape check — `tests/iam_conformance.rs` verifies over this file). Nothing
//! here is DynamoDB-specific except which operations are sanctioned and how the client is built
//! (no path-style addressing quirk — that is an S3/LocalStack peculiarity, not a DynamoDB one).
//!
//! `#![deny(dead_code)]`, scoped to **this file** (below, not on the crate — `lib.rs` carries
//! only `#![forbid(unsafe_code)]`), is what would catch an op method nothing calls; the crate's
//! `tests/iam_conformance.rs` derives the operation set straight from this impl's non-constructor
//! method names, so a new method here is either mapped and granted or the conformance test goes
//! red — see that file's module docs for directions A/B/C.

// Direction C (a dead grant nothing calls) is rustc's job, not a scanner's: an op method below
// that nothing in the crate calls is a **build error** here, not a passing test. Scoped to this
// module (rather than the whole crate) so it applies exactly where `pub(crate)` op methods live.
#![deny(dead_code)]

use aws_sdk_dynamodb::operation::get_item::builders::GetItemFluentBuilder;
use aws_sdk_dynamodb::operation::put_item::builders::PutItemFluentBuilder;
use aws_sdk_dynamodb::operation::query::builders::QueryFluentBuilder;
use aws_sdk_dynamodb::operation::transact_write_items::builders::TransactWriteItemsFluentBuilder;

/// The sanctioned DynamoDB capability: a private client, and one method per operation this
/// crate is allowed to perform. Both halves of the privacy — the field carrying no `pub`, and
/// every op method being exactly `pub(crate)` — are asserted by `tests/iam_conformance.rs`, not
/// merely documented; see [`S3Ops`](crate::aws::s3_ops::S3Ops)'s docs for why.
#[derive(Clone, Debug)]
pub struct DynamoOps(aws_sdk_dynamodb::Client);

impl DynamoOps {
    /// Build the DynamoDB client over an already-resolved `shared` config — see
    /// `aws/config.rs`'s `load_shared_config`, shared with [`S3Ops::build`] so a cold start
    /// pays for one credential-chain resolution, not two. Exempt from the per-method shape
    /// check; every call it makes must be on the checker's config-builder allowlist (see
    /// `S3Ops::build`'s docs — the same list applies).
    ///
    /// [`S3Ops::build`]: crate::aws::s3_ops::S3Ops::build
    pub(crate) fn build(shared: &aws_config::SdkConfig) -> DynamoOps {
        DynamoOps(aws_sdk_dynamodb::Client::new(shared))
    }

    pub(crate) fn get_item(&self) -> GetItemFluentBuilder {
        self.0.get_item()
    }

    pub(crate) fn put_item(&self) -> PutItemFluentBuilder {
        self.0.put_item()
    }

    pub(crate) fn query(&self) -> QueryFluentBuilder {
        self.0.query()
    }

    /// The commit for [`RefStore::compare_and_set_head`](crate::store::RefStore::compare_and_set_head)
    /// (FORK-95 design memo, claim C13): one transaction conditioned on every movable input a
    /// ref-update audit consumed, replacing what was a bare `update_item()` call before this
    /// wrapper method existed — `update_item` is gone from this file because nothing calls it
    /// any more (`#![deny(dead_code)]`, above, would refuse to compile it otherwise).
    pub(crate) fn transact_write_items(&self) -> TransactWriteItemsFluentBuilder {
        self.0.transact_write_items()
    }
}
