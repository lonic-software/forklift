//! The AWS SDK layer: real implementations of the two store seams.
//!
//! [`Head`](crate::Head) runs over [`ObjectStore`](crate::store::ObjectStore) and
//! [`RefStore`](crate::store::RefStore), and the whole protocol suite proves that logic
//! against the in-memory [`memory`](crate::memory) fakes. This module is the *other*
//! implementation of the same two traits — [`S3ObjectStore`] on S3 for the byte plane,
//! [`DynamoRefStore`] on DynamoDB for the consistency point — slotted in without `Head` ever
//! learning it changed. It is the layer the DESIGN.html §4.6 spine was built to receive.
//!
//! The three concerns are split by file:
//!
//! * [`config`] — [`AwsConfig`] and the async builders that turn it into the two stores,
//!   resolving credentials from the default provider chain and TLS through ring-based rustls.
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
pub mod s3;

mod sdk;

pub use config::{build_clients, build_stores, AwsConfig};
pub use dynamo::DynamoRefStore;
pub use s3::{S3ObjectStore, PRESIGN_TTL, STREAMING_THRESHOLD_BYTES};
