//! [`DynamoOps`] — the sanctioned DynamoDB operation surface, and nothing else.
//!
//! The DynamoDB sibling of [`S3Ops`](crate::aws::s3_ops::S3Ops): see that module's docs for the
//! full mechanism (a private client field, `pub(crate)` one-line delegations to the SDK's own
//! fluent builders, and the three checked properties — parse totality, the item-kind allowlist,
//! and the per-method shape check — `tests/iam_conformance.rs` verifies over this file). Nothing
//! here is DynamoDB-specific except which operations are sanctioned and how the client is built
//! (no path-style addressing quirk — that is an S3/LocalStack peculiarity, not a DynamoDB one).
//!
//! `#[deny(dead_code)]` on the crate is what would catch an op method nothing calls; the crate's
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
use aws_sdk_dynamodb::operation::update_item::builders::UpdateItemFluentBuilder;

use aws_smithy_http_client::tls;

use crate::aws::config::AwsConfig;

/// The sanctioned DynamoDB capability: a private client, and one method per operation this
/// crate is allowed to perform.
#[derive(Clone, Debug)]
pub struct DynamoOps(aws_sdk_dynamodb::Client);

impl DynamoOps {
    /// Build the DynamoDB client from `config`, resolving credentials through the default
    /// provider chain and TLS through the same ring-based rustls connector [`S3Ops::build`]
    /// uses. Exempt from the per-method shape check; every call it makes must be on the
    /// checker's config-builder allowlist (see `S3Ops::build`'s docs — the same list applies).
    ///
    /// [`S3Ops::build`]: crate::aws::s3_ops::S3Ops::build
    pub(crate) async fn build(config: &AwsConfig) -> Result<DynamoOps, String> {
        let http_client = aws_smithy_http_client::Builder::new()
            .tls_provider(tls::Provider::Rustls(tls::rustls_provider::CryptoMode::Ring))
            .build_https();

        let mut loader =
            aws_config::defaults(aws_config::BehaviorVersion::latest()).http_client(http_client);

        if let Some(region) = &config.region {
            loader = loader.region(aws_sdk_dynamodb::config::Region::new(region.clone()));
        }

        if let Some(endpoint) = &config.endpoint_url {
            loader = loader.endpoint_url(endpoint.clone());
        }

        let shared = loader.load().await;
        let client = aws_sdk_dynamodb::Client::new(&shared);

        Ok(DynamoOps(client))
    }

    pub(crate) fn get_item(&self) -> GetItemFluentBuilder {
        self.0.get_item()
    }

    pub(crate) fn put_item(&self) -> PutItemFluentBuilder {
        self.0.put_item()
    }

    pub(crate) fn update_item(&self) -> UpdateItemFluentBuilder {
        self.0.update_item()
    }

    pub(crate) fn query(&self) -> QueryFluentBuilder {
        self.0.query()
    }
}
