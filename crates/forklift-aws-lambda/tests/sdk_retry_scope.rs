//! The SDK's own retry is off at the two transaction call sites, and only there (FORK-95 design
//! memo, claim C14).
//!
//! The head re-issues a transiently refused transaction itself, under one budget it can bound
//! against the invocation deadline. That budget is exact only if one store call is one
//! `TransactWriteItems` request: left on the shared default, the SDK would retry inside the call —
//! up to three requests and several seconds of backoff the head never sees. So both transaction
//! sites must send exactly one request per call, and every other DynamoDB call must keep the
//! SDK's retry, since none of them has a loop of its own.
//!
//! The DynamoDB endpoint here is a local stub that answers every request with a retryable
//! failure and counts what it was sent, so the count is what the SDK actually did. No AWS, no
//! LocalStack: it runs everywhere.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use forklift_aws_lambda::aws::build_stores;
use forklift_aws_lambda::reissue::request_token;
use forklift_aws_lambda::store::{
    CasOutcome, OfficePrecondition, RefStore, TransientCause, TrustWriteOutcome,
};
use forklift_aws_lambda::{AsyncBridge, AwsConfig, DynamoRefStore};
use forklift_core::model::remote::TrustAnchorDto;
use forklift_core::util::pallet_utils::PalletNamespace;

/// How the stub answers.
#[derive(Clone, Copy)]
enum Stub {
    /// Every request: HTTP 500 `InternalServerError`, which the SDK's default retry retries.
    InternalServerError,
    /// Transactions stall past the attempt timeout; everything else is a 500 as above.
    StallTransactions,
}

/// Requests received, by `X-Amz-Target` operation name.
type Counts = Arc<Mutex<HashMap<String, usize>>>;

/// Start the stub and build a ref store pointed at it.
async fn stub_store(mode: Stub) -> (DynamoRefStore, Counts) {
    // The provider chain must find these rather than a developer's own profile. Set once, before
    // any test's store exists: writing the environment while another thread reads it is a race.
    static CREDENTIALS: Once = Once::new();
    CREDENTIALS.call_once(|| {
        std::env::set_var("AWS_ACCESS_KEY_ID", "test");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "test");
        std::env::set_var("AWS_EC2_METADATA_DISABLED", "true");
    });

    let counts: Counts = Default::default();
    let seen = counts.clone();

    let app = axum::Router::new().fallback(move |headers: http::HeaderMap| {
        let seen = seen.clone();

        async move {
            let target = headers
                .get("x-amz-target")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.rsplit('.').next())
                .unwrap_or("unknown")
                .to_string();

            *seen.lock().unwrap().entry(target.clone()).or_default() += 1;

            if matches!(mode, Stub::StallTransactions) && target == "TransactWriteItems" {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }

            http::Response::builder()
                .status(500)
                .header("content-type", "application/x-amz-json-1.0")
                .body(axum::body::Body::from(
                    r#"{"__type":"com.amazonaws.dynamodb.v20120810#InternalServerError","message":"stub"}"#,
                ))
                .unwrap()
        }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind the stub");
    let addr = listener.local_addr().expect("the stub's address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let config = AwsConfig::new("bucket", "table", "warehouse")
        .with_region("us-east-1")
        .with_endpoint_url(format!("http://{}", addr));
    let bridge = AsyncBridge::current().expect("a multi-thread runtime");
    let (_objects, refs) = build_stores(&config, bridge).await.expect("build the stores");

    (refs, counts)
}

fn count(counts: &Counts, operation: &str) -> usize {
    counts.lock().unwrap().get(operation).copied().unwrap_or(0)
}

fn anchor() -> TrustAnchorDto {
    TrustAnchorDto {
        genesis: "b".repeat(64),
        enabled_at: 1_780_000_001,
        boundary: vec![],
        prior_genesis: Some("a".repeat(64)),
        adopts: None,
    }
}

/// The ref-update commit sends one request per call, and an internal error comes back as a
/// transient refusal of unknown fate — the head's to re-issue — not as a fault.
#[tokio::test(flavor = "multi_thread")]
async fn the_ref_update_commit_sends_one_request_per_call() {
    let (refs, counts) = stub_store(Stub::InternalServerError).await;

    let outcome = tokio::task::spawn_blocking(move || {
        refs.compare_and_set_head(
            PalletNamespace::User,
            "main",
            None,
            &"c".repeat(64),
            OfficePrecondition::NotConsumed,
            None,
            &request_token(),
        )
    })
    .await
    .unwrap();

    assert_eq!(outcome, Ok(CasOutcome::Transient(TransientCause::FateUnknown)));
    assert_eq!(count(&counts, "TransactWriteItems"), 1);
}

/// The anchor write is the second transaction site and carries the same override: one request.
#[tokio::test(flavor = "multi_thread")]
async fn the_anchor_write_sends_one_request_per_call() {
    let (refs, counts) = stub_store(Stub::InternalServerError).await;

    let outcome = tokio::task::spawn_blocking(move || {
        refs.replace_trust(&anchor().to_anchor(), "{}", None, &request_token())
    })
    .await
    .unwrap();

    assert_eq!(outcome, Ok(TrustWriteOutcome::Transient(TransientCause::FateUnknown)));
    assert_eq!(count(&counts, "TransactWriteItems"), 1);
}

/// The override is scoped to the transactions, not set on the client: a plain read still gets
/// the SDK's three attempts, because it has no re-issue loop of its own to fall back on.
#[tokio::test(flavor = "multi_thread")]
async fn other_calls_keep_the_sdk_retry() {
    let (refs, counts) = stub_store(Stub::InternalServerError).await;

    let outcome =
        tokio::task::spawn_blocking(move || refs.get_head(PalletNamespace::User, "main"))
            .await
            .unwrap();

    assert!(outcome.is_err());
    assert_eq!(count(&counts, "GetItem"), 3);
}

/// An attempt that does not answer within the attempt timeout is abandoned as one of unknown
/// fate rather than held open: the head's window check budgets exactly that long for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_transaction_is_abandoned_at_the_attempt_timeout() {
    let (refs, counts) = stub_store(Stub::StallTransactions).await;

    let started = Instant::now();
    let outcome = tokio::task::spawn_blocking(move || {
        refs.compare_and_set_head(
            PalletNamespace::User,
            "main",
            None,
            &"c".repeat(64),
            OfficePrecondition::NotConsumed,
            None,
            &request_token(),
        )
    })
    .await
    .unwrap();

    assert_eq!(outcome, Ok(CasOutcome::Transient(TransientCause::FateUnknown)));
    assert_eq!(count(&counts, "TransactWriteItems"), 1);
    assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
}

/// The one place the crate overrides a client's configuration is the shared transaction send.
/// A second `config_override` anywhere would be either a transaction site that bypassed it or a
/// scope widened past the transactions — the two ways the tests above could go stale unnoticed.
#[test]
fn the_transaction_send_is_the_only_config_override() {
    let mut sites = Vec::new();
    collect_overrides(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut sites);

    assert_eq!(sites.len(), 1, "{sites:?}");
    assert!(sites[0].ends_with("dynamo.rs"), "{sites:?}");
}

fn collect_overrides(dir: &Path, sites: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("read src") {
        let path = entry.expect("an entry").path();

        if path.is_dir() {
            collect_overrides(&path, sites);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let text = std::fs::read_to_string(&path).expect("read a source file");

            for _ in text.matches(".config_override(") {
                sites.push(path.display().to_string());
            }
        }
    }
}
