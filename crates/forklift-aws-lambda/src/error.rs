//! The head's status/error vocabulary.
//!
//! [`Head`](crate::Head) returns provider-agnostic outcomes: a [`Status`] mirrors the
//! HTTP status `REMOTE_PROTOCOL.md` prescribes, and the runtime adapter (`lambda_http`
//! for AWS, or a plain server for tests) translates it to a real HTTP response. Keeping
//! the status out of `axum`/`http` types is what lets the same handler logic run under
//! Lambda, under a test harness, and under LocalStack unchanged.

/// A protocol status. The numeric values are the HTTP status codes of
/// `docs/format/REMOTE_PROTOCOL.md`; the adapter maps them onto its transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// `200` — success with no creation (idempotent no-op, or a read).
    Ok,
    /// `201` — a new object, signature, warehouse or trust anchor was created.
    Created,
    /// `307` — follow the `Location` to a presigned storage URL for the bytes.
    TemporaryRedirect,
    /// `401` — no valid bearer token was presented (the transport-authentication seam).
    Unauthorized,
    /// `403` — `audit_utils::verify_office_chain` found, in the segment an office lift brings,
    /// an authentic parcel that its signer had no right to make (`OfficeChainError::Unauthorized`,
    /// mapped by [`HeadError::office_lift`]): a key-permanence violation (`verify_key_permanence`
    /// — a key removed or altered, or a revocation undone or added without a reason; this one
    /// binds admins too), a *signer* who lacked the office role (or self-service right) it
    /// needed as of that parcel's own signing, or a key endorsed by someone whose authority
    /// does not cover it. An inauthentic chain, or an overstep already in the held office, is a
    /// `422` instead. Not a
    /// caller role or grant — this crate has no caller-identity concept to check one against,
    /// so this is never about who transported the request.
    Forbidden,
    /// `404` — no such warehouse, object, signature or bundle.
    NotFound,
    /// `409` — a CAS conflict: the ref moved, or trust is a one-way door.
    Conflict,
    /// `422` — a verification failure: a bad hash, a missing closure, a failed audit.
    Unprocessable,
    /// `503` — a transient refusal that does not claim any input moved (DynamoDB transaction
    /// contention, principally): retrying may succeed with nothing having changed. Distinct
    /// from `409`, which asserts a movable input's value differs from what was consumed.
    ServiceUnavailable,
    /// `500` — an internal storage error (never a client's fault).
    Internal,
}

impl Status {
    /// The HTTP status code.
    pub fn as_u16(self) -> u16 {
        match self {
            Status::Ok => 200,
            Status::Created => 201,
            Status::TemporaryRedirect => 307,
            Status::Unauthorized => 401,
            Status::Forbidden => 403,
            Status::NotFound => 404,
            Status::Conflict => 409,
            Status::Unprocessable => 422,
            Status::ServiceUnavailable => 503,
            Status::Internal => 500,
        }
    }
}

/// A failed request: the status to answer with and the message for the JSON error body
/// (`{"error": …}` per the protocol).
#[derive(Debug, Clone)]
pub struct HeadError {
    pub status: Status,
    pub message: String,
    /// Set only by [`HeadError::commit_not_ready`]: the adapter answers it with `Retry-After` and
    /// `Forklift-Commit: not-ready`, the headers a client classifies a lift commit by.
    pub commit_not_ready: bool,
}

/// The result of a handler.
pub type HeadResult<T> = Result<T, HeadError>;

impl HeadError {
    fn with(status: Status, message: impl Into<String>) -> HeadError {
        HeadError { status, message: message.into(), commit_not_ready: false }
    }

    /// A `404 Not Found`.
    pub fn not_found(message: impl Into<String>) -> HeadError {
        HeadError::with(Status::NotFound, message)
    }

    /// A `409 Conflict` — a CAS race or a one-way-door violation.
    pub fn conflict(message: impl Into<String>) -> HeadError {
        HeadError::with(Status::Conflict, message)
    }

    /// A `401 Unauthorized` — no valid bearer token was presented. The message is the same
    /// whether the header was missing, malformed, or simply wrong: the seam that emits this
    /// must never let a caller distinguish "no token configured" from "wrong token".
    pub fn unauthorized(message: impl Into<String>) -> HeadError {
        HeadError::with(Status::Unauthorized, message)
    }

    /// A `422 Unprocessable Entity` — a verification failure.
    pub fn unprocessable(message: impl Into<String>) -> HeadError {
        HeadError::with(Status::Unprocessable, message)
    }

    /// A `403 Forbidden` — an authorization failure.
    pub fn forbidden(message: impl Into<String>) -> HeadError {
        HeadError::with(Status::Forbidden, message)
    }

    /// A lifted office chain that did not verify: a `403` when a parcel the lift brings
    /// oversteps its signer's privileges, a `422` otherwise (see
    /// `audit_utils::classify_office_lift_refusal`).
    pub fn office_lift(error: forklift_core::util::audit_utils::OfficeChainError,
                       held_head: Option<&str>) -> HeadError {
        use forklift_core::util::audit_utils::{self, OfficeLiftRefusal};

        match audit_utils::classify_office_lift_refusal(error, held_head) {
            OfficeLiftRefusal::Overstep(message) => HeadError::forbidden(message),
            OfficeLiftRefusal::Unprocessable(message) => HeadError::unprocessable(message),
        }
    }

    /// A `503 Service Unavailable` — a transient refusal (contention, not a moved value).
    pub fn service_unavailable(message: impl Into<String>) -> HeadError {
        HeadError::with(Status::ServiceUnavailable, message)
    }

    /// The `503` a lift commit answers while a blob it names is still staged and not yet promoted:
    /// the one answer a client retries on the remote's word. The signal is the header the adapter
    /// adds, never the message, so a body that is cut short or rewritten cannot hide it.
    pub fn commit_not_ready(message: impl Into<String>) -> HeadError {
        HeadError { commit_not_ready: true, ..HeadError::with(Status::ServiceUnavailable, message) }
    }

    /// A `500 Internal Server Error` — a storage-layer failure. Storage errors bubble up
    /// as strings from the traits; this is where they become a status.
    pub fn internal(message: impl Into<String>) -> HeadError {
        HeadError::with(Status::Internal, message)
    }
}

impl std::fmt::Display for HeadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.status.as_u16())
    }
}

impl std::error::Error for HeadError {}
