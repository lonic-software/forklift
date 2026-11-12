//! The staging verifier Lambda: an S3 object-created event → `verify_and_promote`.
//!
//! The large-blob half of the split-verify decision (`store.rs`). A client `PUT`s a large
//! object straight to `staging/{warehouse}/{session}/{hash}` via a presigned URL, which the
//! control plane never sees; S3 fires an object-created event, and this function runs the *same*
//! [`ObjectStore::verify_and_promote`] the control plane runs synchronously for small objects —
//! it stream-hashes the staged bytes and, only if `Blake3(bytes) == hash` and the object's form is
//! one core's own store takes in, server-side-copies them to that warehouse's canonical key. Promotion is the single moment the object becomes
//! fetchable (invariant 1), and it is idempotent, so this racing the control plane's
//! `commit_lift` on one hash is safe.
//!
//! The warehouse in the key is trusted because the key is: the only presigned `PUT` the control
//! plane mints is to a staging key of the warehouse the request was authorized for, and the
//! signature covers the key.
//!
//! Same blocking discipline as the control plane: `verify_and_promote` bridges S3 futures,
//! so it runs on a blocking thread. It needs an object store and a ref store bound to the
//! warehouse the staging key names ([`parse_staging_key`]): objects are keyed per warehouse, and
//! the key is all an S3 event carries. The ref store is read for two things only: the staged
//! object's redaction item, before promoting and again after, and the chunk round's fence.
//! Content whose redaction has applied is not promoted: its staged copy is deleted, and so is a
//! canonical copy a promotion wrote (a recipe whose chunks are still owed is kept, as the judge
//! keeps it), and the event is consumed. Neither is a chunk of a recipe whose chunk round is
//! running: its staged copy is deleted, so the lift naming it fails at its commit, and run again
//! after the round, uploads it afresh.
//!
//! Build with `cargo build -p forklift-aws-lambda --features lambda --release`.

use aws_lambda_events::event::s3::S3Event;
use forklift_aws_lambda::aws::s3::parse_staging_key;
use forklift_aws_lambda::aws::{build_clients, DynamoOps, DynamoRefStore, S3Ops};
use forklift_aws_lambda::head::{chunk_promotion_refused, delete_if_redaction_applied, AppliedCopies};
use forklift_aws_lambda::store::{ObjectStore, PromoteOutcome};
use forklift_aws_lambda::{config_from_env, AsyncBridge, S3ObjectStore};
use lambda_runtime::{run, service_fn, Error, LambdaEvent};
use tokio::sync::OnceCell;

/// What every warehouse's object store is built from: the S3 client, the bucket and the bridge,
/// built once per cold start. The store itself is built per staged object, for its warehouse.
struct Context {
    s3: S3Ops,
    dynamodb: DynamoOps,
    bucket: String,
    table: String,
    default_pallet: String,
    bridge: AsyncBridge,
}

static CONTEXT: OnceCell<Context> = OnceCell::const_new();

/// Resolve the context, building the S3 client and capturing the bridge on first use.
async fn context() -> Result<&'static Context, Error> {
    CONTEXT
        .get_or_try_init(|| async {
            let (config, _routing) = config_from_env().map_err(Error::from)?;
            let (s3, dynamodb) = build_clients(&config).await.map_err(Error::from)?;
            let bridge = AsyncBridge::current().map_err(Error::from)?;

            Ok(Context {
                s3,
                dynamodb,
                bucket: config.bucket,
                table: config.table,
                default_pallet: config.default_pallet,
                bridge,
            })
        })
        .await
}

/// Verify-and-promote every staged object an S3 event names.
async fn handler(event: LambdaEvent<S3Event>) -> Result<(), Error> {
    let context = context().await?;

    for record in event.payload.records {
        let Some(raw_key) = record.s3.object.key else {
            continue;
        };

        // S3 event notifications URL-encode the key the way a query string is (space → `+`,
        // everything else percent-escaped): "red flower.jpg" arrives as "red+flower.jpg". A
        // warehouse id, session id or hash cannot contain either character, but the key is split
        // on `/` first — an unrelated, encoded key (or a stray notification) must decode cleanly
        // before `parse_staging_key` judges its shape, or a corrupted split could misroute it.
        // Newer event payloads carry the already-decoded form in `url_decoded_key`; prefer it
        // when present, and fall back to a small inline decoder for payloads that predate it,
        // rather than pulling in a URL-decoding dependency for one string.
        let key = record.s3.object.url_decoded_key.unwrap_or_else(|| decode_s3_key(&raw_key));

        let Some(staged) = parse_staging_key(&key) else {
            // The trigger should be scoped to the `staging/` prefix, but a stray event is
            // skipped rather than failed — a failure would have S3/EventBridge retry forever.
            eprintln!("verifier: ignoring an S3 event that is not a staging object: {}", key);
            continue;
        };

        let (session, hash) = (staged.session.to_string(), staged.hash.to_string());
        // A warehouse the store refuses is skipped for the same reason: no retry can change it.
        let objects = match S3ObjectStore::new(
            context.s3.clone(),
            context.bucket.clone(),
            staged.warehouse.to_string(),
            context.bridge.clone(),
        ) {
            Ok(objects) => objects,
            Err(e) => {
                eprintln!("verifier: ignoring a staging object of no warehouse: {}: {}", key, e);
                continue;
            }
        };

        let refs = DynamoRefStore::new(
            context.dynamodb.clone(),
            context.table.clone(),
            staged.warehouse.to_string(),
            context.default_pallet.clone(),
            context.bridge.clone(),
        );

        // A storage error propagates (the event retries); a semantic outcome is logged and the
        // event is consumed — a corrupt object was discarded, not promoted, and retrying it
        // would only rediscard it. So is redacted content: its redaction item is read before
        // anything is promoted, and an applied one has its copies deleted instead; it is read
        // again after the promotion, and an applied one has the copy just written deleted. So is
        // content its host withholds, by its withheld item: the staged copy is deleted, never
        // promoted. An item that cannot be read is a storage error too: never promoted on a guess.
        let outcome = tokio::task::spawn_blocking(move || -> Result<Result<PromoteOutcome, Refused>, String> {
            let before = delete_if_redaction_applied(&objects, &refs, &session, &hash)?;
            if before.applied() {
                return Ok(Err(Refused::Applied(before)));
            }

            // While a redacted-chunk round runs, a chunk of one of its recipes is not promoted: its
            // staged copy is deleted, so the lift that needs it fails at its commit with
            // `staged_upload_discarded`, and run again after the round, uploads it afresh.
            if chunk_promotion_refused(&objects, &refs, std::slice::from_ref(&hash))?.is_some() {
                objects.delete_staged_copy(&session, &hash)?;
                return Ok(Err(Refused::RoundChunk));
            }

            let outcome = objects.verify_and_promote(&session, &hash)?;

            // Read again after anything that may have left a canonical copy: a redaction that
            // applied after the check above may have found no object to delete, and an event
            // retried after a failed delete finds the copy already promoted. A corrupt or
            // malformed copy was discarded, and is reported as such.
            if !matches!(outcome, PromoteOutcome::Corrupt { .. } | PromoteOutcome::Malformed { .. }) {
                let after = delete_if_redaction_applied(&objects, &refs, &session, &hash)?;
                if after.applied() {
                    return Ok(Err(Refused::Applied(after)));
                }
            }

            Ok(Ok(outcome))
        })
        .await
        .map_err(|e| Error::from(format!("The verifier task panicked: {}", e)))?
        .map_err(Error::from)?;

        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(Refused::RoundChunk) => {
                eprintln!("verifier: deleted staged object {}: it is a chunk of a redacted file being deleted", key);
                continue;
            }
            Err(Refused::Applied(AppliedCopies::RecipeKept)) => {
                eprintln!("verifier: deleted staged object {}: its content is redacted, and the recipe is kept until its chunks are deleted", key);
                continue;
            }
            Err(Refused::Applied(AppliedCopies::Deleted)) => {
                eprintln!("verifier: deleted staged object {} and any canonical copy: its content is redacted", key);
                continue;
            }
            Err(Refused::Applied(AppliedCopies::Withheld)) => {
                eprintln!("verifier: deleted staged object {}: this warehouse's host withholds its content", key);
                continue;
            }
            Err(Refused::Applied(AppliedCopies::Unapplied)) => unreachable!("only an applied redaction or a withhold stops a promotion"),
        };

        match outcome {
            PromoteOutcome::Promoted | PromoteOutcome::AlreadyPresent => {}
            PromoteOutcome::Missing => {
                eprintln!("verifier: {} vanished from staging before verification", key)
            }
            PromoteOutcome::Corrupt { actual } => {
                eprintln!("verifier: discarded corrupt staged object {} (hashes to {})", key, actual)
            }
            PromoteOutcome::Malformed { reason } => {
                eprintln!("verifier: discarded malformed staged object {}: {}", key, reason)
            }
        }
    }

    Ok(())
}

/// Why a staged object was not promoted.
enum Refused {
    /// Its redaction has applied, and its copies were deleted.
    Applied(AppliedCopies),
    /// It is a chunk of a recipe whose redacted-chunk round is running.
    RoundChunk,
}

/// Percent-decode an S3 event notification key: S3 encodes it the way a URL query string is
/// (`+` for space, `%XX` for everything else outside the unreserved set), so an object whose
/// name has a space or a non-ASCII character arrives encoded. A tiny inline decoder rather than
/// a new dependency — the alphabet is fixed and the input is one path string, not a query
/// string — used only as the fallback for event payloads that predate `url_decoded_key`.
/// A malformed or truncated `%` escape is left as a literal `%` rather than rejected: this feeds
/// `parse_staging_key`, which already treats anything that does not split into
/// `staging/{warehouse}/{session}/{hash}` as "not a staging object".
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

/// Multi-thread on purpose, exactly as the control plane: [`AsyncBridge`] refuses a
/// single-threaded runtime.
#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Error> {
    run(service_fn(handler)).await
}

#[cfg(test)]
mod tests {
    use super::decode_s3_key;
    use forklift_aws_lambda::aws::s3::{parse_staging_key, StagedKey};

    #[test]
    fn decode_s3_key_handles_plus_as_space_and_percent_escapes() {
        assert_eq!(decode_s3_key("a+b"), "a b");
        assert_eq!(decode_s3_key("a%2Bb"), "a+b");
        assert_eq!(decode_s3_key("Happy%20Face.jpg"), "Happy Face.jpg");
        assert_eq!(decode_s3_key("staging/wh/lift+session/de%2Dad"), "staging/wh/lift session/de-ad");
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
        let key = "staging/tenant-a/lift+session-1/deadbeef";
        let decoded = decode_s3_key(key);
        let staged = StagedKey { warehouse: "tenant-a", session: "lift session-1", hash: "deadbeef" };
        assert_eq!(parse_staging_key(&decoded), Some(staged));
    }
}
