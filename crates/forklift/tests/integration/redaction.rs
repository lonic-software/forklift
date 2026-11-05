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
        let warehouse = Warehouse::empty(name);
        std::fs::create_dir_all(&warehouse.root).unwrap();
        warehouse.run_ok(&["prepare"]);
        warehouse.run_ok(&["config", "set", "operator.identifier", "redactor@forklift"]);

        warehouse
    }

    /// A clone of the remote at `url`.
    fn cloned(name: &str, url: &str) -> Warehouse {
        let warehouse = Warehouse::empty(name);
        let cloned = forklift_command()
            .args(["clone", url, warehouse.root.to_str().unwrap()])
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
    warehouse.stack("secret.env", b"TOKEN=hunter2\n", "leak");
    let hash = recorded_hash(&warehouse, "main:secret.env", "leaked token")["data"]["hash"]
        .as_str().unwrap().to_string();
    let head = std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap();

    // The file still there, beside a new one: the new head would name it again.
    std::fs::write(warehouse.root.join("notes.txt"), b"notes\n").unwrap();
    warehouse.run_ok(&["load", "."]);
    let error = content_redacted(&warehouse, &["stack", "-m", "notes"]);
    assert_eq!(error["details"]["path"], "secret.env", "{}", error);
    assert_eq!(error["details"]["hash"], hash.as_str(), "{}", error);
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap(), head,
        "nothing was written"
    );

    // The same bytes under another name are the same content.
    std::fs::remove_file(warehouse.root.join("secret.env")).unwrap();
    std::fs::write(warehouse.root.join("copy.env"), b"TOKEN=hunter2\n").unwrap();
    warehouse.run_ok(&["load", "."]);
    let error = content_redacted(&warehouse, &["stack", "-m", "copy"]);
    assert_eq!(error["details"]["path"], "copy.env", "{}", error);

    std::fs::remove_file(warehouse.root.join("copy.env")).unwrap();
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "-m", "without the secret"]);
}

#[test]
fn a_meta_pallet_may_not_start_naming_redacted_content() {
    let warehouse = Warehouse::new("meta-adds");
    warehouse.stack("notes.txt", b"first\n", "first");

    // No @tags yet, so no meta history names the marker and the redaction is recorded; the first
    // @tags parcel would then add it.
    warehouse.stack("marker.txt", b"forklift-meta 1 tags", "marker");
    recorded_hash(&warehouse, "main:marker.txt", "r");

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

#[test]
fn a_lift_is_refused_by_this_warehouses_own_redactions_until_the_file_is_taken_out() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-lift-server-{}", std::process::id())));

    let first = Warehouse::cloned("lift-first", &server.url);
    first.stack("notes.txt", b"notes\n", "notes");
    first.run_ok(&["lift"]);

    // This warehouse clones before any redaction exists, so its @redactions is its own: the one it
    // records below diverges from the remote's, its lift fails, and the remote never learns of
    // that redaction. Only this warehouse's own check can refuse the head.
    let warehouse = Warehouse::cloned("lift", &server.url);

    // Another warehouse moves the remote's @redactions on, from a pallet of its own.
    let other = Warehouse::cloned("lift-other", &server.url);
    other.run_ok(&["pallet", "create", "side"]);
    other.stack("other.env", b"OTHER=1\n", "other");
    other.run_ok(&["lift"]);

    // The remote already serves exactly this head, so lifting it moves nothing, and the
    // redaction reaches the remote's @redactions.
    recorded_hash(&other, "side:other.env", "other leak");
    other.run_ok(&["lift"]);

    // Taken out again, so the remote serves no head naming it.
    other.run_ok(&["remove", "other.env"]);
    other.run_ok(&["stack", "-m", "without other"]);
    other.run_ok(&["lift"]);

    warehouse.stack("secret.env", b"TOKEN=hunter2\n", "leak");
    let hash = recorded_hash(&warehouse, "main:secret.env", "leaked token")["data"]["hash"]
        .as_str().unwrap().to_string();
    let served = served_main(&server.url, "lift-before");

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

    // The next step, as it says. The working pallet lifts; the lift still fails over @redactions,
    // which only a lower reconciles with the remote's.
    warehouse.run_ok(&["remove", "secret.env"]);
    warehouse.run_ok(&["stack", "-m", "without the secret"]);
    let lifted = warehouse.run(&["lift"]);
    assert!(!lifted.status.success(), "{:?}", lifted);
    assert!(String::from_utf8_lossy(&lifted.stderr).contains("@redactions"), "{:?}", lifted);

    let head = std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap();
    assert_eq!(served_main(&server.url, "lift-after"), head.trim());

    warehouse.run_ok(&["lower"]);
    warehouse.run_ok(&["lift"]);
}

#[test]
fn a_lift_refused_for_a_pallet_the_remote_lacks_says_it_was_not_created() {
    let server = Server::start(&std::env::temp_dir()
        .join(format!("forklift-redaction-lift-new-server-{}", std::process::id())));

    let warehouse = Warehouse::cloned("lift-new", &server.url);
    warehouse.run_ok(&["pallet", "create", "fresh"]);
    warehouse.stack("secret.env", b"TOKEN=hunter2\n", "leak");
    recorded_hash(&warehouse, "fresh:secret.env", "leaked token");

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
