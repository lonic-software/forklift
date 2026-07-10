//! Wiring the two SDK-backed stores from a small, explicit configuration.
//!
//! [`AwsConfig`] names the four things a deployment must choose — the S3 bucket, the
//! DynamoDB table, the warehouse id that scopes this warehouse's rows in that table, and an
//! optional endpoint override for LocalStack or MinIO (the §5.0 row calls the override out
//! by name). Credentials are never in it: they come from the **default AWS provider chain**
//! (environment, container role, IMDS, profile), so the configuration carries no secret and
//! nothing here is ever logged.
//!
//! [`build_clients`] and [`build_stores`] are `async` on purpose. Loading the provider chain
//! and building the connector are async, and must run inside the tokio runtime during adapter
//! setup — the same place [`AsyncBridge::current`](crate::AsyncBridge) is captured. The
//! bridge then rides inside each store so its synchronous trait methods can drive the SDK's
//! async calls from the blocking thread the `Head` runs on (see `blocking.rs`).

use aws_smithy_http_client::tls;

use forklift_core::util::pallet_utils::DEFAULT_PALLET_NAME;

use crate::aws::dynamo::DynamoRefStore;
use crate::aws::s3::S3ObjectStore;
use crate::blocking::AsyncBridge;

/// Everything an AWS-backed head needs to reach its byte plane and consistency point.
///
/// The credential source is deliberately absent: the default provider chain resolves it, so
/// a Lambda uses its execution role, a box uses its instance profile, and a LocalStack test
/// uses the throwaway keys in its environment — with no secret passing through this struct.
#[derive(Clone, Debug)]
pub struct AwsConfig {
    /// The S3 bucket holding canonical objects, staged uploads, signature sidecars and
    /// offloaded response bodies (all under distinct prefixes; see `aws::s3`).
    pub bucket: String,

    /// The DynamoDB table holding this deployment's pallet heads and trust anchors, keyed so
    /// one table can serve many warehouses (see `aws::dynamo`).
    pub table: String,

    /// The warehouse this head serves. It is the DynamoDB partition key, so it both scopes
    /// `list_refs` to one warehouse and keeps two warehouses' refs from colliding in a shared
    /// table. It must match the `warehouse_id` a pooled head is built with, for the same
    /// reason the scratch pool is keyed by warehouse (`Scratch::shared`).
    pub warehouse_id: String,

    /// The AWS region. `None` defers to the provider chain (the `AWS_REGION` environment
    /// variable or the active profile).
    pub region: Option<String>,

    /// An endpoint override for S3-and-DynamoDB-compatible stacks — LocalStack (both services
    /// on one endpoint) or MinIO (S3 only). `None` targets real AWS. When set, the S3 client
    /// switches to path-style addressing, which LocalStack and MinIO require.
    pub endpoint_url: Option<String>,

    /// The pallet a franchise checks out when the user does not choose (git's `HEAD`).
    /// Defaults to `main`; a hosted deployment that lets a warehouse pick another sets it here.
    pub default_pallet: String,
}

impl AwsConfig {
    /// A configuration for `warehouse_id`, backed by `bucket` and `table`, against real AWS
    /// with the region and credentials resolved from the environment.
    pub fn new(
        bucket: impl Into<String>,
        table: impl Into<String>,
        warehouse_id: impl Into<String>,
    ) -> AwsConfig {
        AwsConfig {
            bucket: bucket.into(),
            table: table.into(),
            warehouse_id: warehouse_id.into(),
            region: None,
            endpoint_url: None,
            default_pallet: DEFAULT_PALLET_NAME.to_string(),
        }
    }

    /// Pin the region rather than resolving it from the environment.
    pub fn with_region(mut self, region: impl Into<String>) -> AwsConfig {
        self.region = Some(region.into());
        self
    }

    /// Point both services at an override endpoint (LocalStack/MinIO), switching S3 to
    /// path-style addressing.
    pub fn with_endpoint_url(mut self, endpoint_url: impl Into<String>) -> AwsConfig {
        self.endpoint_url = Some(endpoint_url.into());
        self
    }

    /// Serve a different default pallet than `main`.
    pub fn with_default_pallet(mut self, default_pallet: impl Into<String>) -> AwsConfig {
        self.default_pallet = default_pallet.into();
        self
    }
}

/// Build the S3 and DynamoDB clients from `config`, resolving credentials through the default
/// provider chain and TLS through a **ring**-based rustls connector.
///
/// The connector is built explicitly rather than left to the SDK default, because the SDK
/// default (`default-https-client`) selects `rustls-aws-lc`, which drags in `aws-lc-sys` — a
/// C/cmake build the workspace has no need of, since `ring` is already present (reqwest) and
/// is forklift's trusted crypto provider. The choice is invisible on the wire; it is purely a
/// build-time and dependency-surface decision.
///
/// Must be called inside a tokio runtime.
///
/// # Arguments
/// * `config` - The deployment's bucket, table, warehouse, region and endpoint.
///
/// # Returns
/// * `Ok((s3, dynamodb))` - The two configured clients.
/// * `Err(String)`        - If the provider chain or connector could not be built.
pub async fn build_clients(
    config: &AwsConfig,
) -> Result<(aws_sdk_s3::Client, aws_sdk_dynamodb::Client), String> {
    let http_client = aws_smithy_http_client::Builder::new()
        .tls_provider(tls::Provider::Rustls(tls::rustls_provider::CryptoMode::Ring))
        .build_https();

    let mut loader =
        aws_config::defaults(aws_config::BehaviorVersion::latest()).http_client(http_client);

    if let Some(region) = &config.region {
        loader = loader.region(aws_sdk_s3::config::Region::new(region.clone()));
    }

    if let Some(endpoint) = &config.endpoint_url {
        loader = loader.endpoint_url(endpoint.clone());
    }

    let shared = loader.load().await;

    // LocalStack and MinIO serve one endpoint for every bucket, so the `bucket.host` virtual
    // addressing real S3 uses cannot resolve there; path-style (`host/bucket/key`) is the form
    // they understand. Real AWS keeps the default (virtual-host) addressing.
    let s3 = if config.endpoint_url.is_some() {
        let s3_config =
            aws_sdk_s3::config::Builder::from(&shared).force_path_style(true).build();
        aws_sdk_s3::Client::from_conf(s3_config)
    } else {
        aws_sdk_s3::Client::new(&shared)
    };

    let dynamodb = aws_sdk_dynamodb::Client::new(&shared);

    Ok((s3, dynamodb))
}

/// Build both stores over freshly configured clients, moving `bridge` into them so their
/// synchronous trait methods can drive the SDK's async calls. Capture the bridge with
/// [`AsyncBridge::current`](crate::AsyncBridge) on the runtime thread before calling this.
///
/// # Arguments
/// * `config` - The deployment configuration.
/// * `bridge` - The sync/async bridge, captured on the runtime thread.
///
/// # Returns
/// * `Ok((objects, refs))` - The byte plane and the consistency point, ready for a [`Head`].
/// * `Err(String)`         - If the clients could not be built.
///
/// [`Head`]: crate::Head
pub async fn build_stores(
    config: &AwsConfig,
    bridge: AsyncBridge,
) -> Result<(S3ObjectStore, DynamoRefStore), String> {
    let (s3, dynamodb) = build_clients(config).await?;

    let objects = S3ObjectStore::new(s3, config.bucket.clone(), bridge.clone());
    let refs = DynamoRefStore::new(
        dynamodb,
        config.table.clone(),
        config.warehouse_id.clone(),
        config.default_pallet.clone(),
        bridge,
    );

    Ok((objects, refs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_config_defaults_the_pallet_and_leaves_region_and_endpoint_unset() {
        let config = AwsConfig::new("my-bucket", "my-table", "warehouse-1");

        assert_eq!(config.bucket, "my-bucket");
        assert_eq!(config.table, "my-table");
        assert_eq!(config.warehouse_id, "warehouse-1");
        assert_eq!(config.default_pallet, DEFAULT_PALLET_NAME);
        assert!(config.region.is_none());
        assert!(config.endpoint_url.is_none());
    }

    #[test]
    fn the_builders_layer_on_region_endpoint_and_default_pallet() {
        let config = AwsConfig::new("b", "t", "w")
            .with_region("eu-west-1")
            .with_endpoint_url("http://localhost:4566")
            .with_default_pallet("trunk");

        assert_eq!(config.region.as_deref(), Some("eu-west-1"));
        assert_eq!(config.endpoint_url.as_deref(), Some("http://localhost:4566"));
        assert_eq!(config.default_pallet, "trunk");
    }
}
