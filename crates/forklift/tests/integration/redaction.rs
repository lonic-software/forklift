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
    let recorded = recorded_hash(&warehouse, &format!("{}:secret.env", leak), "leaked token");
    std::fs::remove_file(warehouse.root.join("secret.env")).unwrap();
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
}

impl Server {
    fn start(root: &std::path::Path) -> Server {
        use std::io::BufRead;

        let _ = std::fs::remove_dir_all(root);
        let root = root.to_str().unwrap();
        let prepared = std::process::Command::new(common::server_binary()).env("FORKLIFT_FSYNC", "0")
            .args(["prepare", "--root", root]).output().unwrap();
        assert!(prepared.status.success(), "{}", String::from_utf8_lossy(&prepared.stderr));

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

        Server { child, url }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The head the remote at `url` serves for `main`, read through a fresh clone named `name`.
fn served_main(url: &str, name: &str) -> String {
    let clone = Warehouse::cloned(name, url);
    std::fs::read_to_string(clone.root.join(".forklift/pallets/main")).unwrap().trim().to_string()
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
    let served = served_main(&server.url, "lift-before");

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
    assert_eq!(served_main(&server.url, "lift-refused"), served, "nothing was lifted");

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
    assert_eq!(served_main(&server.url, "lift-after"), warehouse.head("main"));
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
