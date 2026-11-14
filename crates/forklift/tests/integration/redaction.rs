//! `forklift redact` records a permanent redaction on the `@redactions` meta pallet: one cell per
//! redacted object hash, never changed or removed, refused outright for a hash some meta pallet's
//! history names, and refused while a pallet head names it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Output;

use forklift_core::util::object_utils;
use forklift_core::util::redaction_utils::chunk_list_digest;

use crate::common;
use common::forklift_command;

struct Warehouse {
    root: PathBuf,
    home: PathBuf,
}

impl Warehouse {
    fn new(name: &str) -> Warehouse {
        let warehouse = Warehouse::empty(name);
        std::fs::create_dir_all(&warehouse.root).unwrap();
        warehouse.run_ok(&["prepare"]);
        warehouse.run_ok(&["config", "set", "operator.identifier", "redactor@forklift"]);

        warehouse
    }

    /// A clone of the remote at `url`.
    fn cloned(name: &str, url: &str) -> Warehouse {
        Warehouse::cloned_with(name, url, &[])
    }

    /// A clone of the remote at `url`, with `extra` arguments.
    fn cloned_with(name: &str, url: &str, extra: &[&str]) -> Warehouse {
        let warehouse = Warehouse::empty(name);
        let cloned = forklift_command()
            .args(["clone", url, warehouse.root.to_str().unwrap()])
            .args(extra)
            .env("FORKLIFT_GLOBAL_CONFIG", warehouse.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", warehouse.home.join("keys"))
            .output()
            .unwrap();
        assert!(cloned.status.success(), "clone failed: {}", String::from_utf8_lossy(&cloned.stderr));
        warehouse.run_ok(&["config", "set", "operator.identifier", "redactor@forklift"]);

        warehouse
    }

    /// A fresh scratch directory with nothing in the warehouse root yet.
    fn empty(name: &str) -> Warehouse {
        let base = std::env::temp_dir()
            .join(format!("forklift-redaction-{}-{}", name, std::process::id()));
        let root = base.join("warehouse");
        let home = base.join("home");
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
        std::fs::create_dir_all(&home).unwrap();

        Warehouse { root, home }
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

    /// Stack `file` with `content`, then take it out again, and return the parcel that held it: a
    /// leak that history keeps and no pallet head names.
    fn leaked(&self, file: &str, content: &[u8]) -> String {
        self.stack(file, content, "leak");
        let parcel = self.head("main");
        self.run_ok(&["remove", file]);
        self.run_ok(&["stack", "-m", "take the leak out"]);

        parcel
    }

    /// Record a redaction of `content` by its hash while no head names it: the file is only
    /// loaded, so the store holds the blob, and is unloaded and deleted once the redaction is in.
    fn redact_loaded(&self, file: &str, content: &[u8], reason: &str) -> String {
        std::fs::write(self.root.join(file), content).unwrap();
        self.run_ok(&["load", file]);
        let hash = hash_of(content);
        recorded_hash(self, &hash, reason);
        self.run_ok(&["unload", file]);
        forklift_core::util::file_utils::remove_file(self.root.join(file)).unwrap();

        hash
    }

    /// Park a file holding `content`, so a parked parcel names it and the deletion of an applied
    /// redaction keeps its copy; the bay is left clean.
    fn hold_parked(&self, content: &[u8]) {
        std::fs::write(self.root.join("held.env"), content).unwrap();
        self.run_ok(&["load", "held.env"]);
        self.run_ok(&["park"]);
    }

    /// Whether the store holds a loose copy of `hash`.
    fn holds_loose(&self, hash: &str) -> bool {
        self.root.join(".forklift/objects").join(&hash[..2]).join(&hash[2..]).exists()
    }

    /// Remove the loose object `hash` from the store, as no redaction does: damage.
    fn remove_object(&self, hash: &str) {
        let path = self.root.join(".forklift/objects").join(&hash[..2]).join(&hash[2..]);
        forklift_core::util::file_utils::remove_file(&path).unwrap_or_else(|error| panic!("{}: {}", path.display(), error));
    }

    fn head(&self, pallet: &str) -> String {
        std::fs::read_to_string(self.root.join(".forklift/pallets").join(pallet)).unwrap().trim().to_string()
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
    let leak = warehouse.leaked("secret.env", b"TOKEN=hunter2\n");
    let target = format!("{}:secret.env", leak);

    assert!(warehouse.redactions().is_empty());

    let recorded = recorded_hash(&warehouse, &target, "leaked token");
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
    for target in [target.as_str(), hash.as_str()] {
        let again = warehouse.run(&["redact", "record", target, "-m", "again"]);
        assert!(!again.status.success());
        assert!(String::from_utf8_lossy(&again.stderr).contains("already redacted"), "{:?}", again);
    }

    assert_eq!(warehouse.redactions(), listed);
}

#[test]
fn a_held_hash_is_redacted_by_hash_and_only_file_content_is() {
    let warehouse = Warehouse::new("by-hash");
    let leak = warehouse.leaked("dir/secret.env", b"TOKEN=hunter2\n");

    // Not a file: a directory path (which the tree lookup does not find), a parcel hash,
    // an absent hash, a malformed target, a missing path.
    let parcel = warehouse.head("main");
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
    let empty = warehouse.run(&["redact", "record", &format!("{}:dir/secret.env", leak), "-m", " "]);
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

    // A file no meta pallet or head names is redacted as usual in the same warehouse.
    warehouse.redact_loaded("loaded.txt", b"loaded\n", "fine");
    assert_eq!(warehouse.redactions().len(), 1);
}

/// Run `redact record <target>` in `--json` mode, which must be refused as `head_names_content`
/// with nothing recorded, and return the error.
fn head_names_content(warehouse: &Warehouse, target: &str) -> serde_json::Value {
    let refused = warehouse.run(&["--json", "redact", "record", target, "-m", "r"]);
    let stdout = String::from_utf8_lossy(&refused.stdout);
    assert_eq!(refused.status.code(), Some(50), "{}{}", stdout, String::from_utf8_lossy(&refused.stderr));
    let document: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(document["error"]["code"], "head_names_content", "{}", document);
    assert!(warehouse.redactions().is_empty(), "nothing was recorded");
    document["error"].clone()
}

#[test]
fn content_a_pallet_head_names_is_refused_until_the_file_is_taken_out() {
    let warehouse = Warehouse::new("head-held");
    warehouse.stack("secret.env", b"TOKEN=hunter2\n", "leak");
    let hash = hash_of(b"TOKEN=hunter2\n");

    // A second pallet at the same head, under another name too, and this bay stands on it.
    warehouse.run_ok(&["pallet", "create", "side"]);
    std::fs::write(warehouse.root.join("copy.env"), b"TOKEN=hunter2\n").unwrap();
    forklift_core::util::file_utils::remove_file(warehouse.root.join("secret.env")).unwrap();
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "-m", "renamed"]);

    // Every head is named, and the next step starts from the one this bay stands on.
    let (main, side) = (warehouse.head("main"), warehouse.head("side"));
    let error = head_names_content(&warehouse, &hash);
    assert_eq!(error["details"]["hash"], hash.as_str(), "{}", error);
    assert_eq!(error["details"]["heads"], serde_json::json!([
        { "pallet": "main", "head": main, "path": "secret.env" },
        { "pallet": "side", "head": side, "path": "copy.env" },
    ]), "{}", error);
    assert_eq!(
        error["next_step"].as_str().unwrap(),
        format!(
            "from the working tree's root, forklift remove copy.env, then forklift stack -m <description>, \
            then take it out of the other heads in details.heads the same way, then forklift redact record {}:copy.env -m <reason>",
            side
        ),
        "{}", error
    );

    // By path it is the same object.
    head_names_content(&warehouse, "side:copy.env");

    // Taken out of side, main's head still names it, and this bay cannot take it out of that.
    warehouse.run_ok(&["remove", "copy.env"]);
    warehouse.run_ok(&["stack", "-m", "take it out"]);
    let error = head_names_content(&warehouse, &format!("{}:copy.env", side));
    assert_eq!(error["details"]["heads"].as_array().unwrap().len(), 1, "{}", error);
    assert_eq!(
        error["next_step"].as_str().unwrap(),
        format!(
            "stack a head of pallet \"main\" without \"secret.env\", from a bay standing on its head with that path in scope, \
            then forklift redact record {}:secret.env -m <reason>",
            main
        ),
        "{}", error
    );

    // From a bay on main, the next step is the commands themselves; followed, the redaction lands.
    warehouse.run_ok(&["shift", "main"]);
    let error = head_names_content(&warehouse, &hash);
    assert_eq!(
        error["next_step"].as_str().unwrap(),
        format!(
            "from the working tree's root, forklift remove secret.env, then forklift stack -m <description>, \
            then forklift redact record {}:secret.env -m <reason>",
            main
        ),
        "{}", error
    );

    warehouse.run_ok(&["remove", "secret.env"]);
    warehouse.run_ok(&["stack", "-m", "take it out"]);
    let recorded = recorded_hash(&warehouse, &format!("{}:secret.env", main), "leaked token");
    assert_eq!(recorded["data"]["hash"], hash.as_str(), "{}", recorded);
}

#[test]
fn an_enrolled_admin_records_a_signed_redaction() {
    let warehouse = Warehouse::new("signed");
    warehouse.run_ok(&["office", "enroll"]);
    let leak = warehouse.leaked("secret.env", b"TOKEN=hunter2\n");

    recorded_hash(&warehouse, &format!("{}:secret.env", leak), "leaked token");

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
    let leak = warehouse.leaked("big.bin", &content);

    let recorded = recorded_hash(&warehouse, &format!("{}:big.bin", leak), "leaked dump");
    assert_eq!(recorded["data"]["chunked"], true, "{}", recorded);

    let listed = warehouse.redactions();
    let chunks = listed[0]["chunks"].as_str().expect("a recipe's redaction carries its chunk list");
    assert_eq!(chunks.len(), 64, "{:?}", listed);
    assert_ne!(listed[0]["hash"], chunks, "{:?}", listed);
}

/// Run `args` in `--json` mode, which must be refused as `content_redacted`, and return the error.
fn content_redacted(warehouse: &Warehouse, args: &[&str]) -> serde_json::Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    let refused = warehouse.run(&full);
    let stdout = String::from_utf8_lossy(&refused.stdout);
    assert_eq!(refused.status.code(), Some(49), "`{}`: {}{}", args.join(" "), stdout, String::from_utf8_lossy(&refused.stderr));
    let document: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(document["error"]["code"], "content_redacted", "{}", document);
    document["error"].clone()
}

/// Once a redaction is applied and its object removed, a read that needs the bytes is refused as
/// redacted, naming the hash. The parcel's other content still reads, and an absent object that no
/// redaction names still fails as a missing object.
///
/// Revert: drop the applied check from the store's read → blame and diff fail with code `error`.
#[test]
fn a_read_of_content_whose_redaction_is_applied_is_refused_as_redacted() {
    let warehouse = Warehouse::new("read-applied");
    warehouse.stack("notes.txt", b"notes\n", "notes");
    let before = warehouse.head("main");
    let leak = warehouse.leaked("secret.env", b"TOKEN=hunter2\n");
    let recorded = recorded_hash(&warehouse, &format!("{}:secret.env", leak), "leaked token");
    assert_eq!(recorded["data"]["status"], serde_json::json!({ "state": "applied" }), "{}", recorded);
    let hash = recorded["data"]["hash"].as_str().unwrap().to_string();

    // Applied, and nothing names it, so its copy is deleted as it is recorded.
    assert_eq!(recorded["data"]["deletion"]["deleted"], serde_json::json!([hash]), "{}", recorded);
    assert!(!warehouse.holds_loose(&hash));

    let error = content_redacted(&warehouse, &["blame", "-r", &leak, "secret.env"]);
    assert_eq!(error["details"], serde_json::json!({ "hash": hash }), "{}", error);
    assert_eq!(
        error["message"].as_str().unwrap(),
        format!("Cannot read object {}: it is redacted in this warehouse, and its content has been \
                removed for good.", hash)
    );
    assert_eq!(error["next_step"], "Run \"forklift redact list\" to see the redaction.", "{}", error);

    // A diff's report names the file without its lines, so it needs no bytes; its printed lines do.
    let listed = warehouse.json(&["diff", &before, &leak]);
    assert_eq!(listed["data"]["files"], serde_json::json!([{ "kind": "added", "path": "secret.env" }]), "{}", listed);

    let reads: [&[&str]; 3] = [
        &["blame", "-r", &leak, "secret.env"],
        &["diff", &before, &leak],
        &["diff", &before, &leak, "secret.env"],
    ];

    for args in reads {
        let human = warehouse.run(args);
        assert_eq!(human.status.code(), Some(49), "{:?}: {:?}", args, human);
        assert!(String::from_utf8_lossy(&human.stderr).contains("is redacted in this warehouse"), "{:?}: {:?}", args, human);
    }

    // The parcel's other content is untouched.
    warehouse.run_ok(&["blame", "-r", &leak, "notes.txt"]);

    // An object absent with no redaction naming it is missing, not redacted.
    warehouse.remove_object(&hash_of(b"notes\n"));
    let missing = warehouse.json(&["blame", "-r", &leak, "notes.txt"]);
    assert_eq!(missing["error"]["code"], "error", "{}", missing);
    assert!(missing["error"]["message"].as_str().unwrap().contains("entity not found"), "{}", missing);

    // An applied record that cannot be read leaves the absence undecided: the read still names
    // the missing object, with the record's own error added rather than in its place.
    std::fs::write(warehouse.root.join(".forklift/redactions-applied"), "/forklift-redactions-applied 99\n").unwrap();
    let undecided = warehouse.json(&["blame", "-r", &leak, "notes.txt"]);
    let message = undecided["error"]["message"].as_str().unwrap();
    assert!(message.contains("entity not found"), "{}", undecided);
    assert!(message.contains("Whether a redaction removed it could not be told"), "{}", undecided);
}

/// A full audit of a warehouse whose applied redaction's object is gone passes, and counts the
/// object as redacted, apart from what the sealed record holds; an absent object no redaction
/// names still fails it.
///
/// Revert: leave the redacted count out of the scope → `redacted` is missing from the report.
#[test]
fn a_full_audit_counts_an_applied_redaction_as_redacted_and_passes() {
    let warehouse = Warehouse::new("audit-applied");
    warehouse.run_ok(&["office", "enroll"]);
    warehouse.stack("notes.txt", b"notes\n", "notes");
    let leak = warehouse.leaked("secret.env", b"TOKEN=hunter2\n");

    // Held by a parked parcel, the copy is kept and reported with its holder: nothing is absent.
    warehouse.hold_parked(b"TOKEN=hunter2\n");
    let recorded = recorded_hash(&warehouse, &format!("{}:secret.env", leak), "leaked token");
    assert_eq!(recorded["data"]["status"], serde_json::json!({ "state": "applied" }), "{}", recorded);
    let hash = recorded["data"]["hash"].as_str().unwrap();
    assert_eq!(recorded["data"]["deletion"]["deleted"], serde_json::json!([]), "{}", recorded);
    assert_eq!(recorded["data"]["deletion"]["kept"], serde_json::json!([{
        "hash": hash,
        "holders": [{ "kind": "parked", "description": "a parcel parked in the warehouse's own working tree" }],
    }]), "{}", recorded);
    assert!(warehouse.holds_loose(hash));
    let human = String::from_utf8_lossy(&warehouse.run_ok(&["redact", "apply"]).stdout).into_owned();
    assert!(human.contains(&format!(
        "Kept {}: named by a parcel parked in the warehouse's own working tree. Take the file out of it, \
        then run \"forklift redact apply\" to delete it.", hash
    )), "{}", human);
    let audited = warehouse.json(&["audit", "--full"]);
    assert_eq!(audited["ok"], true, "{}", audited);
    assert_eq!(audited["data"]["scope"].get("redacted"), None, "{}", audited);

    // Popped and taken out, nothing names it, and `redact apply` deletes it.
    warehouse.run_ok(&["park", "pop"]);
    warehouse.run_ok(&["unload", "held.env"]);
    forklift_core::util::file_utils::remove_file(warehouse.root.join("held.env")).unwrap();
    let applied = warehouse.json(&["redact", "apply"]);
    assert_eq!(applied["data"]["deletion"]["deleted"], serde_json::json!([recorded["data"]["hash"]]), "{}", applied);
    let audited = warehouse.json(&["audit", "--full"]);
    assert_eq!(audited["ok"], true, "{}", audited);
    assert_eq!(audited["data"]["scope"]["redacted"], 1, "{}", audited);
    assert_eq!(audited["data"]["scope"].get("held_by_hash"), None, "{}", audited);

    let human = warehouse.run_ok(&["audit", "--full"]);
    assert!(
        String::from_utf8_lossy(&human.stdout).contains("1 absent object(s) are redacted"),
        "{}", String::from_utf8_lossy(&human.stdout)
    );

    // An object absent with no redaction naming it is damage.
    warehouse.remove_object(&hash_of(b"notes\n"));
    let damaged = warehouse.run(&["audit", "--full"]);
    assert!(!damaged.status.success(), "{:?}", damaged);
}

/// A restore from the inventory of a file whose staged content is redacted, applied and removed is
/// refused as redacted, and leaves the working file as it was.
///
/// Revert: drop the applied check from the store's read → the restore fails with code `error`.
#[test]
fn a_restore_of_staged_content_whose_redaction_is_applied_is_refused_as_redacted() {
    let warehouse = Warehouse::new("restore-applied");
    warehouse.stack("notes.txt", b"notes\n", "notes");

    // Redacted while it is only loaded: no head names it, so it applies; the inventory still does.
    std::fs::write(warehouse.root.join("secret.env"), b"TOKEN=hunter2\n").unwrap();
    warehouse.run_ok(&["load", "secret.env"]);
    let hash = hash_of(b"TOKEN=hunter2\n");
    let recorded = recorded_hash(&warehouse, &hash, "leaked token");
    assert_eq!(recorded["data"]["status"], serde_json::json!({ "state": "applied" }), "{}", recorded);
    warehouse.remove_object(&hash);

    std::fs::write(warehouse.root.join("secret.env"), b"TOKEN=rotated\n").unwrap();
    let error = content_redacted(&warehouse, &["restore", "secret.env", "--confirm"]);
    assert_eq!(error["details"], serde_json::json!({ "hash": hash }), "{}", error);
    assert_eq!(std::fs::read(warehouse.root.join("secret.env")).unwrap(), b"TOKEN=rotated\n");
}

#[test]
fn a_stack_naming_redacted_content_is_refused_until_the_file_is_gone() {
    let warehouse = Warehouse::new("stack");
    warehouse.stack("notes.txt", b"notes\n", "notes");

    // Redacted while it is only loaded, so no head names it yet; the next stack would.
    std::fs::write(warehouse.root.join("secret.env"), b"TOKEN=hunter2\n").unwrap();
    warehouse.run_ok(&["load", "secret.env"]);
    let hash = hash_of(b"TOKEN=hunter2\n");
    recorded_hash(&warehouse, &hash, "leaked token");
    let head = std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap();

    // The file still staged, beside a new one: the new head would name it.
    std::fs::write(warehouse.root.join("more.txt"), b"more\n").unwrap();
    warehouse.run_ok(&["load", "more.txt"]);
    let error = content_redacted(&warehouse, &["stack", "-m", "notes"]);
    assert_eq!(error["details"]["path"], "secret.env", "{}", error);
    assert_eq!(error["details"]["hash"], hash.as_str(), "{}", error);
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap(), head,
        "nothing was written"
    );

    forklift_core::util::file_utils::remove_file(warehouse.root.join("secret.env")).unwrap();
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "-m", "without the secret"]);
}

#[test]
fn a_load_staging_redacted_content_is_refused_and_stages_nothing() {
    let warehouse = Warehouse::new("load");
    warehouse.stack("notes.txt", b"notes\n", "notes");
    let hash = warehouse.redact_loaded("secret.env", b"TOKEN=hunter2\n", "leaked token");

    // The same bytes under another name are the same content, by a directory load beside a file
    // that is fine and by the file alone.
    std::fs::create_dir_all(warehouse.root.join("dir")).unwrap();
    std::fs::write(warehouse.root.join("dir/copy.env"), b"TOKEN=hunter2\n").unwrap();
    std::fs::write(warehouse.root.join("more.txt"), b"more\n").unwrap();

    for target in [".", "dir", "dir/copy.env"] {
        let error = content_redacted(&warehouse, &["load", target]);
        assert_eq!(error["details"], serde_json::json!({ "path": "dir/copy.env", "hash": hash }), "{}", error);
        let message = error["message"].as_str().unwrap();
        assert!(message.starts_with("Refusing to load: \"dir/copy.env\" holds content"), "{}", error);
        assert!(message.ends_with("Nothing was staged."), "{}", error);
        assert_eq!(
            error["next_step"],
            "change or delete \"dir/copy.env\" so it no longer holds that content, then load again",
            "{}", error
        );
        assert_eq!(warehouse.json(&["status"])["data"]["staged_count"], 0, "{}: nothing was staged", target);
    }

    // Nothing is left marked as an incomplete load, so a stack of other work goes ahead.
    warehouse.run_ok(&["load", "more.txt"]);
    warehouse.run_ok(&["stack", "-m", "more"]);

    // The next step, as it says.
    std::fs::write(warehouse.root.join("dir/copy.env"), b"TOKEN=rotated\n").unwrap();
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "-m", "rotated"]);
}

#[test]
fn a_park_naming_redacted_content_is_refused_and_parks_nothing() {
    let warehouse = Warehouse::new("park");
    warehouse.stack("notes.txt", b"notes\n", "notes");

    // Redacted while it is only loaded, so no head names it; a parked parcel would.
    std::fs::write(warehouse.root.join("secret.env"), b"TOKEN=hunter2\n").unwrap();
    warehouse.run_ok(&["load", "secret.env"]);
    let hash = hash_of(b"TOKEN=hunter2\n");
    recorded_hash(&warehouse, &hash, "leaked token");

    let error = content_redacted(&warehouse, &["park"]);
    assert_eq!(error["details"], serde_json::json!({ "path": "secret.env", "hash": hash }), "{}", error);
    let message = error["message"].as_str().unwrap();
    assert!(message.starts_with("Refusing to park: \"secret.env\" holds content"), "{}", error);
    assert!(message.ends_with("Nothing was parked."), "{}", error);
    assert_eq!(
        error["next_step"],
        "change or delete \"secret.env\" so it no longer holds that content, then park again",
        "{}", error
    );
    assert_eq!(warehouse.json(&["park", "list"])["data"]["parked"], serde_json::json!([]), "nothing was parked");
    assert_eq!(std::fs::read(warehouse.root.join("secret.env")).unwrap(), b"TOKEN=hunter2\n", "the bay is as it was");

    // The next step, as it says.
    std::fs::write(warehouse.root.join("secret.env"), b"TOKEN=rotated\n").unwrap();
    warehouse.run_ok(&["park"]);
    assert_eq!(warehouse.json(&["park", "list"])["data"]["parked"].as_array().unwrap().len(), 1);

    // A tracked file edited to hold the content is refused before park restages it.
    std::fs::write(warehouse.root.join("notes.txt"), b"TOKEN=hunter2\n").unwrap();
    let error = content_redacted(&warehouse, &["park"]);
    assert_eq!(error["details"]["path"], "notes.txt", "{}", error);
    let status = warehouse.json(&["status"]);
    assert_eq!(status["data"]["staged_count"], 0, "nothing was staged: {}", status);
    assert_eq!(status["data"]["unstaged_count"], 1, "{}", status);
    assert_eq!(warehouse.json(&["park", "list"])["data"]["parked"].as_array().unwrap().len(), 1);
}

#[test]
fn a_pick_bringing_back_redacted_content_is_refused_before_it_touches_the_bay() {
    let warehouse = Warehouse::new("pick");
    warehouse.stack("notes.txt", b"notes\n", "notes");
    warehouse.run_ok(&["pallet", "create", "side"]);
    warehouse.run_ok(&["shift", "main"]);
    let leak = warehouse.leaked("secret.env", b"TOKEN=hunter2\n");
    let short = &leak[..12];
    // A parked parcel holds the copy, so the pick's own refusal is what stops it, not a failed read.
    forklift_core::util::file_utils::remove_file(warehouse.root.join("secret.env")).unwrap();
    warehouse.hold_parked(b"TOKEN=hunter2\n");
    let recorded = recorded_hash(&warehouse, &format!("{}:secret.env", leak), "leaked token");
    assert!(warehouse.holds_loose(recorded["data"]["hash"].as_str().unwrap()), "{}", recorded);
    warehouse.stack("fine.txt", b"fine\n", "fine");
    let fine = warehouse.head("main");

    // Picked onto a pallet whose history never held it.
    warehouse.run_ok(&["shift", "side"]);
    let head = warehouse.head("side");

    let error = content_redacted(&warehouse, &["pick", &leak]);
    assert_eq!(error["details"]["path"], "secret.env", "{}", error);
    assert_eq!(error["details"]["hash"], recorded["data"]["hash"], "{}", error);
    let message = error["message"].as_str().unwrap();
    assert!(message.starts_with(&format!("Refusing to pick {}: \"secret.env\"", short)), "{}", error);
    assert!(message.ends_with("Nothing was changed."), "{}", error);
    assert_eq!(
        error["next_step"].as_str().unwrap(),
        format!("make the change by hand without \"secret.env\" instead of picking {}", short),
        "{}", error
    );
    assert!(!warehouse.root.join("secret.env").exists(), "the content never reached the bay");
    assert_eq!(warehouse.head("side"), head);
    assert_eq!(warehouse.json(&["status"])["data"]["staged_count"], 0, "nothing was staged");

    // With the parked parcel dropped and the copy deleted, the pick is still refused as redacted,
    // and nothing reaches the bay.
    warehouse.run_ok(&["shift", "main"]);
    warehouse.run_ok(&["park", "pop"]);
    warehouse.run_ok(&["unload", "held.env"]);
    forklift_core::util::file_utils::remove_file(warehouse.root.join("held.env")).unwrap();
    warehouse.run_ok(&["redact", "apply"]);
    assert!(!warehouse.holds_loose(recorded["data"]["hash"].as_str().unwrap()));
    warehouse.run_ok(&["shift", "side"]);
    let error = content_redacted(&warehouse, &["pick", &leak]);
    assert_eq!(error["details"]["hash"], recorded["data"]["hash"], "{}", error);
    assert!(!warehouse.root.join("secret.env").exists(), "the content never reached the bay");
    assert_eq!(warehouse.head("side"), head);

    // No pick was left standing: another one goes ahead.
    warehouse.run_ok(&["pick", &fine]);
    assert!(warehouse.root.join("fine.txt").exists());
}

#[test]
fn a_meta_pallet_may_not_start_naming_redacted_content() {
    let warehouse = Warehouse::new("meta-adds");
    warehouse.stack("notes.txt", b"first\n", "first");

    // No @tags yet, so no meta history names the marker, and the marker is only loaded, so no
    // head names it: the redaction is recorded. The first @tags parcel would then add it.
    warehouse.redact_loaded("marker.txt", b"forklift-meta 1 tags", "r");

    let error = content_redacted(&warehouse, &["tag", "create", "v1", "main"]);
    assert_eq!(error["details"]["pallet"], "@tags", "{}", error);
    let listed = warehouse.run_ok(&["tag", "list"]);
    assert!(!String::from_utf8_lossy(&listed.stdout).contains("v1"), "nothing was written: {:?}", listed);
}

/// A running forklift-server over a bare warehouse at `root`.
struct Server {
    child: std::process::Child,
    url: String,
    root: PathBuf,
}

impl Server {
    fn start(root: &std::path::Path) -> Server {
        let _ = forklift_core::util::file_utils::remove_dir_all(root);
        let prepared = std::process::Command::new(common::server_binary()).env("FORKLIFT_FSYNC", "0")
            .args(["prepare", "--root", root.to_str().unwrap()]).output().unwrap();
        assert!(prepared.status.success(), "{}", String::from_utf8_lossy(&prepared.stderr));

        Server::serve(root)
    }

    /// Serve the server root at `root` as it is.
    fn serve(root: &std::path::Path) -> Server {
        Server::serve_with(root, &[])
    }

    /// Serve the server root at `root` as it is, with `args` added to `serve`'s.
    fn serve_with(root: &std::path::Path, args: &[&str]) -> Server {
        use std::io::BufRead;

        let root = root.to_str().unwrap();
        let mut child = std::process::Command::new(common::server_binary()).env("FORKLIFT_FSYNC", "0")
            .args(["serve", "--root", root, "--addr", "127.0.0.1:0", "--open"]).args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap();

        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let url = line.trim().rsplit(' ').next().unwrap().to_string();
        assert!(url.starts_with("http://"), "unexpected startup line: {}", line);

        Server { child, url, root: PathBuf::from(root) }
    }

    /// The head this server serves for `main`, read from its own ref: a fresh clone could not
    /// fetch history naming content whose redaction the server has applied.
    fn main(&self) -> String {
        std::fs::read_to_string(self.root.join(".forklift/pallets/main")).unwrap().trim().to_string()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A head stacked before a redaction came down by `lower` is refused by this warehouse's own check
/// before it is sent. The remote holds the redaction here too; in a full warehouse a recorded
/// redaction never leaves a local head naming the content, so a redaction only this warehouse holds
/// no longer reaches lift.
#[test]
fn a_lift_is_refused_for_a_head_stacked_before_a_redaction_came_down() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-lift-server-{}", std::process::id())));

    let first = Warehouse::cloned("lift-first", &server.url);
    first.stack("notes.txt", b"notes\n", "notes");
    first.run_ok(&["lift"]);

    // This warehouse stacks the file before any redaction exists, and has not lifted it.
    let warehouse = Warehouse::cloned("lift", &server.url);
    warehouse.stack("secret.env", b"TOKEN=hunter2\n", "leak");

    // Another warehouse redacts the same content, which no head it holds names, and lifts the
    // redaction; this warehouse's lower takes it. The remote serves no head naming the content.
    let other = Warehouse::cloned("lift-other", &server.url);
    let hash = other.redact_loaded("secret.env", b"TOKEN=hunter2\n", "leaked token");
    other.run_ok(&["lift"]);
    warehouse.run_ok(&["lower"]);
    assert_eq!(warehouse.redactions().len(), 1);
    let served = server.main();

    // A stack that only carries the file over from its parent, unchanged, is refused too.
    let stacked = warehouse.head("main");
    std::fs::write(warehouse.root.join("todo.txt"), b"todo\n").unwrap();
    warehouse.run_ok(&["load", "todo.txt"]);
    let error = content_redacted(&warehouse, &["stack", "-m", "todo"]);
    assert_eq!(error["details"]["path"], "secret.env", "{}", error);
    assert_eq!(warehouse.head("main"), stacked, "nothing was written");
    warehouse.run_ok(&["unload", "todo.txt"]);
    forklift_core::util::file_utils::remove_file(warehouse.root.join("todo.txt")).unwrap();

    let error = content_redacted(&warehouse, &["lift"]);
    assert_eq!(error["details"]["path"], "secret.env", "{}", error);
    assert_eq!(error["details"]["hash"], hash.as_str(), "{}", error);
    assert_eq!(error["details"]["pallet"], "main", "{}", error);
    let message = error["message"].as_str().unwrap();
    assert!(message.starts_with("Refusing to lift pallet \"main\""), "{}", error);
    assert!(message.contains("The remote's pallet \"main\" was not moved."), "{}", error);
    assert_eq!(
        error["next_step"],
        "from the working tree's root, forklift remove secret.env, then forklift stack -m <description>, then forklift lift",
        "{}", error
    );
    assert_eq!(server.main(), served, "nothing was lifted");

    // Another warehouse lifts a change of its own to main, so this lift would merge it in first.
    // It is refused in lift's own words, before any merge is written.
    let ahead = Warehouse::cloned("lift-ahead", &server.url);
    ahead.stack("more.txt", b"more\n", "more");
    ahead.run_ok(&["lift"]);
    let local = std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap();

    let error = content_redacted(&warehouse, &["lift"]);
    assert!(error["message"].as_str().unwrap().starts_with("Refusing to lift pallet \"main\""), "{}", error);
    assert_eq!(error["details"]["path"], "secret.env", "{}", error);
    assert_eq!(std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap(), local, "no merge was written");

    // The next step, as it says.
    warehouse.run_ok(&["remove", "secret.env"]);
    warehouse.run_ok(&["stack", "-m", "without the secret"]);
    warehouse.run_ok(&["lift"]);
    assert_eq!(server.main(), warehouse.head("main"));
}

#[test]
fn a_pick_conflicting_with_their_redacted_version_is_refused() {
    let warehouse = Warehouse::new("pick-conflict");
    warehouse.stack("shared.txt", b"base\n", "base");
    warehouse.run_ok(&["pallet", "create", "side"]);
    warehouse.stack("shared.txt", b"ours\n", "ours");
    warehouse.run_ok(&["shift", "main"]);

    // Main's leak changes the file both sides changed, then a later parcel changes it again, so
    // no head names the content and the redaction is recorded.
    warehouse.stack("shared.txt", b"TOKEN=hunter2\n", "leak");
    let leak = warehouse.head("main");
    warehouse.stack("shared.txt", b"rotated\n", "rotate");
    // A parked parcel holds the copy, so the pick reads their version and refuses it by path.
    warehouse.hold_parked(b"TOKEN=hunter2\n");
    let hash = recorded_hash(&warehouse, &format!("{}:shared.txt", leak), "leaked token")["data"]["hash"].clone();

    // Picked onto side, the file conflicts, and the markers would carry their version whole.
    warehouse.run_ok(&["shift", "side"]);
    let error = content_redacted(&warehouse, &["pick", &leak]);
    assert_eq!(error["details"], serde_json::json!({ "path": "shared.txt", "hash": hash }), "{}", error);
    assert_eq!(std::fs::read(warehouse.root.join("shared.txt")).unwrap(), b"ours\n", "the bay is as it was");
}

#[test]
fn a_lower_merging_redacted_content_in_is_refused_before_it_touches_the_bay() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-lower-server-{}", std::process::id())));

    let first = Warehouse::cloned("lower-first", &server.url);
    first.stack("notes.txt", b"notes\n", "notes");
    first.run_ok(&["lift"]);

    // This warehouse redacts content the remote does not know is redacted, and stacks work of its
    // own; another warehouse lifts a head naming that content, so a lower here merges it in.
    let warehouse = Warehouse::cloned("lower", &server.url);
    let hash = warehouse.redact_loaded("secret.env", b"TOKEN=hunter2\n", "leaked token");
    let head = warehouse.head("main");

    let other = Warehouse::cloned("lower-other", &server.url);
    other.stack("secret.env", b"TOKEN=hunter2\n", "leak");
    other.run_ok(&["lift"]);

    // With nothing of its own, the lower would fast-forward the bay to the remote's head.
    let error = content_redacted(&warehouse, &["lower", "main"]);
    assert_eq!(error["details"], serde_json::json!({ "path": "secret.env", "hash": hash }), "{}", error);
    let message = error["message"].as_str().unwrap();
    assert!(message.starts_with("Refusing to lower pallet \"main\": \"secret.env\""), "{}", error);
    assert_eq!(
        error["next_step"],
        "change or delete \"secret.env\" on the remote so it no longer holds that content, then lower again",
        "{}", error
    );
    assert!(!warehouse.root.join("secret.env").exists(), "the content never reached the bay");
    assert_eq!(warehouse.head("main"), head, "nothing was lowered");

    warehouse.stack("mine.txt", b"mine\n", "mine");
    let head = warehouse.head("main");

    let error = content_redacted(&warehouse, &["lower", "main"]);
    assert_eq!(error["details"], serde_json::json!({ "path": "secret.env", "hash": hash }), "{}", error);
    let message = error["message"].as_str().unwrap();
    assert!(message.starts_with("Refusing to merge the remote into \"main\": \"secret.env\""), "{}", error);
    assert_eq!(
        error["next_step"],
        "change or delete \"secret.env\" on the remote so it no longer holds that content, then run this again",
        "{}", error
    );
    assert!(!warehouse.root.join("secret.env").exists(), "the content never reached the bay");
    assert_eq!(warehouse.head("main"), head, "no merge was written");

    // The next step, as it says.
    other.run_ok(&["remove", "secret.env"]);
    other.run_ok(&["stack", "-m", "take the leak out"]);
    other.run_ok(&["lift"]);
    warehouse.run_ok(&["lower", "main"]);
    assert!(!warehouse.root.join("secret.env").exists());
    assert!(warehouse.root.join("mine.txt").exists());
}

#[test]
fn a_fast_forward_to_a_head_stacked_before_a_redaction_came_down_is_refused() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-ff-server-{}", std::process::id())));

    let first = Warehouse::cloned("ff-first", &server.url);
    first.stack("notes.txt", b"notes\n", "notes");
    first.run_ok(&["lift"]);

    // Side is stacked ahead of main before any redaction exists; another warehouse's redaction
    // then comes down by lower.
    let warehouse = Warehouse::cloned("ff", &server.url);
    warehouse.run_ok(&["pallet", "create", "side"]);
    warehouse.stack("secret.env", b"TOKEN=hunter2\n", "leak");
    warehouse.run_ok(&["shift", "main"]);

    let other = Warehouse::cloned("ff-other", &server.url);
    let hash = other.redact_loaded("secret.env", b"TOKEN=hunter2\n", "leaked token");
    other.run_ok(&["lift"]);
    warehouse.run_ok(&["lower", "main"]);
    let head = warehouse.head("main");

    let error = content_redacted(&warehouse, &["consolidate", "side"]);
    assert_eq!(error["details"], serde_json::json!({ "path": "secret.env", "hash": hash }), "{}", error);
    let message = error["message"].as_str().unwrap();
    assert!(message.starts_with("Refusing to consolidate \"side\" into \"main\": \"secret.env\""), "{}", error);
    assert_eq!(
        error["next_step"],
        "shift to \"side\", take \"secret.env\" out of it and stack, then shift back to \"main\" and consolidate again",
        "{}", error
    );
    assert!(!warehouse.root.join("secret.env").exists(), "the content never reached the bay");
    assert_eq!(warehouse.head("main"), head);

    // The next step, as it says.
    warehouse.run_ok(&["shift", "side"]);
    warehouse.run_ok(&["remove", "secret.env"]);
    forklift_core::util::file_utils::remove_file(warehouse.root.join("secret.env")).unwrap();
    warehouse.run_ok(&["stack", "-m", "take the leak out"]);
    warehouse.run_ok(&["shift", "main"]);
    warehouse.run_ok(&["consolidate", "side"]);
}

#[test]
fn a_lift_refused_for_a_pallet_the_remote_lacks_says_it_was_not_created() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-lift-new-server-{}", std::process::id())));

    let warehouse = Warehouse::cloned("lift-new", &server.url);
    warehouse.run_ok(&["pallet", "create", "fresh"]);
    warehouse.stack("secret.env", b"TOKEN=hunter2\n", "leak");

    let other = Warehouse::cloned("lift-new-other", &server.url);
    other.stack("notes.txt", b"notes\n", "notes");
    other.redact_loaded("secret.env", b"TOKEN=hunter2\n", "leaked token");
    other.run_ok(&["lift"]);

    // The remote lacks "fresh", so the redaction comes down with main.
    warehouse.run_ok(&["lower", "main"]);

    let error = content_redacted(&warehouse, &["lift"]);
    assert_eq!(error["details"]["pallet"], "fresh", "{}", error);
    let message = error["message"].as_str().unwrap();
    assert!(message.ends_with("Pallet \"fresh\" was not created on the remote."), "{}", error);
}

#[test]
fn a_meta_history_the_remote_serves_is_taken_though_it_names_a_redacted_hash() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-served-meta-server-{}", std::process::id())));

    let other = Warehouse::cloned("served-meta-other", &server.url);
    other.stack("notes.txt", b"notes\n", "notes");
    other.run_ok(&["lift"]);

    // Cloned before the remote has any @tags, so no meta history here names the marker and the
    // redaction is recorded. The marker is only loaded, so no pallet head names it either.
    let warehouse = Warehouse::cloned("served-meta", &server.url);

    // Every @tags parcel's tree names the marker blob, so the remote's @tags names it from here on.
    other.run_ok(&["tag", "create", "v1", "main"]);
    other.run_ok(&["lift"]);

    let marker = b"forklift-meta 1 tags";
    std::fs::write(warehouse.root.join("marker.txt"), marker).unwrap();
    warehouse.run_ok(&["load", "marker.txt"]);
    recorded_hash(&warehouse, &hash_of(marker), "r");
    warehouse.run_ok(&["unload", "marker.txt"]);
    forklift_core::util::file_utils::remove_file(warehouse.root.join("marker.txt")).unwrap();
    warehouse.run_ok(&["lift"]);

    // A fresh clone takes @redactions and then the remote's @tags, which the remote judged when it
    // took each parcel: the redaction is pending there, held by that history.
    let fresh = Warehouse::cloned("served-meta-fresh", &server.url);
    assert_eq!(fresh.redactions().len(), 1);
    let tags = fresh.run_ok(&["tag", "list"]);
    assert!(String::from_utf8_lossy(&tags.stdout).contains("v1"), "{:?}", tags);

    // So does a lower's fast-forward from an unborn @tags.
    warehouse.run_ok(&["lower"]);
    let tags = warehouse.run_ok(&["tag", "list"]);
    assert!(String::from_utf8_lossy(&tags.stdout).contains("v1"), "{:?}", tags);

    // A parcel past the served head only carries the marker its parent names, so it is written.
    warehouse.run_ok(&["tag", "create", "v2", "main"]);
}

#[test]
fn a_head_the_remote_serves_is_adopted_while_the_remote_holds_its_redaction_pending() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-adopt-server-{}", std::process::id())));

    // One warehouse lifts a pallet, which another clones, then a head of it naming the content.
    let owner = Warehouse::cloned("adopt-owner", &server.url);
    owner.stack("notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);
    owner.run_ok(&["pallet", "create", "side"]);
    owner.stack("side.txt", b"side\n", "side");
    owner.run_ok(&["lift"]);
    let behind = Warehouse::cloned_with("adopt-behind", &server.url, &["--pallet", "side"]);
    owner.stack("secret.env", b"TOKEN=hunter2\n", "leak");
    owner.run_ok(&["lift"]);

    // A third, which never held that pallet, records the redaction and lifts it. The remote still
    // serves the pallet: the redaction is pending there until its owner takes the content out.
    let redactor = Warehouse::cloned("adopt-redactor", &server.url);
    let hash = redactor.redact_loaded("secret.env", b"TOKEN=hunter2\n", "leaked token");
    redactor.run_ok(&["lift"]);

    // A clone takes the pallet as the remote serves it, and the redaction with it.
    let fresh = Warehouse::cloned_with("adopt-fresh", &server.url, &["--pallet", "side"]);
    assert_eq!(fresh.redactions().len(), 1);
    assert!(fresh.root.join("secret.env").exists());

    // So does a named lower of it.
    redactor.run_ok(&["lower", "side"]);
    assert_eq!(redactor.head("side"), owner.head("side"));

    // And a lower's fast-forward of the working pallet to the head the remote serves.
    behind.run_ok(&["lower"]);
    assert_eq!(behind.head("side"), owner.head("side"));
    assert_eq!(behind.redactions().len(), 1);
    assert!(behind.root.join("secret.env").exists());

    // Work of its own is no adoption: it is checked in full.
    std::fs::write(fresh.root.join("mine.txt"), b"mine\n").unwrap();
    let error = content_redacted(&fresh, &["load", "."]);
    assert_eq!(error["details"], serde_json::json!({ "path": "secret.env", "hash": hash }), "{}", error);
}

/// The record of applied redactions in `warehouse`, as its raw text; empty when there is none.
fn applied_record(warehouse: &Warehouse) -> String {
    std::fs::read_to_string(warehouse.root.join(".forklift/redactions-applied")).unwrap_or_default()
}

/// In a warehouse with no remote, a redaction that nothing names applies as it is recorded, and is
/// listed applied from then on.
///
/// Revert: judge a clean scan as pending with no remote → the state is not `applied` and the
/// record never names the hash.
#[test]
fn a_redaction_nothing_names_applies_as_it_is_recorded_without_a_remote() {
    let warehouse = Warehouse::new("apply-local");
    let leak = warehouse.leaked("secret.env", b"TOKEN=hunter2\n");

    let recorded = recorded_hash(&warehouse, &format!("{}:secret.env", leak), "leaked token");
    let hash = recorded["data"]["hash"].as_str().unwrap().to_string();
    assert_eq!(recorded["data"]["status"], serde_json::json!({ "state": "applied" }), "{}", recorded);
    assert_eq!(applied_record(&warehouse), format!("/forklift-redactions-applied 1\n{}\n", hash));

    let listed = warehouse.redactions();
    assert_eq!(listed[0]["status"], serde_json::json!({ "state": "applied" }), "{:?}", listed);

    let human = warehouse.run_ok(&["redact", "list"]);
    assert!(String::from_utf8_lossy(&human.stdout).contains("    applied"), "{:?}", human);

    // A re-scan leaves the record as it was.
    let applied = warehouse.json(&["redact", "apply"]);
    assert_eq!(applied["data"]["redactions"], serde_json::Value::Array(listed), "{}", applied);
    assert_eq!(applied_record(&warehouse), format!("/forklift-redactions-applied 1\n{}\n", hash));
}

/// A warehouse with a remote applies nothing by its own scan: a redaction nothing here names waits
/// for the remote. One a pallet head names is held by it, with or without a remote; once the remote
/// is gone and the file taken out, `redact apply` applies it.
///
/// Revert: read no remote when judging → the redactor's redaction applies at once. Revert: drop the
/// head scan → the holder's redaction applies while its head still names the content.
#[test]
fn a_remote_holds_a_redaction_pending_and_a_head_holds_it_until_the_file_is_out() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-apply-server-{}", std::process::id())));

    let owner = Warehouse::cloned("apply-owner", &server.url);
    owner.stack("notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);

    // Cloned before the leak, so no head here names it.
    let redactor = Warehouse::cloned("apply-redactor", &server.url);
    owner.stack("secret.env", b"TOKEN=hunter2\n", "leak");
    owner.run_ok(&["lift"]);

    std::fs::write(redactor.root.join("secret.env"), b"TOKEN=hunter2\n").unwrap();
    redactor.run_ok(&["load", "secret.env"]);
    let hash = hash_of(b"TOKEN=hunter2\n");
    let recorded = recorded_hash(&redactor, &hash, "leaked token");
    assert_eq!(recorded["data"]["status"], serde_json::json!({ "state": "awaiting_remote" }), "{}", recorded);
    redactor.run_ok(&["unload", "secret.env"]);
    forklift_core::util::file_utils::remove_file(redactor.root.join("secret.env")).unwrap();

    // Its main is behind the remote's, which it leaves alone; @redactions goes up.
    let lifted = redactor.run(&["lift"]);
    assert!(String::from_utf8_lossy(&lifted.stdout).contains("Lifted the @redactions pallet"), "{:?}", lifted);

    let listed = redactor.json(&["redact", "apply"]);
    assert_eq!(listed["data"]["redactions"][0]["status"]["state"], "awaiting_remote", "{}", listed);
    assert_eq!(applied_record(&redactor), "");

    // A clone takes the head that names the content, and the redaction with it.
    let holder = Warehouse::cloned("apply-holder", &server.url);
    let held = serde_json::json!({
        "state": "held",
        "heads": [{ "pallet": "main", "head": holder.head("main"), "path": "secret.env" }],
    });
    assert_eq!(holder.redactions()[0]["status"], held);

    // A meta pallet whose history is damaged cannot be judged, but the head that names the content
    // is still reported ahead of that error.
    holder.run_ok(&["tag", "create", "v1", "main"]);
    let tags = std::fs::read_to_string(holder.root.join(".forklift/meta/tags")).unwrap().trim().to_string();
    let loose = holder.root.join(".forklift/objects").join(&tags[..2]).join(&tags[2..]);
    let parcel = std::fs::read(&loose).unwrap();
    forklift_core::util::file_utils::remove_file(&loose).unwrap();
    assert_eq!(holder.redactions()[0]["status"], held);
    std::fs::write(&loose, parcel).unwrap();

    holder.run_ok(&["config", "unset", "remote.url"]);
    let applied = holder.json(&["redact", "apply"]);
    assert_eq!(applied["data"]["redactions"][0]["status"]["state"], "held", "{}", applied);
    assert_eq!(applied_record(&holder), "");

    let human = holder.run_ok(&["redact", "list"]);
    assert!(String::from_utf8_lossy(&human.stdout).contains("pending: named by pallet \"main\" as \"secret.env\""),
            "{:?}", human);

    holder.run_ok(&["remove", "secret.env"]);
    holder.run_ok(&["stack", "-m", "take the leak out"]);

    // Listing applies nothing: it says the redaction is ready.
    assert_eq!(holder.redactions()[0]["status"], serde_json::json!({ "state": "ready" }));
    assert_eq!(applied_record(&holder), "");

    let applied = holder.json(&["redact", "apply"]);
    assert_eq!(applied["data"]["redactions"][0]["status"]["state"], "applied", "{}", applied);
    assert_eq!(applied_record(&holder), format!("/forklift-redactions-applied 1\n{}\n", hash));
}

/// A record of applied redactions this build cannot read fails every command that reads it, and is
/// never written over. A redaction recorded meanwhile is still recorded, and reported undecided.
///
/// Revert: read an unknown version as nothing applied → `redact list` succeeds.
#[test]
fn an_applied_record_this_build_cannot_read_is_refused_loudly() {
    let warehouse = Warehouse::new("apply-unknown");
    let leak = warehouse.leaked("secret.env", b"TOKEN=hunter2\n");
    let record = warehouse.root.join(".forklift/redactions-applied");
    std::fs::write(&record, b"/forklift-redactions-applied 2\n").unwrap();

    let recorded = recorded_hash(&warehouse, &format!("{}:secret.env", leak), "leaked token");
    assert_eq!(recorded["data"]["status"]["state"], "undecided", "{}", recorded);
    assert!(recorded["data"]["status"]["reason"].as_str().unwrap().contains("does not know"), "{}", recorded);

    for args in [["redact", "list"], ["redact", "apply"]] {
        let refused = warehouse.run(&args);
        assert!(!refused.status.success(), "{:?}", args);
        assert!(String::from_utf8_lossy(&refused.stderr).contains("does not know"), "{:?}", refused);
    }

    assert_eq!(std::fs::read(&record).unwrap(), b"/forklift-redactions-applied 2\n");
}

/// Record the redaction of `content`, loaded as `file`, which nothing in `warehouse` names, and
/// lift it, so the remote holds it pending and an adoption of what the remote serves is exempt from
/// it. Then unset the remote, so `redact apply` applies it by this warehouse's own scan, and set
/// the remote back to `url`.
fn applied_without_remote(warehouse: &Warehouse, url: &str, file: &str, content: &[u8]) -> String {
    let hash = warehouse.redact_loaded(file, content, "applied without the remote");
    let lifted = warehouse.run(&["lift"]);
    assert!(String::from_utf8_lossy(&lifted.stdout).contains("Lifted the @redactions pallet"), "{:?}", lifted);

    apply_without_remote(warehouse, url);

    hash
}

/// Apply every redaction `warehouse` holds by its own scan, with its remote unset, then set the
/// remote back to `url`.
fn apply_without_remote(warehouse: &Warehouse, url: &str) {
    warehouse.run_ok(&["config", "unset", "remote.url"]);
    let applied = warehouse.json(&["redact", "apply"]);
    assert_eq!(applied["data"]["redactions"][0]["status"], serde_json::json!({ "state": "applied" }), "{}", applied);
    warehouse.run_ok(&["config", "set", "remote.url", url]);
}

/// A warehouse that applied a redaction while it had no remote never takes a head naming that
/// content again, not even one the remote it has since gained serves: a named lower and a lower's
/// fast-forward are refused, naming a fresh clone as the way out, and a fresh clone takes the head.
///
/// Revert: let the adoption exemption cover an applied hash → both lowers adopt the head.
#[test]
fn a_head_naming_a_hash_applied_here_is_never_adopted_from_the_remote() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-applied-adopt-server-{}", std::process::id())));

    let owner = Warehouse::cloned("applied-adopt-owner", &server.url);
    owner.stack("notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);
    owner.run_ok(&["pallet", "create", "side"]);
    owner.stack("side.txt", b"side\n", "side");
    owner.run_ok(&["lift"]);

    // Cloned before the leak, so neither names it.
    let behind = Warehouse::cloned_with("applied-adopt-behind", &server.url, &["--pallet", "side"]);
    let named = Warehouse::cloned("applied-adopt-named", &server.url);
    owner.stack("secret.env", b"TOKEN=hunter2\n", "leak");
    owner.run_ok(&["lift"]);

    let fresh_clone = "this warehouse has applied that redaction, so it can never take history naming the \
        content again; make a fresh clone of the remote with \"forklift clone\", which takes the remote's \
        history and redactions as the remote holds them, and work there";

    let hash = applied_without_remote(&named, &server.url, "secret.env", b"TOKEN=hunter2\n");
    let error = content_redacted(&named, &["lower", "side"]);
    assert_eq!(error["details"]["hash"], hash, "{}", error);
    assert_eq!(error["next_step"], fresh_clone, "{}", error);
    assert!(error["message"].as_str().unwrap().contains("not even one the remote serves"), "{}", error);
    assert!(!named.root.join(".forklift/pallets/side").exists());

    // This one takes the remote's @redactions as it is with a lower of main, which never named
    // the content, so the remote's pending set is its own.
    let before = behind.head("side");
    behind.run_ok(&["lower", "main"]);
    apply_without_remote(&behind, &server.url);
    let error = content_redacted(&behind, &["lower"]);
    assert_eq!(error["details"], serde_json::json!({ "path": "secret.env", "hash": hash }), "{}", error);
    assert_eq!(error["next_step"], fresh_clone, "{}", error);
    assert_eq!(behind.head("side"), before);
    assert!(!behind.root.join("secret.env").exists(), "the content never reached the bay");

    // The way out, as it says.
    let fresh = Warehouse::cloned_with("applied-adopt-fresh", &server.url, &["--pallet", "side"]);
    assert!(fresh.root.join("secret.env").exists());
}

/// The meta chain a remote serves is no exemption for a hash this warehouse applied: a lower that
/// would take a meta history naming it is refused, naming a fresh clone. Lower reports a meta
/// pallet's failure inside its own error, which carries no code.
///
/// Revert: judge the served chain by nothing → the lower takes @tags, whose history names the
/// applied marker blob.
#[test]
fn a_meta_history_naming_a_hash_applied_here_is_never_taken_from_the_remote() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-applied-meta-server-{}", std::process::id())));

    let other = Warehouse::cloned("applied-meta-other", &server.url);
    other.stack("notes.txt", b"notes\n", "notes");
    other.run_ok(&["lift"]);

    // Cloned before the remote has any @tags, so no meta history here names the marker.
    let warehouse = Warehouse::cloned("applied-meta", &server.url);
    other.run_ok(&["tag", "create", "v1", "main"]);
    other.run_ok(&["lift"]);

    let marker = b"forklift-meta 1 tags";
    let hash = applied_without_remote(&warehouse, &server.url, "marker.txt", marker);

    let lowered = warehouse.json(&["lower"]);
    let message = lowered["error"]["message"].as_str().unwrap();
    assert!(message.contains("@tags: Refusing to set pallet \"@tags\""), "{}", lowered);
    assert!(message.contains(&format!("({}) is redacted in this warehouse", hash)), "{}", lowered);
    assert!(message.contains("make a fresh clone of the remote"), "{}", lowered);
    assert!(!warehouse.root.join(".forklift/meta/tags").exists(), "@tags was not taken");
}

/// A redaction applied here that the remote does not hold is no exemption the remote's adoption
/// would have given, so the refusal is the ordinary one: a fresh clone would take the content with
/// no redaction at all.
///
/// Revert: report a refused adoption applied whenever the hash is applied here → the next step
/// names a fresh clone.
#[test]
fn a_hash_applied_here_that_the_remote_never_redacted_names_no_fresh_clone() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-applied-unshared-server-{}", std::process::id())));

    let owner = Warehouse::cloned("applied-unshared-owner", &server.url);
    owner.stack("notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);
    owner.run_ok(&["pallet", "create", "side"]);
    owner.stack("side.txt", b"side\n", "side");
    owner.run_ok(&["lift"]);

    // The remote holds another redaction, which the warehouse takes, so the remote's @redactions
    // head is one its own reaches and the lower below is an adoption.
    let other = Warehouse::cloned("applied-unshared-other", &server.url);
    other.redact_loaded("other.env", b"OTHER=1\n", "another leak");
    other.run_ok(&["lift"]);
    let warehouse = Warehouse::cloned("applied-unshared", &server.url);
    owner.stack("secret.env", b"TOKEN=hunter2\n", "leak");
    owner.run_ok(&["lift"]);

    // Recorded and applied with no remote, and never lifted.
    warehouse.run_ok(&["config", "unset", "remote.url"]);
    let hash = warehouse.redact_loaded("secret.env", b"TOKEN=hunter2\n", "applied here only");
    let status = warehouse.redactions().into_iter().find(|listed| listed["hash"] == hash.as_str()).unwrap();
    assert_eq!(status["status"], serde_json::json!({ "state": "applied" }), "{}", status);
    warehouse.run_ok(&["config", "set", "remote.url", &server.url]);

    let error = content_redacted(&warehouse, &["lower", "side"]);
    assert_eq!(error["details"]["hash"], hash, "{}", error);
    assert!(!error["next_step"].as_str().unwrap().contains("fresh clone"), "{}", error);
    assert!(!error["message"].as_str().unwrap().contains("not even one the remote serves"), "{}", error);
}

/// Every file under `dir` named as the loose object `hash` is stored.
fn loose_copies(dir: &std::path::Path, hash: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(loose_copies(&path, hash));
        } else if path.file_name().unwrap() == &hash[2..]
            && path.parent().unwrap().file_name().unwrap() == &hash[..2] {
            found.push(path);
        }
    }
    found
}

/// A cache that applied a redaction holds the content's absence as redacted, not as held by the
/// remote. A read that needs the content fetches nothing, though the remote still serves the bytes.
/// A move to a server whose history never reaches content only this cache's own history names goes
/// through, never asking that server for it. Until a remote applies redactions, a cache never
/// reaches the applied state itself, so the test writes the record.
///
/// Revert: let the miss hook fetch an applied hash → the read fetches the content back; leave the
/// walk's redacted absences among the ones the move leans on the new server for → the move refuses
/// as `origin_move_incomplete`.
#[test]
fn a_cache_that_applied_a_redaction_never_fetches_it_nor_asks_a_new_origin_for_it() {
    let temp = std::env::temp_dir();
    let first_root = temp.join(format!("forklift-redaction-applied-cache-server-{}", std::process::id()));
    let server = Server::start(&first_root);

    let owner = Warehouse::cloned("applied-cache-owner", &server.url);
    // A draft the cache never fetches, so the move leans on the new server for something.
    owner.stack("notes.txt", b"draft\n", "draft");
    owner.stack("notes.txt", b"notes\n", "notes");
    let leak = owner.leaked("secret.env", b"TOKEN=hunter2\n");
    owner.run_ok(&["lift"]);

    let cached = Warehouse::cloned_with("applied-cache", &server.url, &["--cache"]);
    // Read once and evicted, so the sealed record holds the content before the redaction applies.
    cached.run_ok(&["show", &format!("{}:secret.env", leak)]);
    cached.run_ok(&["store", "evict"]);
    // A leak in history only this cache holds, never lifted.
    cached.run_ok(&["pallet", "create", "local"]);
    cached.stack("local.env", b"LOCAL=1\n", "local leak");
    let local_leak = cached.head("local");
    cached.run_ok(&["remove", "local.env"]);
    cached.run_ok(&["stack", "-m", "take the local leak out"]);

    let hashes: Vec<String> = [format!("{}:secret.env", leak), format!("{}:local.env", local_leak)].iter()
        .map(|target| recorded_hash(&cached, target, "leaked token")["data"]["hash"].as_str().unwrap().to_string())
        .collect();
    // A cache applies a redaction only once its remote has, which nothing does yet: the record is
    // written as the apply would write it.
    let mut sorted = hashes.clone();
    sorted.sort();
    std::fs::write(cached.root.join(".forklift/redactions-applied"),
                   format!("/forklift-redactions-applied 1\n{}\n", sorted.join("\n"))).unwrap();
    assert!(cached.redactions().iter().all(|listed| listed["status"] == serde_json::json!({ "state": "applied" })));
    // The deletion that follows an applied redaction, whether or not the clone fetched the bytes.
    for hash in &hashes {
        for copy in loose_copies(&cached.root.join(".forklift/objects"), hash) {
            forklift_core::util::file_utils::remove_file(copy).unwrap();
        }
    }

    let read = cached.run(&["show", &format!("{}:secret.env", leak)]);
    assert!(!read.status.success(), "{:?}", read);
    assert!(loose_copies(&cached.root.join(".forklift/objects"), &hashes[0]).is_empty(), "nothing fetched it back");
    assert!(!loose_copies(&first_root, &hashes[0]).is_empty(), "the remote still serves the bytes");

    // A copy of the server's root, served once the first has stopped. Its history never reaches
    // the local leak.
    drop(server);
    let second_root = temp.join(format!("forklift-redaction-applied-cache-copy-{}", std::process::id()));
    let _ = forklift_core::util::file_utils::remove_dir_all(&second_root);
    let copied = std::process::Command::new("cp").arg("-R").arg(&first_root).arg(&second_root).output().unwrap();
    assert!(copied.status.success(), "{:?}", copied);
    // The lock the first server held is not the copy's: no process holds it.
    forklift_core::util::file_utils::remove_file(second_root.join(".forklift/serve.lock")).unwrap();
    let server = Server::serve(&second_root);

    let moved = cached.json(&["remote", "move", &server.url]);
    assert_eq!(moved["ok"], true, "{}", moved);
    assert_eq!(moved["data"]["origin"], server.url.as_str(), "{}", moved);
    assert!(moved["data"]["absent_objects"].as_u64().unwrap() > 0, "the move checked the new server: {}", moved);
}

/// While another process uses the object store, a redaction's deletion is owed: the record and
/// the apply both succeed, keep the copy and say why, and `redact apply` deletes it once the
/// store is free.
///
/// Revert: fail the command on the exclusive try's refusal → `redact apply` exits non-zero.
/// Revert: hold record's shared store access through the deletion → a record nothing else holds
/// owes it too (`a_read_of_content_whose_redaction_is_applied_is_refused_as_redacted`).
#[test]
fn a_deletion_is_owed_while_another_process_uses_the_store() {
    let warehouse = Warehouse::new("owed");
    let leak = warehouse.leaked("secret.env", b"TOKEN=hunter2\n");
    let hash = hash_of(b"TOKEN=hunter2\n");

    {
        let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);
        let _store_access = forklift_core::util::lock_utils::StoreAccessLock::try_shared().unwrap();

        let recorded = recorded_hash(&warehouse, &format!("{}:secret.env", leak), "leaked token");
        assert_eq!(recorded["data"]["status"], serde_json::json!({ "state": "applied" }), "{}", recorded);
        let owed = recorded["data"]["deletion"]["owed"].as_str().unwrap();
        assert!(owed.starts_with("Another forklift process is using the object store"), "{}", recorded);
        assert!(warehouse.holds_loose(&hash));

        let human = String::from_utf8_lossy(&warehouse.run_ok(&["redact", "apply"]).stdout).into_owned();
        assert!(human.contains("The redacted content was not deleted yet. Another forklift process is using"), "{}", human);
        assert!(human.contains("Then run \"forklift redact apply\" again to delete it."), "{}", human);
        assert!(warehouse.holds_loose(&hash));
    }

    let applied = warehouse.json(&["redact", "apply"]);
    assert_eq!(applied["data"]["deletion"]["deleted"], serde_json::json!([hash]), "{}", applied);
    assert_eq!(applied["data"]["deletion"].get("owed"), None, "{}", applied);
    assert!(!warehouse.holds_loose(&hash));

    // Nothing is left to delete, so the next apply reports no deletion.
    assert_eq!(warehouse.json(&["redact", "apply"])["data"].get("deletion"), None);
}

/// The chunks a deletion cut short still owes are deleted by the next `redact apply`, though
/// every applied redaction's own object is gone.
///
/// Revert: decide there is nothing to delete by the redacted objects alone → the chunk is kept.
#[test]
fn a_chunk_a_deletion_cut_short_owes_is_deleted_by_the_next_apply() {
    let warehouse = Warehouse::new("owed-chunk");

    // Above the chunking threshold, and varied, so the file is a recipe of several chunks.
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let content: Vec<u8> = (0..9 * 1024 * 1024).map(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as u8
    }).collect();
    let leak = warehouse.leaked("big.bin", &content);

    let recorded = recorded_hash(&warehouse, &format!("{}:big.bin", leak), "leaked dump");
    let recipe = recorded["data"]["hash"].as_str().unwrap().to_string();
    assert!(!warehouse.holds_loose(&recipe), "the deletion ran");

    // As a deletion cut short between its passes leaves it: the recipe gone, a chunk of it left
    // and owed in the journal.
    let chunk = hash_of(b"an owed chunk\n");
    let loose = warehouse.root.join(".forklift/objects").join(&chunk[..2]).join(&chunk[2..]);
    std::fs::create_dir_all(loose.parent().unwrap()).unwrap();
    std::fs::write(&loose, b"bytes the store holds").unwrap();
    std::fs::write(
        warehouse.root.join(".forklift/redactions-chunks-owed"),
        forklift_core::util::record_utils::to_bytes("redactions-chunks-owed", 1, [format!("{} {}", recipe, chunk)]),
    ).unwrap();

    let applied = warehouse.json(&["redact", "apply"]);
    assert_eq!(applied["data"]["deletion"]["chunks_deleted"], 1, "{}", applied);
    assert!(!loose.exists(), "the owed chunk is deleted");
    assert!(!warehouse.root.join(".forklift/redactions-chunks-owed").exists());
}

/// When every copy left is held, the deletion has nothing to do and takes no lock: the held copy is
/// reported, not an owed deletion, even while another process uses the store.
///
/// Revert: take the locks whatever the first census found → the deletion is owed, as the store is
/// in use.
#[test]
fn a_held_copy_is_reported_without_the_locks_a_deletion_needs() {
    let warehouse = Warehouse::new("held-unlocked");
    let leak = warehouse.leaked("secret.env", b"TOKEN=hunter2\n");
    warehouse.hold_parked(b"TOKEN=hunter2\n");

    let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);
    let _store_access = forklift_core::util::lock_utils::StoreAccessLock::try_shared().unwrap();

    let recorded = recorded_hash(&warehouse, &format!("{}:secret.env", leak), "leaked token");
    let deletion = &recorded["data"]["deletion"];
    assert_eq!(deletion.get("owed"), None, "{}", recorded);
    assert_eq!(deletion["kept"][0]["hash"], recorded["data"]["hash"], "{}", recorded);
}

/// Under trust, a redaction by an operator the office does not admit is refused as such, before
/// anything about the target is told: even for a hash that is already redacted.
///
/// Revert: check for an existing redaction before the operator's checks → the refusal says the
/// hash is already redacted.
#[test]
fn a_non_admin_is_refused_before_the_target_is_looked_at() {
    let warehouse = Warehouse::new("non-admin");
    warehouse.run_ok(&["office", "enroll"]);
    let leak = warehouse.leaked("secret.env", b"TOKEN=hunter2\n");
    let hash = recorded_hash(&warehouse, &format!("{}:secret.env", leak), "leaked token")["data"]["hash"]
        .as_str().unwrap().to_string();

    warehouse.run_ok(&["config", "set", "operator.identifier", "stranger@forklift"]);
    let refused = warehouse.run(&["redact", "record", &hash, "-m", "again"]);
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("\"stranger@forklift\" is not enrolled"), "{:?}", refused);
    assert!(!stderr.contains("already redacted"), "{:?}", refused);
}

/// Send a raw HTTP request with `body` and return `(status line, response body)`.
fn http(url: &str, method: &str, path: &str, body: &[u8]) -> (String, String) {
    use std::io::{Read, Write};

    let address = url.strip_prefix("http://").unwrap();
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    write!(
        stream,
        "{} {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
        Connection: close\r\n\r\n",
        method, path, address, body.len()
    ).unwrap();
    stream.write_all(body).unwrap();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let response = String::from_utf8_lossy(&response).into_owned();
    let status = response.lines().next().unwrap_or_default().to_string();
    let body = response.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();

    (status, body)
}

/// Once a lift leaves no head on forklift-server naming a recorded redaction, the server applies
/// it: the handshake right after the lift reports it applied (deleted, since the server never held
/// the content), every surface that reads or takes content refuses it as redacted, and a warehouse
/// whose unlifted history still names it lifts that history without it.
///
/// Revert: skip applying after a ref write → the handshake reports the redaction pending (the
/// redaction run applies only what a write could not).
#[test]
fn a_server_applies_a_redaction_no_head_names_and_refuses_its_content() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-served-server-{}", std::process::id())));

    let owner = Warehouse::cloned("served-owner", &server.url);
    owner.stack("notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);

    // Its history names the leak; it has lifted none of it.
    let behind = Warehouse::cloned("served-behind", &server.url);
    behind.leaked("secret.env", b"TOKEN=hunter2\n");

    let hash = owner.redact_loaded("secret.env", b"TOKEN=hunter2\n", "leaked token");
    owner.run_ok(&["lift"]);

    let (status, info) = http(&server.url, "GET", "/v1/warehouse", b"");
    assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, info);
    let info: serde_json::Value = serde_json::from_str(&info).unwrap();
    assert_eq!(info["redactions"], serde_json::json!([{ "hash": hash, "state": "deleted" }]), "{}", info);

    let object = format!("/v1/objects/{}", hash);
    let refusals = [
        ("get", http(&server.url, "GET", &object, b"")),
        ("put", http(&server.url, "PUT", &object, &object_utils::blob_bytes(b"TOKEN=hunter2\n"))),
        ("upload-targets", http(&server.url, "POST", "/v1/objects/upload-targets",
                                format!("{{\"session\":\"s\",\"hashes\":[\"{}\"]}}", hash).as_bytes())),
    ];
    for (surface, (status, body)) in refusals {
        assert!(status.starts_with("HTTP/1.1 410"), "{}: {} / {}", surface, status, body);
        assert!(body.contains("content_redacted"), "{}: {}", surface, body);
    }

    // The redaction comes down, and the history that names it goes up without the content.
    behind.run_ok(&["lower"]);
    behind.run_ok(&["lift"]);
    assert_eq!(server.main(), behind.head("main"));
}

/// Content forklift-server holds is deleted once a redaction of it applies: the server drains the
/// warehouse, deletes the bytes in its own process and serves it again, and the handshake then
/// reports the redaction deleted. Before the apply, the bytes were there.
///
/// Revert: skip the deletion after an apply → the handshake keeps reporting the deletion owed, and
/// the bytes stay.
#[test]
fn a_server_deletes_the_content_of_a_redaction_it_applied() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-deleted-server-{}", std::process::id())));

    let owner = Warehouse::cloned("deleted-owner", &server.url);
    owner.stack("notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);

    // The server holds the bytes, and no head names them.
    let content = b"TOKEN=hunter2\n";
    let hash = hash_of(content);
    let (status, body) = http(&server.url, "PUT", &format!("/v1/objects/{}", hash), &object_utils::blob_bytes(content));
    assert!(status.starts_with("HTTP/1.1 20"), "{} / {}", status, body);
    let loose = server.root.join(".forklift/objects").join(&hash[..2]).join(&hash[2..]);
    assert!(loose.exists(), "the server holds the content");

    owner.redact_loaded("secret.env", content, "leaked token");
    owner.run_ok(&["lift"]);

    let mut state = serde_json::Value::Null;
    for _ in 0..500 {
        let (status, info) = http(&server.url, "GET", "/v1/warehouse", b"");
        if status.starts_with("HTTP/1.1 200") {
            let info: serde_json::Value = serde_json::from_str(&info).unwrap();
            state = info["redactions"].clone();
            if state == serde_json::json!([{ "hash": hash, "state": "deleted" }]) {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(state, serde_json::json!([{ "hash": hash, "state": "deleted" }]));
    assert!(!loose.exists(), "the content is deleted");

    let (status, body) = http(&server.url, "GET", &format!("/v1/objects/{}", hash), b"");
    assert!(status.starts_with("HTTP/1.1 410"), "{} / {}", status, body);

    // Served again after the drain.
    let fresh = Warehouse::cloned("deleted-fresh", &server.url);
    assert_eq!(fresh.head("main"), server.main());
}

/// A server root as a crash before an apply leaves it: a redaction is recorded, the ref write that
/// let it apply landed, and the server stopped before applying it, so its bytes are still held. The
/// killed server's lock stays behind, and is removed as its operator would.
///
/// # Returns
/// The root, the redacted hash, and a kept file's hash.
fn crashed_before_apply(name: &str) -> (PathBuf, String, String) {
    let root = std::env::temp_dir().join(format!("forklift-redaction-{}-server-{}", name, std::process::id()));
    let server = Server::start(&root);

    let owner = Warehouse::cloned(&format!("{}-owner", name), &server.url);
    owner.stack("notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);

    let content = b"TOKEN=hunter2\n";
    let hash = hash_of(content);
    let (status, body) = http(&server.url, "PUT", &format!("/v1/objects/{}", hash), &object_utils::blob_bytes(content));
    assert!(status.starts_with("HTTP/1.1 20"), "{} / {}", status, body);
    let loose = root.join(".forklift/objects").join(&hash[..2]).join(&hash[2..]);
    let bytes = std::fs::read(&loose).unwrap();

    owner.redact_loaded("secret.env", content, "leaked token");
    owner.run_ok(&["lift"]);
    let mut gone = false;
    for _ in 0..500 {
        if !loose.exists() {
            gone = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(gone, "the server deleted the content");
    drop(server);

    forklift_core::util::file_utils::remove_file(root.join(".forklift/serve.lock")).unwrap();
    forklift_core::util::file_utils::remove_file(root.join(".forklift/redactions-applied")).unwrap();
    std::fs::create_dir_all(loose.parent().unwrap()).unwrap();
    std::fs::write(&loose, &bytes).unwrap();

    (root, hash, hash_of(b"notes\n"))
}

/// Ask for `path` until it is answered other than `503`, and return that answer.
fn past_503(server: &Server, path: &str) -> (String, String) {
    let mut answered = (String::new(), String::new());
    for _ in 0..1500 {
        answered = http(&server.url, "GET", path, b"");
        if !answered.0.starts_with("HTTP/1.1 503") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    answered
}

/// A redaction a crash left unapplied, after the ref write that let it apply landed, is never
/// served once the server is back: until its first redaction run applies it, the content routes
/// answer `503`, and then they refuse it as redacted. Once every redaction is applied, a server
/// coming back serves content at once, though the ref lock is held: its first run's apply settles
/// that without it.
///
/// Revert: let an object request through without waiting for the owed apply → the restored bytes
/// are served `200` → red. Take the ref lock before settling that nothing is left to apply → the
/// start's owe waits on the held lock, and the kept file is refused `503` → red.
#[test]
fn a_server_back_from_a_crash_before_an_apply_serves_none_of_its_content() {
    let (root, hash, kept) = crashed_before_apply("crash");
    let object = format!("/v1/objects/{}", hash);

    let held = {
        // The ref lock held here keeps the first run's apply waiting.
        let _scope = forklift_core::globals::StorageRootScope::enter(&root);
        let _lock = forklift_core::util::lock_utils::RefLock::acquire().unwrap();
        let server = Server::serve(&root);
        (http(&server.url, "GET", &object, b""), server)
    };
    let ((status, body), server) = held;
    assert!(status.starts_with("HTTP/1.1 503"), "{} / {} bytes", status, body.len());

    let (status, body) = past_503(&server, &object);
    assert!(status.starts_with("HTTP/1.1 410"), "{} / {}", status, body);
    assert!(body.contains("content_redacted"), "{}", body);
    let loose = root.join(".forklift/objects").join(&hash[..2]).join(&hash[2..]);
    for _ in 0..500 {
        if !loose.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(!loose.exists(), "the server deleted the content");
    drop(server);

    // Back again with nothing left to apply or delete: content is served while the first run is
    // held.
    forklift_core::util::file_utils::remove_file(root.join(".forklift/serve.lock")).unwrap();
    let _scope = forklift_core::globals::StorageRootScope::enter(&root);
    let _lock = forklift_core::util::lock_utils::RefLock::acquire().unwrap();
    let server = Server::serve(&root);
    let (status, body) = http(&server.url, "GET", &format!("/v1/objects/{}", kept), b"");
    assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, body);
}

/// An apply that cannot tell whether a head still names a redaction leaves the apply owed: the
/// content stays unserved until a retry can tell, and is then refused as redacted.
///
/// Revert: count an undecided redaction as decided → the apply is discharged and the restored
/// bytes are served `200` → red.
#[test]
fn a_server_serves_no_content_while_an_apply_cannot_decide_a_redaction() {
    let (root, hash, _) = crashed_before_apply("undecided");
    let object = format!("/v1/objects/{}", hash);

    // The head's parcel is gone, so no scan can tell what the head names.
    let head = std::fs::read_to_string(root.join(".forklift/pallets/main")).unwrap().trim().to_string();
    let parcel = root.join(".forklift/objects").join(&head[..2]).join(&head[2..]);
    let parcel_bytes = std::fs::read(&parcel).unwrap();
    forklift_core::util::file_utils::remove_file(&parcel).unwrap();

    let server = Server::serve(&root);
    let (status, body) = http(&server.url, "GET", &object, b"");
    assert!(status.starts_with("HTTP/1.1 503"), "{} / {} bytes", status, body.len());

    std::fs::write(&parcel, &parcel_bytes).unwrap();
    let (status, body) = past_503(&server, &object);
    assert!(status.starts_with("HTTP/1.1 410"), "{} / {}", status, body);
}

/// A pallet deleted on forklift-server leaves a tombstone that keeps its head served and
/// restorable, so a redaction that head names stays pending through the deletion.
///
/// Revert: leave tombstones out of the apply scan → the handshake reports it applied.
#[test]
fn a_server_keeps_a_redaction_pending_while_a_tombstone_names_it() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-tombstone-server-{}", std::process::id())));

    let owner = Warehouse::cloned("tombstone-owner", &server.url);
    owner.stack("notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);

    // Cloned before the leak, so no head here names it.
    let redactor = Warehouse::cloned("tombstone-redactor", &server.url);
    owner.run_ok(&["pallet", "create", "feature"]);
    owner.stack("secret.env", b"TOKEN=hunter2\n", "leak");
    owner.run_ok(&["lift"]);

    let hash = redactor.redact_loaded("secret.env", b"TOKEN=hunter2\n", "leaked token");
    redactor.run_ok(&["lift"]);
    let pending = serde_json::json!([{ "hash": hash, "state": "pending" }]);
    let report = || {
        let (status, info) = http(&server.url, "GET", "/v1/warehouse", b"");
        assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, info);
        serde_json::from_str::<serde_json::Value>(&info).unwrap()["redactions"].clone()
    };
    assert_eq!(report(), pending, "the head of pallet feature names it");

    owner.run_ok(&["shift", "main"]);
    owner.run_ok(&["pallet", "delete", "feature", "--remote"]);
    assert_eq!(report(), pending, "the tombstone of pallet feature keeps that head");
    let (status, _) = http(&server.url, "GET", &format!("/v1/objects/{}", hash), b"");
    assert!(status.starts_with("HTTP/1.1 200"), "{}", status);
}

/// The bytes of every object file under `dir`.
fn stored_bytes(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir).unwrap().map(|entry| {
        let entry = entry.unwrap();
        match entry.file_type().unwrap().is_dir() {
            true => stored_bytes(&entry.path()),
            false => entry.metadata().unwrap().len(),
        }
    }).sum()
}

/// A lift of history naming a chunked file whose redaction the server applied sends neither the
/// recipe nor the chunks only it names.
///
/// Revert: offer a redacted recipe's chunks → the server's store grows by the file.
#[test]
fn a_lift_sends_no_chunk_of_a_file_the_server_applied_a_redaction_of() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-chunks-server-{}", std::process::id())));

    let owner = Warehouse::cloned("chunks-owner", &server.url);
    owner.stack("notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);
    let behind = Warehouse::cloned("chunks-behind", &server.url);

    // Above the chunking threshold, and varied, so the file is a recipe of several chunks.
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let content: Vec<u8> = (0..9 * 1024 * 1024).map(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as u8
    }).collect();

    // The owner records the redaction from a pallet it never lifts.
    owner.run_ok(&["pallet", "create", "scratch"]);
    owner.stack("big.bin", &content, "leak");
    let leak = owner.head("scratch");
    owner.run_ok(&["remove", "big.bin"]);
    owner.run_ok(&["stack", "-m", "take the leak out"]);
    recorded_hash(&owner, &format!("{}:big.bin", leak), "leaked dump");
    owner.run_ok(&["shift", "main"]);
    owner.run_ok(&["lift"]);

    behind.leaked("big.bin", &content);
    behind.run_ok(&["lower"]);
    let objects = server.root.join(".forklift/objects");
    let before = stored_bytes(&objects);
    behind.run_ok(&["lift"]);
    assert_eq!(server.main(), behind.head("main"));
    let grown = stored_bytes(&objects) - before;
    assert!(grown < 1024 * 1024, "the server's store grew by {} bytes", grown);
}

/// While forklift-server owes the deletion of a chunked file's redaction, it serves the chunks only
/// that file named as absent, by hash, in a batch, as missing and to upload-targets, and the
/// handshake reports the deletion owed; a chunk another file names is still served. Once the
/// deletion runs, the handshake reports it deleted.
///
/// Revert: serve a redacted recipe's chunks by hash → the GET of one answers 200; answer one
/// present to upload-targets → `direct` is empty.
#[test]
fn a_server_owing_a_deletion_serves_the_chunks_only_its_recipe_names_as_absent() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-withheld-server-{}", std::process::id())));

    let owner = Warehouse::cloned("withheld-owner", &server.url);

    // Above the chunking threshold, and varied, so the file is a recipe of several chunks; the
    // kept file shares its first half, so it names some of those chunks too.
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as u8
    };
    let content: Vec<u8> = (0..9 * 1024 * 1024).map(|_| next()).collect();
    let mut kept = content[..content.len() / 2].to_vec();
    kept.extend((0..5 * 1024 * 1024).map(|_| next()));

    std::fs::create_dir_all(owner.root.join("keep")).unwrap();
    owner.stack("keep/kept.bin", &kept, "kept");
    owner.run_ok(&["lift"]);

    // The server holds the leak, then a head that names it no more.
    owner.run_ok(&["pallet", "create", "scratch"]);
    std::fs::create_dir_all(owner.root.join("dump")).unwrap();
    owner.stack("dump/big.bin", &content, "leak");
    let leak = owner.head("scratch");
    owner.run_ok(&["lift"]);
    owner.run_ok(&["remove", "dump/big.bin"]);
    owner.run_ok(&["stack", "-m", "take the leak out"]);
    let _ = forklift_core::util::file_utils::remove_file(owner.root.join("dump/big.bin"));

    let recorded = recorded_hash(&owner, &format!("{}:dump/big.bin", leak), "leaked dump");
    let recipe = recorded["data"]["hash"].as_str().unwrap().to_string();

    // The leak's chunks, split by whether the kept file names them too.
    let (own, shared): (Vec<String>, Vec<String>) = {
        let _scope = forklift_core::globals::StorageRootScope::enter(&owner.root);
        let root = object_utils::load_parcel(&owner.head("main")).unwrap().tree_hash;
        let kept = match forklift_core::util::tree_utils::collect_subtree_closure(&root, "keep", usize::MAX).unwrap() {
            Some(forklift_core::util::tree_utils::SubtreeClosure::Complete(closure)) => closure,
            other => panic!("{:?}", other.is_some()),
        };
        let kept: std::collections::HashSet<String> = kept.iter()
            .filter_map(|hash| object_utils::recipe_chunk_hashes(hash).ok())
            .flatten()
            .collect();
        object_utils::recipe_chunk_hashes(&recipe).unwrap().into_iter().partition(|chunk| !kept.contains(chunk))
    };
    assert!(!own.is_empty() && !shared.is_empty(), "own {:?}, shared {:?}", own, shared);

    // Waits out the drain each deletion attempt serves 503 through. While the deletion is owed the
    // deleter keeps retrying, so any request can meet a fresh drain, not only the first.
    let served = |method: &str, path: &str, body: &[u8]| {
        for _ in 0..500 {
            let (status, reply) = http(&server.url, method, path, body);
            if !status.starts_with("HTTP/1.1 503") {
                return (status, reply);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("the warehouse stayed drained");
    };
    let report = || {
        let (status, info) = served("GET", "/v1/warehouse", b"");
        assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, info);
        serde_json::from_str::<serde_json::Value>(&info).unwrap()["redactions"].clone()
    };
    let objects = server.root.join(".forklift/objects");

    {
        // Another process using the server's store keeps the deletion owed.
        let _scope = forklift_core::globals::StorageRootScope::enter(&server.root);
        let _store_access = forklift_core::util::lock_utils::StoreAccessLock::try_shared().unwrap();

        owner.run_ok(&["lift"]);
        assert_eq!(report(), serde_json::json!([{ "hash": recipe, "state": "deletion_owed" }]));

        let (status, body) = served("GET", &format!("/v1/objects/{}", own[0]), b"");
        assert!(status.starts_with("HTTP/1.1 404"), "{}, {} bytes", status, body.len());
        assert!(objects.join(&own[0][..2]).join(&own[0][2..]).exists(), "the server still holds the chunk");

        let (status, _) = served("GET", &format!("/v1/objects/{}", shared[0]), b"");
        assert!(status.starts_with("HTTP/1.1 200"), "{}", status);

        // A bundle of the leak's own chunks holds none of them; one of the shared chunks holds them.
        let batch = |hashes: &[String]| {
            let (status, body) = served("POST", "/v1/objects/batch",
                                      serde_json::json!({ "hashes": hashes }).to_string().as_bytes());
            assert!(status.starts_with("HTTP/1.1 200"), "{}", status);
            body.len()
        };
        assert!(batch(&own) < 1024, "the bundle holds the leak's own chunks");
        assert!(batch(&shared) > 1024, "the bundle holds the shared chunks");

        // And missing, so a lift naming one uploads it.
        let asked = [own[0].clone(), shared[0].clone()];
        let (status, body) = served("POST", "/v1/objects/missing",
                                  serde_json::json!({ "hashes": asked }).to_string().as_bytes());
        assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, body);
        let missing: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(missing["missing"], serde_json::json!([own[0]]), "{}", body);

        // And upload-targets answers it to be uploaded, as missing does.
        let (status, body) = served("POST", "/v1/objects/upload-targets",
                                  serde_json::json!({ "session": "s", "hashes": asked }).to_string().as_bytes());
        assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, body);
        let targets: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!((&targets["direct"], &targets["present"]), (&serde_json::json!([own[0]]), &serde_json::json!([shared[0]])),
            "{}", body);
    }

    // A ref write asks for a round, which deletes the content now the store is free.
    owner.run_ok(&["shift", "main"]);
    owner.stack("notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);
    let mut state = serde_json::Value::Null;
    for _ in 0..500 {
        state = report();
        if state == serde_json::json!([{ "hash": recipe, "state": "deleted" }]) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(state, serde_json::json!([{ "hash": recipe, "state": "deleted" }]));
    assert!(!objects.join(&own[0][..2]).join(&own[0][2..]).exists(), "the chunk is deleted");
    let (status, _) = served("GET", &format!("/v1/objects/{}", shared[0]), b"");
    assert!(status.starts_with("HTTP/1.1 200"), "{}", status);
}

/// A warehouse that took content before its remote applied a redaction of it applies the redaction
/// on the remote's report once nothing here names the content, and deletes its own copy; the lower
/// says so, and names the git mirror's copy, which nothing here deletes. While the remote still
/// holds the redaction pending, a lower applies nothing, though nothing here names the content.
///
/// Revert: apply on any report, not only the hashes it lists → the first lower applies the pending
/// redaction. Revert: drop the apply after the pallet lands → the second lower applies nothing and
/// the copy stays.
#[test]
fn a_lower_applies_what_the_remote_reports_applied_and_deletes_the_copy_here() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-settle-server-{}", std::process::id())));
    let content = b"TOKEN=hunter2\n";
    let hash = hash_of(content);

    let owner = Warehouse::cloned("settle-owner", &server.url);
    owner.stack("notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);

    // Cloned before the leak, so no head here names it.
    let redactor = Warehouse::cloned("settle-redactor", &server.url);
    owner.stack("secret.env", content, "leak");
    owner.run_ok(&["lift"]);

    // This warehouse takes the leak, so it holds a copy, and takes the file out without lifting.
    let holder = Warehouse::cloned("settle-holder", &server.url);
    assert!(holder.holds_loose(&hash));
    holder.run_ok(&["remove", "secret.env"]);
    holder.run_ok(&["stack", "-m", "take the leak out"]);
    std::fs::create_dir_all(holder.root.join(".forklift/mirror.git")).unwrap();

    // The redaction goes up while the server's main still names the content: pending there.
    std::fs::write(redactor.root.join("secret.env"), content).unwrap();
    redactor.run_ok(&["load", "secret.env"]);
    recorded_hash(&redactor, &hash, "leaked token");
    redactor.run_ok(&["unload", "secret.env"]);
    forklift_core::util::file_utils::remove_file(redactor.root.join("secret.env")).unwrap();
    let lifted = redactor.run(&["lift"]);
    assert!(String::from_utf8_lossy(&lifted.stdout).contains("Lifted the @redactions pallet"), "{:?}", lifted);

    let pending = holder.json(&["lower"]);
    assert_eq!(pending["ok"], true, "{}", pending);
    assert_eq!(pending["data"].get("redactions"), None, "{}", pending);
    assert!(holder.holds_loose(&hash));
    assert_eq!(applied_record(&holder), "");
    assert_eq!(holder.redactions()[0]["status"]["state"], "awaiting_remote");

    // Once the server's main no longer names the content, the server applies the redaction.
    holder.run_ok(&["lift"]);
    let (_, info) = http(&server.url, "GET", "/v1/warehouse", b"");
    let info: serde_json::Value = serde_json::from_str(&info).unwrap();
    assert_ne!(info["redactions"][0]["state"], "pending", "{}", info);

    let lowered = holder.json(&["lower"]);
    assert_eq!(lowered["ok"], true, "{}", lowered);
    let settled = &lowered["data"]["redactions"];
    assert_eq!(settled["applied"], serde_json::json!([hash]), "{}", lowered);
    assert_eq!(settled["deletion"]["deleted"], serde_json::json!([hash]), "{}", lowered);
    assert!(settled["mirror_copy"].as_str().unwrap().contains("mirror.git"), "{}", lowered);
    assert!(!holder.holds_loose(&hash), "the copy here is deleted");
    assert_eq!(applied_record(&holder), format!("/forklift-redactions-applied 1\n{}\n", hash));
    assert_eq!(holder.redactions()[0]["status"]["state"], "applied");

    // A lower that applies nothing says nothing about redactions.
    let again = holder.json(&["lower"]);
    assert_eq!(again["data"].get("redactions"), None, "{}", again);
    let human = String::from_utf8_lossy(&holder.run_ok(&["lower"]).stdout).into_owned();
    assert!(!human.contains("mirror"), "nothing was applied this time: {}", human);
}

/// `redact apply` in a warehouse with a remote reads the remote's handshake, and applies on its
/// report; a remote it cannot reach leaves a clean redaction pending, and the list says why. A
/// fresh clone applies on the report too.
///
/// Revert: apply with no report in `redact apply` → the reachable case stays pending. Revert:
/// drop the apply in clone → the clone reports nothing applied. Revert: ask the remote with
/// nothing left to apply → the unreachable remote is reported.
#[test]
fn redact_apply_reads_the_remote_s_report_and_says_when_it_cannot() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-report-server-{}", std::process::id())));
    let content = b"TOKEN=hunter2\n";

    let owner = Warehouse::cloned("report-owner", &server.url);
    owner.stack("notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);
    let hash = owner.redact_loaded("secret.env", content, "leaked token");
    owner.run_ok(&["lift"]);

    // The owner recorded it before the remote had it, so it is still pending here.
    assert_eq!(owner.redactions()[0]["status"]["state"], "awaiting_remote");

    owner.run_ok(&["config", "set", "remote.url", "http://127.0.0.1:9"]);
    let unread = owner.json(&["redact", "apply"]);
    assert_eq!(unread["ok"], true, "{}", unread);
    assert!(unread["data"]["remote_unread"].is_string(), "{}", unread);
    assert_eq!(unread["data"]["redactions"][0]["status"]["state"], "awaiting_remote", "{}", unread);
    let human = String::from_utf8_lossy(&owner.run_ok(&["redact", "apply"]).stdout).into_owned();
    assert!(human.contains("The remote's report of what it has applied could not be read"), "{}", human);
    assert_eq!(applied_record(&owner), "");

    owner.run_ok(&["config", "set", "remote.url", &server.url]);
    let applied = owner.json(&["redact", "apply"]);
    assert_eq!(applied["data"].get("remote_unread"), None, "{}", applied);
    assert_eq!(applied["data"]["redactions"][0]["status"]["state"], "applied", "{}", applied);
    assert_eq!(applied_record(&owner), format!("/forklift-redactions-applied 1\n{}\n", hash));

    // With every redaction applied, the remote is not asked, so one that cannot be reached is not
    // reported.
    owner.run_ok(&["config", "set", "remote.url", "http://127.0.0.1:9"]);
    let settled = owner.json(&["redact", "apply"]);
    assert_eq!(settled["data"].get("remote_unread"), None, "{}", settled);
    owner.run_ok(&["config", "set", "remote.url", &server.url]);

    // A fresh clone applies it as it takes the remote's warehouse.
    let empty = Warehouse::empty("report-clone");
    let cloned = forklift_command()
        .args(["--json", "clone", &server.url, empty.root.to_str().unwrap()])
        .env("FORKLIFT_GLOBAL_CONFIG", empty.home.join("global-config.toml"))
        .env("FORKLIFT_KEYS_DIR", empty.home.join("keys"))
        .output()
        .unwrap();
    let cloned: serde_json::Value = serde_json::from_slice(&cloned.stdout).unwrap();
    assert_eq!(cloned["data"]["redactions"]["applied"], serde_json::json!([hash]), "{}", cloned);
    assert_eq!(applied_record(&empty), format!("/forklift-redactions-applied 1\n{}\n", hash));
}

/// A lower into a warehouse that never held the content, and a fresh clone, take history naming
/// content the remote reports applied without fetching it: each applies the redaction on the
/// remote's report before it writes the pallet's head, whose history check then holds the absence
/// as redacted. Neither holds a copy, and each lands on the remote's head.
///
/// Revert: fetch every file the history names, whatever the remote reports applied → the remote
/// refuses the content and the lower fails as redacted. Revert: apply only once the pallet has
/// landed → the head write finds the history incomplete.
#[test]
fn a_lower_and_a_clone_take_history_naming_applied_content_without_it() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-absent-server-{}", std::process::id())));
    let content = b"TOKEN=hunter2\n";

    let owner = Warehouse::cloned("absent-owner", &server.url);
    owner.stack("notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);

    // Cloned before the leak goes up, so it never holds the content.
    let late = Warehouse::cloned("absent-late", &server.url);

    // Its history names the leak; the redaction goes up first, so the server applies it at once
    // and takes that history without the content.
    let behind = Warehouse::cloned("absent-behind", &server.url);
    behind.leaked("secret.env", content);
    let hash = owner.redact_loaded("secret.env", content, "leaked token");
    owner.run_ok(&["lift"]);
    behind.run_ok(&["lower"]);
    behind.run_ok(&["lift"]);
    let (_, info) = http(&server.url, "GET", "/v1/warehouse", b"");
    let info: serde_json::Value = serde_json::from_str(&info).unwrap();
    assert_eq!(info["redactions"], serde_json::json!([{ "hash": hash, "state": "deleted" }]), "{}", info);

    let lowered = late.json(&["lower"]);
    assert_eq!(lowered["ok"], true, "{}", lowered);
    assert_eq!(lowered["data"]["redactions"]["applied"], serde_json::json!([hash]), "{}", lowered);
    assert_eq!(late.head("main"), server.main());
    assert!(!late.holds_loose(&hash));
    assert_eq!(applied_record(&late), format!("/forklift-redactions-applied 1\n{}\n", hash));

    let empty = Warehouse::empty("absent-clone");
    let cloned = forklift_command()
        .args(["--json", "clone", &server.url, empty.root.to_str().unwrap()])
        .env("FORKLIFT_GLOBAL_CONFIG", empty.home.join("global-config.toml"))
        .env("FORKLIFT_KEYS_DIR", empty.home.join("keys"))
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&cloned.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&cloned.stderr)));
    assert_eq!(report["ok"], true, "{}", report);
    assert_eq!(report["data"]["redactions"]["applied"], serde_json::json!([hash]), "{}", report);
    assert_eq!(empty.head("main"), server.main());
    assert!(!empty.holds_loose(&hash));
    assert_eq!(applied_record(&empty), format!("/forklift-redactions-applied 1\n{}\n", hash));
}

/// A sparse clone and lower walk the in-scope files of every parcel: one named by history in scope,
/// whose content the remote reports applied, is left out as a full fetch leaves it out. Revert:
/// the sparse walk asks for every in-scope file regardless, and both are refused as redacted.
#[test]
fn a_sparse_clone_and_lower_take_in_scope_history_naming_applied_content_without_it() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-sparse-server-{}", std::process::id())));
    let content = b"TOKEN=hunter2\n";

    let owner = Warehouse::cloned("sparse-owner", &server.url);
    owner.stack("src/keep.txt", b"keep\n", "keep");
    owner.run_ok(&["lift"]);

    let late = Warehouse::cloned_with("sparse-late", &server.url, &["--only", "src"]);

    let behind = Warehouse::cloned("sparse-behind", &server.url);
    behind.leaked("src/secret.env", content);
    let hash = owner.redact_loaded("src/secret.env", content, "leaked token");
    owner.run_ok(&["lift"]);
    behind.run_ok(&["lower"]);
    behind.run_ok(&["lift"]);

    let lowered = late.json(&["lower"]);
    assert_eq!(lowered["ok"], true, "{}", lowered);
    assert_eq!(lowered["data"]["redactions"]["applied"], serde_json::json!([hash]), "{}", lowered);
    assert_eq!(late.head("main"), server.main());

    let fresh = Warehouse::cloned_with("sparse-fresh", &server.url, &["--only", "src"]);
    assert_eq!(fresh.head("main"), server.main());
    assert!(!fresh.holds_loose(&hash));
    assert_eq!(applied_record(&fresh), format!("/forklift-redactions-applied 1\n{}\n", hash));
}

/// A lower applies what the remote reports applied before it writes anything, so one refused
/// afterwards keeps the redaction applied and warns `redactions_unsettled`, naming it; the next
/// lower goes through. Revert: dropping the warning on the refused path leaves its stderr silent.
#[test]
fn a_lower_refused_after_applying_a_redaction_warns_it_unsettled() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-refused-server-{}", std::process::id())));
    let content = b"TOKEN=hunter2\n";

    let owner = Warehouse::cloned("refused-owner", &server.url);
    owner.stack("notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);

    let late = Warehouse::cloned("refused-late", &server.url);

    let behind = Warehouse::cloned("refused-behind", &server.url);
    behind.leaked("secret.env", content);
    let _ = forklift_core::util::file_utils::remove_file(behind.root.join("secret.env"));
    behind.stack("more.txt", b"more\n", "more");
    let hash = owner.redact_loaded("secret.env", content, "leaked token");
    owner.run_ok(&["lift"]);
    behind.run_ok(&["lower"]);
    behind.run_ok(&["lift"]);

    // An untracked file in the way of the one the remote's head adds refuses the lower.
    std::fs::write(late.root.join("more.txt"), b"mine\n").unwrap();
    let before = late.head("main");
    let refused = late.run(&["--json", "lower"]);
    assert!(!refused.status.success(), "{}", String::from_utf8_lossy(&refused.stdout));
    assert_eq!(late.head("main"), before);
    let warned = String::from_utf8_lossy(&refused.stderr);
    assert!(warned.contains("\"redactions_unsettled\"") && warned.contains(&hash), "{}", warned);
    assert_eq!(applied_record(&late), format!("/forklift-redactions-applied 1\n{}\n", hash));

    forklift_core::util::file_utils::remove_file(late.root.join("more.txt")).unwrap();
    let lowered = late.json(&["lower"]);
    assert_eq!(lowered["ok"], true, "{}", lowered);
    assert_eq!(late.head("main"), server.main());
}

/// A first-contact lower fetches the pallet's history before any `@redactions` is here: it leaves
/// out what the remote reports applied, applies it once the meta pallets are synced, and takes the
/// head. Revert: fetching every file at first contact, and the lower is refused as redacted.
#[test]
fn a_first_contact_lower_takes_history_naming_applied_content_without_it() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-first-server-{}", std::process::id())));
    let content = b"TOKEN=hunter2\n";

    // An enrolled office, so a warehouse new to this remote takes its anchor at first contact.
    let owner = Warehouse::cloned("first-owner", &server.url);
    owner.run_ok(&["office", "enroll"]);
    owner.stack("notes.txt", b"notes\n", "notes");
    let leak = owner.leaked("secret.env", content);
    let hash = recorded_hash(&owner, &format!("{}:secret.env", leak), "leaked token")["data"]["hash"]
        .as_str().unwrap().to_string();
    // The server applies the redaction once `@redactions` lands, refusing the rest of this lift;
    // the next one leaves the content out.
    owner.run(&["lift"]);
    owner.run_ok(&["lift"]);
    let (_, info) = http(&server.url, "GET", "/v1/warehouse", b"");
    let info: serde_json::Value = serde_json::from_str(&info).unwrap();
    assert_eq!(info["redactions"], serde_json::json!([{ "hash": hash, "state": "deleted" }]), "{}", info);

    let fresh = Warehouse::new("first-fresh");
    fresh.run_ok(&["config", "set", "remote.url", &server.url]);
    let lowered = fresh.json(&["lower"]);
    assert_eq!(lowered["ok"], true, "{}", lowered);
    assert!(lowered["data"]["adopted_genesis"].is_string(), "{}", lowered);
    assert_eq!(lowered["data"]["redactions"]["applied"], serde_json::json!([hash]), "{}", lowered);
    assert_eq!(fresh.head("main"), server.main());
    assert!(!fresh.holds_loose(&hash));
    assert_eq!(applied_record(&fresh), format!("/forklift-redactions-applied 1\n{}\n", hash));
}

/// A sparse warehouse and a cache that took history naming a leak, before their remote applied a
/// redaction of it, and have not lowered since: the remote reports the redaction applied, and this
/// warehouse holds no `@redactions` cell for it.
struct Behind {
    server: Server,
    sparse: Warehouse,
    cache: Warehouse,
    leak: String,
    hash: String,
}

fn behind_an_applied_redaction(name: &str) -> Behind {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-{}-server-{}", name, std::process::id())));
    let content = b"TOKEN=hunter2\n";

    let owner = Warehouse::cloned(&format!("{}-owner", name), &server.url);
    owner.stack("src/keep.txt", b"keep\n", "keep");
    owner.stack("other/keep.txt", b"keep\n", "keep");
    owner.run_ok(&["lift"]);
    let leak = owner.leaked("other/secret.env", content);
    owner.run_ok(&["lift"]);

    // Neither holds the content: it is out of the sparse scope, and a cache fetches no content.
    let sparse = Warehouse::cloned_with(&format!("{}-sparse", name), &server.url, &["--only", "src"]);
    let cache = Warehouse::cloned_with(&format!("{}-cache", name), &server.url, &["--cache"]);

    // No head names it, so the server applies the redaction at once and deletes its copy.
    std::fs::create_dir_all(owner.root.join("other")).unwrap();
    let hash = owner.redact_loaded("other/secret.env", content, "leaked token");
    owner.run_ok(&["lift"]);
    // The server settles the apply after it answers the write, so its report may lag the lift.
    let deleted = serde_json::json!([{ "hash": hash, "state": "deleted" }]);
    let mut info = serde_json::Value::Null;
    for _ in 0..500 {
        let (_, body) = http(&server.url, "GET", "/v1/warehouse", b"");
        info = serde_json::from_str(&body).unwrap();
        if info["redactions"] == deleted {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(info["redactions"], deleted, "{}", info);

    Behind { server, sparse, cache, leak, hash }
}

/// `scope fetch` widens a sparse warehouse over history naming content its remote applied a
/// redaction of: before this warehouse applies it, the remote's refusal is the widening's, coded
/// `content_redacted`; once a lower applied it, the widening leaves the content out and goes
/// through, holding no copy.
///
/// Revert: walk only the files a command set to skip, not the content applied here → the second
/// widening asks the remote and is refused. Revert: frame the widening's refusal as plain text →
/// the first is refused as `error`.
#[test]
fn a_scope_fetch_widens_over_history_naming_content_applied_here() {
    let behind = behind_an_applied_redaction("widen");
    let sparse = &behind.sparse;

    let refused = content_redacted(sparse, &["scope", "fetch", "other"]);
    assert!(refused["message"].as_str().unwrap().contains("cannot be fetched at the widened scope"), "{}", refused);
    assert_eq!(sparse.json(&["scope"])["data"]["fetch_scope"], serde_json::json!(["src"]));

    let lowered = sparse.json(&["lower"]);
    assert_eq!(lowered["ok"], true, "{}", lowered);
    assert_eq!(lowered["data"]["redactions"]["applied"], serde_json::json!([behind.hash]), "{}", lowered);

    let widened = sparse.json(&["scope", "fetch", "other"]);
    assert_eq!(widened["ok"], true, "{}", widened);
    assert!(!sparse.holds_loose(&behind.hash));
    assert_eq!(sparse.head("main"), behind.server.main());
}

/// A cache reads content its remote applied a redaction of as redacted: before the cache applies
/// it, the fetch the read makes is refused by the remote, and the read with it, coded
/// `content_redacted`; once a lower applied it, the read is refused here, asking nothing, and a
/// fresh cache clone applies it as it takes the history. Neither holds a copy.
///
/// Revert: word the miss hook's failed fetch as plain text → the first read is refused as `error`.
#[test]
fn a_cache_reads_content_its_remote_applied_a_redaction_of_as_redacted() {
    let behind = behind_an_applied_redaction("cache-read");
    let cache = &behind.cache;
    let target = format!("{}:other/secret.env", behind.leak);

    let refused = content_redacted(cache, &["show", &target]);
    assert!(refused["message"].as_str().unwrap().contains("from the origin failed"), "{}", refused);
    assert!(!cache.holds_loose(&behind.hash));

    let lowered = cache.json(&["lower"]);
    assert_eq!(lowered["ok"], true, "{}", lowered);
    assert_eq!(lowered["data"]["redactions"]["applied"], serde_json::json!([behind.hash]), "{}", lowered);
    let refused = content_redacted(cache, &["show", &target]);
    assert!(refused["message"].as_str().unwrap().contains("its content has been removed for good"), "{}", refused);

    let fresh = Warehouse::cloned_with("cache-read-fresh", &behind.server.url, &["--cache"]);
    assert_eq!(applied_record(&fresh), format!("/forklift-redactions-applied 1\n{}\n", behind.hash));
    let refused = content_redacted(&fresh, &["show", &target]);
    assert!(refused["message"].as_str().unwrap().contains("its content has been removed for good"), "{}", refused);
    assert!(!fresh.holds_loose(&behind.hash));
}

/// A sparse clone fetches the ignore files above its scope, which are in every scope: one that
/// history names with content the remote applied a redaction of is left out as an in-scope file
/// is, and the clone applies the redaction and goes through. Revert: ask for every ignore file on
/// the walk's spine regardless → the remote refuses it and the clone fails as redacted.
#[test]
fn a_sparse_clone_takes_history_naming_an_applied_ignore_file_above_its_scope() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-spine-server-{}", std::process::id())));
    let content = b"TOKEN=hunter2\n";

    let owner = Warehouse::cloned("spine-owner", &server.url);
    owner.stack("src/keep.txt", b"keep\n", "keep");
    owner.run_ok(&["lift"]);

    let behind = Warehouse::cloned("spine-behind", &server.url);
    behind.leaked(".gitignore", content);
    let hash = owner.redact_loaded(".gitignore", content, "leaked token");
    owner.run_ok(&["lift"]);
    behind.run_ok(&["lower"]);
    behind.run_ok(&["lift"]);

    let empty = Warehouse::empty("spine-fresh");
    let cloned = forklift_command()
        .args(["--json", "clone", &server.url, empty.root.to_str().unwrap(), "--only", "src"])
        .env("FORKLIFT_GLOBAL_CONFIG", empty.home.join("global-config.toml"))
        .env("FORKLIFT_KEYS_DIR", empty.home.join("keys"))
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&cloned.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&cloned.stderr)));
    assert_eq!(report["ok"], true, "{}", report);
    assert_eq!(report["data"]["redactions"]["applied"], serde_json::json!([hash]), "{}", report);
    assert_eq!(empty.head("main"), server.main());
    assert!(!empty.holds_loose(&hash));
}

/// A walk over an unreadable record of applied redactions is refused before it asks for anything:
/// the record is what says which content was removed here on purpose, so a walk that cannot read
/// it would take back from the remote whatever it meets. `scope fetch` reads the record first
/// through the walk itself.
///
/// Revert: let the walk fall back to skipping nothing → the widening succeeds, taking the content.
#[test]
fn a_walk_over_an_unreadable_applied_record_takes_nothing_back() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-applied-unreadable-server-{}", std::process::id())));

    let owner = Warehouse::cloned("applied-unreadable-owner", &server.url);
    owner.stack("src/keep.txt", b"keep\n", "src");
    owner.stack("other/secret.env", b"TOKEN=unreadable\n", "other");
    owner.run_ok(&["lift"]);
    let hash = hash_of(b"TOKEN=unreadable\n");

    let sparse = Warehouse::cloned_with("applied-unreadable-sparse", &server.url, &["--only", "src"]);
    assert!(!sparse.holds_loose(&hash));
    std::fs::write(sparse.root.join(".forklift/redactions-applied"), b"not a record\n").unwrap();

    let widened = sparse.json(&["scope", "fetch", "other"]);
    assert_eq!(widened["ok"], false, "{}", widened);
    assert!(widened["error"]["message"].as_str().unwrap().contains("record of applied redactions"), "{}", widened);
    assert!(!sparse.holds_loose(&hash), "the walk fetched content past an unreadable record");
}

/// The paths of every file and folder in the git commit `commit` of the repository at `repo`.
fn git_paths(repo: &std::path::Path, commit: &str) -> Vec<String> {
    let listed = std::process::Command::new("git")
        .args(["ls-tree", "-r", "-t", "--name-only", commit])
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
        .output()
        .unwrap();
    assert!(listed.status.success(), "{}", String::from_utf8_lossy(&listed.stderr));
    String::from_utf8_lossy(&listed.stdout).lines().map(str::to_string).collect()
}

/// `git export` leaves a file whose redaction this warehouse applied out of every commit that
/// carried it, never writing anything in its place, and a folder that held nothing else goes with
/// it; every other file is exported as before. The report names the file by path, with each parcel
/// whose commit left it out.
///
/// Revert: export the file as before → the export fails, reading the deleted content. Revert: keep
/// a folder every entry of which was left out → the leak's commit holds an empty `other`.
#[test]
fn git_export_leaves_out_a_file_whose_redaction_is_applied_and_names_it() {
    let warehouse = Warehouse::new("export-omit");
    let content = b"TOKEN=hunter2\n";
    warehouse.stack("keep.txt", b"keep\n", "keep");
    warehouse.stack("other/secret.env", content, "leak");
    let leak = warehouse.head("main");
    warehouse.stack("keep.txt", b"keep 2\n", "change");
    let change = warehouse.head("main");
    warehouse.run_ok(&["remove", "other/secret.env"]);
    warehouse.run_ok(&["stack", "-m", "take the leak out"]);

    let recorded = recorded_hash(&warehouse, &format!("{}:other/secret.env", leak), "leaked token");
    let hash = recorded["data"]["hash"].as_str().unwrap().to_string();
    assert_eq!(recorded["data"]["status"]["state"], "applied", "{}", recorded);
    assert!(!warehouse.holds_loose(&hash));

    let target = warehouse.root.parent().unwrap().join("exported");
    let exported = warehouse.json(&["git", "export", target.to_str().unwrap()]);
    assert_eq!(exported["ok"], true, "{}", exported);
    assert_eq!(exported["data"]["commits"], 4, "{}", exported);
    assert_eq!(exported["data"]["redacted"], serde_json::json!([
        { "path": "other/secret.env", "hash": hash, "parcels": [leak, change] }
    ]), "{}", exported);

    let commits = std::process::Command::new("git").args(["rev-list", "--all"]).current_dir(&target)
        .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
        .output().unwrap();
    let commits = String::from_utf8_lossy(&commits.stdout).lines().map(str::to_string).collect::<Vec<_>>();
    assert_eq!(commits.len(), 4, "{:?}", commits);
    for commit in &commits {
        assert_eq!(git_paths(&target, commit), vec!["keep.txt".to_string()], "commit {}", commit);
    }

    let human = String::from_utf8_lossy(&warehouse.run_ok(&["git", "export", target.with_extension("2").to_str().unwrap()]).stdout)
        .into_owned();
    assert!(human.contains("Left out \"other/secret.env\""), "{}", human);
    assert!(human.contains("of 2 commit(s)"), "{}", human);
}

/// A lift may name content the server's host withholds and the server never held: nothing asks
/// for its bytes, the upload negotiation answers it withheld and `missing` leaves it out, and the
/// ref writes tolerate its absence, including one that moves the file to another directory. The
/// server still lacks it, and refuses it.
///
/// Revert: answer a withheld hash as `direct` in the negotiation → the negotiation names it there;
/// drop the withheld arm of the closure tolerance → the lift's ref write is refused; list a
/// withheld hash as missing → `missing` names it.
#[test]
fn a_lift_naming_withheld_content_lands_without_its_bytes() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-takedown-unheld-server-{}", std::process::id())));

    let owner = Warehouse::cloned("takedown-unheld-owner", &server.url);
    owner.stack("notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);

    let content = b"a film the server never held\n";
    let hash = hash_of(content);
    let statement = "court order 2026-41";
    let added = withhold(&server.root, &["add", &hash, "--statement", statement]);
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));

    let (status, body) = http(&server.url, "POST", "/v1/objects/missing",
                              format!("{{\"hashes\":[\"{}\"]}}", hash).as_bytes());
    assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, body);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["missing"], serde_json::json!([]), "{}", body);

    let (status, body) = http(&server.url, "POST", "/v1/objects/upload-targets",
                              format!("{{\"session\":\"s\",\"hashes\":[\"{}\"]}}", hash).as_bytes());
    assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, body);
    let negotiated: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(negotiated["withheld"], serde_json::json!([hash]), "{}", body);
    assert_eq!(negotiated["direct"], serde_json::json!([]), "{}", body);

    owner.stack("film.mkv", content, "film");
    owner.run_ok(&["lift"]);
    assert_eq!(server.main(), owner.head("main"));

    // A tree that moves the file elsewhere names it again; the write tolerates that too.
    std::fs::create_dir_all(owner.root.join("films")).unwrap();
    std::fs::write(owner.root.join("films/film.mkv"), content).unwrap();
    forklift_core::util::file_utils::remove_file(owner.root.join("film.mkv")).unwrap();
    owner.run_ok(&["load", "."]);
    owner.run_ok(&["stack", "-m", "move the film"]);
    owner.run_ok(&["lift"]);
    assert_eq!(server.main(), owner.head("main"));

    // The server never took the bytes, and refuses them.
    let (status, info) = http(&server.url, "GET", "/v1/warehouse", b"");
    assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, info);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&info).unwrap()["withheld"], serde_json::json!([
        { "hash": hash, "statement": statement, "state": "deleted" }
    ]), "{}", info);
    let (status, body) = http(&server.url, "GET", &format!("/v1/objects/{}", hash), b"");
    assert!(status.starts_with("HTTP/1.1 451"), "{} / {}", status, body);
}

/// A lift leaves out content the server's host withholds, a chunked file's chunks with its recipe,
/// and reports each such hash with the host's statement; the ref write lands without them.
///
/// Revert: descend a withheld recipe as any other → its chunks are uploaded, and
/// `uploaded_objects` counts them; drop the report → `withheld` is absent.
#[test]
fn a_lift_leaves_out_withheld_content_and_its_chunks_and_reports_them() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-takedown-lift-server-{}", std::process::id())));

    let owner = Warehouse::cloned("takedown-lift-owner", &server.url);
    owner.stack("notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);

    // Above the chunking threshold, and varied, so the file is a recipe of several chunks.
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let content: Vec<u8> = (0..9 * 1024 * 1024).map(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as u8
    }).collect();
    std::fs::write(owner.root.join("big.bin"), &content).unwrap();
    std::fs::write(owner.root.join("film.mkv"), b"a film\n").unwrap();
    owner.run_ok(&["load", "."]);
    owner.run_ok(&["stack", "-m", "leaks"]);

    let (recipe, chunks) = {
        let _scope = forklift_core::globals::StorageRootScope::enter(&owner.root);
        let root = object_utils::load_parcel(&owner.head("main")).unwrap().tree_hash;
        let tree = object_utils::load_tree(&root).unwrap();
        let recipe = tree.get_files().find(|(name, _)| name.as_str() == "big.bin").unwrap().1.hash.clone();
        let chunks = object_utils::recipe_chunk_hashes(&recipe).unwrap();
        (recipe, chunks)
    };
    assert!(chunks.len() > 1, "{:?}", chunks);
    let film = hash_of(b"a film\n");

    for (hash, statement) in [(&recipe, "court order 2026-51"), (&film, "court order 2026-52")] {
        let added = withhold(&server.root, &["add", hash, "--statement", statement]);
        assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    }

    let lifted = owner.json(&["lift"]);
    assert_eq!(server.main(), owner.head("main"));

    // The new parcel and its tree, and nothing the withheld content names.
    assert_eq!(lifted["data"]["uploaded_objects"], 2, "{}", lifted);
    let mut expected = vec![
        serde_json::json!({ "hash": recipe, "statement": "court order 2026-51" }),
        serde_json::json!({ "hash": film, "statement": "court order 2026-52" }),
    ];
    expected.sort_by_key(|item| item["hash"].as_str().unwrap().to_string());
    assert_eq!(lifted["data"]["withheld"], serde_json::Value::Array(expected), "{}", lifted);

    let objects = server.root.join(".forklift/objects");
    for chunk in &chunks {
        assert!(loose_copies(&objects, chunk).is_empty(), "chunk {} reached the server", chunk);
    }

    // The server never held the recipe, so only the owner's copy, sent to its digest intake, gave
    // it the digest; it kept nothing of it.
    assert_eq!(withheld_digests(&server.url).get(&recipe), Some(&Some(chunk_list_digest(&chunks))));
    assert_eq!(withheld_digests(&server.url).get(&film), Some(&None), "a blob has no digest");
    assert!(loose_copies(&objects, &recipe).is_empty(), "the recipe was stored");
}

/// The `chunks` digest of every withheld entry the handshake at `url` lists, by hash.
fn withheld_digests(url: &str) -> BTreeMap<String, Option<String>> {
    let (status, info) = http(url, "GET", "/v1/warehouse", b"");
    assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, info);
    let info: serde_json::Value = serde_json::from_str(&info).unwrap();
    assert_eq!(info["withheld_intake"], "direct", "{}", info);

    info["withheld"].as_array().unwrap().iter()
        .map(|item| (
            item["hash"].as_str().unwrap().to_string(),
            item["chunks"].as_str().map(str::to_string),
        ))
        .collect()
}

/// 9 MiB of varied bytes from `seed`: above the chunking threshold, so the file is a recipe of
/// several chunks.
fn chunked_content(seed: u64) -> Vec<u8> {
    let mut state = seed;

    (0..9 * 1024 * 1024).map(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as u8
    }).collect()
}

/// The recipe `name` is at in the head of `warehouse`'s main pallet, and its chunks.
fn recipe_at(warehouse: &Warehouse, name: &str) -> (String, Vec<String>) {
    let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);
    let root = object_utils::load_parcel(&warehouse.head("main")).unwrap().tree_hash;
    let tree = object_utils::load_tree(&root).unwrap();
    let recipe = tree.get_files().find(|(entry, _)| entry.as_str() == name).unwrap().1.hash.clone();
    let chunks = object_utils::recipe_chunk_hashes(&recipe).unwrap();

    (recipe, chunks)
}

/// forklift-server's digest intake records the chunk-list digest of a withheld recipe it never
/// held from a copy a client sends, and keeps nothing of it. It refuses, with a `422` and nothing
/// changed, bytes that do not hash to the hash and bytes that are not a recipe. Bytes for a hash
/// not withheld change nothing either.
///
/// Revert: record without verifying the hash → the mismatched copy (another recipe's bytes)
/// records that recipe's digest. Accept any object → the blob is answered `204`. Store the bytes →
/// the recipe is in the server's store.
#[test]
fn the_digest_intake_records_a_withheld_recipes_digest_and_keeps_nothing() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-takedown-intake-server-{}", std::process::id())));

    let owner = Warehouse::cloned("takedown-intake-owner", &server.url);
    std::fs::write(owner.root.join("big.bin"), chunked_content(0x2545_f491_4f6c_dd1d)).unwrap();
    std::fs::write(owner.root.join("other.bin"), chunked_content(0x9e37_79b9_7f4a_7c15)).unwrap();
    std::fs::write(owner.root.join("film.mkv"), b"a film\n").unwrap();
    owner.run_ok(&["load", "."]);
    owner.run_ok(&["stack", "-m", "leaks"]);

    let (recipe, chunks) = recipe_at(&owner, "big.bin");
    let (other, _) = recipe_at(&owner, "other.bin");
    let film = hash_of(b"a film\n");
    let bytes = |hash: &str| {
        let _scope = forklift_core::globals::StorageRootScope::enter(&owner.root);
        forklift_core::util::file_utils::retrieve_object_by_hash(hash).unwrap()
    };

    for hash in [&recipe, &film] {
        let added = withhold(&server.root, &["add", hash, "--statement", "court order 2026-61"]);
        assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    }

    let unknown = BTreeMap::from([(film.clone(), None), (recipe.clone(), None)]);
    assert_eq!(withheld_digests(&server.url), unknown);

    // Another recipe's bytes, and a blob's under its own hash.
    for (hash, body) in [(&recipe, bytes(&other)), (&film, bytes(&film))] {
        let (status, body) = http(&server.url, "PUT", &format!("/v1/withheld/{}/recipe", hash), &body);
        assert!(status.starts_with("HTTP/1.1 422"), "{} / {}", status, body);
    }
    assert_eq!(withheld_digests(&server.url), unknown, "a refused copy changes nothing");

    // A recipe not withheld is answered, and nothing is recorded.
    let (status, body) = http(&server.url, "PUT", &format!("/v1/withheld/{}/recipe", other), &bytes(&other));
    assert!(status.starts_with("HTTP/1.1 204"), "{} / {}", status, body);
    assert_eq!(withheld_digests(&server.url), unknown);

    let (status, body) = http(&server.url, "PUT", &format!("/v1/withheld/{}/recipe", recipe), &bytes(&recipe));
    assert!(status.starts_with("HTTP/1.1 204"), "{} / {}", status, body);
    assert_eq!(withheld_digests(&server.url), BTreeMap::from([
        (film.clone(), None), (recipe.clone(), Some(chunk_list_digest(&chunks))),
    ]));

    let objects = server.root.join(".forklift/objects");
    for hash in [&recipe, &other, &film] {
        assert!(loose_copies(&objects, hash).is_empty(), "{} was stored", hash);
    }
}

/// A lift sends the digest intake a withheld recipe the server has no digest for even when its
/// walk never meets the recipe: a new pallet whose history the server holds on another pallet
/// up to its head, which the walk stops at.
///
/// The fixture takes the digest off the server's record by hand, as a takedown recorded before
/// the server took digests leaves it, since no command does.
///
/// Revert: send only the withheld content the lift's walk met → the entry stays without a digest.
#[test]
fn a_lift_sends_the_digest_intake_a_withheld_recipe_its_walk_never_meets() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-takedown-intake-elsewhere-server-{}", std::process::id())));

    let owner = Warehouse::cloned("takedown-intake-elsewhere-owner", &server.url);
    std::fs::write(owner.root.join("big.bin"), chunked_content(0x2545_f491_4f6c_dd1d)).unwrap();
    owner.run_ok(&["load", "."]);
    owner.run_ok(&["stack", "-m", "leak"]);
    let (recipe, chunks) = recipe_at(&owner, "big.bin");

    let added = withhold(&server.root, &["add", &recipe, "--statement", "court order 2026-62"]);
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    owner.run_ok(&["lift"]);
    assert_eq!(server.main(), owner.head("main"));

    let digest = chunk_list_digest(&chunks);
    assert_eq!(withheld_digests(&server.url).get(&recipe), Some(&Some(digest.clone())));

    let record = server.root.join(".forklift/withheld");
    let text = std::fs::read_to_string(&record).unwrap();
    let stripped = text.replace(&format!("{} {} ", recipe, digest), &format!("{} - ", recipe));
    assert_ne!(stripped, text, "{}", text);
    std::fs::write(&record, stripped).unwrap();
    assert_eq!(withheld_digests(&server.url).get(&recipe), Some(&None));

    owner.run_ok(&["pallet", "create", "copy"]);
    owner.stack("notes.txt", b"notes\n", "notes");
    let lifted = owner.json(&["lift"]);
    assert!(lifted["data"].get("withheld").is_none(), "the walk met the recipe: {}", lifted);

    assert_eq!(withheld_digests(&server.url).get(&recipe), Some(&Some(digest)));
}

/// Only a file's content is tolerated absent for being withheld: a lift whose history names a
/// withheld hash as a parcel is refused, and the refusal says the host withholds it, since the
/// negotiation never asked for it.
///
/// Revert: drop either note → the refusal reads as history the client failed to upload.
#[test]
fn a_lift_naming_a_withheld_hash_as_history_says_the_host_withholds_it() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-takedown-parcel-server-{}", std::process::id())));

    let owner = Warehouse::cloned("takedown-parcel-owner", &server.url);
    owner.stack("notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);
    let lifted = server.main();

    // The head itself.
    owner.stack("draft.txt", b"draft\n", "draft");
    let draft = owner.head("main");
    let added = withhold(&server.root, &["add", &draft, "--statement", "court order 2026-42"]);
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    let refused = owner.run(&["lift"]);
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success() && stderr.contains("host withholds it"), "{}", stderr);
    assert_eq!(server.main(), lifted);

    // A parcel the new head descends from.
    owner.stack("final.txt", b"final\n", "final");
    let refused = owner.run(&["lift"]);
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success() && stderr.contains(&format!("host withholds {}", draft)), "{}", stderr);
    assert_eq!(server.main(), lifted);
}

/// Run `forklift-server withhold` with `args` against the server root at `root`.
fn withhold(root: &std::path::Path, args: &[&str]) -> Output {
    std::process::Command::new(common::server_binary()).env("FORKLIFT_FSYNC", "0")
        .arg("withhold").args(args).args(["--root", root.to_str().unwrap()])
        .output().unwrap()
}

/// A takedown on forklift-server takes effect while it serves: every surface that reads or takes
/// the content answers `451` with the host's statement, the handshake lists
/// it, and the head naming the file is untouched. The served root deletes the copy within seconds,
/// draining the warehouse meanwhile, so a request refused as unavailable is asked again and the
/// deletion is listed owed or done. The bundle that held it is
/// withdrawn. A tree's hash is refused, as a redaction's would be.
///
/// Revert: answer a withheld hash as served on `GET` (drop the withheld arm of the refusal) → the
/// read is a `200`; drop the handshake's list → `withheld` is null.
#[test]
fn a_server_withholds_content_on_its_hosts_statement_while_it_serves() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-takedown-server-{}", std::process::id())));

    let owner = Warehouse::cloned("takedown-owner", &server.url);
    let content = b"leaked film\n";
    owner.stack("film.mkv", content, "film");
    owner.stack("notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);
    let head = server.main();
    let hash = hash_of(content);

    let built = std::process::Command::new(common::server_binary()).env("FORKLIFT_FSYNC", "0")
        .args(["bundle", "--root", server.root.to_str().unwrap()]).output().unwrap();
    assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));
    let bundle = server.root.join(".forklift/bundles/latest");
    assert!(bundle.exists(), "a bundle is published: {:?}", std::fs::read_dir(server.root.join(".forklift/bundles")));

    let statement = "DMCA notice 2026-117 from Example Studios";
    let added = withhold(&server.root, &["add", &hash, "--statement", statement]);
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    assert!(String::from_utf8_lossy(&added.stdout).contains("Withheld"), "{:?}", added);
    assert!(!bundle.exists(), "the bundle that held the content is withdrawn");

    let listed = withhold(&server.root, &["list"]);
    assert_eq!(String::from_utf8_lossy(&listed.stdout), format!("{} {}\n", hash, statement));

    // The deletion drains the warehouse briefly: a request it refuses is asked again.
    let ask = |method: &str, path: &str, body: &[u8]| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let (status, answer) = http(&server.url, method, path, body);
            if !status.starts_with("HTTP/1.1 503") || std::time::Instant::now() >= deadline {
                return (status, answer);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    };

    let (status, info) = ask("GET", "/v1/warehouse", b"");
    assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, info);
    let info: serde_json::Value = serde_json::from_str(&info).unwrap();
    let state = info["withheld"][0]["state"].as_str().unwrap_or_default().to_string();
    assert!(state == "deletion_owed" || state == "deleted", "{}", info);
    assert_eq!(info["withheld"], serde_json::json!([
        { "hash": hash, "statement": statement, "state": state }
    ]), "{}", info);

    let object = format!("/v1/objects/{}", hash);
    let refusals = [
        ("get", ask("GET", &object, b"")),
        ("put", ask("PUT", &object, &object_utils::blob_bytes(content))),
    ];
    for (surface, (status, body)) in refusals {
        assert!(status.starts_with("HTTP/1.1 451"), "{}: {} / {}", surface, status, body);
        assert!(body.contains("content_withheld") && body.contains(statement), "{}: {}", surface, body);
    }

    // History is the warehouse's: the head naming the file stays.
    assert_eq!(server.main(), head);

    // A takedown removes one file's content: a parcel's hash is refused.
    let parcel = withhold(&server.root, &["add", &head, "--statement", statement]);
    assert!(!parcel.status.success(), "{:?}", parcel);
    assert_eq!(withhold(&server.root, &["list"]).stdout, listed.stdout);
}

/// The raw response to a `GET` of `path`: status line, headers and body.
fn http_raw(url: &str, path: &str) -> String {
    use std::io::{Read, Write};

    let address = url.strip_prefix("http://").unwrap();
    let mut stream = std::net::TcpStream::connect(address).unwrap();
    write!(stream, "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", path, address).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();

    String::from_utf8_lossy(&response).into_owned()
}

/// A served root whose record of withheld content is lost or damaged never serves the content it
/// withheld, nor answers as if nothing were withheld: every request that reads the record, the
/// handshake and a read of the content alike, is a `503` `withheld_damaged` naming the operator's
/// repair, with no `Retry-After` and no path of the server's; a clone of it exits 56 and is told
/// to ask the operator. The repair keeps a hash whose entry lost its statement
/// withheld, and lists content a lost entry withheld whose copy the server already deleted.
///
/// Revert: read a missing record as nothing withheld → the content still stored is a `200`;
/// leave the damage uncoded → a `500` naming the server's path; drop the client's arm → the next
/// step is the operator's command alone; drop the placeholder → the content is served again after
/// the repair; drop the walk → the deleted content is not listed.
#[test]
fn a_served_root_whose_withheld_record_is_lost_or_damaged_refuses_until_its_operator_repairs_it() {
    let root = std::env::temp_dir().join(format!("forklift-withheld-damaged-{}", std::process::id()));
    // A killed server leaves its serve lock behind; no process holds it.
    let stop = |server: Server| {
        drop(server);
        forklift_core::util::file_utils::remove_file(root.join(".forklift/serve.lock")).unwrap();
    };
    let record = root.join(".forklift/withheld");
    let server = Server::start(&root);

    let owner = Warehouse::cloned("withheld-damaged-owner", &server.url);
    let film = b"leaked film 4\n";
    owner.stack("film.mkv", film, "film");
    owner.stack("notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);
    let film = hash_of(film);
    stop(server);

    // Withheld while nothing serves the root, so its copy is still stored, and then lost.
    let added = withhold(&root, &["add", &film, "--statement", "court order 2026-90"]);
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    std::fs::remove_file(&record).unwrap();

    let refuses = |server: &Server, what: &str| {
        for path in ["/v1/warehouse".to_string(), format!("/v1/objects/{}", film)] {
            let response = http_raw(&server.url, &path);
            let (head, body) = response.split_once("\r\n\r\n").unwrap();
            assert!(head.starts_with("HTTP/1.1 503"), "{} {}: {}", what, path, response);
            assert!(!head.to_ascii_lowercase().contains("retry-after"), "{} {}: {}", what, path, head);
            assert!(body.contains(r#""code":"withheld_damaged""#), "{} {}: {}", what, path, body);
            assert!(body.contains("forklift-server withhold repair --root <warehouse>"), "{} {}: {}", what, path, body);
            assert!(!body.contains(&*root.to_string_lossy()), "{} {}: no server path: {}", what, path, body);
        }
    };

    let server = Server::serve(&root);
    refuses(&server, "lost");

    // Damaged: the film's entry cut inside its digest column.
    std::fs::write(&record, format!("/forklift-withheld 2\n{} 1", film)).unwrap();
    refuses(&server, "damaged");

    let clone = Warehouse::empty("withheld-damaged-clone");
    let cloned = forklift_command()
        .args(["--json", "clone", &server.url, clone.root.to_str().unwrap()])
        .env("FORKLIFT_GLOBAL_CONFIG", clone.home.join("global-config.toml"))
        .env("FORKLIFT_KEYS_DIR", clone.home.join("keys"))
        .output().unwrap();
    assert_eq!(cloned.status.code(), Some(56), "{}{}", String::from_utf8_lossy(&cloned.stdout), String::from_utf8_lossy(&cloned.stderr));
    let error = &serde_json::from_slice::<serde_json::Value>(&cloned.stdout).unwrap()["error"];
    assert_eq!(error["code"], "withheld_damaged", "{}", error);
    let next_step = error["next_step"].as_str().unwrap();
    assert!(next_step.starts_with("Ask the remote's operator"), "{}", error);
    assert!(next_step.contains("forklift-server withhold repair --root <warehouse>"), "{}", error);

    // The repair refuses while the root is served, then keeps the film withheld under a placeholder.
    let refused = withhold(&root, &["repair"]);
    assert!(!refused.status.success(), "{:?}", refused);
    stop(server);
    let repaired = withhold(&root, &["repair"]);
    assert!(repaired.status.success(), "{}", String::from_utf8_lossy(&repaired.stderr));
    let report = String::from_utf8_lossy(&repaired.stdout).into_owned();
    assert!(report.contains(&format!("forklift-server withhold add {} <statement>", film)), "{}", report);

    let server = Server::serve(&root);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let state = loop {
        let (status, info) = http(&server.url, "GET", "/v1/warehouse", b"");
        // The deletion drains the warehouse: a handshake meanwhile is refused for now.
        if status.starts_with("HTTP/1.1 200") {
            let info: serde_json::Value = serde_json::from_str(&info).unwrap();
            assert_eq!(info["withheld"][0]["hash"], film.as_str(), "{}", info);
            assert_eq!(info["withheld"][0]["statement"], forklift_core::util::withheld_repair::PLACEHOLDER_STATEMENT, "{}", info);
            if info["withheld"][0]["state"] == "deleted" {
                break info;
            }
        }
        assert!(std::time::Instant::now() < deadline, "timed out waiting for the deletion: {} / {}", status, info);
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let (status, body) = http(&server.url, "GET", &format!("/v1/objects/{}", film), b"");
    assert!(status.starts_with("HTTP/1.1 451"), "{} / {} after {}", status, body, state);

    // Lost again once its copy is deleted: the repair lists the content history names and lacks.
    stop(server);
    std::fs::remove_file(&record).unwrap();
    let repaired = withhold(&root, &["repair"]);
    assert!(repaired.status.success(), "{}", String::from_utf8_lossy(&repaired.stderr));
    let report = String::from_utf8_lossy(&repaired.stdout).into_owned();
    assert!(report.contains(&format!("  {}", film)), "{}", report);
}

/// A takedown deletes the copy on a served root, though the head still names the file: the server
/// notices the record the withhold command wrote and deletes the copy, then rebuilds the bundle
/// the takedown withdrew. One withheld while nothing serves the root is deleted, and the bundle
/// rebuilt, once it is served again.
///
/// Revert: never watch the record → the copy withheld while served stays `deletion_owed`; leave
/// the withheld hashes out of the deletion's targets → both stay; never rebuild on the first round
/// → no bundle is published after the restart.
#[test]
fn a_served_root_deletes_a_withheld_copy_and_rebuilds_its_bundle() {
    let root = std::env::temp_dir().join(format!("forklift-takedown-deletion-{}", std::process::id()));
    let serve = ["--rebuild-after-lifts", "1000"];
    // A killed server leaves its serve lock behind; no process holds it.
    let stop = |server: Server| {
        drop(server);
        forklift_core::util::file_utils::remove_file(root.join(".forklift/serve.lock")).unwrap();
    };
    stop(Server::start(&root));
    let server = Server::serve_with(&root, &serve);

    let owner = Warehouse::cloned("takedown-deletion-owner", &server.url);
    let (film, trailer) = (b"leaked film 2\n".as_slice(), b"leaked trailer\n".as_slice());
    owner.stack("film.mkv", film, "film");
    owner.stack("trailer.mkv", trailer, "trailer");
    owner.run_ok(&["lift"]);
    let head = server.main();
    let (film, trailer) = (hash_of(film), hash_of(trailer));
    let bundle = root.join(".forklift/bundles/latest");

    let state = |server: &Server, hash: &str| {
        let (status, info) = http(&server.url, "GET", "/v1/warehouse", b"");
        // The deletion drains the warehouse: a handshake meanwhile is refused for now.
        if status.starts_with("HTTP/1.1 503") {
            return None;
        }
        assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, info);
        let info: serde_json::Value = serde_json::from_str(&info).unwrap();
        info["withheld"].as_array().unwrap().iter()
            .find(|entry| entry["hash"] == hash).map(|entry| entry["state"].as_str().unwrap().to_string())
    };
    let wait_for = |what: &str, done: &dyn Fn() -> bool| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for {}", what);
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    };

    // Withheld while nothing serves the root: its first round deletes the copy and rebuilds.
    stop(server);
    let built = std::process::Command::new(common::server_binary()).env("FORKLIFT_FSYNC", "0")
        .args(["bundle", "--root", root.to_str().unwrap()]).output().unwrap();
    assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));
    let added = withhold(&root, &["add", &trailer, "--statement", "court order 11"]);
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    assert!(!bundle.exists(), "the takedown withdraws the bundle");

    let server = Server::serve_with(&root, &serve);
    wait_for("the unserved takedown's deletion", &|| state(&server, &trailer).as_deref() == Some("deleted"));
    wait_for("the rebuild after it", &|| bundle.exists());

    // Withheld while served: the watch notices, deletes the copy and rebuilds again.
    let rebuilt = std::fs::metadata(&bundle).unwrap().modified().unwrap();
    let added = withhold(&root, &["add", &film, "--statement", "DMCA notice 2026-118"]);
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    wait_for("the served takedown's deletion", &|| state(&server, &film).as_deref() == Some("deleted"));
    wait_for("the rebuild after it", &|| {
        std::fs::metadata(&bundle).ok().and_then(|meta| meta.modified().ok()).is_some_and(|at| at != rebuilt)
    });

    // History is the warehouse's: the head naming both files stays, and both are still refused.
    assert_eq!(server.main(), head);
    for hash in [&film, &trailer] {
        let (status, body) = http(&server.url, "GET", &format!("/v1/objects/{}", hash), b"");
        assert!(status.starts_with("HTTP/1.1 451"), "{} / {}", status, body);
    }
}

/// A takedown of a chunked file withholds its recipe, and a served root serves the chunks only the
/// recipe names as absent until its deletion is done, though a head names the file and nothing is
/// applied; a chunk another file shares stays served. Once the store is free, a round deletes the
/// recipe and its own chunks, and keeps the shared ones.
///
/// Revert: judge the chunks by the applied redactions alone on `GET` → the own chunk is a `200`;
/// refuse a recipe's hash → the takedown fails; judge the chunks by the applied redactions alone
/// on upload-targets → the own chunk is answered present.
#[test]
fn a_served_root_withholds_a_chunked_file_and_the_chunks_only_it_names() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-takedown-chunked-server-{}", std::process::id())));

    let owner = Warehouse::cloned("takedown-chunked-owner", &server.url);

    // Above the chunking threshold, and varied, so the file is a recipe of several chunks; the
    // kept file shares its first half, so it names some of those chunks too.
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as u8
    };
    let content: Vec<u8> = (0..9 * 1024 * 1024).map(|_| next()).collect();
    let mut kept = content[..content.len() / 2].to_vec();
    kept.extend((0..5 * 1024 * 1024).map(|_| next()));

    std::fs::create_dir_all(owner.root.join("keep")).unwrap();
    std::fs::create_dir_all(owner.root.join("film")).unwrap();
    owner.stack("keep/kept.bin", &kept, "kept");
    owner.stack("film/film.mkv", &content, "film");
    owner.run_ok(&["lift"]);

    // The film's recipe and its chunks, split by whether the kept file names them too.
    let closure = |root: &str, path: &str| match forklift_core::util::tree_utils::collect_subtree_closure(root, path, usize::MAX).unwrap() {
        Some(forklift_core::util::tree_utils::SubtreeClosure::Complete(closure)) => closure,
        other => panic!("{:?}", other.is_some()),
    };
    let (recipe, own, shared): (String, Vec<String>, Vec<String>) = {
        let _scope = forklift_core::globals::StorageRootScope::enter(&owner.root);
        let root = object_utils::load_parcel(&owner.head("main")).unwrap().tree_hash;
        let recipes = |path: &str| -> Vec<String> {
            closure(&root, path).into_iter().filter(|hash| object_utils::recipe_chunk_hashes(hash).is_ok()).collect()
        };
        let recipe = recipes("film").pop().unwrap();
        let kept: std::collections::HashSet<String> = recipes("keep").iter()
            .flat_map(|hash| object_utils::recipe_chunk_hashes(hash).unwrap())
            .collect();
        let (own, shared) = object_utils::recipe_chunk_hashes(&recipe).unwrap().into_iter()
            .partition(|chunk| !kept.contains(chunk));
        (recipe, own, shared)
    };
    assert!(!own.is_empty() && !shared.is_empty(), "own {:?}, shared {:?}", own, shared);

    // A deletion attempt drains the warehouse briefly: a request it refuses is asked again.
    let ask = |method: &str, path: &str, body: &[u8]| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let (status, answer) = http(&server.url, method, path, body);
            if !status.starts_with("HTTP/1.1 503") || std::time::Instant::now() >= deadline {
                return (status, answer);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    };
    let withheld_state = || {
        let (status, info) = ask("GET", "/v1/warehouse", b"");
        assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, info);
        let info: serde_json::Value = serde_json::from_str(&info).unwrap();
        assert!(info["redactions"].as_array().is_none_or(|applied| applied.is_empty()), "{}", info);
        info["withheld"][0]["state"].as_str().unwrap_or_default().to_string()
    };
    let objects = server.root.join(".forklift/objects");
    let stored = |hash: &str| objects.join(&hash[..2]).join(&hash[2..]).exists();

    {
        // Another process using the server's store keeps the deletion owed.
        let _scope = forklift_core::globals::StorageRootScope::enter(&server.root);
        let _store_access = forklift_core::util::lock_utils::StoreAccessLock::try_shared().unwrap();

        let added = withhold(&server.root, &["add", &recipe, "--statement", "DMCA notice 2026-119"]);
        assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
        assert_eq!(withheld_state(), "deletion_owed");

        let (status, body) = ask("GET", &format!("/v1/objects/{}", recipe), b"");
        assert!(status.starts_with("HTTP/1.1 451"), "{} / {}", status, body);

        let (status, body) = ask("GET", &format!("/v1/objects/{}", own[0]), b"");
        assert!(status.starts_with("HTTP/1.1 404"), "{}, {} bytes", status, body.len());
        assert!(stored(&recipe) && stored(&own[0]), "the server still holds the recipe and chunk");

        let (status, _) = ask("GET", &format!("/v1/objects/{}", shared[0]), b"");
        assert!(status.starts_with("HTTP/1.1 200"), "{}", status);

        // A bundle of the film's own chunks holds none of them; one of the shared chunks holds them.
        let batch = |hashes: &[String]| {
            let (status, body) = ask("POST", "/v1/objects/batch", serde_json::json!({ "hashes": hashes }).to_string().as_bytes());
            assert!(status.starts_with("HTTP/1.1 200"), "{}", status);
            body.len()
        };
        assert!(batch(&own) < 1024, "the bundle holds the film's own chunks");
        assert!(batch(&shared) > 1024, "the bundle holds the shared chunks");

        // Missing lists the own chunk, never the withheld recipe.
        let asked = [own[0].clone(), shared[0].clone(), recipe.clone()];
        let (status, body) = ask("POST", "/v1/objects/missing", serde_json::json!({ "hashes": asked }).to_string().as_bytes());
        assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, body);
        let missing: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(missing["missing"], serde_json::json!([own[0]]), "{}", body);

        // Upload-targets answers the own chunk to be uploaded, as missing does, and the recipe
        // as withheld.
        let (status, body) = ask("POST", "/v1/objects/upload-targets", serde_json::json!({ "session": "s", "hashes": asked }).to_string().as_bytes());
        assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, body);
        let targets: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!((&targets["direct"], &targets["present"], &targets["withheld"]),
                   (&serde_json::json!([own[0]]), &serde_json::json!([shared[0]]), &serde_json::json!([recipe])), "{}", body);
    }

    // A ref write asks for a round, which deletes the content now the store is free.
    owner.stack("notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while withheld_state() != "deleted" {
        assert!(std::time::Instant::now() < deadline, "the deletion stayed owed");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(!stored(&recipe) && !stored(&own[0]), "the recipe and its own chunk are deleted");
    let (status, _) = ask("GET", &format!("/v1/objects/{}", shared[0]), b"");
    assert!(status.starts_with("HTTP/1.1 200"), "{}", status);
}

/// A move to a server whose host withholds content a cache leans on its origin for is refused
/// while the previous origin answers and does not withhold it, and the refusal names it. Once the previous origin
/// withholds it too, or no longer answers, the move goes ahead and reports the content as what the
/// new origin will not supply.
///
/// Revert: let the walk find a withheld file held by the new server → the first move goes through
/// reporting nothing withheld. Revert: lean only on the absent trees the walk from the roots met,
/// not on what is beneath them → the same. Revert: never read the previous origin's handshake (read every
/// previous origin as withholding) → the first move goes through. Revert: refuse over a previous
/// origin that does not answer → the last move is refused.
#[test]
fn a_move_to_an_origin_withholding_leaned_on_content_reads_the_previous_origin() {
    let temp = std::env::temp_dir();
    let first_root = temp.join(format!("forklift-takedown-move-server-{}", std::process::id()));
    let server = Server::start(&first_root);

    let owner = Warehouse::cloned("takedown-move-owner", &server.url);
    // A draft no cache fetches, so each leans on its origin for it, beneath two trees it lacks.
    owner.stack("docs/notes.txt", b"a draft the host takes down\n", "draft");
    owner.stack("docs/notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);
    let draft = hash_of(b"a draft the host takes down\n");
    let statement = "court order 2026-77";

    let refused_cache = Warehouse::cloned_with("takedown-move-cache", &server.url, &["--cache"]);
    let unanswered_cache = Warehouse::cloned_with("takedown-move-cache-2", &server.url, &["--cache"]);

    // A copy of the server's root whose host withholds the draft.
    let second_root = temp.join(format!("forklift-takedown-move-copy-{}", std::process::id()));
    let _ = forklift_core::util::file_utils::remove_dir_all(&second_root);
    let copied = std::process::Command::new("cp").arg("-R").arg(&first_root).arg(&second_root).output().unwrap();
    assert!(copied.status.success(), "{:?}", copied);
    // The lock the first server holds is not the copy's.
    forklift_core::util::file_utils::remove_file(second_root.join(".forklift/serve.lock")).unwrap();
    let added = withhold(&second_root, &["add", &draft, "--statement", statement]);
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    let second = Server::serve(&second_root);

    let refused = refused_cache.json(&["remote", "move", &second.url]);
    assert_eq!(refused["ok"], false, "{}", refused);
    assert_eq!(refused["error"]["code"], "origin_move_incomplete", "{}", refused);
    assert!(refused["error"]["message"].as_str().unwrap().contains(&format!("current origin \"{}\" does not withhold", server.url)), "{}", refused);
    assert_eq!(refused["error"]["details"]["withheld"], serde_json::json!([{ "hash": draft, "statement": statement }]), "{}", refused);
    assert_eq!(refused_cache.json(&["remote"])["data"]["origin"], server.url.as_str());

    // The previous origin's host withholds it too.
    let added = withhold(&server.root, &["add", &draft, "--statement", statement]);
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    let moved = refused_cache.json(&["remote", "move", &second.url]);
    assert_eq!(moved["ok"], true, "{}", moved);
    assert_eq!(moved["data"]["origin"], second.url.as_str(), "{}", moved);
    assert_eq!(moved["data"]["withheld"], serde_json::json!([{ "hash": draft, "statement": statement }]), "{}", moved);
    assert_eq!(moved["data"]["previous_origin_withheld"], "withholds", "{}", moved);

    // The previous origin no longer answers.
    drop(server);
    let moved = unanswered_cache.json(&["remote", "move", &second.url]);
    assert_eq!(moved["ok"], true, "{}", moved);
    assert_eq!(moved["data"]["withheld"], serde_json::json!([{ "hash": draft, "statement": statement }]), "{}", moved);
    assert_eq!(moved["data"]["previous_origin_withheld"], "unreachable", "{}", moved);
    assert!(moved["data"]["previous_origin_unreachable"].is_string(), "{}", moved);
}

/// A cache keeps its copy of content its origin's host withholds: an eviction counts it kept and
/// leaves it readable, where it frees the copy of content the origin still serves. A read of
/// withheld content the cache never fetched is refused as the origin answered it, withheld.
///
/// Revert: read no withheld list from the handshake → the eviction frees the film and counts
/// nothing kept. Revert: word the miss hook's failed fetch as plain text → the draft's read is
/// refused as `error`.
#[test]
fn a_cache_keeps_what_its_origin_withholds_and_a_miss_on_it_is_withheld() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-takedown-evict-server-{}", std::process::id())));
    let film = b"leaked film\n";
    let draft = b"a draft the host takes down\n";

    let owner = Warehouse::cloned("takedown-evict-owner", &server.url);
    owner.stack("film.mkv", film, "film");
    owner.stack("trailer.mkv", b"trailer\n", "trailer");
    owner.stack("docs/notes.txt", draft, "draft");
    owner.run_ok(&["lift"]);
    let old = server.main();
    owner.stack("film.mkv", b"poster\n", "poster");
    owner.stack("trailer.mkv", b"teaser\n", "teaser");
    owner.stack("docs/notes.txt", b"notes\n", "notes");
    owner.run_ok(&["lift"]);

    // The cache reads the old film and trailer, so it holds copies its checkout does not need.
    let cache = Warehouse::cloned_with("takedown-evict-cache", &server.url, &["--cache"]);
    for path in ["film.mkv", "trailer.mkv"] {
        cache.run_ok(&["show", &format!("{}:{}", old, path)]);
    }
    let (film_hash, trailer_hash) = (hash_of(film), hash_of(b"trailer\n"));
    assert!(cache.holds_loose(&film_hash) && cache.holds_loose(&trailer_hash));

    let statement = "DMCA notice 2026-118 from Example Studios";
    for hash in [&film_hash, &hash_of(draft)] {
        let added = withhold(&server.root, &["add", hash, "--statement", statement]);
        assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    }

    let evicted = cache.json(&["store", "evict"]);
    assert_eq!(evicted["ok"], true, "{}", evicted);
    assert!(evicted["data"]["kept_withheld"].as_u64().unwrap() >= 1, "{}", evicted);
    assert!(cache.holds_loose(&film_hash), "the withheld copy is kept: {}", evicted);
    assert!(!cache.holds_loose(&trailer_hash), "a served copy is freed: {}", evicted);
    let shown = cache.run_ok(&["show", &format!("{}:film.mkv", old)]);
    assert_eq!(shown.stdout, film);

    let refused = cache.json(&["show", &format!("{}:docs/notes.txt", old)]);
    assert_eq!(refused["ok"], false, "{}", refused);
    assert_eq!(refused["error"]["code"], "content_withheld", "{}", refused);
    assert!(refused["error"]["message"].as_str().unwrap().contains(statement), "{}", refused);
    assert!(!cache.holds_loose(&hash_of(draft)));
}

/// A sparse warehouse's prune keeps its copy of content the remote's host withholds, and the
/// folder above it, where it frees what the remote still serves.
///
/// Revert: pass the prune no withheld list → the film is freed and nothing is counted kept.
#[test]
fn a_prune_keeps_what_the_remote_withholds() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-takedown-prune-server-{}", std::process::id())));
    let film = b"leaked film\n";

    let owner = Warehouse::cloned("takedown-prune-owner", &server.url);
    owner.stack("media/film.mkv", film, "film");
    owner.stack("media/trailer.mkv", b"trailer\n", "trailer");
    owner.stack("src/keep.txt", b"keep\n", "keep");
    owner.run_ok(&["lift"]);

    let sparse = Warehouse::cloned_with("takedown-prune-sparse", &server.url, &["--only", "media", "--only", "src"]);
    let (film_hash, trailer_hash) = (hash_of(film), hash_of(b"trailer\n"));
    assert!(sparse.holds_loose(&film_hash) && sparse.holds_loose(&trailer_hash));

    let added = withhold(&server.root, &["add", &film_hash, "--statement", "DMCA notice 2026-119"]);
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));

    sparse.run_ok(&["scope", "narrow", "media"]);
    let pruned = sparse.json(&["scope", "prune", "--confirm", "media"]);
    assert_eq!(pruned["ok"], true, "{}", pruned);
    assert_eq!(pruned["data"]["kept_withheld"], 3, "the film and both media folders naming it: {}", pruned);
    assert!(sparse.holds_loose(&film_hash), "the withheld copy is kept: {}", pruned);
    assert!(!sparse.holds_loose(&trailer_hash), "a served copy is freed: {}", pruned);
}

/// A remote holding `media/film.mkv`, `media/trailer.mkv` and `src/code.txt` at `old`; once
/// `withhold_new_film` runs, the remote's head changes the film and its host withholds the new
/// version, whose hash (a recipe when chunked) is `film`.
struct Takedown {
    server: Server,
    owner: Warehouse,
    old: String,
    film: String,
}

/// The statement `withhold_new_film` withholds the film on.
const TAKEDOWN: &str = "DMCA notice 2026-120 from Example Studios";

fn takedown(name: &str) -> Takedown {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-takedown-{}-server-{}", name, std::process::id())));

    let owner = Warehouse::cloned(&format!("takedown-{}-owner", name), &server.url);
    owner.run_ok(&["office", "enroll"]);
    owner.stack("media/film.mkv", b"the old film\n", "old film");
    owner.stack("media/trailer.mkv", b"trailer\n", "trailer");
    owner.stack("src/code.txt", b"code\n", "code");
    owner.run_ok(&["lift"]);
    let old = server.main();

    Takedown { server, owner, old, film: String::new() }
}

impl Takedown {
    /// Lift a new version of the film and withhold it on the remote, waiting until its copy there
    /// is deleted.
    fn withhold_new_film(&mut self, chunked: bool) {
        // Above the chunking threshold, and varied, so the film is a recipe of several chunks.
        let film: Vec<u8> = match chunked {
            true => {
                let mut state: u64 = 0x2545_f491_4f6c_dd1d;
                (0..9 * 1024 * 1024).map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                }).collect()
            }
            false => b"the new film\n".to_vec(),
        };
        self.owner.stack("media/film.mkv", &film, "new film");
        self.owner.run_ok(&["lift"]);
        self.film = {
            let _scope = forklift_core::globals::StorageRootScope::enter(&self.owner.root);
            let root = object_utils::load_parcel(&self.owner.head("main")).unwrap().tree_hash;
            let media = forklift_core::util::tree_utils::resolve_subtree_hash(&root, "media").unwrap().unwrap();
            let tree = object_utils::load_tree(&media).unwrap();
            tree.get_files().find(|(name, _)| name.as_str() == "film.mkv").unwrap().1.hash.clone()
        };

        self.withhold_film();
    }

    /// Withhold `film` on the remote, waiting until its copy there is deleted.
    fn withhold_film(&self) {
        let added = withhold(&self.server.root, &["add", &self.film, "--statement", TAKEDOWN]);
        assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
        // The deletion drains the warehouse briefly; wait until it is done.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let (status, info) = http(&self.server.url, "GET", "/v1/warehouse", b"");
            if status.starts_with("HTTP/1.1 200") {
                let info: serde_json::Value = serde_json::from_str(&info).unwrap();
                if info["withheld"][0]["state"] == "deleted" {
                    break;
                }
            }
            assert!(std::time::Instant::now() < deadline, "the takedown's deletion stayed owed");
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

impl Takedown {
    /// A clone of the remote that stacks as the owner does, from a copy of the owner's keys: the
    /// owner's work on a second machine.
    fn writer(&self, name: &str, extra: &[&str]) -> Warehouse {
        let warehouse = Warehouse::cloned_with(name, &self.server.url, extra);
        let keys = self.owner.home.join("keys");
        for entry in walk_files(&keys) {
            let target = warehouse.home.join("keys").join(entry.strip_prefix(&keys).unwrap());
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::copy(&entry, &target).unwrap();
        }

        warehouse
    }
}

/// Every file under `dir`, at any depth.
fn walk_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        match path.is_dir() {
            true => files.extend(walk_files(&path)),
            false => files.push(path),
        }
    }

    files
}

/// The film is left out of `warehouse`: no file at its path, no copy in the store, the unavailable
/// record names it with the host's statement, stocktake reports it and nothing else, and the full
/// audit passes; the trailer beside it is checked out.
fn left_out_film(warehouse: &Warehouse, takedown: &Takedown) {
    assert!(!warehouse.root.join("media/film.mkv").exists(), "the withheld film is not checked out");
    assert_eq!(std::fs::read(warehouse.root.join("media/trailer.mkv")).unwrap(), b"trailer\n");
    recorded_film(warehouse, takedown);

    let report = warehouse.json(&["stocktake"]);
    assert_eq!(report["ok"], true, "{}", report);
    assert_eq!(report["data"]["unavailable"], serde_json::json!(["media/film.mkv"]), "{}", report);
    assert_eq!((&report["data"]["staged_count"], &report["data"]["unstaged_count"]),
        (&0.into(), &0.into()), "{}", report);
}

/// The store holds no copy of the film, the unavailable record names it with the host's
/// statement, and the full audit passes.
fn recorded_film(warehouse: &Warehouse, takedown: &Takedown) {
    assert!(!warehouse.holds_loose(&takedown.film), "the store holds no copy of the withheld film");

    let record: Vec<(String, String)> = {
        let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);
        forklift_core::util::unavailable::read_unavailable().unwrap().entries()
            .map(|(hash, statement)| (hash.clone(), statement.clone())).collect()
    };
    assert_eq!(record, [(takedown.film.clone(), TAKEDOWN.to_string())]);

    let audited = warehouse.json(&["audit", "--full"]);
    assert_eq!(audited["ok"], true, "{}", audited);
}

/// A full clone, a cache clone and a sparse clone of a head naming content the remote's host
/// withholds check the head out without it: the head is the remote's, the file is left out and
/// recorded unavailable, and the clone is clean. A sparse clone whose scope leaves the file out
/// records nothing, and widening the scope over it records it then.
///
/// Revert: leave the withheld hashes out of the fetch's skip set → every clone is refused with
/// `content_withheld`. Revert: record nothing after the fetch → the full clone's head check is
/// refused as incomplete. Revert: refuse what the fetch left out as withheld as an unapplied
/// redaction → the full clone is refused as redacted. Revert: open the scope fetch without the remote's withheld list → the
/// widening is refused with `content_withheld`.
#[test]
fn a_clone_checks_out_a_head_naming_withheld_content_without_it() {
    let mut takedown = takedown("clone");
    takedown.withhold_new_film(false);
    let head = takedown.server.main();

    for (name, extra) in [
        ("takedown-clone-full", &[][..]),
        ("takedown-clone-cache", &["--cache"][..]),
        ("takedown-clone-sparse", &["--only", "media"][..]),
    ] {
        let clone = Warehouse::cloned_with(name, &takedown.server.url, extra);
        assert_eq!(clone.head("main"), head, "{}", name);
        left_out_film(&clone, &takedown);
    }

    let narrow = Warehouse::cloned_with("takedown-clone-narrow", &takedown.server.url, &["--only", "src"]);
    let record = narrow.root.join(".forklift/unavailable");
    assert!(!record.exists() || {
        let _scope = forklift_core::globals::StorageRootScope::enter(&narrow.root);
        forklift_core::util::unavailable::read_unavailable().unwrap().is_empty()
    }, "a scope leaving the film out records nothing");

    // Widening fetches, and leaves the checkout's scope as it is.
    let widened = narrow.json(&["scope", "fetch", "media"]);
    assert_eq!(widened["ok"], true, "{}", widened);
    assert!(narrow.holds_loose(&hash_of(b"trailer\n")), "the widening fetched the trailer");
    recorded_film(&narrow, &takedown);
}

/// A lower to a head changing a file to content the remote's host withholds removes the old
/// version and leaves the new one out, recorded unavailable, in a full warehouse and in a cache.
/// The film is chunked, so a cache's presence phase meets an absent recipe it must not read.
///
/// Revert: let a cache's presence phase fetch what the unavailable record lists → the
/// cache's lower fails reading the absent recipe.
#[test]
fn a_lower_to_a_head_naming_withheld_content_takes_it_without_it() {
    let mut takedown = takedown("lower");
    // Cloned while the remote serves the old head: the bay holds the old film.
    let warehouses = [("takedown-lower-full", &[][..]), ("takedown-lower-cache", &["--cache"][..])]
        .map(|(name, extra)| (name, Warehouse::cloned_with(name, &takedown.server.url, extra)));
    takedown.withhold_new_film(true);
    let head = takedown.server.main();

    for (name, warehouse) in &warehouses {
        assert_eq!(std::fs::read(warehouse.root.join("media/film.mkv")).unwrap(), b"the old film\n");

        let lowered = warehouse.json(&["lower"]);
        assert_eq!(lowered["ok"], true, "{}: {}", name, lowered);
        assert_eq!(warehouse.head("main"), head, "{}", name);
        left_out_film(&warehouse, &takedown);
    }
}

/// A lower to a head naming withheld content, interrupted after its ref write and before it
/// removed the old version, is finished by heal without the content: the head stays the remote's,
/// the old version is removed, and the film stays recorded unavailable rather than taken for a
/// removal or fetched.
///
/// Revert: judge a left-out write in heal's put-back by the rebuild's unheld files, as any other
/// write → the old film stays.
#[test]
fn heal_finishes_an_interrupted_lower_to_a_head_naming_withheld_content() {
    use forklift_core::util::{audit_utils, operation_utils, pallet_utils};
    use forklift_core::util::operation_utils::{Operation, OperationKind};
    use forklift_core::util::pallet_utils::{BayPosition, RefPreconditions};

    let mut takedown = takedown("heal");
    let warehouse = Warehouse::cloned("takedown-heal", &takedown.server.url);
    takedown.withhold_new_film(false);
    warehouse.run_ok(&["lower"]);
    let (from, to) = (takedown.old.clone(), takedown.server.main());
    assert_eq!(warehouse.head("main"), to);

    {
        let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);
        let (_, office) = audit_utils::verified_local_trust().unwrap().expect("the clone holds an anchor");
        pallet_utils::update_pallet_head("main", Some(&to), &from, &RefPreconditions::NONE).unwrap();
        pallet_utils::write_position(&BayPosition::new("main", Some(from.clone()))).unwrap();

        let mut operation = Operation::new(
            OperationKind::Lower,
            BayPosition::new("main", Some(from.clone())),
            Some(object_utils::load_parcel(&from).unwrap().tree_hash),
            BayPosition::new("main", Some(to.clone())),
        );
        operation.target_tree = Some(object_utils::load_parcel(&to).unwrap().tree_hash);
        operation.expected = Some(from.clone());
        let preconditions = office.preconditions();
        operation.office = preconditions.office;
        operation.anchor = preconditions.anchor;
        operation_utils::raise(&operation).unwrap();
        operation_utils::forget_raised_for_tests();
    }

    // The crash came before the lower removed the old version.
    std::fs::write(warehouse.root.join("media/film.mkv"), b"the old film\n").unwrap();

    let healed = warehouse.json(&["heal"]);
    assert_eq!(healed["ok"], true, "{}", healed);
    assert_eq!(warehouse.head("main"), to);
    let standing = {
        let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);
        operation_utils::read().unwrap().is_some()
    };
    assert!(!standing, "heal finished the lower");
    left_out_film(&warehouse, &takedown);
}

/// A lower whose bay has diverged from a remote head changing a file to content the remote's host
/// withholds merges that head in without it, in a full warehouse and in a cache: the merge parcel
/// has both heads as parents, the local change stays, the old version is removed and the new one
/// is left out, recorded unavailable. The film is chunked, so a cache's presence phase meets an
/// absent recipe it must not read.
///
/// Revert: let the merge take a withheld file as any other TakeTheirs → the merge fails reading
/// the absent content.
#[test]
fn a_diverged_lower_merges_in_a_head_naming_withheld_content_without_it() {
    let mut takedown = takedown("diverged-lower");
    let warehouses = [("takedown-diverged-full", &[][..]), ("takedown-diverged-cache", &["--cache"][..])]
        .map(|(name, extra)| (name, takedown.writer(name, extra)));
    for (_, warehouse) in &warehouses {
        warehouse.stack("src/local.txt", b"local\n", "local change");
    }
    takedown.withhold_new_film(true);
    let theirs = takedown.server.main();

    for (name, warehouse) in &warehouses {
        let ours = warehouse.head("main");
        let lowered = warehouse.json(&["lower"]);
        assert_eq!(lowered["ok"], true, "{}: {}", name, lowered);
        assert_eq!(lowered["data"]["outcome"], "merged", "{}: {}", name, lowered);

        let merged = warehouse.head("main");
        let parents = {
            let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);
            object_utils::load_parcel(&merged).unwrap().parents
        };
        assert_eq!(parents, [ours, theirs.clone()], "{}", name);
        assert_eq!(std::fs::read(warehouse.root.join("src/local.txt")).unwrap(), b"local\n", "{}", name);
        left_out_film(warehouse, &takedown);
    }
}

/// A lift whose remote has diverged to a head changing a file to content the remote's host
/// withholds auto-merges that head in without it and lifts the merge: the remote's head is the
/// merge parcel, and the new film is left out of this warehouse, recorded unavailable.
///
/// Revert: drop the lift's opt-in to leaving withheld content out → the auto-merge's fetch is
/// refused with `content_withheld`.
#[test]
fn a_lift_auto_merges_a_head_naming_withheld_content_without_it() {
    let mut takedown = takedown("lift-merge");
    let warehouse = takedown.writer("takedown-lift-merge", &[]);
    warehouse.stack("src/local.txt", b"local\n", "local change");
    takedown.withhold_new_film(false);
    let (ours, theirs) = (warehouse.head("main"), takedown.server.main());

    let lifted = warehouse.json(&["lift"]);
    assert_eq!(lifted["ok"], true, "{}", lifted);
    assert_eq!(lifted["data"]["auto_merged"], 1, "{}", lifted);

    let merged = warehouse.head("main");
    assert_eq!(takedown.server.main(), merged, "the merge is lifted");
    let parents = {
        let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);
        object_utils::load_parcel(&merged).unwrap().parents
    };
    assert_eq!(parents, [ours, theirs]);
    assert_eq!(std::fs::read(warehouse.root.join("src/local.txt")).unwrap(), b"local\n");
    left_out_film(&warehouse, &takedown);
}

/// A lift whose auto-merge would have to read content the remote's host withholds refuses with
/// `content_withheld`, naming the file and the host's statement, rather than the diverged error,
/// whose way out (a lower) would meet the same refusal; nothing is stacked or lifted, the bay's
/// file is as it was, and no operation is left standing.
///
/// Revert: drop the auto-merge client's opt-in → the fetch is refused instead, naming no file.
#[test]
fn a_lift_whose_merge_must_read_withheld_content_refuses_it() {
    let mut takedown = takedown("lift-overlap");
    let warehouse = takedown.writer("takedown-lift-overlap", &[]);
    warehouse.stack("media/film.mkv", b"our film\n", "our film");
    takedown.withhold_new_film(false);
    let (ours, theirs) = (warehouse.head("main"), takedown.server.main());

    let lifted = warehouse.json(&["lift"]);
    assert_eq!(lifted["ok"], false, "{}", lifted);
    assert_eq!(lifted["error"]["code"], "content_withheld", "{}", lifted);
    assert_eq!(lifted["error"]["details"]["path"], "media/film.mkv", "{}", lifted);
    assert!(lifted["error"]["message"].as_str().unwrap().contains(TAKEDOWN), "{}", lifted);
    assert_eq!(warehouse.head("main"), ours, "no merge is stacked");
    assert_eq!(takedown.server.main(), theirs, "nothing is lifted");
    assert_eq!(std::fs::read(warehouse.root.join("media/film.mkv")).unwrap(), b"our film\n");

    let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);
    forklift_core::util::operation_utils::check_none_standing().unwrap();
}

/// Heal fetches a damaged object back from a remote whose host withholds other content, and
/// records none of that content unavailable: heal repairs this warehouse's own store, and a
/// withholding it recorded could stand in for the very damage it was asked to repair.
///
/// Revert: opt heal's refetch in to leaving withheld content out → the film is recorded.
#[test]
fn heal_fetches_a_damaged_object_back_without_recording_withheld_content() {
    let mut takedown = takedown("heal-refetch");
    let warehouse = Warehouse::cloned("takedown-heal-refetch", &takedown.server.url);
    takedown.withhold_new_film(false);

    let trailer = hash_of(b"trailer\n");
    warehouse.remove_object(&trailer);
    let taint = forklift_core::util::taint_utils::taint_dir_path_under(&warehouse.root);
    std::fs::create_dir_all(&taint).unwrap();
    let relative = format!("objects/{}/{}", &trailer[..2], &trailer[2..]);
    std::fs::write(taint.join("taint-1-0"), forklift_core::util::taint_utils::taint_file_bytes(&[&relative])).unwrap();

    let healed = warehouse.json(&["heal"]);
    assert_eq!(healed["ok"], true, "{}", healed);
    assert!(warehouse.holds_loose(&trailer), "the damaged object is fetched back");
    assert_eq!(warehouse.head("main"), takedown.old, "heal moves no ref");
    assert!(!warehouse.holds_loose(&takedown.film), "the store holds no copy of the withheld film");

    let record = {
        let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);
        forklift_core::util::unavailable::read_unavailable().unwrap().entries().count()
    };
    assert_eq!(record, 0, "heal records no withheld content unavailable");
}

/// A warehouse that held content its remote's host now withholds, and lost its copy, does not ask
/// the remote for it: heal reports it still referenced with the host's statement and says no
/// re-run will bring it back, records nothing unavailable, and still fetches back the damaged
/// object beside it.
///
/// Revert: ask the remote for every candidate, withheld or not → the remote refuses the film, and
/// its remedy tells the user to re-run once connectivity is restored, without the statement.
#[test]
fn heal_does_not_ask_for_content_the_remote_withholds_and_says_so() {
    let mut takedown = takedown("heal-withheld-candidate");
    takedown.withhold_new_film(false);
    let owner = &takedown.owner;
    assert!(owner.holds_loose(&takedown.film), "the owner held the film before the takedown");

    let trailer = hash_of(b"trailer\n");
    owner.remove_object(&takedown.film);
    owner.remove_object(&trailer);
    let taint = forklift_core::util::taint_utils::taint_dir_path_under(&owner.root);
    std::fs::create_dir_all(&taint).unwrap();
    let relative = |hash: &str| format!("objects/{}/{}", &hash[..2], &hash[2..]);
    std::fs::write(taint.join("taint-1-0"),
        forklift_core::util::taint_utils::taint_file_bytes([&relative(&takedown.film), &relative(&trailer)])).unwrap();

    let healed = owner.json(&["heal"]);
    assert_eq!(healed["ok"], false, "{}", healed);
    let message = healed["error"]["message"].as_str().unwrap();
    assert!(message.contains(&format!("vanished and still referenced: \"{}\"", takedown.film)), "{}", message);
    assert!(message.contains(&format!("withholds this content (\"{}\")", TAKEDOWN)), "{}", message);
    assert!(message.contains("a re-run will not either"), "{}", message);
    assert!(!message.contains("connectivity"), "{}", message);
    assert!(owner.holds_loose(&trailer), "the damaged object beside it is fetched back");

    let record = {
        let _scope = forklift_core::globals::StorageRootScope::enter(&owner.root);
        forklift_core::util::unavailable::read_unavailable().unwrap().entries().count()
    };
    assert_eq!(record, 0, "heal records no withheld content unavailable");
}

/// A lift to a remote other than the one whose host withholds the film, of a head naming it,
/// refuses `content_withheld` with the hash and the host's statement: this warehouse cannot upload
/// content it was never given, and the other remote's `main` does not move.
///
/// Revert: drop the read path's unavailable arm → the upload's read of the film fails as a raw
/// missing object, with no code → red.
#[test]
fn a_lift_to_another_remote_refuses_content_withheld_here() {
    let mut takedown = takedown("lift-elsewhere");
    takedown.withhold_new_film(false);
    let writer = takedown.writer("takedown-lift-elsewhere-writer", &[]);
    left_out_film(&writer, &takedown);

    let other = Server::start(&std::env::temp_dir()
        .join(format!("forklift-takedown-lift-elsewhere-other-{}", std::process::id())));
    writer.run_ok(&["config", "set", "remote.url", &other.url]);

    let lifted = writer.json(&["lift"]);
    assert_eq!(lifted["error"]["code"], "content_withheld", "{}", lifted);
    assert_eq!(lifted["error"]["details"]["hash"], takedown.film.as_str(), "{}", lifted);
    assert_eq!(lifted["error"]["details"]["statement"], TAKEDOWN, "{}", lifted);
    assert!(!other.root.join(".forklift/pallets/main").exists(), "the other remote's main moved: {}", lifted);
}

/// `git export` in a warehouse that lacks content the host of its remote withholds leaves the file
/// out of every commit whose tree names that content, never writing anything in its place, and
/// names it with the host's statement and the parcels whose commits left it out. An earlier version
/// of the file, which this warehouse holds, is exported as before.
///
/// Revert: export the file as before → the export refuses `content_withheld`, reading the absent
/// film.
#[test]
fn git_export_leaves_out_a_file_whose_content_the_remote_withholds_and_names_it() {
    let mut takedown = takedown("export");
    takedown.withhold_new_film(false);
    let head = takedown.server.main();
    let clone = Warehouse::cloned("takedown-export", &takedown.server.url);
    left_out_film(&clone, &takedown);

    let target = clone.root.parent().unwrap().join("exported");
    let exported = clone.json(&["git", "export", target.to_str().unwrap()]);
    assert_eq!(exported["ok"], true, "{}", exported);
    assert_eq!(exported["data"]["withheld"], serde_json::json!([
        { "path": "media/film.mkv", "hash": takedown.film, "statement": TAKEDOWN, "parcels": [head] }
    ]), "{}", exported);
    assert_eq!(exported["data"]["redacted"], serde_json::Value::Null, "{}", exported);

    let paths = git_paths(&target, "main");
    assert!(!paths.contains(&"media/film.mkv".to_string()), "{:?}", paths);
    assert!(paths.contains(&"media/trailer.mkv".to_string()), "{:?}", paths);
    assert!(git_paths(&target, "main~1").contains(&"media/film.mkv".to_string()), "the old film is exported");

    let human = String::from_utf8_lossy(&clone.run_ok(&["git", "export", target.with_extension("2").to_str().unwrap()]).stdout)
        .into_owned();
    assert!(human.contains("Left out \"media/film.mkv\""), "{}", human);
    assert!(human.contains(&format!("The host's statement: {}", TAKEDOWN)), "{}", human);
}

/// `git export` in a served root leaves out a file whose content that root's own host withholds,
/// whether the store still holds the copy (withheld while the server was not serving, so nothing
/// deleted it yet) or the served root has deleted it, and names it with the host's statement. The
/// earlier version of the file is exported as before, and nothing reads as redacted or as withheld
/// by a remote's host.
///
/// Revert: leave the withheld record out of the export → the stored copy's export names nothing
/// withheld here, and the deleted one fails the export as a missing object.
#[test]
fn git_export_in_a_served_root_leaves_out_content_its_host_withholds() {
    for deleted in [false, true] {
        let name = if deleted { "export-here-deleted" } else { "export-here-stored" };
        let mut takedown = takedown(name);
        let head = match deleted {
            true => {
                takedown.withhold_new_film(false);
                takedown.server.main()
            }
            false => {
                takedown.owner.stack("media/film.mkv", b"the new film\n", "new film");
                takedown.owner.run_ok(&["lift"]);
                let head = takedown.server.main();
                let _ = takedown.server.child.kill();
                let _ = takedown.server.child.wait();
                takedown.film = hash_of(b"the new film\n");
                let added = withhold(&takedown.server.root, &["add", &takedown.film, "--statement", TAKEDOWN]);
                assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
                head
            }
        };
        let served = Warehouse { root: takedown.server.root.clone(), home: takedown.owner.home.clone() };
        assert_eq!(served.holds_loose(&takedown.film), !deleted, "the fixture's copy is {}",
            if deleted { "deleted" } else { "stored" });

        let target = takedown.server.root.parent().unwrap().join(format!("{}-git", name));
        let _ = forklift_core::util::file_utils::remove_dir_all(&target);
        let _ = forklift_core::util::file_utils::remove_dir_all(target.with_extension("2"));
        let exported = served.json(&["git", "export", target.to_str().unwrap()]);
        assert_eq!(exported["ok"], true, "{}", exported);
        assert_eq!(exported["data"]["withheld_here"], serde_json::json!([
            { "path": "media/film.mkv", "hash": takedown.film, "statement": TAKEDOWN, "parcels": [head] }
        ]), "{}", exported);
        assert_eq!(exported["data"]["withheld"], serde_json::Value::Null, "{}", exported);
        assert_eq!(exported["data"]["redacted"], serde_json::Value::Null, "{}", exported);

        let paths = git_paths(&target, "main");
        assert!(!paths.contains(&"media/film.mkv".to_string()), "{:?}", paths);
        assert!(paths.contains(&"media/trailer.mkv".to_string()), "{:?}", paths);
        assert!(git_paths(&target, "main~1").contains(&"media/film.mkv".to_string()), "the old film is exported");

        let human = String::from_utf8_lossy(&served.run_ok(&["git", "export", target.with_extension("2").to_str().unwrap()]).stdout)
            .into_owned();
        assert!(human.contains("Left out \"media/film.mkv\""), "{}", human);
        assert!(human.contains("the host of this warehouse withholds its content"), "{}", human);
        assert!(human.contains(&format!("The host's statement: {}", TAKEDOWN)), "{}", human);
    }
}

/// `git mirror` run in a served root, whose remote is the server itself, leaves out of the commit
/// it pushes a file whose content that root's own host withholds, once the served root has deleted
/// the copy, and names it with the host's statement. The commit an earlier run wrote before the
/// withholding began keeps the file, and its folder is not reused for a later parcel that leaves
/// the folder as it was.
///
/// Revert: leave the withheld record out of the mirror's exporter → the mirror fails, reading the
/// deleted film. Revert: judge a seeded folder clean by the applied redactions alone → the later
/// run names nothing.
#[test]
fn git_mirror_in_a_served_root_leaves_out_content_its_host_withholds() {
    let mut takedown = takedown("mirror-here");
    takedown.owner.stack("media/film.mkv", b"the new film\n", "new film");
    takedown.owner.run_ok(&["lift"]);
    takedown.film = hash_of(b"the new film\n");
    let served = Warehouse { root: takedown.server.root.clone(), home: takedown.owner.home.clone() };
    let git = takedown.server.root.parent().unwrap().join("forklift-takedown-mirror-here-git");
    let _ = forklift_core::util::file_utils::remove_dir_all(&git);
    let initialised = std::process::Command::new("git").args(["init", "-q", "--bare", git.to_str().unwrap()])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
        .output().unwrap();
    assert!(initialised.status.success(), "{}", String::from_utf8_lossy(&initialised.stderr));
    served.run_ok(&["config", "set", "remote.url", &takedown.server.url]);
    served.run_ok(&["config", "set", "mirror.url", git.to_str().unwrap()]);

    // Mirrored before the withholding: the commit holds the film.
    let mirrored = served.json(&["git", "mirror"]);
    assert_eq!(mirrored["ok"], true, "{}", mirrored);
    assert_eq!(mirrored["data"]["pallets"][0]["withheld_here"], serde_json::Value::Null, "{}", mirrored);
    assert!(git_paths(&git, "main").contains(&"media/film.mkv".to_string()));

    takedown.withhold_film();
    takedown.owner.stack("src/code.txt", b"code 2\n", "code 2");
    takedown.owner.run_ok(&["lift"]);
    let later = takedown.server.main();

    let mirrored = served.json(&["git", "mirror"]);
    assert_eq!(mirrored["ok"], true, "{}", mirrored);
    let main = &mirrored["data"]["pallets"][0];
    assert_eq!(main["status"], "pushed", "{}", mirrored);
    assert_eq!(main["withheld_here"], serde_json::json!([
        { "path": "media/film.mkv", "hash": takedown.film, "statement": TAKEDOWN, "parcels": [later] }
    ]), "{}", mirrored);
    let paths = git_paths(&git, "main");
    assert!(!paths.contains(&"media/film.mkv".to_string()), "{:?}", paths);
    assert!(paths.contains(&"media/trailer.mkv".to_string()), "{:?}", paths);
    assert!(git_paths(&git, "main~1").contains(&"media/film.mkv".to_string()), "the earlier commit is kept");
}

/// `git mirror` in a warehouse behind its remote takes the new history without the content the
/// remote's host withholds, recording it unavailable as a lower does, and its export leaves the file
/// out of the commit it pushes, naming it with the host's statement. The film is chunked, so what
/// is left out is a recipe. A later run, over a parcel that leaves the folder as it was, names the
/// file again.
///
/// Revert: open the mirror's walk without the remote's withheld list → the mirror is refused, its
/// pallet failing `content_withheld`. Revert: export the file as before → the pallet fails reading
/// the absent recipe. Revert: keep the record the run started with → the same. Revert: reuse a
/// seeded folder the mirror's withheld-trees log names → the later run names nothing.
#[test]
fn git_mirror_leaves_out_a_file_whose_content_the_remote_withholds_and_names_it() {
    let mut takedown = takedown("mirror");
    let clone = Warehouse::cloned("takedown-mirror", &takedown.server.url);
    let git = clone.root.parent().unwrap().join("mirror.git");
    let initialised = std::process::Command::new("git").args(["init", "-q", "--bare", git.to_str().unwrap()])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
        .output().unwrap();
    assert!(initialised.status.success(), "{}", String::from_utf8_lossy(&initialised.stderr));
    clone.run_ok(&["config", "set", "mirror.url", git.to_str().unwrap()]);
    takedown.withhold_new_film(true);
    let head = takedown.server.main();

    let mirrored = clone.json(&["git", "mirror"]);
    assert_eq!(mirrored["ok"], true, "{}", mirrored);
    let main = &mirrored["data"]["pallets"][0];
    assert_eq!(main["status"], "pushed", "{}", mirrored);
    assert_eq!(main["withheld"], serde_json::json!([
        { "path": "media/film.mkv", "hash": takedown.film, "statement": TAKEDOWN, "parcels": [head] }
    ]), "{}", mirrored);
    recorded_film(&clone, &takedown);

    let paths = git_paths(&git, "main");
    assert!(!paths.contains(&"media/film.mkv".to_string()), "{:?}", paths);
    assert!(paths.contains(&"media/trailer.mkv".to_string()), "{:?}", paths);

    // A later parcel leaves the folder as it was: the git folder the first run wrote is not
    // reused unnamed, so the film is named again, under the new parcel.
    takedown.owner.stack("src/code.txt", b"code 2\n", "code 2");
    takedown.owner.run_ok(&["lift"]);
    let later = takedown.server.main();
    let mirrored = clone.json(&["git", "mirror"]);
    assert_eq!(mirrored["ok"], true, "{}", mirrored);
    assert_eq!(mirrored["data"]["pallets"][0]["withheld"], serde_json::json!([
        { "path": "media/film.mkv", "hash": takedown.film, "statement": TAKEDOWN, "parcels": [later] }
    ]), "{}", mirrored);
    assert!(!git_paths(&git, "main").contains(&"media/film.mkv".to_string()));
}

/// Once the bytes of a file a mirror left out are back in the warehouse (put back and loaded,
/// which drops the unavailable record's entry), a later mirror run over a parcel that leaves the
/// folder as it was writes the file: the git folder the first run wrote without it is not reused,
/// although no record here still names the content. Nothing is reported left out.
///
/// Revert: reuse a seeded folder the mirror's withheld-trees log names → the later commit lacks
/// the film. Revert: never log a tree → the same.
#[test]
fn git_mirror_writes_a_file_it_left_out_once_its_bytes_are_back() {
    let mut takedown = takedown("mirror-back");
    let clone = Warehouse::cloned("takedown-mirror-back", &takedown.server.url);
    let git = clone.root.parent().unwrap().join("mirror.git");
    let initialised = std::process::Command::new("git").args(["init", "-q", "--bare", git.to_str().unwrap()])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
        .output().unwrap();
    assert!(initialised.status.success(), "{}", String::from_utf8_lossy(&initialised.stderr));
    clone.run_ok(&["config", "set", "mirror.url", git.to_str().unwrap()]);
    takedown.withhold_new_film(false);

    let mirrored = clone.json(&["git", "mirror"]);
    assert_eq!(mirrored["data"]["pallets"][0]["withheld"][0]["path"], "media/film.mkv", "{}", mirrored);
    assert!(!git_paths(&git, "main").contains(&"media/film.mkv".to_string()));

    // The bytes come back: put back at the path not checked out and loaded.
    std::fs::write(clone.root.join("media/film.mkv"), b"the new film\n").unwrap();
    clone.run_ok(&["load", "media/film.mkv"]);
    assert!(clone.holds_loose(&takedown.film), "the load stored the film");
    let record = {
        let _scope = forklift_core::globals::StorageRootScope::enter(&clone.root);
        forklift_core::util::unavailable::read_unavailable().unwrap().entries().count()
    };
    assert_eq!(record, 0, "the load dropped the record's entry");

    takedown.owner.stack("src/code.txt", b"code 2\n", "code 2");
    takedown.owner.run_ok(&["lift"]);
    let mirrored = clone.json(&["git", "mirror"]);
    assert_eq!(mirrored["ok"], true, "{}", mirrored);
    let main = &mirrored["data"]["pallets"][0];
    assert_eq!(main["status"], "pushed", "{}", mirrored);
    assert!(main["withheld"].is_null(), "{}", mirrored);

    let shown = std::process::Command::new("git").args(["show", "main:media/film.mkv"])
        .current_dir(&git)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
        .output().unwrap();
    assert!(shown.status.success(), "{}", String::from_utf8_lossy(&shown.stderr));
    assert_eq!(shown.stdout, b"the new film\n");
}

/// A chunk-list redaction in `@redactions` judges a withheld chunked file whose recipe the store
/// no longer holds by the chunk-list digest recorded for it: the host's, which it computed while it
/// held the recipe, and the one a clone's record copies from the host's report. Neither the
/// server's ref update over a tree naming the file nor a full clone fails on the missing recipe.
///
/// Revert: judge an unreadable recipe as before → the lift is refused (500) and the clone fails
/// on reading the film; or let the server leave the digest out of its report → the clone's record
/// carries none.
#[test]
fn a_chunk_list_redaction_judges_a_withheld_recipe_by_its_recorded_digest() {
    let mut takedown = takedown("chunkjudge");

    // A chunk-list redaction of an unrelated chunked secret, recorded while no head names it.
    let mut state: u64 = 0x1234_5678_9abc_def1;
    let secret: Vec<u8> = (0..9 * 1024 * 1024).map(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as u8
    }).collect();
    takedown.owner.run_ok(&["pallet", "create", "leakbox"]);
    takedown.owner.run_ok(&["shift", "leakbox"]);
    takedown.owner.stack("secret.bin", &secret, "leak");
    let leak = takedown.owner.head("leakbox");
    takedown.owner.run_ok(&["remove", "secret.bin"]);
    takedown.owner.run_ok(&["stack", "-m", "take the leak out"]);
    let recorded = recorded_hash(&takedown.owner, &format!("{}:secret.bin", leak), "leaked dump");
    assert_eq!(recorded["data"]["chunked"], true, "{}", recorded);
    assert_eq!(takedown.owner.redactions()[0]["chunks"].as_str().map(str::len), Some(64));
    let _ = forklift_core::util::file_utils::remove_file(takedown.owner.root.join("secret.bin"));
    takedown.owner.run_ok(&["shift", "main"]);
    takedown.owner.run_ok(&["lift"]);

    // The film is a chunked recipe, withheld while the server held it, then deleted there.
    takedown.withhold_new_film(true);
    let (status, info) = http(&takedown.server.url, "GET", "/v1/warehouse", b"");
    assert!(status.starts_with("HTTP/1.1 200"), "{}", status);
    let info: serde_json::Value = serde_json::from_str(&info).unwrap();
    let digest = info["withheld"][0]["chunks"].as_str().expect("the host reports the film's digest").to_string();
    assert_eq!(digest.len(), 64, "{}", info);

    takedown.owner.stack("notes.txt", b"notes\n", "notes");
    let lifted = takedown.owner.run(&["--json", "lift"]);
    assert!(lifted.status.success(), "{}", String::from_utf8_lossy(&lifted.stdout));
    assert_eq!(takedown.server.main(), takedown.owner.head("main"));

    let dir = std::env::temp_dir().join(format!("forklift-redaction-chunkjudge-writer-{}", std::process::id()));
    let _ = forklift_core::util::file_utils::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cloned = forklift_command()
        .args(["clone", &takedown.server.url, "warehouse"])
        .current_dir(&dir)
        .env("FORKLIFT_GLOBAL_CONFIG", dir.join("global-config.toml"))
        .env("FORKLIFT_KEYS_DIR", dir.join("keys"))
        .output().unwrap();
    assert!(cloned.status.success(), "{}", String::from_utf8_lossy(&cloned.stderr));

    let record = std::fs::read_to_string(dir.join("warehouse/.forklift/unavailable")).unwrap();
    assert!(record.lines().any(|line| line.starts_with(&format!("{} {} ", takedown.film, digest))), "{}", record);
}

/// A host withholding a chunked recipe whose chunk list it never learnt refuses a lift whose new
/// history names that recipe as a chunked file afresh, with `content_withheld` naming the path and
/// the way out: lift from a warehouse that holds the recipe. A lift that names it no more than the
/// pallet's head already does still lands; a new pallet has no head, so one over the file's parcel
/// is refused. The test strips the digest by hand, standing in for a takedown made before the host
/// held the file whose recipe no lift has sent it yet, and lifts from a clone that never held it.
///
/// Revert: tolerate absent withheld content at a chunked entry whatever its digest → the pick lands.
#[test]
fn a_lift_newly_naming_a_withheld_recipe_the_host_cannot_judge_is_refused() {
    let mut takedown = takedown("uncomparable");
    takedown.withhold_new_film(true);
    let picked = takedown.owner.head("main");

    // Main names the film no more, so no head the host carries vouches for it.
    takedown.owner.run_ok(&["remove", "media/film.mkv"]);
    takedown.owner.run_ok(&["stack", "-m", "take the film out"]);
    takedown.owner.run_ok(&["lift"]);

    // The host forgets the film's digest, as when no lift sent it the recipe.
    let record = takedown.server.root.join(".forklift/withheld");
    let text = std::fs::read_to_string(&record).unwrap();
    let stripped: String = text.lines()
        .map(|line| match line.strip_prefix(&format!("{} ", takedown.film)) {
            Some(rest) => format!("{} - {}\n", takedown.film, rest.split_once(' ').unwrap().1),
            None => format!("{}\n", line),
        })
        .collect();
    assert_ne!(stripped, text, "{}", text);
    std::fs::write(&record, stripped).unwrap();
    assert_eq!(withheld_digests(&takedown.server.url).get(&takedown.film), Some(&None));

    // A clone holds no copy of the film, so it can send the host no recipe.
    let warehouse = takedown.writer("takedown-uncomparable", &[]);
    warehouse.stack("notes.txt", b"notes\n", "notes");
    warehouse.run_ok(&["lift"]);
    assert_eq!(takedown.server.main(), warehouse.head("main"), "a lift naming the film no more than main does lands");

    // A creation has no current head to vouch for the film, so a new pallet over its parcel is
    // refused too, though main's history names it.
    warehouse.run_ok(&["pallet", "create", "over", &picked]);
    warehouse.stack("over.txt", b"over\n", "over");
    let over = warehouse.json(&["lift"]);
    assert_eq!(over["ok"], false, "{}", over);
    assert_eq!(over["error"]["code"], "content_withheld", "{}", over);
    assert!(!takedown.server.root.join(".forklift/pallets/over").exists(), "nothing is lifted");

    // Picking the new film onto a pallet at the old head, beside a poster, names it chunked in a
    // tree no ref carries.
    warehouse.run_ok(&["pallet", "create", "copy", &takedown.old]);
    warehouse.stack("media/poster.txt", b"poster\n", "poster");
    warehouse.run_ok(&["pick", &picked]);
    let lifted = warehouse.json(&["lift"]);
    assert_eq!(lifted["ok"], false, "{}", lifted);
    assert_eq!(lifted["error"]["code"], "content_withheld", "{}", lifted);
    let message = lifted["error"]["message"].as_str().unwrap();
    assert!(message.contains("\"media/film.mkv\" as a chunked file") && message.contains(&takedown.film), "{}", lifted);
    assert!(message.contains(TAKEDOWN), "{}", lifted);
    let next_step = lifted["error"]["next_step"].as_str().unwrap();
    assert!(next_step.starts_with("Lift from a warehouse that holds the recipe"), "{}", lifted);
    assert!(!takedown.server.root.join(".forklift/pallets/copy").exists(), "nothing is lifted");
}
