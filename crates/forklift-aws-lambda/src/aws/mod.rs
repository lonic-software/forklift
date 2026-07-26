//! The AWS SDK layer: real implementations of the two store seams.
//!
//! [`Head`](crate::Head) runs over [`ObjectStore`](crate::store::ObjectStore) and
//! [`RefStore`](crate::store::RefStore), and the whole protocol suite proves that logic
//! against the in-memory [`memory`](crate::memory) fakes. This module is the *other*
//! implementation of the same two traits — [`S3ObjectStore`] on S3 for the byte plane,
//! [`DynamoRefStore`] on DynamoDB for the consistency point — slotted in without `Head` ever
//! learning it changed. It is the layer the DESIGN.html §4.6 spine was built to receive.
//!
//! The concerns are split by file:
//!
//! * [`config`] — [`AwsConfig`] and [`build_clients`]/[`build_stores`], which turn it into the
//!   two stores. `config.rs` names zero raw-client tokens — it only ever asks [`s3_ops`]/
//!   [`dynamo_ops`] to build themselves (see those modules' docs on why that split is the
//!   mechanism behind C11 v4, `tests/iam_conformance.rs`).
//! * [`s3_ops`] / [`dynamo_ops`] — [`S3Ops`]/[`DynamoOps`]: the sanctioned operation surface,
//!   each a private SDK client behind one inherent impl of one-line delegations. Nothing
//!   outside these two files, anywhere in the crate, can name an S3/DynamoDB operation these
//!   impls do not expose — that is enforced by rustc's privacy checker, not a scan.
//! * [`s3`] — [`S3ObjectStore`]: the content-addressed key layout, the `If-None-Match`
//!   conditional-write CAS, presigned reads and staged writes, and verify-and-promote.
//! * [`dynamo`] — [`DynamoRefStore`]: the per-warehouse item layout and the real
//!   `ConditionExpression` head CAS and one-way trust door.
//!
//! Every SDK failure that corresponds to a *semantic* outcome the head branches on — a
//! not-found, a CAS conflict, an already-present — is mapped to the matching trait variant by
//! the shared helpers in [`sdk`], so the two backends fail exactly as the fakes do for
//! equivalent inputs.
//!
//! Both stores are synchronous over async SDKs by the settled sync/async seam: they drive each future
//! with the [`AsyncBridge`](crate::AsyncBridge) from a blocking thread. See `blocking.rs`.

pub mod config;
pub mod dynamo;
pub mod dynamo_ops;
pub mod s3;
pub mod s3_ops;

mod sdk;

pub use config::{build_clients, build_stores, AwsConfig};
pub use dynamo::DynamoRefStore;
pub use dynamo_ops::DynamoOps;
pub use s3::{S3ObjectStore, PRESIGN_TTL, STREAMING_THRESHOLD_BYTES};
pub use s3_ops::S3Ops;
