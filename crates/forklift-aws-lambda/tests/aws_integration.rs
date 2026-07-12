//! The protocol suite against the *real* S3 + DynamoDB store implementations.
//!
//! This is the sibling of `protocol.rs`: the same security-critical semantics, but over
//! [`S3ObjectStore`] and [`DynamoRefStore`] instead of the in-memory fakes. It is **gated on
//! `FORKLIFT_AWS_TEST_ENDPOINT`** — a LocalStack (or MinIO + DynamoDB-Local) endpoint — and
//! **skips cleanly** when the variable is unset, so a plain `cargo test --workspace` never
//! needs AWS or Docker.
//!
//! To run it against LocalStack:
//!
//! ```sh
//! docker run --rm -d -p 4566:4566 localstack/localstack
//! cargo build -p forklift  # the suite drives this CLI binary; it must exist first
//! FORKLIFT_AWS_TEST_ENDPOINT=http://localhost:4566 \
//!   AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_REGION=us-east-1 \
//!   cargo test -p forklift-aws-lambda --test aws_integration
//! ```
//!
//! Credentials come from the environment (the default provider chain), and each test
//! provisions a uniquely-named bucket and table so concurrent runs never collide.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aws_sdk_dynamodb::types::{
    AttributeDefinition, BillingMode, KeySchemaElement, KeyType, ScalarAttributeType, TableStatus,
};

use http::{Request, Response};

use forklift_aws_lambda::aws::{build_clients, build_stores, AwsConfig};
use forklift_aws_lambda::store::{
    CasOutcome, ObjectAccess, ObjectStore, PromoteOutcome, PutOutcome, PutTarget, RefStore,
    SignatureOutcome, TrustOutcome,
};
use forklift_aws_lambda::{
    handle, AsyncBridge, AuthConfig, DynamoRefStore, Head, HeadResult, Routing, S3ObjectStore,
};

use forklift_core::globals::StorageRootScope;
use forklift_core::model::remote::{CommitLiftRequest, RefUpdateRequest, TrustAnchorDto};
use forklift_core::util::pallet_utils::{self, PalletNamespace};
use forklift_core::util::{file_utils, object_utils, sign_utils};

// ---------------------------------------------------------------------------------------
// Gating and provisioning.
// ---------------------------------------------------------------------------------------

static UNIQUE: AtomicU64 = AtomicU64::new(0);

/// The endpoint to run against, or `None` — in which case every test is a clean skip.
fn endpoint() -> Option<String> {
    std::env::var("FORKLIFT_AWS_TEST_ENDPOINT").ok().filter(|value| !value.is_empty())
}

/// A short, unique suffix so a bucket/table/warehouse name is unique to this test.
fn unique(kind: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_nanos();
    let counter = UNIQUE.fetch_add(1, Ordering::Relaxed);

    format!("forklift-{}-{}-{}", kind, nanos, counter)
}

/// A configuration pointing at the test endpoint, with fresh bucket/table/warehouse names.
fn test_config(endpoint: &str) -> AwsConfig {
    AwsConfig::new(unique("bucket"), unique("table"), unique("warehouse"))
        .with_region("us-east-1")
        .with_endpoint_url(endpoint)
}

/// Create the bucket and the ref table (`wh` partition, `entity` sort), then wait for the
/// table to go active. Idempotent enough for a one-shot test: the names are unique.
async fn provision(config: &AwsConfig) {
    let (s3, dynamodb) = build_clients(config).await.expect("build clients");

    s3.create_bucket().bucket(&config.bucket).send().await.expect("create the bucket");

    dynamodb
        .create_table()
        .table_name(&config.table)
        .attribute_definitions(
            AttributeDefinition::builder()
                .attribute_name("wh")
                .attribute_type(ScalarAttributeType::S)
                .build()
                .expect("the wh attribute"),
        )
        .attribute_definitions(
            AttributeDefinition::builder()
                .attribute_name("entity")
                .attribute_type(ScalarAttributeType::S)
                .build()
                .expect("the entity attribute"),
        )
        .key_schema(
            KeySchemaElement::builder()
                .attribute_name("wh")
                .key_type(KeyType::Hash)
                .build()
                .expect("the partition key"),
        )
        .key_schema(
            KeySchemaElement::builder()
                .attribute_name("entity")
                .key_type(KeyType::Range)
                .build()
                .expect("the sort key"),
        )
        .billing_mode(BillingMode::PayPerRequest)
        .send()
        .await
        .expect("create the table");

    // Poll until the table is servable — creation is not synchronous.
    for _ in 0..60 {
        let described = dynamodb
            .describe_table()
            .table_name(&config.table)
            .send()
            .await
            .expect("describe the table");

        let status = described.table().and_then(|table| table.table_status());

        if status == Some(&TableStatus::Active) {
            return;
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    panic!("the table never became active");
}

// ---------------------------------------------------------------------------------------
// The store-level semantics: every trait method, and the equivalences head.rs branches on.
// ---------------------------------------------------------------------------------------

/// The full [`ObjectStore`] contract against real S3: hash-verified writes, the presigned
/// staging path, and verify-and-promote (promoted / already-present / missing / corrupt).
#[tokio::test(flavor = "multi_thread")]
async fn s3_object_store_upholds_the_object_contract() {
    let Some(endpoint) = endpoint() else {
        eprintln!("skipping: FORKLIFT_AWS_TEST_ENDPOINT is unset");
        return;
    };

    let config = test_config(&endpoint);
    provision(&config).await;

    let bridge = AsyncBridge::current().expect("a multi-thread runtime");
    let (objects, _refs) = build_stores(&config, bridge).await.expect("build stores");

    tokio::task::spawn_blocking(move || {
        let bytes = b"a real object".to_vec();
        let hash = object_utils::hash_object_bytes(&bytes);

        // Absent before it is written.
        assert!(!objects.exists(&hash).expect("exists"));
        assert!(objects.get(&hash).expect("get").is_none());

        // A hash-mismatched write is refused, and nothing lands.
        let err = objects.put_verified(&hash, b"not this").expect_err("mismatch");
        assert!(err.contains("does not match"), "{}", err);
        assert!(!objects.exists(&hash).expect("still absent"));

        // A verified write creates, and a re-write is an idempotent no-op.
        assert_eq!(objects.put_verified(&hash, &bytes).expect("put"), PutOutcome::Created);
        assert_eq!(
            objects.put_verified(&hash, &bytes).expect("re-put"),
            PutOutcome::AlreadyPresent
        );
        assert!(objects.exists(&hash).expect("exists now"));
        assert_eq!(objects.get(&hash).expect("get").as_deref(), Some(bytes.as_slice()));

        // A present object reads back as a presigned GET redirect.
        match objects.access(&hash).expect("access") {
            Some(ObjectAccess::Redirect(url)) => {
                assert!(url.contains(&format!("objects/{}", hash)), "{}", url)
            }
            other => panic!("expected a redirect, got {:?}", other.is_some()),
        }
        // An absent object has no access.
        assert!(objects.access(&"0".repeat(64)).expect("absent access").is_none());

        // The upload target is a presigned PUT under the session's staging prefix — never the
        // hash key — and a session-less upload is refused.
        let staged_hash = object_utils::hash_object_bytes(b"staged object");
        match objects.put_target(Some("lift-1"), &staged_hash).expect("target") {
            PutTarget::Staged(url) => {
                assert!(url.contains(&format!("staging/lift-1/{}", staged_hash)), "{}", url);
                assert!(!url.contains("/objects/"), "an upload must never target the hash key");
            }
            _ => panic!("expected a staged target"),
        }
        assert!(matches!(
            objects.put_target(None, &staged_hash).expect("no session"),
            PutTarget::SessionRequired
        ));

        // verify_and_promote: nothing staged is Missing.
        assert_eq!(
            objects.verify_and_promote("lift-1", &staged_hash).expect("missing"),
            PromoteOutcome::Missing
        );

        objects
    })
    .await
    .expect("the blocking assertions");
}

/// verify-and-promote across the presigned path: a client's staged bytes are not fetchable
/// until promoted, a corrupt staged object is discarded, and promotion is idempotent.
#[tokio::test(flavor = "multi_thread")]
async fn s3_verify_and_promote_gates_the_canonical_namespace() {
    let Some(endpoint) = endpoint() else {
        eprintln!("skipping: FORKLIFT_AWS_TEST_ENDPOINT is unset");
        return;
    };

    let config = test_config(&endpoint);
    provision(&config).await;

    let bridge = AsyncBridge::current().expect("a multi-thread runtime");
    let (s3, _dynamodb) = build_clients(&config).await.expect("clients");
    let (objects, _refs) = build_stores(&config, bridge).await.expect("stores");
    let bucket = config.bucket.clone();

    // Stage bytes straight to the staging prefix, as a client's presigned PUT would.
    let good = b"a good staged object".to_vec();
    let good_hash = object_utils::hash_object_bytes(&good);
    let corrupt_hash = object_utils::hash_object_bytes(b"the declared content");

    let stage = |key: String, body: Vec<u8>| {
        let s3 = s3.clone();
        let bucket = bucket.clone();
        async move {
            s3.put_object()
                .bucket(bucket)
                .key(key)
                .body(body.into())
                .send()
                .await
                .expect("stage bytes");
        }
    };
    stage(format!("staging/lift-1/{}", good_hash), good.clone()).await;
    stage(format!("staging/lift-1/{}", corrupt_hash), b"tampered".to_vec()).await;

    tokio::task::spawn_blocking(move || {
        // Neither is fetchable while merely staged.
        assert!(!objects.exists(&good_hash).expect("good not canonical"));
        assert!(!objects.exists(&corrupt_hash).expect("corrupt not canonical"));

        // The corrupt one is discarded, never promoted.
        match objects.verify_and_promote("lift-1", &corrupt_hash).expect("corrupt") {
            PromoteOutcome::Corrupt { actual } => {
                assert_ne!(actual, corrupt_hash, "the bytes hash to something else")
            }
            other => panic!("expected Corrupt, got {:?}", other),
        }
        assert!(!objects.exists(&corrupt_hash).expect("still not canonical"));

        // The good one promotes, and only then is it fetchable.
        assert_eq!(
            objects.verify_and_promote("lift-1", &good_hash).expect("promote"),
            PromoteOutcome::Promoted
        );
        assert!(objects.exists(&good_hash).expect("now canonical"));

        // Promotion is idempotent.
        assert_eq!(
            objects.verify_and_promote("lift-1", &good_hash).expect("retry"),
            PromoteOutcome::AlreadyPresent
        );

        // Signatures: created, idempotent, and a conflicting sidecar is refused.
        let parcel = "a".repeat(64);
        let sig_a = b"sidecar-a".to_vec();
        let sig_b = b"sidecar-b".to_vec();
        assert_eq!(
            objects.put_signature(&parcel, &sig_a).expect("sig"),
            SignatureOutcome::Created
        );
        assert_eq!(
            objects.put_signature(&parcel, &sig_a).expect("sig again"),
            SignatureOutcome::AlreadyPresent
        );
        assert_eq!(
            objects.put_signature(&parcel, &sig_b).expect("sig conflict"),
            SignatureOutcome::Conflict
        );
        assert_eq!(objects.get_signature(&parcel).expect("get sig").as_deref(), Some(sig_a.as_slice()));

        // A response body offloads to a presigned GET outside the object namespace.
        match objects.offload_response(b"a bundle").expect("offload") {
            Some(url) => {
                assert!(url.contains("responses/"), "{}", url);
                assert!(!url.contains("/objects/"), "a response is never an object");
            }
            None => panic!("an offloading store hands out a URL"),
        }

        // discard_session sweeps staging; a promote after it is Missing (nothing staged, not
        // canonical for a brand-new hash).
        objects.discard_session("lift-1").expect("discard");
        let fresh = object_utils::hash_object_bytes(b"never staged");
        assert_eq!(
            objects.verify_and_promote("lift-1", &fresh).expect("post-discard"),
            PromoteOutcome::Missing
        );
    })
    .await
    .expect("the blocking assertions");
}

/// The DoS-hardening review finding (C1): a staged object at or above
/// [`forklift_aws_lambda::aws::STREAMING_THRESHOLD_BYTES`] is never buffered whole —
/// `verify_and_promote` stream-hashes it through an incremental Blake3 hasher and promotes it
/// with a server-side `CopyObject` pinned to the exact bytes it hashed. This is the first time
/// that code path runs against real S3 (the unit tests in `s3.rs` exercise the pure
/// stream-hashing logic without AWS); it proves, over the real service: a corrupted large
/// object is discarded exactly as the small-object path discards one, a valid large object
/// promotes and reads back byte-identical (proof the `CopyObject`, not a buffered re-upload,
/// is what moved the bytes), and promotion stays idempotent.
#[tokio::test(flavor = "multi_thread")]
async fn s3_verify_and_promote_streams_large_staged_objects() {
    let Some(endpoint) = endpoint() else {
        eprintln!("skipping: FORKLIFT_AWS_TEST_ENDPOINT is unset");
        return;
    };

    let config = test_config(&endpoint);
    provision(&config).await;

    let bridge = AsyncBridge::current().expect("a multi-thread runtime");
    let (s3, _dynamodb) = build_clients(&config).await.expect("clients");
    let (objects, _refs) = build_stores(&config, bridge).await.expect("stores");
    let bucket = config.bucket.clone();

    // One byte over the streaming threshold: large enough to force the stream-hash +
    // CopyObject path this test exists to exercise, small enough (a few MiB) to keep the
    // test fast — the point is the code path, not the byte count.
    let size = (forklift_aws_lambda::aws::STREAMING_THRESHOLD_BYTES as usize) + 1;
    let good: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let good_hash = object_utils::hash_object_bytes(&good);

    // Bytes that do NOT match the hash they are declared (staged) under — the large-object
    // analogue of `s3_verify_and_promote_gates_the_canonical_namespace`'s corrupt case.
    let mut wrong_content = good.clone();
    wrong_content[0] ^= 0xFF;
    let declared_hash = object_utils::hash_object_bytes(b"a declared hash the bytes will not match");

    let stage = |key: String, body: Vec<u8>| {
        let s3 = s3.clone();
        let bucket = bucket.clone();
        async move {
            s3.put_object()
                .bucket(bucket)
                .key(key)
                .body(body.into())
                .send()
                .await
                .expect("stage bytes");
        }
    };
    stage(format!("staging/lift-big/{}", good_hash), good.clone()).await;
    stage(format!("staging/lift-big/{}", declared_hash), wrong_content).await;

    tokio::task::spawn_blocking(move || {
        // The corrupt large object is discarded, never promoted, over the streaming path.
        match objects.verify_and_promote("lift-big", &declared_hash).expect("corrupt large") {
            PromoteOutcome::Corrupt { actual } => assert_ne!(actual, declared_hash),
            other => panic!("expected Corrupt, got {:?}", other),
        }
        assert!(!objects.exists(&declared_hash).expect("never canonical"));

        // The valid large object promotes via stream-hash + CopyObject, and the bytes at the
        // canonical key are exactly the ones staged.
        assert_eq!(
            objects.verify_and_promote("lift-big", &good_hash).expect("promote large"),
            PromoteOutcome::Promoted
        );
        assert!(objects.exists(&good_hash).expect("now canonical"));
        assert_eq!(objects.get(&good_hash).expect("read back").as_deref(), Some(good.as_slice()));

        // Idempotent, exactly like the small-object path.
        assert_eq!(
            objects.verify_and_promote("lift-big", &good_hash).expect("retry"),
            PromoteOutcome::AlreadyPresent
        );
    })
    .await
    .expect("the blocking assertions");
}

/// The full [`RefStore`] contract against real DynamoDB: the head CAS (committed / conflict
/// with the current head reported), enumeration, and the one-way trust door.
#[tokio::test(flavor = "multi_thread")]
async fn dynamo_ref_store_upholds_the_cas_and_the_trust_door() {
    let Some(endpoint) = endpoint() else {
        eprintln!("skipping: FORKLIFT_AWS_TEST_ENDPOINT is unset");
        return;
    };

    let config = test_config(&endpoint);
    provision(&config).await;

    let bridge = AsyncBridge::current().expect("a multi-thread runtime");
    let (_objects, refs) = build_stores(&config, bridge).await.expect("stores");

    tokio::task::spawn_blocking(move || {
        let one = "1".repeat(64);
        let two = "2".repeat(64);

        // Unborn.
        assert!(refs.get_head(PalletNamespace::User, "main").expect("get").is_none());
        assert_eq!(refs.default_pallet().expect("default"), "main");

        // A CAS naming a *non-None* `expected` head against a pallet that does not exist yet
        // (no item at all, not merely a different head) is a conflict reporting no current
        // head — not a special "missing item" case distinct from an ordinary CAS mismatch, and
        // never an error (the `UpdateItem` condition `#h = :old` simply cannot hold against an
        // item with no `head` attribute).
        assert_eq!(
            refs.compare_and_set_head(PalletNamespace::User, "main", Some(&one), &two)
                .expect("cas against a genuinely missing item"),
            CasOutcome::Conflict { current: None }
        );

        // Create with expected None.
        assert_eq!(
            refs.compare_and_set_head(PalletNamespace::User, "main", None, &one).expect("create"),
            CasOutcome::Committed
        );
        assert_eq!(
            refs.get_head(PalletNamespace::User, "main").expect("get").as_deref(),
            Some(one.as_str())
        );

        // A replay with expected None now conflicts, reporting the current head.
        assert_eq!(
            refs.compare_and_set_head(PalletNamespace::User, "main", None, &two).expect("replay"),
            CasOutcome::Conflict { current: Some(one.clone()) }
        );

        // A fast-forward from the right expected commits.
        assert_eq!(
            refs.compare_and_set_head(PalletNamespace::User, "main", Some(&one), &two)
                .expect("ff"),
            CasOutcome::Committed
        );

        // A stale expected conflicts, again reporting the actual head.
        assert_eq!(
            refs.compare_and_set_head(PalletNamespace::User, "main", Some(&one), &one)
                .expect("stale"),
            CasOutcome::Conflict { current: Some(two.clone()) }
        );

        // A meta pallet lives in the same partition without colliding, and enumeration returns
        // both, qualified.
        assert_eq!(
            refs.compare_and_set_head(PalletNamespace::Meta, "office", None, &one).expect("office"),
            CasOutcome::Committed
        );
        let mut listed: Vec<(String, String)> = refs
            .list_refs()
            .expect("list")
            .into_iter()
            .map(|(pallet_ref, head)| (pallet_ref.to_wire(), head))
            .collect();
        listed.sort();
        assert_eq!(listed, vec![
            ("@office".to_string(), one.clone()),
            ("main".to_string(), two.clone()),
        ]);

        // The one-way trust door: established, idempotent, then refused for a different anchor.
        let anchor = TrustAnchorDto {
            genesis: one.clone(),
            enabled_at: 1_780_000_000,
            boundary: vec![two.clone()],
            prior_genesis: None,
            adopts: None,
        }
        .to_anchor();

        assert!(refs.get_trust().expect("no trust yet").is_none());
        assert_eq!(refs.put_trust_if_absent(&anchor).expect("plant"), TrustOutcome::Established);
        assert_eq!(
            refs.put_trust_if_absent(&anchor).expect("idempotent"),
            TrustOutcome::AlreadyIdentical
        );

        let different = TrustAnchorDto {
            genesis: two.clone(),
            enabled_at: 1_780_000_001,
            boundary: vec![],
            prior_genesis: None,
            adopts: None,
        }
        .to_anchor();
        assert_eq!(
            refs.put_trust_if_absent(&different).expect("conflict"),
            TrustOutcome::Conflict
        );

        // The stored anchor round-trips.
        let read = refs.get_trust().expect("read trust").expect("present");
        assert_eq!(read.genesis, one);
        assert_eq!(read.boundary, vec![two.clone()]);

        // replace_trust is the sanctioned overwrite.
        refs.replace_trust(&different).expect("replace");
        assert_eq!(refs.get_trust().expect("re-read").expect("present").genesis, two);
    })
    .await
    .expect("the blocking assertions");
}

// ---------------------------------------------------------------------------------------
// End to end: a real Head untrusted lift over the real stores, from a CLI-built warehouse.
// ---------------------------------------------------------------------------------------

/// An untrusted lift replayed through a [`Head`] over S3 + DynamoDB — the protocol suite's
/// `untrusted_lift_and_the_cas_guards`, but end to end against the real backends: objects are
/// hash-verified into S3, the closure audit runs against the S3-mirrored scratch, and the CAS
/// commit is the DynamoDB conditional write.
///
/// An S3 head *stages* presigned uploads (its `object_put` answers `422`/redirect, never a
/// direct store), so the objects are seeded into the canonical namespace with `put_verified`
/// — the same way `protocol.rs::batch_offloads_the_bundle_to_a_presigned_url` seeds a staging
/// store. The staging → verify-and-promote path itself is covered by
/// `s3_verify_and_promote_gates_the_canonical_namespace` above.
#[tokio::test(flavor = "multi_thread")]
async fn a_head_untrusted_lift_commits_over_s3_and_dynamodb() {
    let Some(endpoint) = endpoint() else {
        eprintln!("skipping: FORKLIFT_AWS_TEST_ENDPOINT is unset");
        return;
    };

    let config = test_config(&endpoint);
    provision(&config).await;

    // Build a real warehouse with the CLI and harvest its objects/refs.
    let area = Area::new("head-lift");
    prepare(&area, "wh");
    area.write_file("wh/readme.txt", "hello\n");
    area.write_file("wh/src/main.txt", "fn main\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "first"]);

    let harvest = harvest(&area.path("wh"));
    let main_head = harvest.head_of("main").expect("main has a head");

    let bridge = AsyncBridge::current().expect("a multi-thread runtime");
    let (objects, refs) = build_stores(&config, bridge).await.expect("stores");
    let head = Head::new(objects, refs);

    let expected = main_head.clone();

    tokio::task::spawn_blocking(move || {
        // Fresh remote: empty handshake, everything missing.
        let info = head.handshake().expect("handshake");
        assert!(info.pallets.is_empty());
        let all: Vec<String> = harvest.objects.keys().cloned().collect();
        assert_eq!(head.missing(&all).expect("missing").len(), all.len());

        // Seed the canonical namespace directly: `put_verified` hash-checks each object into
        // `objects/{hash}` on S3, and each sidecar into `signatures/{hash}`.
        for (hash, bytes) in &harvest.objects {
            head.objects.put_verified(hash, bytes).expect("seed object");
        }
        for (hash, sidecar) in &harvest.signatures {
            head.objects.put_signature(hash, sidecar).expect("seed signature");
        }
        assert!(head.missing(&all).expect("missing").is_empty());

        // The lift commits, and a stale replay conflicts.
        let request = RefUpdateRequest { old_head: None, new_head: main_head.clone() };
        head.ref_update("main", &request).expect("lift main");
        assert_eq!(
            head.ref_update("main", &request).expect_err("stale replay").status,
            forklift_aws_lambda::Status::Conflict
        );

        // The handshake reflects the committed head.
        assert_eq!(head.handshake().expect("handshake").pallets.get("main"), Some(&expected));
    })
    .await
    .expect("the blocking lift");
}

// ---------------------------------------------------------------------------------------
// End to end through the HTTP edge: `handle` over the real stores, following a 307 by hand.
// ---------------------------------------------------------------------------------------

/// Route one request through `entrypoint::handle` over freshly-built S3 + DynamoDB stores, on a
/// blocking thread (every `Head` method blocks on its store's futures, the sync/async seam
/// this suite is built around). The clients are cheap to clone, so a per-request store matches how
/// the control-plane binary serves each invocation.
async fn edge(
    s3: aws_sdk_s3::Client,
    dynamodb: aws_sdk_dynamodb::Client,
    bridge: AsyncBridge,
    config: AwsConfig,
    request: Request<Vec<u8>>,
) -> Response<Vec<u8>> {
    let routing = Routing::Single(config.warehouse_id.clone());

    tokio::task::spawn_blocking(move || {
        handle(
            &routing,
            // This suite drives the protocol walk over real S3 + DynamoDB, not the auth seam
            // (which has its own dedicated, store-free tests in `entrypoint.rs`) — every call
            // here is explicitly pre-authenticated rather than depending on the fail-closed
            // default.
            &AuthConfig::Open,
            move |warehouse_id| {
                let objects = S3ObjectStore::new(s3, config.bucket.clone(), bridge.clone());
                let refs = DynamoRefStore::new(
                    dynamodb,
                    config.table.clone(),
                    warehouse_id.to_string(),
                    config.default_pallet.clone(),
                    bridge,
                );

                Ok(Head::pooled(objects, refs, warehouse_id.to_string()))
            },
            request,
        )
    })
    .await
    .expect("the edge task")
}

/// The end-to-end proof the HTTP edge asks for: a client drives the control-plane router over
/// real S3 + DynamoDB, and the `307`s it answers actually carry bytes. One object travels the
/// whole presigned staging path — `PUT` answered `307`, bytes `PUT` straight to the presigned
/// staging URL, `commit_lift` verifying and promoting it — and is then read back through the
/// `307` a `GET` answers, byte-identical. The lift commits its ref over the DynamoDB CAS, and a
/// stale replay conflicts.
#[tokio::test(flavor = "multi_thread")]
async fn the_http_edge_drives_a_staged_lift_over_s3_and_dynamodb_following_307s() {
    let Some(endpoint) = endpoint() else {
        eprintln!("skipping: FORKLIFT_AWS_TEST_ENDPOINT is unset");
        return;
    };

    let config = test_config(&endpoint);
    provision(&config).await;

    // A real warehouse to harvest valid objects and a valid head from.
    let area = Area::new("edge-lift");
    prepare(&area, "wh");
    area.write_file("wh/readme.txt", "hello\n");
    area.write_file("wh/src/main.txt", "fn main\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "first"]);

    let harvest = harvest(&area.path("wh"));
    let main_head = harvest.head_of("main").expect("main has a head");

    let bridge = AsyncBridge::current().expect("a multi-thread runtime");
    let (s3, dynamodb) = build_clients(&config).await.expect("clients");

    // One object travels the whole staged way; the rest are seeded straight into the canonical
    // namespace (the staging → promote path is proven for the one, and the closure needs all).
    let staged_hash = harvest.objects.keys().next().expect("an object").clone();
    let staged_bytes = harvest.objects[&staged_hash].clone();

    // Handshake: a fresh warehouse.
    let response =
        edge(s3.clone(), dynamodb.clone(), bridge.clone(), config.clone(), get("/v1/warehouse")).await;
    assert_eq!(response.status().as_u16(), 200);

    // 1. `PUT` the staged object: the head answers `307` to a presigned *staging* URL.
    let response = edge(
        s3.clone(),
        dynamodb.clone(),
        bridge.clone(),
        config.clone(),
        put_body(&format!("/v1/objects/{}?session=lift-edge", staged_hash), b"ignored".to_vec()),
    )
    .await;
    assert_eq!(response.status().as_u16(), 307, "a staging head redirects the upload");
    let staging_url = location(&response);
    assert!(staging_url.contains(&format!("staging/lift-edge/{}", staged_hash)), "{}", staging_url);
    assert!(!staging_url.contains("/objects/"), "an upload never targets the hash key");

    // 2. Follow the redirect by hand: `PUT` the real bytes straight to storage.
    let client = reqwest::Client::new();
    let put =
        client.put(&staging_url).body(staged_bytes.clone()).send().await.expect("presigned PUT");
    assert!(put.status().is_success(), "the presigned staging PUT failed: {}", put.status());

    // Not fetchable yet — nothing at the hash key until it is promoted (invariant 1).
    let response = edge(
        s3.clone(),
        dynamodb.clone(),
        bridge.clone(),
        config.clone(),
        get(&format!("/v1/objects/{}", staged_hash)),
    )
    .await;
    assert_eq!(response.status().as_u16(), 404, "a staged object is not fetchable before commit");

    // 3. `commit_lift`: the head verifies and promotes the staged object to its canonical key.
    let commit = CommitLiftRequest { control_plane: vec![staged_hash.clone()], blobs: vec![], more: false };
    let response = edge(
        s3.clone(),
        dynamodb.clone(),
        bridge.clone(),
        config.clone(),
        post_body("/v1/lift/lift-edge/commit", &commit),
    )
    .await;
    assert_eq!(response.status().as_u16(), 200, "the clean commit promotes the staged object");

    // Seed the rest of the closure straight into the canonical namespace.
    {
        let objects = S3ObjectStore::new(s3.clone(), config.bucket.clone(), bridge.clone());
        let harvest_objects = harvest.objects.clone();
        let harvest_signatures = harvest.signatures.clone();
        let staged = staged_hash.clone();

        tokio::task::spawn_blocking(move || {
            for (hash, bytes) in &harvest_objects {
                if *hash == staged {
                    continue; // already promoted through the staging path
                }
                objects.put_verified(hash, bytes).expect("seed object");
            }
            for (hash, sidecar) in &harvest_signatures {
                objects.put_signature(hash, sidecar).expect("seed signature");
            }
        })
        .await
        .expect("seeding");
    }

    // 4. The lift commits over the DynamoDB CAS.
    let update = RefUpdateRequest { old_head: None, new_head: main_head.clone() };
    let response = edge(
        s3.clone(),
        dynamodb.clone(),
        bridge.clone(),
        config.clone(),
        post_body("/v1/pallets/main", &update),
    )
    .await;
    assert_eq!(response.status().as_u16(), 200, "the lift commits");

    // The handshake reflects the committed head.
    let response =
        edge(s3.clone(), dynamodb.clone(), bridge.clone(), config.clone(), get("/v1/warehouse")).await;
    let info: serde_json::Value = serde_json::from_slice(response.body()).expect("handshake json");
    assert_eq!(info["pallets"]["main"], serde_json::Value::String(main_head.clone()));

    // 5. Read the promoted object back through the `307` a GET answers, and follow it by hand:
    // the bytes are exactly what the client staged.
    let response = edge(
        s3.clone(),
        dynamodb.clone(),
        bridge.clone(),
        config.clone(),
        get(&format!("/v1/objects/{}", staged_hash)),
    )
    .await;
    assert_eq!(response.status().as_u16(), 307, "a present object reads back as a redirect");
    let canonical_url = location(&response);
    assert!(canonical_url.contains(&format!("objects/{}", staged_hash)), "{}", canonical_url);
    let fetched = client.get(&canonical_url).send().await.expect("presigned GET");
    assert!(fetched.status().is_success(), "the presigned GET failed: {}", fetched.status());
    let fetched = fetched.bytes().await.expect("read the object").to_vec();
    assert_eq!(fetched, staged_bytes, "the object fetched through the redirect is byte-identical");

    // 6. A stale replay conflicts.
    let response = edge(s3, dynamodb, bridge, config, post_body("/v1/pallets/main", &update)).await;
    assert_eq!(response.status().as_u16(), 409, "a stale replay conflicts");
}

/// A GET request with an empty body.
fn get(uri: &str) -> Request<Vec<u8>> {
    Request::builder().method("GET").uri(uri).body(Vec::new()).unwrap()
}

/// A PUT request with a raw body.
fn put_body(uri: &str, body: Vec<u8>) -> Request<Vec<u8>> {
    Request::builder().method("PUT").uri(uri).body(body).unwrap()
}

/// A POST request with a JSON body.
fn post_body<T: serde::Serialize>(uri: &str, body: &T) -> Request<Vec<u8>> {
    Request::builder().method("POST").uri(uri).body(serde_json::to_vec(body).unwrap()).unwrap()
}

/// The `Location` header of a redirect response.
fn location(response: &Response<Vec<u8>>) -> String {
    response
        .headers()
        .get(http::header::LOCATION)
        .expect("a Location header")
        .to_str()
        .unwrap()
        .to_string()
}

// ---------------------------------------------------------------------------------------
// A compact harness: build a warehouse with the CLI, harvest its objects/refs. (A trimmed
// copy of protocol.rs's harness, which cannot be shared across test binaries.)
// ---------------------------------------------------------------------------------------

struct Area {
    root: PathBuf,
}

impl Area {
    fn new(name: &str) -> Area {
        let root = std::env::temp_dir().join(unique(&format!("area-{}", name)));
        std::fs::create_dir_all(&root).expect("create the test area");
        Area { root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn forklift(&self, dir: &str, args: &[&str]) {
        let working = self.path(dir);
        std::fs::create_dir_all(&working).expect("create the working directory");

        let output = Command::new(forklift_binary())
            .args(args)
            .current_dir(&working)
            .env("FORKLIFT_GLOBAL_CONFIG", self.path("global.toml"))
            .env("FORKLIFT_KEYS_DIR", self.path("keys"))
            .output()
            .expect("run forklift");

        assert!(
            output.status.success(),
            "forklift {:?} failed: {}{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write_file(&self, relative: &str, content: &str) {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(path, content).expect("write file");
    }
}

impl Drop for Area {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The compiled `forklift` CLI, located next to this test binary.
fn forklift_binary() -> PathBuf {
    let mut dir = std::env::current_exe().expect("the test binary has a path");
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }

    let binary = dir.join(format!("forklift{}", std::env::consts::EXE_SUFFIX));
    assert!(binary.exists(), "forklift is not built at {}", binary.display());

    binary
}

struct Harvest {
    objects: HashMap<String, Vec<u8>>,
    signatures: HashMap<String, Vec<u8>>,
    refs: Vec<(pallet_utils::PalletRef, String)>,
}

impl Harvest {
    fn head_of(&self, wire: &str) -> Option<String> {
        self.refs
            .iter()
            .find(|(pallet_ref, _)| pallet_ref.to_wire() == wire)
            .map(|(_, head)| head.clone())
    }
}

fn harvest(warehouse: &Path) -> Harvest {
    let objects_dir = warehouse.join(".forklift").join("objects");
    let mut object_hashes: Vec<String> = Vec::new();
    let mut signature_hashes: Vec<String> = Vec::new();

    for fan in std::fs::read_dir(&objects_dir).expect("read the objects dir") {
        let fan = fan.expect("read a fan entry");
        if !fan.file_type().expect("fan file type").is_dir() {
            continue;
        }

        let prefix = fan.file_name().to_string_lossy().to_string();

        for object in std::fs::read_dir(fan.path()).expect("read a fan folder") {
            let object = object.expect("read an object entry");
            let name = object.file_name().to_string_lossy().to_string();

            match name.strip_suffix(".sig") {
                Some(rest) => signature_hashes.push(format!("{}{}", prefix, rest)),
                None => object_hashes.push(format!("{}{}", prefix, name)),
            }
        }
    }

    let _scope = StorageRootScope::enter(warehouse);

    let mut objects = HashMap::new();
    for hash in object_hashes {
        objects.insert(hash.clone(), file_utils::retrieve_object_by_hash(&hash).expect("object"));
    }

    let mut signatures = HashMap::new();
    for hash in signature_hashes {
        let sidecar = sign_utils::load_raw_parcel_signature(&hash)
            .expect("load signature")
            .expect("signature present");
        signatures.insert(hash, sidecar);
    }

    let refs = pallet_utils::all_pallet_refs().expect("read refs");

    Harvest { objects, signatures, refs }
}

fn prepare(area: &Area, dir: &str) {
    area.forklift(dir, &["prepare"]);
    area.forklift(dir, &["config", "--global", "operator.name", "AWS Integration Tester"]);
    area.forklift(dir, &["config", "--global", "operator.identifier", "tester@forklift"]);
}

// ---------------------------------------------------------------------------------------
// The money test: the REAL CLI drives the whole presigned staging flow end to end over the
// real S3 + DynamoDB, proving the founding-bet loop (lift → lower) closes byte-for-byte.
// ---------------------------------------------------------------------------------------

/// A local HTTP head that serves `entrypoint::handle` over the real S3 + DynamoDB stores, plus
/// a background "staging verifier" that promotes whatever lands under the `staging/` prefix.
///
/// The verifier stands in for the S3-object-created Lambda that does verify-and-promote in the
/// hosted deployment (that trait operation is already proven over real S3 by
/// `s3_verify_and_promote_gates_the_canonical_namespace`); here it lets the CLI's staging lift
/// complete — the control plane promotes the small control-plane objects synchronously at
/// commit, this poller promotes the working blobs, and the client's bounded commit retry
/// bridges the timing.
struct StagingHead {
    url: String,
    server: tokio::task::JoinHandle<()>,
    verifier: tokio::task::JoinHandle<()>,
}

impl StagingHead {
    async fn start(config: &AwsConfig, bridge: AsyncBridge) -> StagingHead {
        let (s3, dynamodb) = build_clients(config).await.expect("build clients for the shim");

        let state = Arc::new(ShimState {
            s3: s3.clone(),
            dynamodb,
            bridge: bridge.clone(),
            config: config.clone(),
        });

        let app = axum::Router::new().fallback(shim_handler).with_state(state);

        let listener =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind the shim listener");
        let addr = listener.local_addr().expect("the shim's bound address");

        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let verifier = tokio::spawn(run_staging_verifier(s3, config.clone(), bridge));

        StagingHead { url: format!("http://{}", addr), server, verifier }
    }
}

impl Drop for StagingHead {
    fn drop(&mut self) {
        self.server.abort();
        self.verifier.abort();
    }
}

/// The shim's shared state: the clients the per-request [`Head`] is built from.
struct ShimState {
    s3: aws_sdk_s3::Client,
    dynamodb: aws_sdk_dynamodb::Client,
    bridge: AsyncBridge,
    config: AwsConfig,
}

/// One request: buffer it into the `http::Request` the pure router speaks, run `handle` on a
/// blocking thread (every `Head` method blocks on its store's futures), convert back.
/// Headers are dropped on purpose: the shim always calls `handle` with `AuthConfig::Open` (see
/// below), so only the method, path/query and body matter — a real deployment configures
/// `AuthConfig::Token` instead and this shim would need to forward the header.
async fn shim_handler(
    axum::extract::State(state): axum::extract::State<Arc<ShimState>>,
    method: http::Method,
    uri: http::Uri,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let request =
        http::Request::builder().method(method).uri(uri).body(body.to_vec()).expect("a request");

    let routing = Routing::Single(state.config.warehouse_id.clone());
    let s3 = state.s3.clone();
    let dynamodb = state.dynamodb.clone();
    let bridge = state.bridge.clone();
    let config = state.config.clone();

    let response = tokio::task::spawn_blocking(move || {
        handle(
            &routing,
            &AuthConfig::Open,
            move |warehouse_id| {
                let objects = S3ObjectStore::new(s3, config.bucket.clone(), bridge.clone());
                let refs = DynamoRefStore::new(
                    dynamodb,
                    config.table.clone(),
                    warehouse_id.to_string(),
                    config.default_pallet.clone(),
                    bridge,
                );
                Ok(Head::pooled(objects, refs, warehouse_id.to_string()))
            },
            request,
        )
    })
    .await
    .expect("the shim routing task");

    let (parts, bytes) = response.into_parts();
    http::Response::from_parts(parts, axum::body::Body::from(bytes))
}

/// The stand-in staging verifier: promote every object sitting under the `staging/` prefix,
/// exactly as the S3-event Lambda would. Idempotent and race-safe (a control-plane object the
/// commit also promotes just reads `AlreadyPresent`; a swept key reads `Missing`), so running
/// it continuously is harmless.
async fn run_staging_verifier(s3: aws_sdk_s3::Client, config: AwsConfig, bridge: AsyncBridge) {
    loop {
        if let Ok(listed) =
            s3.list_objects_v2().bucket(&config.bucket).prefix("staging/").send().await
        {
            for object in listed.contents() {
                let Some(key) = object.key() else { continue };

                // key == "staging/{session}/{hash}"
                let parts: Vec<&str> = key.splitn(3, '/').collect();
                if parts.len() != 3 || parts[0] != "staging" {
                    continue;
                }

                let session = parts[1].to_string();
                let hash = parts[2].to_string();
                let s3 = s3.clone();
                let bucket = config.bucket.clone();
                let bridge = bridge.clone();

                let _ = tokio::task::spawn_blocking(move || {
                    let objects = S3ObjectStore::new(s3, bucket, bridge);
                    let _ = objects.verify_and_promote(&session, &hash);
                })
                .await;
            }
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The founding-bet loop, end to end through the real CLI: a warehouse the CLI built and lifted
/// through the presigned staging flow (`upload-targets` → presigned staging `PUT`s →
/// `commit_lift` → the DynamoDB ref CAS), franchised back down through the same head, and its
/// content is byte-identical. Then a second round proves incremental lift/lower over staging.
#[tokio::test(flavor = "multi_thread")]
async fn the_cli_lifts_and_lowers_through_the_staging_flow_over_s3_and_dynamodb() {
    let Some(endpoint) = endpoint() else {
        eprintln!("skipping: FORKLIFT_AWS_TEST_ENDPOINT is unset");
        return;
    };

    let config = test_config(&endpoint);
    provision(&config).await;

    let bridge = AsyncBridge::current().expect("a multi-thread runtime");
    let head = StagingHead::start(&config, bridge).await;

    // A real warehouse the CLI builds, points at the staging head, and lifts.
    let area = Area::new("cli-staging");
    prepare(&area, "wh");
    area.write_file("wh/readme.txt", "hello staging\n");
    area.write_file("wh/src/main.txt", "fn main\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "first"]);
    area.forklift("wh", &["config", "remote.url", &head.url]);

    // The money shot: lift through the staging flow (a failed staging PUT, commit, or ref update
    // would make `forklift lift` exit non-zero, which `Area::forklift` turns into a panic).
    area.forklift("wh", &["lift"]);

    // Franchise a fresh copy back down through the same head; the content must be byte-identical.
    area.forklift(".", &["franchise", &head.url, "clone"]);
    assert_eq!(
        std::fs::read_to_string(area.path("clone/readme.txt")).expect("clone readme"),
        "hello staging\n"
    );
    assert_eq!(
        std::fs::read_to_string(area.path("clone/src/main.txt")).expect("clone main"),
        "fn main\n"
    );

    // Incremental: new work in `wh` lifts through staging and lowers into `clone`.
    area.write_file("wh/readme.txt", "hello staging, twice\n");
    area.forklift("wh", &["load", "readme.txt"]);
    area.forklift("wh", &["stack", "second"]);
    area.forklift("wh", &["lift"]);

    area.forklift("clone", &["lower"]);
    assert_eq!(
        std::fs::read_to_string(area.path("clone/readme.txt")).expect("clone readme v2"),
        "hello staging, twice\n"
    );
}

// ---------------------------------------------------------------------------------------
// Chunk transport (§9.4b): the real-S3/DynamoDB verification the Stage 3 review flagged as
// the remaining gap. `protocol.rs` already proves this logic against the in-memory fakes
// (`a_ref_update_with_a_missing_chunk_is_refused`,
// `an_intermediate_commit_batch_does_not_sweep_a_later_batchs_staged_objects`); everything below
// re-proves the same invariants against real S3 + DynamoDB semantics, plus a full CLI round trip.
//
// A chunk is not a special case anywhere on the wire (`head.rs`/`aws/s3.rs` are type-blind by
// construction — see their module docs) — every test here drives a *real* `Recipe`/`Chunk` pair
// built through `object_utils::ingest_file`'s CDC path, the same code `forklift load` calls.
// ---------------------------------------------------------------------------------------

/// The chunk threshold (bytes): content at or above this is stored chunked. Mirrors
/// `chunk_utils::CHUNK_THRESHOLD_BYTES` (a frozen format constant) — kept as a local literal
/// exactly like `protocol.rs`'s copy of this harness, since the two test binaries cannot share one.
const CHUNK_THRESHOLD: usize = 8 * 1024 * 1024;

impl Area {
    /// Write a large (chunk-threshold-crossing) file of deterministic, RNG-free bytes so it is
    /// stored chunked and chunks reproducibly. A straight copy of `protocol.rs`'s helper of the
    /// same name.
    fn write_large_file(&self, relative: &str, seed: u64, size: usize) {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }

        let mut bytes = Vec::with_capacity(size);
        let mut state = seed;
        while bytes.len() < size {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            bytes.extend_from_slice(&(z ^ (z >> 31)).to_le_bytes());
        }
        bytes.truncate(size);
        std::fs::write(path, bytes).expect("write large file");
    }
}

/// A chunked file's identity, resolved from the *source* warehouse before it is harvested: the
/// recipe hash and its ordered chunk hashes. Resolved exactly the way `protocol.rs`'s
/// `a_ref_update_with_a_missing_chunk_is_refused` resolves them.
struct ChunkedFileIdentity {
    recipe_hash: String,
    chunk_hashes: Vec<String>,
}

/// Build a warehouse with one file at/above [`CHUNK_THRESHOLD`] (so `forklift load` stores it as
/// a recipe plus chunks), lift-stack it, and harvest every object/signature/ref plus the chunked
/// file's own identity. Deterministic bytes (no RNG), so the chunk boundaries — and therefore the
/// chunk count — are reproducible from one run to the next.
fn build_chunked_warehouse(
    name: &str,
    size: usize,
) -> (Area, Harvest, String, ChunkedFileIdentity) {
    let area = Area::new(name);
    prepare(&area, "wh");
    area.write_large_file("wh/big.bin", 0xC0FFEE, size);
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "a giant"]);

    let harvest = harvest(&area.path("wh"));
    let main_head = harvest.head_of("main").expect("main has a head");

    let identity = {
        let _scope = StorageRootScope::enter(&area.path("wh"));
        let tree = object_utils::load_parcel(&main_head).expect("head parcel").tree_hash;
        let (recipe_hash, item_type) = object_utils::resolve_tree_file(&tree, "big.bin")
            .expect("resolve big.bin")
            .expect("big.bin is tracked");
        assert!(item_type.is_chunked(), "the giant is stored chunked");
        let chunk_hashes = object_utils::recipe_chunk_hashes(&recipe_hash).expect("chunk hashes");

        ChunkedFileIdentity { recipe_hash, chunk_hashes }
    };

    (area, harvest, main_head, identity)
}

/// Take a [`S3ObjectStore`] by value, run one blocking `ObjectStore` call on it, and hand it back
/// alongside the result — `S3ObjectStore` is not `Clone`, so ownership threads through a chain of
/// blocking calls instead of being rebuilt or shared. Used only by the chunk-transport tests
/// below, which (unlike the tests above) need real `reqwest` calls interleaved between blocking
/// store calls.
async fn chunk_stage_target(objects: S3ObjectStore, session: &str, hash: &str) -> (S3ObjectStore, String) {
    let (session, hash) = (session.to_string(), hash.to_string());

    tokio::task::spawn_blocking(move || {
        let url = match objects.put_target(Some(&session), &hash).expect("put target") {
            PutTarget::Staged(url) => url,
            _ => panic!("expected a staged target"),
        };
        (objects, url)
    })
    .await
    .expect("blocking put_target")
}

async fn chunk_promote(
    objects: S3ObjectStore,
    session: &str,
    hash: &str,
) -> (S3ObjectStore, PromoteOutcome) {
    let (session, hash) = (session.to_string(), hash.to_string());

    tokio::task::spawn_blocking(move || {
        let outcome = objects.verify_and_promote(&session, &hash).expect("verify_and_promote");
        (objects, outcome)
    })
    .await
    .expect("blocking verify_and_promote")
}

async fn chunk_object_exists(objects: S3ObjectStore, hash: &str) -> (S3ObjectStore, bool) {
    let hash = hash.to_string();

    tokio::task::spawn_blocking(move || {
        let found = objects.exists(&hash).expect("exists");
        (objects, found)
    })
    .await
    .expect("blocking exists")
}

async fn chunk_object_get(objects: S3ObjectStore, hash: &str) -> (S3ObjectStore, Option<Vec<u8>>) {
    let hash = hash.to_string();

    tokio::task::spawn_blocking(move || {
        let bytes = objects.get(&hash).expect("get");
        (objects, bytes)
    })
    .await
    .expect("blocking get")
}

/// Stage bytes straight to `staging/{session}/{hash}` via the real client — the same shortcut
/// `s3_verify_and_promote_gates_the_canonical_namespace` takes above, standing in for a client's
/// presigned staging `PUT` (proven end to end, for a chunk specifically, by
/// `a_chunked_files_chunks_stage_and_promote_over_real_s3` below).
async fn chunk_stage_raw(s3: &aws_sdk_s3::Client, bucket: &str, session: &str, hash: &str, bytes: Vec<u8>) {
    s3.put_object()
        .bucket(bucket)
        .key(format!("staging/{}/{}", session, hash))
        .body(bytes.into())
        .send()
        .await
        .expect("stage bytes");
}

/// List every key currently under `session`'s staging prefix in real S3 — the ground truth this
/// suite's in-memory-fake counterpart (`protocol.rs`'s `MemoryObjectStore::staged_count`) has no
/// equivalent for, and the direct proof a sweep did or did not run.
async fn chunk_list_staging_keys(s3: &aws_sdk_s3::Client, bucket: &str, session: &str) -> Vec<String> {
    let listed = s3
        .list_objects_v2()
        .bucket(bucket)
        .prefix(format!("staging/{}/", session))
        .send()
        .await
        .expect("list staging keys");

    listed.contents().iter().filter_map(|object| object.key().map(str::to_string)).collect()
}

/// Take a [`Head`] over the real stores by value, run one blocking `commit_lift` call, and hand
/// it back alongside the result — the same ownership-threading `chunk_stage_target` and its
/// siblings use, since `Head` is not `Clone` either.
async fn chunk_commit_lift(
    objects: S3ObjectStore,
    refs: DynamoRefStore,
    session: &str,
    control_plane: Vec<String>,
    blobs: Vec<String>,
    more: bool,
) -> (S3ObjectStore, DynamoRefStore, HeadResult<()>) {
    let session = session.to_string();

    tokio::task::spawn_blocking(move || {
        let head = Head::new(objects, refs);
        let result = head.commit_lift(&session, &control_plane, &blobs, more);
        (head.objects, head.refs, result)
    })
    .await
    .expect("blocking commit_lift")
}

/// PROOF 1 — a chunked file's lift, chunk by chunk, over real S3: each chunk stages via its own
/// presigned `PUT` (the same `put_target`/`staging/{session}/{hash}` scheme as any other object —
/// a chunk is not a special case anywhere on the wire, see `head.rs::commit_lift`'s doc comment),
/// and lands canonical only once `verify_and_promote` runs on the staged key.
///
/// LocalStack's community edition does not deliver S3 bucket-notification events to a Lambda the
/// way production does (that is what `verifier.rs`'s `S3Event` handler reacts to), so — exactly
/// like this suite's `run_staging_verifier` stand-in and `s3_verify_and_promote_gates_the_canonical_namespace`
/// above — this test drives the verifier by calling `verify_and_promote` directly on each staged
/// key, rather than waiting for a bucket notification that never arrives here.
#[tokio::test(flavor = "multi_thread")]
async fn a_chunked_files_chunks_stage_and_promote_over_real_s3() {
    let Some(endpoint) = endpoint() else {
        eprintln!("skipping: FORKLIFT_AWS_TEST_ENDPOINT is unset");
        return;
    };

    let (_area, harvest, _main_head, identity) =
        build_chunked_warehouse("chunk-lift", CHUNK_THRESHOLD + 3 * 1024 * 1024);
    assert!(identity.chunk_hashes.len() >= 2, "a chunked file always has at least two chunks");

    let config = test_config(&endpoint);
    provision(&config).await;
    let bridge = AsyncBridge::current().expect("a multi-thread runtime");
    let (objects, _refs) = build_stores(&config, bridge).await.expect("stores");

    let session = "lift-chunks";

    // The recipe itself travels the identical staging path — it is just another hash-addressed
    // object (the wire has no separate "chunks" field; see `remote_utils.rs`'s classification).
    let recipe_bytes = harvest.objects[&identity.recipe_hash].clone();
    let (objects, staging_url) = chunk_stage_target(objects, session, &identity.recipe_hash).await;
    let put =
        reqwest::Client::new().put(&staging_url).body(recipe_bytes.clone()).send().await.expect("PUT");
    assert!(put.status().is_success(), "the presigned staging PUT failed: {}", put.status());

    let (objects, outcome) = chunk_promote(objects, session, &identity.recipe_hash).await;
    assert_eq!(outcome, PromoteOutcome::Promoted);

    let (objects, exists) = chunk_object_exists(objects, &identity.recipe_hash).await;
    assert!(exists, "the recipe is canonical after promotion");
    let (mut objects, got) = chunk_object_get(objects, &identity.recipe_hash).await;
    assert_eq!(got.as_deref(), Some(recipe_bytes.as_slice()));

    // A promoted recipe reads back and parses — the store-backed `load_recipe_chunks` path
    // `Head::ref_update` uses for the W4 gate (proven end to end again, through a real head, by
    // `ref_update_refuses_a_missing_chunk_and_commits_once_complete_over_s3_and_dynamodb` below).
    let reparsed = object_utils::parse_recipe_bytes(&identity.recipe_hash, &recipe_bytes)
        .expect("a promoted recipe parses");
    let reparsed_chunk_hashes: Vec<String> =
        reparsed.chunks.into_iter().map(|chunk| chunk.hash).collect();
    assert_eq!(reparsed_chunk_hashes, identity.chunk_hashes);

    // Every chunk: absent while staged, `Promoted` once verified, and its bytes read back
    // byte-identical — real S3 semantics for the exact object type the W4 gate cares about.
    for chunk_hash in &identity.chunk_hashes {
        let bytes = harvest.objects[chunk_hash].clone();

        let (o, existed_before) = chunk_object_exists(objects, chunk_hash).await;
        assert!(!existed_before, "a chunk is not fetchable while merely staged");

        let (o, staging_url) = chunk_stage_target(o, session, chunk_hash).await;
        let put = reqwest::Client::new().put(&staging_url).body(bytes.clone()).send().await.expect("PUT");
        assert!(put.status().is_success(), "the presigned staging PUT failed: {}", put.status());

        let (o, outcome) = chunk_promote(o, session, chunk_hash).await;
        assert_eq!(outcome, PromoteOutcome::Promoted, "chunk {} did not promote", chunk_hash);

        let (o, existed_after) = chunk_object_exists(o, chunk_hash).await;
        assert!(existed_after, "chunk {} is canonical after promotion", chunk_hash);

        let (o, got) = chunk_object_get(o, chunk_hash).await;
        assert_eq!(got.as_deref(), Some(bytes.as_slice()));

        objects = o;
    }

    // Promotion is idempotent for a chunk exactly as for any other object: nothing is staged the
    // second time (the first promotion already dropped the staged copy), so the store simply
    // finds the hash already canonical.
    let repeat_hash = identity.chunk_hashes[0].clone();
    let (_objects, outcome) = chunk_promote(objects, session, &repeat_hash).await;
    assert_eq!(outcome, PromoteOutcome::AlreadyPresent);
}

/// PROOF 2 — a multi-batch `commit_lift` over real S3: an intermediate batch (`more: true`) must
/// never sweep the session's staging prefix (a later batch's still-staged chunks would be
/// discarded before they can be promoted), and only the final batch (`more: false`) sweeps —
/// verified here by re-`list_objects_v2`-ing the staging prefix in between, over real S3, rather
/// than the in-memory fake's `staged_count()` (`protocol.rs`'s
/// `an_intermediate_commit_batch_does_not_sweep_a_later_batchs_staged_objects`). A third, never
/// referenced chunk stands in for an unrelated straggler still sitting in the session's staging
/// prefix, proving the final sweep is session-wide, not scoped to the hashes the last batch named.
#[tokio::test(flavor = "multi_thread")]
async fn a_multi_batch_commit_lift_does_not_sweep_staging_until_the_final_batch_over_s3() {
    let Some(endpoint) = endpoint() else {
        eprintln!("skipping: FORKLIFT_AWS_TEST_ENDPOINT is unset");
        return;
    };

    let (_area, harvest, _main_head, identity) =
        build_chunked_warehouse("chunk-batches", CHUNK_THRESHOLD + 6 * 1024 * 1024);
    assert!(
        identity.chunk_hashes.len() >= 3,
        "need at least three chunks: one per batch plus a never-referenced straggler"
    );

    let config = test_config(&endpoint);
    provision(&config).await;
    let bridge = AsyncBridge::current().expect("a multi-thread runtime");
    let (s3, _dynamodb) = build_clients(&config).await.expect("clients");
    let bucket = config.bucket.clone();
    let (objects, refs) = build_stores(&config, bridge).await.expect("stores");

    let session = "lift-batches";
    let first_chunk = identity.chunk_hashes[0].clone();
    let second_chunk = identity.chunk_hashes[1].clone();
    let straggler = identity.chunk_hashes[2].clone();

    // Stage every chunk this test touches straight via the real client (the presigned-PUT path
    // itself is proven end to end above; this test is about the sweep, not the upload).
    for hash in [&first_chunk, &second_chunk, &straggler] {
        let bytes = harvest.objects[hash].clone();
        chunk_stage_raw(&s3, &bucket, session, hash, bytes).await;
    }

    // The background staging verifier has caught up on batch 1's chunk only.
    let (objects, outcome) = chunk_promote(objects, session, &first_chunk).await;
    assert_eq!(outcome, PromoteOutcome::Promoted);

    // Intermediate batch (`more: true`): presence-checks the promoted chunk, must not sweep.
    let (objects, refs, result) =
        chunk_commit_lift(objects, refs, session, vec![], vec![first_chunk.clone()], true).await;
    result.expect("the intermediate batch commits");

    let staged = chunk_list_staging_keys(&s3, &bucket, session).await;
    assert!(
        staged.iter().any(|key| key.ends_with(&second_chunk)),
        "batch 2's staged chunk must survive an intermediate (more) batch: {:?}",
        staged
    );
    assert!(
        staged.iter().any(|key| key.ends_with(&straggler)),
        "an object no batch names must also survive an intermediate batch: {:?}",
        staged
    );

    // The verifier catches up on batch 2's chunk before the final batch commits.
    let (objects, outcome) = chunk_promote(objects, session, &second_chunk).await;
    assert_eq!(outcome, PromoteOutcome::Promoted);

    // Final batch (`more: false`): presence-checks batch 2's chunk and sweeps the whole
    // session — including the straggler, which no batch ever named.
    let (_objects, _refs, result) =
        chunk_commit_lift(objects, refs, session, vec![], vec![second_chunk.clone()], false).await;
    result.expect("the final batch commits");

    let staged_after = chunk_list_staging_keys(&s3, &bucket, session).await;
    assert!(
        staged_after.is_empty(),
        "the final (more: false) batch sweeps the whole session, straggler included: {:?}",
        staged_after
    );
}

/// PROOF 3 — the commit-gate closure audit's chunk descent (§9.4b W4), over real S3 + DynamoDB:
/// `Head::ref_update` refuses a ref whose chunked file is missing even one chunk (the recipe
/// itself, and every other chunk, is present — a walk that stopped at the recipe would wrongly
/// pass), then commits once the withheld chunk is uploaded. The real-backend analogue of
/// `protocol.rs`'s `a_ref_update_with_a_missing_chunk_is_refused`, plus the store-backed
/// `load_recipe_chunks` read (`Head::ref_update`'s local closure over
/// `object_utils::parse_recipe_bytes`) against a recipe actually sitting in S3, not a fake.
#[tokio::test(flavor = "multi_thread")]
async fn ref_update_refuses_a_missing_chunk_and_commits_once_complete_over_s3_and_dynamodb() {
    let Some(endpoint) = endpoint() else {
        eprintln!("skipping: FORKLIFT_AWS_TEST_ENDPOINT is unset");
        return;
    };

    let (_area, harvest, main_head, identity) =
        build_chunked_warehouse("chunk-w4", CHUNK_THRESHOLD + 2 * 1024 * 1024);

    let config = test_config(&endpoint);
    provision(&config).await;
    let bridge = AsyncBridge::current().expect("a multi-thread runtime");
    let (objects, refs) = build_stores(&config, bridge).await.expect("stores");
    let head = Head::new(objects, refs);

    let victim = identity.chunk_hashes[0].clone();
    let expected_chunks = identity.chunk_hashes.clone();
    let recipe_hash = identity.recipe_hash.clone();

    tokio::task::spawn_blocking(move || {
        // Seed everything except one chunk directly into the canonical namespace (mirrors how
        // `a_head_untrusted_lift_commits_over_s3_and_dynamodb` above seeds a closure).
        for (hash, bytes) in &harvest.objects {
            if *hash == victim {
                continue;
            }
            head.objects.put_verified(hash, bytes).expect("seed object");
        }
        for (hash, sidecar) in &harvest.signatures {
            head.objects.put_signature(hash, sidecar).expect("seed signature");
        }
        assert!(
            head.objects.exists(&recipe_hash).expect("exists"),
            "the recipe itself is present on the head"
        );

        let request = RefUpdateRequest { old_head: None, new_head: main_head.clone() };
        let err = head.ref_update("main", &request).expect_err("a missing chunk fails the closure");
        assert_eq!(err.status, forklift_aws_lambda::Status::Unprocessable);

        // The control: upload the withheld chunk, and the identical update now commits.
        let victim_bytes = harvest.objects[&victim].clone();
        head.objects.put_verified(&victim, &victim_bytes).expect("upload the last chunk");
        head.ref_update("main", &request).expect("complete once every chunk is present");

        assert_eq!(head.handshake().expect("handshake").pallets.get("main"), Some(&main_head));

        // The store-backed `load_recipe_chunks` path: read the promoted recipe back from real S3
        // and parse it — exactly as `Head::ref_update`'s closure does for the gate above.
        let bytes = head.objects.get(&recipe_hash).expect("get").expect("recipe present");
        let recipe = object_utils::parse_recipe_bytes(&recipe_hash, &bytes).expect("parses");
        let chunk_hashes: Vec<String> = recipe.chunks.into_iter().map(|chunk| chunk.hash).collect();
        assert_eq!(chunk_hashes, expected_chunks);
    })
    .await
    .expect("the blocking closure audit");
}

/// PROOF 4 — a full franchise of a chunked file from the AWS head, end to end through the real
/// CLI: presigned chunk `GET`s (redirects) hash-verify (`assemble_chunked_file`'s incremental
/// `Blake3`) and the file materializes byte-identical. Reuses [`StagingHead`] unmodified — the
/// staging verifier, the presigned PUT/GET machinery and `commit_lift` pagination are all
/// type-blind (see `head.rs`/`aws/s3.rs`'s module docs), so a chunked file needs no special
/// handling anywhere in this harness; it only needs to be big enough to be stored chunked.
#[tokio::test(flavor = "multi_thread")]
async fn the_cli_lifts_and_franchises_a_chunked_file_over_s3_and_dynamodb() {
    let Some(endpoint) = endpoint() else {
        eprintln!("skipping: FORKLIFT_AWS_TEST_ENDPOINT is unset");
        return;
    };

    let config = test_config(&endpoint);
    provision(&config).await;

    let bridge = AsyncBridge::current().expect("a multi-thread runtime");
    let head = StagingHead::start(&config, bridge).await;

    let area = Area::new("cli-staging-chunked");
    prepare(&area, "wh");
    area.write_large_file("wh/big.bin", 0xBADA55, CHUNK_THRESHOLD + 5 * 1024 * 1024);
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "a giant"]);
    area.forklift("wh", &["config", "remote.url", &head.url]);

    // The money shot: lift a chunked file through the staging flow (a failed staging PUT, a
    // rejected commit, or a refused ref update would make `forklift lift` exit non-zero, which
    // `Area::forklift` turns into a panic).
    area.forklift("wh", &["lift"]);

    // Franchise a fresh copy back down through the same head — every chunk travels a presigned
    // `GET` redirect, and `assemble_chunked_file` re-verifies the whole content hash as it writes.
    area.forklift(".", &["franchise", &head.url, "clone"]);

    let original = std::fs::read(area.path("wh/big.bin")).expect("read the source giant");
    let materialized = std::fs::read(area.path("clone/big.bin")).expect("read the franchised giant");
    assert_eq!(
        materialized, original,
        "the franchised chunked file must be byte-identical to the source"
    );
}
