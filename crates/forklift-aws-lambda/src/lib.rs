//! The AWS serverless head: Lambda handlers behind API Gateway — negotiation,
//! presigned URL issuing, signature & privilege verification, and ref CAS commits
//! (metadata only; file bytes flow client ↔ S3 directly). See docs/DESIGN.html §4.
//!
//! The crate is named for its cloud on purpose: a serverless head is an adapter to one
//! provider's runtime, so a future Azure/GCP head would be a sibling crate, not a
//! feature flag here. Its contract already exists: it speaks `docs/format/
//! REMOTE_PROTOCOL.md` (like the self-hostable `forklift-server`), issuing redirects to
//! presigned S3 URLs where the server head serves bytes itself — `307` for an object
//! `GET`/`PUT`, `303` for the `batch` bundle, whose `POST` must switch to a `GET` at the
//! presigned target rather than replay itself — and reusing `forklift_core::util::audit_utils`
//! for the ref-update verification.
//!
//! This head is owned by this repo and open source (decided 2026-07-03): it is the AWS
//! "driver" — anyone can deploy it on their own AWS infrastructure and host forklift,
//! exactly as anyone deploys `forklift-server` on a box. Hosting products build the
//! infrastructure, registry, UI and billing around it, in their own repos. The
//! implementation design lives in docs/DESIGN.html §4.6.
//!
//! # Architecture (the testable spine)
//!
//! The head's protocol logic lives in [`Head`], generic over two narrow traits —
//! [`store::ObjectStore`] (the S3-backed byte plane) and [`store::RefStore`] (the
//! DynamoDB-backed consistency point). The whole protocol suite runs in CI against the
//! real handler logic using the in-memory fakes in [`memory`]; the AWS SDK
//! implementations of the two traits (S3 + DynamoDB, with an endpoint override for
//! LocalStack/MinIO) live in [`aws`] as a separate layer that slots in without touching
//! [`Head`].
//!
//! Verification is *reused, not reimplemented*: the ref-update handler mirrors the small
//! objects `forklift_core`'s audit must read (parcels, trees, office-record blobs,
//! signatures) into a throwaway on-disk `.forklift` ([`scratch`]) and runs the exact
//! same `audit_utils` checks the CLI and the server head run — with the one seam that a
//! serverless head varies, the working-blob existence check, routed to `ObjectStore`
//! (an S3 `HEAD`) via `audit_utils::verify_parcel_closure_with`.

pub mod aws;
pub mod blocking;
pub mod entrypoint;
pub mod error;
pub mod store;
pub mod memory;
pub mod scratch;
pub mod head;

pub use aws::{AwsConfig, DynamoRefStore, S3ObjectStore};
pub use blocking::AsyncBridge;
pub use entrypoint::{auth_from_env, config_from_env, handle, AuthConfig, Routing};
pub use error::{HeadError, HeadResult, Status};
pub use head::{BatchResult, Head};
pub use store::{
    CasOutcome, ObjectAccess, ObjectStore, PromoteOutcome, PutOutcome, PutTarget, RefStore,
    SignatureOutcome, TrustOutcome,
};
