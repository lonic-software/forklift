//! End-to-end tests of the remote stack: a real `forklift-server` process serving a
//! bare warehouse over HTTP, driven by the real CLI — lift, lower, franchise, bundles,
//! trust, and the server-side verification guarantees of the protocol.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

/// Path to the compiled forklift binary (provided by cargo for integration tests).
const FORKLIFT: &str = env!("CARGO_BIN_EXE_forklift");

/// Path to the compiled forklift-server binary. Cargo exposes `CARGO_BIN_EXE_*` only
/// for the package's own binaries, so the server is located next to the CLI (both land
/// in the same target directory when the workspace builds).
fn server_binary() -> PathBuf {
    let path = Path::new(FORKLIFT)
        .parent()
        .expect("the forklift binary has a parent directory")
        .join(format!("forklift-server{}", std::env::consts::EXE_SUFFIX));

    assert!(
        path.is_file(),
        "forklift-server is not built at {}; run the tests via a workspace build \
        (plain \"cargo test\").",
        path.to_string_lossy()
    );

    path
}

/// A scratch area for one test: warehouses, the server root, the shared key directory
/// and the per-"machine" global configurations all live under it, and it is deleted
/// when the test ends.
struct TestArea {
    root: PathBuf,
}

impl TestArea {
    fn new(name: &str) -> TestArea {
        let root = std::env::temp_dir()
            .join(format!("forklift-remote-{}-{}", name, std::process::id()));

        // A leftover directory from a previous run must not leak state into this one.
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        TestArea { root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// Run the CLI inside a directory of the area. The global configuration and the
    /// key directory are shared across the whole area — the same operator working from
    /// several copies of the warehouse.
    fn forklift(&self, relative_dir: &str, args: &[&str]) -> Output {
        Command::new(FORKLIFT)
            .args(args)
            .current_dir(self.path(relative_dir))
            .env("FORKLIFT_GLOBAL_CONFIG", self.path("area-global.toml"))
            .env("FORKLIFT_KEYS_DIR", self.path("shared-keys"))
            .output()
            .unwrap()
    }

    fn write_file(&self, relative_path: &str, content: &str) {
        let path = self.path(relative_path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn read_file(&self, relative_path: &str) -> String {
        std::fs::read_to_string(self.path(relative_path)).unwrap()
    }
}

impl Drop for TestArea {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A running forklift-server over a bare warehouse in the test area.
struct Server {
    child: Child,
    url: String,
}

impl Server {
    /// Prepare a bare warehouse under `area` and serve it on an ephemeral port.
    fn start(area: &TestArea, token: Option<&str>) -> Server {
        let root = area.path("server-root");
        let root_str = root.to_str().unwrap().to_string();

        let prepared = Command::new(server_binary())
            .args(["prepare", "--root", &root_str])
            .output()
            .unwrap();

        assert!(prepared.status.success(), "server prepare failed: {}",
                String::from_utf8_lossy(&prepared.stderr));

        Server::spawn(vec![
            "serve".to_string(),
            "--root".to_string(), root_str,
            "--addr".to_string(), "127.0.0.1:0".to_string(),
        ], token)
    }

    /// Serve a bare warehouse with a per-operator token file (transport authorization).
    fn start_with_tokens(area: &TestArea, token: Option<&str>, tokens_relative: &str) -> Server {
        let root = area.path("server-root");
        let root_str = root.to_str().unwrap().to_string();

        let prepared = Command::new(server_binary())
            .args(["prepare", "--root", &root_str])
            .output()
            .unwrap();

        assert!(prepared.status.success(), "server prepare failed: {}",
                String::from_utf8_lossy(&prepared.stderr));

        Server::spawn(vec![
            "serve".to_string(),
            "--root".to_string(), root_str,
            "--addr".to_string(), "127.0.0.1:0".to_string(),
            "--tokens".to_string(), area.path(tokens_relative).to_str().unwrap().to_string(),
        ], token)
    }

    /// Serve a folder of warehouses (multi-warehouse mode) on an ephemeral port.
    fn start_multi(area: &TestArea, token: Option<&str>) -> Server {
        let base = area.path("warehouses-base");
        std::fs::create_dir_all(&base).unwrap();

        Server::spawn(vec![
            "serve".to_string(),
            "--warehouses".to_string(), base.to_str().unwrap().to_string(),
            "--addr".to_string(), "127.0.0.1:0".to_string(),
        ], token)
    }

    fn spawn(mut args: Vec<String>, token: Option<&str>) -> Server {
        if let Some(token) = token {
            args.push("--token".to_string());
            args.push(token.to_string());
        }

        let mut child = Command::new(server_binary())
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();

        // The single startup line carries the bound address (port 0 picks a free one).
        let stdout = child.stdout.take().unwrap();
        let mut line = String::new();

        BufReader::new(stdout).read_line(&mut line).unwrap();

        let url = line.trim()
            .rsplit(' ')
            .next()
            .expect("the startup line ends with the URL")
            .to_string();

        assert!(url.starts_with("http://"), "unexpected startup line: {}", line);

        Server { child, url }
    }

    fn rebuild_bundle(&self, area: &TestArea) -> Output {
        let output = Command::new(server_binary())
            .args(["bundle", "--root", area.path("server-root").to_str().unwrap()])
            .output()
            .unwrap();

        assert!(output.status.success(), "server bundle failed: {}",
                String::from_utf8_lossy(&output.stderr));

        output
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Send a bodyless raw HTTP request and return the response's status line.
fn http_status(server_url: &str, method: &str, path: &str, token: Option<&str>) -> String {
    let address = server_url.strip_prefix("http://").unwrap();
    let mut stream = std::net::TcpStream::connect(address).unwrap();

    let auth = token
        .map(|token| format!("Authorization: Bearer {}\r\n", token))
        .unwrap_or_default();

    write!(
        stream,
        "{} {} HTTP/1.1\r\nHost: {}\r\n{}Content-Length: 0\r\nConnection: close\r\n\r\n",
        method, path, address, auth
    ).unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();

    response.lines().next().unwrap_or_default().to_string()
}

/// POST a JSON body over raw HTTP and return `(status line, response body)`. Keeps the
/// upload-targets contract test honest — it asserts the exact wire shape the one client flow
/// depends on, without going through the client that consumes it.
fn http_post_json(server_url: &str, path: &str, body: &str) -> (String, String) {
    let address = server_url.strip_prefix("http://").unwrap();
    let mut stream = std::net::TcpStream::connect(address).unwrap();

    write!(
        stream,
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
        Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        path, address, body.len(), body
    ).unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();

    let status = response.lines().next().unwrap_or_default().to_string();
    let body = response.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();

    (status, body)
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// Create a warehouse under the area with the operator configured and the remote set.
fn prepare_warehouse(area: &TestArea, name: &str, remote_url: &str) {
    std::fs::create_dir_all(area.path(name)).unwrap();

    assert_success(&area.forklift(name, &["prepare"]));
    assert_success(&area.forklift(name, &["config", "--global", "operator.name", "Remote Tester"]));
    assert_success(&area.forklift(name, &["config", "--global", "operator.identifier", "tester@forklift"]));
    assert_success(&area.forklift(name, &["config", "remote.url", remote_url]));
}

#[test]
fn lift_franchise_lower_round_trip() {
    let area = TestArea::new("round-trip");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/readme.txt", "hello remote\n");
    area.write_file("a/src/main.txt", "fn main\n");

    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "first parcel"]));

    area.write_file("a/readme.txt", "hello remote, twice\n");
    assert_success(&area.forklift("a", &["load", "readme.txt"]));
    assert_success(&area.forklift("a", &["stack", "second parcel"]));

    let lifted = area.forklift("a", &["lift"]);
    assert_success(&lifted);
    assert!(stdout(&lifted).contains("Lifted pallet \"main\""), "{}", stdout(&lifted));

    // Lifting again is a clean no-op.
    let again = area.forklift("a", &["lift"]);
    assert_success(&again);
    assert!(stdout(&again).contains("Already up to date"), "{}", stdout(&again));

    // Franchise (clone) into a fresh directory.
    let franchised = area.forklift(".", &["franchise", &server.url, "b"]);
    assert_success(&franchised);
    assert!(stdout(&franchised).contains("Franchised"), "{}", stdout(&franchised));

    assert_eq!(area.read_file("b/readme.txt"), "hello remote, twice\n");
    assert_eq!(area.read_file("b/src/main.txt"), "fn main\n");

    // The franchise is a normal warehouse: clean, with the full history.
    let status = area.forklift("b", &["stocktake"]);
    assert_success(&status);
    assert!(stdout(&status).contains("nothing is staged"), "{}", stdout(&status));
    assert!(stdout(&status).contains("matches the inventory"), "{}", stdout(&status));

    let history = area.forklift("b", &["history"]);
    assert_success(&history);
    assert!(stdout(&history).contains("first parcel"));
    assert!(stdout(&history).contains("second parcel"));

    // New work in A flows to B through the remote.
    area.write_file("a/src/main.txt", "fn main v2\n");
    assert_success(&area.forklift("a", &["load", "src/main.txt"]));
    assert_success(&area.forklift("a", &["stack", "third parcel"]));
    assert_success(&area.forklift("a", &["lift"]));

    let lowered = area.forklift("b", &["lower"]);
    assert_success(&lowered);
    assert!(stdout(&lowered).contains("Lowered pallet \"main\""), "{}", stdout(&lowered));
    assert_eq!(area.read_file("b/src/main.txt"), "fn main v2\n");

    let lowered_again = area.forklift("b", &["lower"]);
    assert_success(&lowered_again);
    assert!(stdout(&lowered_again).contains("Already up to date"), "{}", stdout(&lowered_again));
}

#[test]
fn the_direct_head_answers_upload_targets_all_direct() {
    // The direct head (`forklift-server`) has no staging prefix, so its `upload-targets`
    // negotiation answers with `present` for what it holds and every missing hash in `direct`
    // (empty `targets`) — the exact shape that lets ONE client code path serve both this head
    // and a storage-backed one. The whole suite already lifts through this endpoint; this pins
    // the wire contract directly.
    let area = TestArea::new("upload-targets-direct");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/doc.txt", "content\n");
    assert_success(&area.forklift("a", &["load", "."]));

    let stacked = area.forklift("a", &["stack", "first"]);
    assert_success(&stacked);
    let head = stdout(&stacked)
        .split_whitespace()
        .find(|word| word.len() == 64)
        .expect("the stack output carries the parcel hash")
        .to_string();

    let request = format!("{{\"session\":\"lift-x\",\"hashes\":[\"{}\"]}}", head);

    // Before the lift the new head is missing: `direct`, never staged.
    let (status, body) = http_post_json(&server.url, "/v1/objects/upload-targets", &request);
    assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, body);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["targets"], serde_json::json!({}), "a direct head stages nothing: {}", body);
    assert!(
        json["direct"].as_array().unwrap().iter().any(|h| h == &serde_json::json!(head)),
        "the missing head must be offered as a direct upload: {}", body
    );

    // After the lift the same hash is `present`, nothing left to upload.
    assert_success(&area.forklift("a", &["lift"]));
    let (_, body) = http_post_json(&server.url, "/v1/objects/upload-targets", &request);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        json["present"].as_array().unwrap().iter().any(|h| h == &serde_json::json!(head)),
        "after lifting the object is present: {}", body
    );
    assert!(json["direct"].as_array().unwrap().is_empty(), "nothing left to upload: {}", body);
    assert_eq!(json["targets"], serde_json::json!({}), "{}", body);
}

#[test]
fn upload_targets_rejects_an_invalid_hash_as_client_error() {
    // An invalid hash (non-hex, too short) is a client mistake, not a server fault: `upload-
    // targets` must map it to 422, the same idiom `GET /v1/objects/{hash}` uses, not a 500.
    let area = TestArea::new("upload-targets-invalid-hash");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);

    let request = "{\"session\":\"lift-x\",\"hashes\":[\"not-a-valid-hash\"]}";
    let (status, body) = http_post_json(&server.url, "/v1/objects/upload-targets", request);
    assert!(status.starts_with("HTTP/1.1 422"), "{} / {}", status, body);
}

#[test]
fn diverged_pallets_are_refused_not_merged() {
    let area = TestArea::new("diverged");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/base.txt", "base\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "base"]));
    assert_success(&area.forklift("a", &["lift"]));

    assert_success(&area.forklift(".", &["franchise", &server.url, "b"]));

    // A and B both stack on top of the same base; A lifts first.
    area.write_file("a/base.txt", "a's version\n");
    assert_success(&area.forklift("a", &["load", "base.txt"]));
    assert_success(&area.forklift("a", &["stack", "a work"]));
    assert_success(&area.forklift("a", &["lift"]));

    area.write_file("b/base.txt", "b's version\n");
    assert_success(&area.forklift("b", &["load", "base.txt"]));
    assert_success(&area.forklift("b", &["stack", "b work"]));

    // B's lift must be refused: the remote moved. Nothing may be overwritten.
    let refused = area.forklift("b", &["lift"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("lower"), "{}", stderr(&refused));

    // B's lower reports the divergence (with the parcels fetched for a consolidate).
    let diverged = area.forklift("b", &["lower"]);
    assert!(!diverged.status.success());
    assert!(stderr(&diverged).contains("diverged"), "{}", stderr(&diverged));
}

#[test]
fn server_never_exposes_an_unverified_object() {
    let area = TestArea::new("verify");
    let server = Server::start(&area, None);

    // A well-formed PUT whose body does not match the claimed hash must be rejected,
    // and the hash must stay unfetchable. Raw HTTP keeps the client honest.
    let claimed = "a".repeat(64);
    let body = b"not the content of that hash";

    let address = server.url.strip_prefix("http://").unwrap();
    let mut stream = std::net::TcpStream::connect(address).unwrap();

    write!(
        stream,
        "PUT /v1/objects/{} HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        claimed, address, body.len()
    ).unwrap();
    stream.write_all(body).unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();

    assert!(response.starts_with("HTTP/1.1 422"), "unexpected response: {}", response);

    let mut check = std::net::TcpStream::connect(address).unwrap();
    write!(
        check,
        "GET /v1/objects/{} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        claimed, address
    ).unwrap();

    let mut fetched = String::new();
    check.read_to_string(&mut fetched).unwrap();

    assert!(fetched.starts_with("HTTP/1.1 404"), "unexpected response: {}", fetched);
}

#[test]
fn signed_history_round_trips_and_the_server_audits() {
    let area = TestArea::new("signed");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/code.txt", "v1\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "pre-trust parcel"]));

    // Trust: from here on every parcel is signed, and the remote enforces it.
    assert_success(&area.forklift("a", &["office", "enroll"]));

    area.write_file("a/code.txt", "v2 signed\n");
    assert_success(&area.forklift("a", &["load", "code.txt"]));
    assert_success(&area.forklift("a", &["stack", "signed parcel"]));

    let lifted = area.forklift("a", &["lift"]);
    assert_success(&lifted);
    assert!(stdout(&lifted).contains("Lifted the office pallet"), "{}", stdout(&lifted));

    // The franchise adopts the anchor and audits clean offline.
    let franchised = area.forklift(".", &["franchise", &server.url, "b"]);
    assert_success(&franchised);
    assert!(stdout(&franchised).contains("Adopted the remote's trust anchor"),
            "{}", stdout(&franchised));

    let audit = area.forklift("b", &["audit"]);
    assert_success(&audit);
    assert!(stdout(&audit).contains("verified"), "{}", stdout(&audit));

    // The same operator works from B (the key directory is shared): signed lift works.
    area.write_file("b/code.txt", "v3 from b\n");
    assert_success(&area.forklift("b", &["load", "code.txt"]));
    assert_success(&area.forklift("b", &["stack", "signed from b"]));
    assert_success(&area.forklift("b", &["lift"]));

    let lowered = area.forklift("a", &["lower"]);
    assert_success(&lowered);
    assert_eq!(area.read_file("a/code.txt"), "v3 from b\n");
    assert_success(&area.forklift("a", &["audit"]));

    // A parcel whose signature sidecar is stripped must be refused by the remote:
    // the audit at the ref update is the server-side guarantee.
    area.write_file("a/code.txt", "v4 tampered\n");
    assert_success(&area.forklift("a", &["load", "code.txt"]));
    let stacked = area.forklift("a", &["stack", "to be tampered"]);
    assert_success(&stacked);

    let line = stdout(&stacked);
    let hash = line.split_whitespace()
        .find(|word| word.len() == 64)
        .expect("the stack output contains the parcel hash")
        .to_string();

    let (folder, file) = hash.split_at(2);
    let sidecar = area.path(&format!("a/.forklift/objects/{}/{}.sig", folder, file));
    std::fs::remove_file(&sidecar).expect("the signature sidecar exists");

    let refused = area.forklift("a", &["lift"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("signature"), "{}", stderr(&refused));
}

#[test]
fn optimistic_lift_auto_merges_a_disjoint_divergence_but_stops_on_overlap() {
    let area = TestArea::new("optimistic");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/shared.txt", "base\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "initial"]));
    assert_success(&area.forklift("a", &["office", "enroll"]));
    assert_success(&area.forklift("a", &["lift"]));

    assert_success(&area.forklift(".", &["franchise", &server.url, "b"]));

    // A publishes a change to a file only it touches.
    area.write_file("a/from_a.txt", "a\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "a's file"]));
    assert_success(&area.forklift("a", &["lift"]));

    // B, still at the base, stacks a change to a *different* file and lifts. The remote has
    // diverged, but the change is disjoint — so the lift auto-merges and goes through
    // instead of stopping (optimistic lift, §7.7).
    area.write_file("b/from_b.txt", "b\n");
    assert_success(&area.forklift("b", &["load", "."]));
    assert_success(&area.forklift("b", &["stack", "b's file"]));
    let lifted = area.forklift("b", &["lift"]);
    assert_success(&lifted);
    assert!(stdout(&lifted).contains("auto-merged"), "{}", stdout(&lifted));

    // A lowers and now has both files; the merged history audits clean.
    assert_success(&area.forklift("a", &["lower"]));
    assert_eq!(area.read_file("a/from_b.txt"), "b\n");
    assert_eq!(area.read_file("a/from_a.txt"), "a\n");
    assert_success(&area.forklift("a", &["audit"]));

    // Overlap: both edit the *same* file. A publishes first.
    area.write_file("a/shared.txt", "from a\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "a edits shared"]));
    assert_success(&area.forklift("a", &["lift"]));

    // B (now behind) edits the same file differently and lifts — the merge would conflict,
    // so the lift refuses instead of auto-merging, and the warehouse stays clean.
    area.write_file("b/shared.txt", "from b\n");
    assert_success(&area.forklift("b", &["load", "."]));
    assert_success(&area.forklift("b", &["stack", "b edits shared"]));
    let refused = area.forklift("b", &["lift"]);
    assert!(!refused.status.success(), "an overlapping divergence must not auto-merge");
    assert!(stderr(&refused).contains("lower"), "{}", stderr(&refused));
    assert_eq!(area.read_file("b/shared.txt"), "from b\n"); // untouched — no half-merge
}

#[test]
fn manifest_syncs_across_remotes_and_merges_when_diverged() {
    let area = TestArea::new("manifest-remote");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/code.txt", "v1\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "first parcel"]));
    assert_success(&area.forklift("a", &["office", "enroll"]));

    // Record an approval, then lift — the @manifest meta pallet rides along.
    assert_success(&area.forklift("a", &["manifest", "approve", "main", "-m", "LGTM"]));
    let lifted = area.forklift("a", &["lift"]);
    assert_success(&lifted);
    assert!(stdout(&lifted).contains("Lifted the @manifest pallet"), "{}", stdout(&lifted));

    // A franchise carries the manifest, not just the working history.
    let franchised = area.forklift(".", &["franchise", &server.url, "b"]);
    assert_success(&franchised);
    assert!(stdout(&franchised).contains("Adopted the @manifest pallet"), "{}", stdout(&franchised));
    assert!(stdout(&area.forklift("b", &["manifest", "show", "main"])).contains("LGTM"));

    // Diverge the manifest: B records and publishes a note; A records a different note
    // on the same base without lowering first.
    assert_success(&area.forklift("b", &["manifest", "note", "main", "-m", "from b"]));
    assert_success(&area.forklift("b", &["lift"]));

    assert_success(&area.forklift("a", &["manifest", "note", "main", "-m", "from a"]));
    let refused = area.forklift("a", &["lift"]);
    assert!(!refused.status.success(), "a diverged manifest lift must be refused");
    assert!(stderr(&refused).contains("lower"), "{}", stderr(&refused));

    // Lowering merges the two branches (conflict-free: the records are independent), and
    // the merge lifts cleanly.
    let lowered = area.forklift("a", &["lower"]);
    assert_success(&lowered);
    assert!(stdout(&lowered).contains("Merged the remote @manifest"), "{}", stdout(&lowered));
    assert_success(&area.forklift("a", &["lift"]));

    // Both notes and the approval are now visible from A, and B sees them after lowering.
    let a_view = stdout(&area.forklift("a", &["manifest", "show", "main"]));
    assert!(a_view.contains("from a") && a_view.contains("from b") && a_view.contains("LGTM"), "{}", a_view);

    assert_success(&area.forklift("b", &["lower"]));
    let b_view = stdout(&area.forklift("b", &["manifest", "show", "main"]));
    assert!(b_view.contains("from a") && b_view.contains("from b"), "{}", b_view);

    // The merged manifest audits clean on both sides.
    assert!(stdout(&area.forklift("b", &["audit", "@manifest"])).contains("verified"));
}

#[test]
fn hauls_sync_across_remotes_and_merge_when_diverged() {
    let area = TestArea::new("haul-remote");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/base.txt", "base\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "base"]));
    assert_success(&area.forklift("a", &["office", "enroll"]));

    // Open a haul (feat → main) and lift — the @haul meta pallet rides along.
    assert_success(&area.forklift("a", &["palletize", "feat"]));
    area.write_file("a/feat.txt", "x\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "feat work"]));
    assert_success(&area.forklift("a", &["shift", "main"]));

    let opened = area.forklift("a", &["haul", "open", "--target", "main", "--source", "feat", "--title", "Feat", "-m", "review"]);
    assert_success(&opened);
    let id = stdout(&opened).split_whitespace().nth(2).unwrap().to_string(); // "Opened haul <id> — …"

    let lifted = area.forklift("a", &["lift"]);
    assert_success(&lifted);
    assert!(stdout(&lifted).contains("Lifted the @haul pallet"), "{}", stdout(&lifted));

    // A franchise carries the haul.
    assert_success(&area.forklift(".", &["franchise", &server.url, "b"]));
    assert!(stdout(&area.forklift("b", &["haul", "show", &id])).contains("Feat"), "b must see the haul");

    // Diverge: B comments and publishes; A comments on the same base without lowering.
    assert_success(&area.forklift("b", &["haul", "comment", &id, "-m", "from b"]));
    assert_success(&area.forklift("b", &["lift"]));

    assert_success(&area.forklift("a", &["haul", "comment", &id, "-m", "from a"]));
    let refused = area.forklift("a", &["lift"]);
    assert!(!refused.status.success(), "a diverged @haul lift must be refused");

    // Lowering unions the two event branches (conflict-free), and lifts cleanly.
    let lowered = area.forklift("a", &["lower"]);
    assert_success(&lowered);
    assert!(stdout(&lowered).contains("Merged the remote @haul"), "{}", stdout(&lowered));
    assert_success(&area.forklift("a", &["lift"]));

    // Both comments are visible from A; B sees them after lowering; the log audits clean.
    let a_view = stdout(&area.forklift("a", &["haul", "show", &id]));
    assert!(a_view.contains("from a") && a_view.contains("from b"), "{}", a_view);

    assert_success(&area.forklift("b", &["lower"]));
    assert!(stdout(&area.forklift("b", &["audit", "@haul"])).contains("verified"));
}

#[test]
fn bundles_speed_up_franchising() {
    let area = TestArea::new("bundle");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/one.txt", "one\n");
    area.write_file("a/two.txt", "two\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "bundled history"]));
    assert_success(&area.forklift("a", &["lift"]));

    server.rebuild_bundle(&area);

    let franchised = area.forklift(".", &["franchise", &server.url, "c"]);
    assert_success(&franchised);
    assert!(stdout(&franchised).contains("Imported the remote's bundle"),
            "{}", stdout(&franchised));
    // Everything came from the bundle; the loose walk had nothing left to fetch.
    assert!(stdout(&franchised).contains("0 object(s) fetched loose"),
            "{}", stdout(&franchised));

    assert_eq!(area.read_file("c/one.txt"), "one\n");
    assert_eq!(area.read_file("c/two.txt"), "two\n");
}

#[test]
fn a_token_protected_remote_requires_the_token() {
    let area = TestArea::new("token");
    let server = Server::start(&area, Some("sesame"));

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/secret.txt", "locked\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "guarded parcel"]));

    // Without the token the remote answers 401 to everything.
    let refused = area.forklift("a", &["lift"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("401"), "{}", stderr(&refused));

    assert_success(&area.forklift("a", &["config", "remote.token", "sesame"]));
    assert_success(&area.forklift("a", &["lift"]));

    // Franchise takes the token as a flag and remembers it in the configuration.
    let franchised = area.forklift(".", &["franchise", &server.url, "b", "--token", "sesame"]);
    assert_success(&franchised);
    assert_eq!(area.read_file("b/secret.txt"), "locked\n");

    let token_value = area.forklift("b", &["config", "remote.token"]);
    assert_success(&token_value);
    assert!(stdout(&token_value).contains("sesame"));
}

#[test]
fn enroll_covers_the_remote_history_in_the_trust_boundary() {
    let area = TestArea::new("enroll-boundary");
    let server = Server::start(&area, None);

    // A lifts unsigned history; B franchises and lifts more, so the remote is ahead
    // of A. Without the remote check, A's enroll would set a boundary that misses the
    // remote's head — and the pallet could never be lifted again once the anchor
    // reaches the remote (trust is a one-way door, history is immutable).
    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/doc.txt", "v1\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "first"]));
    assert_success(&area.forklift("a", &["lift"]));

    assert_success(&area.forklift(".", &["franchise", &server.url, "b"]));
    area.write_file("b/doc.txt", "v2, unsigned, only on the remote\n");
    assert_success(&area.forklift("b", &["load", "doc.txt"]));
    assert_success(&area.forklift("b", &["stack", "second"]));
    assert_success(&area.forklift("b", &["lift"]));

    // A enrolls while behind the remote: the remote's heads join the boundary.
    assert_success(&area.forklift("a", &["office", "enroll"]));

    // A catches up, stacks signed work on top of the remote's unsigned head, and the
    // trusted remote accepts the lift: the unsigned parcel is inside the boundary.
    assert_success(&area.forklift("a", &["lower"]));
    area.write_file("a/doc.txt", "v3 signed\n");
    assert_success(&area.forklift("a", &["load", "doc.txt"]));
    assert_success(&area.forklift("a", &["stack", "signed on top"]));

    let lifted = area.forklift("a", &["lift"]);
    assert_success(&lifted);
    assert!(stdout(&lifted).contains("Lifted the office pallet"), "{}", stdout(&lifted));
    assert!(stdout(&lifted).contains("Lifted pallet \"main\""), "{}", stdout(&lifted));
}

#[test]
fn enroll_refuses_when_the_remote_already_has_trust() {
    let area = TestArea::new("enroll-taken");
    let server = Server::start(&area, None);

    // A establishes trust and lifts it to the remote.
    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/doc.txt", "v1\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "first"]));
    assert_success(&area.forklift("a", &["office", "enroll"]));
    assert_success(&area.forklift("a", &["lift"]));

    // A second warehouse pointed at the same remote must not mint its own genesis:
    // the two anchors could never be reconciled.
    prepare_warehouse(&area, "c", &server.url);

    let refused = area.forklift("c", &["office", "enroll"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("already has trust"), "{}", stderr(&refused));
}

#[test]
fn enroll_needs_the_remote_or_the_offline_flag() {
    let area = TestArea::new("enroll-offline");

    // The configured remote is unreachable: enroll must refuse (its heads cannot be
    // included in the boundary) unless --offline explicitly waives them.
    prepare_warehouse(&area, "a", "http://127.0.0.1:1");
    area.write_file("a/doc.txt", "v1\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "first"]));

    let refused = area.forklift("a", &["office", "enroll"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("--offline"), "{}", stderr(&refused));

    assert_success(&area.forklift("a", &["office", "enroll", "--offline"]));
}

#[test]
fn multi_warehouse_serving_isolates_warehouses() {
    let area = TestArea::new("multi");
    let server = Server::start_multi(&area, Some("admin"));

    // Warehouse creation is explicit and idempotent: 201, then 200.
    assert_eq!(http_status(&server.url, "PUT", "/warehouses/alpha", Some("admin")), "HTTP/1.1 201 Created");
    assert_eq!(http_status(&server.url, "PUT", "/warehouses/alpha", Some("admin")), "HTTP/1.1 200 OK");
    assert_eq!(http_status(&server.url, "PUT", "/warehouses/beta", Some("admin")), "HTTP/1.1 201 Created");

    // Two warehouses on one server: the id travels inside remote.url.
    prepare_warehouse(&area, "a", &format!("{}/warehouses/alpha", server.url));
    assert_success(&area.forklift("a", &["config", "remote.token", "admin"]));
    area.write_file("a/alpha.txt", "alpha content\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "alpha parcel"]));
    assert_success(&area.forklift("a", &["lift"]));

    prepare_warehouse(&area, "b", &format!("{}/warehouses/beta", server.url));
    assert_success(&area.forklift("b", &["config", "remote.token", "admin"]));
    area.write_file("b/beta.txt", "beta content\n");
    assert_success(&area.forklift("b", &["load", "."]));
    assert_success(&area.forklift("b", &["stack", "beta parcel"]));
    assert_success(&area.forklift("b", &["lift"]));

    // A franchise of alpha gets alpha's history and nothing of beta's.
    let franchised = area.forklift(".", &[
        "franchise", &format!("{}/warehouses/alpha", server.url), "c", "--token", "admin",
    ]);
    assert_success(&franchised);
    assert_eq!(area.read_file("c/alpha.txt"), "alpha content\n");
    assert!(!area.path("c/beta.txt").exists());

    // Lifting to a warehouse that was never created is refused, with the remedy named.
    prepare_warehouse(&area, "d", &format!("{}/warehouses/gamma", server.url));
    assert_success(&area.forklift("d", &["config", "remote.token", "admin"]));
    area.write_file("d/doc.txt", "content\n");
    assert_success(&area.forklift("d", &["load", "."]));
    assert_success(&area.forklift("d", &["stack", "parcel"]));

    let refused = area.forklift("d", &["lift"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("Create it first"), "{}", stderr(&refused));
}

#[test]
fn warehouse_creation_is_gated_and_validated() {
    let area = TestArea::new("create-gate");

    // An open (token-less) multi-warehouse server refuses creation outright.
    let open = Server::start_multi(&area, None);
    assert_eq!(http_status(&open.url, "PUT", "/warehouses/alpha", None), "HTTP/1.1 403 Forbidden");
    drop(open);

    let area = TestArea::new("create-gate2");
    let server = Server::start_multi(&area, Some("admin"));

    // The token is required, and ids are validated (no hidden folders, no traversal).
    assert_eq!(http_status(&server.url, "PUT", "/warehouses/alpha", None), "HTTP/1.1 401 Unauthorized");
    assert_eq!(
        http_status(&server.url, "PUT", "/warehouses/.hidden", Some("admin")),
        "HTTP/1.1 422 Unprocessable Entity"
    );

    // A single-warehouse server has no creation surface at all.
    let area_single = TestArea::new("create-gate3");
    let single = Server::start(&area_single, Some("admin"));
    assert_eq!(
        http_status(&single.url, "PUT", "/warehouses/alpha", Some("admin")),
        "HTTP/1.1 404 Not Found"
    );
}

#[test]
fn operator_tokens_enforce_roles_and_pallet_grants() {
    let area = TestArea::new("roles");

    // Server tokens map to operator ids (opaque strings; these tests pick readable
    // ones — unset identities would mint pseudonymous UUIDs instead).
    area.write_file("tokens.toml", concat!(
        "[operators]\n",
        "\"w-token\" = \"worker@forklift\"\n",
        "\"r-token\" = \"reader@forklift\"\n",
    ));

    let server = Server::start_with_tokens(&area, Some("root"), "tokens.toml");

    // The admin (the area's default operator) establishes trust and lifts with the
    // static token.
    prepare_warehouse(&area, "a", &server.url);
    assert_success(&area.forklift("a", &["config", "remote.token", "root"]));
    area.write_file("a/code.txt", "v1\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "base"]));
    assert_success(&area.forklift("a", &["office", "enroll"]));

    // Admit a writer (granted "main" only) and a reader; the keys land in the area's
    // shared key directory, so the other "machines" below can sign with them.
    let worker_id = "worker@forklift";
    let reader_id = "reader@forklift";

    let worker = keygen_admit_args(&area, worker_id);
    assert_success(&area.forklift("a", &[
        "office", "admit", &worker[0], &worker[1], &worker[2],
        "--role", "writer", "--pallet", "main",
    ]));

    let reader = keygen_admit_args(&area, reader_id);
    assert_success(&area.forklift("a", &[
        "office", "admit", &reader[0], &reader[1], &reader[2], "--role", "reader",
    ]));

    assert_success(&area.forklift("a", &["lift"]));

    // A non-admin cannot manage the office locally either.
    let refused_admit = area.forklift("a", &["office", "role", worker_id, "admin"]);
    assert_success(&refused_admit); // the admin CAN change roles...
    assert_success(&area.forklift("a", &["office", "role", worker_id, "writer", "--pallet", "main"]));
    assert_success(&area.forklift("a", &["lift"]));

    // The writer works from a franchise, authenticated by their own token.
    let franchised = area.forklift(".", &[
        "franchise", &server.url, "b", "--token", "w-token",
    ]);
    assert_success(&franchised);
    // The worker's identity on this "machine": the warehouse-scoped override.
    assert_success(&area.forklift("b", &["config", "operator.identifier", worker_id]));

    area.write_file("b/code.txt", "v2 by worker\n");
    assert_success(&area.forklift("b", &["load", "code.txt"]));
    assert_success(&area.forklift("b", &["stack", "worker change"]));
    assert_success(&area.forklift("b", &["lift"]));

    // The worker may not admit anyone (admin move), locally refused up front.
    let refused = area.forklift("b", &["office", "admit", "x@forklift", &"a".repeat(64), "00"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("not an office admin"), "{}", stderr(&refused));

    // The writer's grant is "main" only: another pallet is refused by the remote.
    assert_success(&area.forklift("b", &["palletize", "side"]));
    area.write_file("b/code.txt", "v3 on side\n");
    assert_success(&area.forklift("b", &["load", "code.txt"]));
    assert_success(&area.forklift("b", &["stack", "side change"]));

    let refused = area.forklift("b", &["lift"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("may not move pallet"), "{}", stderr(&refused));

    // Key rotation is self-service: the office lift by a non-admin passes the remote's
    // privilege check because only the signer's own keys change.
    assert_success(&area.forklift("b", &["shift", "main"]));
    assert_success(&area.forklift("b", &["office", "rotate"]));

    let lifted = area.forklift("b", &["lift"]);
    assert_success(&lifted);
    assert!(stdout(&lifted).contains("Lifted the office pallet"), "{}", stdout(&lifted));

    // The reader can franchise (read) but not lift (write).
    let franchised = area.forklift(".", &[
        "franchise", &server.url, "c", "--token", "r-token",
    ]);
    assert_success(&franchised);
    assert_eq!(area.read_file("c/code.txt"), "v2 by worker\n");

    assert_success(&area.forklift("c", &["config", "operator.identifier", reader_id]));

    area.write_file("c/code.txt", "v4 by reader\n");
    assert_success(&area.forklift("c", &["load", "code.txt"]));
    assert_success(&area.forklift("c", &["stack", "reader change"]));

    let refused = area.forklift("c", &["lift"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("reader"), "{}", stderr(&refused));

    // An unknown token is a plain 401.
    let unknown = area.forklift(".", &["franchise", &server.url, "d", "--token", "wrong"]);
    assert!(!unknown.status.success());
    assert!(stderr(&unknown).contains("401"), "{}", stderr(&unknown));
}

#[test]
fn regenesis_is_gated_by_the_static_token_and_accepted_consciously() {
    let area = TestArea::new("regenesis");

    area.write_file("tokens.toml", concat!(
        "[operators]\n",
        "\"w-token\" = \"worker@forklift\"\n",
    ));

    let server = Server::start_with_tokens(&area, Some("root"), "tokens.toml");

    // The admin sets up warehouse A, enrolls, admits a worker, and lifts.
    prepare_warehouse(&area, "a", &server.url);
    assert_success(&area.forklift("a", &["config", "remote.token", "root"]));
    area.write_file("a/code.txt", "v1\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "base"]));
    assert_success(&area.forklift("a", &["office", "enroll"]));

    let worker = keygen_admit_args(&area, "worker@forklift");
    assert_success(&area.forklift("a", &["office", "admit", &worker[0], &worker[1], &worker[2]]));
    assert_success(&area.forklift("a", &["lift"]));

    // The worker clones B before the reset.
    assert_success(&area.forklift(".", &["franchise", &server.url, "b", "--token", "w-token"]));
    assert_success(&area.forklift("b", &["config", "operator.identifier", "worker@forklift"]));

    // Total key loss on A's side: nobody can extend the chain anymore.
    std::fs::remove_dir_all(area.path("shared-keys")).unwrap();
    assert_success(&area.forklift("a", &["office", "regenesis", "--confirm"]));

    // Pushing the reset with a per-operator token is refused: its authority comes
    // from exactly the chain being replaced. Only the static token may sanction it.
    assert_success(&area.forklift("a", &["config", "remote.token", "w-token"]));
    let refused = area.forklift("a", &["lift"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("static token"), "{}", stderr(&refused));

    assert_success(&area.forklift("a", &["config", "remote.token", "root"]));
    let lifted = area.forklift("a", &["lift"]);
    assert_success(&lifted);

    // B's next sync is refused loudly, pointing at the conscious re-accept.
    let refused = area.forklift("b", &["lower"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("RESET"), "{}", stderr(&refused));
    assert!(stderr(&refused).contains("accept-regenesis"), "{}", stderr(&refused));

    // The acceptance itself is two-step: a dry-run description, then --confirm.
    let dry = area.forklift("b", &["office", "accept-regenesis"]);
    assert!(!dry.status.success());
    assert!(stdout(&dry).contains("RESET"), "{}", stdout(&dry));

    assert_success(&area.forklift("b", &["office", "accept-regenesis", "--confirm"]));
    assert_success(&area.forklift("b", &["lower"]));

    // B's offline audit passes with the prior history attested (legacy).
    let audit = area.forklift("b", &["audit"]);
    assert!(audit.status.success(), "{}", stderr(&audit));
    assert!(stdout(&audit).contains("legacy parcel(s)"), "{}", stdout(&audit));
}

#[test]
fn a_compromised_key_cannot_sign_beyond_its_distrust_boundary() {
    let area = TestArea::new("compromise");

    area.write_file("tokens.toml", concat!(
        "[operators]\n",
        "\"w-token\" = \"worker@forklift\"\n",
    ));

    let server = Server::start_with_tokens(&area, Some("root"), "tokens.toml");

    prepare_warehouse(&area, "a", &server.url);
    assert_success(&area.forklift("a", &["config", "remote.token", "root"]));
    area.write_file("a/code.txt", "v1\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "base"]));
    assert_success(&area.forklift("a", &["office", "enroll"]));

    let worker = keygen_admit_args(&area, "worker@forklift");
    assert_success(&area.forklift("a", &["office", "admit", &worker[0], &worker[1], &worker[2]]));
    assert_success(&area.forklift("a", &["lift"]));

    // The worker lifts legitimate work before the compromise is noticed…
    assert_success(&area.forklift(".", &["franchise", &server.url, "b", "--token", "w-token"]));
    assert_success(&area.forklift("b", &["config", "operator.identifier", "worker@forklift"]));
    area.write_file("b/code.txt", "v2 by worker\n");
    assert_success(&area.forklift("b", &["load", "code.txt"]));
    assert_success(&area.forklift("b", &["stack", "lifted before revocation"]));
    assert_success(&area.forklift("b", &["lift"]));

    // …and stacks more that never reaches the remote before the revocation.
    area.write_file("b/code.txt", "v3 by worker\n");
    assert_success(&area.forklift("b", &["load", "code.txt"]));
    assert_success(&area.forklift("b", &["stack", "signed after the boundary"]));

    // The admin revokes the worker's key as compromised: the distrust boundary is the
    // current heads — the remote's included, so the lifted v2 stays vouched for.
    assert_success(&area.forklift("a", &["lower"]));
    let list = stdout(&area.forklift("a", &["office", "list"]));
    let worker_key_id = list.lines()
        .rev()
        .find(|line| line.trim_start().starts_with("key "))
        .and_then(|line| line.split_whitespace().nth(1))
        .expect("office list shows the worker's key")
        .to_string();

    let revoked = area.forklift("a", &["office", "retire", &worker_key_id, "--compromised"]);
    assert!(revoked.status.success(), "{}", stderr(&revoked));
    assert!(stdout(&revoked).contains("compromise"), "{}", stdout(&revoked));
    assert_success(&area.forklift("a", &["lift"]));

    // The remote now refuses the out-of-boundary parcel: the same audit a local
    // verifier would run offline. (The worker syncs the revocation first — the lift
    // is refused either way, this just exercises the server-side audit.)
    assert_success(&area.forklift("b", &["lower"]));
    let refused = area.forklift("b", &["lift"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("distrust boundary"), "{}", stderr(&refused));

    // Work lifted before the revocation stays valid: a fresh clone audits clean.
    assert_success(&area.forklift(".", &["franchise", &server.url, "c", "--token", "w-token"]));
    let audit = area.forklift("c", &["audit"]);
    assert!(audit.status.success(), "{}", stderr(&audit));
}

/// Generate a keypair for the given operator id (on the area's shared key directory)
/// and return the printed admit arguments: [id, public_key, proof_of_possession].
/// The keygen runs under the target operator's id (warehouse-scope override), then
/// the admin's identity is restored.
fn keygen_admit_args(area: &TestArea, operator_id: &str) -> Vec<String> {
    let admin_output = area.forklift("a", &["config", "--global", "operator.identifier"]);
    assert_success(&admin_output);
    let admin_id = stdout(&admin_output).trim().to_string();

    assert_success(&area.forklift("a", &["config", "operator.identifier", operator_id]));
    let output = area.forklift("a", &["office", "keygen"]);
    assert_success(&output);
    assert_success(&area.forklift("a", &["config", "operator.identifier", &admin_id]));

    stdout(&output).lines()
        .find(|line| line.trim_start().starts_with("office admit "))
        .expect("keygen prints the admit line")
        .split_whitespace()
        .skip(2)
        .map(|s| s.to_string())
        .collect()
}

#[test]
fn healthz_is_open_and_the_config_file_configures_the_server() {
    let area = TestArea::new("ops");

    // The whole serve configuration can come from a TOML file.
    let root = area.path("server-root");
    std::fs::create_dir_all(&root).unwrap();

    let prepared = Command::new(server_binary())
        .args(["prepare", "--root", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(prepared.status.success());

    area.write_file("server.toml", &format!(
        "root = '{}'\naddr = \"127.0.0.1:0\"\ntoken = \"sesame\"\n",
        root.to_str().unwrap()
    ));

    let server = Server::spawn(vec![
        "serve".to_string(),
        "--config".to_string(), area.path("server.toml").to_str().unwrap().to_string(),
    ], None);

    // The health endpoint needs no token; the protocol does (both from the file).
    assert_eq!(http_status(&server.url, "GET", "/healthz", None), "HTTP/1.1 200 OK");
    assert_eq!(http_status(&server.url, "GET", "/v1/warehouse", None), "HTTP/1.1 401 Unauthorized");
    assert_eq!(http_status(&server.url, "GET", "/v1/warehouse", Some("sesame")), "HTTP/1.1 200 OK");
}

#[test]
fn gc_is_refused_while_serving_then_sweeps_orphans_once_the_server_stops() {
    let area = TestArea::new("gc");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/keep.txt", "live content\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "live parcel"]));
    assert_success(&area.forklift("a", &["lift"]));

    // Plant an orphan object on the server (what an abandoned lift leaves behind).
    let orphan = area.path("server-root/.forklift/objects/aa/orphan-object");
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, b"junk").unwrap();

    let root = area.path("server-root");
    let root_str = root.to_str().unwrap().to_string();

    // gc against the *live* server is refused — it would sweep the server's in-flight objects
    // and make a concurrent lift fail its ref update. It must wait until the server is stopped, and
    // it sweeps nothing when refused.
    let refused = Command::new(server_binary())
        .args(["gc", "--root", &root_str])
        .output()
        .unwrap();
    assert!(!refused.status.success(),
            "gc must be refused while a server serves this root");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("locked by another forklift process"),
            "the refusal should name the serve lock: {}", String::from_utf8_lossy(&refused.stderr));
    assert!(orphan.exists(), "a refused gc must not have swept anything");

    // Stop the server. A graceful shutdown (SIGTERM/SIGINT) drops the serve lock automatically; the
    // test harness hard-kills the child (SIGKILL), which leaves the lock behind — exactly the
    // stale-lock case an operator clears after a crash — so we clear it here before gc.
    drop(server);
    let _ = std::fs::remove_file(root.join(".forklift").join("serve.lock"));

    // Within the grace period the orphan is protected...
    let output = Command::new(server_binary())
        .args(["gc", "--root", &root_str])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("deleted 0"),
            "{}", String::from_utf8_lossy(&output.stdout));
    assert!(orphan.exists());

    // ...with the grace period at zero it is swept, and live history survives.
    let output = Command::new(server_binary())
        .args(["gc", "--root", &root_str, "--grace-hours", "0"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("deleted 1"),
            "{}", String::from_utf8_lossy(&output.stdout));
    assert!(!orphan.exists());

    // Live history survives the sweep: re-serve the same root (the lock is free again) and
    // franchise a fresh copy.
    let server = Server::start(&area, None);
    let franchised = area.forklift(".", &["franchise", &server.url, "b"]);
    assert_success(&franchised);
    assert_eq!(area.read_file("b/keep.txt"), "live content\n");
}

#[test]
fn a_second_server_and_gc_are_refused_while_serving_but_bundle_is_allowed() {
    let area = TestArea::new("serve-lock");
    let server = Server::start(&area, None);
    let root_str = area.path("server-root").to_str().unwrap().to_string();

    // A second server on the same root is refused up front (it would silently break the first
    // server's in-process ref-update CAS). The acquire happens before the serve loop, so this
    // fails fast rather than blocking.
    let second = Command::new(server_binary())
        .args(["serve", "--root", &root_str, "--addr", "127.0.0.1:0"])
        .output()
        .unwrap();
    assert!(!second.status.success(),
            "a second server on the same root must be refused");
    assert!(String::from_utf8_lossy(&second.stderr).contains("locked by another forklift process"),
            "second-server refusal should name the serve lock: {}",
            String::from_utf8_lossy(&second.stderr));

    // gc is likewise refused while serving.
    let gc = Command::new(server_binary())
        .args(["gc", "--root", &root_str])
        .output()
        .unwrap();
    assert!(!gc.status.success(), "gc must be refused while serving");

    // bundle, by contrast, is deliberately *allowed* against a live server: it never deletes an
    // object, writes atomically, and a stale bundle is self-healing.
    let bundle = Command::new(server_binary())
        .args(["bundle", "--root", &root_str])
        .output()
        .unwrap();
    assert!(bundle.status.success(),
            "bundle must be allowed while serving: {}", String::from_utf8_lossy(&bundle.stderr));

    drop(server);
}

#[test]
fn franchising_an_unborn_default_pallet_starts_unborn() {
    let area = TestArea::new("unborn-default");
    let server = Server::start(&area, None);

    // The remote ends up with history on "feature" only: its default pallet ("main")
    // is legitimately unborn while other pallets exist.
    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/doc.txt", "v1\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "base"]));
    assert_success(&area.forklift("a", &["palletize", "feature"]));
    area.write_file("a/doc.txt", "v2 on feature\n");
    assert_success(&area.forklift("a", &["load", "doc.txt"]));
    assert_success(&area.forklift("a", &["stack", "feature work"]));
    assert_success(&area.forklift("a", &["lift"]));

    // Franchising the remote's default pallet starts unborn instead of erroring.
    let franchised = area.forklift(".", &["franchise", &server.url, "b"]);
    assert_success(&franchised);
    assert!(stdout(&franchised).contains("starts unborn"), "{}", stdout(&franchised));

    // A pallet asked for by name still gets typo protection.
    let typo = area.forklift(".", &["franchise", &server.url, "c", "--pallet", "faeture"]);
    assert!(!typo.status.success());
    assert!(stderr(&typo).contains("no pallet \"faeture\""), "{}", stderr(&typo));
}

// ---------------------------------------------------------------------------------
// Hook protocol (docs/format/HOOK_PROTOCOL.md): a mock provider endpoint receiving
// signed hook requests from the server head.
// ---------------------------------------------------------------------------------

/// One recorded hook request: the path it hit and the signed envelope.
#[derive(Clone)]
struct HookRequest {
    path: String,
    hook: String,
    timestamp: i64,
    signature: String,
    body: Vec<u8>,
}

/// A minimal HTTP endpoint standing in for a hosting provider's hook service:
/// records every request, answers /auth, /admission and /events with canned JSON.
struct HookServer {
    url: String,
    requests: std::sync::Arc<std::sync::Mutex<Vec<HookRequest>>>,
    admit: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl HookServer {
    fn start() -> HookServer {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let admit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));

        let recorded = std::sync::Arc::clone(&requests);
        let admitting = std::sync::Arc::clone(&admit);

        // The thread lives for the rest of the test process; each test starts its own
        // endpoint on its own port, so leaking the acceptor is harmless.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };

                let Some(request) = read_http_request(&mut stream) else { continue };

                let body = match request.path.as_str() {
                    "/auth" => {
                        let token_is_known = String::from_utf8_lossy(&request.body)
                            .contains("\"token\":\"hook-token\"");

                        if token_is_known {
                            ("200 OK", r#"{"identifier":"tester@forklift"}"#)
                        } else {
                            ("403 Forbidden", r#"{}"#)
                        }
                    }
                    "/admission" => {
                        if admitting.load(std::sync::atomic::Ordering::SeqCst) {
                            ("200 OK", r#"{"allow":true}"#)
                        } else {
                            ("200 OK", r#"{"allow":false,"reason":"the quota is exhausted"}"#)
                        }
                    }
                    "/resolve" => ("200 OK", r#"{"names":{"tester@forklift":"Remote Tester"}}"#),
                    _ => ("200 OK", r#"{}"#),
                };

                recorded.lock().unwrap().push(request);

                let _ = write!(
                    stream,
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.0, body.1.len(), body.1
                );
            }
        });

        HookServer { url, requests, admit }
    }

    fn recorded(&self) -> Vec<HookRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// Wait (up to ~5s) for a recorded request on `path` whose body contains `needle`
    /// — events are delivered asynchronously after the triggering response.
    fn wait_for(&self, path: &str, needle: &str) -> HookRequest {
        for _ in 0..100 {
            let hit = self.recorded().into_iter().find(|request| {
                request.path == path && String::from_utf8_lossy(&request.body).contains(needle)
            });

            if let Some(hit) = hit {
                return hit;
            }

            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        panic!(
            "no {} request containing {:?} arrived; recorded: {:?}",
            path,
            needle,
            self.recorded().iter()
                .map(|r| format!("{} {}", r.path, String::from_utf8_lossy(&r.body)))
                .collect::<Vec<String>>()
        );
    }
}

/// Read one HTTP/1.1 request (start line, headers, content-length body).
fn read_http_request(stream: &mut std::net::TcpStream) -> Option<HookRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];

    let header_end = loop {
        if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break position + 4;
        }

        match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
    };

    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let path = head.lines().next()?.split_whitespace().nth(1)?.to_string();

    let header = |name: &str| -> Option<String> {
        head.lines()
            .find(|line| line.to_ascii_lowercase().starts_with(&format!("{}:", name)))
            .and_then(|line| line.split_once(':'))
            .map(|(_, value)| value.trim().to_string())
    };

    let content_length: usize = header("content-length")?.parse().ok()?;
    let mut body = buffer[header_end..].to_vec();

    while body.len() < content_length {
        match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
    }

    body.truncate(content_length);

    Some(HookRequest {
        path,
        hook: header("x-forklift-hook").unwrap_or_default(),
        timestamp: header("x-forklift-hook-timestamp")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0),
        signature: header("x-forklift-hook-signature").unwrap_or_default(),
        body,
    })
}

/// Assert a hook request's Blake3 MAC verifies under the shared secret — the mutual
/// authentication every hook endpoint must run before acting.
fn assert_signed(request: &HookRequest, secret: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    forklift_core::util::hook_utils::verify_hook_request(
        secret,
        request.timestamp,
        now,
        &request.body,
        &request.signature,
    ).unwrap_or_else(|reason| panic!(
        "the {} hook request must be signed: {}", request.hook, reason
    ));
}

/// Start a server whose config wires all three hooks at the mock endpoint.
fn start_hooked_server(area: &TestArea, hooks: &HookServer, secret: &str) -> Server {
    let root = area.path("server-root");
    let root_str = root.to_str().unwrap().to_string();

    let prepared = Command::new(server_binary())
        .args(["prepare", "--root", &root_str])
        .output()
        .unwrap();
    assert!(prepared.status.success());

    area.write_file("hooks.toml", &format!(
        "root = '{}'\naddr = \"127.0.0.1:0\"\n\n[hooks]\n\
        authentication_url = \"{}/auth\"\nauthentication_secret = \"{}\"\n\
        admission_url = \"{}/admission\"\nadmission_secret = \"{}\"\n\
        events_url = \"{}/events\"\nevents_secret = \"{}\"\n",
        root_str, hooks.url, secret, hooks.url, secret, hooks.url, secret
    ));

    Server::spawn(vec![
        "serve".to_string(),
        "--config".to_string(), area.path("hooks.toml").to_str().unwrap().to_string(),
    ], None)
}

#[test]
fn hooks_authenticate_admit_and_notify() {
    let area = TestArea::new("hooks");
    let hooks = HookServer::start();
    let secret = "a shared hook secret";
    let server = start_hooked_server(&area, &hooks, secret);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/readme.txt", "hello hooks\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "first parcel"]));

    // Without a credential the hook knows, nothing moves (the server has no static
    // token — the authentication hook is the whole gate).
    let refused = area.forklift("a", &["lift"]);
    assert!(!refused.status.success(), "an unauthenticated lift must fail");

    assert_success(&area.forklift("a", &["config", "remote.token", "wrong-token"]));
    let refused = area.forklift("a", &["lift"]);
    assert!(!refused.status.success(), "a token the hook refuses must fail");

    // The hook-known credential authenticates, admission admits, the lift lands.
    assert_success(&area.forklift("a", &["config", "remote.token", "hook-token"]));
    assert_success(&area.forklift("a", &["lift"]));

    // Every hook request is signed with the shared secret, and the admission saw the
    // operator the authentication hook resolved.
    let auth = hooks.wait_for("/auth", "hook-token");
    assert_eq!(auth.hook, "authentication");
    assert_signed(&auth, secret);

    let admission = hooks.wait_for("/admission", "\"action\":\"ref_update\"");
    assert_eq!(admission.hook, "admission");
    assert_signed(&admission, secret);
    let admission_body = String::from_utf8_lossy(&admission.body).to_string();
    assert!(admission_body.contains("\"operator\":\"tester@forklift\""), "{}", admission_body);
    assert!(admission_body.contains("\"pallet\":\"main\""), "{}", admission_body);

    // The accepted lift is announced (asynchronously) as a pallet_updated event.
    let event = hooks.wait_for("/events", "\"event\":\"pallet_updated\"");
    assert_eq!(event.hook, "event");
    assert_signed(&event, secret);
    let event_body = String::from_utf8_lossy(&event.body).to_string();
    assert!(event_body.contains("\"pallet\":\"main\""), "{}", event_body);
    assert!(event_body.contains("\"new_head\""), "{}", event_body);

    // Trust establishment and a key revocation reach the events hook too: enroll
    // (PUT /v1/trust → trust_established), then rotate (the office lift carries the
    // old key's revocation → key_revoked).
    assert_success(&area.forklift("a", &["office", "enroll"]));
    area.write_file("a/readme.txt", "hello trust\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "signed parcel"]));
    assert_success(&area.forklift("a", &["lift"]));
    hooks.wait_for("/events", "\"event\":\"trust_established\"");

    assert_success(&area.forklift("a", &["office", "rotate"]));
    assert_success(&area.forklift("a", &["lift"]));
    let revoked = hooks.wait_for("/events", "\"event\":\"key_revoked\"");
    let revoked_body = String::from_utf8_lossy(&revoked.body).to_string();
    assert!(revoked_body.contains("\"detail\":\"retirement\""), "{}", revoked_body);

    // Admission is a soft gate with teeth: a denial refuses the lift, with the
    // hook's reason in the client's error.
    hooks.admit.store(false, std::sync::atomic::Ordering::SeqCst);
    area.write_file("a/readme.txt", "hello denial\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "denied parcel"]));
    let denied = area.forklift("a", &["lift"]);
    assert!(!denied.status.success(), "a lift the admission hook denies must fail");
    assert!(stderr(&denied).contains("quota is exhausted"), "{}", stderr(&denied));
}

#[test]
fn an_unreachable_authentication_hook_fails_closed() {
    let area = TestArea::new("hooks-closed");

    let root = area.path("server-root");
    let prepared = Command::new(server_binary())
        .args(["prepare", "--root", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(prepared.status.success());

    // A hook URL nothing listens on: every authentication must be refused — an
    // unreachable authorizer is a closed door, never an open one.
    area.write_file("hooks.toml", &format!(
        "root = '{}'\naddr = \"127.0.0.1:0\"\n\n[hooks]\n\
        authentication_url = \"http://127.0.0.1:9/auth\"\nauthentication_secret = \"s\"\n",
        root.to_str().unwrap()
    ));

    let server = Server::spawn(vec![
        "serve".to_string(),
        "--config".to_string(), area.path("hooks.toml").to_str().unwrap().to_string(),
    ], None);

    prepare_warehouse(&area, "a", &server.url);
    assert_success(&area.forklift("a", &["config", "remote.token", "any-token"]));
    area.write_file("a/readme.txt", "hello\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "parcel"]));

    let refused = area.forklift("a", &["lift"]);
    assert!(!refused.status.success(), "an unreachable authentication hook must fail closed");
    assert!(
        stderr(&refused).contains("authentication service is unavailable"),
        "{}", stderr(&refused)
    );
}

#[test]
fn resolution_is_server_mediated_and_degrades_to_pseudonyms() {
    let area = TestArea::new("hooks-resolve");
    let hooks = HookServer::start();
    let secret = "a shared resolution secret";

    // A server whose only hook is resolution — the directory a provider runs. The
    // client never talks to it directly; it asks the server, which asks the directory
    // (§8.12: server-mediated so the resolution policy is enforced, not advisory).
    let root = area.path("server-root");
    let prepared = Command::new(server_binary())
        .args(["prepare", "--root", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(prepared.status.success());

    area.write_file("resolve.toml", &format!(
        "root = '{}'\naddr = \"127.0.0.1:0\"\n\n[hooks]\n\
        resolution_url = \"{}/resolve\"\nresolution_secret = \"{}\"\n",
        root.to_str().unwrap(), hooks.url, secret
    ));

    let server = Server::spawn(vec![
        "serve".to_string(),
        "--config".to_string(), area.path("resolve.toml").to_str().unwrap().to_string(),
    ], None);

    // A warehouse with a local office and this server as its remote.
    prepare_warehouse(&area, "a", &server.url);
    assert_success(&area.forklift("a", &["office", "enroll"]));
    area.write_file("a/readme.txt", "hello\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "a parcel"]));

    // The display name never touches the chain — it can only come from the directory,
    // reached through the server.
    let history = area.forklift("a", &["history"]);
    assert_success(&history);
    assert!(
        stdout(&history).contains("Remote Tester <tester@forklift>"),
        "{}", stdout(&history)
    );

    let listed = area.forklift("a", &["office", "list"]);
    assert_success(&listed);
    assert!(
        stdout(&listed).contains("tester@forklift (Remote Tester)"),
        "{}", stdout(&listed)
    );

    // What reached the directory was a signed *resolution* hook request from the
    // server (not the client), naming the ids to resolve.
    let resolve = hooks.wait_for("/resolve", "tester@forklift");
    assert_eq!(resolve.hook, "resolution");
    assert_signed(&resolve, secret);

    // Resolution never fails a command: point the remote at a dead port and history
    // degrades to the pseudonymous id instead of erroring.
    assert_success(&area.forklift("a", &["config", "remote.url", "http://127.0.0.1:9"]));
    let history = area.forklift("a", &["history"]);
    assert_success(&history);
    assert!(!stdout(&history).contains("Remote Tester"), "{}", stdout(&history));
    assert!(stdout(&history).contains("tester@forklift"), "{}", stdout(&history));
}

#[test]
fn delta_bundle_reconstructs_a_files_many_versions() {
    let area = TestArea::new("delta-bundle");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);

    // A largish text file that evolves through many small edits — exactly what deltas are
    // for: the bundle should move each change, not re-ship the whole file every version.
    let version = |v: usize| -> String {
        (0..400)
            .map(|i| if i == 0 {
                format!("VERSION {}\n", v)
            } else {
                format!("line {} lorem ipsum dolor sit amet consectetur\n", i)
            })
            .collect::<String>()
    };

    area.write_file("a/big.txt", &version(1));
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "v1"]));

    for v in 2..=8 {
        area.write_file("a/big.txt", &version(v));
        assert_success(&area.forklift("a", &["load", "big.txt"]));
        assert_success(&area.forklift("a", &["stack", &format!("v{}", v)]));
    }

    assert_success(&area.forklift("a", &["lift"]));

    // The bundle build reports deltas — the successive versions were stored as differences.
    let bundle = server.rebuild_bundle(&area);
    let out = stdout(&bundle);
    assert!(out.contains("delta(s)") && !out.contains("0 delta(s)"),
            "the bundle should contain deltas: {}", out);

    // Franchise: the client imports the *delta* bundle, reconstructs every version against
    // its base, and lands the latest content — proof the delta round-trip is correct. Every
    // object is hash-verified on import, so a bad delta would fail here (or leave a loose
    // fetch); "0 object(s) fetched loose" confirms the whole closure came from the bundle.
    let franchised = area.forklift(".", &["franchise", &server.url, "b"]);
    assert_success(&franchised);
    assert!(stdout(&franchised).contains("Imported the remote's bundle"), "{}", stdout(&franchised));
    assert!(stdout(&franchised).contains("0 object(s) fetched loose"), "{}", stdout(&franchised));

    assert_eq!(area.read_file("b/big.txt"), version(8));

    // The franchise is a clean, fully-verifiable warehouse with the full history.
    let status = area.forklift("b", &["stocktake"]);
    assert_success(&status);
    assert!(stdout(&status).contains("matches the inventory"), "{}", stdout(&status));

    let history = stdout(&area.forklift("b", &["history"]));
    assert!(history.contains("v1") && history.contains("v8"), "{}", history);
}

/// A sync walks the *gap* between the heads, not the length of history.
///
/// The old walk descended every parcel back to the genesis on every `lower`, skipping only
/// the *fetch* of objects it already had — and re-probing the remote for the signature of
/// every unsigned parcel, since "no sidecar here" is indistinguishable from "not fetched
/// yet". It now stops at any parcel already reachable from a local ref head, whose closure
/// was proven complete when that ref moved.
///
/// Driven through `fetch_history` directly, because the bound is invisible from the CLI:
/// the wasted work was local walking, not bytes on the wire.
#[test]
fn a_sync_walks_the_gap_between_the_heads_not_the_history() {
    let area = TestArea::new("bounded-lower");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);

    // A history worth not re-walking.
    for version in 0..6 {
        area.write_file("a/code.txt", &format!("v{}\n", version));
        assert_success(&area.forklift("a", &["load", "."]));
        assert_success(&area.forklift("a", &["stack", &format!("parcel {}", version)]));
    }

    assert_success(&area.forklift("a", &["lift"]));

    // B clones the whole history, so its ref head has a complete closure.
    assert_success(&area.forklift(".", &["franchise", &server.url, "b"]));

    // One more parcel on the remote.
    area.write_file("a/code.txt", "the new segment\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "the new segment"]));
    assert_success(&area.forklift("a", &["lift"]));

    let remote_head = std::fs::read_to_string(area.path("server-root/.forklift/pallets/main"))
        .unwrap()
        .trim()
        .to_string();

    let sync = |head: &str| -> forklift_core::util::remote_utils::FetchStats {
        // A current-thread runtime: `fetch_history` spawns its transfers, and the storage
        // scope is a thread-local, so the tasks must stay on this thread.
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _scope = forklift_core::globals::StorageRootScope::enter(&area.path("b"));
        let client = forklift_core::util::remote_utils::RemoteClient::new(&server.url, None).unwrap();

        runtime
            .block_on(forklift_core::util::remote_utils::fetch_history(&client, head))
            .expect("fetch")
    };

    // Seven parcels behind it, and it walks the one that is new.
    assert_eq!(sync(&remote_head).walked_parcels, 1, "a sync must walk only the gap");

    // An interrupted sync is still healed. B's ref has not moved, so the parcel the fetch
    // above stored sits *above* the bound: deleting it leaves a gap the next sync re-walks
    // and re-fetches, exactly as the unbounded walk did.
    let stored = area.path(&format!(
        "b/.forklift/objects/{}/{}",
        &remote_head[0..2],
        &remote_head[2..]
    ));
    assert!(stored.exists(), "the earlier fetch stored the new parcel");
    std::fs::remove_file(&stored).unwrap();

    let healed = sync(&remote_head);
    assert_eq!(healed.walked_parcels, 1, "the un-referenced parcel is still walked");
    assert!(healed.fetched_objects >= 1, "an interrupted sync above the bound is healed");

    // Once the ref has moved, an up-to-date sync walks nothing at all.
    assert_success(&area.forklift("b", &["lower"]));
    assert_eq!(sync(&remote_head).walked_parcels, 0, "an up-to-date sync walks nothing");
}
