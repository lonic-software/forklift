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
    /// `403` — authenticated but not authorized (a role or grant refuses it).
    Forbidden,
    /// `404` — no such warehouse, object, signature or bundle.
    NotFound,
    /// `409` — a CAS conflict: the ref moved, or trust is a one-way door.
    Conflict,
    /// `422` — a verification failure: a bad hash, a missing closure, a failed audit.
    Unprocessable,
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
}

/// The result of a handler.
pub type HeadResult<T> = Result<T, HeadError>;

impl HeadError {
    /// A `404 Not Found`.
    pub fn not_found(message: impl Into<String>) -> HeadError {
        HeadError { status: Status::NotFound, message: message.into() }
    }

    /// A `409 Conflict` — a CAS race or a one-way-door violation.
    pub fn conflict(message: impl Into<String>) -> HeadError {
        HeadError { status: Status::Conflict, message: message.into() }
    }

    /// A `401 Unauthorized` — no valid bearer token was presented. The message is the same
    /// whether the header was missing, malformed, or simply wrong: the seam that emits this
    /// must never let a caller distinguish "no token configured" from "wrong token".
    pub fn unauthorized(message: impl Into<String>) -> HeadError {
        HeadError { status: Status::Unauthorized, message: message.into() }
    }

    /// A `422 Unprocessable Entity` — a verification failure.
    pub fn unprocessable(message: impl Into<String>) -> HeadError {
        HeadError { status: Status::Unprocessable, message: message.into() }
    }

    /// A `403 Forbidden` — an authorization failure.
    pub fn forbidden(message: impl Into<String>) -> HeadError {
        HeadError { status: Status::Forbidden, message: message.into() }
    }

    /// A `500 Internal Server Error` — a storage-layer failure. Storage errors bubble up
    /// as strings from the traits; this is where they become a status.
    pub fn internal(message: impl Into<String>) -> HeadError {
        HeadError { status: Status::Internal, message: message.into() }
    }
}

impl std::fmt::Display for HeadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.status.as_u16())
    }
}

impl std::error::Error for HeadError {}
