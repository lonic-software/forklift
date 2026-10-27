//! The protocol suite for the AWS serverless head, run entirely in CI without AWS.
//!
//! The strategy: build a *real* warehouse with the `forklift` CLI (so the objects, the
//! signed office chain and the trust anchor are exactly what a client produces), harvest
//! its objects and refs, then replay the lift/lower protocol against a [`Head`] over the
//! in-memory fakes — the same handler logic the AWS Lambda control-plane function runs.
//! This exercises the security-critical paths (hash-verified uploads, the fast-forward
//! CAS, and the full offline audit reused via the scratch bridge) against abstracted
//! storage, no S3 or DynamoDB required.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use forklift_aws_lambda::reissue::request_token;
use forklift_aws_lambda::error::Status;
use forklift_aws_lambda::head::{ObjectReadResult, ObjectWriteResult, TrustResult};
use forklift_aws_lambda::memory::{MemoryObjectStore, MemoryRefStore};
use forklift_aws_lambda::scratch::Scratch;
use forklift_aws_lambda::store::{
    ExpectedHead, RefHead, RefListing,
    CasOutcome, ObjectStore, OfficePrecondition, PromoteOutcome, PutOutcome, RefStore,
    SignatureOutcome, TransientCause, TrustOutcome, TrustWriteOutcome,
};
use forklift_aws_lambda::{AsyncBridge, BatchResult, Head};

use forklift_core::error::{CoreError, RefusalCode};
use forklift_core::globals::StorageRootScope;
use forklift_core::util::authority_utils::Principal;
use forklift_core::model::remote::{HoldAnswer, RefUpdateRequest, TrustAnchorDto};
use forklift_core::util::office_utils::{self, OFFICE_PALLET_NAME};
use forklift_core::util::pallet_utils;
use forklift_core::util::{file_utils, object_utils, sign_utils};

// ---------------------------------------------------------------------------------------
// Harness: build a warehouse with the CLI, harvest it into the fakes.
// ---------------------------------------------------------------------------------------

static AREA_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The compiled `forklift` CLI. Cargo exposes `CARGO_BIN_EXE_*` only to a package's own
/// tests, so — like `forklift/tests/remote.rs` locates the server next to the CLI — this
/// locates the CLI next to the test binary (both land in the target dir).
fn forklift_binary() -> PathBuf {
    let mut dir = std::env::current_exe().expect("the test binary has a path");
    dir.pop(); // the test executable's file name
    if dir.ends_with("deps") {
        dir.pop();
    }

    let binary = dir.join(format!("forklift{}", std::env::consts::EXE_SUFFIX));

    assert!(
        binary.exists(),
        "forklift is not built at {}; run the suite via a workspace `cargo test`.",
        binary.display()
    );

    binary
}

/// `forklift` with fsync off. Durability is not what these tests check, and paying an fsync per
/// write made the suite much slower.
/// Only the CLI is affected: the server half runs in this test process, which leaves fsync on.
fn forklift_command() -> Command {
    let mut command = Command::new(forklift_binary());
    command.env("FORKLIFT_FSYNC", "0");
    command
}

/// A scratch directory for one test, cleaned up on drop.
struct Area {
    root: PathBuf,
}

impl Area {
    fn new(name: &str) -> Area {
        let unique = format!(
            "forklift-aws-test-{}-{}-{}",
            name,
            std::process::id(),
            AREA_COUNTER.fetch_add(1, Ordering::Relaxed)
        );

        let root = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&root).expect("create the test area");
        Area { root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// Run the CLI in a subdirectory of the area (created first). A fresh key directory
    /// per area keeps signing self-contained.
    fn forklift(&self, dir: &str, args: &[&str]) {
        self.forklift_stdout(dir, args);
    }

    /// Run the CLI as [`Area::forklift`] does, and return what it printed.
    fn forklift_stdout(&self, dir: &str, args: &[&str]) -> String {
        let working = self.path(dir);
        std::fs::create_dir_all(&working).expect("create the working directory");

        let output = forklift_command()
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

        String::from_utf8_lossy(&output.stdout).to_string()
    }

    fn write_file(&self, relative: &str, content: &str) {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(path, content).expect("write file");
    }

    /// Write a large (chunk-threshold-crossing) file of deterministic, RNG-free bytes so it is
    /// stored chunked and chunks reproducibly.
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

/// The chunk threshold (bytes): content at or above this is stored chunked. Mirrors
/// `chunk_utils::CHUNK_THRESHOLD_BYTES` (a frozen format constant).
const CHUNK_THRESHOLD: usize = 8 * 1024 * 1024;

impl Drop for Area {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Everything a client would push, harvested from a built warehouse.
struct Harvest {
    objects: HashMap<String, Vec<u8>>,
    signatures: HashMap<String, Vec<u8>>,
    refs: Vec<(pallet_utils::PalletRef, String)>,
    trust: Option<TrustAnchorDto>,
}

impl Harvest {
    fn head_of(&self, wire: &str) -> Option<String> {
        self.refs
            .iter()
            .find(|(pallet_ref, _)| pallet_ref.to_wire() == wire)
            .map(|(_, head)| head.clone())
    }
}

/// Read every object, signature, ref and the trust anchor out of a built warehouse. Object
/// bytes come back in their uncompressed wire form (what the protocol carries).
fn harvest(warehouse: &Path) -> Harvest {
    // Enumerate object and signature files from the single-level fan-out object store.
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

    // Read them (and the refs/trust) under the warehouse's storage-root scope.
    let _scope = StorageRootScope::enter(warehouse);

    let mut objects = HashMap::new();
    for hash in object_hashes {
        let bytes = file_utils::retrieve_object_by_hash(&hash).expect("retrieve object");
        objects.insert(hash, bytes);
    }

    let mut signatures = HashMap::new();
    for hash in signature_hashes {
        let sidecar = sign_utils::load_raw_parcel_signature(&hash)
            .expect("load signature")
            .expect("signature present");
        signatures.insert(hash, sidecar);
    }

    let refs = pallet_utils::all_pallet_refs().expect("read refs");
    let trust = office_utils::read_trust_anchor()
        .expect("read trust")
        .map(|anchor| TrustAnchorDto::from(&anchor));

    Harvest { objects, signatures, refs, trust }
}

/// Configure an operator in a fresh warehouse dir (prepare + identity).
fn prepare(area: &Area, dir: &str) {
    area.forklift(dir, &["prepare"]);
    area.forklift(dir, &["config", "--global", "operator.identifier", "tester@forklift"]);
}

/// Upload every harvested object and signature to the head (the direct-store path
/// verifies each object's hash on the way in).
fn upload_all<O: forklift_aws_lambda::store::ObjectStore, R: forklift_aws_lambda::store::RefStore>(
    head: &Head<O, R>,
    harvest: &Harvest,
) {
    for (hash, bytes) in &harvest.objects {
        head.object_put(&Principal::Static, None, hash, bytes).expect("upload object");
    }
    for (hash, sidecar) in &harvest.signatures {
        head.signature_put(&Principal::Static, hash, sidecar).expect("upload signature");
    }
}

// ---------------------------------------------------------------------------------------
// The tests.
// ---------------------------------------------------------------------------------------

/// The untrusted path: CAS, closure presence and fast-forward, no crypto.
#[test]
fn untrusted_lift_and_the_cas_guards() {
    let area = Area::new("untrusted");
    prepare(&area, "wh");
    area.write_file("wh/readme.txt", "hello\n");
    area.write_file("wh/src/main.txt", "fn main\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "first"]);
    area.write_file("wh/readme.txt", "hello again\n");
    area.forklift("wh", &["load", "readme.txt"]);
    area.forklift("wh", &["stack", "second"]);

    let harvest = harvest(&area.path("wh"));
    let main_head = harvest.head_of("main").expect("main has a head");

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());

    // A fresh remote: empty handshake.
    let info = head.handshake().expect("handshake");
    assert!(info.pallets.is_empty());
    assert!(info.trust.is_none());
    assert_eq!(info.default_pallet, "main");

    // The negotiation names everything as missing.
    let all: Vec<String> = harvest.objects.keys().cloned().collect();
    let missing = head.missing(&all).expect("missing");
    assert_eq!(missing.len(), all.len());

    upload_all(&head, &harvest);

    // Now nothing is missing.
    assert!(head.missing(&all).expect("missing").is_empty());

    // The lift commits.
    let request = RefUpdateRequest { old_head: None, new_head: main_head.clone(), tombstone: None };
    head.ref_update(&Principal::Static, "main", &request).expect("lift main");

    // The handshake reflects it.
    let info = head.handshake().expect("handshake");
    assert_eq!(info.pallets.get("main"), Some(&main_head));

    // A replay with the same `old_head: None` now conflicts (the pallet exists).
    let err = head.ref_update(&Principal::Static, "main", &request).expect_err("stale replay");
    assert_eq!(err.status, Status::Conflict);
    // Framed, so the entrypoint threads the code onto the wire for the client to branch on.
    let (code, _, next_step) = forklift_core::error::deframe(&err.message).expect("a classified refusal");
    assert_eq!((code, next_step), ("remote_ahead", "forklift lower"), "{}", err.message);

    // A stale `old_head` conflicts too.
    let stale = RefUpdateRequest {
        old_head: Some("0".repeat(64)),
        new_head: main_head.clone(),
        tombstone: None,
    };
    assert_eq!(
        head.ref_update(&Principal::Static, "main", &stale).expect_err("stale old_head").status,
        Status::Conflict
    );
}

/// A holds walk on an invocation with less time left than the answer reserve reads nothing and
/// answers `unknown` for what the head alone does not settle, rather than run into the gateway.
#[test]
fn a_holds_walk_stops_short_of_the_invocation_deadline() {
    let area = Area::new("holds-deadline");
    prepare(&area, "wh");
    area.write_file("wh/readme.txt", "hello\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "first"]);
    area.write_file("wh/readme.txt", "hello again\n");
    area.forklift("wh", &["load", "readme.txt"]);
    area.forklift("wh", &["stack", "second"]);

    let harvest = harvest(&area.path("wh"));
    let main_head = harvest.head_of("main").expect("main has a head");
    let first = object_utils::parse_parcel_bytes(&main_head, &harvest.objects[&main_head])
        .expect("the head is a parcel")
        .parents
        .remove(0);

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &harvest);
    let request = RefUpdateRequest { old_head: None, new_head: main_head.clone(), tombstone: None };
    head.ref_update(&Principal::Static, "main", &request).expect("lift main");
    let candidates = [main_head.clone(), first.clone()];

    let holds = head.holds("main", &candidates).expect("holds");
    assert_eq!(holds[&first], HoldAnswer::Held, "the control: with time left, the walk reaches it");

    let head = head.with_deadline(std::time::SystemTime::now() + std::time::Duration::from_secs(1));
    head.objects.reset_reads();
    let holds = head.holds("main", &candidates).expect("holds");
    assert_eq!(holds[&main_head], HoldAnswer::Held, "the head needs no read");
    assert_eq!(holds[&first], HoldAnswer::Unknown, "no time was left to read toward it");
    assert_eq!(head.objects.reads(), 0);
}

/// A ref update whose closure is not fully uploaded is refused (`422`).
#[test]
fn a_ref_update_with_a_missing_blob_is_refused() {
    let area = Area::new("missing-blob");
    prepare(&area, "wh");
    area.write_file("wh/a.txt", "alpha\n");
    area.write_file("wh/b.txt", "beta\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "two files"]);

    let harvest = harvest(&area.path("wh"));
    let main_head = harvest.head_of("main").expect("main head");

    // Find a blob (a leaf file's object). Blobs are the objects that are neither a parcel
    // nor a tree; the simplest way to pick one deterministically is to drop the smallest
    // object that still leaves the parcel/tree readable — but we only need *some* object
    // absent to break the closure, so drop one arbitrary object and confirm a 422.
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());

    // Upload everything except one blob (an object that is not the head parcel).
    let mut skipped: Option<String> = None;
    for (hash, bytes) in &harvest.objects {
        if *hash != main_head && skipped.is_none() && is_probably_blob(&area.path("wh"), hash) {
            skipped = Some(hash.clone());
            continue;
        }
        head.object_put(&Principal::Static, None, hash, bytes).expect("upload object");
    }
    assert!(skipped.is_some(), "a blob was found to withhold");

    let request = RefUpdateRequest { old_head: None, new_head: main_head, tombstone: None };
    let err = head.ref_update(&Principal::Static, "main", &request).expect_err("incomplete closure");
    assert_eq!(err.status, Status::Unprocessable);
}

/// Classify an object as a blob by trying to parse it as a parcel/tree under the source
/// warehouse's scope; a blob is neither.
fn is_probably_blob(warehouse: &Path, hash: &str) -> bool {
    let _scope = StorageRootScope::enter(warehouse);
    object_utils::load_parcel(hash).is_err() && object_utils::load_tree(hash).is_err()
}

/// The commit-gate closure audit on the AWS head descends a chunked file's recipe and
/// presence-checks every chunk **non-tolerantly** (§9.4b W4), reading the recipe from the object
/// store (its recipes are never mirrored into the audit scratch). A ref update whose chunked file
/// is missing even one chunk — while the recipe itself is present, so a walk that stopped at the
/// recipe hash would wrongly pass — is refused (`422`). Uploading the last chunk lets the same
/// update commit, proving the check gates on exactly the chunk closure.
#[test]
fn a_ref_update_with_a_missing_chunk_is_refused() {
    let area = Area::new("missing-chunk");
    prepare(&area, "wh");
    area.write_large_file("wh/big.bin", 0xD00D, CHUNK_THRESHOLD + 50_000);
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "a giant"]);

    let harvest = harvest(&area.path("wh"));
    let main_head = harvest.head_of("main").expect("main head");

    // Resolve the chunked file's recipe and its chunk hashes from the source warehouse.
    let (recipe_hash, chunk_hashes) = {
        let _scope = StorageRootScope::enter(&area.path("wh"));
        let tree = object_utils::load_parcel(&main_head).expect("head parcel").tree_hash;
        let (recipe, item_type) = object_utils::resolve_tree_file(&tree, "big.bin")
            .expect("resolve")
            .expect("big.bin tracked");
        assert!(item_type.is_chunked(), "the giant is stored chunked");
        (recipe.clone(), object_utils::recipe_chunk_hashes(&recipe).expect("chunks"))
    };
    let victim = chunk_hashes[0].clone();

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());

    // Upload everything except one chunk. The recipe itself is uploaded.
    for (hash, bytes) in &harvest.objects {
        if *hash == victim {
            continue;
        }
        head.object_put(&Principal::Static, None, hash, bytes).expect("upload object");
    }
    for (hash, sidecar) in &harvest.signatures {
        head.signature_put(&Principal::Static, hash, sidecar).expect("upload signature");
    }
    assert!(head.object_get(&recipe_hash).is_ok(), "the recipe itself is present on the head");

    let request = RefUpdateRequest { old_head: None, new_head: main_head.clone(), tombstone: None };
    let err = head.ref_update(&Principal::Static, "main", &request).expect_err("a missing chunk fails the closure");
    assert_eq!(err.status, Status::Unprocessable);

    // The control: upload the withheld chunk, and the identical update now commits.
    head.object_put(&Principal::Static, None, &victim, &harvest.objects[&victim]).expect("upload the last chunk");
    // A chunk is served as an ordinary content-addressed object (`GET /v1/objects/{hash}`).
    assert!(head.object_get(&victim).is_ok(), "the head serves a chunk like any other object");
    head.ref_update(&Principal::Static, "main", &request).expect("the closure is complete once every chunk is present");
    assert_eq!(head.handshake().expect("handshake").pallets.get("main"), Some(&main_head));

    // The handshake advertises chunking, so a chunk-aware client knows it may lift here.
    assert!(head.handshake().expect("handshake").chunking, "the head advertises chunking");
}

/// A wrong-hash upload is rejected — nothing unverified enters the store.
#[test]
fn a_tampered_object_upload_is_rejected() {
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());

    let err = head
        .object_put(&Principal::Static, None, &"a".repeat(64), b"not the content of that hash")
        .expect_err("hash mismatch");
    assert_eq!(err.status, Status::Unprocessable);
}

/// The trusted path: a signed office chain plus a working pallet audit, all reused from
/// `forklift_core` through the scratch bridge.
#[test]
fn trusted_lift_audits_the_office_and_the_pallet() {
    let area = Area::new("trusted");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "signed one"]);
    area.write_file("wh/app.txt", "v2\n");
    area.forklift("wh", &["load", "app.txt"]);
    area.forklift("wh", &["stack", "signed two"]);

    let harvest = harvest(&area.path("wh"));
    let anchor = harvest.trust.clone().expect("trust established");
    let office_head = harvest.head_of(&format!("@{}", OFFICE_PALLET_NAME)).expect("office head");
    let main_head = harvest.head_of("main").expect("main head");

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &harvest);

    // A malformed anchor is refused before anything is stored: clients would refuse it at the
    // handshake, and a stored one could never be replaced.
    let mut malformed = anchor.clone();
    malformed.genesis = anchor.genesis.to_uppercase();
    assert_eq!(head.put_trust(&Principal::Static, &malformed).expect_err("malformed anchor").status, Status::Unprocessable);

    // Trust first, then the office pallet, then the working pallet — the client's order.
    assert_eq!(head.put_trust(&Principal::Static, &anchor).expect("put trust"), TrustResult::Established);
    // Idempotent.
    assert_eq!(head.put_trust(&Principal::Static, &anchor).expect("put trust again"), TrustResult::Unchanged);

    head.ref_update(&Principal::Static, &format!("@{}", OFFICE_PALLET_NAME), &RefUpdateRequest {
        old_head: None,
        new_head: office_head.clone(),
        tombstone: None,
    })
    .expect("lift office");

    head.ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: None, new_head: main_head.clone(), tombstone: None })
        .expect("lift main (audited)");

    let info = head.handshake().expect("handshake");
    assert_eq!(info.pallets.get("main"), Some(&main_head));
    assert_eq!(info.pallets.get(&format!("@{}", OFFICE_PALLET_NAME)), Some(&office_head));
    assert!(info.trust.is_some());
}

/// `@kv` lifts only under a lifted office, through the content check, incrementally from a
/// head it already holds; a meta pallet name the head does not know is refused outright.
#[test]
fn kv_lifts_under_the_office_and_unknown_meta_names_are_refused() {
    let area = Area::new("kv");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "signed"]);
    area.forklift("wh", &["kv", "set", "main", "ci", "green"]);
    let first = harvest(&area.path("wh")).head_of("@kv").expect("kv head");
    area.forklift("wh", &["kv", "set", "main", "review", "done"]);

    let harvest = harvest(&area.path("wh"));
    let second = harvest.head_of("@kv").expect("kv head");
    let office_head = harvest.head_of(&format!("@{}", OFFICE_PALLET_NAME)).expect("office head");

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &harvest);
    head.put_trust(&Principal::Static, &harvest.trust.clone().expect("trust")).expect("put trust");

    // Before the office: there are no keys to check the stamps against.
    let err = head
        .ref_update(&Principal::Static, "@kv", &RefUpdateRequest { old_head: None, new_head: first.clone(), tombstone: None })
        .expect_err("kv before the office");
    assert_eq!(err.status, Status::Unprocessable);

    head.ref_update(&Principal::Static, &format!("@{}", OFFICE_PALLET_NAME), &RefUpdateRequest {
        old_head: None,
        new_head: office_head,
        tombstone: None,
    })
    .expect("lift office");

    let err = head
        .ref_update(&Principal::Static, "@foo", &RefUpdateRequest { old_head: None, new_head: first.clone(), tombstone: None })
        .expect_err("an unknown meta pallet");
    assert_eq!(err.status, Status::Unprocessable);

    head.ref_update(&Principal::Static, "@kv", &RefUpdateRequest { old_head: None, new_head: first.clone(), tombstone: None })
        .expect("lift kv");
    head.ref_update(&Principal::Static, "@kv", &RefUpdateRequest { old_head: Some(first), new_head: second.clone(), tombstone: None })
        .expect("lift kv again, checked from the head it holds");
    assert_eq!(head.handshake().expect("handshake").pallets.get("@kv"), Some(&second));
}

/// `@haul` lifts through the content check: under the office the head holds, and never on a
/// warehouse without one, where a working pallet would lift unaudited.
#[test]
fn a_haul_lifts_only_under_the_office() {
    let area = Area::new("haul");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "signed"]);
    area.forklift("wh", &["palletize", "feature"]);
    area.write_file("wh/app.txt", "v2\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "feature"]);
    area.forklift("wh", &["haul", "open", "--target", "main", "--title", "Feature"]);

    let harvest = harvest(&area.path("wh"));
    let hauls = harvest.head_of("@haul").expect("haul head");
    let office_head = harvest.head_of(&format!("@{}", OFFICE_PALLET_NAME)).expect("office head");

    let untrusted = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&untrusted, &harvest);
    let err = untrusted
        .ref_update(&Principal::Static, "@haul", &RefUpdateRequest { old_head: None, new_head: hauls.clone(), tombstone: None })
        .expect_err("a haul on a warehouse without an office");
    assert_eq!(err.status, Status::Unprocessable);

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &harvest);
    head.put_trust(&Principal::Static, &harvest.trust.clone().expect("trust")).expect("put trust");
    head.ref_update(&Principal::Static, &format!("@{}", OFFICE_PALLET_NAME), &RefUpdateRequest { old_head: None, new_head: office_head, tombstone: None })
        .expect("lift office");
    head.ref_update(&Principal::Static, "@haul", &RefUpdateRequest { old_head: None, new_head: hauls.clone(), tombstone: None })
        .expect("lift haul under the office");
    assert_eq!(head.handshake().expect("handshake").pallets.get("@haul"), Some(&hauls));
}

/// A `@tags` lift is checked against the office the head holds as it admits it: a tag whose
/// tagger is an admin there is admitted, and the same tag under an office that has demoted the
/// tagger since is refused.
#[test]
fn a_tag_is_admitted_only_from_an_admin_of_the_held_office() {
    let area = Area::new("tags");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "signed"]);

    // A second identity in the same warehouse, admitted as an admin, tags a release.
    let worker = "worker@forklift";
    area.forklift("wh", &["config", "operator.identifier", worker]);
    let keygen = area.forklift_stdout("wh", &["office", "keygen"]);
    area.forklift("wh", &["config", "operator.identifier", "tester@forklift"]);
    let mut admit: Vec<&str> = keygen.lines()
        .find(|line| line.trim_start().starts_with("office admit "))
        .expect("keygen prints the admit line")
        .split_whitespace()
        .collect();
    admit.extend(["--role", "admin"]);
    area.forklift("wh", &admit);

    area.forklift("wh", &["config", "operator.identifier", worker]);
    area.forklift("wh", &["tag", "create", "v1.0", "main"]);
    area.forklift("wh", &["config", "operator.identifier", "tester@forklift"]);

    let before = harvest(&area.path("wh"));
    let office_wire = format!("@{}", OFFICE_PALLET_NAME);
    let admitting_office = before.head_of(&office_wire).expect("office head");
    let tags = before.head_of("@tags").expect("tags head");

    area.forklift("wh", &["office", "role", worker, "writer"]);
    let after = harvest(&area.path("wh"));
    let demoting_office = after.head_of(&office_wire).expect("office head");

    let lift = |office: &str| {
        let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
        upload_all(&head, &after);
        head.put_trust(&Principal::Static, &after.trust.clone().expect("trust")).expect("put trust");
        head.ref_update(&Principal::Static, &office_wire, &RefUpdateRequest { old_head: None, new_head: office.to_string(), tombstone: None })
            .expect("lift office");

        head.ref_update(&Principal::Static, "@tags", &RefUpdateRequest { old_head: None, new_head: tags.clone(), tombstone: None })
    };

    lift(&admitting_office).expect("a tag from an admin");

    let err = lift(&demoting_office).expect_err("a tag from a tagger the held office demoted");
    assert_eq!(err.status, Status::Unprocessable);
    assert!(err.message.contains("not an admin"), "{}", err.message);
}

/// A haul merge is admitted only from a writer of its target under the held office: the same
/// @haul head is admitted under the office that let its signer write `main`, and refused under the
/// office that ungranted it since.
#[test]
fn a_haul_merge_is_admitted_only_from_a_writer_of_its_target() {
    let area = Area::new("haul-merge");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "signed"]);
    area.forklift("wh", &["palletize", "feat"]);
    area.write_file("wh/feat.txt", "x\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "feature"]);
    area.forklift("wh", &["shift", "main"]);
    let opened = area.forklift_stdout("wh", &["--json", "haul", "open", "--target", "main", "--source", "feat", "--title", "Feat"]);
    let id = serde_json::from_str::<serde_json::Value>(&opened).unwrap()["data"]["id"].as_str().unwrap().to_string();

    let worker = "worker@forklift";
    area.forklift("wh", &["config", "operator.identifier", worker]);
    let keygen = area.forklift_stdout("wh", &["office", "keygen"]);
    area.forklift("wh", &["config", "operator.identifier", "tester@forklift"]);
    let admit: Vec<&str> = keygen.lines()
        .find(|line| line.trim_start().starts_with("office admit "))
        .expect("keygen prints the admit line")
        .split_whitespace()
        .collect();
    area.forklift("wh", &admit);

    area.forklift("wh", &["config", "operator.identifier", worker]);
    area.forklift("wh", &["haul", "merge", &id]);
    area.forklift("wh", &["config", "operator.identifier", "tester@forklift"]);

    let before = harvest(&area.path("wh"));
    let office_wire = format!("@{}", OFFICE_PALLET_NAME);
    let granting_office = before.head_of(&office_wire).expect("office head");
    let haul = before.head_of("@haul").expect("haul head");

    area.forklift("wh", &["office", "role", worker, "writer", "--pallet", "feat"]);
    let after = harvest(&area.path("wh"));
    let ungranting_office = after.head_of(&office_wire).expect("office head");

    let lift = |office: &str| {
        let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
        upload_all(&head, &after);
        head.put_trust(&Principal::Static, &after.trust.clone().expect("trust")).expect("put trust");
        head.ref_update(&Principal::Static, &office_wire, &RefUpdateRequest { old_head: None, new_head: office.to_string(), tombstone: None })
            .expect("lift office");

        head.ref_update(&Principal::Static, "@haul", &RefUpdateRequest { old_head: None, new_head: haul.clone(), tombstone: None })
    };

    lift(&granting_office).expect("a merge by a writer of the target");

    let err = lift(&ungranting_office).expect_err("a merge by a writer the held office ungranted");
    assert_eq!(err.status, Status::Unprocessable);
    assert!(err.message.contains("records a merge into main, but is signed by worker@forklift, who may not write it"), "{}", err.message);
}

/// FORK-220: an office parcel that oversteps its signer's privileges is a `403` on this head too,
/// now that the chain walk finds it: a writer promoting themselves to admin. The same promotion
/// signed by the admin is admitted, so the refusal is the signer's role and nothing else.
#[test]
fn a_writer_promoting_themselves_is_forbidden_and_an_admin_promoting_them_is_not() {
    let area = Area::new("self-promotion");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);

    let worker = "worker@forklift";
    area.forklift("wh", &["config", "operator.identifier", worker]);
    let keygen = area.forklift_stdout("wh", &["office", "keygen"]);
    area.forklift("wh", &["config", "operator.identifier", "tester@forklift"]);
    let mut admit: Vec<&str> = keygen.lines()
        .find(|line| line.trim_start().starts_with("office admit "))
        .expect("keygen prints the admit line")
        .split_whitespace()
        .collect();
    admit.extend(["--role", "writer"]);
    area.forklift("wh", &admit);

    // Sign both promotions in process, with the area's own keys. Nothing else in this binary
    // signs in process, and the CLI runs get the variable explicitly; the guard restores it
    // even if signing panics.
    struct KeysDirScope(Option<String>);

    impl Drop for KeysDirScope {
        fn drop(&mut self) {
            match self.0.take() {
                Some(previous) => std::env::set_var("FORKLIFT_KEYS_DIR", previous),
                None => std::env::remove_var("FORKLIFT_KEYS_DIR"),
            }
        }
    }

    let (office_head, by_worker, by_admin, demotion) = {
        let _keys = KeysDirScope(std::env::var("FORKLIFT_KEYS_DIR").ok());
        std::env::set_var("FORKLIFT_KEYS_DIR", area.path("keys"));
        let _scope = StorageRootScope::enter(&area.path("wh"));
        let office_head = pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME)
            .expect("read the office head")
            .expect("office head");

        let mut state = office_utils::read_office_state().expect("the office state");
        let root_of = |identifier: &str| state.find_user(identifier).expect("enrolled").identity_root.clone();
        let (worker_key, admin_key) = (root_of(worker), root_of("tester@forklift"));
        state.users.iter_mut().find(|user| user.identifier == worker).unwrap().role = office_utils::Role::Admin;

        let sign = |identifier: &str, key_id: &str| {
            let actor = forklift_core::model::operator::Operator {
                identifier: identifier.to_string(),
            };
            office_utils::build_and_sign_office_parcel(
                &state, &actor, "promote the worker".to_string(), key_id, vec![office_head.clone()]
            )
            .expect("sign the promotion")
        };

        let (by_worker, by_admin) = (sign(worker, &worker_key), sign("tester@forklift", &admin_key));

        // The admin demoting the worker again, on top of the worker's own promotion.
        let mut demoted = state.clone();
        demoted.users.iter_mut().find(|user| user.identifier == worker).unwrap().role = office_utils::Role::Writer;
        let admin = forklift_core::model::operator::Operator {
            identifier: "tester@forklift".to_string(),
        };
        let demotion = office_utils::build_and_sign_office_parcel(
            &demoted, &admin, "demote the worker".to_string(), &admin_key, vec![by_worker.clone()]
        )
        .expect("sign the demotion");

        (office_head.clone(), by_worker, by_admin, demotion)
    };

    let harvest = harvest(&area.path("wh"));
    let office_wire = format!("@{}", OFFICE_PALLET_NAME);

    let lift = |promotion: &str| {
        let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
        upload_all(&head, &harvest);
        head.put_trust(&Principal::Static, &harvest.trust.clone().expect("trust")).expect("put trust");
        head.ref_update(&Principal::Static, &office_wire, &RefUpdateRequest { old_head: None, new_head: office_head.clone(), tombstone: None })
            .expect("lift the admitted office");

        head.ref_update(&Principal::Static, &office_wire, &RefUpdateRequest {
            old_head: Some(office_head.clone()),
            new_head: promotion.to_string(),
            tombstone: None,
        })
    };

    let err = lift(&by_worker).expect_err("a writer's self-promotion");
    assert_eq!(err.status, Status::Forbidden, "{}", err.message);
    assert!(err.message.contains("not an admin"), "{}", err.message);

    lift(&by_admin).expect("an admin's promotion");

    // The worker's promotion planted past the head, as a store written before the check would
    // hold it: the admin's legitimate demotion on top is refused, but the overstep is not the
    // admin's, so it is a 422 and not a 403.
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &harvest);
    head.put_trust(&Principal::Static, &harvest.trust.clone().expect("trust")).expect("put trust");
    head.ref_update(&Principal::Static, &office_wire, &RefUpdateRequest { old_head: None, new_head: office_head.clone(), tombstone: None })
        .expect("lift the admitted office");
    let trust = head.refs.get_trust().expect("read trust");
    assert!(matches!(
        head.refs.compare_and_set_head(
            pallet_utils::PalletNamespace::Meta,
            OFFICE_PALLET_NAME,
            ExpectedHead::At(&office_head),
            &by_worker,
            OfficePrecondition::NotConsumed,
            trust.as_ref().map(|(_, raw)| raw.as_str()),
            &request_token(),
        ),
        Ok(CasOutcome::Committed)
    ));

    let err = head.ref_update(&Principal::Static, &office_wire, &RefUpdateRequest {
        old_head: Some(by_worker.clone()),
        new_head: demotion,
        tombstone: None,
    })
    .expect_err("a demotion on top of a held overstep");
    assert_eq!(err.status, Status::Unprocessable, "{}", err.message);
    assert!(err.message.contains(&by_worker), "{}", err.message);
    assert!(err.message.contains("already in this warehouse's office"), "{}", err.message);
}

/// On a trusted warehouse, a user-pallet lift before the office is lifted is refused: the
/// audit has no keys to verify against.
#[test]
fn a_user_lift_before_the_office_is_refused() {
    let area = Area::new("office-first");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "signed"]);

    let harvest = harvest(&area.path("wh"));
    let anchor = harvest.trust.clone().expect("trust");
    let main_head = harvest.head_of("main").expect("main head");

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &harvest);
    head.put_trust(&Principal::Static, &anchor).expect("put trust");

    // Skipping the office lift: main's audit finds no office pallet.
    let err = head
        .ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: None, new_head: main_head, tombstone: None })
        .expect_err("office missing");
    assert_eq!(err.status, Status::Unprocessable);
}

/// An office is never lifted without an anchor, and an anchor is never planted over an office
/// that has lost its own: either state reads to a client as a stripped anchor, and the first
/// client to lift would otherwise plant an anchor over a chain it never verified.
#[test]
fn an_office_and_an_anchor_never_exist_apart() {
    let area = Area::new("office-anchor-pair");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "signed"]);

    let harvest = harvest(&area.path("wh"));
    let anchor = harvest.trust.clone().expect("trust");
    let office_head = harvest.head_of(&format!("@{}", OFFICE_PALLET_NAME)).expect("office head");

    let refs = Arc::new(MemoryRefStore::new());
    let head = Head::new(MemoryObjectStore::new(), SharedMemoryRefs(refs.clone()));
    upload_all(&head, &harvest);

    // Lifting the office before the anchor is refused, and leaves the office unborn.
    let err = head
        .ref_update(&Principal::Static, &format!("@{}", OFFICE_PALLET_NAME), &RefUpdateRequest {
            old_head: None,
            new_head: office_head.clone(),
            tombstone: None,
        })
        .expect_err("office without an anchor");
    assert_eq!(err.status, Status::Unprocessable);
    assert_eq!(refs.get_head(pallet_utils::PalletNamespace::Meta, OFFICE_PALLET_NAME), Ok(RefHead::Absent));

    // An office whose anchor was lost — planted beneath the head, as a stripped store would
    // hold it. Planting an anchor over it is refused, and nothing is stored.
    assert!(matches!(
        refs.compare_and_set_head(
            pallet_utils::PalletNamespace::Meta,
            OFFICE_PALLET_NAME,
            ExpectedHead::Unborn,
            &office_head,
            OfficePrecondition::NotConsumed,
            None,
            &request_token(),
        ),
        Ok(CasOutcome::Committed)
    ));
    let err = head.put_trust(&Principal::Static, &anchor).expect_err("anchor over an orphaned office");
    assert_eq!(err.status, Status::Conflict);
    assert!(refs.get_trust().expect("read trust").is_none());
}

/// A signature sidecar is immutable: a conflicting re-upload is a `409`.
#[test]
fn a_conflicting_signature_is_refused() {
    let area = Area::new("sig-immutable");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "signed"]);

    let harvest = harvest(&area.path("wh"));
    let (parcel, sidecar) = harvest.signatures.iter().next().expect("a signed parcel");

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());

    assert_eq!(head.signature_put(&Principal::Static, parcel, sidecar).expect("store"), SignatureOutcome::Created);
    // Identical re-store: idempotent.
    assert_eq!(
        head.signature_put(&Principal::Static, parcel, sidecar).expect("re-store"),
        SignatureOutcome::AlreadyPresent
    );

    // A different (but still structurally valid) sidecar for the same parcel: conflict.
    // Reuse another parcel's sidecar bytes if there is one; otherwise mutate is impossible
    // without a valid signature, so only assert immutability when a second sidecar exists.
    if let Some((_, other)) = harvest.signatures.iter().find(|(hash, _)| *hash != parcel) {
        let err = head.signature_put(&Principal::Static, parcel, other).expect_err("conflict");
        assert_eq!(err.status, Status::Conflict);
    }
}

/// The presigned byte plane: with a staging store, object reads answer with a `307` to the
/// canonical key, while uploads are redirected to a **staging** key — never to the hash key
/// the reads serve. A session-less upload has nowhere to stage and is refused.
#[test]
fn a_staging_store_redirects_uploads_to_a_session_staging_key() {
    let store = MemoryObjectStore::with_redirect("https://s3.example/bucket");

    // Seed one object as if it were already promoted into S3.
    let bytes = b"an object".to_vec();
    let hash = object_utils::hash_object_bytes(&bytes);
    store.put_verified(&hash, &bytes).expect("seed a canonical object");

    let head = Head::new(store, MemoryRefStore::new());

    match head.object_get(&hash).expect("get") {
        ObjectReadResult::Redirect(url) => {
            assert_eq!(url, format!("https://s3.example/bucket/objects/{}", hash))
        }
        ObjectReadResult::Bytes(_) => panic!("expected a redirect"),
    }

    // The upload target is under the session's staging prefix, not `objects/{hash}`.
    match head.object_put(&Principal::Static, Some("lift-1"), &hash, b"ignored").expect("put") {
        ObjectWriteResult::Redirect(url) => {
            assert_eq!(url, format!("https://s3.example/bucket/staging/lift-1/{}", hash));
            assert!(!url.contains("/objects/"), "an upload must never target the hash key");
        }
        ObjectWriteResult::Stored { .. } => panic!("expected a redirect"),
    }

    // Without a session there is nowhere to stage, so the head refuses rather than
    // handing out a presigned PUT to the canonical key.
    let err = head.object_put(&Principal::Static, None, &hash, b"ignored").expect_err("session-less upload");
    assert_eq!(err.status, Status::Unprocessable);
}

/// Invariant 1 on the presigned path: bytes a client `PUT`s straight to the staging prefix
/// are **not fetchable at their hash key** until `commit_lift` verifies and promotes them,
/// and a corrupt staged object is discarded rather than promoted.
#[test]
fn a_staged_object_is_not_fetchable_until_it_is_verified_and_promoted() {
    let store = MemoryObjectStore::with_redirect("https://s3.example/bucket");

    let good = b"a good control-plane object".to_vec();
    let good_hash = object_utils::hash_object_bytes(&good);

    // Bytes that do NOT match the hash they are staged under — a client uploading garbage
    // to a presigned URL, the case the promote step must catch.
    let corrupt_hash = object_utils::hash_object_bytes(b"the declared content");

    store.stage("lift-1", &good_hash, good);
    store.stage("lift-1", &corrupt_hash, b"tampered content".to_vec());

    let head = Head::new(store, MemoryRefStore::new());

    // Neither is fetchable while it is merely staged: this is the invariant the old
    // canonical-key upload broke.
    for hash in [&good_hash, &corrupt_hash] {
        let err = head.object_get(hash).expect_err("a staged object is not fetchable");
        assert_eq!(err.status, Status::NotFound);
    }

    // A commit naming the corrupt object is refused...
    let err = head
        .commit_lift(&Principal::Static, "lift-1", &[good_hash.clone(), corrupt_hash.clone()], &[], false)
        .expect_err("corrupt control-plane object");
    assert_eq!(err.status, Status::Unprocessable);
    assert_eq!(refusal_code(&err.message), Some(RefusalCode::StagedUploadDiscarded), "{}", err.message);

    // ...and the corrupt bytes are gone, never having reached the hash key.
    let err = head.object_get(&corrupt_hash).expect_err("corrupt bytes were discarded");
    assert_eq!(err.status, Status::NotFound);

    // A commit over only the good object promotes it: now — and only now — it is fetchable.
    head.commit_lift(&Principal::Static, "lift-1", std::slice::from_ref(&good_hash), &[], false).expect("clean commit");

    match head.object_get(&good_hash).expect("the promoted object") {
        ObjectReadResult::Redirect(url) => {
            assert_eq!(url, format!("https://s3.example/bucket/objects/{}", good_hash))
        }
        ObjectReadResult::Bytes(_) => panic!("expected a redirect"),
    }

    // The commit swept the session's staging prefix, and promotion is idempotent.
    assert_eq!(head.objects.staged_count(), 0, "staging is swept after a commit");
    head.commit_lift(&Principal::Static, "lift-1", std::slice::from_ref(&good_hash), &[], false).expect("retried commit");

    // A commit naming an object that was never staged is refused the same way: its upload is
    // not on the remote, and only running the lift again can put it there.
    let err = head.commit_lift(&Principal::Static, "lift-1", &["f".repeat(64)], &[], false).expect_err("missing object");
    assert_eq!(err.status, Status::Unprocessable);
    assert_eq!(refusal_code(&err.message), Some(RefusalCode::StagedUploadDiscarded), "{}", err.message);
}

/// The discarded-upload refusal's framed code, read back the way the client reads it.
fn refusal_code(message: &str) -> Option<RefusalCode> {
    match CoreError::from(message.to_string()) {
        CoreError::Refusal { code, .. } => Some(code),
        CoreError::Other(_) => None,
    }
}

/// A blob is presence-checked at its *canonical* key, which is the proof the staging
/// verifier already hash-checked it: a blob still in staging is not ready yet, which the head
/// answers with the retryable not-ready `503`, never a terminal refusal.
#[test]
fn a_blob_still_in_staging_is_not_ready_to_commit() {
    let store = MemoryObjectStore::with_redirect("https://s3.example/bucket");

    let blob = b"a large working blob".to_vec();
    let blob_hash = object_utils::hash_object_bytes(&blob);
    store.stage("lift-1", &blob_hash, blob);

    let head = Head::new(store, MemoryRefStore::new());

    let err = head
        .commit_lift(&Principal::Static, "lift-1", &[], std::slice::from_ref(&blob_hash), false)
        .expect_err("unpromoted blob");
    assert_eq!(err.status, Status::ServiceUnavailable);
    assert!(err.commit_not_ready, "the adapter answers it with the not-ready header");
    assert!(err.message.contains(&blob_hash), "{}", err.message);

    // The staging verifier promotes it out of band — the same trait operation the control
    // plane uses for small objects — and the commit then succeeds.
    let outcome = head.objects.verify_and_promote("lift-1", &blob_hash).expect("promote");
    assert_eq!(outcome, PromoteOutcome::Promoted);

    head.commit_lift(&Principal::Static, "lift-1", &[], &[blob_hash], false).expect("the blob is verified and present");
}

/// A blob in neither place — its upload discarded by the verifier, or never sent — can never
/// commit, and the head says so with the framed discarded code rather than "not ready".
#[test]
fn a_blob_in_neither_place_is_refused_as_discarded() {
    let head = Head::new(MemoryObjectStore::with_redirect("https://s3.example/bucket"), MemoryRefStore::new());
    let blob_hash = object_utils::hash_object_bytes(b"never uploaded");

    let err = head
        .commit_lift(&Principal::Static, "lift-1", &[], std::slice::from_ref(&blob_hash), false)
        .expect_err("absent blob");

    assert_eq!(err.status, Status::Unprocessable);
    assert!(!err.commit_not_ready, "a discarded upload is terminal");
    assert_eq!(refusal_code(&err.message), Some(RefusalCode::StagedUploadDiscarded), "{}", err.message);
    assert!(err.message.contains(&blob_hash), "{}", err.message);
}

/// A staged blob whose bytes the verifier rejects goes from not-ready to discarded, and the lift
/// run again — uploading it afresh — commits.
#[test]
fn a_corrupt_staged_blob_turns_not_ready_into_discarded_and_a_fresh_upload_commits() {
    let store = MemoryObjectStore::with_redirect("https://s3.example/bucket");
    let blob = b"the declared blob".to_vec();
    let blob_hash = object_utils::hash_object_bytes(&blob);
    store.stage("lift-1", &blob_hash, b"bytes that are not it".to_vec());
    let head = Head::new(store, MemoryRefStore::new());
    let commit = || head.commit_lift(&Principal::Static, "lift-1", &[], std::slice::from_ref(&blob_hash), false);

    assert_eq!(commit().expect_err("still staged").status, Status::ServiceUnavailable);

    let outcome = head.objects.verify_and_promote("lift-1", &blob_hash).expect("verify");
    assert!(matches!(outcome, PromoteOutcome::Corrupt { .. }), "{:?}", outcome);
    let err = commit().expect_err("the upload was discarded");
    assert_eq!(err.status, Status::Unprocessable);
    assert_eq!(refusal_code(&err.message), Some(RefusalCode::StagedUploadDiscarded), "{}", err.message);

    head.objects.stage("lift-2", &blob_hash, blob);
    head.objects.verify_and_promote("lift-2", &blob_hash).expect("verify the fresh upload");
    head.commit_lift(&Principal::Static, "lift-2", &[], &[blob_hash], false).expect("the fresh upload commits");
}

/// A store whose staged-upload probe sees the verifier promote every blob it is asked about just
/// before it answers, in the order the verifier writes: canonical copy first, staged copy dropped
/// after. A blob absent at canonical and no longer staged may have been promoted in between.
struct PromotesDuringTheStagedProbe {
    inner: MemoryObjectStore,
}

impl ObjectStore for PromotesDuringTheStagedProbe {
    fn exists(&self, hash: &str) -> Result<bool, String> {
        self.inner.exists(hash)
    }

    fn objects_staged(&self, session: &str, hashes: &[String]) -> Result<Vec<String>, String> {
        for hash in hashes {
            self.inner.verify_and_promote(session, hash)?;
        }
        self.inner.objects_staged(session, hashes)
    }

    fn get(&self, hash: &str) -> Result<Option<Vec<u8>>, String> {
        self.inner.get(hash)
    }

    fn put_verified(&self, hash: &str, bytes: &[u8]) -> Result<PutOutcome, String> {
        self.inner.put_verified(hash, bytes)
    }

    fn get_signature(&self, parcel_hash: &str) -> Result<Option<Vec<u8>>, String> {
        self.inner.get_signature(parcel_hash)
    }

    fn put_signature(&self, parcel_hash: &str, bytes: &[u8]) -> Result<SignatureOutcome, String> {
        self.inner.put_signature(parcel_hash, bytes)
    }
}

/// A blob promoted between the canonical probe and the staged probe is found by the canonical
/// re-probe: the commit succeeds, and is never refused as discarded.
#[test]
fn a_blob_promoted_between_the_probes_commits() {
    let inner = MemoryObjectStore::with_redirect("https://s3.example/bucket");
    let blob = b"a blob the verifier is about to promote".to_vec();
    let blob_hash = object_utils::hash_object_bytes(&blob);
    inner.stage("lift-1", &blob_hash, blob);
    let head = Head::new(PromotesDuringTheStagedProbe { inner }, MemoryRefStore::new());

    head.commit_lift(&Principal::Static, "lift-1", &[], &[blob_hash], false).expect("promoted mid-check");
}

/// The commit-pagination sweep-ordering fix (§9.4b Stage 3): an intermediate commit batch
/// (`more: true`) verifies and presence-checks its own slice but **must not** sweep the session's
/// staging prefix, or it would discard a later batch's chunks that are still staged (not yet
/// promoted). Only the final batch (`more: false`) sweeps. The orphan here stands in for exactly
/// such a later-batch chunk: it survives the intermediate batch and is swept only by the final one.
#[test]
fn an_intermediate_commit_batch_does_not_sweep_a_later_batchs_staged_objects() {
    let store = MemoryObjectStore::with_redirect("https://s3.example/bucket");

    let committed = b"a blob in this batch".to_vec();
    let committed_hash = object_utils::hash_object_bytes(&committed);
    let orphan = b"a chunk for a later batch, still staging".to_vec();
    let orphan_hash = object_utils::hash_object_bytes(&orphan);

    store.stage("lift-1", &committed_hash, committed);
    store.stage("lift-1", &orphan_hash, orphan);

    // This batch's blob is promoted (present at canonical); the orphan is not — it is a later
    // batch's still-staged object.
    store.verify_and_promote("lift-1", &committed_hash).expect("promote this batch's blob");
    assert_eq!(store.staged_count(), 1, "only the orphan remains staged");

    let head = Head::new(store, MemoryRefStore::new());

    // Intermediate batch: presence-check the promoted blob, but do NOT sweep — the orphan survives.
    head.commit_lift(&Principal::Static, "lift-1", &[], std::slice::from_ref(&committed_hash), true)
        .expect("the intermediate batch commits");
    assert_eq!(
        head.objects.staged_count(),
        1,
        "an intermediate (more) batch never sweeps: the later batch's staged object survives"
    );

    // Final batch (idempotent presence-check of the same blob): now the session is swept.
    head.commit_lift(&Principal::Static, "lift-1", &[], std::slice::from_ref(&committed_hash), false)
        .expect("the final batch commits");
    assert_eq!(
        head.objects.staged_count(),
        0,
        "the final (more: false) batch sweeps the whole session's staging prefix"
    );
}

/// The control plane and the staging verifier can promote the same hash at the same time.
/// Exactly one wins; the loser sees the canonical object rather than a spurious "missing",
/// so a lift never fails because the other promoter got there first.
#[test]
fn racing_promoters_serialize_and_never_report_missing() {
    let store = MemoryObjectStore::with_redirect("https://s3.example/bucket");

    let bytes = b"an object two promoters both want".to_vec();
    let hash = object_utils::hash_object_bytes(&bytes);
    store.stage("lift-1", &hash, bytes);

    let barrier = std::sync::Barrier::new(2);
    let (store, barrier, hash) = (&store, &barrier, &hash);

    let outcomes: Vec<PromoteOutcome> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(move || {
                    barrier.wait();
                    store.verify_and_promote("lift-1", hash).expect("promote")
                })
            })
            .collect();

        handles.into_iter().map(|handle| handle.join().expect("promoter thread")).collect()
    });

    assert_eq!(outcomes.iter().filter(|o| **o == PromoteOutcome::Promoted).count(), 1);
    assert_eq!(outcomes.iter().filter(|o| **o == PromoteOutcome::AlreadyPresent).count(), 1);
    assert!(!outcomes.contains(&PromoteOutcome::Missing), "the loser must not see 'missing'");
    assert_eq!(store.object_count(), 1);
    assert_eq!(store.staged_count(), 0);
}

/// Build a warehouse of `dirs` directories, then `touches` parcels each rewriting the same
/// file. Every touch supersedes two trees (the root and `d0`), so the history accumulates
/// tree versions that only an unbounded mirror would ever fetch. The head's own tree closure
/// stays the same size no matter how many touches came before.
fn layered_warehouse(area: &Area, dirs: usize, touches: usize) {
    prepare(area, "wh");

    for dir in 0..dirs {
        area.write_file(&format!("wh/d{}/f.txt", dir), "v0\n");
    }

    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "create"]);

    for touch in 0..touches {
        area.write_file("wh/d0/f.txt", &format!("v{}\n", touch + 1));
        area.forklift("wh", &["load", "."]);
        area.forklift("wh", &["stack", &format!("touch {}", touch)]);
    }
}

/// Audit one more parcel on top of a `touches`-long history, both ways, and report how many
/// object bodies each mirror pulled from the store: `(bounded, unbounded)`. The two differ
/// only in `old_head` — same graph, same objects.
fn mirror_reads(touches: usize) -> (usize, usize) {
    let area = Area::new("bounded-mirror");
    layered_warehouse(&area, 4, touches);

    let old_head = harvest(&area.path("wh")).head_of("main").expect("main head");

    // The segment the incremental update actually audits.
    area.write_file("wh/d0/f.txt", "the new segment\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "the new segment"]);

    let latest = harvest(&area.path("wh"));
    let new_head = latest.head_of("main").expect("main head");
    assert_ne!(old_head, new_head);

    // Bounded: the pallet already sits at `old_head`, so the audit stops expanding there.
    let bounded = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&bounded, &latest);
    bounded
        .ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: None, new_head: old_head.clone(), tombstone: None })
        .expect("establish the old head");
    bounded.objects.reset_reads();
    bounded
        .ref_update(
            &Principal::Static,
            "main",
            &RefUpdateRequest { old_head: Some(old_head), new_head: new_head.clone(), tombstone: None },
        )
        .expect("the bounded ref update still audits clean");

    // Unbounded: the same head parcel audited as a creation — the whole history expands.
    let unbounded = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&unbounded, &latest);
    unbounded.objects.reset_reads();
    unbounded
        .ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: None, new_head, tombstone: None })
        .expect("the unbounded ref update audits clean");

    (bounded.objects.reads(), unbounded.objects.reads())
}

/// The ref-update mirror is bounded at `old_head` — in the dimension that costs.
///
/// Below the bound it still reads one parcel *body* apiece (`collect_reachable` walks
/// `old_head`'s ancestry to build the closure check's prune set), but **no trees**. So
/// lengthening the history by `k` parcels costs a bounded mirror exactly `k` more reads,
/// while an unbounded one also re-fetches every superseded tree version.
#[test]
fn the_ref_update_mirror_is_bounded_at_old_head() {
    let extra = 4;
    let (bounded_short, unbounded_short) = mirror_reads(2);
    let (bounded_long, unbounded_long) = mirror_reads(2 + extra);

    assert!(bounded_short < unbounded_short, "the bound saves reads even on a short history");

    assert_eq!(
        bounded_long - bounded_short,
        extra,
        "a bounded mirror pays exactly one parcel body per extra parcel of history and no \
         trees ({} vs {} reads)",
        bounded_long,
        bounded_short
    );

    assert!(
        unbounded_long - unbounded_short > extra,
        "an unbounded mirror also re-reads every superseded tree ({} vs {} reads)",
        unbounded_long,
        unbounded_short
    );
}

/// The sidecar bound is the subtle half of that guarantee: `verify_pallet_history` never traverses
/// *through* `old_head`, so a merge lift whose new segment forks below it must re-expand
/// that older branch — signatures and all — or a trusted audit would see unsigned parcels.
#[test]
fn a_trusted_merge_lift_below_the_bound_still_audits() {
    let area = Area::new("merge-bound");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);

    area.write_file("wh/app.txt", "base\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "base"]);

    // A branch forking at `base` — below the bound the lift will later carry.
    area.forklift("wh", &["palletize", "feature"]);
    area.write_file("wh/feature.txt", "from the branch\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "on the branch"]);

    // main moves on, and that head becomes the remote's `old_head`.
    area.forklift("wh", &["shift", "main"]);
    area.write_file("wh/app.txt", "moved on\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "on main"]);

    let before = harvest(&area.path("wh"));
    let old_head = before.head_of("main").expect("main head");
    let office_head = before.head_of("@office").expect("office head");

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &before);
    head.put_trust(&Principal::Static, before.trust.as_ref().expect("trust")).expect("plant trust");
    head.ref_update(&Principal::Static, "@office", &RefUpdateRequest { old_head: None, new_head: office_head, tombstone: None })
        .expect("lift the office");
    head.ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: None, new_head: old_head.clone(), tombstone: None })
        .expect("establish the old head");

    // The merge parcel: its second parent is the branch tip, whose ancestry forks below
    // `old_head`. The audit walks into it; the mirror must follow.
    area.forklift("wh", &["consolidate", "feature"]);

    let after = harvest(&area.path("wh"));
    let new_head = after.head_of("main").expect("merged main head");
    assert_ne!(old_head, new_head);

    // Guard the point of the test: a fast-forward would never walk below the bound.
    let parents = {
        let _scope = StorageRootScope::enter(&area.path("wh"));
        object_utils::load_parcel(&new_head).expect("the merge parcel").parents
    };
    assert_eq!(parents.len(), 2, "consolidate stacked a real merge parcel");

    upload_all(&head, &after);
    head.ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: Some(old_head), new_head, tombstone: None })
        .expect("a merge lift across the bound audits clean");
}

/// The warm-container scratch: a second ref update against the same warehouse finds the
/// history already mirrored and re-reads almost nothing from the object store. The pool is
/// keyed by warehouse, because scratch presence is read as store presence.
#[test]
fn a_pooled_scratch_amortizes_the_mirror_and_is_keyed_by_warehouse() {
    // Unique per run: a shared scratch is keyed by warehouse alone, so a directory left in
    // /tmp by an earlier run would silently pre-warm the "cold" measurement below.
    let warehouse = format!("pooled-{}-{}", std::process::id(), unique_suffix());

    let alpha = Scratch::shared(&warehouse).expect("shared scratch");
    let again = Scratch::shared(&warehouse).expect("shared scratch");
    let beta = Scratch::shared(&format!("{}-other", warehouse)).expect("shared scratch");

    assert_eq!(alpha.root(), again.root(), "one scratch per warehouse, reused");
    assert_ne!(alpha.root(), beta.root(), "never shared across warehouses");

    let area = Area::new("pooled-scratch");
    layered_warehouse(&area, 4, 3);

    let old_head =
        harvest(&area.path("wh")).head_of("main").expect("main head");

    area.write_file("wh/d0/f.txt", "v2\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "the new segment"]);

    let latest = harvest(&area.path("wh"));
    let new_head = latest.head_of("main").expect("main head");

    // A warehouse id unique to this test, so the process-global pool stays isolated.
    let head = Head::pooled(
        MemoryObjectStore::new(),
        MemoryRefStore::new(),
        &warehouse,
        std::time::SystemTime::now() + std::time::Duration::from_secs(600),
    );
    upload_all(&head, &latest);

    head.objects.reset_reads();
    head.ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: None, new_head: old_head.clone(), tombstone: None })
        .expect("cold ref update");
    let cold = head.objects.reads();

    head.objects.reset_reads();
    head.ref_update(
        &Principal::Static,
        "main",
        &RefUpdateRequest { old_head: Some(old_head), new_head: new_head.clone(), tombstone: None },
    )
    .expect("warm ref update");
    let warm = head.objects.reads();

    assert!(cold > 0, "the cold mirror reads the history");
    assert!(
        warm * 3 < cold,
        "a warm scratch re-reads almost nothing: {} warm vs {} cold",
        warm,
        cold
    );

    // And it is still correct: the head moved.
    assert_eq!(
        head.refs.get_head(pallet_utils::PalletNamespace::User, "main").unwrap().live(),
        Some(new_head.as_str())
    );

    // A pooled scratch outlives the request by design; this test owns these two, so it
    // leaves no directories behind.
    let _ = std::fs::remove_dir_all(alpha.root());
    let _ = std::fs::remove_dir_all(beta.root());
}

/// A monotonically increasing suffix, so a scratch key is unique to this run.
fn unique_suffix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after 1970")
        .as_nanos() as u64
}

/// `batch` returns a bundle-format stream the negotiation can consume, and the round trip
/// of `missing` is exact.
#[test]
fn batch_returns_a_bundle_stream() {
    let area = Area::new("batch");
    prepare(&area, "wh");
    area.write_file("wh/a.txt", "alpha\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "one"]);

    let harvest = harvest(&area.path("wh"));
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &harvest);

    let hashes: Vec<String> = harvest.objects.keys().cloned().collect();

    match head.batch(&hashes).expect("batch") {
        BatchResult::Bundle(bundle) => {
            assert!(!bundle.is_empty(), "the batch produced a non-empty bundle stream")
        }
        BatchResult::Redirect(_) => panic!("a direct store serves the bundle inline"),
    }

    // Nothing is missing after the upload.
    assert!(head.missing(&hashes).expect("missing").is_empty());
}

/// A store that can offload keeps the bundle out of the control plane: `batch` answers with
/// a presigned `GET` whose bytes are exactly the bundle the direct head would have streamed.
#[test]
fn batch_offloads_the_bundle_to_a_presigned_url() {
    let area = Area::new("batch-offload");
    prepare(&area, "wh");
    area.write_file("wh/a.txt", "alpha\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "one"]);

    let harvest = harvest(&area.path("wh"));
    let hashes: Vec<String> = harvest.objects.keys().cloned().collect();

    // The same warehouse behind a direct head and a staging head.
    let direct = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&direct, &harvest);

    let staging = Head::new(
        MemoryObjectStore::with_redirect("https://s3.example/bucket"),
        MemoryRefStore::new(),
    );
    for (hash, bytes) in &harvest.objects {
        staging.objects.put_verified(hash, bytes).expect("seed the staging store");
    }

    let inline = match direct.batch(&hashes).expect("direct batch") {
        BatchResult::Bundle(bundle) => bundle,
        BatchResult::Redirect(_) => panic!("a direct store serves the bundle inline"),
    };

    match staging.batch(&hashes).expect("offloaded batch") {
        BatchResult::Redirect(url) => {
            assert!(url.starts_with("https://s3.example/bucket/responses/"));
            assert!(!url.contains("/objects/"), "a response body is never an object");

            let served = staging.objects.offloaded_response(&url).expect("the presigned bytes");
            assert_eq!(served, inline, "the offloaded bundle is the bundle");
        }
        BatchResult::Bundle(_) => panic!("an offloading store hands out a presigned GET"),
    }
}

/// The body-less upload negotiation: one round trip sorts the hashes into already-present,
/// upload-straight-to-storage, and send-through-the-control-plane — without a single body.
#[test]
fn upload_targets_negotiates_without_sending_bodies() {
    let present = b"an object the remote already has".to_vec();
    let present_hash = object_utils::hash_object_bytes(&present);
    let wanted_hash = object_utils::hash_object_bytes(b"an object it does not");

    // A staging head: the missing object gets a presigned staging URL.
    let store = MemoryObjectStore::with_redirect("https://s3.example/bucket");
    store.put_verified(&present_hash, &present).expect("seed");
    let staging = Head::new(store, MemoryRefStore::new());

    let answer = staging
        .upload_targets(&Principal::Static, "lift-1", &[present_hash.clone(), wanted_hash.clone(), present_hash.clone()])
        .expect("negotiate");

    assert_eq!(answer.present, vec![present_hash.clone()], "duplicates collapse");
    assert!(answer.direct.is_empty());
    assert_eq!(
        answer.targets.get(&wanted_hash).map(String::as_str),
        Some(format!("https://s3.example/bucket/staging/lift-1/{}", wanted_hash).as_str())
    );

    // `present` is exactly the complement of `missing`, so this subsumes that call.
    assert_eq!(staging.missing(&[present_hash.clone(), wanted_hash.clone()]).unwrap(), vec![
        wanted_hash.clone()
    ]);

    // A direct head: the same request routes the missing object through the control plane,
    // so one client code path serves both heads.
    let store = MemoryObjectStore::new();
    store.put_verified(&present_hash, &present).expect("seed");
    let direct = Head::new(store, MemoryRefStore::new());

    let answer = direct
        .upload_targets(&Principal::Static, "lift-1", &[present_hash.clone(), wanted_hash.clone()])
        .expect("negotiate");

    assert_eq!(answer.present, vec![present_hash]);
    assert_eq!(answer.direct, vec![wanted_hash]);
    assert!(answer.targets.is_empty());
}

/// Content-addressed (hash, bytes) pairs for a synthetic batch — cheap to generate in the
/// hundreds, unlike real (up to 4 MiB) chunks, and exactly what the bulk presence seam only ever
/// sees on the hash side: the bytes are kept alongside only so a test can seed `put_verified`
/// with content that actually hashes to the claimed key.
fn synthetic_objects(count: usize, seed: &str) -> Vec<(String, Vec<u8>)> {
    (0..count)
        .map(|i| {
            let bytes = format!("{seed}-{i}").into_bytes();
            (object_utils::hash_object_bytes(&bytes), bytes)
        })
        .collect()
}

/// The hash-taking endpoints refuse a hash spelled any way but the store's own, 64 lowercase hex
/// characters, with a 422 that does not repeat it: the same answer the self-hosted server gives,
/// rather than a miss decided by S3's key casing (or, for `batch`, an internal error from the
/// scratch store's presence check).
#[test]
fn hash_taking_endpoints_refuse_an_uppercase_hash_as_unprocessable() {
    let store = MemoryObjectStore::new();
    let (hash, bytes) = synthetic_objects(1, "uppercase").remove(0);
    store.put_verified(&hash, &bytes).expect("seed");
    let head = Head::new(store, MemoryRefStore::new());

    let upper = hash.to_ascii_uppercase();
    assert_ne!(upper, hash, "the hash must carry a letter for the probe to mean anything");

    // The control: the canonical spelling is accepted, and present.
    assert!(head.missing(std::slice::from_ref(&hash)).expect("missing").is_empty());
    assert!(head.object_get(&hash).is_ok(), "the canonical spelling reads back");

    let batch = [upper.clone()];
    let errors = [
        ("missing", head.missing(&batch).expect_err("missing")),
        ("upload-targets", head.upload_targets(&Principal::Static, "lift-1", &batch).err().expect("upload-targets")),
        ("batch", head.batch(&batch).err().expect("batch")),
        ("object get", head.object_get(&upper).err().expect("object get")),
        ("object put", head.object_put(&Principal::Static, None, &upper, &bytes).err().expect("object put")),
        ("signature get", head.signature_get(&upper).err().expect("signature get")),
        ("signature put", head.signature_put(&Principal::Static, &upper, b"sig").err().expect("signature put")),
    ];
    for (endpoint, err) in errors {
        assert_eq!(err.status, Status::Unprocessable, "{}: {}", endpoint, err.message);
        // The spelling check's own refusal, not another 422 further in (`object put` without a
        // session, `signature put` with bytes that are no signature, are 422s of their own).
        assert!(err.message.contains("not an object hash"), "{}: {}", endpoint, err.message);
        assert!(!err.message.contains(&upper), "{}: the value is not repeated: {}", endpoint, err.message);
    }
}

/// `Head::missing` now answers via one bulk `ObjectStore::objects_missing` probe instead of a
/// serial `exists` loop (the fix for the same 29 s-ceiling pattern the ref-update chunk descent
/// had). Over a batch of hundreds of hashes with a scattered few absent — including a duplicate
/// of one of them — the response must be byte-for-byte what the old per-hash loop produced:
/// exactly the absent hashes, in input order, with duplicates preserved (no dedup — `missing`
/// never deduped, unlike `upload_targets`).
#[test]
fn missing_reports_every_absent_hash_in_a_large_batch_preserving_input_order() {
    let store = MemoryObjectStore::new();
    let objects = synthetic_objects(300, "missing-batch");
    let mut all: Vec<String> = objects.iter().map(|(hash, _)| hash.clone()).collect();

    // Store all but a scattered few; remember which by index.
    let absent_indices = [0usize, 47, 148, 299];
    for (i, (hash, bytes)) in objects.iter().enumerate() {
        if !absent_indices.contains(&i) {
            store.put_verified(hash, bytes).expect("seed");
        }
    }

    // A duplicate of an absent hash, appended at the end — the old loop would report it twice.
    let duplicate = all[absent_indices[1]].clone();
    all.push(duplicate.clone());

    let head = Head::new(store, MemoryRefStore::new());
    let missing = head.missing(&all).expect("missing");

    let expected: Vec<String> = absent_indices.iter().map(|&i| all[i].clone()).chain([duplicate]).collect();
    assert_eq!(missing, expected, "exactly the absent hashes, in input order, duplicates intact");
}

/// `Head::upload_targets` now resolves presence via the same bulk probe instead of one `exists`
/// per unique hash. Over a batch of hundreds with a scattered few absent (plus duplicates), the
/// `present`/`direct` (or `targets`, on a staging head) split must be unchanged: `present` holds
/// exactly the stored hashes (deduped, first-occurrence order — unlike `missing`, this endpoint
/// already deduped before this change) and every absent hash gets a target.
#[test]
fn upload_targets_negotiates_a_large_batch_matching_current_semantics() {
    let absent_indices = [3usize, 91, 250];
    let objects = synthetic_objects(400, "upload-targets-batch");
    let mut hashes: Vec<String> = objects.iter().map(|(hash, _)| hash.clone()).collect();

    // A direct head: every absent hash goes to `direct`.
    let direct_store = MemoryObjectStore::new();
    for (i, (hash, bytes)) in objects.iter().enumerate() {
        if !absent_indices.contains(&i) {
            direct_store.put_verified(hash, bytes).expect("seed");
        }
    }
    // Duplicate one present and one absent hash — both must collapse in the response.
    hashes.push(hashes[10].clone());
    hashes.push(hashes[absent_indices[0]].clone());

    let direct = Head::new(direct_store, MemoryRefStore::new());
    let answer = direct.upload_targets(&Principal::Static, "lift-1", &hashes).expect("negotiate");

    let expected_present: Vec<String> = (0..400)
        .filter(|i| !absent_indices.contains(i))
        .map(|i| hashes[i].clone())
        .collect();
    let expected_direct: Vec<String> = absent_indices.iter().map(|&i| hashes[i].clone()).collect();

    assert_eq!(answer.present, expected_present, "present is deduped, first-occurrence order");
    assert_eq!(answer.direct, expected_direct, "every absent hash is named exactly once");
    assert!(answer.targets.is_empty(), "a direct head hands out no staging targets");

    // A staging head: the same absent hashes get presigned staging targets instead of `direct`.
    let staging_store = MemoryObjectStore::with_redirect("https://s3.example/bucket");
    for (i, (hash, bytes)) in objects.iter().enumerate() {
        if !absent_indices.contains(&i) {
            staging_store.put_verified(hash, bytes).expect("seed");
        }
    }
    let staging = Head::new(staging_store, MemoryRefStore::new());
    let answer = staging.upload_targets(&Principal::Static, "lift-1", &hashes).expect("negotiate");

    assert_eq!(answer.present, expected_present);
    assert!(answer.direct.is_empty(), "a staging head never answers `direct`");
    for hash in &expected_direct {
        assert!(answer.targets.contains_key(hash), "{} should have a staging target", hash);
    }
    assert_eq!(answer.targets.len(), expected_direct.len());
}

/// `Head::commit_lift`'s blob-presence check resolves via the same bulk probe instead of one
/// `exists` per blob. Over a batch of hundreds with a scattered few in neither place, the
/// refusal names exactly the *first* such blob in `blobs` order, not every missing blob.
#[test]
fn commit_lift_blobs_presence_check_matches_current_semantics_over_a_large_batch() {
    let store = MemoryObjectStore::new();
    let objects = synthetic_objects(500, "commit-lift-batch");
    let blobs: Vec<String> = objects.iter().map(|(hash, _)| hash.clone()).collect();

    // Promote all but a scattered few (store them directly at their canonical key — `commit_lift`
    // checks presence there, exactly as `verify_and_promote` would leave them).
    let absent_indices = [12usize, 200, 480];
    for (i, (hash, bytes)) in objects.iter().enumerate() {
        if !absent_indices.contains(&i) {
            store.put_verified(hash, bytes).expect("seed");
        }
    }

    let head = Head::new(store, MemoryRefStore::new());

    let err = head
        .commit_lift(&Principal::Static, "lift-1", &[], &blobs, false)
        .expect_err("not every blob is promoted yet");
    assert_eq!(err.status, Status::Unprocessable);
    assert_eq!(refusal_code(&err.message), Some(RefusalCode::StagedUploadDiscarded), "{}", err.message);
    // The *first* absent blob in `blobs` order — index 12, not 200 or 480.
    assert!(err.message.contains(&blobs[absent_indices[0]]), "{}", err.message);
    assert!(!err.message.contains(&blobs[absent_indices[1]]), "{}", err.message);

    // The control: promote everything and the identical batch commits clean.
    for &i in &absent_indices {
        head.objects.put_verified(&objects[i].0, &objects[i].1).expect("promote");
    }
    head.commit_lift(&Principal::Static, "lift-1", &[], &blobs, false).expect("every blob is now present");
}

// ---------------------------------------------------------------------------------------
// The sync/async seam: stores whose every operation is a future — the AWS SDK's shape —
// implementing the synchronous traits through `AsyncBridge`.
// ---------------------------------------------------------------------------------------

/// An [`ObjectStore`] whose every call suspends, as an `aws-sdk-s3` call does. It exists to
/// prove the seam: `forklift_core`'s synchronous audit, its thread-local storage scope and
/// the whole `Head` run on one blocking thread, over a backend that is async underneath.
struct AsyncObjectStore {
    inner: MemoryObjectStore,
    bridge: AsyncBridge,
}

/// Suspend first, *then* do the work — the shape of a real SDK call, whose response is
/// handled after the await. A future that cannot resolve on its first poll, so the driver
/// underneath must genuinely be running for the bridged call to return at all.
async fn suspending<T>(work: impl FnOnce() -> T) -> T {
    tokio::task::yield_now().await;

    work()
}

impl ObjectStore for AsyncObjectStore {
    fn exists(&self, hash: &str) -> Result<bool, String> {
        self.bridge.block_on(suspending(|| self.inner.exists(hash)))
    }

    fn get(&self, hash: &str) -> Result<Option<Vec<u8>>, String> {
        self.bridge.block_on(suspending(|| self.inner.get(hash)))
    }

    fn objects_staged(&self, session: &str, hashes: &[String]) -> Result<Vec<String>, String> {
        self.bridge.block_on(suspending(|| self.inner.objects_staged(session, hashes)))
    }

    fn put_verified(&self, hash: &str, bytes: &[u8]) -> Result<PutOutcome, String> {
        self.bridge.block_on(suspending(|| self.inner.put_verified(hash, bytes)))
    }

    fn get_signature(&self, parcel_hash: &str) -> Result<Option<Vec<u8>>, String> {
        self.bridge.block_on(suspending(|| self.inner.get_signature(parcel_hash)))
    }

    fn put_signature(&self, parcel_hash: &str, bytes: &[u8]) -> Result<SignatureOutcome, String> {
        self.bridge.block_on(suspending(|| self.inner.put_signature(parcel_hash, bytes)))
    }
}

/// The same for the consistency point: DynamoDB is async too.
struct AsyncRefStore {
    inner: MemoryRefStore,
    bridge: AsyncBridge,
}

impl RefStore for AsyncRefStore {
    fn get_head(&self, namespace: pallet_utils::PalletNamespace, name: &str) -> Result<RefHead, String> {
        self.bridge.block_on(suspending(|| self.inner.get_head(namespace, name)))
    }

    fn compare_and_set_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        expected: ExpectedHead<'_>,
        new: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.bridge.block_on(suspending(|| {
            self.inner.compare_and_set_head(namespace, name, expected, new, office_head, anchor, request_token)
        }))
    }

    fn delete_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        old: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.bridge.block_on(suspending(|| self.inner.delete_head(namespace, name, old, office_head, anchor, request_token)))
    }
    fn list_refs(&self) -> Result<RefListing, String> {
        self.bridge.block_on(suspending(|| self.inner.list_refs()))
    }

    fn default_pallet(&self) -> Result<String, String> {
        self.bridge.block_on(suspending(|| self.inner.default_pallet()))
    }

    fn get_trust(&self) -> Result<Option<(office_utils::TrustAnchor, String)>, String> {
        self.bridge.block_on(suspending(|| self.inner.get_trust()))
    }

    fn put_trust_if_absent(&self, anchor: &office_utils::TrustAnchor) -> Result<TrustOutcome, String> {
        self.bridge.block_on(suspending(|| self.inner.put_trust_if_absent(anchor)))
    }

    fn replace_trust(
        &self,
        anchor: &office_utils::TrustAnchor,
        expected_anchor: &str,
        office_head: Option<&str>,
        request_token: &str,
    ) -> Result<TrustWriteOutcome, String> {
        self.bridge.block_on(suspending(|| self.inner.replace_trust(anchor, expected_anchor, office_head, request_token)))
    }
}

/// The whole trusted lift — mirror, thread-local storage scope, signature audit, CAS —
/// runs synchronously on a blocking thread over stores that are async underneath. This is
/// the shape the S3 + DynamoDB implementations take, minus AWS.
#[tokio::test(flavor = "multi_thread")]
async fn a_trusted_lift_runs_over_async_backed_stores_from_a_blocking_thread() {
    let area = Area::new("async-seam");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "signed one"]);

    let harvested = harvest(&area.path("wh"));
    let office_head = harvested.head_of("@office").expect("office head");
    let main_head = harvested.head_of("main").expect("main head");

    let bridge = AsyncBridge::current().expect("the test runs on a multi-thread runtime");

    let head = Head::new(
        AsyncObjectStore { inner: MemoryObjectStore::new(), bridge: bridge.clone() },
        AsyncRefStore { inner: MemoryRefStore::new(), bridge },
    );

    let expected = main_head.clone();

    tokio::task::spawn_blocking(move || {
        upload_all(&head, &harvested);
        head.put_trust(&Principal::Static, harvested.trust.as_ref().expect("trust")).expect("plant trust");

        head.ref_update(&Principal::Static, "@office", &RefUpdateRequest { old_head: None, new_head: office_head, tombstone: None })
            .expect("lift the office over an async store");
        head.ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: None, new_head: main_head, tombstone: None })
            .expect("lift the pallet over an async store");

        assert_eq!(
            head.refs.get_head(pallet_utils::PalletNamespace::User, "main").unwrap().live(),
            Some(expected.as_str())
        );
    })
    .await
    .expect("the head runs to completion on a blocking thread");
}

// ---------------------------------------------------------------------------------------
// FORK-95 slice 2: the commit precondition across the office head and the trust anchor.
//
// `ref_update` snapshots the office head and the trust anchor once, before its audit runs,
// then commits conditioned on both still holding those values (`compare_and_set_head`). The
// tests below prove that both directions of the precondition actually bind:
//
// * a concurrent office lift or re-genesis landing after the snapshot and before the commit
//   refuses the update, distinguishably from an ordinary moved-pallet-head conflict;
// * a concurrent move of a pallet the transaction never names does not.
//
// The two wrapper `RefStore`s below make the interleaving deterministic rather than a real
// race: each hooks the exact snapshot read `ref_update` performs and injects the "concurrent"
// move immediately after that read returns — the earliest point a real race could land it, and
// strictly before `ref_update` issues its commit (nothing between that snapshot and the commit
// reads the ref store again; see `head.rs`'s `ref_update`).
// ---------------------------------------------------------------------------------------

/// Forwards every [`RefStore`] call to a shared [`MemoryRefStore`] — the plumbing that lets a
/// "setup" [`Head`] and an "attack" [`Head`] below operate on the same warehouse state.
#[derive(Clone)]
struct SharedMemoryRefs(Arc<MemoryRefStore>);

impl RefStore for SharedMemoryRefs {
    fn get_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
    ) -> Result<RefHead, String> {
        self.0.get_head(namespace, name)
    }

    fn compare_and_set_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        expected: ExpectedHead<'_>,
        new: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.0.compare_and_set_head(namespace, name, expected, new, office_head, anchor, request_token)
    }

    fn delete_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        old: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.0.delete_head(namespace, name, old, office_head, anchor, request_token)
    }
    fn list_refs(&self) -> Result<RefListing, String> {
        self.0.list_refs()
    }

    fn default_pallet(&self) -> Result<String, String> {
        self.0.default_pallet()
    }

    fn get_trust(&self) -> Result<Option<(office_utils::TrustAnchor, String)>, String> {
        self.0.get_trust()
    }

    fn put_trust_if_absent(&self, anchor: &office_utils::TrustAnchor) -> Result<TrustOutcome, String> {
        self.0.put_trust_if_absent(anchor)
    }

    fn replace_trust(
        &self,
        anchor: &office_utils::TrustAnchor,
        expected_anchor: &str,
        office_head: Option<&str>,
        request_token: &str,
    ) -> Result<TrustWriteOutcome, String> {
        self.0.replace_trust(anchor, expected_anchor, office_head, request_token)
    }
}

/// Wraps a shared [`MemoryRefStore`]; the first time `ref_update`'s office-head snapshot read
/// fires, injects a concurrent office lift to `move_to` immediately afterward, using the
/// snapshot's own answer as the CAS's `expected` — so the injected move is itself a genuine,
/// uncontested commit, not a forced write. Forwards every other call.
struct OfficeMovesAfterTheSnapshot {
    inner: Arc<MemoryRefStore>,
    move_to: String,
    fired: Cell<bool>,
}

impl RefStore for OfficeMovesAfterTheSnapshot {
    fn get_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
    ) -> Result<RefHead, String> {
        let result = self.inner.get_head(namespace, name);

        if namespace == pallet_utils::PalletNamespace::Meta
            && name == OFFICE_PALLET_NAME
            && !self.fired.replace(true)
        {
            if let Ok(current) = &result {
                // This injected CAS is itself subject to the same three-way precondition —
                // the anchor is checked unconditionally, regardless of which pallet is being
                // updated — so it must supply the anchor's *current* bytes to commit
                // uncontested. `office_head` is irrelevant here: the target *is* `@office`
                // itself, so `checks_office_separately` is false and that parameter is never
                // consulted.
                let anchor_bytes = self
                    .inner
                    .get_trust()
                    .expect("read the anchor for the injected move")
                    .map(|(_, bytes)| bytes);

                let outcome = self
                    .inner
                    .compare_and_set_head(
                        pallet_utils::PalletNamespace::Meta,
                        OFFICE_PALLET_NAME,
                        current.live().map_or(ExpectedHead::Unborn, ExpectedHead::At),
                        &self.move_to,
                        OfficePrecondition::NotConsumed,
                        anchor_bytes.as_deref(),
                        &request_token(),
                    )
                    .expect("inject the concurrent office move");
                assert_eq!(
                    outcome,
                    CasOutcome::Committed,
                    "the injected office move must itself succeed uncontested"
                );
            }
        }

        result
    }

    fn compare_and_set_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        expected: ExpectedHead<'_>,
        new: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.compare_and_set_head(namespace, name, expected, new, office_head, anchor, request_token)
    }

    fn delete_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        old: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.delete_head(namespace, name, old, office_head, anchor, request_token)
    }
    fn list_refs(&self) -> Result<RefListing, String> {
        self.inner.list_refs()
    }

    fn default_pallet(&self) -> Result<String, String> {
        self.inner.default_pallet()
    }

    fn get_trust(&self) -> Result<Option<(office_utils::TrustAnchor, String)>, String> {
        self.inner.get_trust()
    }

    fn put_trust_if_absent(&self, anchor: &office_utils::TrustAnchor) -> Result<TrustOutcome, String> {
        self.inner.put_trust_if_absent(anchor)
    }

    fn replace_trust(
        &self,
        anchor: &office_utils::TrustAnchor,
        expected_anchor: &str,
        office_head: Option<&str>,
        request_token: &str,
    ) -> Result<TrustWriteOutcome, String> {
        self.inner.replace_trust(anchor, expected_anchor, office_head, request_token)
    }
}

/// Wraps a shared [`MemoryRefStore`]; the first time `ref_update`'s anchor snapshot read
/// (`get_trust`) fires, injects a concurrent re-genesis to `move_to` immediately afterward.
/// Forwards every other call.
struct AnchorMovesAfterTheSnapshot {
    inner: Arc<MemoryRefStore>,
    move_to: office_utils::TrustAnchor,
    fired: Cell<bool>,
}

impl RefStore for AnchorMovesAfterTheSnapshot {
    fn get_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
    ) -> Result<RefHead, String> {
        self.inner.get_head(namespace, name)
    }

    fn compare_and_set_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        expected: ExpectedHead<'_>,
        new: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.compare_and_set_head(namespace, name, expected, new, office_head, anchor, request_token)
    }

    fn delete_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        old: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.delete_head(namespace, name, old, office_head, anchor, request_token)
    }
    fn list_refs(&self) -> Result<RefListing, String> {
        self.inner.list_refs()
    }

    fn default_pallet(&self) -> Result<String, String> {
        self.inner.default_pallet()
    }

    fn get_trust(&self) -> Result<Option<(office_utils::TrustAnchor, String)>, String> {
        let result = self.inner.get_trust();

        if !self.fired.replace(true) {
            // The injected re-genesis is itself a conditional write now, so this fake has to
            // supply the preconditions any real caller would: the incumbent bytes it replaces —
            // the very ones this read is about to return — and the office head it lands against.
            // A fake still able to write unconditionally would be modelling a store this crate
            // no longer has, and the interleaving it injects would be one no real client could
            // produce.
            let expected = result
                .as_ref()
                .ok()
                .and_then(|trust| trust.as_ref().map(|(_, bytes)| bytes.clone()))
                .expect("an incumbent anchor to replace");
            let office_head = self
                .inner
                .get_head(pallet_utils::PalletNamespace::Meta, OFFICE_PALLET_NAME)
                .expect("read the office head");

            assert_eq!(
                self.inner
                    .replace_trust(&self.move_to, &expected, office_head.live(), &request_token())
                    .expect("inject the concurrent re-genesis"),
                TrustWriteOutcome::Replaced,
                "the injected re-genesis must actually land, or the test it feeds proves \
                nothing about a moved anchor"
            );
        }

        result
    }

    fn put_trust_if_absent(&self, anchor: &office_utils::TrustAnchor) -> Result<TrustOutcome, String> {
        self.inner.put_trust_if_absent(anchor)
    }

    fn replace_trust(
        &self,
        anchor: &office_utils::TrustAnchor,
        expected_anchor: &str,
        office_head: Option<&str>,
        request_token: &str,
    ) -> Result<TrustWriteOutcome, String> {
        self.inner.replace_trust(anchor, expected_anchor, office_head, request_token)
    }
}

/// A [`RefStore`] that forwards everything except `compare_and_set_head`, whose first
/// `refusals` attempts answer [`CasOutcome::Transient`] regardless of their arguments, after
/// which it forwards that too. Every attempt's request token is recorded, so a test can count the
/// head's attempts and see which token each carried.
///
/// `MemoryRefStore` itself never produces `Transient` (see its own docs), so it is otherwise
/// unreachable through `Head::ref_update` in this crate's test suite — only a fake built to
/// return it can prove the head's re-issue loop, `transient_contention()` and the
/// `Status::ServiceUnavailable -> 503` mapping actually fire, rather than that variant silently
/// falling through to a default match arm.
struct TransientOnCommit {
    inner: MemoryRefStore,
    refusals: usize,
    refusal: CasOutcome,
    /// What the first attempt answers instead of `refusal`, when set: a request whose attempts
    /// disagree, as when an attempt of unknown fate is followed by contention.
    first: Option<CasOutcome>,
    /// Fail every attempt after the first with a fault instead.
    fault: bool,
    tokens: std::sync::Mutex<Vec<String>>,
}

impl TransientOnCommit {
    fn always() -> TransientOnCommit {
        TransientOnCommit::times(usize::MAX)
    }

    fn times(refusals: usize) -> TransientOnCommit {
        TransientOnCommit {
            inner: MemoryRefStore::new(),
            refusals,
            refusal: CasOutcome::Transient(TransientCause::Contended),
            first: None,
            fault: false,
            tokens: Default::default(),
        }
    }

    /// Every attempt answers that the pallet moved — the refusal the loop must not re-issue.
    fn always_moved() -> TransientOnCommit {
        TransientOnCommit {
            refusal: CasOutcome::Conflict { current: RefHead::Live("f".repeat(64)) },
            ..TransientOnCommit::always()
        }
    }

    fn tokens(&self) -> Vec<String> {
        self.tokens.lock().unwrap().clone()
    }
}

impl RefStore for TransientOnCommit {
    fn get_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
    ) -> Result<RefHead, String> {
        self.inner.get_head(namespace, name)
    }

    fn compare_and_set_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        expected: ExpectedHead<'_>,
        new: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        let attempt = {
            let mut tokens = self.tokens.lock().unwrap();
            tokens.push(request_token.to_string());
            tokens.len()
        };

        if attempt == 1 {
            if let Some(first) = &self.first {
                return Ok(first.clone());
            }
        }

        if attempt > 1 && self.fault {
            return Err("a fault".to_string());
        }

        if attempt <= self.refusals {
            return Ok(self.refusal.clone());
        }

        self.inner.compare_and_set_head(namespace, name, expected, new, office_head, anchor, request_token)
    }

    fn delete_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        old: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.delete_head(namespace, name, old, office_head, anchor, request_token)
    }
    fn list_refs(&self) -> Result<RefListing, String> {
        self.inner.list_refs()
    }

    fn default_pallet(&self) -> Result<String, String> {
        self.inner.default_pallet()
    }

    fn get_trust(&self) -> Result<Option<(office_utils::TrustAnchor, String)>, String> {
        self.inner.get_trust()
    }

    fn put_trust_if_absent(&self, anchor: &office_utils::TrustAnchor) -> Result<TrustOutcome, String> {
        self.inner.put_trust_if_absent(anchor)
    }

    fn replace_trust(
        &self,
        anchor: &office_utils::TrustAnchor,
        expected_anchor: &str,
        office_head: Option<&str>,
        request_token: &str,
    ) -> Result<TrustWriteOutcome, String> {
        self.inner.replace_trust(anchor, expected_anchor, office_head, request_token)
    }
}

/// A pushed history ready to commit to `main` of an untrusted warehouse, uploaded into `head`.
fn uploaded_first_push<R: RefStore>(area_name: &str, refs: R) -> (Area, Head<MemoryObjectStore, R>, String) {
    let area = Area::new(area_name);
    prepare(&area, "wh");
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "first"]);

    let harvested = harvest(&area.path("wh"));
    let main_head = harvested.head_of("main").expect("main head");

    let head = Head::new(MemoryObjectStore::new(), refs);
    upload_all(&head, &harvested);

    (area, head, main_head)
}

/// `CasOutcome::Transient` reaches `head.rs::transient_contention` and
/// `Status::ServiceUnavailable`'s `503` only through these tests — an edit remapping the
/// `Transient` arm in `ref_update`'s match to `self.moved(...)` or `HeadError::internal(...)`
/// would otherwise pass the entire suite, since nothing else in this crate ever observes that arm
/// running.
///
/// A commit that stays transient is re-issued until the budget is spent — four attempts, all
/// under one request token — and only then answered `503`. A second request gets a token of its
/// own: a token per process or per head would make DynamoDB replay one request's outcome for
/// another.
#[test]
fn a_commit_that_stays_transient_is_attempted_four_times_under_one_token_then_answers_503() {
    let (_area, head, main_head) =
        uploaded_first_push("transient-cas-outcome", TransientOnCommit::always());

    let err = head
        .ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: None, new_head: main_head.clone(), tombstone: None })
        .expect_err("a Transient CasOutcome must refuse the commit, not accept it");

    assert_eq!(err.status, Status::ServiceUnavailable);
    assert_eq!(err.status.as_u16(), 503);

    let first = head.refs.tokens();
    assert_eq!(first.len(), 4, "one budget is four attempts");
    assert!(first.iter().all(|token| *token == first[0]), "one token per request: {first:?}");

    let _ = head.ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: None, new_head: main_head, tombstone: None });
    let both = head.refs.tokens();
    assert_eq!(both.len(), 8);
    assert!(both[4..].iter().all(|token| *token == both[4]));
    assert_ne!(both[0], both[4], "each request mints its own token");
}

/// A commit any of whose attempts may have landed must not answer that nothing moved: a client
/// trusting that would push again and be told the pallet moved — to its own head. That holds
/// when the uncertain attempt is not the last: under one token, an attempt of unknown fate can
/// still commit after the attempts behind it are refused for contention. An attempt still in
/// progress is uncertain in the same way. Only a request refused for contention throughout keeps
/// "nothing moved".
#[test]
fn a_commit_of_unknown_fate_answers_503_without_claiming_nothing_moved() {
    let uncertain = [
        ("unknown-fate-cas", None, TransientCause::FateUnknown),
        ("unknown-then-contended-cas", Some(TransientCause::FateUnknown), TransientCause::Contended),
        ("in-progress-cas", None, TransientCause::InProgress),
    ];

    for (name, first, then) in uncertain {
        let (_area, head, main_head) = uploaded_first_push(
            name,
            TransientOnCommit {
                refusal: CasOutcome::Transient(then),
                first: first.map(CasOutcome::Transient),
                ..TransientOnCommit::always()
            },
        );
        // Near enough the deadline that an in-progress refusal is not waited out.
        let head = head.with_deadline(std::time::SystemTime::now() + std::time::Duration::from_secs(3));

        let err = head
            .ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: None, new_head: main_head, tombstone: None })
            .expect_err("an uncertain outcome is still a refusal");

        assert_eq!(err.status, Status::ServiceUnavailable);
        assert!(err.message.contains("may or may not have been applied"), "{name}: {}", err.message);
        assert!(err.message.contains("Fetch the warehouse"), "{name}: {}", err.message);
        assert!(!err.message.contains("No input is known to have moved"), "{name}: {}", err.message);
    }

    // A fault on the re-issue is a `503` of unknown fate after an attempt of unknown fate, and the
    // fault itself after contention.
    for (name, first, status) in [
        ("unknown-then-fault-cas", TransientCause::FateUnknown, Status::ServiceUnavailable),
        ("contended-then-fault-cas", TransientCause::Contended, Status::Internal),
    ] {
        let (_area, head, main_head) = uploaded_first_push(
            name,
            TransientOnCommit { first: Some(CasOutcome::Transient(first)), fault: true, ..TransientOnCommit::always() },
        );
        let err = head
            .ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: None, new_head: main_head, tombstone: None })
            .expect_err("a fault is a refusal");

        assert_eq!(err.status, status, "{name}: {}", err.message);
        assert_eq!(err.message.contains("may or may not have been applied"), status == Status::ServiceUnavailable, "{name}: {}", err.message);
    }

    let (_area, head, main_head) =
        uploaded_first_push("contended-cas-message", TransientOnCommit::always());
    let err = head
        .ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: None, new_head: main_head, tombstone: None })
        .expect_err("contention outlasting the budget is a refusal");

    assert!(err.message.contains("No input is known to have moved"), "{}", err.message);
}

/// The point of the loop: a commit refused once for contention, with nothing having moved,
/// succeeds on its re-issue instead of reaching the client as a `503`.
#[test]
fn a_commit_refused_transiently_once_is_reissued_and_lands() {
    let (_area, head, main_head) =
        uploaded_first_push("transient-cas-once", TransientOnCommit::times(1));

    head.ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: None, new_head: main_head.clone(), tombstone: None })
        .expect("the re-issue lands");

    assert_eq!(head.refs.tokens().len(), 2);
    assert_eq!(
        head.refs.inner.get_head(pallet_utils::PalletNamespace::User, "main").unwrap(),
        RefHead::Live(main_head)
    );
}

/// A refusal naming a moved input is never re-issued: it is the client's to act on, and the
/// same transaction would only be refused again.
#[test]
fn a_moved_pallet_head_is_answered_on_the_first_attempt() {
    let (_area, head, main_head) =
        uploaded_first_push("moved-cas-not-reissued", TransientOnCommit::always_moved());

    let err = head
        .ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: None, new_head: main_head, tombstone: None })
        .expect_err("the commit reports the pallet moved");

    assert_eq!(err.status, Status::Conflict);
    assert_eq!(head.refs.tokens().len(), 1);
}

/// With too little of the invocation left to cover a wait and another attempt, the head answers
/// `503` after the first attempt rather than re-issuing into a window it cannot finish in.
#[test]
fn a_commit_near_its_deadline_is_not_reissued() {
    let (_area, head, main_head) =
        uploaded_first_push("transient-cas-deadline", TransientOnCommit::always());
    let head = head.with_deadline(std::time::SystemTime::now() + std::time::Duration::from_millis(500));

    let err = head
        .ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: None, new_head: main_head, tombstone: None })
        .expect_err("transient");

    assert_eq!(err.status, Status::ServiceUnavailable);
    assert_eq!(head.refs.tokens().len(), 1);
}

/// Falsifier 1a (office): the ref update must be refused, and refused as a **moved office**,
/// not as an ordinary moved-pallet-head conflict — the pallet's own head never moved.
#[test]
fn a_concurrent_office_move_refuses_the_commit_as_office_moved_not_pallet_moved() {
    let area = Area::new("office-races-the-commit");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "signed one"]);

    let main_v1 = harvest(&area.path("wh")).head_of("main").expect("main v1");

    area.write_file("wh/app.txt", "v2\n");
    area.forklift("wh", &["load", "app.txt"]);
    area.forklift("wh", &["stack", "signed two"]);

    let latest = harvest(&area.path("wh"));
    let anchor = latest.trust.clone().expect("trust established");
    let office_head = latest.head_of(&format!("@{}", OFFICE_PALLET_NAME)).expect("office head");
    let main_v2 = latest.head_of("main").expect("main v2");
    assert_ne!(main_v1, main_v2);

    let store = Arc::new(MemoryRefStore::new());

    // Setup: establish trust, lift the office, lift main to v1 — nothing races here.
    let setup = Head::new(MemoryObjectStore::new(), SharedMemoryRefs(store.clone()));
    upload_all(&setup, &latest);
    setup.put_trust(&Principal::Static, &anchor).expect("put trust");
    setup
        .ref_update(&Principal::Static, &format!("@{}", OFFICE_PALLET_NAME), &RefUpdateRequest {
            old_head: None,
            new_head: office_head.clone(),
            tombstone: None,
        })
        .expect("lift office");
    setup
        .ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: None, new_head: main_v1.clone(), tombstone: None })
        .expect("lift main v1");

    // Attack: lift main v1 -> v2 while an office lift lands right after the audit's
    // office-head snapshot — landing before the commit `ref_update` issues once its (still
    // valid, still v1-office-audited) audit finishes.
    let attack = Head::new(
        MemoryObjectStore::new(),
        OfficeMovesAfterTheSnapshot {
            inner: store.clone(),
            move_to: "f".repeat(64),
            fired: Cell::new(false),
        },
    );
    upload_all(&attack, &latest);

    let err = attack
        .ref_update(&Principal::Static, "main", &RefUpdateRequest {
            old_head: Some(main_v1.clone()),
            new_head: main_v2.clone(),
            tombstone: None,
        })
        .expect_err("the office moved between the audit and the commit");
    assert_eq!(err.status, Status::Conflict);
    let (code, _, _) = forklift_core::error::deframe(&err.message).expect("a classified refusal");
    assert_eq!(code, "remote_ahead", "{}", err.message);
    assert!(
        err.message.to_lowercase().contains("office"),
        "the refusal must name the office: {}",
        err.message
    );
    assert!(
        !err.message.contains("The pallet moved"),
        "an office-moved refusal must not be indistinguishable from an ordinary moved-pallet \
        conflict: {}",
        err.message
    );

    // The control: main's own head is unaffected by the refused commit or the injected move.
    assert_eq!(
        store.get_head(pallet_utils::PalletNamespace::User, "main").expect("get").live(),
        Some(main_v1.as_str())
    );
}

/// A grant revoked by an office lift landing between the move gate's office read and the commit
/// refuses the move. The gate judges the office head the update read once, and the commit is
/// conditioned on that same head, so the revocation moving the office refuses the commit rather
/// than racing it. Were the gate to judge an office read of its own, earlier than the one the
/// commit is conditioned on, the writer whose grant was just revoked would land the move.
#[test]
fn a_grant_revoked_between_the_move_gate_and_the_commit_refuses_the_move() {
    let area = Area::new("grant-revoked-mid-update");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);

    let writer = "wes@forklift";
    area.forklift("wh", &["config", "operator.identifier", writer]);
    let keygen = area.forklift_stdout("wh", &["office", "keygen"]);
    area.forklift("wh", &["config", "operator.identifier", "tester@forklift"]);
    let admit: Vec<&str> = keygen.lines()
        .find(|line| line.trim_start().starts_with("office admit "))
        .expect("keygen prints the admit line")
        .split_whitespace()
        .collect();
    area.forklift("wh", &admit);
    area.forklift("wh", &["office", "role", writer, "writer", "--pallet", "main"]);

    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "signed one"]);

    let granted = harvest(&area.path("wh"));
    let office_granted = granted.head_of(&format!("@{}", OFFICE_PALLET_NAME)).expect("office head");
    let main_v1 = granted.head_of("main").expect("main v1");

    area.write_file("wh/app.txt", "v2\n");
    area.forklift("wh", &["load", "app.txt"]);
    area.forklift("wh", &["stack", "signed two"]);

    // The revocation: the writer keeps their role and key, but not main.
    area.forklift("wh", &["office", "role", writer, "writer", "--pallet", "side"]);

    let latest = harvest(&area.path("wh"));
    let anchor = latest.trust.clone().expect("trust established");
    let office_revoked = latest.head_of(&format!("@{}", OFFICE_PALLET_NAME)).expect("office head");
    let main_v2 = latest.head_of("main").expect("main v2");
    assert_ne!(office_granted, office_revoked);

    let caller = Principal::Operator(writer.to_string());
    let office_wire = format!("@{}", OFFICE_PALLET_NAME);
    let advance = RefUpdateRequest { old_head: Some(main_v1.clone()), new_head: main_v2.clone(), tombstone: None };

    // Trust, the granting office and main v1, set up by the administrator.
    let set_up = || {
        let store = Arc::new(MemoryRefStore::new());
        let setup = Head::new(MemoryObjectStore::new(), SharedMemoryRefs(store.clone()));
        upload_all(&setup, &latest);
        setup.put_trust(&Principal::Static, &anchor).expect("put trust");
        setup
            .ref_update(&Principal::Static, &office_wire, &RefUpdateRequest { old_head: None, new_head: office_granted.clone(), tombstone: None })
            .expect("lift the granting office");
        setup
            .ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: None, new_head: main_v1.clone(), tombstone: None })
            .expect("lift main v1");
        store
    };

    // Controls: under the granting office the writer moves main; under the revoking one, not.
    let store = set_up();
    let granted_head = Head::new(MemoryObjectStore::new(), SharedMemoryRefs(store.clone()));
    upload_all(&granted_head, &latest);
    granted_head.ref_update(&caller, "main", &advance).expect("the granted writer moves main");

    let store = set_up();
    let revoking = Head::new(MemoryObjectStore::new(), SharedMemoryRefs(store.clone()));
    upload_all(&revoking, &latest);
    revoking
        .ref_update(&Principal::Static, &office_wire, &RefUpdateRequest {
            old_head: Some(office_granted.clone()),
            new_head: office_revoked.clone(),
            tombstone: None,
        })
        .expect("lift the revoking office");
    let err = revoking.ref_update(&caller, "main", &advance).expect_err("the revoked writer");
    assert_eq!(err.status, Status::Forbidden, "{}", err.message);

    // The race: the revoking office lands right after the update's one office read.
    let store = set_up();
    let attack = Head::new(
        MemoryObjectStore::new(),
        OfficeMovesAfterTheSnapshot {
            inner: store.clone(),
            move_to: office_revoked.clone(),
            fired: Cell::new(false),
        },
    );
    upload_all(&attack, &latest);

    let err = attack.ref_update(&caller, "main", &advance).expect_err("the grant was revoked mid-update");
    assert_eq!(err.status, Status::Conflict, "{}", err.message);
    let (code, _, _) = forklift_core::error::deframe(&err.message).expect("a classified refusal");
    assert_eq!(code, "remote_ahead", "{}", err.message);
    assert!(err.message.to_lowercase().contains("office"), "{}", err.message);

    assert_eq!(
        store.get_head(pallet_utils::PalletNamespace::User, "main").expect("get").live(),
        Some(main_v1.as_str()),
        "main did not move"
    );
    assert_eq!(
        store.get_head(pallet_utils::PalletNamespace::Meta, OFFICE_PALLET_NAME).expect("get").live(),
        Some(office_revoked.as_str()),
        "the revocation landed"
    );
}

/// On a warehouse with no anchor the audit never reads the office head at all — both the
/// office-chain mirror and the whole trust block in `ref_update` are gated on
/// `anchor.is_some()` — so the commit must not condition on it either. No head lifts an office
/// without an anchor, so the only such warehouse that has an office is a stripped store (which
/// clients refuse at the handshake); this pins the store-level contract that
/// `OfficePrecondition::NotConsumed` is never upgraded to a check the audit did not earn. Do not
/// rebuild the setup as a head lift: that is now a `422`. Sibling to
/// `a_concurrent_office_move_refuses_the_commit_as_office_moved_not_pallet_moved`, which pins
/// the opposite, the *trusted* case where the same race must refuse.
#[test]
fn an_untrusted_warehouses_concurrent_office_move_does_not_refuse_the_commit() {
    let area = Area::new("untrusted-office-races-the-commit");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "first"]);

    let main_v1 = harvest(&area.path("wh")).head_of("main").expect("main v1");

    area.write_file("wh/app.txt", "v2\n");
    area.forklift("wh", &["load", "app.txt"]);
    area.forklift("wh", &["stack", "second"]);

    let latest = harvest(&area.path("wh"));
    let office_head = latest.head_of(&format!("@{}", OFFICE_PALLET_NAME)).expect("office head");
    let main_v2 = latest.head_of("main").expect("main v2");
    assert_ne!(main_v1, main_v2);

    let store = Arc::new(MemoryRefStore::new());

    // Setup: an office with no anchor, planted beneath the head — no head lifts one, so the
    // state is reachable only as a stripped store — then main v1, unaudited (no `put_trust`
    // anywhere in this test: an untrusted warehouse verifies nothing).
    assert!(matches!(
        store.compare_and_set_head(
            pallet_utils::PalletNamespace::Meta,
            OFFICE_PALLET_NAME,
            ExpectedHead::Unborn,
            &office_head,
            OfficePrecondition::NotConsumed,
            None,
            &request_token(),
        ),
        Ok(CasOutcome::Committed)
    ));
    let setup = Head::new(MemoryObjectStore::new(), SharedMemoryRefs(store.clone()));
    upload_all(&setup, &latest);
    setup
        .ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: None, new_head: main_v1.clone(), tombstone: None })
        .expect("lift main v1 (untrusted, unaudited)");

    // Attack: lift main v1 -> v2 while the office pallet moves right after `ref_update`'s
    // office-head snapshot read — the same injection the trusted sibling test uses. There the
    // move refuses the commit; here it must not, because this warehouse's audit never
    // consumed the office head to begin with.
    let attack = Head::new(
        MemoryObjectStore::new(),
        OfficeMovesAfterTheSnapshot {
            inner: store.clone(),
            move_to: "f".repeat(64),
            fired: Cell::new(false),
        },
    );
    upload_all(&attack, &latest);

    attack
        .ref_update(&Principal::Static, "main", &RefUpdateRequest {
            old_head: Some(main_v1),
            new_head: main_v2.clone(),
            tombstone: None,
        })
        .expect(
            "an untrusted push must never be refused for an office move its audit never \
            consumed",
        );

    assert_eq!(
        store.get_head(pallet_utils::PalletNamespace::User, "main").expect("get").live(),
        Some(main_v2.as_str())
    );
}

/// Falsifier 1b (anchor): likewise for the trust anchor — the ref update must be refused as a
/// **moved anchor**, not as an ordinary moved-pallet-head conflict.
#[test]
fn a_concurrent_trust_anchor_change_refuses_the_commit_as_anchor_moved_not_pallet_moved() {
    let area = Area::new("anchor-races-the-commit");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "signed one"]);

    let main_v1 = harvest(&area.path("wh")).head_of("main").expect("main v1");

    area.write_file("wh/app.txt", "v2\n");
    area.forklift("wh", &["load", "app.txt"]);
    area.forklift("wh", &["stack", "signed two"]);

    let latest = harvest(&area.path("wh"));
    let anchor = latest.trust.clone().expect("trust established");
    let office_head = latest.head_of(&format!("@{}", OFFICE_PALLET_NAME)).expect("office head");
    let main_v2 = latest.head_of("main").expect("main v2");

    let store = Arc::new(MemoryRefStore::new());

    let setup = Head::new(MemoryObjectStore::new(), SharedMemoryRefs(store.clone()));
    upload_all(&setup, &latest);
    setup.put_trust(&Principal::Static, &anchor).expect("put trust");
    setup
        .ref_update(&Principal::Static, &format!("@{}", OFFICE_PALLET_NAME), &RefUpdateRequest {
            old_head: None,
            new_head: office_head.clone(),
            tombstone: None,
        })
        .expect("lift office");
    setup
        .ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: None, new_head: main_v1.clone(), tombstone: None })
        .expect("lift main v1");

    // A well-formed but different anchor. Its content need not be a valid re-genesis chain —
    // this test drives `replace_trust` at the *store* level, below `put_trust`'s
    // chain-of-custody validation — only different from the incumbent, to exercise the
    // byte-equality precondition `compare_and_set_head` enforces. The injecting fake supplies
    // the incumbent bytes and office head that `replace_trust`'s own conditions now require, so
    // the injected write is a genuine uncontested commit rather than a forced one.
    let different_anchor = TrustAnchorDto {
        genesis: "f".repeat(64),
        enabled_at: anchor.enabled_at + 1,
        boundary: vec![],
        prior_genesis: Some(anchor.genesis.clone()),
        adopts: Some(office_head.clone()),
    }
    .to_anchor();

    let attack = Head::new(
        MemoryObjectStore::new(),
        AnchorMovesAfterTheSnapshot {
            inner: store.clone(),
            move_to: different_anchor,
            fired: Cell::new(false),
        },
    );
    upload_all(&attack, &latest);

    let err = attack
        .ref_update(&Principal::Static, "main", &RefUpdateRequest {
            old_head: Some(main_v1.clone()),
            new_head: main_v2.clone(),
            tombstone: None,
        })
        .expect_err("the trust anchor moved between the audit and the commit");
    assert_eq!(err.status, Status::Conflict);
    assert!(
        err.message.to_lowercase().contains("anchor"),
        "the refusal must name the trust anchor: {}",
        err.message
    );
    assert!(
        !err.message.contains("The pallet moved"),
        "an anchor-moved refusal must not be indistinguishable from an ordinary moved-pallet \
        conflict: {}",
        err.message
    );
}

/// Falsifier 2 (over-tightened): a pallet the transaction never names moving concurrently must
/// never refuse this commit — the precondition is scoped to exactly three inputs (the target
/// pallet, the office pallet, the trust anchor), never to "nothing else in the warehouse
/// moved" (FORK-95 design memo, "The movable inputs: exactly three").
#[test]
fn a_concurrently_moved_unrelated_pallet_does_not_refuse_the_commit() {
    let area = Area::new("unrelated-pallet-races");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "signed one"]);

    let main_v1 = harvest(&area.path("wh")).head_of("main").expect("main v1");

    area.write_file("wh/app.txt", "v2\n");
    area.forklift("wh", &["load", "app.txt"]);
    area.forklift("wh", &["stack", "signed two"]);

    let latest = harvest(&area.path("wh"));
    let anchor = latest.trust.clone().expect("trust established");
    let office_head = latest.head_of(&format!("@{}", OFFICE_PALLET_NAME)).expect("office head");
    let main_v2 = latest.head_of("main").expect("main v2");

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &latest);
    head.put_trust(&Principal::Static, &anchor).expect("put trust");
    head.ref_update(&Principal::Static, &format!("@{}", OFFICE_PALLET_NAME), &RefUpdateRequest {
        old_head: None,
        new_head: office_head.clone(),
        tombstone: None,
    })
    .expect("lift office");
    head.ref_update(&Principal::Static, "main", &RefUpdateRequest { old_head: None, new_head: main_v1.clone(), tombstone: None })
        .expect("lift main v1");

    // A pallet this transaction never names moves concurrently. Planting it is itself a
    // commit through the same upgraded `compare_and_set_head`, so — like any other commit on
    // a trusted warehouse — it must supply the office head and anchor *currently* in the
    // store to succeed; that is the general rule this test exists to distinguish from
    // ("everything must stay still") rather than a special case of it.
    let anchor_bytes =
        head.refs.get_trust().expect("read anchor").map(|(_, bytes)| bytes).expect("trust");
    assert_eq!(
        head.refs
            .compare_and_set_head(
                pallet_utils::PalletNamespace::User,
                "unrelated",
                ExpectedHead::Unborn,
                &"a".repeat(64),
                OfficePrecondition::At(&office_head),
                Some(&anchor_bytes),
                &request_token(),
            )
            .expect("plant an unrelated pallet"),
        CasOutcome::Committed
    );

    head.ref_update(&Principal::Static, "main", &RefUpdateRequest {
        old_head: Some(main_v1),
        new_head: main_v2.clone(),
        tombstone: None,
    })
    .expect("an unrelated pallet moving must never refuse this commit");

    assert_eq!(head.handshake().expect("handshake").pallets.get("main"), Some(&main_v2));
}

// ---------------------------------------------------------------------------------------------
// FORK-95 slice 4: the trust anchor's own write is conditional too.
//
// `put_trust`'s re-genesis path is a read-validate-write — read the incumbent anchor, test that
// the new one names it as `prior_genesis`, read the office head, test that the new one `adopts`
// it, write — and validating is not holding. Until this slice the write was a bare unconditional
// `put_item`, so both values could move underneath the tests that had just passed.
//
// These tests reuse the two fakes the ref-update falsifiers already use, because `put_trust`
// makes exactly the reads they fire on: `get_trust` for the incumbent, `get_head(@office)` for
// the `adopts` check. That reuse is the point — the same injected interleaving that the commit
// refuses must now be refused here.
// ---------------------------------------------------------------------------------------------

/// Seed a store with an established anchor and an office head, returning both plus the anchor's
/// stored bytes. No fixture warehouse: this path never touches the object store, and driving it
/// through a real audit would only add ways for the test to fail for other reasons.
fn seeded_trust(office_head: &str) -> (Arc<MemoryRefStore>, TrustAnchorDto, String) {
    let store = Arc::new(MemoryRefStore::new());

    let anchor_v1 = TrustAnchorDto {
        genesis: "a".repeat(64),
        enabled_at: 1_780_000_000,
        boundary: vec![],
        prior_genesis: None,
        adopts: None,
    };

    assert_eq!(
        store.put_trust_if_absent(&anchor_v1.to_anchor()).expect("plant the incumbent anchor"),
        TrustOutcome::Established
    );

    let (_decoded, bytes) = store.get_trust().expect("read trust").expect("present");

    assert_eq!(
        store
            .compare_and_set_head(
                pallet_utils::PalletNamespace::Meta,
                OFFICE_PALLET_NAME,
                ExpectedHead::Unborn,
                office_head,
                OfficePrecondition::NotConsumed,
                Some(&bytes),
                &request_token(),
            )
            .expect("lift the office pallet"),
        CasOutcome::Committed
    );

    (store, anchor_v1, bytes)
}

/// A re-genesis anchor succeeding `prior` and adopting `office_head`, distinguished by `genesis`
/// so two racing re-geneses are tellable apart.
fn re_genesis(prior: &TrustAnchorDto, office_head: &str, genesis: char) -> TrustAnchorDto {
    TrustAnchorDto {
        genesis: genesis.to_string().repeat(64),
        enabled_at: prior.enabled_at + 1,
        boundary: vec![],
        prior_genesis: Some(prior.genesis.clone()),
        adopts: Some(office_head.to_string()),
    }
}

/// Falsifier 1 (reverted direction): two re-geneses reading the same incumbent must not both
/// land. Before this slice they did — each passed the `prior_genesis` test against the same value
/// and then wrote unconditionally, so one genesis silently overwrote another through a door this
/// code calls one-way.
///
/// Mutate `DynamoRefStore`/`MemoryRefStore::replace_trust` back to an unconditional write and
/// this returns `TrustResult::Established` (`201`) instead of the `409` asserted here — the
/// loser's anchor wins, and nothing anywhere reports it.
#[test]
fn a_concurrent_re_genesis_refuses_the_second_one() {
    let office_head = "0".repeat(64);
    let (store, anchor_v1, _bytes) = seeded_trust(&office_head);

    let theirs = re_genesis(&anchor_v1, &office_head, 'b');
    let mine = re_genesis(&anchor_v1, &office_head, 'c');

    // Their re-genesis lands the instant this request reads the incumbent — after the read that
    // feeds the chain-of-custody test, before the write that acts on it.
    let head = Head::new(
        MemoryObjectStore::new(),
        AnchorMovesAfterTheSnapshot {
            inner: store.clone(),
            move_to: theirs.to_anchor(),
            fired: Cell::new(false),
        },
    );

    let err = head
        .put_trust(&Principal::Static, &mine)
        .expect_err("the incumbent anchor moved between the validation and the write");

    assert_eq!(err.status, Status::Conflict);

    // The winner's anchor is the one that stands. Asserting the *state*, not just the status: a
    // refusal that still wrote would be the very defect this test exists for, and a status
    // assertion alone cannot see it.
    let (survivor, _) = store.get_trust().expect("read trust").expect("present");
    assert_eq!(
        survivor.genesis, theirs.genesis,
        "the refused re-genesis must not have overwritten the one that won the race"
    );
}

/// Falsifier 2 (reverted direction): the office head moving between the `adopts` test and the
/// write must refuse. That write would plant an anchor dropping precisely the history the
/// `adopts` test exists to protect, and it is a `422` — the same refusal, from the same helper,
/// that the read-side test gives for the same mismatch.
#[test]
fn a_concurrent_office_move_refuses_the_re_genesis() {
    let office_head = "0".repeat(64);
    let (store, anchor_v1, _bytes) = seeded_trust(&office_head);

    let mine = re_genesis(&anchor_v1, &office_head, 'c');

    let head = Head::new(
        MemoryObjectStore::new(),
        OfficeMovesAfterTheSnapshot {
            inner: store.clone(),
            move_to: "9".repeat(64),
            fired: Cell::new(false),
        },
    );

    let err = head
        .put_trust(&Principal::Static, &mine)
        .expect_err("the office head moved between the adopts check and the write");

    assert_eq!(err.status, Status::Unprocessable);
    assert!(
        err.message.contains("adopts"),
        "the refusal must name the adopts mismatch: {}",
        err.message
    );

    let (survivor, _) = store.get_trust().expect("read trust").expect("present");
    assert_eq!(
        survivor.genesis, anchor_v1.genesis,
        "a refused re-genesis must leave the incumbent anchor in place"
    );
}

/// Falsifier 3a (over-tightened direction), and the case the other over-tightened tests did not
/// reach: losing the race to a *identical* anchor is not a conflict.
///
/// `PUT /v1/trust` documents itself as idempotent for an identical anchor
/// (`docs/format/REMOTE_PROTOCOL.md`). `put_trust`'s read-side `existing_dto == *anchor` check
/// honours that when the anchor is already identical at the time of the read. Conditioning the
/// write re-opened it for the window: with no `AlreadyIdentical` outcome, an identical anchor
/// landing *inside* the window fails the anchor precondition and the client is told `409 "trust
/// cannot be replaced silently"` for a request whose desired state already holds.
///
/// Delete `TrustWriteOutcome::AlreadyIdentical`'s branch in either store and this reds with a
/// `409` — while every other test in this file stays green, which is why the gap survived the
/// first round of both-direction falsification.
#[test]
fn a_concurrent_identical_re_genesis_is_idempotent_not_a_conflict() {
    let office_head = "0".repeat(64);
    let (store, anchor_v1, _bytes) = seeded_trust(&office_head);

    let mine = re_genesis(&anchor_v1, &office_head, 'c');

    // Someone else plants exactly the anchor this request is asking for, inside its window.
    let head = Head::new(
        MemoryObjectStore::new(),
        AnchorMovesAfterTheSnapshot {
            inner: store.clone(),
            move_to: mine.to_anchor(),
            fired: Cell::new(false),
        },
    );

    assert_eq!(
        head.put_trust(&Principal::Static, &mine).expect("the desired state holds; this is not a conflict"),
        TrustResult::Unchanged
    );

    let (survivor, _) = store.get_trust().expect("read trust").expect("present");
    assert_eq!(survivor.genesis, mine.genesis);
}

/// Falsifier 3 (over-tightened direction): a re-genesis whose two inputs both hold must still
/// commit. Without this, an implementation that refuses every re-genesis passes both tests above
/// — the failure mode a conditional write makes easy, since "refuse more" is exactly what the
/// conditions do.
#[test]
fn an_uncontended_re_genesis_still_commits() {
    let office_head = "0".repeat(64);
    let (store, anchor_v1, _bytes) = seeded_trust(&office_head);

    let mine = re_genesis(&anchor_v1, &office_head, 'c');

    let head = Head::new(MemoryObjectStore::new(), SharedMemoryRefs(store.clone()));

    assert_eq!(
        head.put_trust(&Principal::Static, &mine).expect("nothing moved; the re-genesis must land"),
        TrustResult::Established
    );

    let (survivor, _) = store.get_trust().expect("read trust").expect("present");
    assert_eq!(survivor.genesis, mine.genesis);
}

/// Falsifier 4 (over-tightened direction): a pallet the anchor write never names moving
/// concurrently must not refuse it. This transaction conditions on exactly two inputs — the
/// incumbent anchor and the office head — never on "nothing else in the warehouse moved".
#[test]
fn a_concurrently_moved_unrelated_pallet_does_not_refuse_the_re_genesis() {
    let office_head = "0".repeat(64);
    let (store, anchor_v1, bytes) = seeded_trust(&office_head);

    // An ordinary pallet is lifted while the re-genesis is in flight. The anchor write names no
    // pallet item at all, so this must be invisible to it.
    assert_eq!(
        store
            .compare_and_set_head(
                pallet_utils::PalletNamespace::User,
                "main",
                ExpectedHead::Unborn,
                &"e".repeat(64),
                OfficePrecondition::NotConsumed,
                Some(&bytes),
                &request_token(),
            )
            .expect("lift an unrelated pallet"),
        CasOutcome::Committed
    );

    let mine = re_genesis(&anchor_v1, &office_head, 'c');
    let head = Head::new(MemoryObjectStore::new(), SharedMemoryRefs(store.clone()));

    assert_eq!(
        head.put_trust(&Principal::Static, &mine).expect("an unrelated pallet's move must not refuse the re-genesis"),
        TrustResult::Established
    );
}

/// Wraps a shared [`MemoryRefStore`] and answers the anchor write — and only the anchor write —
/// as transient contention, the way DynamoDB's `TransactionConflict` reaches this crate.
///
/// `MemoryRefStore` cannot produce that outcome itself (its single mutex makes every write either
/// succeed or lose to a real, attributable mismatch), so the status it maps to has no other way
/// to be pinned. Separate from `TransientOnCommit` rather than folded into it: that fake pins the
/// *commit's* transient path, and one fake answering both would let either test pass on the
/// other's code path. Like it, the first `refusals` attempts are transient and later ones are
/// forwarded, and every attempt's token is recorded.
struct TransientOnAnchorWrite {
    inner: Arc<MemoryRefStore>,
    refusals: usize,
    refusal: TrustWriteOutcome,
    /// What the first attempt answers instead of `refusal`, when set.
    first: Option<TrustWriteOutcome>,
    /// Fail every attempt after the first with a fault instead.
    fault: bool,
    tokens: std::sync::Mutex<Vec<String>>,
}

impl TransientOnAnchorWrite {
    fn new(inner: Arc<MemoryRefStore>, refusals: usize) -> TransientOnAnchorWrite {
        TransientOnAnchorWrite {
            inner,
            refusals,
            refusal: TrustWriteOutcome::Transient(TransientCause::Contended),
            first: None,
            fault: false,
            tokens: Default::default(),
        }
    }

    fn tokens(&self) -> Vec<String> {
        self.tokens.lock().unwrap().clone()
    }
}

impl RefStore for TransientOnAnchorWrite {
    fn get_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
    ) -> Result<RefHead, String> {
        self.inner.get_head(namespace, name)
    }

    fn compare_and_set_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        expected: ExpectedHead<'_>,
        new: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.compare_and_set_head(namespace, name, expected, new, office_head, anchor, request_token)
    }

    fn delete_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        old: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.delete_head(namespace, name, old, office_head, anchor, request_token)
    }
    fn list_refs(&self) -> Result<RefListing, String> {
        self.inner.list_refs()
    }

    fn default_pallet(&self) -> Result<String, String> {
        self.inner.default_pallet()
    }

    fn get_trust(&self) -> Result<Option<(office_utils::TrustAnchor, String)>, String> {
        self.inner.get_trust()
    }

    fn put_trust_if_absent(
        &self,
        anchor: &office_utils::TrustAnchor,
    ) -> Result<TrustOutcome, String> {
        self.inner.put_trust_if_absent(anchor)
    }

    fn replace_trust(
        &self,
        anchor: &office_utils::TrustAnchor,
        expected_anchor: &str,
        office_head: Option<&str>,
        request_token: &str,
    ) -> Result<TrustWriteOutcome, String> {
        let attempt = {
            let mut tokens = self.tokens.lock().unwrap();
            tokens.push(request_token.to_string());
            tokens.len()
        };

        if attempt == 1 {
            if let Some(first) = &self.first {
                return Ok(first.clone());
            }
        }

        if attempt > 1 && self.fault {
            return Err("a fault".to_string());
        }

        if attempt <= self.refusals {
            return Ok(self.refusal.clone());
        }

        self.inner.replace_trust(anchor, expected_anchor, office_head, request_token)
    }
}

/// Wraps a shared [`MemoryRefStore`] and answers the anchor write with a chosen
/// `AnchorMoved { current }`, so the head's three-way refusal can be driven into each of its
/// arms.
///
/// Two of the three are not reachable through a real interleaving today — nothing deletes the
/// anchor, and nothing writes undecodable bytes — but both are representable, `store.rs` says so
/// explicitly, and the head branches on them with *different remedies*. A branch that exists only
/// in prose is the shape this PR has already been caught by four times.
struct AnchorMovedOnAnchorWrite {
    inner: Arc<MemoryRefStore>,
    current: Option<String>,
}

impl RefStore for AnchorMovedOnAnchorWrite {
    fn get_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
    ) -> Result<RefHead, String> {
        self.inner.get_head(namespace, name)
    }

    fn compare_and_set_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        expected: ExpectedHead<'_>,
        new: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.compare_and_set_head(namespace, name, expected, new, office_head, anchor, request_token)
    }

    fn delete_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        old: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.delete_head(namespace, name, old, office_head, anchor, request_token)
    }
    fn list_refs(&self) -> Result<RefListing, String> {
        self.inner.list_refs()
    }

    fn default_pallet(&self) -> Result<String, String> {
        self.inner.default_pallet()
    }

    fn get_trust(&self) -> Result<Option<(office_utils::TrustAnchor, String)>, String> {
        self.inner.get_trust()
    }

    fn put_trust_if_absent(
        &self,
        anchor: &office_utils::TrustAnchor,
    ) -> Result<TrustOutcome, String> {
        self.inner.put_trust_if_absent(anchor)
    }

    fn replace_trust(
        &self,
        _anchor: &office_utils::TrustAnchor,
        _expected_anchor: &str,
        _office_head: Option<&str>,
        _request_token: &str,
    ) -> Result<TrustWriteOutcome, String> {
        Ok(TrustWriteOutcome::AnchorMoved { current: self.current.clone() })
    }
}

/// PR #117 round 3, finding 4: an anchor that is *absent* must not be reported as one that is
/// *unreadable*.
///
/// Both states arrive as `AnchorMoved { current: None }`, and an earlier revision collapsed them
/// into "its genesis is now (unreadable)" — which asserts to the operator that an incumbent
/// exists and is corrupt, and points at re-running the re-genesis. For an absent anchor the
/// correct remedy is the opposite: there is nothing to re-read, and trust must be established.
#[test]
fn an_absent_incumbent_is_not_reported_as_an_unreadable_one() {
    let office_head = "0".repeat(64);
    let (store, anchor_v1, _bytes) = seeded_trust(&office_head);

    let mine = re_genesis(&anchor_v1, &office_head, 'c');
    let head = Head::new(
        MemoryObjectStore::new(),
        AnchorMovedOnAnchorWrite { inner: store, current: None },
    );

    let err = head.put_trust(&Principal::Static, &mine).expect_err("the incumbent is gone");

    assert_eq!(err.status, Status::Conflict);
    assert!(
        err.message.contains("no anchor is present"),
        "the refusal must say the anchor is absent: {}",
        err.message
    );

    // The *remedy*, not just the description — which is what the defect was actually about, and
    // what the first version of this test failed to pin. A message describing the absent state
    // correctly while still carrying the replaced state's "re-run the re-genesis" advice is the
    // exact wrong-remedy bug, and it passed every assertion above.
    assert!(
        err.message.contains("Establish trust"),
        "the refusal must give the remedy for an absent anchor: {}",
        err.message
    );
    // Named as the replaced case's specific advice rather than the bare word "re-run": the
    // correct absent-anchor message says "Establish trust rather than re-running the
    // re-genesis", so a substring check on "re-run" rejects the right answer.
    assert!(
        !err.message.contains("re-run the re-genesis against the new incumbent"),
        "an absent anchor must not be given the replaced anchor's remedy — there is nothing \
        to re-read: {}",
        err.message
    );
}

/// PR #117 round 4, finding 4: an incumbent whose bytes will not decode must not be told to
/// re-run either — that advice provably fails.
///
/// Both stores' `get_trust` returns `Err` on undecodable bytes, so `put_trust` maps a re-run to a
/// `500` and never reaches the anchor write again. An earlier revision gave this state the
/// replaced state's tail, promising a remedy the code cannot deliver. Three states, three
/// remedies — this is the third.
#[test]
fn an_undecodable_incumbent_is_not_told_to_re_run() {
    let office_head = "0".repeat(64);
    let (store, anchor_v1, _bytes) = seeded_trust(&office_head);

    let mine = re_genesis(&anchor_v1, &office_head, 'c');
    let head = Head::new(
        MemoryObjectStore::new(),
        AnchorMovedOnAnchorWrite {
            inner: store,
            current: Some("{not valid json".to_string()),
        },
    );

    let err = head.put_trust(&Principal::Static, &mine).expect_err("the incumbent cannot be decoded");

    assert_eq!(err.status, Status::Conflict);
    assert!(
        err.message.contains("cannot be decoded"),
        "the refusal must name the decode failure: {}",
        err.message
    );
    assert!(
        !err.message.contains("re-run the re-genesis against the new incumbent"),
        "an undecodable anchor must not be given the replaced anchor's remedy — reading it \
        fails outright: {}",
        err.message
    );
    assert!(
        !err.message.contains("no anchor is present"),
        "an undecodable anchor is present; it must not be reported as absent: {}",
        err.message
    );
}

/// An anchor write that stays transient is a `503`, not the `500` every refusal on this path
/// used to be. It establishes nothing about whether either input moved, so it must not wear a
/// `409` or a `422` either — both of those tell a client something specific and false. It is
/// answered only once the same budget the commit has is spent: four attempts, one token.
#[test]
fn an_anchor_write_that_stays_transient_is_attempted_four_times_then_answers_503() {
    let office_head = "0".repeat(64);
    let (store, anchor_v1, _bytes) = seeded_trust(&office_head);

    let mine = re_genesis(&anchor_v1, &office_head, 'c');
    let head = Head::new(MemoryObjectStore::new(), TransientOnAnchorWrite::new(store, usize::MAX));

    let err = head.put_trust(&Principal::Static, &mine).expect_err("transient contention");

    assert_eq!(err.status, Status::ServiceUnavailable);

    let tokens = head.refs.tokens();
    assert_eq!(tokens.len(), 4);
    assert!(tokens.iter().all(|token| *token == tokens[0]), "one token per request: {tokens:?}");
}

/// An anchor write any of whose attempts may have planted the anchor — the last, or an earlier
/// one of unknown fate followed by contention — must not answer that nothing was changed; it
/// sends the operator to read the anchor first.
#[test]
fn an_anchor_write_of_unknown_fate_answers_503_without_claiming_nothing_changed() {
    let office_head = "0".repeat(64);
    let (store, anchor_v1, _bytes) = seeded_trust(&office_head);

    let mine = re_genesis(&anchor_v1, &office_head, 'c');
    let uncertain = [
        (None, TransientCause::FateUnknown),
        (Some(TransientCause::FateUnknown), TransientCause::Contended),
    ];

    for (first, then) in uncertain {
        let head = Head::new(
            MemoryObjectStore::new(),
            TransientOnAnchorWrite {
                refusal: TrustWriteOutcome::Transient(then),
                first: first.map(TrustWriteOutcome::Transient),
                ..TransientOnAnchorWrite::new(store.clone(), usize::MAX)
            },
        );

        let err = head.put_trust(&Principal::Static, &mine).expect_err("an uncertain outcome is still a refusal");

        assert_eq!(err.status, Status::ServiceUnavailable);
        assert!(err.message.contains("may or may not have been applied"), "{}", err.message);
        assert!(!err.message.contains("nothing was changed"), "{}", err.message);
    }

    for (first, status) in
        [(TransientCause::FateUnknown, Status::ServiceUnavailable), (TransientCause::Contended, Status::Internal)]
    {
        let head = Head::new(
            MemoryObjectStore::new(),
            TransientOnAnchorWrite {
                first: Some(TrustWriteOutcome::Transient(first)),
                fault: true,
                ..TransientOnAnchorWrite::new(store.clone(), usize::MAX)
            },
        );
        let err = head.put_trust(&Principal::Static, &mine).expect_err("a fault is a refusal");

        assert_eq!(err.status, status, "{}", err.message);
        assert_eq!(err.message.contains("may or may not have been applied"), status == Status::ServiceUnavailable, "{}", err.message);
    }

    let head = Head::new(MemoryObjectStore::new(), TransientOnAnchorWrite::new(store, usize::MAX));
    let err = head.put_trust(&Principal::Static, &mine).expect_err("contention outlasting the budget is a refusal");

    assert!(err.message.contains("nothing was changed"), "{}", err.message);
}

/// The anchor write has the commit's recovery, not a lesser one: refused once for contention, it
/// is re-issued and the re-genesis lands.
#[test]
fn an_anchor_write_refused_transiently_once_is_reissued_and_lands() {
    let office_head = "0".repeat(64);
    let (store, anchor_v1, _bytes) = seeded_trust(&office_head);

    let mine = re_genesis(&anchor_v1, &office_head, 'c');
    let head = Head::new(MemoryObjectStore::new(), TransientOnAnchorWrite::new(store.clone(), 1));

    assert_eq!(head.put_trust(&Principal::Static, &mine).expect("the re-issue lands"), TrustResult::Established);
    assert_eq!(head.refs.tokens().len(), 2);
    let (planted, _) = store.get_trust().unwrap().expect("an anchor");
    assert!(TrustAnchorDto::from(&planted) == mine, "the re-genesis anchor is the one planted");
}

/// A moved office head is the client's to act on; the anchor write answers it on the first
/// attempt rather than re-issuing a transaction that can only be refused again.
#[test]
fn a_moved_office_head_refuses_the_anchor_write_on_the_first_attempt() {
    let office_head = "0".repeat(64);
    let (store, anchor_v1, _bytes) = seeded_trust(&office_head);

    let mine = re_genesis(&anchor_v1, &office_head, 'c');
    let head = Head::new(
        MemoryObjectStore::new(),
        TransientOnAnchorWrite {
            refusal: TrustWriteOutcome::OfficeMoved { current: Some("1".repeat(64)) },
            ..TransientOnAnchorWrite::new(store, usize::MAX)
        },
    );

    let err = head.put_trust(&Principal::Static, &mine).expect_err("the office moved");

    assert_eq!(err.status, Status::Unprocessable);
    assert_eq!(head.refs.tokens().len(), 1);
}

/// The window check guards the anchor write as it does the commit.
#[test]
fn an_anchor_write_near_its_deadline_is_not_reissued() {
    let office_head = "0".repeat(64);
    let (store, anchor_v1, _bytes) = seeded_trust(&office_head);

    let mine = re_genesis(&anchor_v1, &office_head, 'c');
    let head = Head::new(MemoryObjectStore::new(), TransientOnAnchorWrite::new(store, usize::MAX))
        .with_deadline(std::time::SystemTime::now() + std::time::Duration::from_millis(500));

    let err = head.put_trust(&Principal::Static, &mine).expect_err("transient contention");

    assert_eq!(err.status, Status::ServiceUnavailable);
    assert_eq!(head.refs.tokens().len(), 1);
}

/// FORK-114: a first anchor arrives over an unborn office, so its `adopts` is tied to nothing
/// else — and every warehouse that adopts the anchor later fetches that history from here. One
/// naming history the head does not hold is refused; the same anchor once the history is
/// uploaded is established.
#[test]
fn a_first_anchor_adopting_history_the_head_does_not_hold_is_refused() {
    let area = Area::new("first-anchor-adopts");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "signed"]);

    let harvest = harvest(&area.path("wh"));
    let office_head = harvest.head_of(&format!("@{}", OFFICE_PALLET_NAME)).expect("office head");
    let anchor = TrustAnchorDto {
        genesis: "e".repeat(64),
        adopts: Some(office_head.clone()),
        ..harvest.trust.clone().expect("trust")
    };

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    let err = head.put_trust(&Principal::Static, &anchor).expect_err("adopted history not held");
    assert_eq!(err.status, Status::Unprocessable);
    assert!(err.message.contains(&office_head), "the refusal must name the pin: {}", err.message);

    // The head parcel alone is not the history: its tree and records are still missing.
    head.object_put(&Principal::Static, None, &office_head, &harvest.objects[&office_head]).expect("upload head");
    let err = head.put_trust(&Principal::Static, &anchor).expect_err("adopted history incomplete");
    assert_eq!(err.status, Status::Unprocessable, "{}", err.message);
    assert!(err.message.contains(&office_head), "the refusal must name the pin: {}", err.message);

    upload_all(&head, &harvest);
    head.put_trust(&Principal::Static, &anchor).expect("adopted history held");
}

// ---------------------------------------------------------------------------------------
// Pallet deletes.
// ---------------------------------------------------------------------------------------

#[path = "support/tombstones.rs"]
mod tombstones;

/// The tombstone contract (`support/tombstones.rs`) against the in-memory store, so the fake the
/// rest of this suite runs on keeps the item state the DynamoDB store writes.
#[test]
fn the_memory_store_upholds_the_tombstone_contract() {
    tombstones::upholds_the_tombstone_contract(&MemoryRefStore::new());
}

/// An untrusted warehouse with `main` at one parcel and `feature` two parcels past it, every
/// object harvested. Returns the harvest, main's head, and feature's first and second heads.
fn feature_warehouse(area: &Area) -> (Harvest, String, String, String) {
    prepare(area, "wh");
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "first"]);
    area.forklift("wh", &["palletize", "feature"]);
    area.write_file("wh/app.txt", "v2\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "second"]);
    let feature_v2 = harvest(&area.path("wh")).head_of("feature").expect("feature v2");
    area.write_file("wh/app.txt", "v3\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "third"]);

    let harvested = harvest(&area.path("wh"));
    let main_head = harvested.head_of("main").expect("main head");
    let feature_v3 = harvested.head_of("feature").expect("feature v3");
    (harvested, main_head, feature_v2, feature_v3)
}

/// A head over `refs` holding every harvested object, with `main` and `feature` at `feature_v2`
/// lifted.
fn lifted_feature<R: RefStore>(refs: R, harvest: &Harvest, main_head: &str, feature_v2: &str) -> Head<MemoryObjectStore, R> {
    let head = Head::new(MemoryObjectStore::new(), refs);
    upload_all(&head, harvest);
    for (pallet, new_head) in [("main", main_head), ("feature", feature_v2)] {
        let lift = RefUpdateRequest { old_head: None, new_head: new_head.to_string(), tombstone: None };
        head.ref_update(&Principal::Static, pallet, &lift).expect("lift");
    }
    head
}

/// The refusal code and next step a head error carries on the wire.
fn code_and_next_step(message: &str) -> (String, String) {
    let (code, _, next_step) = forklift_core::error::deframe(message).expect("a classified refusal");
    (code.to_string(), next_step.to_string())
}

/// A delete leaves a tombstone: the handshake names it in place of the head, holds walks from it,
/// a retry at it answers `200` with nothing written, and a restore at its head clears it.
#[test]
fn a_delete_leaves_a_tombstone_the_handshake_and_holds_read() {
    let area = Area::new("delete");
    let (harvest, main_head, feature_v2, feature_v3) = feature_warehouse(&area);
    let head = lifted_feature(MemoryRefStore::new(), &harvest, &main_head, &feature_v2);

    head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect("delete feature");

    let info = head.handshake().expect("handshake");
    assert_eq!(info.pallets.get("feature"), None);
    assert_eq!(info.tombstone("feature"), Some(&feature_v2));
    assert_eq!(info.pallets.get("main"), Some(&main_head), "only the named pallet");

    let holds = head.holds("feature", &[feature_v2.clone(), main_head.clone(), feature_v3.clone()]).expect("holds");
    assert_eq!(holds[&feature_v2], HoldAnswer::Held);
    assert_eq!(holds[&main_head], HoldAnswer::Held, "the tombstone's history");
    assert_eq!(holds[&feature_v3], HoldAnswer::NotHeld);

    head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect("a retry at the tombstone");
    assert_eq!(head.handshake().expect("handshake").tombstone("feature"), Some(&feature_v2));

    // A restore is the acknowledged re-creation at the tombstone's own head.
    let restore = RefUpdateRequest { old_head: None, new_head: feature_v2.clone(), tombstone: Some(feature_v2.clone()) };
    head.ref_update(&Principal::Static, "feature", &restore).expect("restore feature");
    let info = head.handshake().expect("handshake");
    assert_eq!((info.pallets.get("feature"), info.tombstone("feature")), (Some(&feature_v2), None));
}

/// Each refusal of a delete, in the direct head's codes, and nothing deleted by any of them: a
/// malformed head and a meta pallet are `422`; a stale head is `remote_ahead`; the default pallet
/// is a `409`; a pallet deleted at another head names the deletion and the restore, and no
/// tombstone drop, which this head does not have.
#[test]
fn a_delete_names_what_stands_in_its_way() {
    let area = Area::new("delete-refusals");
    let (harvest, main_head, feature_v2, feature_v3) = feature_warehouse(&area);
    let head = lifted_feature(MemoryRefStore::new(), &harvest, &main_head, &feature_v2);

    let err = head.delete_pallet(&Principal::Static, "feature", "not-a-hash").expect_err("malformed head");
    assert_eq!(err.status, Status::Unprocessable, "{}", err.message);

    let err = head.delete_pallet(&Principal::Static, "@kv", &feature_v2).expect_err("a meta pallet");
    assert_eq!(err.status, Status::Unprocessable, "{}", err.message);
    assert!(err.message.contains("tracked metadata"), "{}", err.message);

    let err = head.delete_pallet(&Principal::Static, "feature", &main_head).expect_err("stale head");
    assert_eq!(err.status, Status::Conflict);
    assert_eq!(code_and_next_step(&err.message).0, "remote_ahead", "{}", err.message);

    let err = head.delete_pallet(&Principal::Static, "main", &main_head).expect_err("the default pallet");
    assert_eq!(err.status, Status::Conflict);
    assert!(err.message.contains("default pallet"), "{}", err.message);

    let info = head.handshake().expect("handshake");
    assert_eq!((info.pallets.get("main"), info.pallets.get("feature")), (Some(&main_head), Some(&feature_v2)));

    head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect("delete feature");
    let err = head.delete_pallet(&Principal::Static, "feature", &feature_v3).expect_err("deleted at another head");
    assert_eq!(err.status, Status::Conflict);
    assert_eq!(
        code_and_next_step(&err.message),
        ("pallet_deleted_on_remote".to_string(), "forklift palletize --restore feature --remote".to_string())
    );
    assert!(err.message.contains(&feature_v2), "names the tombstone: {}", err.message);
    assert!(!err.message.contains("tombstone drop"), "this head has no drop: {}", err.message);
}

/// A deleted pallet moves again only through an acknowledged creation from its own history: an
/// update from the old head, a creation that does not acknowledge the tombstone or acknowledges
/// another head, and an acknowledged creation from history that does not descend from it are all
/// `pallet_deleted_on_remote`; the acknowledged descendant lands and clears the tombstone.
#[test]
fn a_deleted_pallet_is_re_created_only_from_its_history_with_acknowledgement() {
    let area = Area::new("delete-recreate");
    let (harvest, main_head, feature_v2, feature_v3) = feature_warehouse(&area);
    let head = lifted_feature(MemoryRefStore::new(), &harvest, &main_head, &feature_v2);
    head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect("delete feature");

    let refused = [
        ("an update from the old head", Some(feature_v2.clone()), feature_v3.clone(), None, "deleted on this remote"),
        ("an unacknowledged creation", None, feature_v3.clone(), None, "acknowledge the deletion"),
        ("another head acknowledged", None, feature_v3.clone(), Some(main_head.clone()), "acknowledge the deletion"),
        ("a non-descendant", None, main_head.clone(), Some(feature_v2.clone()), "does not descend"),
    ];
    for (case, old_head, new_head, tombstone, says) in refused {
        let err = head
            .ref_update(&Principal::Static, "feature", &RefUpdateRequest { old_head, new_head, tombstone })
            .expect_err(case);
        assert_eq!(err.status, Status::Conflict, "{}: {}", case, err.message);
        assert_eq!(code_and_next_step(&err.message).0, "pallet_deleted_on_remote", "{}: {}", case, err.message);
        assert!(err.message.contains(says), "{}: {}", case, err.message);
        assert!(!err.message.contains("tombstone drop"), "{}: {}", case, err.message);
    }
    assert_eq!(head.handshake().expect("handshake").tombstone("feature"), Some(&feature_v2), "nothing moved");

    let recreate = RefUpdateRequest { old_head: None, new_head: feature_v3.clone(), tombstone: Some(feature_v2.clone()) };
    head.ref_update(&Principal::Static, "feature", &recreate).expect("an acknowledged descendant");
    let info = head.handshake().expect("handshake");
    assert_eq!(info.pallets.get("feature"), Some(&feature_v3));
    assert_eq!(info.tombstone("feature"), None);
}

/// An open haul naming the pallet refuses the delete, read from the `@haul` head the store holds.
#[test]
fn an_open_haul_refuses_the_delete() {
    let area = Area::new("delete-haul");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "signed"]);
    area.forklift("wh", &["palletize", "feature"]);
    area.write_file("wh/feature.txt", "x\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "feature"]);
    area.forklift("wh", &["shift", "main"]);
    area.forklift("wh", &["haul", "open", "--target", "main", "--source", "feature", "--title", "Feature"]);

    let harvest = harvest(&area.path("wh"));
    let feature_head = harvest.head_of("feature").expect("feature head");
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &harvest);
    head.put_trust(&Principal::Static, &harvest.trust.clone().expect("trust")).expect("put trust");
    for wire in [format!("@{}", OFFICE_PALLET_NAME), "main".to_string(), "feature".to_string(), "@haul".to_string()] {
        let new_head = harvest.head_of(&wire).expect("a harvested head");
        head.ref_update(&Principal::Static, &wire, &RefUpdateRequest { old_head: None, new_head, tombstone: None })
            .expect("lift");
    }

    let err = head.delete_pallet(&Principal::Static, "feature", &feature_head).expect_err("an open haul names it");
    assert_eq!(err.status, Status::Conflict, "{}", err.message);
    assert!(err.message.contains("Open haul"), "{}", err.message);
    assert_eq!(head.handshake().expect("handshake").pallets.get("feature"), Some(&feature_head));
}

/// A write raced against a delete, run once on the shared store.
type RacingWrite = Box<dyn FnOnce(&MemoryRefStore)>;

/// What [`RacesTheDelete`] does when the head's delete reaches the store.
enum DeleteRace {
    /// Run this against the shared store before the first attempt, then forward every attempt.
    Before(RefCell<Option<RacingWrite>>),
    /// Forward the delete, then answer it as transient of unknown fate, on every attempt.
    LandsThenTransient,
    /// Answer every attempt as transient of unknown fate without forwarding it.
    Transient,
    /// Run this against the shared store before the first attempt, then answer as [`Self::Transient`].
    TransientAfter(RefCell<Option<RacingWrite>>),
    /// Answer every attempt as transient for this cause without forwarding it, and fail every ref
    /// read once the first attempt was made.
    TransientThenUnreadable(TransientCause, Cell<bool>),
    /// Answer the first attempt as transient for this cause, forwarding it first when `lands`,
    /// then fail every later attempt with a fault.
    ThenFault { first: TransientCause, lands: bool, attempted: Cell<bool> },
}

/// Wraps a shared [`MemoryRefStore`], interfering with `delete_head` as its [`DeleteRace`]
/// says. Forwards every other call.
struct RacesTheDelete {
    inner: Arc<MemoryRefStore>,
    race: DeleteRace,
}

impl DeleteRace {
    fn before(race: impl FnOnce(&MemoryRefStore) + 'static) -> DeleteRace {
        DeleteRace::Before(RefCell::new(Some(Box::new(race))))
    }
}

impl RefStore for RacesTheDelete {
    fn get_head(&self, namespace: pallet_utils::PalletNamespace, name: &str) -> Result<RefHead, String> {
        match &self.race {
            DeleteRace::TransientThenUnreadable(_, attempted) if attempted.get() => Err("throttled".to_string()),
            _ => self.inner.get_head(namespace, name),
        }
    }

    fn compare_and_set_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        expected: ExpectedHead<'_>,
        new: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.compare_and_set_head(namespace, name, expected, new, office_head, anchor, request_token)
    }

    fn delete_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        old: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        match &self.race {
            DeleteRace::Before(race) => {
                if let Some(race) = race.borrow_mut().take() {
                    race(&self.inner);
                }
                self.inner.delete_head(namespace, name, old, office_head, anchor, request_token)
            }
            DeleteRace::LandsThenTransient => {
                self.inner.delete_head(namespace, name, old, office_head, anchor, request_token)?;
                Ok(CasOutcome::Transient(TransientCause::FateUnknown))
            }
            DeleteRace::Transient => Ok(CasOutcome::Transient(TransientCause::FateUnknown)),
            DeleteRace::ThenFault { attempted, .. } if attempted.replace(true) => Err("a fault".to_string()),
            DeleteRace::ThenFault { first, lands, .. } => {
                if *lands {
                    self.inner.delete_head(namespace, name, old, office_head, anchor, request_token)?;
                }
                Ok(CasOutcome::Transient(*first))
            }
            DeleteRace::TransientThenUnreadable(cause, attempted) => {
                attempted.set(true);
                Ok(CasOutcome::Transient(*cause))
            }
            DeleteRace::TransientAfter(race) => {
                if let Some(race) = race.borrow_mut().take() {
                    race(&self.inner);
                }
                Ok(CasOutcome::Transient(TransientCause::FateUnknown))
            }
        }
    }

    fn list_refs(&self) -> Result<RefListing, String> {
        self.inner.list_refs()
    }

    fn default_pallet(&self) -> Result<String, String> {
        self.inner.default_pallet()
    }

    fn get_trust(&self) -> Result<Option<(office_utils::TrustAnchor, String)>, String> {
        self.inner.get_trust()
    }

    fn put_trust_if_absent(&self, anchor: &office_utils::TrustAnchor) -> Result<TrustOutcome, String> {
        self.inner.put_trust_if_absent(anchor)
    }

    fn replace_trust(
        &self,
        anchor: &office_utils::TrustAnchor,
        expected_anchor: &str,
        office_head: Option<&str>,
        request_token: &str,
    ) -> Result<TrustWriteOutcome, String> {
        self.inner.replace_trust(anchor, expected_anchor, office_head, request_token)
    }
}

/// `feature` lifted at `feature_v2` in a shared store, and a head over that store that races its
/// delete as `race` says.
fn racing_head(area: &Area, race: impl FnOnce(String, String) -> DeleteRace) -> (Arc<MemoryRefStore>, Head<MemoryObjectStore, RacesTheDelete>, String, String) {
    let (harvest, main_head, feature_v2, feature_v3) = feature_warehouse(area);
    let store = Arc::new(MemoryRefStore::new());
    lifted_feature(SharedMemoryRefs(store.clone()), &harvest, &main_head, &feature_v2);

    let head = Head::new(MemoryObjectStore::new(), RacesTheDelete { inner: store.clone(), race: race(feature_v2.clone(), feature_v3.clone()) });
    upload_all(&head, &harvest);
    (store, head, feature_v2, feature_v3)
}

/// A lift of `feature` from `from` to `to`, raced against a delete.
fn racing_lift(from: String, to: String) -> RacingWrite {
    Box::new(move |store: &MemoryRefStore| {
        let outcome = store
            .compare_and_set_head(
                pallet_utils::PalletNamespace::User,
                "feature",
                ExpectedHead::At(&from),
                &to,
                OfficePrecondition::NotConsumed,
                None,
                &request_token(),
            )
            .expect("the racing lift");
        assert_eq!(outcome, CasOutcome::Committed);
    })
}

/// A lift landing between the delete's read and its commit refuses the delete as moved, and the
/// lift stands.
#[test]
fn a_lift_racing_the_delete_refuses_it_as_moved() {
    let area = Area::new("delete-races-lift");
    let (store, head, feature_v2, feature_v3) =
        racing_head(&area, |from, to| DeleteRace::Before(RefCell::new(Some(racing_lift(from, to)))));

    let err = head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect_err("the pallet moved");
    assert_eq!(err.status, Status::Conflict);
    assert_eq!(code_and_next_step(&err.message).0, "remote_ahead", "{}", err.message);
    assert_eq!(store.get_head(pallet_utils::PalletNamespace::User, "feature").expect("get"), RefHead::Live(feature_v3));
}

/// Two deletes from one head: the one whose commit finds the other's tombstone answers `200`,
/// since that tombstone is what it asked for.
#[test]
fn of_two_racing_deletes_the_loser_answers_200() {
    let area = Area::new("delete-races-delete");
    let (store, head, feature_v2, _) = racing_head(&area, |from, _| {
        DeleteRace::before(move |store: &MemoryRefStore| {
            let outcome = store
                .delete_head(pallet_utils::PalletNamespace::User, "feature", &from, OfficePrecondition::Unborn, None, &request_token())
                .expect("the racing delete");
            assert_eq!(outcome, CasOutcome::Committed);
        })
    });

    head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect("the loser");
    assert_eq!(store.get_head(pallet_utils::PalletNamespace::User, "feature").expect("get"), RefHead::Deleted(feature_v2));
}

/// A delete answered transient on every attempt, or failed after one of unknown fate, asks the
/// ref: it answers `200` when an attempt landed, `503` when none did and the pallet is still live
/// or the ref cannot be read either, and `remote_ahead` when the pallet moved meanwhile, since then
/// nothing is in doubt.
#[test]
fn a_delete_of_unknown_fate_is_settled_by_the_ref() {
    let area = Area::new("delete-transient-landed");
    let (store, head, feature_v2, _) = racing_head(&area, |_, _| DeleteRace::LandsThenTransient);
    head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect("an attempt landed");
    assert_eq!(store.get_head(pallet_utils::PalletNamespace::User, "feature").expect("get"), RefHead::Deleted(feature_v2));

    let area = Area::new("delete-transient-not-landed");
    let (store, head, feature_v2, _) = racing_head(&area, |_, _| DeleteRace::Transient);
    let err = head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect_err("nothing landed");
    assert_eq!(err.status, Status::ServiceUnavailable, "{}", err.message);
    assert_eq!(store.get_head(pallet_utils::PalletNamespace::User, "feature").expect("get"), RefHead::Live(feature_v2));

    // A ref that cannot be read leaves the fate where the attempts left it.
    for (cause, says) in [(TransientCause::FateUnknown, "could not be confirmed"), (TransientCause::Contended, "could not complete")] {
        let area = Area::new("delete-transient-unreadable");
        let (_, head, feature_v2, _) = racing_head(&area, |_, _| DeleteRace::TransientThenUnreadable(cause, Cell::new(false)));
        let err = head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect_err("the ref cannot be read");
        assert_eq!(err.status, Status::ServiceUnavailable, "{}", err.message);
        assert!(err.message.contains(says), "{}", err.message);
        assert!(err.message.contains("throttled"), "the read's own fault is named: {}", err.message);
    }

    // A fault on the re-issue does not hide an attempt of unknown fate before it: the ref decides
    // whether it landed. After contention the fault is the answer.
    let fault = |first, lands| move |_: String, _: String| DeleteRace::ThenFault { first, lands, attempted: Cell::new(false) };

    let area = Area::new("delete-lands-then-fault");
    let (_, head, feature_v2, _) = racing_head(&area, fault(TransientCause::FateUnknown, true));
    head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect("the first attempt landed");

    let area = Area::new("delete-unknown-then-fault");
    let (_, head, feature_v2, _) = racing_head(&area, fault(TransientCause::FateUnknown, false));
    let err = head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect_err("nothing landed");
    assert_eq!(err.status, Status::ServiceUnavailable, "{}", err.message);
    assert!(err.message.contains("could not be confirmed"), "{}", err.message);
    assert!(err.message.contains("before deleting it again"), "read first, never resend blind: {}", err.message);

    let area = Area::new("delete-contended-then-fault");
    let (_, head, feature_v2, _) = racing_head(&area, fault(TransientCause::Contended, false));
    let err = head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect_err("the fault");
    assert_eq!(err.status, Status::Internal, "{}", err.message);

    let area = Area::new("delete-transient-moved");
    let (_, head, feature_v2, _) =
        racing_head(&area, |from, to| DeleteRace::TransientAfter(RefCell::new(Some(racing_lift(from, to)))));
    let err = head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect_err("the pallet moved");
    assert_eq!(err.status, Status::Conflict, "{}", err.message);
    assert_eq!(code_and_next_step(&err.message).0, "remote_ahead", "{}", err.message);
}

/// Enroll `identifier` in `dir`'s office with `role_args` (`office role`'s arguments after the
/// identifier), from a key generated under that identifier, then switch back to the admin.
fn enroll(area: &Area, dir: &str, identifier: &str, role_args: &[&str]) {
    area.forklift(dir, &["config", "operator.identifier", identifier]);
    let keygen = area.forklift_stdout(dir, &["office", "keygen"]);
    area.forklift(dir, &["config", "operator.identifier", "tester@forklift"]);

    let admit: Vec<&str> = keygen.lines()
        .find(|line| line.trim_start().starts_with("office admit "))
        .expect("keygen prints the admit line")
        .split_whitespace()
        .collect();
    area.forklift(dir, &admit);

    let mut role = vec!["office", "role", identifier];
    role.extend(role_args);
    area.forklift(dir, &role);
}

/// A trusted warehouse whose office grants `wes@forklift` writing `feature` and makes
/// `rea@forklift` a reader, with signed `main` and `feature`; then a second office that only
/// admits another reader, changing neither's authority. Returns the harvest, both office heads
/// and feature's head.
fn rostered_feature_warehouse(area: &Area) -> (Harvest, String, String, String) {
    prepare(area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    enroll(area, "wh", "wes@forklift", &["writer", "--pallet", "feature"]);
    enroll(area, "wh", "rea@forklift", &["reader"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "signed"]);
    area.forklift("wh", &["palletize", "feature"]);
    area.write_file("wh/app.txt", "v2\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "feature"]);
    let office_wire = format!("@{}", OFFICE_PALLET_NAME);
    let first_office = harvest(&area.path("wh")).head_of(&office_wire).expect("office head");

    enroll(area, "wh", "kit@forklift", &["reader"]);
    let harvested = harvest(&area.path("wh"));
    let second_office = harvested.head_of(&office_wire).expect("office head");
    let feature_head = harvested.head_of("feature").expect("feature head");
    assert_ne!(first_office, second_office);
    (harvested, first_office, second_office, feature_head)
}

/// An `@office` lift that leaves the caller's authority as it was, landing between the delete's
/// read and its commit, refuses that commit; the delete decides again from a fresh read and
/// answers `200`, as the direct head's does.
#[test]
fn an_office_lift_racing_the_delete_is_decided_again() {
    let area = Area::new("delete-redecides");
    let (harvest, first_office, second_office, feature_head) = rostered_feature_warehouse(&area);
    let store = Arc::new(MemoryRefStore::new());
    let setup = Head::new(MemoryObjectStore::new(), SharedMemoryRefs(store.clone()));
    upload_all(&setup, &harvest);
    setup.put_trust(&Principal::Static, &harvest.trust.clone().expect("trust")).expect("put trust");
    for (wire, new_head) in [(format!("@{}", OFFICE_PALLET_NAME), &first_office), ("feature".to_string(), &feature_head)] {
        setup
            .ref_update(&Principal::Static, &wire, &RefUpdateRequest { old_head: None, new_head: new_head.clone(), tombstone: None })
            .expect("lift");
    }

    let attack = Head::new(
        MemoryObjectStore::new(),
        OfficeMovesAfterTheSnapshot { inner: store.clone(), move_to: second_office.clone(), fired: Cell::new(false) },
    );
    upload_all(&attack, &harvest);

    attack
        .delete_pallet(&Principal::Operator("wes@forklift".to_string()), "feature", &feature_head)
        .expect("decided again under the new office");
    assert_eq!(store.get_head(pallet_utils::PalletNamespace::User, "feature").expect("get"), RefHead::Deleted(feature_head));
    assert_eq!(
        store.get_head(pallet_utils::PalletNamespace::Meta, OFFICE_PALLET_NAME).expect("get"),
        RefHead::Live(second_office),
        "the office lift landed"
    );
}

/// Trust established and no office lifted: there is no roster, so every operator may delete. An
/// office lift that denies the caller, landing between the delete's read and its commit, refuses
/// the commit, because the delete conditions on the office staying unborn; decided again under
/// that office, the delete is a `403` and the pallet stays live.
#[test]
fn an_office_lift_in_the_bootstrap_window_refuses_a_delete_it_denies() {
    let area = Area::new("delete-bootstrap");
    let (harvest, first_office, _, feature_head) = rostered_feature_warehouse(&area);
    let reader = Principal::Operator("rea@forklift".to_string());

    // Trust, and `feature` written straight into the store: no head lifts a working pallet before
    // the office, but a warehouse that was served untrusted before it established trust has them.
    let bootstrapped = || {
        let store = Arc::new(MemoryRefStore::new());
        let setup = Head::new(MemoryObjectStore::new(), SharedMemoryRefs(store.clone()));
        setup.put_trust(&Principal::Static, &harvest.trust.clone().expect("trust")).expect("put trust");
        let anchor = store.get_trust().expect("read trust").map(|(_, bytes)| bytes);
        let outcome = store
            .compare_and_set_head(
                pallet_utils::PalletNamespace::User,
                "feature",
                ExpectedHead::Unborn,
                &feature_head,
                OfficePrecondition::NotConsumed,
                anchor.as_deref(),
                &request_token(),
            )
            .expect("seed feature");
        assert_eq!(outcome, CasOutcome::Committed);
        store
    };

    // The control: in the window, the reader's delete lands.
    let store = bootstrapped();
    let head = Head::new(MemoryObjectStore::new(), SharedMemoryRefs(store.clone()));
    upload_all(&head, &harvest);
    head.delete_pallet(&reader, "feature", &feature_head).expect("no roster yet");

    let store = bootstrapped();
    let attack = Head::new(
        MemoryObjectStore::new(),
        OfficeMovesAfterTheSnapshot { inner: store.clone(), move_to: first_office.clone(), fired: Cell::new(false) },
    );
    upload_all(&attack, &harvest);

    let err = attack.delete_pallet(&reader, "feature", &feature_head).expect_err("the office denies the reader");
    assert_eq!(err.status, Status::Forbidden, "{}", err.message);
    assert_eq!(store.get_head(pallet_utils::PalletNamespace::User, "feature").expect("get"), RefHead::Live(feature_head));
}
