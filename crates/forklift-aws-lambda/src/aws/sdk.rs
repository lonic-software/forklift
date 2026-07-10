//! Shared SDK-error plumbing for the S3 and DynamoDB stores.
//!
//! The two traits return `Result<_, String>`, and the [`Head`](crate::Head) turns every
//! `Err` into a `500`. The *semantic* outcomes a store must distinguish — a CAS conflict, a
//! not-found, an already-present, a corrupt promote — are trait enum variants, **not**
//! errors: `head.rs` branches on them. So the whole job of error mapping here is to notice
//! the specific SDK failure that corresponds to one of those semantic outcomes (a `404`, a
//! `412` precondition failure, a DynamoDB `ConditionalCheckFailedException`) and hand back
//! the matching variant, while every *other* SDK failure becomes an opaque `Err(String)`
//! that reads as `500` — exactly as the in-memory fakes behave for equivalent inputs.
//!
//! [`http_status`]/[`is_precondition_failed`] read the transport status; [`is_no_such_key`]
//! and [`is_head_object_not_found`] are more careful than a bare `404` (see their docs — a
//! bucket that does not exist must error loudly, not read as an empty warehouse); [`describe`]
//! renders a genuine failure into a message. Neither ever surfaces credentials — the SDK
//! error chain carries an AWS error code and request id, never the signing key.

use aws_sdk_s3::operation::head_object::HeadObjectError;
use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
use aws_smithy_runtime_api::client::result::SdkError;
use aws_smithy_types::error::display::DisplayErrorContext;
use aws_smithy_types::error::metadata::ProvideErrorMetadata;

/// The HTTP status of a failed SDK call, when the failure reached the service and came back
/// with a response (a modeled or unmodeled service error). `None` for a failure that never
/// got a response — a dispatch, timeout, or construction error — which is always a genuine
/// `500`, never a semantic outcome.
pub(crate) fn http_status<E>(err: &SdkError<E, HttpResponse>) -> Option<u16> {
    err.raw_response().map(|response| response.status().as_u16())
}

/// Whether a `GetObject` failure is specifically **`NoSuchKey`** — the object is absent, fold
/// it into `Ok(None)`, mirroring the fake's `HashMap` miss.
///
/// Deliberately *not* a bare `404` status check: S3's `GetObject` error body carries a real
/// error code, and `NoSuchBucket` is also a `404`. Collapsing both into "absent" would make a
/// warehouse pointed at a deleted or misconfigured bucket silently read as an *empty* one —
/// every object "missing", every lift looking like a fresh start — instead of failing loudly
/// the moment anything is actually read. `GetObject` is the operation used wherever this store
/// must read bytes (`get`, `get_signature`, the staged-object read inside
/// `verify_and_promote`), so this check is what makes a bucket misconfiguration surface there.
pub(crate) fn is_no_such_key(err: &SdkError<aws_sdk_s3::operation::get_object::GetObjectError, HttpResponse>) -> bool {
    http_status(err) == Some(404) && err.code() == Some("NoSuchKey")
}

/// Whether a `HeadObject` failure is S3's modeled "not found" response.
///
/// This is the one place the `NoSuchBucket`-vs-missing-key distinction [`is_no_such_key`]
/// draws **cannot** be drawn: a `HEAD` response carries no body by protocol, so S3 sends no
/// error code to tell "no such key" apart from "no such bucket" — both are a bare `404`, and
/// the SDK models both as the same `HeadObjectError::NotFound`. That is an AWS API limitation,
/// not a gap in this mapping. Matching the *modeled* variant (rather than a raw status code)
/// still buys something: any `404`-shaped response S3 did not model as this specific case
/// falls through to `Unhandled` and becomes a loud `Err`, not a silent "absent".
///
/// The residual risk this leaves is narrow: a store built against a bucket that does not
/// exist can still have `exists`/`access` (both `HEAD`-based) misreport "absent" rather than
/// erroring. It cannot corrupt or leak anything — nothing unverified ever reaches a canonical
/// key regardless — and it is bounded to read-only traffic before any write: the very first
/// `put_verified`/`verify_and_promote` against that bucket goes through `PutObject`/
/// `GetObject`, both of which fail loudly.
pub(crate) fn is_head_object_not_found(err: &SdkError<HeadObjectError, HttpResponse>) -> bool {
    matches!(err.as_service_error(), Some(HeadObjectError::NotFound(_)))
}

/// Whether an S3 error is a `412 Precondition Failed` — the response to an `If-None-Match`
/// write (a canonical key that already exists, or a `CopyObject` whose pinned source changed).
/// No S3 operation models a distinct error type for this; it is a bare status code by design.
/// This is the conditional-write CAS the byte plane leans on: it is how `put_verified` reports
/// `AlreadyPresent` and how `verify_and_promote` serializes racing promoters.
pub(crate) fn is_precondition_failed<E>(err: &SdkError<E, HttpResponse>) -> bool {
    http_status(err) == Some(412)
}

/// Render a genuine SDK failure into the opaque `Err(String)` the head reports as `500`.
/// [`DisplayErrorContext`] walks the whole source chain (AWS error code, message, request
/// id) — diagnostic, and free of secrets, which the SDK never puts in an error.
pub(crate) fn describe<E, R>(operation: &str, err: SdkError<E, R>) -> String
where
    SdkError<E, R>: std::error::Error,
{
    format!("{} failed: {}", operation, DisplayErrorContext(&err))
}
