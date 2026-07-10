//! The staging verifier Lambda: an S3 object-created event → `verify_and_promote`.
//!
//! The large-blob half of the split-verify decision (`store.rs`). A client `PUT`s a large
//! object straight to `staging/{session}/{hash}` via a presigned URL, which the control plane
//! never sees; S3 fires an object-created event, and this function runs the *same*
//! [`ObjectStore::verify_and_promote`] the control plane runs synchronously for small objects —
//! it stream-hashes the staged bytes and, only if `Blake3(bytes) == hash`, server-side-copies
//! them to the canonical key. Promotion is the single moment the object becomes fetchable
//! (invariant 1), and it is idempotent, so this racing the control plane's `commit_lift` on one
//! hash is safe.
//!
//! Same blocking discipline as the control plane: `verify_and_promote` bridges S3 futures,
//! so it runs on a blocking thread. It needs only the object store — no ref store, no warehouse
//! id — because promotion is content-addressed, not warehouse-scoped.
//!
//! Build with `cargo build -p forklift-aws-lambda --features lambda --release`.

use aws_lambda_events::event::s3::S3Event;
use forklift_aws_lambda::aws::build_clients;
use forklift_aws_lambda::store::{ObjectStore, PromoteOutcome};
use forklift_aws_lambda::{config_from_env, AsyncBridge, S3ObjectStore};
use lambda_runtime::{run, service_fn, Error, LambdaEvent};
use tokio::sync::OnceCell;

/// The object store, built once per cold start (the bucket is warehouse-agnostic).
static OBJECTS: OnceCell<S3ObjectStore> = OnceCell::const_new();

/// Resolve the object store, building the S3 client and capturing the bridge on first use.
async fn objects() -> Result<&'static S3ObjectStore, Error> {
    OBJECTS
        .get_or_try_init(|| async {
            let (config, _routing) = config_from_env().map_err(Error::from)?;
            let (s3, _dynamodb) = build_clients(&config).await.map_err(Error::from)?;
            let bridge = AsyncBridge::current().map_err(Error::from)?;

            Ok(S3ObjectStore::new(s3, config.bucket, bridge))
        })
        .await
}

/// Verify-and-promote every staged object an S3 event names.
async fn handler(event: LambdaEvent<S3Event>) -> Result<(), Error> {
    let objects = objects().await?;

    for record in event.payload.records {
        let Some(raw_key) = record.s3.object.key else {
            continue;
        };

        // S3 event notifications URL-encode the key the way a query string is (space → `+`,
        // everything else percent-escaped): "red flower.jpg" arrives as "red+flower.jpg". A
        // session id or hash cannot contain either character, but the key is split on `/`
        // first — an unrelated, encoded key (or a stray notification) must decode cleanly
        // before `parse_staging_key` judges its shape, or a corrupted split could misroute it.
        // Newer event payloads carry the already-decoded form in `url_decoded_key`; prefer it
        // when present, and fall back to a small inline decoder for payloads that predate it,
        // rather than pulling in a URL-decoding dependency for one string.
        let key = record.s3.object.url_decoded_key.unwrap_or_else(|| decode_s3_key(&raw_key));

        let Some((session, hash)) = parse_staging_key(&key) else {
            // The trigger should be scoped to the `staging/` prefix, but a stray event is
            // skipped rather than failed — a failure would have S3/EventBridge retry forever.
            eprintln!("verifier: ignoring an S3 event that is not a staging object: {}", key);
            continue;
        };

        let (session, hash) = (session.to_string(), hash.to_string());

        // A storage error propagates (the event retries); a semantic outcome is logged and the
        // event is consumed — a corrupt object was discarded, not promoted, and retrying it
        // would only rediscard it.
        let outcome =
            tokio::task::spawn_blocking(move || objects.verify_and_promote(&session, &hash))
                .await
                .map_err(|e| Error::from(format!("The verifier task panicked: {}", e)))?
                .map_err(Error::from)?;

        match outcome {
            PromoteOutcome::Promoted | PromoteOutcome::AlreadyPresent => {}
            PromoteOutcome::Missing => {
                eprintln!("verifier: {} vanished from staging before verification", key)
            }
            PromoteOutcome::Corrupt { actual } => {
                eprintln!("verifier: discarded corrupt staged object {} (hashes to {})", key, actual)
            }
        }
    }

    Ok(())
}

/// Percent-decode an S3 event notification key: S3 encodes it the way a URL query string is
/// (`+` for space, `%XX` for everything else outside the unreserved set), so an object whose
/// name has a space or a non-ASCII character arrives encoded. A tiny inline decoder rather than
/// a new dependency — the alphabet is fixed and the input is one path string, not a query
/// string — used only as the fallback for event payloads that predate `url_decoded_key`.
/// A malformed or truncated `%` escape is left as a literal `%` rather than rejected: this feeds
/// `parse_staging_key`, which already treats anything that does not split into
/// `staging/{session}/{hash}` as "not a staging object".
fn decode_s3_key(key: &str) -> String {
    let bytes = key.as_bytes();
    let mut decoded: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                decoded.push(b' ');
                i += 1;
            }
            b'%' if i + 3 <= bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        decoded.push(byte);
                        i += 3;
                    }
                    None => {
                        decoded.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            byte => {
                decoded.push(byte);
                i += 1;
            }
        }
    }

    String::from_utf8_lossy(&decoded).into_owned()
}

/// Parse a `staging/{session}/{hash}` key into its parts. `None` for any other key shape.
fn parse_staging_key(key: &str) -> Option<(&str, &str)> {
    let rest = key.strip_prefix("staging/")?;
    let (session, hash) = rest.split_once('/')?;

    // A well-formed staging key is exactly two more segments; a nested slash is not one.
    if session.is_empty() || hash.is_empty() || hash.contains('/') {
        return None;
    }

    Some((session, hash))
}

/// Multi-thread on purpose, exactly as the control plane: [`AsyncBridge`] refuses a
/// single-threaded runtime.
#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Error> {
    run(service_fn(handler)).await
}

#[cfg(test)]
mod tests {
    use super::{decode_s3_key, parse_staging_key};

    #[test]
    fn a_staging_key_splits_into_session_and_hash() {
        assert_eq!(parse_staging_key("staging/lift-1/abc123"), Some(("lift-1", "abc123")));
    }

    #[test]
    fn a_non_staging_key_is_rejected() {
        assert_eq!(parse_staging_key("objects/abc123"), None);
        assert_eq!(parse_staging_key("staging/lift-1"), None, "no hash segment");
        assert_eq!(parse_staging_key("staging//abc"), None, "empty session");
        assert_eq!(parse_staging_key("staging/lift-1/"), None, "empty hash");
        assert_eq!(parse_staging_key("staging/lift-1/a/b"), None, "extra segment");
    }

    #[test]
    fn decode_s3_key_handles_plus_as_space_and_percent_escapes() {
        assert_eq!(decode_s3_key("a+b"), "a b");
        assert_eq!(decode_s3_key("a%2Bb"), "a+b");
        assert_eq!(decode_s3_key("Happy%20Face.jpg"), "Happy Face.jpg");
        assert_eq!(decode_s3_key("staging/lift+session/de%2Dad"), "staging/lift session/de-ad");
    }

    #[test]
    fn decode_s3_key_leaves_a_malformed_escape_as_a_literal_percent() {
        // A truncated or non-hex escape at the end of the string is not a valid encoding; it is
        // left as-is rather than dropped or panicking. `parse_staging_key` still judges the
        // result as not a staging key, which is the behaviour that matters.
        assert_eq!(decode_s3_key("abc%"), "abc%");
        assert_eq!(decode_s3_key("abc%2"), "abc%2");
        assert_eq!(decode_s3_key("abc%zz"), "abc%zz");
    }

    /// The regression this fix is for: an S3-notification-encoded staging key still parses into
    /// the right `(session, hash)` once decoded — encoding it first the way S3 would, then
    /// running it through the same `decode_s3_key` → `parse_staging_key` pipeline `handler` uses.
    #[test]
    fn a_url_encoded_staging_key_still_parses_after_decoding() {
        let key = "staging/lift+session-1/deadbeef";
        let decoded = decode_s3_key(key);
        assert_eq!(parse_staging_key(&decoded), Some(("lift session-1", "deadbeef")));
    }
}
