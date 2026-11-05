//! `forklift redact` records a permanent redaction on the `@redactions` meta pallet: one cell per
//! redacted object hash, never changed or removed, refused outright for a hash some meta pallet's
//! history names.

use std::path::PathBuf;
use std::process::Output;

use forklift_core::util::object_utils;

use crate::common;
use common::forklift_command;

struct Warehouse {
    root: PathBuf,
    home: PathBuf,
}

impl Warehouse {
    fn new(name: &str) -> Warehouse {
        let base = std::env::temp_dir()
            .join(format!("forklift-redaction-{}-{}", name, std::process::id()));
        let root = base.join("warehouse");
        let home = base.join("home");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        let warehouse = Warehouse { root, home };
        warehouse.run_ok(&["prepare"]);
        warehouse.run_ok(&["config", "set", "operator.identifier", "redactor@forklift"]);

        warehouse
    }

    fn run(&self, args: &[&str]) -> Output {
        forklift_command()
            .args(args)
            .current_dir(&self.root)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("keys"))
            .output()
            .unwrap()
    }

    fn run_ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "`{}` failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut full = vec!["--json"];
        full.extend_from_slice(args);
        let output = self.run(&full);

        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap_or_else(|error| {
            panic!("not JSON ({}): {}", error, String::from_utf8_lossy(&output.stdout))
        })
    }

    fn stack(&self, file: &str, content: &[u8], message: &str) {
        let path = self.root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
        self.run_ok(&["load", "."]);
        self.run_ok(&["stack", "-m", message]);
    }

    fn redactions(&self) -> Vec<serde_json::Value> {
        self.json(&["redact", "list"])["data"]["redactions"].as_array().unwrap().clone()
    }
}

fn hash_of(content: &[u8]) -> String {
    object_utils::hash_object_bytes(&object_utils::blob_bytes(content))
}

/// Record a redaction of `target`, which must succeed, and return the JSON report.
fn recorded_hash(warehouse: &Warehouse, target: &str, reason: &str) -> serde_json::Value {
    let recorded = warehouse.json(&["redact", "record", target, "-m", reason]);
    assert_eq!(recorded["ok"], true, "{}", recorded);
    recorded
}

#[test]
fn a_redaction_is_recorded_listed_and_never_recorded_twice() {
    let warehouse = Warehouse::new("record");
    warehouse.stack("secret.env", b"TOKEN=hunter2\n", "leak");

    assert!(warehouse.redactions().is_empty());

    let recorded = recorded_hash(&warehouse, "main:secret.env", "leaked token");
    let hash = recorded["data"]["hash"].as_str().unwrap().to_string();
    assert_eq!(recorded["data"]["chunked"], false, "{}", recorded);
    assert!(recorded["data"]["redactions_head"].as_str().is_some(), "{}", recorded);

    let listed = warehouse.redactions();
    assert_eq!(listed.len(), 1, "{:?}", listed);
    assert_eq!(listed[0]["hash"], hash.as_str());
    assert_eq!(listed[0]["reason"], "leaked token");
    assert_eq!(listed[0]["redactor"], "redactor@forklift");
    assert!(listed[0].get("signer").is_none(), "unenrolled, it writes unsigned: {:?}", listed);
    assert!(listed[0].get("chunks").is_none(), "{:?}", listed);

    // Again, by path and by hash: a redaction is permanent, and the second leaves the first as it
    // was.
    for target in ["main:secret.env", hash.as_str()] {
        let again = warehouse.run(&["redact", "record", target, "-m", "again"]);
        assert!(!again.status.success());
        assert!(String::from_utf8_lossy(&again.stderr).contains("already redacted"), "{:?}", again);
    }

    assert_eq!(warehouse.redactions(), listed);
}

#[test]
fn a_held_hash_is_redacted_by_hash_and_only_file_content_is() {
    let warehouse = Warehouse::new("by-hash");
    warehouse.stack("dir/secret.env", b"TOKEN=hunter2\n", "leak");

    // Not a file: a directory path (which the tree lookup does not find), a parcel hash,
    // an absent hash, a malformed target, a missing path.
    let parcel = std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main"))
        .unwrap()
        .trim()
        .to_string();
    let absent = "0".repeat(64);

    for (target, expected) in [
        ("main:dir", "was not found"),
        (parcel.as_str(), "not a file's content"),
        (absent.as_str(), "is not in this store"),
        ("not-a-target", "neither a full object hash"),
        ("main:missing.txt", "was not found"),
    ] {
        let refused = warehouse.run(&["redact", "record", target, "-m", "r"]);
        assert!(!refused.status.success(), "{} must be refused", target);
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(stderr.contains(expected), "{}: {:?}", target, refused);
    }

    assert!(warehouse.redactions().is_empty());

    // An empty reason is refused before anything is written.
    let empty = warehouse.run(&["redact", "record", "main:dir/secret.env", "-m", " "]);
    assert!(!empty.status.success());
    assert!(String::from_utf8_lossy(&empty.stderr).contains("empty reason"), "{:?}", empty);

    let hash = hash_of(b"TOKEN=hunter2\n");
    let recorded = recorded_hash(&warehouse, &hash, "leaked token");
    assert_eq!(recorded["data"]["hash"], hash.as_str(), "{}", recorded);
}

#[test]
fn content_a_meta_pallet_history_names_is_refused_with_its_code() {
    let warehouse = Warehouse::new("meta-held");
    warehouse.stack("notes.txt", b"first\n", "first");
    warehouse.run_ok(&["tag", "create", "v1", "main"]);

    // A user file whose bytes are exactly the @tags format marker shares its blob with every
    // @tags parcel's tree.
    warehouse.stack("marker.txt", b"forklift-meta 1 tags", "marker");

    let refused = warehouse.run(&["--json", "redact", "record", "main:marker.txt", "-m", "r"]);
    let stdout = String::from_utf8_lossy(&refused.stdout);
    assert_eq!(refused.status.code(), Some(48), "{}", stdout);
    let document: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let error = &document["error"];
    assert_eq!(error["code"], "redaction_blocked", "{}", document);
    assert_eq!(error["details"]["meta_pallets"], serde_json::json!(["@tags"]), "{}", document);
    assert!(warehouse.redactions().is_empty());

    // A file no meta pallet names is redacted as usual in the same warehouse.
    recorded_hash(&warehouse, "main:notes.txt", "fine");
    assert_eq!(warehouse.redactions().len(), 1);
}

#[test]
fn an_enrolled_admin_records_a_signed_redaction() {
    let warehouse = Warehouse::new("signed");
    warehouse.run_ok(&["office", "enroll"]);
    warehouse.stack("secret.env", b"TOKEN=hunter2\n", "leak");

    recorded_hash(&warehouse, "main:secret.env", "leaked token");

    let listed = warehouse.redactions();
    assert_eq!(listed.len(), 1, "{:?}", listed);
    assert!(listed[0]["signer"].as_str().is_some(), "an admin under trust signs: {:?}", listed);
}

#[test]
fn a_chunked_file_is_redacted_with_its_chunk_list() {
    let warehouse = Warehouse::new("chunked");

    // Above the chunking threshold, and varied, so the file is a recipe of several chunks.
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let content: Vec<u8> = (0..9 * 1024 * 1024).map(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as u8
    }).collect();
    warehouse.stack("big.bin", &content, "big");

    let recorded = recorded_hash(&warehouse, "main:big.bin", "leaked dump");
    assert_eq!(recorded["data"]["chunked"], true, "{}", recorded);

    let listed = warehouse.redactions();
    let chunks = listed[0]["chunks"].as_str().expect("a recipe's redaction carries its chunk list");
    assert_eq!(chunks.len(), 64, "{:?}", listed);
    assert_ne!(listed[0]["hash"], chunks, "{:?}", listed);
}
