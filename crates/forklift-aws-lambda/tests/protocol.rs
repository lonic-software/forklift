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
    ExpectedHead, ItemKey, ItemWrite, RedactionWrite, RefHead, RefListing,
    CasOutcome, ObjectStore, OfficePrecondition, RedactionsPrecondition, PromoteOutcome, PutOutcome, RefStore,
    SignatureOutcome, TransientCause, TrustOutcome, TrustWriteOutcome,
};
use forklift_aws_lambda::{AsyncBridge, BatchCut, BatchDelivery, BatchResult, Head};

use forklift_core::error::{CoreError, RefusalCode};
use forklift_core::globals::StorageRootScope;
use forklift_core::util::authority_utils::Principal;
use forklift_core::model::remote::{HoldAnswer, RefUpdateRequest, ServedRedactionState, ServedWithholdingState, TrustAnchorDto};
use forklift_core::util::office_utils::{self, OFFICE_PALLET_NAME};
use forklift_core::util::pallet_utils;
use forklift_core::util::redaction_utils::REDACTIONS_PALLET_NAME;
use forklift_core::util::{bundle_utils, file_utils, object_utils, sign_utils};

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
    area.forklift(dir, &["config", "set", "--global", "operator.identifier", "tester@forklift"]);
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
    area.forklift("wh", &["stack", "-m", "first"]);
    area.write_file("wh/readme.txt", "hello again\n");
    area.forklift("wh", &["load", "readme.txt"]);
    area.forklift("wh", &["stack", "-m", "second"]);

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
    area.forklift("wh", &["stack", "-m", "first"]);
    area.write_file("wh/readme.txt", "hello again\n");
    area.forklift("wh", &["load", "readme.txt"]);
    area.forklift("wh", &["stack", "-m", "second"]);

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
    area.forklift("wh", &["stack", "-m", "two files"]);

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
    // Revert: answer the closure refusal as plain text → no code → the client cannot retry → red.
    assert_eq!(refusal_code(&err.message), Some(RefusalCode::RemoteHistoryIncomplete), "{}", err.message);
}

/// A blob, tree or parcel built in memory and uploaded to `head` in its wire form.
fn put_object<O: ObjectStore, R: RefStore>(head: &Head<O, R>,
                                           object: forklift_core::model::object::loose_object::LooseObject) -> String {
    head.object_put(&Principal::Static, None, &object.hash, &object.content).expect("upload object");
    object.hash
}

fn gate_file<O: ObjectStore, R: RefStore>(head: &Head<O, R>, name: &str, content: &str) -> forklift_core::model::tree_item::TreeItem {
    use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
    use forklift_core::enums::dir_entry_type::DirEntryType;
    use forklift_core::model::blob::Blob;

    let hash = put_object(head, LooseObjectBuilder::build_blob(&Blob { content: content.as_bytes().to_vec() }));
    forklift_core::model::tree_item::TreeItem::new(name.to_string(), hash, DirEntryType::Normal)
}

/// An uploaded tree named `name` holding `children`.
fn gate_dir<O: ObjectStore, R: RefStore>(head: &Head<O, R>,
                                         name: &str,
                                         children: Vec<forklift_core::model::tree_item::TreeItem>)
                                         -> forklift_core::model::tree_item::TreeItem {
    use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
    use forklift_core::enums::dir_entry_type::DirEntryType;

    let mut tree = forklift_core::model::tree_item::TreeItem::new(name.to_string(), String::new(), DirEntryType::Tree);
    for child in children {
        tree.add_child(child);
    }
    tree.hash = put_object(head, LooseObjectBuilder::build_tree(&tree));
    tree
}

/// An uploaded parcel on `parents` whose root holds `children`.
fn gate_parcel<O: ObjectStore, R: RefStore>(head: &Head<O, R>,
                                            parents: &[&str],
                                            children: Vec<forklift_core::model::tree_item::TreeItem>) -> String {
    use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
    use forklift_core::model::parcel::Parcel;

    let root = gate_dir(head, "", children);
    put_object(head, LooseObjectBuilder::build_parcel(&Parcel {
        tree_hash: root.hash,
        parents: parents.iter().map(|parent| parent.to_string()).collect(),
        actions: Vec::new(),
        description: Some("gate".to_string()),
    }))
}

/// Plant `new` on user pallet `name` past the gate, as a writer from before it would have.
fn plant(refs: &MemoryRefStore, name: &str, new: &str) {
    let fence = forklift_aws_lambda::format::gate(refs).expect("format gate");
    let outcome = refs.compare_and_set_head(
        pallet_utils::PalletNamespace::User, name, ExpectedHead::Unborn, new, OfficePrecondition::NotConsumed, RedactionsPrecondition::NotConsumed, None, None,
        &fence, &request_token(),
    ).expect("plant");
    assert_eq!(outcome, CasOutcome::Committed);
}

fn user_head(head: &Head<MemoryObjectStore, MemoryRefStore>, name: &str) -> Option<String> {
    head.refs.get_head(pallet_utils::PalletNamespace::User, name).unwrap().live().map(str::to_string)
}

/// A push of a head holding the metadata folder, at the root or below it, is refused, and a clean
/// head lands beside them.
// Revert: drop the materializable-head gate from `ref_update` → the crafted heads commit.
#[test]
fn a_ref_update_to_a_head_holding_the_metadata_folder_is_refused() {
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    let at_root = gate_parcel(&head, &[], vec![gate_dir(&head, ".forklift", vec![gate_file(&head, "state", "x")])]);
    let below = gate_parcel(&head, &[], vec![gate_dir(&head, "docs", vec![gate_file(&head, "a", "1"), gate_file(&head, ".ForkLift", "x")])]);
    let clean = gate_parcel(&head, &[], vec![gate_dir(&head, "docs", vec![gate_file(&head, "a", "1")])]);

    let request = RefUpdateRequest { old_head: None, new_head: at_root, tombstone: None };
    let err = head.ref_update(&Principal::Static, "main", &request).expect_err("the root holds .forklift");
    assert_eq!(err.status, Status::Unprocessable, "{}", err.message);
    assert!(err.message.contains(r#"holds ".forklift""#), "{}", err.message);

    let request = RefUpdateRequest { old_head: None, new_head: below, tombstone: None };
    let err = head.ref_update(&Principal::Static, "main", &request).expect_err("docs holds .ForkLift");
    assert!(err.message.contains(r#"holds "docs/.ForkLift""#), "the path is named whole: {}", err.message);
    assert_eq!(user_head(&head, "main"), None);

    let request = RefUpdateRequest { old_head: None, new_head: clean.clone(), tombstone: None };
    head.ref_update(&Principal::Static, "main", &request).expect("the control: a clean head lifts");
    assert_eq!(user_head(&head, "main"), Some(clean));
}

/// A creation that names a wedged parcel as its tombstone when none stands, and keeps its tree.
// Revert: take the base from `request.tombstone` → the creation commits → red.
#[test]
fn a_creation_is_judged_against_the_stored_tombstone_never_the_request() {
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    let wedged_tree = vec![gate_file(&head, ".forklift", "x"), gate_file(&head, "a", "1")];
    let wedged = gate_parcel(&head, &[], wedged_tree.clone());
    let on_it = gate_parcel(&head, &[&wedged], wedged_tree);

    let request = RefUpdateRequest { old_head: None, new_head: on_it, tombstone: Some(wedged) };
    let err = head.ref_update(&Principal::Static, "side", &request).expect_err("no tombstone stands");
    assert_eq!(err.status, Status::Unprocessable, "{}", err.message);
    assert_eq!(user_head(&head, "side"), None);
}

/// A wedged head deleted here comes back by a restore: its tree is its stored tombstone's, so
/// nothing is loaded.
// Revert: judge a creation against nothing even over a tombstone → the restore is refused → red.
#[test]
fn a_restore_of_a_wedged_tombstone_head_is_accepted() {
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    let wedged = gate_parcel(&head, &[], vec![gate_file(&head, ".forklift", "x")]);
    plant(&head.refs, "side", &wedged);
    head.delete_pallet(&Principal::Static, "side", &wedged).expect("delete side");

    let restore = RefUpdateRequest { old_head: None, new_head: wedged.clone(), tombstone: Some(wedged.clone()) };
    head.ref_update(&Principal::Static, "side", &restore).expect("restore side");
    assert_eq!(user_head(&head, "side"), Some(wedged));
}

/// A pallet planted with a bad name in `d`: an edit in `d` is refused, an edit in `e` commits. The
/// old head's trees are read from the object store, never the audit scratch.
// Revert: skip a name the base already held → the edit in `d` commits → red. Revert: load every
// subtree whatever the base → the edit in `e` is refused → red.
#[test]
fn an_edit_beside_an_old_bad_name_is_refused_and_one_elsewhere_commits() {
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    let d = |f: &str| gate_dir(&head, "d", vec![gate_file(&head, ".forklift", "x"), gate_file(&head, "f", f)]);
    let e = |g: &str| gate_dir(&head, "e", vec![gate_file(&head, "g", g)]);
    let wedged = gate_parcel(&head, &[], vec![d("1"), e("1")]);
    plant(&head.refs, "side", &wedged);

    let beside = gate_parcel(&head, &[&wedged], vec![d("2"), e("1")]);
    let request = RefUpdateRequest { old_head: Some(wedged.clone()), new_head: beside, tombstone: None };
    let err = head.ref_update(&Principal::Static, "side", &request).expect_err("an edit beside .forklift");
    assert_eq!(err.status, Status::Unprocessable, "{}", err.message);
    assert!(err.message.contains(r#"holds "d/.forklift""#), "{}", err.message);

    let elsewhere = gate_parcel(&head, &[&wedged], vec![d("1"), e("2")]);
    let request = RefUpdateRequest { old_head: Some(wedged), new_head: elsewhere.clone(), tombstone: None };
    head.ref_update(&Principal::Static, "side", &request).expect("an edit elsewhere commits");
    assert_eq!(user_head(&head, "side"), Some(elsewhere));
}

/// The gate reads the new head's trees from the audit scratch the closure check just filled, so a
/// wide creation costs no store read per tree beyond the closure check's own.
// Revert: load the new head's trees through the store → every tree is read twice → red.
#[test]
fn the_gate_reads_the_new_heads_trees_from_the_audit_scratch() {
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    let dirs = (0..40).map(|i| gate_dir(&head, &format!("d{}", i), vec![gate_file(&head, "f", &i.to_string())])).collect();
    let wide = gate_parcel(&head, &[], dirs);

    head.objects.reset_reads();
    let request = RefUpdateRequest { old_head: None, new_head: wide, tombstone: None };
    head.ref_update(&Principal::Static, "main", &request).expect("a clean creation lifts");
    let reads = head.objects.reads();

    // 41 trees and a parcel: a gate reading through the store would add at least 41 more.
    assert!(reads <= 42 + 5, "{} reads for 41 trees and a parcel", reads);
}

/// The blob hash of `content`, as a warehouse stores it.
fn blob_hash(content: &str) -> String {
    use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
    use forklift_core::model::blob::Blob;

    LooseObjectBuilder::build_blob(&Blob { content: content.as_bytes().to_vec() }).hash
}

/// A warehouse in `area` at `dir` that records a redaction of each of `contents` in turn, by
/// hash, while no head names it, harvesting the warehouse after each: the `@redactions` heads in
/// recording order, and every object any of them needs.
fn recorded_redactions(area: &Area, dir: &str, contents: &[&str]) -> (Vec<String>, Harvest) {
    prepare(area, dir);
    area.write_file(&format!("{}/notes.txt", dir), "notes\n");
    area.forklift(dir, &["load", "."]);
    area.forklift(dir, &["stack", "-m", "notes"]);

    let mut heads = Vec::new();
    for content in contents {
        area.write_file(&format!("{}/loaded.txt", dir), content);
        area.forklift(dir, &["load", "loaded.txt"]);
        area.forklift(dir, &["redact", "record", &blob_hash(content), "-m", "a leak"]);
        area.forklift(dir, &["unload", "loaded.txt"]);
        std::fs::remove_file(area.path(&format!("{}/loaded.txt", dir))).expect("remove the loaded file");
        heads.push(harvest(&area.path(dir)).head_of("@redactions").expect("a @redactions head"));
    }

    (heads, harvest(&area.path(dir)))
}

/// Plant `new` on meta pallet `name` past every check, as `plant` does a user pallet.
fn plant_meta(refs: &MemoryRefStore, name: &str, expected: Option<&str>, new: &str) {
    let fence = forklift_aws_lambda::format::gate(refs).expect("format gate");
    let outcome = refs.compare_and_set_head(
        pallet_utils::PalletNamespace::Meta, name, expected.map_or(ExpectedHead::Unborn, ExpectedHead::At), new,
        OfficePrecondition::NotConsumed, RedactionsPrecondition::NotConsumed, None, None, &fence, &request_token(),
    ).expect("plant");
    assert_eq!(outcome, CasOutcome::Committed);
}

/// Assert `err` is the redacted-content refusal, naming `hash`.
fn assert_content_redacted(err: &forklift_aws_lambda::error::HeadError, hash: &str) {
    assert_eq!(err.status, Status::Unprocessable, "{}", err.message);
    assert_eq!(refusal_code(&err.message), Some(RefusalCode::ContentRedacted), "{}", err.message);
    assert!(err.message.contains(hash), "the refusal names the hash: {}", err.message);
}

/// A user head is judged whole, never pruned at `old_head`: a redacted file deep in a subtree the
/// update leaves as `old_head` had it refuses the update; the head without it lifts.
// Revert: drop the redaction check from `ref_update` → the edit commits → red.
#[test]
fn a_user_head_naming_redacted_content_deep_in_an_unchanged_subtree_is_refused() {
    let area = Area::new("redacted-deep");
    let (redactions, recorded) = recorded_redactions(&area, "red", &["leaked\n"]);
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &recorded);

    // Pushed before the redaction is recorded: once it applies, the content is not taken again.
    let deep = gate_dir(&head, "a", vec![gate_dir(&head, "b", vec![gate_file(&head, "secret.env", "leaked\n")])]);
    let before = gate_parcel(&head, &[], vec![deep.clone(), gate_file(&head, "x", "1")]);
    plant(&head.refs, "main", &before);
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions[0]);

    let edit = gate_parcel(&head, &[&before], vec![deep, gate_file(&head, "x", "2")]);
    let request = RefUpdateRequest { old_head: Some(before.clone()), new_head: edit, tombstone: None };
    let err = head.ref_update(&Principal::Static, "main", &request).expect_err("the edit still names the leak");
    assert_content_redacted(&err, &blob_hash("leaked\n"));
    assert_eq!(user_head(&head, "main"), Some(before.clone()));

    let clean = gate_parcel(&head, &[&before], vec![gate_file(&head, "x", "2")]);
    let request = RefUpdateRequest { old_head: Some(before), new_head: clean.clone(), tombstone: None };
    head.ref_update(&Principal::Static, "main", &request).expect("the head without the leak lifts");
    assert_eq!(user_head(&head, "main"), Some(clean));
}

/// A redaction carrying a chunk list makes the check read every chunked file's recipe, which the
/// audit's mirror never holds: an unrelated chunked file's is read from the store, and it lifts.
// Revert: walk the scratch alone (`LocalObjects`) → the unrelated file's recipe is missing → red.
#[test]
fn a_chunked_redaction_reads_each_pushed_recipe_from_the_store() {
    let area = Area::new("redacted-chunked");
    prepare(&area, "red");
    area.write_large_file("red/big.bin", 0xBAD, CHUNK_THRESHOLD + 50_000);
    area.forklift("red", &["load", "."]);
    area.forklift("red", &["stack", "-m", "leak"]);
    let leak = harvest(&area.path("red")).head_of("main").expect("main head");
    area.forklift("red", &["remove", "big.bin"]);
    area.forklift("red", &["stack", "-m", "take the leak out"]);
    area.forklift("red", &["redact", "record", &format!("{}:big.bin", leak), "-m", "a leak"]);
    let recorded = harvest(&area.path("red"));

    prepare(&area, "wh");
    area.write_large_file("wh/other.bin", 0x600D, CHUNK_THRESHOLD + 50_000);
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "-m", "other"]);
    let other = harvest(&area.path("wh"));

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &recorded);
    upload_all(&head, &other);
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &recorded.head_of("@redactions").expect("@redactions"));

    let main = other.head_of("main").expect("main head");
    let request = RefUpdateRequest { old_head: None, new_head: main.clone(), tombstone: None };
    head.ref_update(&Principal::Static, "main", &request).expect("an unrelated chunked file lifts");
    assert_eq!(user_head(&head, "main"), Some(main));
}

/// A meta parcel is judged by what it adds: the first `@tags` parcel adds the tags marker, so with
/// the marker redacted it is refused, while a later parcel carrying the marker from a parent that
/// already stands lifts.
// Revert: drop the redaction check from `ref_update` → the first parcel commits → red.
#[test]
fn a_meta_parcel_adding_redacted_content_is_refused_and_one_carrying_it_is_not() {
    let area = Area::new("redacted-meta");
    prepare(&area, "wh");
    area.write_file("wh/notes.txt", "notes\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "-m", "notes"]);
    area.forklift("wh", &["tag", "create", "v1", "main"]);
    let first = harvest(&area.path("wh")).head_of("@tags").expect("tags head");
    area.forklift("wh", &["tag", "create", "v2", "main"]);
    let tagged = harvest(&area.path("wh"));
    let second = tagged.head_of("@tags").expect("tags head");

    let marker = "forklift-meta 1 tags";
    let (redactions, recorded) = recorded_redactions(&area, "red", &[marker]);
    let meta_head = |head: &Head<MemoryObjectStore, MemoryRefStore>| {
        head.refs.get_head(pallet_utils::PalletNamespace::Meta, "tags").unwrap().live().map(str::to_string)
    };

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &tagged);
    upload_all(&head, &recorded);
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions[0]);

    let request = RefUpdateRequest { old_head: None, new_head: first.clone(), tombstone: None };
    let err = head.ref_update(&Principal::Static, "@tags", &request).expect_err("the first parcel adds the marker");
    assert_content_redacted(&err, &blob_hash(marker));
    assert_eq!(meta_head(&head), None);

    plant_meta(&head.refs, "tags", None, &first);
    let request = RefUpdateRequest { old_head: Some(first), new_head: second.clone(), tombstone: None };
    head.ref_update(&Principal::Static, "@tags", &request).expect("a parcel carrying the marker lifts");
    assert_eq!(meta_head(&head), Some(second));
}

/// A write to `@redactions` itself is judged against the redactions it replaces and conditions on
/// no `@redactions` item but its own: recording a second redaction lifts.
// The memory store does not model DynamoDB's refusal of two actions on one item, so this pins
// only that the write is judged and lifts; `dynamo.rs` drops the self-condition.
#[test]
fn a_redactions_write_lifts_over_the_head_it_replaces() {
    let area = Area::new("redactions-write");
    let (redactions, recorded) = recorded_redactions(&area, "red", &["one\n", "two\n"]);
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &recorded);
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions[0]);

    let wire = format!("@{}", REDACTIONS_PALLET_NAME);
    let request = RefUpdateRequest { old_head: Some(redactions[0].clone()), new_head: redactions[1].clone(), tombstone: None };
    head.ref_update(&Principal::Static, &wire, &request).expect("a second redaction lifts");
    let current = head.refs.get_head(pallet_utils::PalletNamespace::Meta, REDACTIONS_PALLET_NAME).unwrap();
    assert_eq!(current.live(), Some(redactions[1].as_str()));
}

/// Wraps a shared [`MemoryRefStore`]; before each user-pallet commit while `moves` holds one,
/// lifts `@redactions` to the next of `moves`, as a redaction recorded between `ref_update`'s
/// check and its commit would. Forwards every other call.
struct RedactionsMoveBeforeTheCommit {
    inner: Arc<MemoryRefStore>,
    moves: RefCell<Vec<String>>,
}

impl RefStore for RedactionsMoveBeforeTheCommit {
    fn read_format(&self) -> Result<Option<String>, String> {
        self.inner.read_format()
    }

    fn partition_has_items(&self) -> Result<bool, String> {
        self.inner.partition_has_items()
    }

    fn get_head(&self, namespace: pallet_utils::PalletNamespace, name: &str) -> Result<RefHead, String> {
        self.inner.get_head(namespace, name)
    }

    fn compare_and_set_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        expected: ExpectedHead<'_>,
        new: &str,
        office_head: OfficePrecondition<'_>,
        redactions: RedactionsPrecondition<'_>,
        anchor: Option<&str>,
        chunk_fence: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        if namespace == pallet_utils::PalletNamespace::User && !self.moves.borrow().is_empty() {
            let next = self.moves.borrow_mut().remove(0);
            let current = self.inner.get_head(pallet_utils::PalletNamespace::Meta, REDACTIONS_PALLET_NAME)?;
            plant_meta(&self.inner, REDACTIONS_PALLET_NAME, current.live(), &next);
        }

        self.inner.compare_and_set_head(namespace, name, expected, new, office_head, redactions, anchor, chunk_fence, format, request_token)
    }

    fn delete_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        old: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.delete_head(namespace, name, old, office_head, anchor, format, request_token)
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
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<TrustOutcome, String> {
        self.inner.put_trust_if_absent(anchor, format, request_token)
    }

    fn replace_trust(
        &self,
        anchor: &office_utils::TrustAnchor,
        expected_anchor: &str,
        office_head: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<TrustWriteOutcome, String> {
        self.inner.replace_trust(anchor, expected_anchor, office_head, format, request_token)
    }

    fn get_item(&self, key: forklift_aws_lambda::store::ItemKey<'_>) -> Result<Option<String>, String> {
        self.inner.get_item(key)
    }

    fn list_withheld(&self) -> Result<Vec<(String, String)>, String> {
        self.inner.list_withheld()
    }

    fn put_items(
        &self,
        writes: &[forklift_aws_lambda::store::ItemWrite<'_>],
        format: &forklift_aws_lambda::FormatFence,
    ) -> Result<forklift_aws_lambda::store::RedactionWrite, String> {
        self.inner.put_items(writes, format)
    }
}

/// A redaction recorded between the check and the commit refuses the commit, and the check runs
/// again against it: one naming the pushed content turns the update into the redacted refusal,
/// and redactions that keep moving end in a retryable `409`, with nothing committed either way.
// Revert: commit with `RedactionsPrecondition::NotConsumed` → the update commits over the new
// redaction → red. Revert: retry whatever the deadline → the last arm spends every move → red.
#[test]
fn a_redaction_recorded_during_the_check_refuses_the_commit_and_is_judged() {
    let area = Area::new("redacted-race");
    // Four redactions that leave the pushed content alone, then the one naming it.
    let (redactions, recorded) =
        recorded_redactions(&area, "red", &["other\n", "more\n", "most\n", "last\n", "leaked\n"]);
    let racing = |moves: &[&String]| {
        let inner = Arc::new(MemoryRefStore::new());
        let refs = RedactionsMoveBeforeTheCommit {
            inner: inner.clone(),
            moves: RefCell::new(moves.iter().map(|head| head.to_string()).collect()),
        };
        let head = Head::new(MemoryObjectStore::new(), refs);
        upload_all(&head, &recorded);
        plant_meta(&inner, REDACTIONS_PALLET_NAME, None, &redactions[0]);
        let pushed = gate_parcel(&head, &[], vec![gate_file(&head, "secret.env", "leaked\n")]);

        let request = RefUpdateRequest { old_head: None, new_head: pushed, tombstone: None };
        let answer = head.ref_update(&Principal::Static, "main", &request);
        assert_eq!(inner.get_head(pallet_utils::PalletNamespace::User, "main").unwrap(), RefHead::Absent);
        answer
    };

    let err = racing(&[&redactions[4]]).expect_err("the leak was redacted before the commit");
    assert_content_redacted(&err, &blob_hash("leaked\n"));

    let err = racing(&[&redactions[1], &redactions[2], &redactions[3]]).expect_err("the redactions kept moving");
    assert_eq!(err.status, Status::Conflict, "{}", err.message);

    // With no time left in the invocation, the first move answers the `409` at once.
    let inner = Arc::new(MemoryRefStore::new());
    let refs = RedactionsMoveBeforeTheCommit {
        inner: inner.clone(),
        moves: RefCell::new(vec![redactions[1].clone(), redactions[2].clone()]),
    };
    let head = Head::new(MemoryObjectStore::new(), refs).with_deadline(std::time::SystemTime::now());
    upload_all(&head, &recorded);
    plant_meta(&inner, REDACTIONS_PALLET_NAME, None, &redactions[0]);
    let pushed = gate_parcel(&head, &[], vec![gate_file(&head, "secret.env", "leaked\n")]);
    let request = RefUpdateRequest { old_head: None, new_head: pushed, tombstone: None };
    let err = head.ref_update(&Principal::Static, "main", &request).expect_err("no time for another check");
    assert_eq!(err.status, Status::Conflict, "{}", err.message);
    assert_eq!(head.refs.moves.borrow().len(), 1, "one attempt, not three");
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
    area.forklift("wh", &["stack", "-m", "a giant"]);

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
    assert_eq!(refusal_code(&err.message), Some(RefusalCode::RemoteHistoryIncomplete), "{}", err.message);

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
    area.forklift("wh", &["stack", "-m", "signed one"]);
    area.write_file("wh/app.txt", "v2\n");
    area.forklift("wh", &["load", "app.txt"]);
    area.forklift("wh", &["stack", "-m", "signed two"]);

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
    area.forklift("wh", &["stack", "-m", "signed"]);
    area.forklift("wh", &["manifest", "set", "main", "ci", "green"]);
    let first = harvest(&area.path("wh")).head_of("@kv").expect("kv head");
    area.forklift("wh", &["manifest", "set", "main", "review", "done"]);

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
    area.forklift("wh", &["stack", "-m", "signed"]);
    area.forklift("wh", &["pallet", "create", "feature"]);
    area.write_file("wh/app.txt", "v2\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "-m", "feature"]);
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
///
/// Over the Windows time budget until FORK-384 (25.8 s and 10.6 s on the Windows CI runner in two runs):
/// most of its time is syncs, 1.2 s alone on an M-series Mac with fsync on and 0.15 s with it off.
/// FORK-384 tracks the per-object syncs of the scratch mirror these tests write through.
#[test]
fn a_tag_is_admitted_only_from_an_admin_of_the_held_office() {
    let area = Area::new("tags");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "-m", "signed"]);

    // A second identity in the same warehouse, admitted as an admin, tags a release.
    let worker = "worker@forklift";
    area.forklift("wh", &["config", "set", "operator.identifier", worker]);
    let keygen = area.forklift_stdout("wh", &["profile", "keygen"]);
    area.forklift("wh", &["config", "set", "operator.identifier", "tester@forklift"]);
    let mut admit: Vec<&str> = keygen.lines()
        .find(|line| line.trim_start().starts_with("office admit "))
        .expect("keygen prints the admit line")
        .split_whitespace()
        .collect();
    admit.extend(["--role", "admin"]);
    area.forklift("wh", &admit);

    area.forklift("wh", &["config", "set", "operator.identifier", worker]);
    area.forklift("wh", &["tag", "create", "v1.0", "main"]);
    area.forklift("wh", &["config", "set", "operator.identifier", "tester@forklift"]);

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
///
/// Over the Windows time budget until FORK-384 (30.2 s and 12.5 s on the Windows CI runner in two runs):
/// most of its time is syncs, 1.5 s alone on an M-series Mac with fsync on and 0.21 s with it off.
/// FORK-384 tracks the per-object syncs of the scratch mirror these tests write through.
#[test]
fn a_haul_merge_is_admitted_only_from_a_writer_of_its_target() {
    let area = Area::new("haul-merge");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "-m", "signed"]);
    area.forklift("wh", &["pallet", "create", "feat"]);
    area.write_file("wh/feat.txt", "x\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "-m", "feature"]);
    area.forklift("wh", &["shift", "main"]);
    let opened = area.forklift_stdout("wh", &["--json", "haul", "open", "--target", "main", "--source", "feat", "--title", "Feat"]);
    let id = serde_json::from_str::<serde_json::Value>(&opened).unwrap()["data"]["id"].as_str().unwrap().to_string();

    let worker = "worker@forklift";
    area.forklift("wh", &["config", "set", "operator.identifier", worker]);
    let keygen = area.forklift_stdout("wh", &["profile", "keygen"]);
    area.forklift("wh", &["config", "set", "operator.identifier", "tester@forklift"]);
    let admit: Vec<&str> = keygen.lines()
        .find(|line| line.trim_start().starts_with("office admit "))
        .expect("keygen prints the admit line")
        .split_whitespace()
        .collect();
    area.forklift("wh", &admit);

    area.forklift("wh", &["config", "set", "operator.identifier", worker]);
    area.forklift("wh", &["haul", "consolidate", &id]);
    area.forklift("wh", &["config", "set", "operator.identifier", "tester@forklift"]);

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
    area.forklift("wh", &["config", "set", "operator.identifier", worker]);
    let keygen = area.forklift_stdout("wh", &["profile", "keygen"]);
    area.forklift("wh", &["config", "set", "operator.identifier", "tester@forklift"]);
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
            RedactionsPrecondition::NotConsumed,
            trust.as_ref().map(|(_, raw)| raw.as_str()),
            None,
            &forklift_aws_lambda::format::gate(&head.refs).expect("format gate"),
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
    area.forklift("wh", &["stack", "-m", "signed"]);

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
    area.forklift("wh", &["stack", "-m", "signed"]);

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
            RedactionsPrecondition::NotConsumed,
            None,
            None,
            &forklift_aws_lambda::format::gate(&*refs).expect("format gate"),
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
    area.forklift("wh", &["stack", "-m", "signed"]);

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

/// A refusal's framed code, read back the way the client reads it.
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
    area.forklift("wh", &["stack", "-m", "create"]);

    for touch in 0..touches {
        area.write_file("wh/d0/f.txt", &format!("v{}\n", touch + 1));
        area.forklift("wh", &["load", "."]);
        area.forklift("wh", &["stack", "-m", &format!("touch {}", touch)]);
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
    area.forklift("wh", &["stack", "-m", "the new segment"]);

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
/// while an unbounded one also re-fetches every superseded tree version. The name gate reads the
/// old head's changed trees from the store on an update and nothing extra on a creation, a fixed
/// cost the short history is long enough to outweigh.
///
/// Over the Windows time budget until FORK-384 (14.5 s and 16.3 s on the Windows CI runner in two runs):
/// most of its time is syncs, 1.5 s alone on an M-series Mac with fsync on and 0.36 s with it off.
/// FORK-384 tracks the per-object syncs of the scratch mirror these tests write through.
#[test]
fn the_ref_update_mirror_is_bounded_at_old_head() {
    let extra = 4;
    let (bounded_short, unbounded_short) = mirror_reads(3);
    let (bounded_long, unbounded_long) = mirror_reads(3 + extra);

    assert!(bounded_short < unbounded_short, "the bound saves reads even on a short history: {} vs {}", bounded_short, unbounded_short);

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
    area.forklift("wh", &["stack", "-m", "base"]);

    // A branch forking at `base` — below the bound the lift will later carry.
    area.forklift("wh", &["pallet", "create", "feature"]);
    area.write_file("wh/feature.txt", "from the branch\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "-m", "on the branch"]);

    // main moves on, and that head becomes the remote's `old_head`.
    area.forklift("wh", &["shift", "main"]);
    area.write_file("wh/app.txt", "moved on\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "-m", "on main"]);

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
    // A history long enough to dominate the cold read: the name gate re-reads the trees the
    // update changes on both heads, warm or cold, a cost set by the change, not the history.
    layered_warehouse(&area, 4, 12);

    let old_head =
        harvest(&area.path("wh")).head_of("main").expect("main head");

    area.write_file("wh/d0/f.txt", "v2\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "-m", "the new segment"]);

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
    area.forklift("wh", &["stack", "-m", "one"]);

    let harvest = harvest(&area.path("wh"));
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &harvest);

    let hashes: Vec<String> = harvest.objects.keys().cloned().collect();

    let answer = head.batch(&hashes).expect("batch");
    assert!(answer.cut.is_none(), "a small batch is answered whole");
    match answer.delivery {
        BatchDelivery::Bundle(bundle) => {
            assert!(!bundle.is_empty(), "the batch produced a non-empty bundle stream")
        }
        BatchDelivery::Redirect(_) => panic!("a direct store serves the bundle inline"),
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
    area.forklift("wh", &["stack", "-m", "one"]);

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

    let inline = match direct.batch(&hashes).expect("direct batch").delivery {
        BatchDelivery::Bundle(bundle) => bundle,
        BatchDelivery::Redirect(_) => panic!("a direct store serves the bundle inline"),
    };

    match staging.batch(&hashes).expect("offloaded batch").delivery {
        BatchDelivery::Redirect(url) => {
            assert!(url.starts_with("https://s3.example/bucket/responses/"));
            assert!(!url.contains("/objects/"), "a response body is never an object");

            let served = staging.objects.offloaded_response(&url).expect("the presigned bytes");
            assert_eq!(served, inline, "the offloaded bundle is the bundle");
        }
        BatchDelivery::Bundle(_) => panic!("an offloading store hands out a presigned GET"),
    }
}

/// `count` distinct objects of `size` bytes each, seeded by `seed`, with their hashes.
fn sized_objects(count: usize, size: usize, seed: &str) -> Vec<(String, Vec<u8>)> {
    (0..count)
        .map(|i| {
            let mut content = format!("{seed}-{i}-").into_bytes();
            content.resize(size, b'x');
            let bytes = object_utils::blob_bytes(&content);
            (object_utils::hash_object_bytes(&bytes), bytes)
        })
        .collect()
}

/// A direct head over a store seeded with `objects`.
fn seeded_head(store: MemoryObjectStore, objects: &[(String, Vec<u8>)]) -> Head<MemoryObjectStore, MemoryRefStore> {
    for (hash, bytes) in objects {
        store.put_verified(hash, bytes).expect("seed");
    }
    Head::new(store, MemoryRefStore::new())
}

/// The requested objects an inline batch answer carries, read back by importing its bundle into a
/// fresh warehouse `name`, in request order.
fn bundled(area: &Area, name: &str, answer: BatchResult, requested: &[String]) -> Vec<String> {
    let BatchDelivery::Bundle(bundle) = answer.delivery else {
        panic!("a direct store serves the bundle inline");
    };

    prepare(area, name);
    let _scope = StorageRootScope::enter(&area.path(name));
    let admission = bundle_utils::StreamAdmission::Requested(requested.iter().cloned().collect());
    bundle_utils::import_streamed_bundle(&bundle[..], admission).expect("the bundle imports");

    requested.iter().filter(|hash| file_utils::does_object_exist(hash).expect("presence")).cloned().collect()
}

/// A batch past the byte budget stops at the object that crossed it, marked as cut at the
/// budget: a strict prefix of what was asked, at least one object, and over the budget by less
/// than one object.
/// Revert: drop the byte rule from `batch_answer_cut` → every object comes back, unmarked.
#[test]
fn a_batch_past_its_byte_budget_answers_a_marked_prefix() {
    const SIZE: usize = 100 * 1024;
    let area = Area::new("batch-budget");
    let objects = sized_objects(10, SIZE, "budget");
    let hashes: Vec<String> = objects.iter().map(|(hash, _)| hash.clone()).collect();

    let head = seeded_head(MemoryObjectStore::new(), &objects)
        .with_batch_limits(250 * 1024, std::time::Duration::ZERO);
    let answer = head.batch(&hashes).expect("batch");
    assert!(answer.cut == Some(BatchCut::Budget), "an answer cut at the budget is marked so");

    let carried = bundled(&area, "budget", answer, &hashes);
    assert_eq!(carried, hashes[..3], "it stops at the object that crossed 250 KiB, in request order");
    assert!(carried.len() * SIZE < 250 * 1024 + SIZE, "the overshoot is under one object");

    // The control: the same batch inside its budget is whole and unmarked.
    let head = seeded_head(MemoryObjectStore::new(), &objects)
        .with_batch_limits(10 * SIZE as u64 + 1, std::time::Duration::ZERO);
    let answer = head.batch(&hashes).expect("batch");
    assert!(answer.cut.is_none(), "a whole answer is not marked");
    assert_eq!(bundled(&area, "whole", answer, &hashes), hashes);
}

/// The byte budget never stops an answer that carries nothing: at a zero budget the first object
/// held is still sent, and objects the store lacks count toward nothing. An answer short only
/// of what the store lacks is not marked; one that stopped before looking for every requested
/// object is, even when the store lacks the ones it skipped.
/// Revert: let the byte rule in `batch_answer_cut` stop an empty answer → the first answer
/// carries nothing.
#[test]
fn a_batch_always_carries_one_object_and_absent_ones_do_not_mark_it() {
    let area = Area::new("batch-one");
    let objects = sized_objects(3, 64 * 1024, "one");
    let absent: Vec<String> = synthetic_objects(2, "never-stored").into_iter().map(|(hash, _)| hash).collect();
    let mut hashes = absent.clone();
    hashes.extend(objects.iter().map(|(hash, _)| hash.clone()));

    // A zero budget: every object is over it, and only the rule that an empty answer never
    // stops lets one through.
    let head = seeded_head(MemoryObjectStore::new(), &objects).with_batch_limits(0, std::time::Duration::ZERO);
    let answer = head.batch(&hashes).expect("batch");
    assert_eq!(answer.cut, Some(BatchCut::Budget));
    assert_eq!(bundled(&area, "one", answer, &hashes), hashes[2..3], "the absent ones are skipped, then one object");

    // Only absent objects: nothing to carry, and nothing was cut.
    let answer = head.batch(&hashes[..2]).expect("batch");
    assert!(answer.cut.is_none(), "an answer short only of what the store lacks is not marked");

    // Cut before objects the store lacks: the head did not look for them, so it cannot say the
    // answer is short only of those, and marks it.
    let cut = [hashes[2].clone(), absent[0].clone(), absent[1].clone()];
    let answer = head.batch(&cut).expect("batch");
    assert!(answer.cut == Some(BatchCut::Budget), "an answer that skipped looking for an object is marked");
    assert_eq!(bundled(&area, "skipped", answer, &cut), cut[..1]);

    // A deadline already past still looks for the first object, and carries it when it is held.
    // The budget is spent too, and the deadline is the cut it names: the one that tells the client
    // not to ask again.
    let head = seeded_head(MemoryObjectStore::new(), &objects)
        .with_batch_limits(0, std::time::Duration::from_secs(10))
        .with_deadline(std::time::SystemTime::now());
    let answer = head.batch(&hashes[2..]).expect("batch");
    assert_eq!(answer.cut, Some(BatchCut::Deadline));
    assert_eq!(bundled(&area, "late", answer, &hashes[2..]), hashes[2..3]);
}

/// The invocation deadline of the slow-store batch tests, and their answer reserve. The cut comes
/// once only the reserve is left, 1 s in, a few 300 ms reads into twelve. All twelve take 3.6 s,
/// well inside the deadline, so a head that ignores the deadline or its reserve answers whole and
/// unmarked. The wide reserve leaves 9 s after the cut, so a loaded runner's scheduling delay is
/// unlikely to carry the answer past the deadline; that the head stops reading at the cut is held
/// by the store's read count, which no delay moves.
const BATCH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
const BATCH_RESERVE: std::time::Duration = std::time::Duration::from_secs(9);

/// The deadline counts lookups of objects the store lacks: a batch of them over a slow store
/// stops when the deadline nears, marked, rather than running on while it carries nothing.
/// Revert: let the deadline rule in `batch_answer_cut` wait for a carried object → all twelve
/// lookups run and the answer is unmarked. Revert: keep looking after the cut → twelve lookups.
#[test]
fn a_batch_of_absent_objects_over_a_slow_store_answers_before_the_invocation_deadline() {
    let hashes: Vec<String> = synthetic_objects(12, "slow-absent").into_iter().map(|(hash, _)| hash).collect();

    let head = seeded_head(MemoryObjectStore::new().with_read_delay(std::time::Duration::from_millis(300)), &[]);
    let started = std::time::SystemTime::now();
    let head = head.with_batch_limits(u64::MAX, BATCH_RESERVE).with_deadline(started + BATCH_DEADLINE);

    let answer = head.batch(&hashes).expect("batch");
    let elapsed = started.elapsed().expect("a forward clock");

    assert!(answer.cut == Some(BatchCut::Deadline), "an answer cut at the deadline is marked so, carried objects or not");
    assert!(head.objects.reads() < hashes.len(), "it stopped looking at the cut: {} lookups", head.objects.reads());
    assert!(elapsed < BATCH_DEADLINE, "it answered before the deadline: {:?}", elapsed);
}

/// A batch over a slow store stops mirroring when the invocation deadline nears, and answers
/// before the deadline with a marked, strict subset.
/// Revert: drop the deadline rule from `batch_answer_cut` → all twelve reads run and the answer
/// is whole. Revert: keep reading after the cut → more reads than objects carried.
#[test]
fn a_batch_over_a_slow_store_answers_before_the_invocation_deadline() {
    let area = Area::new("batch-deadline");
    let objects = sized_objects(12, 1024, "slow");
    let hashes: Vec<String> = objects.iter().map(|(hash, _)| hash.clone()).collect();

    let head = seeded_head(MemoryObjectStore::new().with_read_delay(std::time::Duration::from_millis(300)), &objects);
    let started = std::time::SystemTime::now();
    let head = head.with_batch_limits(u64::MAX, BATCH_RESERVE).with_deadline(started + BATCH_DEADLINE);

    let answer = head.batch(&hashes).expect("batch");
    let elapsed = started.elapsed().expect("a forward clock");

    assert!(answer.cut == Some(BatchCut::Deadline), "an answer cut at the deadline is marked so");
    assert!(elapsed < BATCH_DEADLINE, "it answered before the deadline: {:?}", elapsed);

    let carried = bundled(&area, "deadline", answer, &hashes);
    assert!(!carried.is_empty() && carried.len() < hashes.len(), "a strict, non-empty subset: {}", carried.len());
    assert_eq!(carried, hashes[..carried.len()], "a prefix, in request order");
    // One read per carried object holds because `seeded_head` gives the head an ephemeral scratch,
    // which never already holds an object; a pooled one would let objects skip the store.
    assert_eq!(head.objects.reads(), carried.len(), "it read nothing past the cut");
}

/// A deadline further off than the reserve does not cut: the same batch, with time to spare,
/// answers whole and unmarked.
/// Revert: invert the reserve comparison in `batch_answer_cut` → it cuts after the first object.
#[test]
fn a_batch_with_time_to_spare_before_the_deadline_answers_whole() {
    let area = Area::new("batch-spare");
    let objects = sized_objects(12, 1024, "spare");
    let hashes: Vec<String> = objects.iter().map(|(hash, _)| hash.clone()).collect();

    let head = seeded_head(MemoryObjectStore::new(), &objects)
        .with_batch_limits(u64::MAX, BATCH_RESERVE)
        .with_deadline(std::time::SystemTime::now() + BATCH_DEADLINE * 10);

    let answer = head.batch(&hashes).expect("batch");
    assert_eq!(answer.cut, None, "an answer with time to spare is not marked");
    assert_eq!(bundled(&area, "spare", answer, &hashes), hashes, "the whole batch, in request order");
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
    fn read_format(&self) -> Result<Option<String>, String> {
        self.inner.read_format()
    }

    fn partition_has_items(&self) -> Result<bool, String> {
        self.inner.partition_has_items()
    }

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
        redactions: RedactionsPrecondition<'_>,
        anchor: Option<&str>,
        chunk_fence: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.bridge.block_on(suspending(|| {
            self.inner.compare_and_set_head(namespace, name, expected, new, office_head, redactions, anchor, chunk_fence, format, request_token)
        }))
    }

    fn delete_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        old: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.bridge.block_on(suspending(|| self.inner.delete_head(namespace, name, old, office_head, anchor, format, request_token)))
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

    fn put_trust_if_absent(
        &self,
        anchor: &office_utils::TrustAnchor,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<TrustOutcome, String> {
        self.bridge.block_on(suspending(|| self.inner.put_trust_if_absent(anchor, format, request_token)))
    }

    fn replace_trust(
        &self,
        anchor: &office_utils::TrustAnchor,
        expected_anchor: &str,
        office_head: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<TrustWriteOutcome, String> {
        self.bridge.block_on(suspending(|| self.inner.replace_trust(anchor, expected_anchor, office_head, format, request_token)))
    }

    fn get_item(&self, key: forklift_aws_lambda::store::ItemKey<'_>) -> Result<Option<String>, String> {
        self.inner.get_item(key)
    }

    fn list_withheld(&self) -> Result<Vec<(String, String)>, String> {
        self.inner.list_withheld()
    }

    fn put_items(
        &self,
        writes: &[forklift_aws_lambda::store::ItemWrite<'_>],
        format: &forklift_aws_lambda::FormatFence,
    ) -> Result<forklift_aws_lambda::store::RedactionWrite, String> {
        self.inner.put_items(writes, format)
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
    area.forklift("wh", &["stack", "-m", "signed one"]);

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
    fn read_format(&self) -> Result<Option<String>, String> {
        self.0.read_format()
    }

    fn partition_has_items(&self) -> Result<bool, String> {
        self.0.partition_has_items()
    }

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
        redactions: RedactionsPrecondition<'_>,
        anchor: Option<&str>,
        chunk_fence: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.0.compare_and_set_head(namespace, name, expected, new, office_head, redactions, anchor, chunk_fence, format, request_token)
    }

    fn delete_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        old: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.0.delete_head(namespace, name, old, office_head, anchor, format, request_token)
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

    fn put_trust_if_absent(
        &self,
        anchor: &office_utils::TrustAnchor,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<TrustOutcome, String> {
        self.0.put_trust_if_absent(anchor, format, request_token)
    }

    fn replace_trust(
        &self,
        anchor: &office_utils::TrustAnchor,
        expected_anchor: &str,
        office_head: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<TrustWriteOutcome, String> {
        self.0.replace_trust(anchor, expected_anchor, office_head, format, request_token)
    }

    fn get_item(&self, key: forklift_aws_lambda::store::ItemKey<'_>) -> Result<Option<String>, String> {
        self.0.get_item(key)
    }

    fn list_withheld(&self) -> Result<Vec<(String, String)>, String> {
        self.0.list_withheld()
    }

    fn put_items(
        &self,
        writes: &[forklift_aws_lambda::store::ItemWrite<'_>],
        format: &forklift_aws_lambda::FormatFence,
    ) -> Result<forklift_aws_lambda::store::RedactionWrite, String> {
        self.0.put_items(writes, format)
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
    fn read_format(&self) -> Result<Option<String>, String> {
        self.inner.read_format()
    }

    fn partition_has_items(&self) -> Result<bool, String> {
        self.inner.partition_has_items()
    }

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
                // This injected CAS is itself subject to the same preconditions — the anchor
                // is checked unconditionally, regardless of which pallet is being
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
                        RedactionsPrecondition::NotConsumed,
                        anchor_bytes.as_deref(),
                        None,
                        &forklift_aws_lambda::format::gate(&*self.inner).expect("format gate"),
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
        redactions: RedactionsPrecondition<'_>,
        anchor: Option<&str>,
        chunk_fence: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.compare_and_set_head(namespace, name, expected, new, office_head, redactions, anchor, chunk_fence, format, request_token)
    }

    fn delete_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        old: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.delete_head(namespace, name, old, office_head, anchor, format, request_token)
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
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<TrustOutcome, String> {
        self.inner.put_trust_if_absent(anchor, format, request_token)
    }

    fn replace_trust(
        &self,
        anchor: &office_utils::TrustAnchor,
        expected_anchor: &str,
        office_head: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<TrustWriteOutcome, String> {
        self.inner.replace_trust(anchor, expected_anchor, office_head, format, request_token)
    }

    fn get_item(&self, key: forklift_aws_lambda::store::ItemKey<'_>) -> Result<Option<String>, String> {
        self.inner.get_item(key)
    }

    fn list_withheld(&self) -> Result<Vec<(String, String)>, String> {
        self.inner.list_withheld()
    }

    fn put_items(
        &self,
        writes: &[forklift_aws_lambda::store::ItemWrite<'_>],
        format: &forklift_aws_lambda::FormatFence,
    ) -> Result<forklift_aws_lambda::store::RedactionWrite, String> {
        self.inner.put_items(writes, format)
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
    fn read_format(&self) -> Result<Option<String>, String> {
        self.inner.read_format()
    }

    fn partition_has_items(&self) -> Result<bool, String> {
        self.inner.partition_has_items()
    }

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
        redactions: RedactionsPrecondition<'_>,
        anchor: Option<&str>,
        chunk_fence: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.compare_and_set_head(namespace, name, expected, new, office_head, redactions, anchor, chunk_fence, format, request_token)
    }

    fn delete_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        old: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.delete_head(namespace, name, old, office_head, anchor, format, request_token)
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
                    .replace_trust(&self.move_to, &expected, office_head.live(), &forklift_aws_lambda::format::gate(&*self.inner).expect("format gate"), &request_token())
                    .expect("inject the concurrent re-genesis"),
                TrustWriteOutcome::Replaced,
                "the injected re-genesis must actually land, or the test it feeds proves \
                nothing about a moved anchor"
            );
        }

        result
    }

    fn put_trust_if_absent(
        &self,
        anchor: &office_utils::TrustAnchor,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<TrustOutcome, String> {
        self.inner.put_trust_if_absent(anchor, format, request_token)
    }

    fn replace_trust(
        &self,
        anchor: &office_utils::TrustAnchor,
        expected_anchor: &str,
        office_head: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<TrustWriteOutcome, String> {
        self.inner.replace_trust(anchor, expected_anchor, office_head, format, request_token)
    }

    fn get_item(&self, key: forklift_aws_lambda::store::ItemKey<'_>) -> Result<Option<String>, String> {
        self.inner.get_item(key)
    }

    fn list_withheld(&self) -> Result<Vec<(String, String)>, String> {
        self.inner.list_withheld()
    }

    fn put_items(
        &self,
        writes: &[forklift_aws_lambda::store::ItemWrite<'_>],
        format: &forklift_aws_lambda::FormatFence,
    ) -> Result<forklift_aws_lambda::store::RedactionWrite, String> {
        self.inner.put_items(writes, format)
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
    fn read_format(&self) -> Result<Option<String>, String> {
        self.inner.read_format()
    }

    fn partition_has_items(&self) -> Result<bool, String> {
        self.inner.partition_has_items()
    }

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
        redactions: RedactionsPrecondition<'_>,
        anchor: Option<&str>,
        chunk_fence: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
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

        self.inner.compare_and_set_head(namespace, name, expected, new, office_head, redactions, anchor, chunk_fence, format, request_token)
    }

    fn delete_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        old: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.delete_head(namespace, name, old, office_head, anchor, format, request_token)
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
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<TrustOutcome, String> {
        self.inner.put_trust_if_absent(anchor, format, request_token)
    }

    fn replace_trust(
        &self,
        anchor: &office_utils::TrustAnchor,
        expected_anchor: &str,
        office_head: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<TrustWriteOutcome, String> {
        self.inner.replace_trust(anchor, expected_anchor, office_head, format, request_token)
    }

    fn get_item(&self, key: forklift_aws_lambda::store::ItemKey<'_>) -> Result<Option<String>, String> {
        self.inner.get_item(key)
    }

    fn list_withheld(&self) -> Result<Vec<(String, String)>, String> {
        self.inner.list_withheld()
    }

    fn put_items(
        &self,
        writes: &[forklift_aws_lambda::store::ItemWrite<'_>],
        format: &forklift_aws_lambda::FormatFence,
    ) -> Result<forklift_aws_lambda::store::RedactionWrite, String> {
        self.inner.put_items(writes, format)
    }
}

/// A pushed history ready to commit to `main` of an untrusted warehouse, uploaded into `head`.
fn uploaded_first_push<R: RefStore>(area_name: &str, refs: R) -> (Area, Head<MemoryObjectStore, R>, String) {
    let area = Area::new(area_name);
    prepare(&area, "wh");
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "-m", "first"]);

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
    area.forklift("wh", &["stack", "-m", "signed one"]);

    let main_v1 = harvest(&area.path("wh")).head_of("main").expect("main v1");

    area.write_file("wh/app.txt", "v2\n");
    area.forklift("wh", &["load", "app.txt"]);
    area.forklift("wh", &["stack", "-m", "signed two"]);

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
///
/// Over the Windows time budget until FORK-384 (19.8 s and 21.5 s on the Windows CI runner in two runs):
/// most of its time is syncs, 3.0 s alone on an M-series Mac with fsync on and 0.25 s with it off.
/// FORK-384 tracks the per-object syncs of the scratch mirror these tests write through.
#[test]
fn a_grant_revoked_between_the_move_gate_and_the_commit_refuses_the_move() {
    let area = Area::new("grant-revoked-mid-update");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);

    let writer = "wes@forklift";
    area.forklift("wh", &["config", "set", "operator.identifier", writer]);
    let keygen = area.forklift_stdout("wh", &["profile", "keygen"]);
    area.forklift("wh", &["config", "set", "operator.identifier", "tester@forklift"]);
    let admit: Vec<&str> = keygen.lines()
        .find(|line| line.trim_start().starts_with("office admit "))
        .expect("keygen prints the admit line")
        .split_whitespace()
        .collect();
    area.forklift("wh", &admit);
    area.forklift("wh", &["office", "role", writer, "writer", "--pallet", "main"]);

    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "-m", "signed one"]);

    let granted = harvest(&area.path("wh"));
    let office_granted = granted.head_of(&format!("@{}", OFFICE_PALLET_NAME)).expect("office head");
    let main_v1 = granted.head_of("main").expect("main v1");

    area.write_file("wh/app.txt", "v2\n");
    area.forklift("wh", &["load", "app.txt"]);
    area.forklift("wh", &["stack", "-m", "signed two"]);

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
    area.forklift("wh", &["stack", "-m", "first"]);

    let main_v1 = harvest(&area.path("wh")).head_of("main").expect("main v1");

    area.write_file("wh/app.txt", "v2\n");
    area.forklift("wh", &["load", "app.txt"]);
    area.forklift("wh", &["stack", "-m", "second"]);

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
            RedactionsPrecondition::NotConsumed,
            None,
            None,
            &forklift_aws_lambda::format::gate(&*store).expect("format gate"),
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
    area.forklift("wh", &["stack", "-m", "signed one"]);

    let main_v1 = harvest(&area.path("wh")).head_of("main").expect("main v1");

    area.write_file("wh/app.txt", "v2\n");
    area.forklift("wh", &["load", "app.txt"]);
    area.forklift("wh", &["stack", "-m", "signed two"]);

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
/// never refuse this commit — the precondition is scoped to exactly the inputs the checks read
/// (the target pallet, the office pallet, `@redactions`, the trust anchor), never to "nothing
/// else in the warehouse moved" (FORK-95 design memo, "The movable inputs").
#[test]
fn a_concurrently_moved_unrelated_pallet_does_not_refuse_the_commit() {
    let area = Area::new("unrelated-pallet-races");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "-m", "signed one"]);

    let main_v1 = harvest(&area.path("wh")).head_of("main").expect("main v1");

    area.write_file("wh/app.txt", "v2\n");
    area.forklift("wh", &["load", "app.txt"]);
    area.forklift("wh", &["stack", "-m", "signed two"]);

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
                RedactionsPrecondition::NotConsumed,
                Some(&anchor_bytes),
                None,
                &forklift_aws_lambda::format::gate(&head.refs).expect("format gate"),
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
        store.put_trust_if_absent(&anchor_v1.to_anchor(), &forklift_aws_lambda::format::gate(&*store).expect("format gate"), &forklift_aws_lambda::reissue::request_token()).expect("plant the incumbent anchor"),
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
                RedactionsPrecondition::NotConsumed,
                Some(&bytes),
                None,
                &forklift_aws_lambda::format::gate(&*store).expect("format gate"),
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
                RedactionsPrecondition::NotConsumed,
                Some(&bytes),
                None,
                &forklift_aws_lambda::format::gate(&*store).expect("format gate"),
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
    fn read_format(&self) -> Result<Option<String>, String> {
        self.inner.read_format()
    }

    fn partition_has_items(&self) -> Result<bool, String> {
        self.inner.partition_has_items()
    }

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
        redactions: RedactionsPrecondition<'_>,
        anchor: Option<&str>,
        chunk_fence: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.compare_and_set_head(namespace, name, expected, new, office_head, redactions, anchor, chunk_fence, format, request_token)
    }

    fn delete_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        old: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.delete_head(namespace, name, old, office_head, anchor, format, request_token)
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
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<TrustOutcome, String> {
        self.inner.put_trust_if_absent(anchor, format, request_token)
    }

    fn replace_trust(
        &self,
        anchor: &office_utils::TrustAnchor,
        expected_anchor: &str,
        office_head: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
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

        self.inner.replace_trust(anchor, expected_anchor, office_head, format, request_token)
    }

    fn get_item(&self, key: forklift_aws_lambda::store::ItemKey<'_>) -> Result<Option<String>, String> {
        self.inner.get_item(key)
    }

    fn list_withheld(&self) -> Result<Vec<(String, String)>, String> {
        self.inner.list_withheld()
    }

    fn put_items(
        &self,
        writes: &[forklift_aws_lambda::store::ItemWrite<'_>],
        format: &forklift_aws_lambda::FormatFence,
    ) -> Result<forklift_aws_lambda::store::RedactionWrite, String> {
        self.inner.put_items(writes, format)
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
    fn read_format(&self) -> Result<Option<String>, String> {
        self.inner.read_format()
    }

    fn partition_has_items(&self) -> Result<bool, String> {
        self.inner.partition_has_items()
    }

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
        redactions: RedactionsPrecondition<'_>,
        anchor: Option<&str>,
        chunk_fence: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.compare_and_set_head(namespace, name, expected, new, office_head, redactions, anchor, chunk_fence, format, request_token)
    }

    fn delete_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        old: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.delete_head(namespace, name, old, office_head, anchor, format, request_token)
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
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<TrustOutcome, String> {
        self.inner.put_trust_if_absent(anchor, format, request_token)
    }

    fn replace_trust(
        &self,
        _anchor: &office_utils::TrustAnchor,
        _expected_anchor: &str,
        _office_head: Option<&str>,
        _format: &forklift_aws_lambda::FormatFence,
        _request_token: &str,
    ) -> Result<TrustWriteOutcome, String> {
        Ok(TrustWriteOutcome::AnchorMoved { current: self.current.clone() })
    }

    fn get_item(&self, key: forklift_aws_lambda::store::ItemKey<'_>) -> Result<Option<String>, String> {
        self.inner.get_item(key)
    }

    fn list_withheld(&self) -> Result<Vec<(String, String)>, String> {
        self.inner.list_withheld()
    }

    fn put_items(
        &self,
        writes: &[forklift_aws_lambda::store::ItemWrite<'_>],
        format: &forklift_aws_lambda::FormatFence,
    ) -> Result<forklift_aws_lambda::store::RedactionWrite, String> {
        self.inner.put_items(writes, format)
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
    area.forklift("wh", &["stack", "-m", "signed"]);

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

/// A first anchor's adopted history is held strictly: one naming content a redaction applied
/// here took away is refused, as on the served root, since every warehouse that adopts the pin
/// fetches it whole from here and the head can never serve it.
// Revert: tolerate the applied redactions in `require_adopted_history` → the anchor lands → red.
#[test]
fn a_first_anchor_adopting_history_naming_redacted_content_is_refused() {
    let area = Area::new("first-anchor-adopts-redacted");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "-m", "signed"]);

    let office = harvest(&area.path("wh"));
    let office_head = office.head_of(&format!("@{}", OFFICE_PALLET_NAME)).expect("office head");
    let (record, content) = {
        let _scope = StorageRootScope::enter(&area.path("wh"));
        let mut tree = object_utils::load_tree(&object_utils::load_parcel(&office_head).expect("parcel").tree_hash)
            .expect("the office tree");
        let record = loop {
            if let Some((_, file)) = tree.get_files().next() {
                break file.hash.clone();
            }
            let (_, subtree) = tree.get_subtrees().next().expect("the office tree names a record");
            tree = object_utils::load_tree(&subtree.hash).expect("a subtree");
        };
        let content = String::from_utf8(object_utils::load_blob(&record).expect("the record").content).expect("text");
        (record, content)
    };

    // A meta pallet's content is never redacted where its history names it, so the redaction is
    // recorded in a warehouse where only a file holds the same bytes.
    let (redactions, recorded) = recorded_redactions(&area, "red", &[&content]);
    assert_eq!(blob_hash(&content), record);

    let anchor = TrustAnchorDto {
        genesis: "e".repeat(64),
        adopts: Some(office_head.clone()),
        ..office.trust.clone().expect("trust")
    };

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    // Every object but the content the redaction took away.
    for harvest in [&recorded, &office] {
        for (hash, bytes) in harvest.objects.iter().filter(|(hash, _)| **hash != record) {
            head.object_put(&Principal::Static, None, hash, bytes).expect("upload object");
        }
        for (hash, sidecar) in &harvest.signatures {
            head.signature_put(&Principal::Static, hash, sidecar).expect("upload signature");
        }
    }
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions[0]);
    plant_item(&head.refs, &record, &forklift_aws_lambda::head::redaction_record(true, &[], Some(1)));

    let err = head.put_trust(&Principal::Static, &anchor).expect_err("the pin names redacted content");
    assert_eq!(err.status, Status::Unprocessable, "{}", err.message);
    assert!(err.message.contains(&record), "the refusal names the content: {}", err.message);
}

/// A first anchor adopting a head that is not office history is refused even with its history
/// held: clients refuse such an anchor, and a stored anchor is a one-way door.
// Revert: drop the office-shape check from `require_adopted_history` → the anchor is stored.
#[test]
fn a_first_anchor_adopting_anything_but_office_history_is_refused() {
    let area = Area::new("first-anchor-adopts-user-head");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "-m", "signed"]);

    let harvest = harvest(&area.path("wh"));
    let user_head = harvest.head_of("main").expect("main head");
    let anchor = TrustAnchorDto {
        genesis: "e".repeat(64),
        adopts: Some(user_head.clone()),
        ..harvest.trust.clone().expect("trust")
    };

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &harvest);
    let err = head.put_trust(&Principal::Static, &anchor).expect_err("not office history");
    assert_eq!(err.status, Status::Unprocessable, "{}", err.message);
    assert!(err.message.contains("not office history") && err.message.contains(&user_head), "{}", err.message);
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
    area.forklift("wh", &["stack", "-m", "first"]);
    area.forklift("wh", &["pallet", "create", "feature"]);
    area.write_file("wh/app.txt", "v2\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "-m", "second"]);
    let feature_v2 = harvest(&area.path("wh")).head_of("feature").expect("feature v2");
    area.write_file("wh/app.txt", "v3\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "-m", "third"]);

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
        ("pallet_deleted_on_remote".to_string(), "forklift pallet restore feature --remote".to_string())
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
    area.forklift("wh", &["stack", "-m", "signed"]);
    area.forklift("wh", &["pallet", "create", "feature"]);
    area.write_file("wh/feature.txt", "x\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "-m", "feature"]);
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
    fn read_format(&self) -> Result<Option<String>, String> {
        self.inner.read_format()
    }

    fn partition_has_items(&self) -> Result<bool, String> {
        self.inner.partition_has_items()
    }

    fn get_head(&self, namespace: pallet_utils::PalletNamespace, name: &str) -> Result<RefHead, String> {
        match &self.race {
            DeleteRace::TransientThenUnreadable(_, attempted) if attempted.get() => Err("throttled.".to_string()),
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
        redactions: RedactionsPrecondition<'_>,
        anchor: Option<&str>,
        chunk_fence: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.compare_and_set_head(namespace, name, expected, new, office_head, redactions, anchor, chunk_fence, format, request_token)
    }

    fn delete_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        old: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        match &self.race {
            DeleteRace::Before(race) => {
                if let Some(race) = race.borrow_mut().take() {
                    race(&self.inner);
                }
                self.inner.delete_head(namespace, name, old, office_head, anchor, format, request_token)
            }
            DeleteRace::LandsThenTransient => {
                self.inner.delete_head(namespace, name, old, office_head, anchor, format, request_token)?;
                Ok(CasOutcome::Transient(TransientCause::FateUnknown))
            }
            DeleteRace::Transient => Ok(CasOutcome::Transient(TransientCause::FateUnknown)),
            DeleteRace::ThenFault { attempted, .. } if attempted.replace(true) => Err("a fault".to_string()),
            DeleteRace::ThenFault { first, lands, .. } => {
                if *lands {
                    self.inner.delete_head(namespace, name, old, office_head, anchor, format, request_token)?;
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

    fn put_trust_if_absent(
        &self,
        anchor: &office_utils::TrustAnchor,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<TrustOutcome, String> {
        self.inner.put_trust_if_absent(anchor, format, request_token)
    }

    fn replace_trust(
        &self,
        anchor: &office_utils::TrustAnchor,
        expected_anchor: &str,
        office_head: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<TrustWriteOutcome, String> {
        self.inner.replace_trust(anchor, expected_anchor, office_head, format, request_token)
    }

    fn get_item(&self, key: forklift_aws_lambda::store::ItemKey<'_>) -> Result<Option<String>, String> {
        self.inner.get_item(key)
    }

    fn list_withheld(&self) -> Result<Vec<(String, String)>, String> {
        self.inner.list_withheld()
    }

    fn put_items(
        &self,
        writes: &[forklift_aws_lambda::store::ItemWrite<'_>],
        format: &forklift_aws_lambda::FormatFence,
    ) -> Result<forklift_aws_lambda::store::RedactionWrite, String> {
        self.inner.put_items(writes, format)
    }
}

/// `feature` lifted at `feature_v2` in a shared store, and a head over that store that races its
/// delete as `race` says. Built from a [`feature_warehouse`] the caller harvests once, so a test
/// that races several deletes builds one warehouse rather than one per race.
fn racing_head(warehouse: &(Harvest, String, String, String), race: impl FnOnce(String, String) -> DeleteRace) -> (Arc<MemoryRefStore>, Head<MemoryObjectStore, RacesTheDelete>, String, String) {
    let (harvest, main_head, feature_v2, feature_v3) = warehouse;
    let store = Arc::new(MemoryRefStore::new());
    lifted_feature(SharedMemoryRefs(store.clone()), harvest, main_head, feature_v2);

    let head = Head::new(MemoryObjectStore::new(), RacesTheDelete { inner: store.clone(), race: race(feature_v2.clone(), feature_v3.clone()) });
    upload_all(&head, harvest);
    (store, head, feature_v2.clone(), feature_v3.clone())
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
                RedactionsPrecondition::NotConsumed,
                None,
                None,
                &forklift_aws_lambda::format::gate(&*store).expect("format gate"),
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
    let warehouse = feature_warehouse(&Area::new("delete-races-lift"));
    let (store, head, feature_v2, feature_v3) =
        racing_head(&warehouse, |from, to| DeleteRace::Before(RefCell::new(Some(racing_lift(from, to)))));

    let err = head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect_err("the pallet moved");
    assert_eq!(err.status, Status::Conflict);
    assert_eq!(code_and_next_step(&err.message).0, "remote_ahead", "{}", err.message);
    assert_eq!(store.get_head(pallet_utils::PalletNamespace::User, "feature").expect("get"), RefHead::Live(feature_v3));
}

/// Two deletes from one head: the one whose commit finds the other's tombstone answers `200`,
/// since that tombstone is what it asked for.
#[test]
fn of_two_racing_deletes_the_loser_answers_200() {
    let warehouse = feature_warehouse(&Area::new("delete-races-delete"));
    let (store, head, feature_v2, _) = racing_head(&warehouse, |from, _| {
        DeleteRace::before(move |store: &MemoryRefStore| {
            let outcome = store
                .delete_head(pallet_utils::PalletNamespace::User, "feature", &from, OfficePrecondition::Unborn, None, &forklift_aws_lambda::format::gate(&*store).expect("format gate"), &request_token())
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
///
/// Over the Windows time budget until FORK-384 (9.9 s and 10.1 s on the Windows CI runner in two
/// runs): it races eight deletes, each through a head of its own over one built warehouse, and 1.0 s
/// of its 2.8 s alone on an M-series Mac is syncs, which FORK-384 tracks for the scratch mirror.
#[test]
fn a_delete_of_unknown_fate_is_settled_by_the_ref() {
    let warehouse = feature_warehouse(&Area::new("delete-fate-unknown"));
    let (store, head, feature_v2, _) = racing_head(&warehouse, |_, _| DeleteRace::LandsThenTransient);
    head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect("an attempt landed");
    assert_eq!(store.get_head(pallet_utils::PalletNamespace::User, "feature").expect("get"), RefHead::Deleted(feature_v2));
    let (store, head, feature_v2, _) = racing_head(&warehouse, |_, _| DeleteRace::Transient);
    let err = head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect_err("nothing landed");
    assert_eq!(err.status, Status::ServiceUnavailable, "{}", err.message);
    assert!(err.message.contains(&format!("The delete was for \"feature\" at {}.", feature_v2)), "{}", err.message);
    assert!(err.message.contains(&format!("still live at {} when", feature_v2)), "{}", err.message);
    assert!(
        err.message.ends_with("Check the pallet's head now (\"forklift pallet list --remote\" shows it) before deleting \"feature\" again."),
        "{}",
        err.message
    );
    assert_eq!(store.get_head(pallet_utils::PalletNamespace::User, "feature").expect("get"), RefHead::Live(feature_v2));

    // A ref that cannot be read leaves the fate where the attempts left it.
    for (cause, says) in [
        (TransientCause::FateUnknown, "could not be confirmed"),
        (TransientCause::Contended, "None of its attempts was applied"),
    ] {
        let (_, head, feature_v2, _) = racing_head(&warehouse, |_, _| DeleteRace::TransientThenUnreadable(cause, Cell::new(false)));
        let err = head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect_err("the ref cannot be read");
        assert_eq!(err.status, Status::ServiceUnavailable, "{}", err.message);
        assert!(err.message.contains(says), "{}", err.message);
        assert!(err.message.contains(&format!("The delete was for \"feature\" at {}.", feature_v2)), "{}", err.message);
        assert!(err.message.contains("failed too (throttled)."), "the read's own fault is named: {}", err.message);
    }

    // A fault on the re-issue does not hide an attempt of unknown fate before it: the ref decides
    // whether it landed. After contention the fault is the answer.
    let fault = |first, lands| move |_: String, _: String| DeleteRace::ThenFault { first, lands, attempted: Cell::new(false) };
    let (_, head, feature_v2, _) = racing_head(&warehouse, fault(TransientCause::FateUnknown, true));
    head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect("the first attempt landed");
    let (_, head, feature_v2, _) = racing_head(&warehouse, fault(TransientCause::FateUnknown, false));
    let err = head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect_err("nothing landed");
    assert_eq!(err.status, Status::ServiceUnavailable, "{}", err.message);
    assert!(err.message.contains("could not be confirmed"), "{}", err.message);
    assert!(err.message.contains(&format!("The delete was for \"feature\" at {}.", feature_v2)), "{}", err.message);
    assert!(!err.message.contains("None of its attempts was applied"), "{}", err.message);
    assert!(err.message.contains(&format!("still live at {} when", feature_v2)), "{}", err.message);
    let (_, head, feature_v2, _) = racing_head(&warehouse, fault(TransientCause::Contended, false));
    let err = head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect_err("the fault");
    assert_eq!(err.status, Status::Internal, "{}", err.message);
    let (_, head, feature_v2, _) =
        racing_head(&warehouse, |from, to| DeleteRace::TransientAfter(RefCell::new(Some(racing_lift(from, to)))));
    let err = head.delete_pallet(&Principal::Static, "feature", &feature_v2).expect_err("the pallet moved");
    assert_eq!(err.status, Status::Conflict, "{}", err.message);
    assert_eq!(code_and_next_step(&err.message).0, "remote_ahead", "{}", err.message);
}

/// Enroll `identifier` in `dir`'s office with `role_args` (`office role`'s arguments after the
/// identifier), from a key generated under that identifier, then switch back to the admin.
fn enroll(area: &Area, dir: &str, identifier: &str, role_args: &[&str]) {
    area.forklift(dir, &["config", "set", "operator.identifier", identifier]);
    let keygen = area.forklift_stdout(dir, &["profile", "keygen"]);
    area.forklift(dir, &["config", "set", "operator.identifier", "tester@forklift"]);

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
    area.forklift("wh", &["stack", "-m", "signed"]);
    area.forklift("wh", &["pallet", "create", "feature"]);
    area.write_file("wh/app.txt", "v2\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "-m", "feature"]);
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
///
/// Over the Windows time budget until FORK-384 (10.4 s and 11.2 s on the Windows CI runner in two runs):
/// most of its time is syncs, 1.4 s alone on an M-series Mac with fsync on and 0.24 s with it off.
/// FORK-384 tracks the per-object syncs of the scratch mirror these tests write through.
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
                RedactionsPrecondition::NotConsumed,
                anchor.as_deref(),
                None,
                &forklift_aws_lambda::format::gate(&*store).expect("format gate"),
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

// ---------------------------------------------------------------------------------------
// Applying redactions: holder verdicts, the apply, and the refusals.
// ---------------------------------------------------------------------------------------

/// A memory head holding every object of a warehouse that recorded a redaction of `content`,
/// with no pallet and no `@redactions` head planted yet: the area, the head, the `@redactions`
/// head to plant, and the redacted hash.
fn unplanted_redaction(name: &str, content: &str) -> (Area, Head<MemoryObjectStore, MemoryRefStore>, String, String) {
    let area = Area::new(name);
    let (redactions, recorded) = recorded_redactions(&area, "red", &[content]);
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &recorded);

    (area, head, redactions[0].clone(), blob_hash(content))
}

/// Assert `err` is the applied redaction's `410`, naming `hash`.
fn assert_gone(err: &forklift_aws_lambda::error::HeadError, hash: &str) {
    assert_eq!(err.status, Status::Gone, "{}", err.message);
    assert_eq!(refusal_code(&err.message), Some(RefusalCode::ContentRedacted), "{}", err.message);
    assert!(err.message.contains(hash), "the refusal names the hash: {}", err.message);
}

/// Where the handshake says the redaction of `hash` stands.
fn served_state(head: &Head<MemoryObjectStore, MemoryRefStore>, hash: &str) -> Option<ServedRedactionState> {
    let info = head.handshake().expect("handshake");
    let redactions = info.redactions.expect("the head reports its redactions");
    redactions.into_iter().find(|served| served.hash == hash).map(|served| served.state)
}

/// Whether the object `hash` is served.
fn served<O: ObjectStore, R: RefStore>(head: &Head<O, R>, hash: &str) -> bool {
    match head.object_get(hash) {
        Ok(_) => true,
        Err(err) if err.status == Status::Gone => false,
        Err(err) => panic!("object {} answered {}: {}", hash, err.status.as_u16(), err.message),
    }
}

/// A redaction nothing holds applies on the first request for its content, and its content is then
/// refused on every route that serves or takes it by hash, while other content is served.
// Revert: drop the judge from `object_get` → the content is served → red. Revert: drop it from
// `batch` → the bundle carries it → red.
#[test]
fn an_unheld_redaction_applies_and_its_content_is_refused_on_every_route() {
    let (area, head, redactions, leak) = unplanted_redaction("redaction-applies", "leaked\n");
    let clean = gate_parcel(&head, &[], vec![gate_file(&head, "x", "1")]);
    let other = blob_hash("1");
    plant(&head.refs, "main", &clean);
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions);

    assert_gone(&head.object_get(&leak).expect_err("applied content is not served"), &leak);
    assert!(served(&head, &other));

    let err = head.object_put(&Principal::Static, None, &leak, b"leaked\n").expect_err("nor taken again");
    assert_gone(&err, &leak);
    let Err(err) = head.upload_targets(&Principal::Static, "lift-1", &[other.clone(), leak.clone()]) else {
        panic!("applied content is not negotiated");
    };
    assert_gone(&err, &leak);

    let requested = vec![leak.clone(), other.clone()];
    let answer = head.batch(&requested).expect("batch");
    assert_eq!(bundled(&area, "fetch", answer, &requested), vec![other]);

    assert_eq!(served_state(&head, &leak), Some(ServedRedactionState::DeletionOwed));
    assert!(forklift_aws_lambda::head::redaction_applied(&head.refs, &leak).expect("item"));
}

/// A redaction a pallet's head names stays pending, and its content served, until that pallet moves
/// off the head it was seen at; the move's own write applies it.
// Revert: judge a holder free whatever its head → the content is refused while held → red. Revert:
// drop the judge after a committed ref update → the handshake still says pending → red.
#[test]
fn a_redaction_a_head_names_is_served_until_that_head_moves() {
    let (_area, head, redactions, leak) = unplanted_redaction("redaction-held", "leaked\n");
    let holding = gate_parcel(&head, &[], vec![gate_file(&head, "secret.env", "leaked\n")]);
    plant(&head.refs, "main", &holding);
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions);

    assert!(served(&head, &leak), "a held redaction is served");
    assert_eq!(served_state(&head, &leak), Some(ServedRedactionState::Pending));

    let clean = gate_parcel(&head, &[&holding], vec![gate_file(&head, "x", "1")]);
    let request = RefUpdateRequest { old_head: Some(holding), new_head: clean, tombstone: None };
    head.ref_update(&Principal::Static, "main", &request).expect("the clean head lifts");

    assert!(forklift_aws_lambda::head::redaction_applied(&head.refs, &leak).expect("item"), "the write applied it");
    assert!(!served(&head, &leak));
}

/// A deleted pallet's tombstone still holds what its head named: the content can come back by a
/// restore, so the redaction stays pending.
// Revert: judge user holders by their live heads alone → the redaction applies → red.
#[test]
fn a_tombstone_holds_what_its_head_named() {
    let (_area, head, redactions, leak) = unplanted_redaction("redaction-tombstone", "leaked\n");
    let holding = gate_parcel(&head, &[], vec![gate_file(&head, "secret.env", "leaked\n")]);
    plant(&head.refs, "side", &holding);
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions);

    assert!(served(&head, &leak));
    head.delete_pallet(&Principal::Static, "side", &holding).expect("delete side");

    assert!(served(&head, &leak), "the tombstone holds it");
    assert_eq!(served_state(&head, &leak), Some(ServedRedactionState::Pending));
}

/// A meta pallet seen naming the content holds it while its head descends from the head seen, its
/// history still naming it; a head that does not descend from it is scanned again, and holds
/// nothing.
// Revert: free a moved meta holder unchecked → the descendant does not hold → red. Revert: hold
// on any moved meta holder → the unrelated head still holds → red. Revert: keep the head first
// seen → the item never names the descendant → red.
#[test]
fn a_meta_holder_holds_while_its_head_descends_from_the_one_seen() {
    let (_area, head, redactions, leak) = unplanted_redaction("redaction-meta", "leaked\n");
    let first = gate_parcel(&head, &[], vec![gate_file(&head, "leak", "leaked\n")]);
    plant_meta(&head.refs, "tags", None, &first);
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions);

    assert!(served(&head, &leak), "the meta pallet's head names it");

    let descendant = gate_parcel(&head, &[&first], vec![gate_file(&head, "x", "2")]);
    plant_meta(&head.refs, "tags", Some(&first), &descendant);
    assert!(served(&head, &leak), "its history still names it");
    let item = head.refs.get_redaction(&leak).expect("item").expect("the scan wrote the item");
    assert!(item.contains(&descendant), "the descendant is recorded as seen, so it is walked once: {}", item);

    let unrelated = gate_parcel(&head, &[], vec![gate_file(&head, "y", "3")]);
    plant_meta(&head.refs, "tags", Some(&descendant), &unrelated);
    assert!(!served(&head, &leak), "a head off that history holds nothing");
}

/// A redaction many pallets name is decided in one request: each holder the scan finds is the
/// request's own progress, never counted as a write lost to another request.
// Revert: count every item write against the lost-write bound → the request answers `503` →
// red.
#[test]
fn a_redaction_many_pallets_name_is_decided_in_one_request() {
    let (_area, head, redactions, leak) = unplanted_redaction("redaction-many-holders", "leaked\n");
    for pallet in 0..12 {
        let parcel = gate_parcel(&head, &[], vec![gate_file(&head, "secret.env", "leaked\n"), gate_file(&head, "n", &pallet.to_string())]);
        plant(&head.refs, &format!("p{}", pallet), &parcel);
    }
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions);

    assert!(served(&head, &leak), "twelve holders keep it pending, decided in this request");
}

/// An object store that holds a tree's bytes under `hash`, the hash a redaction records, and
/// delegates everything else.
struct PosesAsATree {
    inner: MemoryObjectStore,
    hash: String,
    tree: Vec<u8>,
}

impl ObjectStore for PosesAsATree {
    fn exists(&self, hash: &str) -> Result<bool, String> {
        Ok(hash == self.hash || self.inner.exists(hash)?)
    }

    fn get(&self, hash: &str) -> Result<Option<Vec<u8>>, String> {
        match hash == self.hash {
            true => Ok(Some(self.tree.clone())),
            false => self.inner.get(hash),
        }
    }

    fn objects_staged(&self, session: &str, hashes: &[String]) -> Result<Vec<String>, String> {
        self.inner.objects_staged(session, hashes)
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

    fn work_get(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        self.inner.work_get(name)
    }

    fn work_put(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        self.inner.work_put(name, bytes)
    }

    fn work_delete(&self, name: &str) -> Result<(), String> {
        self.inner.work_delete(name)
    }
}

/// A redaction naming an object the store holds as a tree never applies, as core never applies
/// it: nothing holds it, yet it stays pending, and the object is served.
// Revert: answer such a redaction undecided → the object answers `503` → red. Revert: drop the
// write keeping that verdict → the item never says so → red.
#[test]
fn a_redaction_naming_a_tree_stays_pending_and_is_served() {
    use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
    use forklift_core::enums::dir_entry_type::DirEntryType;

    let area = Area::new("redaction-tree");
    let (redactions, recorded) = recorded_redactions(&area, "red", &["leaked\n"]);
    let leak = blob_hash("leaked\n");
    let tree = LooseObjectBuilder::build_tree(&forklift_core::model::tree_item::TreeItem::new(String::new(), String::new(), DirEntryType::Tree));
    let head = Head::new(PosesAsATree { inner: MemoryObjectStore::new(), hash: leak.clone(), tree: tree.content }, MemoryRefStore::new());
    upload_all(&head, &recorded);
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions[0]);

    assert!(served(&head, &leak), "a tree's redaction never applies, and is served");
    let item = head.refs.get_redaction(&leak).expect("item").expect("the judge wrote the item");
    assert!(item.contains("\"never_applies\":true"), "kept, so no later judge reads the object again: {}", item);
    let info = head.handshake().expect("handshake");
    let state = info.redactions.expect("redactions").into_iter().find(|served| served.hash == leak).map(|served| served.state);
    assert_eq!(state, Some(ServedRedactionState::Pending));
    assert!(!forklift_aws_lambda::head::redaction_applied(&head.refs, &leak).expect("item"));
}

/// An applied redaction reads as applied even once the request's time is spent: its item is read
/// before the deadline is looked at, so it is refused as `410`, and the handshake says
/// `deletion_owed`, never `pending`.
// Revert: look at the deadline before reading the item → `503`, and `pending` → red.
#[test]
fn an_applied_redaction_reads_as_applied_after_the_deadline() {
    let (_area, head, redactions, leak) = unplanted_redaction("redaction-late", "leaked\n");
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions);
    assert!(!served(&head, &leak), "nothing holds it: it applies");

    let late = head.with_deadline(std::time::SystemTime::now());
    assert_gone(&late.object_get(&leak).expect_err("still refused"), &leak);
    let info = late.handshake().expect("handshake");
    let state = info.redactions.expect("redactions").into_iter().find(|served| served.hash == leak).map(|served| served.state);
    assert_eq!(state, Some(ServedRedactionState::DeletionOwed));
}

/// A request whose time is spent starts no scan: the redaction answers `503`, and its item is left
/// for a request with time to start it.
// Revert: start the scan whatever the time → the item is written → red.
#[test]
fn a_request_out_of_time_starts_no_scan() {
    let (_area, head, redactions, leak) = unplanted_redaction("redaction-no-start", "leaked\n");
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions);

    let late = head.with_deadline(std::time::SystemTime::now());
    let err = late.object_get(&leak).expect_err("undecided");
    assert_eq!(err.status, Status::ServiceUnavailable, "{}", err.message);
    assert_eq!(late.refs.get_redaction(&leak).expect("item"), None, "no scan was started");
}

/// An object store whose `n`th read of a pallet walk fails, to cut a scan off part way.
struct FailsAWalkRead {
    inner: MemoryObjectStore,
    reads: AtomicU64,
    fail_at: u64,
}

impl ObjectStore for FailsAWalkRead {
    fn exists(&self, hash: &str) -> Result<bool, String> {
        self.inner.exists(hash)
    }

    fn get(&self, hash: &str) -> Result<Option<Vec<u8>>, String> {
        self.inner.get(hash)
    }

    fn objects_staged(&self, session: &str, hashes: &[String]) -> Result<Vec<String>, String> {
        self.inner.objects_staged(session, hashes)
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

    fn work_get(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        if name.contains("/walk-") && self.reads.fetch_add(1, Ordering::SeqCst) + 1 == self.fail_at {
            return Err("the work read failed".to_string());
        }

        self.inner.work_get(name)
    }

    fn work_put(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        self.inner.work_put(name, bytes)
    }

    fn work_delete(&self, name: &str) -> Result<(), String> {
        self.inner.work_delete(name)
    }
}

/// A scan cut off by a failed store call keeps the pallets it walked: the list of pallets still
/// to walk is saved on that way out too, so the next request goes on after them.
// Revert: save the list only on the walk's own stops → the error keeps every pallet → red.
#[test]
fn a_scan_cut_off_by_a_store_error_keeps_what_it_walked() {
    let area = Area::new("redaction-cut-off");
    let (redactions, recorded) = recorded_redactions(&area, "red", &["leaked\n"]);
    let leak = blob_hash("leaked\n");
    let head = Head::new(FailsAWalkRead { inner: MemoryObjectStore::new(), reads: AtomicU64::new(0), fail_at: 3 }, MemoryRefStore::new());
    upload_all(&head, &recorded);
    for pallet in 0..4 {
        let parcel = gate_parcel(&head, &[], vec![gate_file(&head, "n", &pallet.to_string())]);
        plant(&head.refs, &format!("p{}", pallet), &parcel);
    }
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions[0]);
    let listing = head.refs.list_refs().expect("list");
    let pallets = listing.live.len() + listing.tombstones.len();

    let err = head.object_get(&leak).expect_err("the third walk's read fails");
    assert_eq!(err.status, Status::Internal, "{}", err.message);

    let saved = head.objects.inner.work_get(&format!("{}/pallets", leak)).expect("work").expect("the list is kept");
    let saved: serde_json::Value = serde_json::from_slice(&saved).expect("a list");
    assert_eq!(saved["pallets"].as_array().expect("pallets").len(), pallets - 2, "the two walked pallets are off it");
}

/// A list of pallets saved for an earlier scan, by a request that read an older record, is never
/// walked by the rescan that followed: the pallets are listed again, so a moved meta holder whose
/// new head still names the content keeps holding it.
// Revert: walk whatever list is stored, whichever scan it was saved for → the stale empty list
// completes the rescan unwalked and the redaction applies → red.
#[test]
fn a_list_saved_for_an_earlier_scan_is_never_walked() {
    let (_area, head, redactions, leak) = unplanted_redaction("redaction-stale-list", "leaked\n");
    let first = gate_parcel(&head, &[], vec![gate_file(&head, "leak", "leaked\n")]);
    plant_meta(&head.refs, "tags", None, &first);
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions);
    assert!(served(&head, &leak), "the meta pallet's head names it");

    // Reset off that history to a head that names the content all the same; a one-read budget
    // finds the reset and starts the rescan, and stops before it walks the new head.
    let rewritten = gate_parcel(&head, &[], vec![gate_file(&head, "leak", "leaked\n"), gate_file(&head, "y", "3")]);
    plant_meta(&head.refs, "tags", Some(&first), &rewritten);
    let head = head.with_scan_step_limit(1);
    match head.object_get(&leak) {
        Err(err) if err.status == Status::ServiceUnavailable => {}
        other => panic!("the rescan cannot finish in one read: {:?}", other.map(|_| ()).map_err(|err| err.message)),
    }

    // A request that read the first scan's record saves its emptied list over the rescan's.
    head.objects.work_put(&format!("{}/pallets", leak), br#"{"version":1,"scan":0,"pallets":[]}"#).expect("put");

    let head = head.with_scan_step_limit(usize::MAX);
    assert!(served(&head, &leak), "the rewritten head still names it");
}

/// A ref store on which every write of a rescan's record loses, as to another request rescanning
/// from the same record.
struct LosesEveryRescan {
    inner: Arc<MemoryRefStore>,
}

impl RefStore for LosesEveryRescan {
    fn read_format(&self) -> Result<Option<String>, String> {
        self.inner.read_format()
    }

    fn partition_has_items(&self) -> Result<bool, String> {
        self.inner.partition_has_items()
    }

    fn get_head(&self, namespace: pallet_utils::PalletNamespace, name: &str) -> Result<RefHead, String> {
        self.inner.get_head(namespace, name)
    }

    fn compare_and_set_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        expected: ExpectedHead<'_>,
        new: &str,
        office_head: OfficePrecondition<'_>,
        redactions: RedactionsPrecondition<'_>,
        anchor: Option<&str>,
        chunk_fence: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.compare_and_set_head(namespace, name, expected, new, office_head, redactions, anchor, chunk_fence, format, request_token)
    }

    fn delete_head(
        &self,
        namespace: pallet_utils::PalletNamespace,
        name: &str,
        old: &str,
        office_head: OfficePrecondition<'_>,
        anchor: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<CasOutcome, String> {
        self.inner.delete_head(namespace, name, old, office_head, anchor, format, request_token)
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
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<TrustOutcome, String> {
        self.inner.put_trust_if_absent(anchor, format, request_token)
    }

    fn replace_trust(
        &self,
        anchor: &office_utils::TrustAnchor,
        expected_anchor: &str,
        office_head: Option<&str>,
        format: &forklift_aws_lambda::FormatFence,
        request_token: &str,
    ) -> Result<TrustWriteOutcome, String> {
        self.inner.replace_trust(anchor, expected_anchor, office_head, format, request_token)
    }

    fn get_item(&self, key: forklift_aws_lambda::store::ItemKey<'_>) -> Result<Option<String>, String> {
        self.inner.get_item(key)
    }

    fn list_withheld(&self) -> Result<Vec<(String, String)>, String> {
        self.inner.list_withheld()
    }

    fn put_items(
        &self,
        writes: &[forklift_aws_lambda::store::ItemWrite<'_>],
        format: &forklift_aws_lambda::FormatFence,
    ) -> Result<forklift_aws_lambda::store::RedactionWrite, String> {
        if writes.iter().any(|write| write.state.contains("\"scan\":1")) {
            return Ok(forklift_aws_lambda::store::RedactionWrite::Moved);
        }

        self.inner.put_items(writes, format)
    }
}

/// A request whose rescan's record write loses saves no list for that rescan: the winner computed
/// the same next scan, so the loser's list would pass for the winner's and be walked under it.
// Revert: save the rescan's list ahead of its record write → the lost rescan leaves its list →
// red.
#[test]
fn a_rescan_that_loses_its_record_write_saves_no_list() {
    let area = Area::new("redaction-lost-rescan");
    let (redactions, recorded) = recorded_redactions(&area, "red", &["leaked\n"]);
    let leak = blob_hash("leaked\n");
    let refs = Arc::new(MemoryRefStore::new());
    let head = Head::new(MemoryObjectStore::new(), LosesEveryRescan { inner: Arc::clone(&refs) });
    upload_all(&head, &recorded);
    let first = gate_parcel(&head, &[], vec![gate_file(&head, "leak", "leaked\n")]);
    plant_meta(&refs, "tags", None, &first);
    plant_meta(&refs, REDACTIONS_PALLET_NAME, None, &redactions[0]);
    assert!(served(&head, &leak), "the meta pallet's head names it");

    let unrelated = gate_parcel(&head, &[], vec![gate_file(&head, "y", "3")]);
    plant_meta(&refs, "tags", Some(&first), &unrelated);
    let err = head.object_get(&leak).expect_err("every rescan write loses");
    assert_eq!(err.status, Status::ServiceUnavailable, "{}", err.message);
    assert_eq!(head.objects.work_get(&format!("{}/pallets", leak)).expect("work"), None, "no list was saved for the lost rescan");
}

/// A scan the request cannot finish answers `503` with `Retry-After` and saves where it stopped,
/// so the requests after it go on from there, until the redaction applies and its work objects
/// are gone.
// Revert: start each walk over rather than load the saved one → no request finishes it → red.
#[test]
fn a_scan_spans_requests_and_goes_on_where_it_stopped() {
    let (_area, head, redactions, leak) = unplanted_redaction("redaction-resume", "leaked\n");
    let head = head.with_scan_step_limit(3);

    for pallet in 0..4 {
        let files = (0..3).map(|dir| gate_dir(&head, &format!("d{}", dir), vec![gate_file(&head, "f", &format!("{}-{}", pallet, dir))]));
        let parcel = gate_parcel(&head, &[], files.collect());
        plant(&head.refs, &format!("p{}", pallet), &parcel);
    }
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions);

    let mut undecided = 0;
    let outcome = loop {
        assert!(undecided < 50, "the scan never finished");

        match head.object_get(&leak) {
            Err(err) if err.status == Status::ServiceUnavailable => {
                assert_eq!(err.retry_after_seconds, Some(2), "{}", err.message);
                assert!(!head.objects.work_names().is_empty(), "the scan saved where it stopped");
                undecided += 1;
            }
            other => break other,
        }
    };

    assert!(undecided >= 2, "a 3-read budget cannot walk four pallets in one request");
    assert_gone(&outcome.expect_err("the redaction applied"), &leak);
    assert_eq!(head.objects.work_names(), Vec::<String>::new(), "a finished scan leaves no work behind");
}

/// A redaction recorded by a ref update starts its scan in that write, and applies there when
/// nothing holds it: no request for its content is needed.
// Revert: drop the judge after a committed ref update → no item exists → red.
#[test]
fn recording_a_redaction_applies_it_when_nothing_holds_it() {
    let area = Area::new("redaction-recorded");
    let (redactions, recorded) = recorded_redactions(&area, "red", &["one\n", "two\n"]);
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &recorded);
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions[0]);

    let wire = format!("@{}", REDACTIONS_PALLET_NAME);
    let request = RefUpdateRequest { old_head: Some(redactions[0].clone()), new_head: redactions[1].clone(), tombstone: None };
    head.ref_update(&Principal::Static, &wire, &request).expect("the second redaction lifts");

    for content in ["one\n", "two\n"] {
        let hash = blob_hash(content);
        assert!(forklift_aws_lambda::head::redaction_applied(&head.refs, &hash).expect("item"), "{}", content.trim());
    }
}

/// A redaction item of a version this build does not read fails every request for that content,
/// loudly, and is never written over: it may record an apply.
// Revert: read an unknown version as no item → the scan starts and the item is overwritten → red.
#[test]
fn a_redaction_item_of_an_unknown_version_is_a_loud_failure_never_pending() {
    let (_area, head, redactions, leak) = unplanted_redaction("redaction-unknown", "leaked\n");
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions);

    let future = r#"{"version":2,"applied_at":1}"#;
    let fence = forklift_aws_lambda::format::gate(&head.refs).expect("format gate");
    head.refs.put_redaction(&leak, future, None, &fence).expect("plant the item");

    let err = head.object_get(&leak).expect_err("unknown item");
    assert_eq!(err.status, Status::Internal, "{}", err.message);
    assert_eq!(head.refs.get_redaction(&leak).expect("item").as_deref(), Some(future));
    assert!(head.handshake().is_err(), "the handshake cannot say where it stands either");
}

/// A lift commit naming applied content drops its staged copy, never promoting it, and is refused.
// Revert: drop the check from `commit_lift` → the commit succeeds → red.
#[test]
fn a_lift_commit_naming_applied_content_drops_it_unpromoted() {
    let (_area, head, redactions, leak) = unplanted_redaction("redaction-commit", "leaked\n");
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions);
    assert!(!served(&head, &leak), "applied");

    for (control_plane, blobs) in [(vec![leak.clone()], vec![]), (vec![], vec![leak.clone()])] {
        head.objects.stage("lift-1", &leak, b"leaked\n".to_vec());

        let err = head.commit_lift(&Principal::Static, "lift-1", &control_plane, &blobs, false).expect_err("refused");
        assert_gone(&err, &leak);
        assert_eq!(head.objects.staged_count(), 0, "the staged copy is dropped");
    }
}

/// Plant `state` as the redaction item of `hash`, over whatever it holds.
fn plant_item(refs: &MemoryRefStore, hash: &str, state: &str) {
    let fence = forklift_aws_lambda::format::gate(refs).expect("format gate");
    let expected = refs.get_redaction(hash).expect("item");
    let outcome = refs.put_redaction(hash, state, expected.as_deref(), &fence).expect("plant the item");
    assert_eq!(outcome, forklift_aws_lambda::store::RedactionWrite::Written);
}

/// An applied redaction's first request deletes every copy the store keeps of the content: its
/// canonical object, and its staged copy in every session. Other content is kept, and the item
/// records the deletion done.
// Revert: drop the sweep from the judge → the object is still stored → red.
#[test]
fn an_applied_redaction_deletes_its_canonical_and_staged_copies() {
    let (_area, head, redactions, leak) = unplanted_redaction("redaction-deletes", "leaked\n");
    gate_file(&head, "x", "leaked\n");
    let kept = gate_file(&head, "y", "kept\n").hash;
    head.objects.stage("lift-1", &leak, b"leaked\n".to_vec());
    head.objects.stage("lift-2", &leak, b"leaked\n".to_vec());
    head.objects.stage("lift-1", &kept, b"kept\n".to_vec());
    assert!(head.objects.exists(&leak).expect("exists"));
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions);

    assert_gone(&head.object_get(&leak).expect_err("applied"), &leak);

    assert!(!head.objects.exists(&leak).expect("exists"), "the canonical copy is deleted");
    assert_eq!(head.objects.staged_count(), 1, "every staged copy of it is deleted, and only those");
    assert!(head.objects.exists(&kept).expect("exists"));
    let item = head.refs.get_redaction(&leak).expect("item").expect("an item");
    assert!(item.contains(r#""swept":true"#) && !item.contains("lease_until"), "{}", item);
    assert_eq!(served_state(&head, &leak), Some(ServedRedactionState::DeletionOwed));
}

/// A deletion another request holds the lease of is left to it; once the lease has run out, the
/// next request deletes.
// Revert: ignore the lease → the first request deletes → red. Revert: treat every lease as live →
// the second never deletes → red.
#[test]
fn a_held_deletion_lease_is_left_alone_until_it_runs_out() {
    let (_area, head, redactions, leak) = unplanted_redaction("redaction-lease", "leaked\n");
    gate_file(&head, "x", "leaked\n");
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions);

    let held = r#"{"version":2,"scan_complete":true,"holders":[],"applied_at":1,"lease_until":99999999999}"#;
    plant_item(&head.refs, &leak, held);
    assert_gone(&head.object_get(&leak).expect_err("applied"), &leak);
    assert!(head.objects.exists(&leak).expect("exists"), "another request holds the deletion");
    assert_eq!(head.refs.get_redaction(&leak).expect("item").as_deref(), Some(held));

    plant_item(&head.refs, &leak, r#"{"version":2,"scan_complete":true,"holders":[],"applied_at":1,"lease_until":1}"#);
    assert_gone(&head.object_get(&leak).expect_err("applied"), &leak);
    assert!(!head.objects.exists(&leak).expect("exists"), "the lease ran out: deleted");
}

/// The redaction item of `hash` as the judge leaves it after its first sweep, but applied `ago`
/// seconds before now: `extra` is spliced in as further fields.
fn applied_item(ago: u64, extra: &str) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock").as_secs();
    format!(r#"{{"version":2,"scan_complete":true,"holders":[],"applied_at":{}{}}}"#, now - ago, extra)
}

/// How long after its apply a redaction's late sweep is due: the Lambda ceiling, a presigned URL's
/// lifetime and the margin, written out here rather than read from the code under test.
const LATE_SWEEP_AFTER: u64 = 900 + 60 + 900 + 60;

/// An applied blob's late sweep, once a presigned URL's lifetime past the last request that could
/// have judged it pending, deletes what arrived since the first sweep, every response bundle
/// written before that request's end, and the redaction's work objects; only then does the
/// handshake say `deleted`. Before it is due, the bundles and the late copy are kept.
// Revert: never run the late sweep → the state stays `deletion_owed` → red. Revert: delete every
// response → the bundle written after the cutoff is gone → red. Revert: delete no response → the
// old bundle is kept → red. Revert: run the late sweep at once → the bundle is gone before it is
// due → red.
#[test]
fn an_applied_blob_is_deleted_once_its_late_sweep_has_run() {
    let area = Area::new("redaction-late-sweep");
    let (redactions, recorded) = recorded_redactions(&area, "red", &["leaked\n"]);
    let leak = blob_hash("leaked\n");
    let head = Head::new(MemoryObjectStore::with_redirect("https://s3.example/bucket"), MemoryRefStore::new());
    for (hash, bytes) in &recorded.objects {
        head.objects.put_verified(hash, bytes).expect("seed an object");
    }
    let blob = |content: &str| {
        use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
        let object = LooseObjectBuilder::build_blob(&forklift_core::model::blob::Blob { content: content.as_bytes().to_vec() });
        head.objects.put_verified(&object.hash, &object.content).expect("seed an object");
        object.hash
    };
    blob("leaked\n");
    let other = blob("1");

    head.batch(&[leak.clone(), other.clone()]).expect("a bundle holding the content, before the redaction");
    assert_eq!(head.objects.response_count(), 1);
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions[0]);
    assert_gone(&head.object_get(&leak).expect_err("applied"), &leak);
    assert!(!head.objects.exists(&leak).expect("exists"), "the first sweep");

    head.objects.stage("lift-9", &leak, b"leaked\n".to_vec());
    head.objects.work_put(&format!("{}/walk-x", leak), b"{}").expect("work");
    head.objects.work_put(&format!("{}/pallets", other), b"{}").expect("work");
    assert_eq!(served_state(&head, &leak), Some(ServedRedactionState::DeletionOwed));
    assert_eq!(head.objects.response_count(), 1, "not due: the bundle is kept");
    assert_eq!(head.objects.staged_count(), 1, "not due: the late copy is kept");

    plant_item(&head.refs, &leak, &applied_item(LATE_SWEEP_AFTER + 40, r#","swept":true"#));
    head.objects.age_responses(std::time::Duration::from_secs(1_200));
    head.batch(std::slice::from_ref(&other)).expect("a bundle written after the cutoff");
    assert_eq!(head.objects.response_count(), 2);

    assert_eq!(served_state(&head, &leak), Some(ServedRedactionState::Deleted));
    assert_eq!(head.objects.response_count(), 1, "only the bundle written after the cutoff is kept");
    assert_eq!(head.objects.staged_count(), 0, "the copy staged after the first sweep is deleted");
    assert_eq!(head.objects.work_names(), vec![format!("{}/pallets", other)], "only another redaction's work is kept");
    let item = head.refs.get_redaction(&leak).expect("item").expect("an item");
    assert!(item.contains(r#""deleted":true"#) && !item.contains("lease_until"), "{}", item);
    assert_gone(&head.object_get(&leak).expect_err("still refused"), &leak);
}

/// A warehouse whose `main` keeps a chunked file sharing its first half with a chunked file an
/// earlier head named and a redaction records: the recorded objects, `main`'s head, the redacted
/// recipe, and its chunks split into those only it names and those the kept file names too.
fn chunked_leak(area: &Area) -> (Harvest, String, String, Vec<String>, Vec<String>) {
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as u8
    };
    let content: Vec<u8> = (0..CHUNK_THRESHOLD + 1024 * 1024).map(|_| next()).collect();
    let mut kept = content[..content.len() / 2].to_vec();
    kept.extend((0..CHUNK_THRESHOLD / 2 + 1024 * 1024).map(|_| next()));

    prepare(area, "red");
    std::fs::write(area.path("red/kept.bin"), &kept).expect("write");
    area.forklift("red", &["load", "."]);
    area.forklift("red", &["stack", "-m", "kept"]);
    std::fs::write(area.path("red/big.bin"), &content).expect("write");
    area.forklift("red", &["load", "."]);
    area.forklift("red", &["stack", "-m", "leak"]);
    let leaked = harvest(&area.path("red")).head_of("main").expect("main head");
    area.forklift("red", &["remove", "big.bin"]);
    area.forklift("red", &["stack", "-m", "take the leak out"]);
    let before = harvest(&area.path("red"));
    let main = before.head_of("main").expect("main head");

    let (recipe, own, shared) = {
        let _scope = StorageRootScope::enter(&area.path("red"));
        let file_hash = |head: &str, name: &str| {
            let tree = object_utils::load_tree_shared(&object_utils::load_parcel(head).expect("parcel").tree_hash).expect("tree");
            tree.get_files().find(|(file, _)| file.as_str() == name).map(|(_, file)| file.hash.clone()).expect("the file")
        };
        let recipe = file_hash(&leaked, "big.bin");
        let named: std::collections::HashSet<String> = object_utils::recipe_chunk_hashes(&file_hash(&main, "kept.bin")).expect("chunks").into_iter().collect();
        let (own, shared) = object_utils::recipe_chunk_hashes(&recipe).expect("chunks").into_iter().partition(|chunk| !named.contains(chunk));
        (recipe, own, shared)
    };

    // The recorder's warehouse drops what it redacts; the server had it from before.
    area.forklift("red", &["redact", "record", &format!("{}:big.bin", leaked), "-m", "a leak"]);
    let mut recorded = harvest(&area.path("red"));
    for (hash, bytes) in before.objects {
        recorded.objects.entry(hash).or_insert(bytes);
    }

    (recorded, main, recipe, own, shared)
}

/// Move every chunk ledger entry's apply, and when it is due, `seconds` into the past.
fn age_chunk_ledger(refs: &MemoryRefStore, seconds: u64) {
    let fence = forklift_aws_lambda::format::gate(refs).expect("format gate");
    let stored = refs.get_item(ItemKey::ChunkLedger).expect("ledger").expect("a ledger");
    let mut ledger: serde_json::Value = serde_json::from_str(&stored).expect("json");
    for journal in ledger["recipes"].as_object_mut().expect("recipes").values_mut() {
        for field in ["applied_at", "next_round_after"] {
            journal[field] = (journal[field].as_u64().expect("seconds") - seconds).into();
        }
    }
    let state = ledger.to_string();
    let written = refs.put_items(&[ItemWrite { key: ItemKey::ChunkLedger, state: &state, expected: Some(&stored) }], &fence).expect("write");
    assert_eq!(written, RedactionWrite::Written);
}

/// Assert `err` is a `503` naming `hash`.
fn assert_withheld(err: &forklift_aws_lambda::error::HeadError, hash: &str) {
    assert_eq!(err.status, Status::ServiceUnavailable, "{}", err.message);
    assert!(err.message.contains(hash), "the refusal names the hash: {}", err.message);
}

/// An applied redaction of a chunked file deletes, over two rounds, the chunks only its recipe
/// names, and then the recipe. Until a round's census decides, every chunk of the recipe is
/// withheld with a `503` and none is stored again; once it has, a chunk another file names is
/// served and the rest are deleted, absent. A copy landed after the first round is deleted by the
/// late one, and only after the recipe is deleted does the handshake say `deleted`.
// Revert: never withhold a chunk → the first GET answers 200 → red. Revert: refuse no promotion
// while a round runs → the PUT stores the chunk → red. Revert: delete every chunk of the recipe →
// the shared one is gone → red. Revert: settle a first round straight to the recipe's deletion →
// the recipe is gone before the late round → red. Revert: never delete the recipe → it is kept →
// red. Revert: let the audit verify a redacted recipe's chunks → the history is refused → red.
#[test]
fn an_applied_recipe_and_the_chunks_only_it_names_are_deleted_over_two_rounds() {
    let area = Area::new("redaction-chunk-rounds");
    let (recorded, main, recipe, own, shared) = chunked_leak(&area);
    assert!(!own.is_empty() && !shared.is_empty(), "own {:?}, shared {:?}", own, shared);
    let chunk_bytes = recorded.objects[&own[0]].clone();

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &recorded);
    plant(&head.refs, "main", &main);
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &recorded.head_of("@redactions").expect("@redactions"));

    // Applied, its round announced and waiting: every chunk is withheld, and none is stored.
    assert_eq!(served_state(&head, &recipe), Some(ServedRedactionState::DeletionOwed));
    assert_withheld(&head.object_get(&own[0]).expect_err("undecided"), &own[0]);
    assert_withheld(&head.object_get(&shared[0]).expect_err("undecided"), &shared[0]);
    assert!(head.missing(&[own[0].clone(), shared[0].clone()]).expect("missing").is_empty(), "withheld is not absent");
    assert_withheld(&head.object_put(&Principal::Static, None, &own[0], &chunk_bytes).expect_err("a round runs"), &own[0]);
    assert!(head.objects.exists(&recipe).expect("exists"), "the recipe is kept while its chunks are owed");

    // The round runs: only what the kept file names is kept.
    let head = head.with_chunk_round_wait(0);
    assert_eq!(served_state(&head, &recipe), Some(ServedRedactionState::DeletionOwed));
    for chunk in &own {
        assert!(!head.objects.exists(chunk).expect("exists"), "a chunk only the recipe names is deleted");
    }
    for chunk in &shared {
        assert!(head.objects.exists(chunk).expect("exists"), "a chunk the kept file names is kept");
    }
    assert!(served(&head, &shared[0]), "a chunk the kept file names is served");
    assert_eq!(head.object_get(&own[0]).expect_err("deleted").status, Status::NotFound);
    assert_eq!(head.missing(&[own[0].clone(), shared[0].clone()]).expect("missing"), vec![own[0].clone()]);
    assert!(head.objects.exists(&recipe).expect("exists"), "the recipe waits for the late round");
    // A head whose history names the redacted file, its recipe still there and its own chunks
    // gone, lifts: the audit does not read the chunks of a redacted recipe.
    let request = RefUpdateRequest { old_head: None, new_head: main.clone(), tombstone: None };
    head.ref_update(&Principal::Static, "again", &request).expect("the history lifts");

    // A copy lands after the round, staged and canonical; the late round deletes both, then the
    // recipe.
    head.objects.stage("lift-9", &own[0], chunk_bytes.clone());
    head.objects.put_verified(&own[0], &chunk_bytes).expect("a late copy");
    let staged = head.objects.staged_count();
    plant_item(&head.refs, &recipe, &applied_item(LATE_SWEEP_AFTER + 40, r#","swept":true"#));
    age_chunk_ledger(&head.refs, LATE_SWEEP_AFTER + 40);
    served_state(&head, &recipe);

    assert!(!head.objects.exists(&own[0]).expect("exists"), "the late round deletes the copy landed since");
    assert_eq!(head.objects.staged_count(), staged - 1, "and its staged copy");
    assert!(head.objects.exists(&shared[0]).expect("exists"));
    assert!(!head.objects.exists(&recipe).expect("exists"), "then the recipe is deleted");
    assert_eq!(served_state(&head, &recipe), Some(ServedRedactionState::Deleted));
    let ledger = head.refs.get_item(ItemKey::ChunkLedger).expect("ledger").expect("a ledger");
    assert!(ledger.contains(r#""recipes":{}"#), "{}", ledger);
    assert!(head.objects.exists(&shared[0]).expect("exists"));
    assert!(served(&head, &shared[0]), "a chunk the kept file names is still served");
}

/// A recipe whose round S3 refused a delete of stays owed: once that round is down, its chunks are
/// still withheld, the ones another file names too, until a round deletes them.
// Revert: serve a refused recipe's chunks once a round has settled it → the shared chunk is
// served → red.
#[test]
fn a_recipe_whose_delete_was_refused_keeps_its_chunks_withheld() {
    let area = Area::new("redaction-chunk-refused");
    let (recorded, main, recipe, _own, shared) = chunked_leak(&area);

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &recorded);
    plant(&head.refs, "main", &main);
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &recorded.head_of("@redactions").expect("@redactions"));
    let head = head.with_chunk_round_wait(0);
    assert_eq!(served_state(&head, &recipe), Some(ServedRedactionState::DeletionOwed));
    assert!(served(&head, &shared[0]), "settled, a chunk the kept file names is served");

    // The round as it settles a refused recipe: owed, backed off, its round recorded.
    let fence = forklift_aws_lambda::format::gate(&head.refs).expect("format gate");
    let stored = head.refs.get_item(ItemKey::ChunkLedger).expect("ledger").expect("a ledger");
    let mut ledger: serde_json::Value = serde_json::from_str(&stored).expect("json");
    let journal = &mut ledger["recipes"][&recipe];
    assert!(journal["settled_round"].is_u64(), "{}", stored);
    journal["state"] = "owed".into();
    journal["refusals"] = 1.into();
    journal["next_round_after"] = 99_999_999_999u64.into();
    let state = ledger.to_string();
    let written = head.refs.put_items(&[ItemWrite { key: ItemKey::ChunkLedger, state: &state, expected: Some(&stored) }], &fence).expect("write");
    assert_eq!(written, RedactionWrite::Written);

    assert_withheld(&head.object_get(&shared[0]).expect_err("owed"), &shared[0]);
}

/// While a round deletes, a ref update answers `503` and moves nothing; once the round is down,
/// it lifts.
// Revert: let a ref update past a deleting fence → it commits → red.
#[test]
fn a_ref_update_waits_out_a_deleting_round() {
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    let fence = || forklift_aws_lambda::format::gate(&head.refs).expect("format gate");
    let deleting = r#"{"version":1,"round":1,"phase":"deleting","started":0,"recipes":[],"lease_until":99999999999}"#;
    let written = head.refs.put_items(&[ItemWrite { key: ItemKey::ChunkFence, state: deleting, expected: None }], &fence()).expect("write");
    assert_eq!(written, RedactionWrite::Written);

    let main = gate_parcel(&head, &[], vec![gate_file(&head, "x", "1")]);
    let request = RefUpdateRequest { old_head: None, new_head: main.clone(), tombstone: None };
    let err = head.ref_update(&Principal::Static, "main", &request).expect_err("a round deletes");
    assert_eq!(err.status, Status::ServiceUnavailable, "{}", err.message);
    assert_eq!(user_head(&head, "main"), None);

    let down = r#"{"version":1,"round":1,"phase":"down","started":0,"recipes":[],"lease_until":null}"#;
    let written = head.refs.put_items(&[ItemWrite { key: ItemKey::ChunkFence, state: down, expected: Some(deleting) }], &fence()).expect("write");
    assert_eq!(written, RedactionWrite::Written);
    head.ref_update(&Principal::Static, "main", &request).expect("the round is down");
    assert_eq!(user_head(&head, "main"), Some(main));
}

/// A head commit conditioned on the fence it read is refused once the fence has moved.
// Revert: drop the fence condition from the memory store's commit → it commits → red.
#[test]
fn a_head_commit_is_refused_once_the_chunk_fence_moved() {
    let refs = MemoryRefStore::new();
    let fence = || forklift_aws_lambda::format::gate(&refs).expect("format gate");
    let commit = |chunk_fence: Option<&str>| refs.compare_and_set_head(
        pallet_utils::PalletNamespace::User, "main", ExpectedHead::Unborn, &"1".repeat(64), OfficePrecondition::NotConsumed,
        RedactionsPrecondition::NotConsumed, None, chunk_fence, &fence(), &request_token(),
    ).expect("commit");

    let announced = r#"{"version":1,"round":1,"phase":"announced","started":0,"recipes":[],"lease_until":null}"#;
    let written = refs.put_items(&[ItemWrite { key: ItemKey::ChunkFence, state: announced, expected: None }], &fence()).expect("write");
    assert_eq!(written, RedactionWrite::Written);

    assert_eq!(commit(None), CasOutcome::ChunkFenceMoved, "read as absent, the fence is there now");
    assert_eq!(commit(Some("{}")), CasOutcome::ChunkFenceMoved);
    assert_eq!(commit(Some(announced)), CasOutcome::Committed);
}

/// A store whose every promotion is met by the redaction of what it promotes applying, just after
/// the promotion's write: the race the check after the write closes.
struct AppliesDuringPromotion {
    inner: MemoryObjectStore,
    refs: Arc<MemoryRefStore>,
}

impl ObjectStore for AppliesDuringPromotion {
    fn exists(&self, hash: &str) -> Result<bool, String> {
        self.inner.exists(hash)
    }

    fn objects_staged(&self, session: &str, hashes: &[String]) -> Result<Vec<String>, String> {
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

    fn verify_and_promote(&self, session: &str, hash: &str) -> Result<PromoteOutcome, String> {
        let outcome = self.inner.verify_and_promote(session, hash)?;
        plant_item(&self.refs, hash, &forklift_aws_lambda::head::redaction_record(true, &[], Some(1)));

        Ok(outcome)
    }

    fn delete_canonical(&self, hash: &str) -> Result<(), String> {
        self.inner.delete_canonical(hash)
    }

    fn delete_staged(&self, hash: &str) -> Result<(), String> {
        self.inner.delete_staged(hash)
    }

    fn delete_staged_copy(&self, session: &str, hash: &str) -> Result<(), String> {
        self.inner.delete_staged_copy(session, hash)
    }
}

/// A lift commit whose promotion of content lands just before that content's redaction applies
/// deletes the copy it wrote, and is refused.
// Revert: drop the check after the promotions from `commit_lift` → the commit succeeds → red.
#[test]
fn a_lift_commit_deletes_what_it_promoted_when_its_redaction_applied_meanwhile() {
    let area = Area::new("redaction-promote-race");
    let (redactions, recorded) = recorded_redactions(&area, "red", &["leaked\n"]);
    let refs = Arc::new(MemoryRefStore::new());
    let head = Head::new(AppliesDuringPromotion { inner: MemoryObjectStore::new(), refs: refs.clone() }, SharedMemoryRefs(refs.clone()));
    upload_all(&head, &recorded);
    plant_meta(&refs, REDACTIONS_PALLET_NAME, None, &redactions[0]);
    let object = forklift_core::builder::object::loose_object_builder::LooseObjectBuilder::build_blob(
        &forklift_core::model::blob::Blob { content: b"leaked\n".to_vec() },
    );
    let leak = object.hash;
    head.objects.inner.delete_canonical(&leak).expect("only the staged copy");
    head.objects.inner.stage("lift-1", &leak, object.content);

    let err = head.commit_lift(&Principal::Static, "lift-1", std::slice::from_ref(&leak), &[], false).expect_err("refused");
    assert_gone(&err, &leak);
    assert!(!head.objects.inner.exists(&leak).expect("exists"), "the promoted copy is deleted");
}

/// The same race over content already canonical when the commit promotes it: the promotion finds
/// it present, and the check after it still deletes it.
// Revert: check only what the promotion wrote → the present copy is kept → red.
#[test]
fn a_lift_commit_deletes_what_was_already_present_when_its_redaction_applied_meanwhile() {
    let area = Area::new("redaction-present-race");
    let (redactions, recorded) = recorded_redactions(&area, "red", &["leaked\n"]);
    let refs = Arc::new(MemoryRefStore::new());
    let head = Head::new(AppliesDuringPromotion { inner: MemoryObjectStore::new(), refs: refs.clone() }, SharedMemoryRefs(refs.clone()));
    upload_all(&head, &recorded);
    plant_meta(&refs, REDACTIONS_PALLET_NAME, None, &redactions[0]);
    let object = forklift_core::builder::object::loose_object_builder::LooseObjectBuilder::build_blob(
        &forklift_core::model::blob::Blob { content: b"leaked\n".to_vec() },
    );
    let leak = object.hash;
    assert!(head.objects.inner.exists(&leak).expect("exists"), "already canonical");
    head.objects.inner.stage("lift-1", &leak, object.content);

    let err = head.commit_lift(&Principal::Static, "lift-1", std::slice::from_ref(&leak), &[], false).expect_err("refused");
    assert_gone(&err, &leak);
    assert!(!head.objects.inner.exists(&leak).expect("exists"), "the present copy is deleted");
}

/// A retried lift commit naming content whose redaction applied after an earlier attempt promoted
/// it, past the sweep, is refused, and deletes the canonical copy that attempt left: the staged
/// copy is already gone, so the check before promoting is the only one that runs.
// Revert: drop only the staged copy before refusing → the canonical copy is kept → red.
#[test]
fn a_retried_lift_commit_deletes_the_copy_an_earlier_attempt_promoted() {
    let (_area, head, redactions, leak) = unplanted_redaction("redaction-commit-retry", "leaked\n");
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions);
    plant_item(&head.refs, &leak, &forklift_aws_lambda::head::redaction_record(true, &[], Some(1)));
    assert!(head.objects.exists(&leak).expect("exists"), "the earlier attempt's promotion");

    let err = head.commit_lift(&Principal::Static, "lift-1", std::slice::from_ref(&leak), &[], false).expect_err("refused");
    assert_gone(&err, &leak);
    assert!(!head.objects.exists(&leak).expect("exists"), "the promoted copy is deleted");
}

/// The staging verifier's check after its write deletes a promoted copy whose redaction applied,
/// leaves one whose redaction did not, and keeps an applied recipe whose chunks the ledger still
/// owes, as the judge does, while still answering that it applied. A recipe the ledger does not
/// hold is kept while its late sweep, which journals it, has still to run, and once it has run is
/// deleted as a blob is.
// Revert: delete whatever the item says → the unapplied copy is gone → red. Revert: delete a
// recipe too → it is gone → red. Revert: keep every recipe → the one off the ledger is kept → red.
// Revert: delete a recipe off the ledger before its late sweep → it is gone → red.
#[test]
fn the_check_after_a_promotion_deletes_only_applied_file_content() {
    use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
    use forklift_core::model::recipe::{Recipe, RecipeChunk};

    let objects = MemoryObjectStore::new();
    let refs = MemoryRefStore::new();
    let applied = forklift_aws_lambda::head::redaction_record(true, &[], Some(1));
    let pending = forklift_aws_lambda::head::redaction_record(false, &[], None);

    let blob = |content: &str| {
        let object = LooseObjectBuilder::build_blob(&forklift_core::model::blob::Blob { content: content.as_bytes().to_vec() });
        objects.put_verified(&object.hash, &object.content).expect("put");
        object.hash
    };
    let (gone, kept) = (blob("applied\n"), blob("pending\n"));
    let recipe = LooseObjectBuilder::build_recipe(&Recipe {
        content_hash: "c".repeat(64),
        total_size: 1,
        chunks: vec![RecipeChunk { hash: "d".repeat(64), size: 1 }],
    });
    objects.put_verified(&recipe.hash, &recipe.content).expect("put");
    plant_item(&refs, &gone, &applied);
    plant_item(&refs, &kept, &pending);
    plant_item(&refs, &recipe.hash, &applied);
    let (ledger, _) = forklift_aws_lambda::head::chunk_fixture_items([&recipe.hash, &"8".repeat(64), &"9".repeat(64)]).expect("items");
    let fence = forklift_aws_lambda::format::gate(&refs).expect("format gate");
    let written = refs.put_items(&[ItemWrite { key: ItemKey::ChunkLedger, state: &ledger, expected: None }], &fence).expect("write");
    assert_eq!(written, RedactionWrite::Written);

    use forklift_aws_lambda::head::AppliedCopies;
    let delete = |hash: &str| forklift_aws_lambda::head::delete_if_redaction_applied(&objects, &refs, "lift-1", hash).expect("checked");
    assert_eq!(delete(&gone), AppliedCopies::Deleted);
    assert_eq!(delete(&kept), AppliedCopies::Unapplied);
    assert_eq!(delete(&recipe.hash), AppliedCopies::RecipeKept, "an applied recipe is applied, though it is kept");
    assert!(AppliedCopies::RecipeKept.applied());
    assert_eq!(delete(&"e".repeat(64)), AppliedCopies::Unapplied, "no item: nothing to delete");

    assert!(!objects.exists(&gone).expect("exists"));
    assert!(objects.exists(&kept).expect("exists"));
    assert!(objects.exists(&recipe.hash).expect("exists"));

    let done = LooseObjectBuilder::build_recipe(&Recipe {
        content_hash: "f".repeat(64),
        total_size: 1,
        chunks: vec![RecipeChunk { hash: "d".repeat(64), size: 1 }],
    });
    objects.put_verified(&done.hash, &done.content).expect("put");
    plant_item(&refs, &done.hash, &applied);
    assert_eq!(delete(&done.hash), AppliedCopies::RecipeKept, "off the ledger, its late sweep journals it");
    assert!(objects.exists(&done.hash).expect("exists"));
    let swept = format!(r#"{},"deleted":true}}"#, &applied[..applied.len() - 1]);
    plant_item(&refs, &done.hash, &swept);
    assert_eq!(delete(&done.hash), AppliedCopies::Deleted, "off the ledger after its late sweep, its chunks are done");
    assert!(!objects.exists(&done.hash).expect("exists"));
}

/// A new pallet whose history names applied content no longer stored, below a head that does not
/// name it, lands: the push gate tolerates the absence of applied file content, as
/// `forklift-server`'s does. Content that is absent and not applied is still refused.
// Revert: pass the gate no applied set → the push is refused as missing the content → red.
#[test]
fn a_push_whose_history_names_deleted_applied_content_lands() {
    let (_area, head, redactions, leak) = unplanted_redaction("redaction-gate", "leaked\n");
    let named = gate_parcel(&head, &[], vec![gate_file(&head, "x", "leaked\n")]);
    let after = gate_parcel(&head, &[&named], vec![gate_file(&head, "y", "1")]);
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions);
    assert!(!served(&head, &leak), "nothing holds it: it applies");
    assert!(!head.objects.exists(&leak).expect("exists"), "and is deleted");

    let request = RefUpdateRequest { old_head: None, new_head: after.clone(), tombstone: None };
    head.ref_update(&Principal::Static, "copy", &request).expect("the applied content's absence is tolerated");
    assert_eq!(user_head(&head, "copy"), Some(after));

    let lost = gate_parcel(&head, &[], vec![gate_file(&head, "z", "lost\n")]);
    head.objects.delete_canonical(&blob_hash("lost\n")).expect("delete");
    let request = RefUpdateRequest { old_head: None, new_head: lost, tombstone: None };
    let err = head.ref_update(&Principal::Static, "lost", &request).expect_err("unapplied content is required");
    assert_eq!(err.status, Status::Unprocessable, "{}", err.message);
}

// ---------------------------------------------------------------------------------------
// Withholding: the host's takedown, refused `451` on every route that serves or takes it.
// ---------------------------------------------------------------------------------------

const STATEMENT: &str = "Withheld under court order 12/2026.";

/// The stored bytes of a blob holding `content`.
fn blob_object(content: &str) -> Vec<u8> {
    use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
    use forklift_core::model::blob::Blob;

    LooseObjectBuilder::build_blob(&Blob { content: content.as_bytes().to_vec() }).content
}

/// Withhold `hash` in the warehouse behind `head`, as its host, with [`STATEMENT`].
fn withhold<O: ObjectStore>(head: &Head<O, MemoryRefStore>, hash: &str) {
    forklift_aws_lambda::withheld::add(&head.refs, hash, STATEMENT).expect("withhold");
}

/// Assert `err` is the host's `451`, naming `hash` and carrying its statement.
fn assert_host_withheld(err: &forklift_aws_lambda::error::HeadError, hash: &str) {
    assert_eq!(err.status, Status::UnavailableForLegalReasons, "{}", err.message);
    assert_eq!(refusal_code(&err.message), Some(RefusalCode::ContentWithheld), "{}", err.message);
    assert!(err.message.contains(hash), "the refusal names the hash: {}", err.message);
    assert!(err.message.contains(STATEMENT), "the refusal carries the statement: {}", err.message);
}

/// The withheld list the handshake reports, as (hash, statement, state).
fn served_withheld<O: ObjectStore, R: RefStore>(head: &Head<O, R>) -> Vec<(String, String, ServedWithholdingState)> {
    let info = head.handshake().expect("handshake");
    info.withheld.expect("the head reports what it withholds").into_iter()
        .map(|served| (served.hash, served.statement, served.state))
        .collect()
}

/// A withheld file's content is refused `451` with the statement on every route that serves or
/// takes it by hash, from the first request after the withhold, while other content is served:
/// upload-targets answers it apart, missing never lists it, batch leaves it out, and the
/// handshake lists it, its deletion owed.
// Revert: drop the refusal from `object_get` → it is served → red. Revert: from `object_put` → it
// is stored → red. Revert: take no withheld list in `upload_targets` → it gets a target → red.
// Revert: list it in `missing` → red. Revert: bundle it in `batch` → red. Revert: report nothing in
// the handshake → red.
#[test]
fn withheld_content_is_refused_451_on_every_route() {
    let area = Area::new("withheld-routes");
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    let main = gate_parcel(&head, &[], vec![gate_file(&head, "x", "taken down\n"), gate_file(&head, "y", "1")]);
    plant(&head.refs, "main", &main);
    let (taken, other) = (blob_hash("taken down\n"), blob_hash("1"));
    assert_eq!(served_withheld(&head), vec![], "nothing withheld yet");
    withhold(&head, &taken);

    assert_host_withheld(&head.object_get(&taken).expect_err("withheld"), &taken);
    assert!(served(&head, &other), "other content is served");
    let err = head.object_put(&Principal::Static, None, &taken, b"taken down\n").expect_err("not taken");
    assert_host_withheld(&err, &taken);

    let fresh = blob_hash("fresh\n");
    let answer = head.upload_targets(&Principal::Static, "lift-1", &[other.clone(), taken.clone(), fresh.clone()]).expect("negotiated");
    assert_eq!(answer.withheld, vec![taken.clone()]);
    assert_eq!(answer.present, vec![other.clone()]);
    assert_eq!(answer.direct, vec![fresh.clone()]);

    head.objects.delete_canonical(&taken).expect("delete");
    assert_eq!(head.missing(&[taken.clone(), fresh.clone()]).expect("missing"), vec![fresh], "withheld is never asked for");
    head.objects.put_verified(&taken, &blob_object("taken down\n")).expect("stored again");

    let requested = vec![taken.clone(), other.clone()];
    let answer = head.batch(&requested).expect("batch");
    assert_eq!(bundled(&area, "fetch", answer, &requested), vec![other]);

    assert_eq!(served_withheld(&head), vec![(taken, STATEMENT.to_string(), ServedWithholdingState::DeletionOwed)]);
}

/// A withheld hash the store holds as a tree never applies: the first request settles it as such,
/// the tree stays served, and the handshake does not list it.
// Revert: settle every item as a blob → the tree is refused → red.
#[test]
fn a_withheld_tree_never_applies_and_stays_served() {
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    let main = gate_parcel(&head, &[], vec![gate_file(&head, "x", "1")]);
    plant(&head.refs, "main", &main);
    let tree = {
        let bytes = head.objects.get(&main).expect("read").expect("the parcel");
        object_utils::parse_parcel_bytes(&main, &bytes).expect("parcel").tree_hash
    };
    withhold(&head, &tree);

    assert!(served(&head, &tree), "a tree is no file's content");
    let listed = forklift_aws_lambda::withheld::list(&head.refs).expect("list");
    assert!(listed[0].item.settled && listed[0].item.never_applies, "{:?}", listed[0].item);
    assert_eq!(served_withheld(&head), vec![]);
}

/// A withheld hash the store lacks applies whatever it names, as a redaction's does: the store
/// cannot tell what it is, so a tree of that hash a lift uploads later is refused, not taken.
// Revert: settle an absent hash as never applying → the tree is taken → red.
#[test]
fn a_withheld_hash_the_store_lacks_applies_to_a_tree_uploaded_later() {
    let source = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    let main = gate_parcel(&source, &[], vec![gate_file(&source, "x", "1")]);
    let bytes = source.objects.get(&main).expect("read").expect("the parcel");
    let tree = object_utils::parse_parcel_bytes(&main, &bytes).expect("parcel").tree_hash;
    let tree_bytes = source.objects.get(&tree).expect("read").expect("the tree");

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    let other = gate_parcel(&head, &[], vec![gate_file(&head, "y", "2")]);
    plant(&head.refs, "main", &other);
    withhold(&head, &tree);

    let err = head.object_put(&Principal::Static, None, &tree, &tree_bytes).expect_err("not taken");
    assert_host_withheld(&err, &tree);
    let listed = forklift_aws_lambda::withheld::list(&head.refs).expect("list");
    assert!(listed[0].item.settled && !listed[0].item.never_applies, "{:?}", listed[0].item);
}

/// A tree a racing upload lands after its withheld hash was settled absent is not deleted as a
/// blob by the late step: it is no file's content, so the item is settled again as never applying,
/// and the tree stays served, as does a copy a lift staged since.
// Revert: drop the `NoFile` outcome of the late step's type read → the tree is deleted → red.
// Revert: delete the staged copies before the type read → the staged tree is gone → red.
#[test]
fn a_withheld_tree_landing_after_it_was_settled_absent_never_applies_and_stays() {
    let source = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    let main = gate_parcel(&source, &[], vec![gate_file(&source, "x", "1")]);
    let bytes = source.objects.get(&main).expect("read").expect("the parcel");
    let tree = object_utils::parse_parcel_bytes(&main, &bytes).expect("parcel").tree_hash;
    let tree_bytes = source.objects.get(&tree).expect("read").expect("the tree");

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    let other = gate_parcel(&head, &[], vec![gate_file(&head, "y", "2")]);
    plant(&head.refs, "main", &other);
    withhold(&head, &tree);
    served_withheld(&head);
    let item = withheld_item(&head, &tree);
    assert!(item.settled && item.absent && !item.never_applies, "settled while absent: {:?}", item);

    head.objects.put_verified(&tree, &tree_bytes).expect("the racing upload");
    head.objects.stage("lift-1", &tree, tree_bytes.clone());
    age_withheld(&head.refs, &tree, LATE_SWEEP_AFTER + 40);
    served_withheld(&head);

    let item = withheld_item(&head, &tree);
    assert!(item.never_applies && !item.absent && !item.deleted && item.lease_until.is_none(), "{:?}", item);
    assert!(head.objects.exists(&tree).expect("exists"), "not deleted as a blob");
    assert_eq!(head.objects.staged_count(), 1, "the staged copy is left to its lift");
    assert!(served(&head, &tree), "a tree is no file's content");
}

/// An object whose header this build cannot read is never guessed at: its withheld item stays
/// unsettled and applies, so its own hash is refused `451`, while other content is served.
// Revert: settle an unreadable header as never applying → it is served → red. Revert: fail the
// listing on it → other content is refused → red.
#[test]
fn a_withheld_object_whose_header_cannot_be_read_is_refused_alone() {
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    let main = gate_parcel(&head, &[], vec![gate_file(&head, "x", "1")]);
    plant(&head.refs, "main", &main);
    let odd = object_utils::hash_object_bytes(b"no header at all");
    head.objects.put_verified(&odd, b"no header at all").expect("seed");
    withhold(&head, &odd);

    assert!(served(&head, &main), "other content is served");
    assert_host_withheld(&head.object_get(&odd).expect_err("not guessed at"), &odd);
    let listed = forklift_aws_lambda::withheld::list(&head.refs).expect("list");
    assert!(!listed[0].item.settled, "{:?}", listed[0].item);
}

/// An applied redaction of withheld content wins: it is for good, and is answered `410`.
// Revert: refuse a withhold before a redaction → `451` → red.
#[test]
fn an_applied_redaction_wins_over_a_withhold() {
    let (_area, head, redactions, leak) = unplanted_redaction("withheld-redacted", "leaked\n");
    plant_meta(&head.refs, REDACTIONS_PALLET_NAME, None, &redactions);
    withhold(&head, &leak);

    assert_gone(&head.object_get(&leak).expect_err("refused"), &leak);
    assert_gone(&head.object_put(&Principal::Static, None, &leak, b"leaked\n").expect_err("refused"), &leak);
    head.objects.stage("lift-1", &leak, b"leaked\n".to_vec());
    assert_gone(&head.commit_lift(&Principal::Static, "lift-1", &[], &[leak.clone()], false).expect_err("refused"), &leak);
}

/// A request that cannot settle a withheld item within its time answers `503` with `Retry-After`,
/// never serving the content as if nothing were withheld; the next request settles it.
// Revert: serve an unsettled item's content → the late GET answers 200 → red.
#[test]
fn an_item_not_settled_in_time_answers_503_and_the_next_request_settles_it() {
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    let main = gate_parcel(&head, &[], vec![gate_file(&head, "x", "taken down\n")]);
    plant(&head.refs, "main", &main);
    let taken = blob_hash("taken down\n");
    withhold(&head, &taken);

    let late = head.with_deadline(std::time::SystemTime::now());
    let err = late.object_get(&taken).expect_err("unsettled");
    assert_eq!(err.status, Status::ServiceUnavailable, "{}", err.message);
    assert!(err.retry_after_seconds.is_some(), "{}", err.message);
    assert!(!forklift_aws_lambda::withheld::list(&late.refs).expect("list")[0].item.settled);

    let head = late.with_deadline(std::time::SystemTime::now() + std::time::Duration::from_secs(60));
    assert_host_withheld(&head.object_get(&taken).expect_err("withheld"), &taken);
    assert!(forklift_aws_lambda::withheld::list(&head.refs).expect("list")[0].item.settled);
}

/// A lift commit naming withheld content is refused `451`, its staged copy deleted and never
/// promoted, and the staging verifier's check deletes a staged copy of it the same way, keeping the
/// canonical object, which is the withhold's own deletion to take.
// Revert: pass over withheld hashes in the commit's refusal → the commit lands → red. Revert: read
// only the redaction item in `delete_if_applied` → the verifier would promote it → red.
#[test]
fn a_lift_commit_and_the_verifier_never_promote_withheld_content() {
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    let main = gate_parcel(&head, &[], vec![gate_file(&head, "x", "1")]);
    plant(&head.refs, "main", &main);
    let taken = blob_hash("taken down\n");
    withhold(&head, &taken);

    for (control_plane, blobs) in [(vec![taken.clone()], vec![]), (vec![], vec![taken.clone()])] {
        head.objects.stage("lift-1", &taken, b"taken down\n".to_vec());

        let err = head.commit_lift(&Principal::Static, "lift-1", &control_plane, &blobs, false).expect_err("refused");
        assert_host_withheld(&err, &taken);
        assert_eq!(head.objects.staged_count(), 0, "the staged copy is dropped");
        assert!(!head.objects.exists(&taken).expect("exists"), "never promoted");
    }

    head.objects.put_verified(&taken, &blob_object("taken down\n")).expect("a canonical copy");
    head.objects.stage("lift-2", &taken, b"taken down\n".to_vec());
    let found = forklift_aws_lambda::head::delete_if_redaction_applied(&head.objects, &head.refs, "lift-2", &taken).expect("check");
    assert_eq!(found, forklift_aws_lambda::head::AppliedCopies::Withheld);
    assert!(found.applied(), "the verifier does not promote it");
    assert_eq!(head.objects.staged_count(), 0, "the staged copy is dropped");
    assert!(head.objects.exists(&taken).expect("exists"), "the canonical copy is the withhold's to delete");
}

/// A ref update tolerates withheld content absent as a file's content, and refuses a head whose
/// withheld hash is absent as a tree, saying the host withholds it; a new head the host withholds
/// is refused saying so.
// Revert: tolerate nothing withheld in the ref update → the first push is refused → red. Revert:
// drop the note on an absent new head → red.
#[test]
fn a_ref_update_tolerates_withheld_file_content_only() {
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    let first = gate_parcel(&head, &[], vec![gate_file(&head, "y", "1")]);
    plant(&head.refs, "first", &first);
    let named = gate_parcel(&head, &[], vec![gate_file(&head, "x", "taken down\n"), gate_file(&head, "y", "1")]);
    let taken = blob_hash("taken down\n");
    head.objects.delete_canonical(&taken).expect("delete");

    let request = RefUpdateRequest { old_head: None, new_head: named.clone(), tombstone: None };
    let err = head.ref_update(&Principal::Static, "main", &request).expect_err("absent content is required");
    assert_eq!(err.status, Status::Unprocessable, "{}", err.message);
    assert!(!err.message.contains("host withholds"), "{}", err.message);

    withhold(&head, &taken);
    head.ref_update(&Principal::Static, "main", &request).expect("withheld content's absence is tolerated");
    assert_eq!(user_head(&head, "main"), Some(named));

    let dir = gate_dir(&head, "d", vec![gate_file(&head, "z", "2")]);
    let nested = gate_parcel(&head, &[], vec![dir.clone()]);
    head.objects.delete_canonical(&dir.hash).expect("delete");
    withhold(&head, &dir.hash);
    let request = RefUpdateRequest { old_head: None, new_head: nested, tombstone: None };
    let err = head.ref_update(&Principal::Static, "nested", &request).expect_err("a tree is never tolerated");
    assert_eq!(err.status, Status::Unprocessable, "{}", err.message);
    assert!(err.message.contains("host withholds"), "{}", err.message);

    let absent = gate_parcel(&head, &[], vec![gate_file(&head, "w", "3")]);
    head.objects.delete_canonical(&absent).expect("delete");
    withhold(&head, &absent);
    let request = RefUpdateRequest { old_head: None, new_head: absent, tombstone: None };
    let err = head.ref_update(&Principal::Static, "absent", &request).expect_err("not uploaded");
    assert!(err.message.contains("host withholds it"), "{}", err.message);
}

/// A withheld recipe gates its chunks from the first request after the withhold, as an applied
/// redaction's does, and a round deletes the chunks only it names: a recipe the host withholds is
/// no census root's to keep. The history naming it still lifts.
// Revert: journal no recipe when settling → the chunk is served → red. Revert: leave withheld
// recipes out of the census basis → the chunks only it names are kept → red. Revert: mark only
// the redaction item of a deleted recipe → the late round errors → red. Revert: record no digest
// when settling → red. Revert: report none in the handshake → red.
#[test]
fn a_withheld_recipe_gates_its_chunks_and_a_round_deletes_the_ones_only_it_names() {
    let area = Area::new("withheld-chunks");
    let (recorded, main, recipe, own, shared) = chunked_leak(&area);
    let digest = forklift_core::util::withholding::recipe_digest(&recipe, &recorded.objects[&recipe]).expect("a recipe");
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &recorded);
    plant(&head.refs, "main", &main);
    assert!(served(&head, &own[0]), "nothing withheld yet");
    withhold(&head, &recipe);

    assert_withheld(&head.object_get(&own[0]).expect_err("undecided"), &own[0]);
    assert_host_withheld(&head.object_get(&recipe).expect_err("withheld"), &recipe);
    assert_eq!(served_withheld(&head), vec![(recipe.clone(), STATEMENT.to_string(), ServedWithholdingState::DeletionOwed)]);
    assert_eq!(withheld_item(&head, &recipe).chunks.as_deref(), Some(digest.as_str()), "settling records the digest");
    assert_eq!(served_digests(&head), vec![(recipe.clone(), Some(digest.clone()))], "the handshake reports it");

    let head = head.with_chunk_round_wait(0);
    served_withheld(&head);
    for chunk in &own {
        assert!(!head.objects.exists(chunk).expect("exists"), "a chunk only the recipe names is deleted");
    }
    for chunk in &shared {
        assert!(head.objects.exists(chunk).expect("exists"), "a chunk the kept file names is kept");
    }
    assert!(served(&head, &shared[0]));
    assert!(head.objects.exists(&recipe).expect("exists"), "the recipe waits for the late round");

    let request = RefUpdateRequest { old_head: None, new_head: main, tombstone: None };
    head.ref_update(&Principal::Static, "again", &request).expect("the history lifts");

    // The late round deletes the recipe, and records it on the withheld item.
    age_chunk_ledger(&head.refs, LATE_SWEEP_AFTER + 40);
    served_withheld(&head);
    assert!(!head.objects.exists(&recipe).expect("exists"), "then the recipe is deleted");
    let listed = forklift_aws_lambda::withheld::list(&head.refs).expect("list");
    assert!(listed[0].item.recipe && listed[0].item.recipe_deleted, "{:?}", listed[0].item);
    assert_eq!(listed[0].item.chunks.as_deref(), Some(digest.as_str()), "the digest outlives the recipe");
    assert_eq!(served_digests(&head), vec![(recipe.clone(), Some(digest))]);
}

/// The handshake's withheld list, as (hash, chunk-list digest).
fn served_digests<O: ObjectStore, R: RefStore>(head: &Head<O, R>) -> Vec<(String, Option<String>)> {
    let info = head.handshake().expect("handshake");
    info.withheld.expect("the head reports what it withholds").into_iter()
        .map(|served| (served.hash, served.chunks))
        .collect()
}

/// A recipe withheld while the store lacked it, which a racing upload lands after it is settled,
/// is never deleted as a blob: the late step finds the copy a recipe and settles it again as one,
/// with its digest and its chunk ledger entry, so its chunks are gated and the digest is recorded
/// before the chunk ledger deletes it; also when meta history names it, which would keep a blob.
// Revert: drop the type read before the late step's `delete_canonical` → the recipe goes as a
// blob with no digest and its chunks stay → red. Revert: drop the read before the walk → the
// recipe meta history names is kept as a blob → red. Revert: journal nothing on the re-settle →
// the chunk is served → red. Revert: record no digest on the re-settle → red. Revert: record no
// `absent` at settling → nothing is read again → red.
#[test]
fn a_withheld_recipe_landing_after_it_was_settled_absent_is_settled_again_with_its_digest() {
    for named in [false, true] {
        landing_after_settled_absent(named);
    }
}

fn landing_after_settled_absent(named: bool) {
    use forklift_core::enums::dir_entry_type::DirEntryType;
    use forklift_core::model::tree_item::TreeItem;

    let area = Area::new(if named { "withheld-race-named" } else { "withheld-race" });
    let (mut recorded, main, recipe, own, shared) = chunked_leak(&area);
    let bytes = recorded.objects.remove(&recipe).expect("the recipe");
    let digest = forklift_core::util::withholding::recipe_digest(&recipe, &bytes).expect("a recipe");
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &recorded);
    plant(&head.refs, "main", &main);
    withhold(&head, &recipe);

    served_withheld(&head);
    let item = withheld_item(&head, &recipe);
    assert!(item.settled && item.absent && item.swept && !item.recipe && item.chunks.is_none(), "settled while absent: {:?}", item);
    assert!(served(&head, &own[0]), "no recipe is journaled, so its chunks are served");

    // The upload that raced the withhold lands.
    head.objects.put_verified(&recipe, &bytes).expect("the racing upload");
    if named {
        let meta = gate_parcel(&head, &[], vec![TreeItem::new("cell".to_string(), recipe.clone(), DirEntryType::Normal)]);
        plant_meta(&head.refs, "tags", None, &meta);
    }
    age_withheld(&head.refs, &recipe, LATE_SWEEP_AFTER + 40);
    served_withheld(&head);

    let item = withheld_item(&head, &recipe);
    assert!(item.recipe && !item.absent && !item.kept && !item.deleted && item.lease_until.is_none(), "settled again as a recipe: {:?}", item);
    assert_eq!(item.chunks.as_deref(), Some(digest.as_str()), "with its digest");
    assert!(head.objects.exists(&recipe).expect("exists"), "not deleted as a blob");
    assert_withheld(&head.object_get(&own[0]).expect_err("gated"), &own[0]);

    // The recipe's path: its own chunks, then, late, the recipe; the digest stays.
    let head = head.with_chunk_round_wait(0);
    served_withheld(&head);
    for chunk in &own {
        assert!(!head.objects.exists(chunk).expect("exists"), "a chunk only the recipe names is deleted");
    }
    assert!(head.objects.exists(&shared[0]).expect("exists"));
    age_chunk_ledger(&head.refs, LATE_SWEEP_AFTER + 40);
    served_withheld(&head);
    assert!(!head.objects.exists(&recipe).expect("exists"), "then the recipe is deleted");
    let item = withheld_item(&head, &recipe);
    assert!(item.recipe_deleted && item.deleted, "{:?}", item);
    assert_eq!(item.chunks.as_deref(), Some(digest.as_str()));
}

/// The chunk ledger records a withheld recipe's digest from the canonical copy before it deletes
/// the recipe, when its item has none (settled by a build that recorded none); with no copy left,
/// it records the deletion and the digest stays not known.
// Revert: drop `fill_withheld_digest` from `finish_recipes` → the recipe goes with no digest → red.
// Revert: refuse to finish a recipe whose copy is gone → it is never marked deleted → red.
#[test]
fn the_chunk_ledger_records_a_withheld_recipes_digest_before_deleting_it() {
    for copy_left in [true, false] {
        let area = Area::new(if copy_left { "withheld-fill" } else { "withheld-fill-gone" });
        let (recorded, main, recipe, _, _) = chunked_leak(&area);
        let digest = forklift_core::util::withholding::recipe_digest(&recipe, &recorded.objects[&recipe]).expect("a recipe");
        let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new()).with_chunk_round_wait(0);
        upload_all(&head, &recorded);
        plant(&head.refs, "main", &main);
        withhold(&head, &recipe);
        served_withheld(&head);

        rewrite_withheld(&head.refs, &recipe, |item| {
            item.as_object_mut().expect("an object").remove("chunks").expect("settling recorded one");
        });
        if !copy_left {
            head.objects.delete_canonical(&recipe).expect("an earlier run deleted it");
        }
        age_chunk_ledger(&head.refs, LATE_SWEEP_AFTER + 40);
        served_withheld(&head);

        assert!(!head.objects.exists(&recipe).expect("exists"));
        let item = withheld_item(&head, &recipe);
        assert!(item.recipe_deleted, "copy left {}: {:?}", copy_left, item);
        let expected = copy_left.then_some(digest.as_str());
        assert_eq!(item.chunks.as_deref(), expected, "copy left {}", copy_left);
    }
}

/// A recipe withheld before the store held it has no digest, so a ref update newly naming it at a
/// chunked entry is refused as content the host cannot judge. The handshake names the staged
/// intake; its target is a presigned upload outside staging, so neither the staging verifier's step
/// nor the deletion step touches the copy, and the intake request records the digest from it,
/// deletes it, and promotes nothing. The ref update then lands, the target names no URL, and a
/// second intake request is answered as taken.
// Revert: keep `AssumedKnown` on the ref update → the first update lands → red. Revert: record no
// digest in `withheld_intake` → the second update is refused → red. Revert: keep the copy after the
// read → red. Revert: hand out a staging target → the verifier's step deletes it, and the intake
// finds no copy → red.
#[test]
fn the_digest_intake_records_a_withheld_recipes_digest_from_an_uploaded_copy_and_keeps_nothing() {
    use forklift_core::enums::dir_entry_type::DirEntryType;
    use forklift_core::model::remote::WithheldIntake;
    use forklift_core::model::tree_item::TreeItem;
    use forklift_aws_lambda::head::delete_if_redaction_applied;

    let area = Area::new("withheld-intake");
    let (mut recorded, main, recipe, _, _) = chunked_leak(&area);
    let bytes = recorded.objects.remove(&recipe).expect("the recipe");
    let digest = forklift_core::util::withholding::recipe_digest(&recipe, &bytes).expect("a recipe");
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &recorded);
    plant(&head.refs, "main", &main);
    withhold(&head, &recipe);
    served_withheld(&head);
    assert!(withheld_item(&head, &recipe).absent, "settled while the store lacks it");

    assert_eq!(head.handshake().expect("handshake").withheld_intake, WithheldIntake::Staged);

    let new = gate_parcel(&head, &[&main], vec![TreeItem::new("film.bin".to_string(), recipe.clone(), DirEntryType::NormalChunked)]);
    let update = RefUpdateRequest { old_head: Some(main.clone()), new_head: new.clone(), tombstone: None };
    let err = head.ref_update(&Principal::Static, "main", &update).expect_err("no digest to judge the file by");
    assert_eq!(err.status, Status::Unprocessable, "{}", err.message);
    assert_eq!(refusal_code(&err.message), Some(RefusalCode::ContentWithheld), "{}", err.message);
    assert!(err.message.contains(&recipe), "{}", err.message);

    // An intake request with no copy sent is refused, and records nothing.
    let err = head.withheld_intake(&Principal::Static, &recipe).expect_err("no copy");
    assert_eq!(err.status, Status::Unprocessable, "{}", err.message);

    let url = head.withheld_intake_target(&Principal::Static, &recipe).expect("target").url.expect("a digest is owed");
    assert!(url.contains(&format!("/intake/{}", recipe)) && !url.contains("/staging/"), "{}", url);

    // A copy that is not the recipe is refused, and is deleted all the same.
    head.objects.land_intake(&recipe, blob_object("not the recipe"));
    let err = head.withheld_intake(&Principal::Static, &recipe).expect_err("not the recipe");
    assert_eq!(err.status, Status::Unprocessable, "{}", err.message);
    assert_eq!(head.objects.intake_count(), 0, "a refused copy is not kept");
    assert!(withheld_item(&head, &recipe).chunks.is_none());

    // The client's upload, then the staging verifier's step and the deletion step, as they run on
    // any event before the intake request.
    head.objects.land_intake(&recipe, bytes.clone());
    assert!(delete_if_redaction_applied(&head.objects, &head.refs, "lift-1", &recipe).expect("verifier").applied(),
        "the verifier deletes every staged copy of withheld content");
    head.objects.delete_staged(&recipe).expect("the deletion step's sweep");
    served_withheld(&head);
    assert_eq!(head.objects.intake_count(), 1, "nothing outside staging deletes the copy");

    head.withheld_intake(&Principal::Static, &recipe).expect("intake");
    assert_eq!(withheld_item(&head, &recipe).chunks.as_deref(), Some(digest.as_str()), "the digest is recorded");
    assert_eq!(served_digests(&head), vec![(recipe.clone(), Some(digest.clone()))], "and reported");
    assert_eq!(head.objects.intake_count(), 0, "the copy is deleted");
    assert!(!head.objects.exists(&recipe).expect("exists"), "nothing is promoted");
    assert_eq!(head.objects.staged_count(), 0);

    assert_eq!(head.withheld_intake_target(&Principal::Static, &recipe).expect("target").url, None, "no digest is owed");
    head.withheld_intake(&Principal::Static, &recipe).expect("a recorded digest is answered as taken");

    head.ref_update(&Principal::Static, "main", &update).expect("the host can judge the file now");
}

/// A first anchor's adopted history is held strictly over withheld content too, and the refusal
/// says the host withholds it rather than sending the client to upload it.
// Revert: drop the withheld note from `require_adopted_history`'s refusal → red.
#[test]
fn a_first_anchor_adopting_history_naming_withheld_content_says_so() {
    let area = Area::new("first-anchor-adopts-withheld");
    prepare(&area, "wh");
    area.forklift("wh", &["office", "enroll"]);
    area.write_file("wh/app.txt", "v1\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "-m", "signed"]);

    let office = harvest(&area.path("wh"));
    let office_head = office.head_of(&format!("@{}", OFFICE_PALLET_NAME)).expect("office head");
    let record = {
        let _scope = StorageRootScope::enter(&area.path("wh"));
        let mut tree = object_utils::load_tree(&object_utils::load_parcel(&office_head).expect("parcel").tree_hash)
            .expect("the office tree");
        loop {
            if let Some((_, file)) = tree.get_files().next() {
                break file.hash.clone();
            }
            let (_, subtree) = tree.get_subtrees().next().expect("the office tree names a record");
            tree = object_utils::load_tree(&subtree.hash).expect("a subtree");
        }
    };
    let anchor = TrustAnchorDto {
        genesis: "e".repeat(64),
        adopts: Some(office_head.clone()),
        ..office.trust.clone().expect("trust")
    };

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    for (hash, bytes) in office.objects.iter().filter(|(hash, _)| **hash != record) {
        head.object_put(&Principal::Static, None, hash, bytes).expect("upload object");
    }
    let err = head.put_trust(&Principal::Static, &anchor).expect_err("the pin's history is incomplete");
    assert!(!err.message.contains("host withholds"), "{}", err.message);

    // A host withholds only in a warehouse that holds something.
    plant(&head.refs, "main", &office_head);
    withhold(&head, &record);
    let err = head.put_trust(&Principal::Static, &anchor).expect_err("the pin names withheld content");
    assert_eq!(err.status, Status::Unprocessable, "{}", err.message);
    assert!(err.message.contains("host withholds"), "{}", err.message);
}

/// The withheld item of `hash`, as listed.
fn withheld_item<O: ObjectStore>(head: &Head<O, MemoryRefStore>, hash: &str) -> forklift_aws_lambda::withheld::WithheldItem {
    forklift_aws_lambda::withheld::list(&head.refs).expect("list").into_iter()
        .find(|listed| listed.hash == hash)
        .expect("an item")
        .item
}

/// Rewrite the withheld item of `hash` through `change`, over whatever it holds.
fn rewrite_withheld(refs: &MemoryRefStore, hash: &str, change: impl FnOnce(&mut serde_json::Value)) {
    let fence = forklift_aws_lambda::format::gate(refs).expect("format gate");
    let stored = refs.get_item(ItemKey::Withheld(hash)).expect("item").expect("an item");
    let mut item: serde_json::Value = serde_json::from_str(&stored).expect("json");
    change(&mut item);
    let state = item.to_string();
    let written = refs.put_items(&[ItemWrite { key: ItemKey::Withheld(hash), state: &state, expected: Some(&stored) }], &fence).expect("write");
    assert_eq!(written, RedactionWrite::Written);
}

/// Move the withhold of `hash` `seconds` into the past.
fn age_withheld(refs: &MemoryRefStore, hash: &str, seconds: u64) {
    rewrite_withheld(refs, hash, |item| item["withheld_at"] = (item["withheld_at"].as_u64().expect("seconds") - seconds).into());
}

/// A withheld blob loses its staged copies at once, and once its late step is due, its canonical
/// copy, though a user head still names it, with every response bundle that may hold it and its
/// work objects; then the handshake reports it deleted. Another redaction's work is kept.
// Revert: skip `delete_staged` in the early step → the staged copy stays → red. Revert: delete the
// canonical copy in the early step → it is gone before the late step → red. Revert: skip
// `delete_canonical` in the late step → red. Revert: skip `delete_responses_before` → red.
// Revert: skip `work_delete_under` → red. Revert: report Deleted without `deleted` → red.
#[test]
fn a_withheld_blob_loses_its_staged_copies_and_then_its_canonical_one_late() {
    use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
    use forklift_core::enums::dir_entry_type::DirEntryType;
    use forklift_core::model::tree_item::TreeItem;

    // A store that stages uploads takes none outright, so the objects are seeded.
    let head = Head::new(MemoryObjectStore::with_redirect("https://s3.example/bucket"), MemoryRefStore::new());
    let seed = |object: forklift_core::model::object::loose_object::LooseObject| {
        head.objects.put_verified(&object.hash, &object.content).expect("seed an object");
        object.hash
    };
    let (leak, other) = (blob_hash("taken down\n"), blob_hash("1"));
    let mut root = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
    for (name, content) in [("x", "taken down\n"), ("y", "1")] {
        let hash = seed(LooseObjectBuilder::build_blob(&forklift_core::model::blob::Blob { content: content.as_bytes().to_vec() }));
        root.add_child(TreeItem::new(name.to_string(), hash, DirEntryType::Normal));
    }
    root.hash = seed(LooseObjectBuilder::build_tree(&root));
    let main = seed(LooseObjectBuilder::build_parcel(&forklift_core::model::parcel::Parcel {
        tree_hash: root.hash, parents: Vec::new(), actions: Vec::new(), description: Some("gate".to_string()),
    }));
    plant(&head.refs, "main", &main);

    head.batch(&[leak.clone(), other.clone()]).expect("a bundle holding the content, before the withhold");
    head.objects.stage("lift-9", &leak, blob_object("taken down\n"));
    withhold(&head, &leak);

    assert_eq!(served_withheld(&head), vec![(leak.clone(), STATEMENT.to_string(), ServedWithholdingState::DeletionOwed)]);
    assert_eq!(head.objects.staged_count(), 0, "the staged copy goes at once");
    assert!(head.objects.exists(&leak).expect("exists"), "the canonical copy waits for the late step");
    let item = withheld_item(&head, &leak);
    assert!(item.swept && !item.deleted && !item.kept && item.lease_until.is_none(), "{:?}", item);

    // A copy staged after the first step, and work objects, before the late step is due.
    head.objects.stage("lift-10", &leak, blob_object("taken down\n"));
    head.objects.work_put(&format!("withheld-{}/x", leak), b"{}").expect("work");
    head.objects.work_put(&format!("{}/pallets", other), b"{}").expect("work");
    head.objects.age_responses(std::time::Duration::from_secs(1_200));
    head.batch(std::slice::from_ref(&other)).expect("a bundle written after the cutoff");
    assert_eq!(head.objects.response_count(), 2);
    assert_eq!(head.objects.staged_count(), 1, "not due: the late copy is kept");

    age_withheld(&head.refs, &leak, LATE_SWEEP_AFTER + 40);
    assert_eq!(served_withheld(&head), vec![(leak.clone(), STATEMENT.to_string(), ServedWithholdingState::Deleted)]);
    assert!(!head.objects.exists(&leak).expect("exists"), "the late step deletes the canonical copy");
    assert_eq!(head.objects.staged_count(), 0, "and the copy staged after the first step");
    assert_eq!(head.objects.response_count(), 1, "only the bundle written after the cutoff is kept");
    assert_eq!(head.objects.work_names(), vec![format!("{}/pallets", other)], "only another redaction's work is kept");
    let item = withheld_item(&head, &leak);
    assert!(item.swept && item.deleted && !item.kept && item.lease_until.is_none(), "{:?}", item);
    assert_host_withheld(&head.object_get(&leak).expect_err("still refused"), &leak);
    assert!(head.objects.exists(&other).expect("exists"), "other content is kept");
}

/// A meta history's head whose first parcel names `content` as a file, and whose second, on it,
/// does not: the second's hash.
fn meta_history_naming<O: ObjectStore, R: RefStore>(head: &Head<O, R>, content: &str, other: &str) -> String {
    let first = gate_parcel(head, &[], vec![gate_file(head, "cell", content)]);
    gate_parcel(head, &[&first], vec![gate_file(head, "cell", other)])
}

/// A withheld blob that meta history, or the trust anchor's adopted history, names is kept for
/// good once its late step is due: its canonical copy stays, its staged copies go, and the
/// handshake reports its deletion owed from then on. The walk spans requests when a request
/// cannot finish it.
// Revert: walk no meta pallet → the blob it names is deleted → red. Revert: walk no adopts pin →
// the blob only it names is deleted → red. Revert: never save the walk → under the step limit it
// never finishes → red. Revert: delete the canonical copy when kept → red.
#[test]
fn a_withheld_blob_meta_or_adopted_history_names_is_kept_for_good() {
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    let meta = meta_history_naming(&head, "a cell\n", "a later cell\n");
    let adopted = meta_history_naming(&head, "an adopted cell\n", "a later adopted cell\n");
    plant_meta(&head.refs, "tags", None, &meta);
    let fence = forklift_aws_lambda::format::gate(&head.refs).expect("format gate");
    let anchor = office_utils::TrustAnchor {
        genesis: "e".repeat(64),
        enabled_at: 0,
        boundary: Vec::new(),
        prior_genesis: Some("d".repeat(64)),
        adopts: Some(adopted),
    };
    assert_eq!(head.refs.put_trust_if_absent(&anchor, &fence, &request_token()).expect("trust"), TrustOutcome::Established);

    let (cell, adopted_cell) = (blob_hash("a cell\n"), blob_hash("an adopted cell\n"));
    for hash in [&cell, &adopted_cell] {
        withhold(&head, hash);
    }
    served_withheld(&head);
    served_withheld(&head);
    for hash in [&cell, &adopted_cell] {
        head.objects.stage("lift-9", hash, b"a late copy".to_vec());
        age_withheld(&head.refs, hash, LATE_SWEEP_AFTER + 40);
    }

    // One step a request, and one object read a step: the walks span requests.
    let head = head.with_scan_step_limit(1);
    let mut requests = 0;
    while !(withheld_item(&head, &cell).deleted && withheld_item(&head, &adopted_cell).deleted) {
        requests += 1;
        assert!(requests < 40, "the walks finish");
        served_withheld(&head);
    }
    assert!(requests > 2, "the walks spanned requests: {}", requests);

    for hash in [&cell, &adopted_cell] {
        let item = withheld_item(&head, hash);
        assert!(item.kept && item.swept && item.deleted, "{:?}", item);
        assert!(head.objects.exists(hash).expect("exists"), "a kept blob keeps its canonical copy");
    }
    assert_eq!(head.objects.staged_count(), 0, "its staged copies go");
    assert!(head.objects.work_names().is_empty(), "the saved walks go: {:?}", head.objects.work_names());
    let states: Vec<ServedWithholdingState> = served_withheld(&head).into_iter().map(|(_, _, state)| state).collect();
    assert_eq!(states, vec![ServedWithholdingState::DeletionOwed; 2]);
}

/// A meta kind's format marker is kept from the moment it is settled, with no walk: no deletion
/// may leave a kind uncreatable.
// Revert: drop the format-marker keep from `settle` → nothing names it, and the late step
// deletes it → red.
#[test]
fn a_withheld_format_marker_is_kept_from_settling() {
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    let marker = "forklift-meta 1 kv";
    let main = gate_parcel(&head, &[], vec![gate_file(&head, "format", marker)]);
    plant(&head.refs, "main", &main);
    let hash = blob_hash(marker);
    withhold(&head, &hash);

    served_withheld(&head);
    assert!(withheld_item(&head, &hash).kept, "kept at settling");
    age_withheld(&head.refs, &hash, LATE_SWEEP_AFTER + 40);
    served_withheld(&head);

    let item = withheld_item(&head, &hash);
    assert!(item.kept && item.deleted, "{:?}", item);
    assert!(head.objects.exists(&hash).expect("exists"), "the marker is kept");
    assert_host_withheld(&head.object_get(&hash).expect_err("still refused"), &hash);
}

/// Every file content hash a parcel's tree names, read from the warehouse at `root`.
fn tree_files(root: &Path, parcel: &str) -> std::collections::HashSet<String> {
    let _scope = StorageRootScope::enter(root);
    let mut files = std::collections::HashSet::new();
    let mut pending = vec![object_utils::load_parcel(parcel).expect("parcel").tree_hash];
    while let Some(hash) = pending.pop() {
        let tree = object_utils::load_tree(&hash).expect("tree");
        files.extend(tree.get_files().map(|(_, file)| file.hash.clone()));
        pending.extend(tree.get_subtrees().map(|(_, subtree)| subtree.hash.clone()));
    }
    files
}

/// Until its item is recorded kept, a meta pallet's update that newly names withheld content is
/// refused `451` with the statement, and one whose old head's tree names it already lifts; so
/// does one whose old head only names it in its history, adding nothing that names it. A kept
/// item refuses nothing.
// Revert: drop `refuse_meta_naming_withheld` from `ref_update` → the first update commits → red.
// Revert: bound nothing by the old head's tree → the second is refused → red. Revert: judge the
// whole history rather than the segment → the third is refused → red. Revert: guard kept items
// too → the kept case is refused → red. Revert: guard format markers → the creation is refused → red.
#[test]
fn a_meta_update_newly_naming_withheld_content_is_refused_451() {
    let area = Area::new("withheld-meta");
    prepare(&area, "wh");
    area.write_file("wh/notes.txt", "notes\n");
    area.forklift("wh", &["load", "."]);
    area.forklift("wh", &["stack", "-m", "notes"]);
    let mut heads = Vec::new();
    for step in [&["manifest", "set", "main", "k1", "v1"][..], &["manifest", "set", "main", "k2", "v2"],
                 &["manifest", "set", "main", "k3", "v3"], &["manifest", "unset", "main", "k2"],
                 &["manifest", "unset", "main", "k3"], &["manifest", "set", "main", "k4", "v4"]] {
        area.forklift("wh", step);
        heads.push(harvest(&area.path("wh")).head_of("@kv").expect("kv head"));
    }
    let tagged = harvest(&area.path("wh"));
    let added: Vec<String> = tree_files(&area.path("wh"), &heads[1]).difference(&tree_files(&area.path("wh"), &heads[0])).cloned().collect();
    assert_eq!(added.len(), 1, "k2 adds one cell: {:?}", added);
    let cell = added[0].clone();
    assert!(!tree_files(&area.path("wh"), &heads[4]).contains(&cell) && !tree_files(&area.path("wh"), &heads[5]).contains(&cell));
    let meta_head = |head: &Head<MemoryObjectStore, MemoryRefStore>| {
        head.refs.get_head(pallet_utils::PalletNamespace::Meta, "kv").unwrap().live().map(str::to_string)
    };
    let update = |head: &Head<MemoryObjectStore, MemoryRefStore>, old: usize, new: usize| {
        head.ref_update(&Principal::Static, "@kv", &RefUpdateRequest {
            old_head: Some(heads[old].clone()), new_head: heads[new].clone(), tombstone: None,
        })
    };

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &tagged);
    plant(&head.refs, "main", &tagged.head_of("main").expect("main"));
    withhold(&head, &cell);
    plant_meta(&head.refs, "kv", None, &heads[0]);

    assert_host_withheld(&update(&head, 0, 1).expect_err("k2 newly names it"), &cell);
    assert_eq!(meta_head(&head), Some(heads[0].clone()));

    plant_meta(&head.refs, "kv", Some(&heads[0]), &heads[1]);
    update(&head, 1, 2).expect("its old head's tree names it");
    plant_meta(&head.refs, "kv", Some(&heads[2]), &heads[4]);
    update(&head, 4, 5).expect("only its old head's history names it, and it adds nothing naming it");
    assert_eq!(meta_head(&head), Some(heads[5].clone()));

    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &tagged);
    plant(&head.refs, "main", &tagged.head_of("main").expect("main"));
    withhold(&head, &cell);
    rewrite_withheld(&head.refs, &cell, |item| item["kept"] = true.into());
    plant_meta(&head.refs, "kv", None, &heads[0]);
    update(&head, 0, 1).expect("a kept item refuses nothing");

    // A format marker is kept from settling, so even unsettled it refuses no creation.
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    upload_all(&head, &tagged);
    plant(&head.refs, "main", &tagged.head_of("main").expect("main"));
    withhold(&head, &blob_hash("forklift-meta 1 kv"));
    head.ref_update(&Principal::Static, "@kv", &RefUpdateRequest { old_head: None, new_head: heads[0].clone(), tombstone: None })
        .expect("the kv marker withheld, the pallet is still created");
}

/// An item whose deletion step keeps failing waits before it is taken again, so the requests
/// between take the other items' steps: one never starves the rest.
// Revert: clear the lease of a failed step → every request takes the failing item first → red.
#[test]
fn a_withheld_item_whose_step_fails_never_starves_another() {
    let head = Head::new(MemoryObjectStore::new(), MemoryRefStore::new());
    let main = gate_parcel(&head, &[], vec![gate_file(&head, "a", "first\n"), gate_file(&head, "b", "second\n")]);
    plant(&head.refs, "main", &main);
    let mut hashes = [blob_hash("first\n"), blob_hash("second\n")];
    hashes.sort();
    let [failing, other] = hashes;
    for hash in [&failing, &other] {
        withhold(&head, hash);
    }
    served_withheld(&head);
    served_withheld(&head);
    for hash in [&failing, &other] {
        age_withheld(&head.refs, hash, LATE_SWEEP_AFTER + 40);
    }

    // The first item's saved walk reaches a parcel the store lacks: its step fails every time.
    let missing = object_utils::hash_object_bytes(b"a parcel this store never had");
    let walk = format!(r#"{{"version":1,"heads":[],"walk":{{"history":true,"frontier":[["{}",true]],"visited":[]}}}}"#, missing);
    head.objects.work_put(&format!("withheld-{}/keepers", failing), walk.as_bytes()).expect("work");

    for _ in 0..4 {
        served_withheld(&head);
    }
    let item = withheld_item(&head, &failing);
    assert!(!item.deleted && item.lease_until.is_some(), "the failing step waits: {:?}", item);
    assert!(withheld_item(&head, &other).deleted, "the other item's step ran");
    assert!(!head.objects.exists(&other).expect("exists"));
    assert!(head.objects.exists(&failing).expect("exists"), "an unfinished walk deletes nothing");
}
