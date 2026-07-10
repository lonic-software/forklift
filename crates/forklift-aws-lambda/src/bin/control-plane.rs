//! The control-plane Lambda: API Gateway → [`forklift_aws_lambda::handle`] → S3 + DynamoDB.
//!
//! A thin runtime adapter and nothing more. It builds the SDK clients once per cold start
//! (into a process-global [`OnceCell`], so warm invocations reuse them), converts the
//! `lambda_http` request into the `http` request the pure router speaks, and — because every
//! `Head` method blocks on its store's futures — runs [`handle`] on a **blocking thread**
//! (`spawn_blocking`), never on a runtime worker where tokio refuses to let a thread block.
//! The router itself is provider-agnostic and tested without any of this; see
//! `entrypoint.rs`.
//!
//! Build with `cargo build -p forklift-aws-lambda --features lambda --release`.

use forklift_aws_lambda::aws::build_clients;
use forklift_aws_lambda::{
    config_from_env, handle, AsyncBridge, AwsConfig, DynamoRefStore, Head, Routing, S3ObjectStore,
};
use lambda_http::{run, service_fn, Body, Error, Request, Response};
use tokio::sync::OnceCell;

/// Built once per cold start, reused by every warm invocation. The SDK clients are cheap to
/// clone (they are `Arc`-backed), so a per-request store — the ref store's warehouse partition
/// varies in multi mode — is nearly free, while the expensive audit mirror is amortized by the
/// process-global scratch pool [`Head::pooled`] keys by warehouse.
struct Context {
    s3: aws_sdk_s3::Client,
    dynamodb: aws_sdk_dynamodb::Client,
    bridge: AsyncBridge,
    config: AwsConfig,
    routing: Routing,
}

impl Context {
    /// Assemble the [`Head`] for one request's warehouse — the fixed id in single mode, the
    /// path's `{id}` in multi mode. Cheap: only the clients (cloned) and the resolved
    /// warehouse id, which the ref store and the scratch pool must share (stage-1 review).
    fn build_head(&self, warehouse_id: &str) -> Result<Head<S3ObjectStore, DynamoRefStore>, String> {
        let objects =
            S3ObjectStore::new(self.s3.clone(), self.config.bucket.clone(), self.bridge.clone());
        let refs = DynamoRefStore::new(
            self.dynamodb.clone(),
            self.config.table.clone(),
            warehouse_id.to_string(),
            self.config.default_pallet.clone(),
            self.bridge.clone(),
        );

        Ok(Head::pooled(objects, refs, warehouse_id.to_string()))
    }
}

/// The process-global context, built lazily inside the runtime on the first invocation.
static CONTEXT: OnceCell<Context> = OnceCell::const_new();

/// Resolve the context, building the clients and capturing the async bridge on first use.
/// Building runs inside the tokio runtime (the SDK builders are async and the bridge must be
/// captured on a runtime thread); the flavour check inside [`AsyncBridge::current`] refuses a
/// single-threaded runtime, which is why `main` is multi-thread.
async fn context() -> Result<&'static Context, Error> {
    CONTEXT
        .get_or_try_init(|| async {
            let (config, routing) = config_from_env().map_err(Error::from)?;
            let (s3, dynamodb) = build_clients(&config).await.map_err(Error::from)?;
            let bridge = AsyncBridge::current().map_err(Error::from)?;

            Ok(Context { s3, dynamodb, bridge, config, routing })
        })
        .await
}

/// One request: convert, route on a blocking thread, convert back.
async fn handler(event: Request) -> Result<Response<Body>, Error> {
    let ctx = context().await?;

    // The runtime has already buffered the whole body, so the router can be fully synchronous.
    let (parts, body) = event.into_parts();
    let request = http::Request::from_parts(parts, body_to_vec(body));

    let response = tokio::task::spawn_blocking(move || {
        handle(&ctx.routing, |warehouse_id| ctx.build_head(warehouse_id), request)
    })
    .await
    .map_err(|e| Error::from(format!("The request handler panicked: {}", e)))?;

    let (parts, body) = response.into_parts();
    Ok(Response::from_parts(parts, Body::from(body)))
}

/// Flatten a `lambda_http` body into the bytes the router reads.
fn body_to_vec(body: Body) -> Vec<u8> {
    match body {
        Body::Empty => Vec::new(),
        Body::Text(text) => text.into_bytes(),
        Body::Binary(data) => data,
    }
}

/// Multi-thread on purpose: [`AsyncBridge`] refuses a single-threaded runtime, where a bridged
/// SDK call driven from a blocking thread would hang instead of failing (see `blocking.rs`).
#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Error> {
    run(service_fn(handler)).await
}
