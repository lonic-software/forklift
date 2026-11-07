//! `forklift redact` records a permanent redaction on the `@redactions` meta pallet: one cell per
//! redacted object hash, never changed or removed, refused outright for a hash some meta pallet's
//! history names, and refused while a pallet head names it.

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
        let _ = std::fs::remove_dir_all(&base);
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
        std::fs::remove_file(self.root.join(file)).unwrap();

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
        std::fs::remove_file(&path).unwrap_or_else(|error| panic!("{}: {}", path.display(), error));
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
    std::fs::remove_file(warehouse.root.join("secret.env")).unwrap();
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
    std::fs::remove_file(warehouse.root.join("held.env")).unwrap();
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

    std::fs::remove_file(warehouse.root.join("secret.env")).unwrap();
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
    std::fs::remove_file(warehouse.root.join("secret.env")).unwrap();
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
    std::fs::remove_file(warehouse.root.join("held.env")).unwrap();
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
        let _ = std::fs::remove_dir_all(root);
        let prepared = std::process::Command::new(common::server_binary()).env("FORKLIFT_FSYNC", "0")
            .args(["prepare", "--root", root.to_str().unwrap()]).output().unwrap();
        assert!(prepared.status.success(), "{}", String::from_utf8_lossy(&prepared.stderr));

        Server::serve(root)
    }

    /// Serve the server root at `root` as it is.
    fn serve(root: &std::path::Path) -> Server {
        use std::io::BufRead;

        let root = root.to_str().unwrap();
        let mut child = std::process::Command::new(common::server_binary()).env("FORKLIFT_FSYNC", "0")
            .args(["serve", "--root", root, "--addr", "127.0.0.1:0", "--open"])
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
    std::fs::remove_file(warehouse.root.join("todo.txt")).unwrap();

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
    std::fs::remove_file(warehouse.root.join("secret.env")).unwrap();
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
    std::fs::remove_file(warehouse.root.join("marker.txt")).unwrap();
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
    std::fs::remove_file(redactor.root.join("secret.env")).unwrap();

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
    std::fs::remove_file(&loose).unwrap();
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
            std::fs::remove_file(copy).unwrap();
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
    let _ = std::fs::remove_dir_all(&second_root);
    let copied = std::process::Command::new("cp").arg("-R").arg(&first_root).arg(&second_root).output().unwrap();
    assert!(copied.status.success(), "{:?}", copied);
    // The lock the first server held is not the copy's: no process holds it.
    std::fs::remove_file(second_root.join(".forklift/serve.lock")).unwrap();
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

    // Until the server serves the redaction's absence, a fresh clone that must fetch the content
    // is refused as redacted: loud, never a quiet gap.
    let empty = Warehouse::empty("served-clone");
    let cloned = forklift_command()
        .args(["clone", &server.url, empty.root.to_str().unwrap()])
        .env("FORKLIFT_GLOBAL_CONFIG", empty.home.join("global-config.toml"))
        .env("FORKLIFT_KEYS_DIR", empty.home.join("keys"))
        .output()
        .unwrap();
    assert!(!cloned.status.success());
    assert!(String::from_utf8_lossy(&cloned.stderr).contains(&format!("Object {} is redacted", hash)),
            "{}", String::from_utf8_lossy(&cloned.stderr));
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

    std::fs::remove_file(root.join(".forklift/serve.lock")).unwrap();
    std::fs::remove_file(root.join(".forklift/redactions-applied")).unwrap();
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
    std::fs::remove_file(root.join(".forklift/serve.lock")).unwrap();
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
    std::fs::remove_file(&parcel).unwrap();

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
/// that file named as absent, by hash, in a batch and as missing, and the handshake reports
/// the deletion owed; a chunk another file names is still served. Once the deletion runs, the
/// handshake reports it deleted.
///
/// Revert: serve a redacted recipe's chunks by hash → the GET of one answers 200.
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
    let _ = std::fs::remove_file(owner.root.join("dump/big.bin"));

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

    // Waits out the drain a deletion attempt serves 503 through.
    let report = || {
        for _ in 0..500 {
            let (status, info) = http(&server.url, "GET", "/v1/warehouse", b"");
            if status.starts_with("HTTP/1.1 503") {
                std::thread::sleep(std::time::Duration::from_millis(20));
                continue;
            }
            assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, info);
            return serde_json::from_str::<serde_json::Value>(&info).unwrap()["redactions"].clone();
        }
        panic!("the warehouse stayed drained");
    };
    let objects = server.root.join(".forklift/objects");

    {
        // Another process using the server's store keeps the deletion owed.
        let _scope = forklift_core::globals::StorageRootScope::enter(&server.root);
        let _store_access = forklift_core::util::lock_utils::StoreAccessLock::try_shared().unwrap();

        owner.run_ok(&["lift"]);
        assert_eq!(report(), serde_json::json!([{ "hash": recipe, "state": "deletion_owed" }]));

        let (status, body) = http(&server.url, "GET", &format!("/v1/objects/{}", own[0]), b"");
        assert!(status.starts_with("HTTP/1.1 404"), "{}, {} bytes", status, body.len());
        assert!(objects.join(&own[0][..2]).join(&own[0][2..]).exists(), "the server still holds the chunk");

        let (status, _) = http(&server.url, "GET", &format!("/v1/objects/{}", shared[0]), b"");
        assert!(status.starts_with("HTTP/1.1 200"), "{}", status);

        // A bundle of the leak's own chunks holds none of them; one of the shared chunks holds them.
        let batch = |hashes: &[String]| {
            let (status, body) = http(&server.url, "POST", "/v1/objects/batch",
                                      serde_json::json!({ "hashes": hashes }).to_string().as_bytes());
            assert!(status.starts_with("HTTP/1.1 200"), "{}", status);
            body.len()
        };
        assert!(batch(&own) < 1024, "the bundle holds the leak's own chunks");
        assert!(batch(&shared) > 1024, "the bundle holds the shared chunks");

        // And missing, so a lift naming one uploads it.
        let asked = [own[0].clone(), shared[0].clone()];
        let (status, body) = http(&server.url, "POST", "/v1/objects/missing",
                                  serde_json::json!({ "hashes": asked }).to_string().as_bytes());
        assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, body);
        let missing: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(missing["missing"], serde_json::json!([own[0]]), "{}", body);

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
    let (status, _) = http(&server.url, "GET", &format!("/v1/objects/{}", shared[0]), b"");
    assert!(status.starts_with("HTTP/1.1 200"), "{}", status);
}
