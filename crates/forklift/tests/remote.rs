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

    /// Write deterministic, seeded pseudo-random raw bytes to a path under the area — for a
    /// large (chunk-threshold-crossing) file, which `write_file`'s `&str` content cannot express.
    /// No clock/RNG, so the content (and thus its chunking) is reproducible.
    fn write_large_file(&self, relative_path: &str, seed: u64, size: usize) {
        let path = self.path(relative_path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

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

        std::fs::write(path, bytes).unwrap();
    }

    fn read_file(&self, relative_path: &str) -> String {
        std::fs::read_to_string(self.path(relative_path)).unwrap()
    }
}

/// The chunk threshold (bytes): content at or above this is stored chunked. Mirrors
/// `chunk_utils::CHUNK_THRESHOLD_BYTES` (a frozen format constant).
const CHUNK_THRESHOLD: usize = 8 * 1024 * 1024;

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

    /// Prepare and serve a bare warehouse under a named root of the area — a second,
    /// independent remote alongside `start`'s `server-root`.
    fn start_at(area: &TestArea, root_relative: &str, token: Option<&str>) -> Server {
        let root = area.path(root_relative);
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

/// Walk a parcel's tree to the object at a warehouse path via `peek --json`, run in a directory
/// of the area — so a test can find (then delete) an object and prove a later walk skips it.
fn path_object_hash(area: &TestArea, dir: &str, parcel: &str, path: &str) -> String {
    let peeked = area.forklift(dir, &["--json", "peek", parcel]);
    assert_success(&peeked);
    let value: serde_json::Value = serde_json::from_str(&stdout(&peeked)).unwrap();
    let mut hash = value["data"]["tree"].as_str().unwrap().to_string();

    for component in path.split('/') {
        let peeked = area.forklift(dir, &["--json", "peek", &hash]);
        assert_success(&peeked);
        let value: serde_json::Value = serde_json::from_str(&stdout(&peeked)).unwrap();
        hash = value["data"]["entries"].as_array().unwrap().iter()
            .find(|entry| entry["name"] == component)
            .unwrap_or_else(|| panic!("no tree entry \"{}\" under {}", component, hash))
            ["hash"].as_str().unwrap().to_string();
    }

    hash
}

/// The object-store path of a loose object in a warehouse under the area (bays share it).
fn object_store_path(area: &TestArea, warehouse_dir: &str, hash: &str) -> PathBuf {
    area.path(warehouse_dir)
        .join(".forklift").join("objects").join(&hash[0..2]).join(&hash[2..])
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
fn a_scoped_merge_lifts_without_reading_the_out_of_scope_object() {
    // A scoped bay's merge-then-lift, end to end: a scoped bay consolidates a diverged remote "work"
    // head that changed an out-of-scope subtree (adopting it by hash into the merge), then lifts.
    // The lift's negotiation walk must prune that subtree against the merge's second parent — the
    // remote head it is lifting against — and never load it. To prove that, the out-of-scope
    // object is deleted from the local store before the lift; the remote already holds it, so the
    // ref update still goes through and the server's closure check passes.
    let area = TestArea::new("scoped-merge-lift");
    let server = Server::start(&area, None);

    // dev seeds the base on main, then registers a scoped "work" pallet on the server.
    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let work_dir = area.path("dev-work");
    assert_success(&area.forklift("dev",
        &["bay", "add", "work", work_dir.to_str().unwrap(), "--scope", "src/api"]));
    assert_success(&area.forklift("dev-work", &["lift"])); // server "work" = base

    // A second machine clones the scoped "work" pallet (full), changes the out-of-scope subtree,
    // and lifts it — so the server's "work" head now carries an out-of-scope change dev never made.
    assert_success(&area.forklift(".", &["franchise", &server.url, "up", "--pallet", "work"]));
    area.write_file("up/src/web/w.txt", "web v2 from up\n");
    assert_success(&area.forklift("up", &["load", "."]));
    assert_success(&area.forklift("up", &["stack", "up edits web (out of scope for dev)"]));
    assert_success(&area.forklift("up", &["lift"]));
    let remote_web_head = area.read_file("up/.forklift/pallets/work").trim().to_string();

    // dev's scoped bay makes an in-scope edit, diverging its "work" head from the server's.
    std::fs::write(work_dir.join("src/api/a.txt"), "api v2 from dev\n").unwrap();
    assert_success(&area.forklift("dev-work", &["load", "."]));
    assert_success(&area.forklift("dev-work", &["stack", "dev edits api"]));

    // Lower fetches the diverged remote head (with its out-of-scope object) and reports the
    // divergence; palletize that head and consolidate it into work — the scoped bay adopts the
    // out-of-scope subtree by hash into the stacked merge parcel.
    let lowered = area.forklift("dev-work", &["lower"]);
    assert!(!lowered.status.success(), "the divergence must be reported: {}", stdout(&lowered));

    assert_success(&area.forklift("dev-work", &["palletize", "incoming", &remote_web_head]));
    assert_success(&area.forklift("dev-work", &["shift", "work"]));
    assert_success(&area.forklift("dev-work", &["consolidate", "incoming"]));

    // Make the out-of-scope object impossible to read (a sparse store would never have fetched it):
    // the merge already committed its hash, so the lift must not need its bytes.
    let web_tree = path_object_hash(&area, "dev-work", &remote_web_head, "src/web");
    std::fs::remove_file(object_store_path(&area, "dev", &web_tree))
        .expect("the out-of-scope object existed");

    // The lift succeeds: its closure walk prunes the out-of-scope subtree against the merge's
    // second parent (the remote "work" head) and never loads it; the remote already holds it.
    let lifted = area.forklift("dev-work", &["lift"]);
    assert_success(&lifted);
    assert!(stdout(&lifted).contains("Lifted") || stdout(&lifted).contains("up to date"),
        "the scoped merge must lift: {}", stdout(&lifted));
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

/// The optimistic auto-merge (`lift::try_auto_merge`) calls the same shared
/// `merge_head_into_current` `consolidate` and `cherry-pick` do: it writes real merge output
/// into the working inventory and only afterwards records consolidation state
/// (`.forklift/consolidation`), before ever reaching `stack_utils::stack_parcel`'s own late
/// guard. The up-front guard added at the call site in `lift::try_auto_merge` must refuse
/// before any of that runs: reverting it makes the state-file assertion below go red, exactly
/// like the `consolidate`/`cherry-pick`/`haul merge` cases in `cli.rs`.
#[test]
fn optimistic_lift_refuses_on_an_incomplete_load_before_writing_consolidation_state() {
    let area = TestArea::new("optimistic-incomplete-load");
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

    // B, still at the base, stacks a disjoint change of its own — the remote has diverged, and
    // the change is disjoint, so an ordinary lift would auto-merge (optimistic lift, §7.7).
    area.write_file("b/from_b.txt", "b\n");
    assert_success(&area.forklift("b", &["load", "."]));
    assert_success(&area.forklift("b", &["stack", "b's file"]));

    // Simulate a load killed outright partway through its walk in B's warehouse (see
    // `load_guard_utils::marker_path_under`'s doc comment for why this is a faithful stand-in
    // for a real interrupted load, driven through a spawned `forklift` subprocess).
    let marker_path = forklift_core::util::load_guard_utils::marker_path_under(&area.path("b"));
    std::fs::write(&marker_path, "./\n").unwrap();

    let lifted = area.forklift("b", &["--json", "lift"]);
    assert!(!lifted.status.success(), "the lift must refuse, not auto-merge over an incomplete load");
    assert_eq!(lifted.status.code(), Some(20));
    let value: serde_json::Value = serde_json::from_str(&stdout(&lifted)).unwrap();
    assert_eq!(value["error"]["code"], "incomplete_load");

    // The auto-merge must not have gotten far enough to write its own in-progress state, or to
    // apply anything to the working directory.
    assert!(
        !area.path("b/.forklift/consolidation").exists(),
        "the optimistic auto-merge's consolidation state must not be written while an \
        incomplete load is outstanding"
    );
    assert!(!area.path("b/from_a.txt").exists(), "the auto-merge must not have applied anything");

    // Healing the load lets the lift auto-merge normally.
    std::fs::remove_file(&marker_path).unwrap();
    let healed = area.forklift("b", &["lift"]);
    assert_success(&healed);
    assert!(stdout(&healed).contains("auto-merged"), "{}", stdout(&healed));
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

/// The head parcel hash of a pallet in a warehouse under the area.
fn pallet_head(area: &TestArea, dir: &str, pallet: &str) -> String {
    area.read_file(&format!("{}/.forklift/pallets/{}", dir, pallet)).trim().to_string()
}

/// Whether a loose object is present in a warehouse's store (bays share it).
fn object_present(area: &TestArea, dir: &str, hash: &str) -> bool {
    object_store_path(area, dir, hash).exists()
}

/// The hash of a recipe's first chunk, read from a warehouse's own store (for asserting a chunk's
/// presence/absence over the wire).
fn recipe_first_chunk(area: &TestArea, dir: &str, recipe_hash: &str) -> String {
    let _scope = forklift_core::globals::StorageRootScope::enter(&area.path(dir));
    forklift_core::util::object_utils::load_recipe(recipe_hash)
        .expect("load the recipe")
        .chunks
        .first()
        .expect("the recipe has at least one chunk")
        .hash
        .clone()
}

#[test]
fn a_sparse_franchise_fetches_only_the_scoped_subtree() {
    // The headline: `franchise --only src/api` fetches the whole signed history but only the
    // content under src/api. The in-scope subtree and blob land; the out-of-scope subtree and a
    // top-level out-of-scope file are never downloaded — sealed by the hash the spine commits.
    let area = TestArea::new("sparse-franchise");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    area.write_file("dev/top.txt", "top v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let api_tree = path_object_hash(&area, "dev", &head, "src/api");
    let api_blob = path_object_hash(&area, "dev", &head, "src/api/a.txt");
    let web_tree = path_object_hash(&area, "dev", &head, "src/web");
    let web_blob = path_object_hash(&area, "dev", &head, "src/web/w.txt");
    let top_blob = path_object_hash(&area, "dev", &head, "top.txt");

    let franchised = area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/api"]);
    assert_success(&franchised);
    assert!(stdout(&franchised).contains("Sparse"), "{}", stdout(&franchised));

    // The working tree materializes only the in-scope subtree.
    assert_eq!(area.read_file("sparse/src/api/a.txt"), "api v1\n");
    assert!(!area.path("sparse/src/web").exists(), "out-of-scope subtree must not materialize");
    assert!(!area.path("sparse/top.txt").exists(), "out-of-scope file must not materialize");

    // The in-scope objects are present; the out-of-scope objects were never fetched.
    assert!(object_present(&area, "sparse", &api_tree), "the in-scope subtree object must be present");
    assert!(object_present(&area, "sparse", &api_blob), "the in-scope blob must be present");
    assert!(!object_present(&area, "sparse", &web_tree), "the out-of-scope subtree object must be sealed, not fetched");
    assert!(!object_present(&area, "sparse", &web_blob), "the out-of-scope blob must be sealed, not fetched");
    assert!(!object_present(&area, "sparse", &top_blob), "the out-of-scope top-level file must be sealed, not fetched");

    // The full signed history is present regardless of content scope.
    let history = area.forklift("sparse", &["history"]);
    assert_success(&history);
    assert!(stdout(&history).contains("base"), "{}", stdout(&history));

    // The store reports itself sparse; the checkout is clean.
    let scope = area.forklift("sparse", &["--json", "scope"]);
    assert_success(&scope);
    let scope_json: serde_json::Value = serde_json::from_str(&stdout(&scope)).unwrap();
    assert_eq!(scope_json["data"]["fetch_scope"], serde_json::json!(["src/api"]), "{}", stdout(&scope));
    assert_eq!(scope_json["data"]["materialization_scope"], serde_json::json!(["src/api"]), "{}", stdout(&scope));

    let stocktake = area.forklift("sparse", &["stocktake"]);
    assert_success(&stocktake);
    assert!(stdout(&stocktake).contains("matches the inventory"), "{}", stdout(&stocktake));

    // In-scope work stacks and lifts back to the origin, without ever needing the sealed content.
    std::fs::write(area.path("sparse/src/api/a.txt"), "api v2 from sparse\n").unwrap();
    let diff = area.forklift("sparse", &["diff"]);
    assert_success(&diff);
    assert!(stdout(&diff).contains("api v2"), "{}", stdout(&diff));

    assert_success(&area.forklift("sparse", &["load", "."]));
    assert_success(&area.forklift("sparse", &["stack", "sparse edits api"]));

    let lifted = area.forklift("sparse", &["lift"]);
    assert_success(&lifted);
    assert!(stdout(&lifted).contains("Lifted"), "{}", stdout(&lifted));

    // The origin now serves the in-scope edit to a full clone.
    assert_success(&area.forklift(".", &["franchise", &server.url, "full"]));
    assert_eq!(area.read_file("full/src/api/a.txt"), "api v2 from sparse\n");
    assert_eq!(area.read_file("full/src/web/w.txt"), "web v1\n", "the sealed sibling is intact on the origin");
}

#[test]
fn a_sparse_franchise_degrades_touches_to_unknown_out_of_scope() {
    // The query engine's sparse degrade (§9.4c Finding 3), end to end: a `--touches`
    // predicate whose confirming diff needs a tree the sparse clone never fetched reads
    // Unknown for that parcel — never an error — and is counted in `scope.out_of_scope`;
    // the parcel simply does not match. The same predicate against an in-scope path on the
    // very same sparse clone still matches normally.
    let area = TestArea::new("sparse-query-touches");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));

    area.write_file("dev/docs/readme.md", "docs v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    let docs_stack = area.forklift("dev", &["--json", "stack", "touch docs"]);
    assert_success(&docs_stack);
    let docs_parcel: serde_json::Value = serde_json::from_str(&stdout(&docs_stack)).unwrap();
    let docs_parcel = docs_parcel["data"]["parcel"].as_str().unwrap().to_string();

    area.write_file("dev/src/api/b.txt", "api v2\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    let src_stack = area.forklift("dev", &["--json", "stack", "touch src"]);
    assert_success(&src_stack);
    let src_parcel: serde_json::Value = serde_json::from_str(&stdout(&src_stack)).unwrap();
    let src_parcel = src_parcel["data"]["parcel"].as_str().unwrap().to_string();

    assert_success(&area.forklift("dev", &["lift"]));

    // A sparse clone scoped to src/api only: the full signed history lands, but docs/ is
    // never fetched.
    let franchised = area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/api"]);
    assert_success(&franchised);
    assert!(stdout(&franchised).contains("Sparse"), "{}", stdout(&franchised));

    // `--touches docs/readme.md` cannot confirm the docs-touching parcel: resolving a file
    // path (not just a bare directory prefix) needs to descend into the `docs/` subtree
    // object itself to list its files, and that subtree was never fetched by the sparse
    // clone. It must degrade to Unknown rather than error — the parcel does not match, the
    // walk still succeeds, and the degrade is counted.
    let touches_docs = area.forklift("sparse", &["--json", "query", "--touches", "docs/readme.md"]);
    assert_success(&touches_docs);
    let report: serde_json::Value = serde_json::from_str(&stdout(&touches_docs)).unwrap();
    assert_eq!(report["ok"], true);
    let matched: std::collections::HashSet<String> = report["data"]["matches"]
        .as_array().unwrap().iter()
        .map(|entry| entry["parcel"].as_str().unwrap().to_string())
        .collect();
    assert!(
        !matched.contains(&docs_parcel),
        "the docs-touching parcel must not match on a clone that never fetched docs/: {}",
        stdout(&touches_docs)
    );
    assert!(
        report["data"]["scope"]["out_of_scope"].as_u64().unwrap() > 0,
        "the sparse degrade must be counted in scope.out_of_scope: {}",
        stdout(&touches_docs)
    );
    assert!(
        report["data"]["scope"]["fetch_scope"].is_array(),
        "scope.fetch_scope must be present on a sparse warehouse: {}",
        stdout(&touches_docs)
    );

    // The in-scope path still matches correctly on the same sparse clone.
    let touches_src = area.forklift("sparse", &["--json", "query", "--touches", "src"]);
    assert_success(&touches_src);
    let report: serde_json::Value = serde_json::from_str(&stdout(&touches_src)).unwrap();
    let matched: std::collections::HashSet<String> = report["data"]["matches"]
        .as_array().unwrap().iter()
        .map(|entry| entry["parcel"].as_str().unwrap().to_string())
        .collect();
    assert!(
        matched.contains(&src_parcel),
        "an in-scope --touches must still match on the sparse clone: {}",
        stdout(&touches_src)
    );
}

#[test]
fn a_partial_clone_missing_a_boundary_head_reads_signer_boundary_as_unresolved() {
    // The presence guard, end to end: a distrust boundary can name a head this store never
    // has at all.
    //
    // Construction note (a deviation from the original sketch, which tried to manufacture
    // this with `undo`): un-stacking a side-pallet parcel with `undo` *after* retiring past
    // it does not work, because `undo`'s journal snapshots and restores *every* pallet ref,
    // meta pallets included (`journal_utils::JournalEntry::heads`) — reversing the stack
    // would also silently revert the office pallet's own ref, un-retiring the key right
    // back to active. That's a real wrinkle in `undo` (out of scope for this predicate
    // work: it never expected a meta-pallet mutation to land *between* the operation being
    // undone and the undo itself). A pallet that is simply never lifted sidesteps it
    // entirely — and is arguably the more realistic shape besides, since `office retire`
    // snapshots *every* current pallet head regardless of what's ever reached the remote:
    // a purely local, never-pushed pallet's head lands in a distrust boundary exactly the
    // same as a published one's.
    //
    // So: a second local-only pallet ("side") supplies the boundary head that a plain
    // (non-sparse) franchise of `main` alone will never even hear about, let alone fetch —
    // it is flatly absent on the clone, not merely "not reachable from here". Silently
    // treating that absence as "not reachable" (the way an ordinary reachability walk
    // always would) must not happen: it would misclassify the agent's still-perfectly-
    // vouched main-line parcel as suspect. It must read "unresolved" instead. On the
    // origin, where the side pallet (and its head) are still right there, the same parcel
    // is fully resolvable and reads its real answer: vouched.
    let area = TestArea::new("boundary-unresolved");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/base.txt", "base\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["office", "enroll"]));

    // Admit an agent.
    assert_success(&area.forklift("dev", &["config", "operator.identifier", "agent@forklift"]));
    let keygen = area.forklift("dev", &["office", "keygen"]);
    assert_success(&keygen);
    let admit_args: Vec<String> = stdout(&keygen)
        .lines()
        .find(|line| line.trim_start().starts_with("office admit "))
        .expect("keygen prints the admit line")
        .split_whitespace()
        .skip(2)
        .map(str::to_string)
        .collect();
    assert_success(&area.forklift("dev", &["config", "operator.identifier", "tester@forklift"]));
    assert_success(&area.forklift("dev", &[
        "office", "admit", &admit_args[0], &admit_args[1], &admit_args[2],
        "--agent", "--supervisor", "tester@forklift",
    ]));

    // The agent's main-line parcel — this is the one whose boundary classification the test
    // actually checks. It stays reachable from `main` throughout.
    assert_success(&area.forklift("dev", &["config", "operator.identifier", "agent@forklift"]));
    area.write_file("dev/agent-main.txt", "agent main work\n");
    assert_success(&area.forklift("dev", &["load", "agent-main.txt"]));
    let agent_stack = area.forklift("dev", &["--json", "stack", "agent main work"]);
    assert_success(&agent_stack);
    let agent_parcel: serde_json::Value = serde_json::from_str(&stdout(&agent_stack)).unwrap();
    let agent_parcel = agent_parcel["data"]["parcel"].as_str().unwrap().to_string();

    // A second, local-only pallet: its head will sit in the retirement's boundary below but
    // will never reach the remote — nothing ever lifts `side`.
    assert_success(&area.forklift("dev", &["palletize", "side"]));
    area.write_file("dev/side.txt", "side work\n");
    assert_success(&area.forklift("dev", &["load", "side.txt"]));
    assert_success(&area.forklift("dev", &["stack", "side work"]));

    // Retire the agent's key: the distrust boundary snapshots every current pallet head —
    // main's and side's alike, published or not.
    assert_success(&area.forklift("dev", &["config", "operator.identifier", "tester@forklift"]));
    // `office list` does not print users in enrollment order (it groups/sorts by
    // identifier), so the agent's key is found by identifier, not by line position.
    let listing = area.forklift("dev", &["--json", "office", "list"]);
    assert_success(&listing);
    let listing: serde_json::Value = serde_json::from_str(&stdout(&listing)).unwrap();
    let agent_key_id = listing["data"]["users"]
        .as_array().unwrap().iter()
        .find(|user| user["identifier"] == "agent@forklift")
        .expect("the agent is enrolled")
        ["keys"][0]["key_id"]
        .as_str().unwrap().to_string();
    assert_success(&area.forklift("dev", &["office", "retire", &agent_key_id, "--offline"]));

    // Back to `main` (only it ever gets lifted; `side` and its head stay purely local).
    assert_success(&area.forklift("dev", &["shift", "main"]));

    // The origin, where the side pallet's head is still right there: the agent's main-line
    // parcel is fully resolvable, and it is — legitimately — vouched (it's one of the
    // boundary's own snapshotted heads).
    let on_origin = area.forklift("dev", &["--json", "query", "main", "--class", "agent"]);
    assert_success(&on_origin);
    let origin_report: serde_json::Value = serde_json::from_str(&stdout(&on_origin)).unwrap();
    let origin_entry = origin_report["data"]["matches"]
        .as_array().unwrap().iter()
        .find(|entry| entry["parcel"] == agent_parcel)
        .unwrap_or_else(|| panic!("no match for the agent parcel on the origin: {}", origin_report));
    assert_eq!(origin_entry["author"]["trust"], "signed-revoked");
    assert_eq!(
        origin_entry["signer"]["boundary"], "vouched",
        "on the origin, where the side pallet's head is present, the agent parcel resolves \
         to its real answer: {}",
        origin_report
    );

    assert_success(&area.forklift("dev", &["lift"]));

    // A fresh clone: an ordinary (non-sparse) franchise fetches only what the remote
    // actually knows about — `side` was never lifted, so its head never reaches the clone
    // at all.
    let franchised = area.forklift(".", &["franchise", &server.url, "clone"]);
    assert_success(&franchised);

    let on_clone = area.forklift("clone", &["--json", "query", "main", "--class", "agent"]);
    assert_success(&on_clone);
    let clone_report: serde_json::Value = serde_json::from_str(&stdout(&on_clone)).unwrap();
    let clone_entry = clone_report["data"]["matches"]
        .as_array().unwrap().iter()
        .find(|entry| entry["parcel"] == agent_parcel)
        .unwrap_or_else(|| panic!("no match for the agent parcel on the clone: {}", clone_report));
    assert_eq!(clone_entry["author"]["trust"], "signed-revoked");
    assert_eq!(
        clone_entry["signer"]["boundary"], "unresolved",
        "on a clone missing the orphaned boundary head, the question must read unresolved, \
         never suspect: {}",
        clone_report
    );

    // A `signer.boundary eq suspect` predicate must not match the unresolved parcel on the
    // clone (unresolved reads Unknown for this leaf, which never matches either value).
    let suspect_on_clone = area.forklift("clone", &[
        "--json", "query", "main", "--where",
        r#"{"field":"signer.boundary","op":"eq","value":"suspect"}"#,
    ]);
    assert_success(&suspect_on_clone);
    let suspect_report: serde_json::Value = serde_json::from_str(&stdout(&suspect_on_clone)).unwrap();
    let suspect_matches: std::collections::HashSet<String> = suspect_report["data"]["matches"]
        .as_array().unwrap().iter()
        .map(|entry| entry["parcel"].as_str().unwrap().to_string())
        .collect();
    assert!(
        !suspect_matches.contains(&agent_parcel),
        "an unresolved boundary must never satisfy signer.boundary eq suspect: {}",
        suspect_report
    );
}

#[test]
fn a_partial_clone_missing_an_unrelated_boundary_head_still_audits_the_vouched_parcel() {
    // `audit`'s side of the presence guard is asymmetric, on purpose (see
    // `audit_utils::DistrustBoundaryMemo`'s doc): a distrust boundary can name a head this
    // store never has, but `audit` (unlike `query`) must not refuse just because *some*
    // boundary head is missing — only when the parcel under test cannot be vouched *without*
    // it. This construction reuses the query engine's own partial-clone fixture (a
    // never-lifted local "side" pallet's head sits in the boundary alongside main's own) to
    // pin exactly that: the agent's main-line parcel is vouched by main's own boundary
    // snapshot alone, so `audit` must pass on the clone even though `side`'s head never
    // arrived there — the missing head is irrelevant to this parcel, and an audit that
    // refused anyway would be a regression (the same construction's `lift`, run against the
    // very same gap, must keep succeeding: see
    // `a_partial_clone_missing_a_boundary_head_reads_signer_boundary_as_unresolved`, which
    // shares this scenario and predates this test). The complementary case — a boundary that
    // truly *cannot* be resolved, where `audit` must refuse honestly rather than allege
    // tampering — is exercised directly against `verify_pallet_history` in
    // `audit_utils::tests`, because (as the reasoning above implies) a same-pallet parcel's
    // vouching can never genuinely hinge on a head absent from a plain, non-sparse clone of
    // that same pallet: the pallet's own retirement-time snapshot is always present and
    // always sufficient for anything on it that predates the retirement.
    let area = TestArea::new("audit-boundary-unresolved");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/base.txt", "base\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["office", "enroll"]));

    // Admit an agent.
    assert_success(&area.forklift("dev", &["config", "operator.identifier", "agent@forklift"]));
    let keygen = area.forklift("dev", &["office", "keygen"]);
    assert_success(&keygen);
    let admit_args: Vec<String> = stdout(&keygen)
        .lines()
        .find(|line| line.trim_start().starts_with("office admit "))
        .expect("keygen prints the admit line")
        .split_whitespace()
        .skip(2)
        .map(str::to_string)
        .collect();
    assert_success(&area.forklift("dev", &["config", "operator.identifier", "tester@forklift"]));
    assert_success(&area.forklift("dev", &[
        "office", "admit", &admit_args[0], &admit_args[1], &admit_args[2],
        "--agent", "--supervisor", "tester@forklift",
    ]));

    // The agent's main-line parcel: it stays reachable from `main` throughout, and is the one
    // whose revoked-key signature `audit` must resolve against the distrust boundary.
    assert_success(&area.forklift("dev", &["config", "operator.identifier", "agent@forklift"]));
    area.write_file("dev/agent-main.txt", "agent main work\n");
    assert_success(&area.forklift("dev", &["load", "agent-main.txt"]));
    assert_success(&area.forklift("dev", &["stack", "agent main work"]));

    // A second, local-only pallet: its head will sit in the retirement's boundary below but
    // will never reach the remote — nothing ever lifts `side`.
    assert_success(&area.forklift("dev", &["palletize", "side"]));
    area.write_file("dev/side.txt", "side work\n");
    assert_success(&area.forklift("dev", &["load", "side.txt"]));
    assert_success(&area.forklift("dev", &["stack", "side work"]));
    let side_head = std::fs::read_to_string(area.path("dev/.forklift/pallets/side"))
        .expect("the side pallet has a head")
        .trim()
        .to_string();

    // Retire the agent's key: the distrust boundary snapshots every current pallet head —
    // main's and side's alike, published or not.
    assert_success(&area.forklift("dev", &["config", "operator.identifier", "tester@forklift"]));
    let listing = area.forklift("dev", &["--json", "office", "list"]);
    assert_success(&listing);
    let listing: serde_json::Value = serde_json::from_str(&stdout(&listing)).unwrap();
    let agent_key_id = listing["data"]["users"]
        .as_array().unwrap().iter()
        .find(|user| user["identifier"] == "agent@forklift")
        .expect("the agent is enrolled")
        ["keys"][0]["key_id"]
        .as_str().unwrap().to_string();
    assert_success(&area.forklift("dev", &["office", "retire", &agent_key_id, "--offline"]));

    // Back to `main` (only it ever gets lifted; `side` and its head stay purely local).
    assert_success(&area.forklift("dev", &["shift", "main"]));

    // On the origin, where the side pallet's head is still right there, the agent's
    // main-line parcel is fully resolvable and legitimately vouched — audit passes.
    let audit_on_origin = area.forklift("dev", &["audit"]);
    assert_success(&audit_on_origin);

    assert_success(&area.forklift("dev", &["lift"]));

    // A fresh clone: an ordinary (non-sparse) franchise fetches only what the remote
    // actually knows about — `side` was never lifted, so its head never reaches the clone.
    let franchised = area.forklift(".", &["franchise", &server.url, "clone"]);
    assert_success(&franchised);
    assert!(
        !object_store_path(&area, "clone", &side_head).exists(),
        "the side pallet's head must genuinely be absent from the clone for this test to mean \
         anything"
    );

    // `side`'s head never reaching the clone must not stop `audit` from passing: the agent's
    // main-line parcel is reachable from main's own present boundary snapshot regardless.
    let audit_on_clone = area.forklift("clone", &["audit"]);
    assert!(
        audit_on_clone.status.success(),
        "a missing boundary head that this parcel's vouching does not need must never make \
         audit refuse: {}",
        stderr(&audit_on_clone)
    );
    assert!(
        stdout(&audit_on_clone).contains("verified"),
        "{}",
        stdout(&audit_on_clone)
    );
}

#[test]
fn a_sparse_franchise_still_audits() {
    // The meta/office carve-out: a sparse franchise fetches office and every meta pallet at full
    // scope, so a trusted warehouse's offline audit still passes — the office chain is verified
    // with full content, exactly as a full clone would.
    let area = TestArea::new("sparse-audit");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "pre-trust base"]));

    // Establish trust: every parcel from here is signed, and the office pallet carries the keys.
    assert_success(&area.forklift("dev", &["office", "enroll"]));
    area.write_file("dev/src/api/a.txt", "api v2 signed\n");
    assert_success(&area.forklift("dev", &["load", "src/api/a.txt"]));
    assert_success(&area.forklift("dev", &["stack", "signed parcel"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let franchised = area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/api"]);
    assert_success(&franchised);
    assert!(stdout(&franchised).contains("Adopted the remote's trust anchor"), "{}", stdout(&franchised));

    // The audit reads the office chain's full content (present via the carve-out) and the user
    // pallet's signatures (parcels are always fully present) — it passes on the sparse store.
    let audit = area.forklift("sparse", &["audit"]);
    assert_success(&audit);
    assert!(stdout(&audit).contains("Office chain verified"), "{}", stdout(&audit));
    assert!(stdout(&audit).contains("verified"), "{}", stdout(&audit));
}

#[test]
fn lower_into_a_sparse_store_stays_pruned() {
    // A lower into a sparse store fetches new in-scope content and stays pruned on the rest: an
    // out-of-scope change made upstream is never downloaded.
    let area = TestArea::new("sparse-lower");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/api"]));

    // A full clone changes both an in-scope and an out-of-scope file, and lifts.
    assert_success(&area.forklift(".", &["franchise", &server.url, "up"]));
    area.write_file("up/src/api/a.txt", "api v2 from up\n");
    area.write_file("up/src/web/w.txt", "web v2 from up\n");
    assert_success(&area.forklift("up", &["load", "."]));
    assert_success(&area.forklift("up", &["stack", "up edits both"]));
    assert_success(&area.forklift("up", &["lift"]));

    let up_head = pallet_head(&area, "up", "main");
    let new_web_tree = path_object_hash(&area, "up", &up_head, "src/web");

    // The sparse store lowers the in-scope change and stays pruned on the out-of-scope one.
    let lowered = area.forklift("sparse", &["lower"]);
    assert_success(&lowered);
    assert!(stdout(&lowered).contains("Lowered"), "{}", stdout(&lowered));

    assert_eq!(area.read_file("sparse/src/api/a.txt"), "api v2 from up\n");
    assert!(!area.path("sparse/src/web").exists(), "out-of-scope subtree still not materialized");
    assert!(!object_present(&area, "sparse", &new_web_tree),
        "the upstream out-of-scope change must stay sealed, not fetched");
}

#[test]
fn expand_fetches_the_widened_subtree_and_a_bay_can_scope_to_it() {
    // `expand` widens a sparse warehouse's fetch scope and downloads the newly in-scope subtree
    // across history; a bay can then be scoped to the newly available path.
    let area = TestArea::new("sparse-expand");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let web_tree = path_object_hash(&area, "dev", &head, "src/web");
    let web_blob = path_object_hash(&area, "dev", &head, "src/web/w.txt");

    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/api"]));
    assert!(!object_present(&area, "sparse", &web_tree), "src/web is sealed after the sparse franchise");

    // Expand fetches the newly in-scope subtree.
    let expanded = area.forklift("sparse", &["expand", "src/web"]);
    assert_success(&expanded);
    assert!(stdout(&expanded).contains("Expanded"), "{}", stdout(&expanded));
    assert!(object_present(&area, "sparse", &web_tree), "expand must fetch the widened subtree object");
    assert!(object_present(&area, "sparse", &web_blob), "expand must fetch the widened subtree's blob");

    let scope = area.forklift("sparse", &["--json", "scope"]);
    let scope_json: serde_json::Value = serde_json::from_str(&stdout(&scope)).unwrap();
    assert_eq!(scope_json["data"]["fetch_scope"], serde_json::json!(["src/api", "src/web"]), "{}", stdout(&scope));

    // A bay can now be scoped to the newly fetched path, and it materializes it.
    let bay_dir = area.path("sparse-web");
    assert_success(&area.forklift("sparse",
        &["bay", "add", "web", bay_dir.to_str().unwrap(), "--scope", "src/web"]));
    assert_eq!(std::fs::read_to_string(bay_dir.join("src/web/w.txt")).unwrap(), "web v1\n");
}

#[test]
fn a_sparse_franchise_seals_an_out_of_scope_chunked_file_and_expand_fetches_it() {
    // Sparse × chunked (§9.4b Stage 3): an out-of-scope chunked file fetches NOTHING — its recipe
    // is sealed by hash and, by the store invariant "recipe absent ⟹ chunks absent", none of its
    // chunks are ever named or fetched. `expand` into its scope then fetches the recipe AND every
    // chunk, and a bay scoped to it materializes the file byte-for-byte.
    let area = TestArea::new("sparse-chunked");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_large_file("dev/big/giant.bin", 0xBEEF, CHUNK_THRESHOLD + 50_000);
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let recipe = path_object_hash(&area, "dev", &head, "big/giant.bin");
    let chunk = recipe_first_chunk(&area, "dev", &recipe);

    // Sparse franchise excluding the chunked file's subtree.
    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/api"]));

    // Out of scope: neither the recipe nor its chunks were fetched.
    assert!(!object_present(&area, "sparse", &recipe), "the out-of-scope recipe is sealed, not fetched");
    assert!(!object_present(&area, "sparse", &chunk), "an out-of-scope chunk is never fetched");
    assert!(!area.path("sparse/big/giant.bin").exists(), "the out-of-scope giant is not materialized");

    // Expand into the chunked file's scope: the recipe AND its chunks arrive.
    let expanded = area.forklift("sparse", &["expand", "big"]);
    assert_success(&expanded);
    assert!(object_present(&area, "sparse", &recipe), "expand fetches the recipe");
    assert!(object_present(&area, "sparse", &chunk), "expand fetches the recipe's chunks");

    // A bay scoped to the newly-fetched path materializes the giant byte-for-byte.
    let bay_dir = area.path("sparse-big");
    assert_success(&area.forklift("sparse",
        &["bay", "add", "big", bay_dir.to_str().unwrap(), "--scope", "big"]));
    let original = std::fs::read(area.path("dev/big/giant.bin")).unwrap();
    let restored = std::fs::read(bay_dir.join("big/giant.bin")).unwrap();
    assert_eq!(restored, original, "the expanded chunked file materializes byte-for-byte");
}

#[test]
fn narrow_shrinks_the_scope_and_frees_nothing() {
    // `narrow` drops a subtree from this checkout's materialization scope and de-materializes its
    // files, but frees nothing in the object store — the content is still reachable history.
    let area = TestArea::new("sparse-narrow");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/docs/guide.md", "guide v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let docs_tree = path_object_hash(&area, "dev", &head, "docs");
    let docs_blob = path_object_hash(&area, "dev", &head, "docs/guide.md");

    assert_success(&area.forklift(".",
        &["franchise", &server.url, "sparse", "--only", "src/api", "--only", "docs"]));
    assert_eq!(area.read_file("sparse/docs/guide.md"), "guide v1\n");

    // Narrow away docs.
    let narrowed = area.forklift("sparse", &["narrow", "docs"]);
    assert_success(&narrowed);
    assert!(stdout(&narrowed).contains("Narrowed away docs"), "{}", stdout(&narrowed));

    // The files are de-materialized, the in-scope ones stay, and nothing is freed.
    assert!(!area.path("sparse/docs").exists(), "narrow de-materializes the dropped subtree");
    assert_eq!(area.read_file("sparse/src/api/a.txt"), "api v1\n");
    assert!(object_present(&area, "sparse", &docs_tree), "narrow frees nothing: the subtree object stays");
    assert!(object_present(&area, "sparse", &docs_blob), "narrow frees nothing: the blob stays");

    // The materialization scope shrank; a clean stocktake proves the dropped subtree is not
    // reported as a removal.
    let scope = area.forklift("sparse", &["--json", "scope"]);
    let scope_json: serde_json::Value = serde_json::from_str(&stdout(&scope)).unwrap();
    assert_eq!(scope_json["data"]["materialization_scope"], serde_json::json!(["src/api"]), "{}", stdout(&scope));

    let stocktake = area.forklift("sparse", &["stocktake"]);
    assert_success(&stocktake);
    assert!(stdout(&stocktake).contains("matches the inventory"), "{}", stdout(&stocktake));
}

#[test]
fn a_sparse_workspace_refuses_to_lift_to_a_non_origin_remote() {
    // A sparse workspace only ever proved its out-of-scope closure present on its origin, so it
    // refuses to lift to a different remote up front, with a stable code — and even if forced
    // (origin unset), the lift fails loudly rather than silently corrupting the other remote.
    let origin = TestArea::new("non-origin");
    let server1 = Server::start(&origin, None);

    prepare_warehouse(&origin, "dev", &server1.url);
    origin.write_file("dev/src/api/a.txt", "api v1\n");
    origin.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&origin.forklift("dev", &["load", "."]));
    assert_success(&origin.forklift("dev", &["stack", "base"]));
    assert_success(&origin.forklift("dev", &["lift"]));

    assert_success(&origin.forklift(".", &["franchise", &server1.url, "sparse", "--only", "src/api"]));

    // A second, independent remote (empty).
    let server2 = Server::start_at(&origin, "server-root-2", None);
    assert_success(&origin.forklift("sparse", &["config", "remote.url", &server2.url]));

    // The client refuses before touching the wire, with the stable code and exit 11.
    let refused = origin.forklift("sparse", &["--json", "lift"]);
    assert_eq!(refused.status.code(), Some(11), "non-origin lift exits 11: {}", stderr(&refused));
    let json: serde_json::Value = serde_json::from_str(&stdout(&refused)).unwrap();
    assert_eq!(json["error"]["code"], "non_origin_lift", "{}", stdout(&refused));
    assert!(json["error"]["message"].as_str().unwrap().contains(&server1.url),
        "the refusal names the origin: {}", stdout(&refused));

    // Defense in depth: forcing it (origin unset) does not silently succeed — the sparse history
    // cannot be closed against the empty remote, so it fails loudly.
    assert_success(&origin.forklift("sparse", &["config", "--unset", "remote.origin"]));
    let forced = origin.forklift("sparse", &["lift"]);
    assert!(!forced.status.success(), "a forced non-origin sparse lift must fail loudly: {}", stdout(&forced));
}

#[test]
fn lift_and_franchise_round_trip_a_chunked_file() {
    // Chunk transport, end to end (§9.4b Stage 3). A chunk-aware server advertises `chunking` on
    // its handshake, so a chunked file lifts like any other content: the recipe rides the control
    // plane, its chunks ride the blob plane (per-object negotiation + upload), and the ref advances
    // only after the commit-gate closure audit confirms every chunk is present. A fresh franchise
    // then imports the tree+recipe closure (bundles never carry chunks) and fetches each chunk per
    // object, re-assembling the file byte-for-byte — proving both directions of the wire.
    let area = TestArea::new("chunked-round-trip");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/small.txt", "a normal file\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "plain work"]));

    // The plain work lifts fine — the chunk path must not touch an ordinary lift.
    let lifted = area.forklift("dev", &["lift"]);
    assert_success(&lifted);
    assert!(stdout(&lifted).contains("Lifted pallet"), "{}", stdout(&lifted));

    // A chunked giant now lifts too — no refusal against a chunk-aware remote.
    area.write_large_file("dev/big.bin", 0xC0FFEE, CHUNK_THRESHOLD + 50_000);
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "with a giant"]));

    let lifted_giant = area.forklift("dev", &["lift"]);
    assert_success(&lifted_giant);
    assert!(stdout(&lifted_giant).contains("Lifted pallet"),
        "a chunked file lifts to a chunk-aware remote: {}", stdout(&lifted_giant));

    // A fresh franchise re-materializes the chunked file byte-for-byte (chunks fetched per object).
    let franchised = area.forklift(".", &["franchise", &server.url, "check"]);
    assert_success(&franchised);
    assert_eq!(area.read_file("check/small.txt"), "a normal file\n");

    let original = std::fs::read(area.path("dev/big.bin")).unwrap();
    let restored = std::fs::read(area.path("check/big.bin")).unwrap();
    assert_eq!(restored.len(), CHUNK_THRESHOLD + 50_000, "the restored giant is the full size");
    assert_eq!(restored, original, "the chunked file round-trips byte-for-byte");
}

#[test]
fn a_merge_second_parent_below_the_remote_head_lifts_from_a_sparse_store() {
    // The residual from the merge stage's review: a merge whose second parent is an interior
    // ancestor of the remote head (reached via the merge's other side) carries an out-of-scope
    // change a sparse store never fetched. The lift's parcel walk must prune every parcel the
    // remote already has — not just the remote head hash — or it would try to load that sealed
    // object. Two sequential merges build exactly that shape; the lift must still succeed.
    let area = TestArea::new("sparse-second-parent");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    // The sparse workspace under test.
    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/api"]));

    // A full clone drives the remote forward twice, each time changing the out-of-scope subtree.
    assert_success(&area.forklift(".", &["franchise", &server.url, "up"]));
    area.write_file("up/src/web/w.txt", "web v2 (out of scope)\n");
    assert_success(&area.forklift("up", &["load", "."]));
    assert_success(&area.forklift("up", &["stack", "up edits web (1)"]));
    assert_success(&area.forklift("up", &["lift"]));
    let p2 = pallet_head(&area, "up", "main");
    let sealed_web = path_object_hash(&area, "up", &p2, "src/web");

    // The sparse workspace makes an in-scope edit, then merges the diverged remote head p2.
    std::fs::write(area.path("sparse/src/api/a.txt"), "api v2 from sparse\n").unwrap();
    assert_success(&area.forklift("sparse", &["load", "."]));
    assert_success(&area.forklift("sparse", &["stack", "sparse edits api (1)"]));

    let lowered = area.forklift("sparse", &["lower"]);
    assert!(!lowered.status.success(), "the first divergence must be reported");
    assert_success(&area.forklift("sparse", &["palletize", "incoming1", &p2]));
    assert_success(&area.forklift("sparse", &["shift", "main"]));
    assert_success(&area.forklift("sparse", &["consolidate", "incoming1"]));

    // The remote moves forward again, on top of p2 — so p2 becomes an interior ancestor of the
    // new remote head, reachable in the sparse workspace only through the first merge's parent.
    assert_success(&area.forklift("up", &["lower"]));
    area.write_file("up/src/web/w.txt", "web v3 (out of scope)\n");
    assert_success(&area.forklift("up", &["load", "."]));
    assert_success(&area.forklift("up", &["stack", "up edits web (2)"]));
    assert_success(&area.forklift("up", &["lift"]));
    let p3 = pallet_head(&area, "up", "main");

    // The sparse workspace merges p3 too — building the second merge whose ancestry re-reaches p2.
    let lowered = area.forklift("sparse", &["lower"]);
    assert!(!lowered.status.success(), "the second divergence must be reported");
    assert_success(&area.forklift("sparse", &["palletize", "incoming2", &p3]));
    assert_success(&area.forklift("sparse", &["shift", "main"]));
    assert_success(&area.forklift("sparse", &["consolidate", "incoming2"]));

    // p2's out-of-scope subtree was never fetched into the sparse store.
    assert!(!object_present(&area, "sparse", &sealed_web),
        "the interior merge parent's out-of-scope object must be sealed, not fetched");

    // The lift must succeed: the parcel walk prunes p2 (and everything the remote already has),
    // never loading the sealed object; the remote holds p2's whole closure.
    let lifted = area.forklift("sparse", &["lift"]);
    assert_success(&lifted);
    assert!(stdout(&lifted).contains("Lifted") || stdout(&lifted).contains("up to date"),
        "the two-merge sparse lift must go through: {}", stdout(&lifted));
}

#[test]
fn narrow_refuses_to_delete_dirty_tracked_changes() {
    // narrow has no working-directory-preserving path the way shift does — its delete is
    // unconditional once it decides to act. An unstaged edit and a staged (loaded) edit under
    // the narrowed prefix must both block it, naming the stable code, rather than be silently
    // discarded.
    let area = TestArea::new("sparse-narrow-dirty-tracked");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/docs/guide.md", "guide v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    assert_success(&area.forklift(".",
        &["franchise", &server.url, "sparse", "--only", "src/api", "--only", "docs"]));

    // An unstaged edit blocks narrowing "docs".
    std::fs::write(area.path("sparse/docs/guide.md"), "guide v2 (unsaved)\n").unwrap();

    let refused = area.forklift("sparse", &["--json", "narrow", "docs"]);
    assert_eq!(refused.status.code(), Some(12), "narrow_unclean exits 12: {}", stderr(&refused));
    let json: serde_json::Value = serde_json::from_str(&stdout(&refused)).unwrap();
    assert_eq!(json["error"]["code"], "narrow_unclean", "{}", stdout(&refused));
    assert_eq!(area.read_file("sparse/docs/guide.md"), "guide v2 (unsaved)\n",
        "the unstaged edit must survive a refused narrow");

    // A staged (loaded) edit blocks narrowing "src/api" too.
    std::fs::write(area.path("sparse/src/api/a.txt"), "api v2 staged\n").unwrap();
    assert_success(&area.forklift("sparse", &["load", "src/api/a.txt"]));

    let refused2 = area.forklift("sparse", &["--json", "narrow", "src/api"]);
    assert_eq!(refused2.status.code(), Some(12), "narrow_unclean exits 12: {}", stderr(&refused2));
    assert_eq!(area.read_file("sparse/src/api/a.txt"), "api v2 staged\n",
        "the staged edit must survive a refused narrow");

    // Neither prefix was dropped: the materialization scope is unchanged.
    let scope = area.forklift("sparse", &["--json", "scope"]);
    let scope_json: serde_json::Value = serde_json::from_str(&stdout(&scope)).unwrap();
    assert_eq!(scope_json["data"]["materialization_scope"], serde_json::json!(["docs", "src/api"]),
        "{}", stdout(&scope));
}

#[test]
fn narrow_refuses_to_delete_untracked_files() {
    // An untracked file under the narrowed prefix — never loaded, never stacked — must also
    // block narrow: it is uncommitted work, even though it was never tracked.
    let area = TestArea::new("sparse-narrow-untracked");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/docs/guide.md", "guide v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    assert_success(&area.forklift(".",
        &["franchise", &server.url, "sparse", "--only", "src/api", "--only", "docs"]));

    area.write_file("sparse/docs/scratch.md", "not tracked\n");

    let refused = area.forklift("sparse", &["--json", "narrow", "docs"]);
    assert_eq!(refused.status.code(), Some(12), "narrow_unclean exits 12: {}", stderr(&refused));
    let json: serde_json::Value = serde_json::from_str(&stdout(&refused)).unwrap();
    assert_eq!(json["error"]["code"], "narrow_unclean", "{}", stdout(&refused));
    assert!(json["error"]["message"].as_str().unwrap().contains("scratch.md"),
        "the refusal names the untracked file: {}", stdout(&refused));

    assert_eq!(area.read_file("sparse/docs/scratch.md"), "not tracked\n",
        "the untracked file must survive a refused narrow");
    assert!(area.path("sparse/docs/guide.md").exists(), "tracked content must survive too");
}

#[test]
fn bay_add_scope_refuses_a_spine_ancestor_outside_the_fetch_scope() {
    // A bay scope wider than the warehouse fetch scope (here, a spine ancestor whose sibling was
    // never fetched) must be refused cleanly and up front — never half-create the bay (a
    // registered pallet ref, a working directory, a redirect) before materialize discovers the
    // problem.
    let area = TestArea::new("sparse-bay-scope-spine");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/api"]));

    let bay_dir = area.path("sparse-src");
    let refused = area.forklift("sparse",
        &["bay", "add", "work", bay_dir.to_str().unwrap(), "--scope", "src"]);
    assert!(!refused.status.success(), "a bay scope wider than the fetch scope must be refused");
    assert!(stderr(&refused).contains("fetch scope"), "{}", stderr(&refused));
    assert!(stderr(&refused).contains("expand"), "the refusal names expand: {}", stderr(&refused));

    // Nothing was left behind: no working directory, no bay registered, no pallet ref.
    assert!(!bay_dir.exists(), "a refused bay add must not create the working directory");
    let list = area.forklift("sparse", &["bay"]);
    assert_success(&list);
    assert!(!stdout(&list).contains("work"), "a refused bay add must not register the bay: {}", stdout(&list));
    assert!(!area.path("sparse/.forklift/pallets/work").exists(),
        "a refused bay add must not create a pallet ref");
}

#[test]
fn bay_add_scope_refuses_a_sealed_sibling_outside_the_fetch_scope() {
    // A bay scope naming an out-of-scope path directly (the sealed sibling itself, not a spine
    // ancestor) must also refuse cleanly — never a raw "object not found" from trying to load an
    // object that was never fetched.
    let area = TestArea::new("sparse-bay-scope-sealed");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/api"]));

    let bay_dir = area.path("sparse-web");
    let refused = area.forklift("sparse",
        &["bay", "add", "work", bay_dir.to_str().unwrap(), "--scope", "src/web"]);
    assert!(!refused.status.success(), "a bay scope outside the fetch scope must be refused");
    assert!(stderr(&refused).contains("fetch scope"), "a clean scope refusal, not a raw object error: {}", stderr(&refused));
    assert!(stderr(&refused).contains("expand"), "the refusal names expand: {}", stderr(&refused));

    assert!(!bay_dir.exists(), "a refused bay add must not create the working directory");
}

#[test]
fn expand_recovers_after_an_unreachable_remote_leaves_the_scope_unchanged() {
    // The crash-consistency property: expand must not persist the widened fetch scope until the
    // content actually landed. Simulated deterministically by pointing at an unreachable remote
    // (the same failure shape as a fetch that never completes) — the fetch scope must come back
    // unchanged, and a plain re-run against the real remote must then complete on its own,
    // proving the self-heal a premature scope write would have defeated.
    let area = TestArea::new("sparse-expand-interrupt");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let web_tree = path_object_hash(&area, "dev", &head, "src/web");

    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/api"]));

    // Point at an unreachable remote and attempt to expand.
    assert_success(&area.forklift("sparse", &["config", "remote.url", "http://127.0.0.1:9"]));
    let failed = area.forklift("sparse", &["expand", "src/web"]);
    assert!(!failed.status.success(), "expand against an unreachable remote must fail");

    // The fetch scope must be unchanged — not left claiming "src/web" is in scope while nothing
    // was actually fetched (that would make the next expand see it as already-in-scope and
    // no-op, defeating self-heal).
    let scope = area.forklift("sparse", &["--json", "scope"]);
    let scope_json: serde_json::Value = serde_json::from_str(&stdout(&scope)).unwrap();
    assert_eq!(scope_json["data"]["fetch_scope"], serde_json::json!(["src/api"]),
        "a failed expand must not persist the widened scope: {}", stdout(&scope));
    assert!(!object_present(&area, "sparse", &web_tree), "nothing was fetched by the failed attempt");

    // Point back at the real remote: a plain re-run completes the fetch.
    assert_success(&area.forklift("sparse", &["config", "remote.url", &server.url]));
    let recovered = area.forklift("sparse", &["expand", "src/web"]);
    assert_success(&recovered);
    assert!(stdout(&recovered).contains("Expanded"), "{}", stdout(&recovered));
    assert!(object_present(&area, "sparse", &web_tree), "the re-run must complete the fetch");

    let scope2 = area.forklift("sparse", &["--json", "scope"]);
    let scope2_json: serde_json::Value = serde_json::from_str(&stdout(&scope2)).unwrap();
    assert_eq!(scope2_json["data"]["fetch_scope"], serde_json::json!(["src/api", "src/web"]),
        "{}", stdout(&scope2));
}

#[test]
fn a_sparse_franchise_with_a_typo_d_only_path_leaves_no_state_behind() {
    // A typo'd --only path (naming nothing in the head) must be rejected before the fetch scope,
    // origin or pallet head are ever written — a fresh, discarded directory should not also be a
    // scope-inconsistent warehouse an operator has to notice.
    let area = TestArea::new("sparse-franchise-typo");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    // "src/ap" (missing the final "i") names nothing in the head.
    let failed = area.forklift(".", &["franchise", &server.url, "typo", "--only", "src/ap"]);
    assert!(!failed.status.success(), "a typo'd --only path must be refused");
    assert!(stderr(&failed).contains("Nothing was recorded"), "{}", stderr(&failed));

    let dir = area.path("typo");
    assert!(!dir.join(".forklift/config/fetch-scope").exists(), "no fetch scope was persisted");
    assert!(!dir.join(".forklift/pallets/main").exists(), "no pallet head was recorded");

    let origin = std::fs::read_to_string(dir.join(".forklift/config/warehouse.toml")).unwrap_or_default();
    assert!(!origin.contains("origin"), "no remote origin was recorded: {}", origin);
}

#[test]
fn lift_from_a_sparse_workspace_with_no_remote_configured_reports_the_plain_error() {
    // The origin guard must not fire when remote.url is simply unset: that is a different, plain
    // "no remote configured" problem. Treating "unset" as "configured to the empty string" would
    // wrongly compare it against the recorded origin and mask the real error behind a confusing
    // "lifting to \"\"" non-origin refusal.
    let area = TestArea::new("sparse-no-remote");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/api"]));
    assert_success(&area.forklift("sparse", &["config", "--unset", "remote.url"]));

    let failed = area.forklift("sparse", &["--json", "lift"]);
    assert!(!failed.status.success(), "lift with no remote configured must fail");
    assert_ne!(failed.status.code(), Some(11), "must not be misclassified as non_origin_lift: {}", stdout(&failed));

    let json: serde_json::Value = serde_json::from_str(&stdout(&failed)).unwrap();
    assert_ne!(json["error"]["code"], serde_json::json!("non_origin_lift"), "{}", stdout(&failed));
    assert!(json["error"]["message"].as_str().unwrap().contains("No remote is configured"),
        "{}", stdout(&failed));
}

#[test]
fn a_sparse_audit_names_its_scope_boundary() {
    // Sparse-audit honesty: signatures are verified in full and in-scope content is verified by
    // re-hash, but the rest is sealed by hash — and the audit says so in a distinct statement a
    // full-clone audit never prints. A sealed out-of-scope subtree does not fail the audit; the
    // human output and the --json envelope both carry the scope boundary.
    let area = TestArea::new("sparse-audit-boundary");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "pre-trust base"]));
    assert_success(&area.forklift("dev", &["office", "enroll"]));
    area.write_file("dev/src/api/a.txt", "api v2 signed\n");
    assert_success(&area.forklift("dev", &["load", "src/api/a.txt"]));
    assert_success(&area.forklift("dev", &["stack", "signed parcel"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let web_tree = path_object_hash(&area, "dev", &head, "src/web");

    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/api"]));

    // The out-of-scope subtree was sealed, not fetched — yet the audit passes over it.
    assert!(!object_present(&area, "sparse", &web_tree),
        "the out-of-scope subtree must be sealed, not fetched");

    let audit = area.forklift("sparse", &["audit"]);
    assert_success(&audit);
    let report = stdout(&audit);
    assert!(report.contains("Office chain verified"), "the office chain is verified in full: {}", report);
    assert!(report.contains("sparse"), "the boundary names the store as sparse: {}", report);
    assert!(report.contains("src/api"), "the boundary names the fetched scope: {}", report);
    assert!(report.contains("sealed by hash"), "out-of-scope content is stated sealed: {}", report);
    assert!(report.contains("advisory"), "the boundary states enforcement is advisory: {}", report);

    // --json carries the scope object only on a sparse run.
    let audit_json = area.forklift("sparse", &["--json", "audit"]);
    assert_success(&audit_json);
    let json: serde_json::Value = serde_json::from_str(&stdout(&audit_json)).unwrap();
    let scope = &json["data"]["scope"];
    assert_eq!(scope["fetch_scope"], serde_json::json!(["src/api"]), "{}", stdout(&audit_json));
    assert_eq!(scope["signatures"], serde_json::json!("verified"), "{}", stdout(&audit_json));
    assert_eq!(scope["in_scope_content"], serde_json::json!("verified"), "{}", stdout(&audit_json));
    assert_eq!(scope["out_of_scope_content"], serde_json::json!("sealed"), "{}", stdout(&audit_json));
    assert_eq!(scope["enforcement"], serde_json::json!("advisory"), "{}", stdout(&audit_json));
}

#[test]
fn a_sparse_audit_fails_on_a_tampered_in_scope_tree() {
    // The scoped closure loads every in-scope tree, and a content-addressed read re-hashes it,
    // so a corrupted in-scope subtree object is caught — the seal covers only what was never
    // fetched, never the in-scope content the audit re-verifies.
    let area = TestArea::new("sparse-audit-tamper");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "pre-trust base"]));
    assert_success(&area.forklift("dev", &["office", "enroll"]));
    area.write_file("dev/src/api/a.txt", "api v2 signed\n");
    assert_success(&area.forklift("dev", &["load", "src/api/a.txt"]));
    assert_success(&area.forklift("dev", &["stack", "signed parcel"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let api_tree = path_object_hash(&area, "dev", &head, "src/api");
    let api_blob = path_object_hash(&area, "dev", &head, "src/api/a.txt");

    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/api"]));

    // A healthy sparse audit first — the store is valid.
    assert_success(&area.forklift("sparse", &["audit"]));

    // Overwrite the in-scope subtree object with another valid object's bytes: the read
    // decompresses cleanly, but the re-hash no longer matches the name it was fetched under.
    let blob_bytes = std::fs::read(object_store_path(&area, "sparse", &api_blob)).unwrap();
    std::fs::write(object_store_path(&area, "sparse", &api_tree), &blob_bytes).unwrap();

    let tampered = area.forklift("sparse", &["audit"]);
    assert!(!tampered.status.success(),
        "a corrupted in-scope tree must fail the sparse audit: {}", stdout(&tampered));
}

#[test]
fn a_content_corrupted_in_scope_blob_passes_closure_but_fails_on_read() {
    // The limitation, stated honestly: audit presence-checks blobs, it does not re-read their
    // content — so a present-but-corrupted in-scope blob passes the closure check (true of a
    // full-store audit too) and is caught only when it is next actually read.
    let area = TestArea::new("sparse-audit-blob");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "pre-trust base"]));
    assert_success(&area.forklift("dev", &["office", "enroll"]));
    area.write_file("dev/src/api/a.txt", "api v2 signed\n");
    assert_success(&area.forklift("dev", &["load", "src/api/a.txt"]));
    assert_success(&area.forklift("dev", &["stack", "signed parcel"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let api_blob = path_object_hash(&area, "dev", &head, "src/api/a.txt");

    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/api"]));
    let sparse_head = pallet_head(&area, "sparse", "main");

    // Corrupt the blob's bytes but leave the file present — a degraded-on-disk in-scope blob.
    std::fs::write(object_store_path(&area, "sparse", &api_blob), b"corrupted-not-valid-zstd").unwrap();

    let _scope = forklift_core::globals::StorageRootScope::enter(&area.path("sparse"));
    let fetch_scope = forklift_core::util::scope_utils::read_fetch_scope().unwrap();

    // Closure passes: the blob is present, and a normal audit only presence-checks blobs.
    forklift_core::util::audit_utils::verify_parcel_closure_scoped(&sparse_head, None, &fetch_scope, false)
        .expect("a present (if content-corrupted) in-scope blob passes the presence-only closure check");

    // Reading it does catch the corruption — the content-addressed read re-hashes.
    assert!(forklift_core::util::object_utils::load_blob(&api_blob).is_err(),
        "the corrupted blob must fail when it is actually read");
}

#[test]
fn path_maybe_changed_absorbs_an_absent_out_of_scope_object() {
    // The cold-path hardening: computing a parcel's changed-path filter diffs its tree against
    // its parent's, which in a sparse store descends toward a sealed out-of-scope subtree the
    // store never fetched. path_maybe_changed absorbs that absence into the honest "maybe"
    // answer itself — every caller is safe by construction, not only blame's defensive wrapper.
    let area = TestArea::new("sparse-maybe-changed");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    // A second parcel changes ONLY the out-of-scope subtree, so a filter diff must descend into
    // it to be computed at all.
    area.write_file("dev/src/web/w.txt", "web v2\n");
    assert_success(&area.forklift("dev", &["load", "src/web/w.txt"]));
    assert_success(&area.forklift("dev", &["stack", "web only"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let web_tree = path_object_hash(&area, "dev", &head, "src/web");

    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/api"]));
    let sparse_head = pallet_head(&area, "sparse", "main");

    // The changed out-of-scope subtree the filter computation must descend into was never fetched.
    assert!(!object_present(&area, "sparse", &web_tree), "the changed out-of-scope subtree must be sealed");

    // Force the cold path: drop any precomputed filter so the head parcel's filter is recomputed.
    let _ = std::fs::remove_dir_all(area.path("sparse/.forklift/graph"));

    let _scope = forklift_core::globals::StorageRootScope::enter(&area.path("sparse"));
    // Called directly — no `.unwrap_or(true)` — the function itself must degrade to Ok(true)
    // rather than propagate the absent-object read error out of a total query.
    let answer = forklift_core::util::graph_utils::path_maybe_changed(&sparse_head, "src/api/a.txt");
    assert_eq!(answer, Ok(true),
        "the cold path must absorb the absent out-of-scope object into a total \"maybe\", not an Err");
}

#[test]
fn a_full_store_audit_omits_the_sparse_boundary() {
    // The referee: a full (unscoped) store's audit is byte-for-byte what it always was — no
    // scope boundary line, no scope field — so a sparse pass can never be confused for one.
    let area = TestArea::new("full-audit-referee");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "pre-trust base"]));
    assert_success(&area.forklift("dev", &["office", "enroll"]));
    area.write_file("dev/src/api/a.txt", "api v2 signed\n");
    assert_success(&area.forklift("dev", &["load", "src/api/a.txt"]));
    assert_success(&area.forklift("dev", &["stack", "signed parcel"]));
    assert_success(&area.forklift("dev", &["lift"]));

    assert_success(&area.forklift(".", &["franchise", &server.url, "full"]));

    let audit = area.forklift("full", &["audit"]);
    assert_success(&audit);
    let report = stdout(&audit);
    assert!(report.contains("Office chain verified"), "{}", report);
    assert!(!report.contains("sparse"), "a full-store audit must not print the sparse boundary: {}", report);
    assert!(!report.contains("sealed by hash"), "{}", report);

    let audit_json = area.forklift("full", &["--json", "audit"]);
    assert_success(&audit_json);
    let json: serde_json::Value = serde_json::from_str(&stdout(&audit_json)).unwrap();
    assert!(json["data"]["scope"].is_null(),
        "a full-store audit --json carries no scope field: {}", stdout(&audit_json));
}

#[test]
fn scope_prune_frees_a_path_no_checkout_needs_and_leaves_the_rest_intact() {
    // The scope-prune headline: on a sparse warehouse with two bays of different scopes, prune a
    // fetched path no checkout materializes. Its content is freed; every other scope's content
    // survives; the store still audits (the pruned path re-enters the sealed-but-unfetched
    // state, never "missing"); and the path is re-fetchable with expand.
    let area = TestArea::new("scope-prune-two-bays");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    area.write_file("dev/docs/guide.md", "guide v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));

    // Establish trust so a later audit verifies a real office chain — and so the office/meta
    // carve-out (never scoped, never pruned) is exercised end to end.
    assert_success(&area.forklift("dev", &["office", "enroll"]));
    area.write_file("dev/src/api/a.txt", "api v2 signed\n");
    assert_success(&area.forklift("dev", &["load", "src/api/a.txt"]));
    assert_success(&area.forklift("dev", &["stack", "signed"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let docs_tree = path_object_hash(&area, "dev", &head, "docs");
    let docs_blob = path_object_hash(&area, "dev", &head, "docs/guide.md");
    let api_tree = path_object_hash(&area, "dev", &head, "src/api");
    let web_tree = path_object_hash(&area, "dev", &head, "src/web");
    let web_blob = path_object_hash(&area, "dev", &head, "src/web/w.txt");

    // A sparse franchise of all three paths, then two bays of different scopes on the one store.
    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse",
        "--only", "src/api", "--only", "src/web", "--only", "docs"]));

    let apibay = area.path("bay-api");
    assert_success(&area.forklift("sparse",
        &["bay", "add", "apibay", apibay.to_str().unwrap(), "--scope", "src/api"]));
    let webbay = area.path("bay-web");
    assert_success(&area.forklift("sparse",
        &["bay", "add", "webbay", webbay.to_str().unwrap(), "--scope", "src/web"]));

    // The main checkout still materializes docs, so a prune of docs refuses until it narrows.
    let blocked = area.forklift("sparse", &["--json", "scope-prune", "docs"]);
    assert_eq!(blocked.status.code(), Some(13),
        "a materialized path blocks the prune: {}", stderr(&blocked));

    // Narrow docs away (the main checkout is the only one that had it), then prune it.
    assert_success(&area.forklift("sparse", &["narrow", "docs"]));

    let pruned = area.forklift("sparse", &["scope-prune", "docs"]);
    assert_success(&pruned);
    assert!(stdout(&pruned).contains("Pruned docs"), "{}", stdout(&pruned));
    assert!(stdout(&pruned).contains("freed 2"),
        "docs' subtree object and blob are freed: {}", stdout(&pruned));

    // docs' content is gone; every other scope's content — and the office/meta chain — survives.
    assert!(!object_present(&area, "sparse", &docs_tree), "the pruned subtree object is freed");
    assert!(!object_present(&area, "sparse", &docs_blob), "the pruned blob is freed");
    assert!(object_present(&area, "sparse", &api_tree), "bay apibay's content survives");
    assert!(object_present(&area, "sparse", &web_tree), "bay webbay's content survives");
    assert!(object_present(&area, "sparse", &web_blob), "bay webbay's content survives");

    // The fetch scope was narrowed — docs is sealed, not missing — so the store still audits...
    let scope = area.forklift("sparse", &["--json", "scope"]);
    let scope_json: serde_json::Value = serde_json::from_str(&stdout(&scope)).unwrap();
    assert_eq!(scope_json["data"]["fetch_scope"], serde_json::json!(["src/api", "src/web"]),
        "{}", stdout(&scope));

    let audit = area.forklift("sparse", &["audit"]);
    assert_success(&audit);

    // ...stocktake is clean...
    let stocktake = area.forklift("sparse", &["stocktake"]);
    assert_success(&stocktake);
    assert!(stdout(&stocktake).contains("matches the inventory"), "{}", stdout(&stocktake));

    // ...an in-scope edit still stacks and lifts (the sealed docs are never needed)...
    std::fs::write(area.path("sparse/src/api/a.txt"), "api v3 after prune\n").unwrap();
    assert_success(&area.forklift("sparse", &["load", "."]));
    assert_success(&area.forklift("sparse", &["stack", "edit after prune"]));
    assert_success(&area.forklift("sparse", &["lift"]));

    // ...and the pruned path is re-fetchable from the origin, proving it was only sealed.
    let expanded = area.forklift("sparse", &["expand", "docs"]);
    assert_success(&expanded);
    assert!(object_present(&area, "sparse", &docs_tree), "expand re-fetches the pruned subtree object");
    assert!(object_present(&area, "sparse", &docs_blob), "expand re-fetches the pruned blob");
}

#[test]
fn scope_prune_refuses_while_a_bay_still_materializes_the_path() {
    // The multi-bay hazard: one object store is shared across checkouts, so pruning a path a bay
    // still materializes would break that bay. The prune refuses, naming the bay, and the bay's
    // objects are untouched — the bay's scope protects them from the prune.
    let area = TestArea::new("scope-prune-hazard");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let web_tree = path_object_hash(&area, "dev", &head, "src/web");
    let web_blob = path_object_hash(&area, "dev", &head, "src/web/w.txt");

    assert_success(&area.forklift(".",
        &["franchise", &server.url, "sparse", "--only", "src/api", "--only", "src/web"]));

    // A bay scopes src/web; the main checkout narrows it away — so only the bay still needs it.
    let webbay = area.path("bay-web");
    assert_success(&area.forklift("sparse",
        &["bay", "add", "webbay", webbay.to_str().unwrap(), "--scope", "src/web"]));
    assert_success(&area.forklift("sparse", &["narrow", "src/web"]));

    // The prune refuses: the bay still materializes src/web. Exit 13, stable code, names the bay.
    let refused = area.forklift("sparse", &["--json", "scope-prune", "src/web"]);
    assert_eq!(refused.status.code(), Some(13), "the bay blocks the prune: {}", stderr(&refused));
    let json: serde_json::Value = serde_json::from_str(&stdout(&refused)).unwrap();
    assert_eq!(json["error"]["code"], "scope_prune_blocked", "{}", stdout(&refused));
    assert!(json["error"]["message"].as_str().unwrap().contains("webbay"),
        "the refusal names the blocking bay: {}", stdout(&refused));

    // The bay's objects — and the fetch scope — are untouched: the refusal protected them.
    assert!(object_present(&area, "sparse", &web_tree), "the bay's subtree object survives the refused prune");
    assert!(object_present(&area, "sparse", &web_blob), "the bay's blob survives the refused prune");

    let scope = area.forklift("sparse", &["--json", "scope"]);
    let scope_json: serde_json::Value = serde_json::from_str(&stdout(&scope)).unwrap();
    assert_eq!(scope_json["data"]["fetch_scope"], serde_json::json!(["src/api", "src/web"]),
        "the refused prune left the fetch scope unchanged: {}", stdout(&scope));
}

#[test]
fn scope_prune_refuses_on_a_full_warehouse() {
    // A full (non-sparse) warehouse holds the whole tree by design; there is nothing to prune.
    let area = TestArea::new("scope-prune-full");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    assert_success(&area.forklift(".", &["franchise", &server.url, "full"]));
    let head = pallet_head(&area, "full", "main");
    let web_tree = path_object_hash(&area, "full", &head, "src/web");

    let refused = area.forklift("full", &["--json", "scope-prune", "src/web"]);
    assert!(!refused.status.success(), "a full warehouse has nothing to prune: {}", stdout(&refused));
    let json: serde_json::Value = serde_json::from_str(&stdout(&refused)).unwrap();
    assert!(json["error"]["message"].as_str().unwrap().contains("full tree"),
        "the refusal explains the warehouse is full: {}", stdout(&refused));

    assert!(object_present(&area, "full", &web_tree), "nothing is freed on a full warehouse");
}

#[test]
fn scope_prune_dry_run_reports_and_frees_nothing() {
    // A dry run states what it would free and changes nothing — no object deleted, no fetch scope
    // narrowed. A destructive verb must let the operator look before it leaps.
    let area = TestArea::new("scope-prune-dry-run");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let web_tree = path_object_hash(&area, "dev", &head, "src/web");
    let web_blob = path_object_hash(&area, "dev", &head, "src/web/w.txt");

    assert_success(&area.forklift(".",
        &["franchise", &server.url, "sparse", "--only", "src/api", "--only", "src/web"]));
    assert_success(&area.forklift("sparse", &["narrow", "src/web"]));

    let dry = area.forklift("sparse", &["--json", "scope-prune", "src/web", "--dry-run"]);
    assert_success(&dry);
    let json: serde_json::Value = serde_json::from_str(&stdout(&dry)).unwrap();
    assert_eq!(json["data"]["dry_run"], serde_json::json!(true), "{}", stdout(&dry));
    assert_eq!(json["data"]["would_free"], serde_json::json!(2),
        "the dry run counts the freeable objects: {}", stdout(&dry));
    assert_eq!(json["data"]["freed"], serde_json::json!(0), "a dry run frees nothing: {}", stdout(&dry));

    // Nothing changed: the objects are present and the fetch scope is unchanged.
    assert!(object_present(&area, "sparse", &web_tree), "a dry run frees no object");
    assert!(object_present(&area, "sparse", &web_blob), "a dry run frees no object");
    let scope = area.forklift("sparse", &["--json", "scope"]);
    let scope_json: serde_json::Value = serde_json::from_str(&stdout(&scope)).unwrap();
    assert_eq!(scope_json["data"]["fetch_scope"], serde_json::json!(["src/api", "src/web"]),
        "a dry run leaves the fetch scope unchanged: {}", stdout(&scope));
}

#[test]
fn scope_prune_resumes_after_an_interrupted_free_and_completes() {
    // Simulate a crash mid-prune: the fetch scope was already narrowed (the durable half of a
    // prune) but freeing the objects (the destructive half) was interrupted before it finished.
    // A bare re-run of the SAME path must not refuse "not a fetched path" — it must detect the
    // already-pruned case, re-derive the closure against the now-narrowed scope, and finish
    // freeing whatever the interrupted run left behind. The store stays healthy throughout.
    let area = TestArea::new("scope-prune-resume");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));

    // Trust, so the audit at the end verifies something real, not just "nothing to check."
    assert_success(&area.forklift("dev", &["office", "enroll"]));
    area.write_file("dev/src/api/a.txt", "api v2 signed\n");
    assert_success(&area.forklift("dev", &["load", "src/api/a.txt"]));
    assert_success(&area.forklift("dev", &["stack", "signed"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let web_tree = path_object_hash(&area, "dev", &head, "src/web");
    let web_blob = path_object_hash(&area, "dev", &head, "src/web/w.txt");

    assert_success(&area.forklift(".",
        &["franchise", &server.url, "sparse", "--only", "src/api", "--only", "src/web"]));
    assert_success(&area.forklift("sparse", &["narrow", "src/web"]));

    // Simulate an interrupted prune by hand: narrow the shared fetch scope directly (step one of
    // a real prune, already durable) and delete only the blob — the child, freed before its
    // parent tree in a real run's order — leaving the parent tree behind (step two, interrupted).
    std::fs::write(area.path("sparse/.forklift/config/fetch-scope"), "src/api\n").unwrap();
    std::fs::remove_file(object_store_path(&area, "sparse", &web_blob)).unwrap();
    assert!(object_present(&area, "sparse", &web_tree),
        "the simulated interruption leaves the parent tree behind");

    // A bare re-run of "src/web" — no longer a fetch-scope prefix — resumes instead of refusing.
    let resumed = area.forklift("sparse", &["--json", "scope-prune", "src/web"]);
    assert_success(&resumed);
    let json: serde_json::Value = serde_json::from_str(&stdout(&resumed)).unwrap();
    assert!(json["ok"].as_bool().unwrap(), "a resumed prune must not error: {}", stdout(&resumed));
    assert_eq!(json["data"]["freed"], serde_json::json!(1),
        "the resumed prune frees exactly what the interruption left: {}", stdout(&resumed));

    // The rest of the plan is now gone, and the store is still healthy.
    assert!(!object_present(&area, "sparse", &web_tree), "the resumed prune frees the rest of the plan");
    assert_success(&area.forklift("sparse", &["audit"]));
    let stocktake = area.forklift("sparse", &["stocktake"]);
    assert_success(&stocktake);
    assert!(stdout(&stocktake).contains("matches the inventory"), "{}", stdout(&stocktake));

    // Running it again, now that nothing is left, is a clean no-op, not an error.
    let again = area.forklift("sparse", &["--json", "scope-prune", "src/web"]);
    assert_success(&again);
    let again_json: serde_json::Value = serde_json::from_str(&stdout(&again)).unwrap();
    assert_eq!(again_json["data"]["freed"], serde_json::json!(0),
        "a second resume finds nothing left to free: {}", stdout(&again));
}

#[test]
fn scope_prune_reports_zero_freed_as_retained_not_already_pruned() {
    // A FRESH prune can free zero loose objects for a reason that has nothing to do with an
    // earlier, interrupted run: the pruned path's entire content is byte-identical to (and thus
    // content-addressed to the same hash as) content a still-fetched path keeps. Saying "already
    // pruned" here would be a lie — nothing was ever pruned before this call — so the report must
    // say the content is retained by other scopes instead.
    let area = TestArea::new("scope-prune-shared");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    // src/web has the exact same entry name and content as src/api, so their tree objects are
    // content-addressed to the SAME hash too — not just the blob. That makes the pruned
    // subtree's whole closure (tree and blob alike) shared with the retained src/api, so a fresh
    // prune of src/web frees nothing at all.
    area.write_file("dev/src/api/same.txt", "identical bytes\n");
    area.write_file("dev/src/web/same.txt", "identical bytes\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let web_tree = path_object_hash(&area, "dev", &head, "src/web");
    let api_tree = path_object_hash(&area, "dev", &head, "src/api");
    let web_blob = path_object_hash(&area, "dev", &head, "src/web/same.txt");
    assert_eq!(web_tree, api_tree, "the fixture's subtrees must collide by content-addressing");

    assert_success(&area.forklift(".",
        &["franchise", &server.url, "sparse", "--only", "src/api", "--only", "src/web"]));
    assert_success(&area.forklift("sparse", &["narrow", "src/web"]));

    // A fresh prune (src/web was a live fetch-scope prefix until this call) that frees nothing:
    // both the tree and the blob under src/web are retained via src/api.
    let pruned = area.forklift("sparse", &["scope-prune", "src/web"]);
    assert_success(&pruned);
    let report = stdout(&pruned);
    assert!(report.contains("retained by other scopes"),
        "a fresh zero-freed prune must explain the content is retained, not call it already pruned: {}",
        report);
    assert!(!report.contains("already pruned"),
        "a fresh prune must never claim to be a resume: {}", report);

    // The shared content really did survive, proving the message is not just optimistic wording.
    assert!(object_present(&area, "sparse", &web_tree),
        "the tree shared with src/api must survive the prune of src/web");
    assert!(object_present(&area, "sparse", &web_blob),
        "the blob shared with src/api must survive the prune of src/web");
}

#[test]
fn scope_prune_retains_a_user_blob_that_collides_with_an_office_blob() {
    // The mechanism the multi-bay/meta-carve-out retention depends on, pinned directly: prune's
    // retained set is a hash SET spanning meta (full closure) and in-scope user content, so a
    // pruned user blob that happens to share its hash with an office record (identical bytes,
    // content-addressing) must survive — and the office chain, which needs that exact content,
    // must still verify afterward. Content-addressing makes the collision easy to force
    // deliberately: copy the office's own known content into a path this test then prunes.
    let area = TestArea::new("scope-prune-office-collision");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));

    // Establish trust, which writes at least one tracked-key record under the office pallet.
    assert_success(&area.forklift("dev", &["office", "enroll"]));

    let office_head = area.read_file("dev/.forklift/meta/office").trim().to_string();
    let keys_tree = path_object_hash(&area, "dev", &office_head, ".forklift/tracked/keys");

    // Find one key record's blob hash and content via peek (never guessing the on-disk layout).
    let listing = area.forklift("dev", &["--json", "peek", &keys_tree]);
    assert_success(&listing);
    let listing_json: serde_json::Value = serde_json::from_str(&stdout(&listing)).unwrap();
    let key_blob_hash = listing_json["data"]["entries"].as_array().unwrap().iter()
        .find(|entry| entry["item_type"].as_str().unwrap().trim() != "tree")
        .expect("the office keys tree has at least one record")
        ["hash"].as_str().unwrap().to_string();

    let blob_peek = area.forklift("dev", &["--json", "peek", &key_blob_hash]);
    assert_success(&blob_peek);
    let blob_json: serde_json::Value = serde_json::from_str(&stdout(&blob_peek)).unwrap();
    let key_blob_content = blob_json["data"]["content"].as_str().unwrap().to_string();

    // Write the office record's EXACT content at a user-pallet path, so the stored blob hash
    // collides with the office's — proving retention is keyed by hash, not by pallet or path.
    area.write_file("dev/src/web/w.txt", &key_blob_content);
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "web collides with an office key record"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let web_blob = path_object_hash(&area, "dev", &head, "src/web/w.txt");
    assert_eq!(web_blob, key_blob_hash, "the forced collision landed on the same hash");

    assert_success(&area.forklift(".",
        &["franchise", &server.url, "sparse", "--only", "src/api", "--only", "src/web"]));
    assert_success(&area.forklift("sparse", &["narrow", "src/web"]));

    let pruned = area.forklift("sparse", &["scope-prune", "src/web"]);
    assert_success(&pruned);

    // The colliding object SURVIVES the prune: the office's full-closure retention protects it,
    // even though the only *user-pallet* path that named it was just pruned.
    assert!(object_present(&area, "sparse", &web_blob),
        "a user blob colliding with an office record must survive a prune of its user path");

    // The office chain still verifies — proving the survival is real, not a coincidence: had the
    // object actually been freed, the office audit (which needs this exact blob's content) would
    // fail, not merely read as "missing" in some unrelated sense.
    let audit = area.forklift("sparse", &["audit"]);
    assert_success(&audit);
    assert!(stdout(&audit).contains("Office chain verified"), "{}", stdout(&audit));
}

#[test]
fn the_path_addressed_subtree_endpoint_serves_a_subtree_and_signals_fallback_on_404() {
    // The FORK-10 enforcement seam: a path-addressed fetch. The remote resolves a path to a
    // subtree and serves its closure — the wire surface a per-path authorizer will gate (a
    // hash-addressed object GET is path-blind and cannot). The endpoint is additive: the client
    // treats a 404 — an older remote without the route, or a path that does not resolve — as the
    // signal to fall back to the shipped hash-addressed walk, so shipping it needs no version bump.
    let area = TestArea::new("subtree-endpoint");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let web_tree = path_object_hash(&area, "dev", &head, "src/web");
    let web_blob = path_object_hash(&area, "dev", &head, "src/web/w.txt");

    // A sparse franchise: src/web is sealed by hash, its objects never fetched.
    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/api"]));
    assert!(!object_present(&area, "sparse", &web_tree), "src/web starts sealed, not fetched");

    // Drive the client straight against the endpoint (a current-thread runtime: the storage scope
    // is thread-local, so the import must stay on this thread).
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let _scope = forklift_core::globals::StorageRootScope::enter(&area.path("sparse"));
    let client = forklift_core::util::remote_utils::RemoteClient::new(&server.url, None).unwrap();

    // The endpoint resolves src/web to its subtree and serves the closure; importing it lands the
    // sealed objects, hash-verified like any bundle.
    let bundle = runtime
        .block_on(client.fetch_subtree(&head, "src/web"))
        .expect("the subtree fetch must not error")
        .expect("a present remote serves the subtree, not a 404");
    forklift_core::util::bundle_utils::import_bundle_bytes(&bundle).expect("the served subtree imports");

    assert!(object_present(&area, "sparse", &web_tree), "the endpoint delivered the subtree object");
    assert!(object_present(&area, "sparse", &web_blob), "the endpoint delivered the subtree's blob");

    // A path the remote cannot resolve answers 404, which the client returns as `None` — the
    // signal to fall back to the hash-addressed walk. An older remote without the route 404s
    // identically, so the fallback path is the same one this returns.
    let missing = runtime
        .block_on(client.fetch_subtree(&head, "does/not/exist"))
        .expect("a 404 is not an error");
    assert!(missing.is_none(), "an unresolved path signals the client to fall back");
}

#[test]
fn the_path_addressed_subtree_endpoint_round_trips_a_path_with_reserved_characters() {
    // The URL is built by splicing the warehouse path into it; a segment holding a character
    // reserved in the URL grammar (space, `#`, `%`) must be percent-encoded on the way out and
    // decoded back to the exact original name on the way in — otherwise the request is invalid
    // or misrouted, and a directory containing one of these characters could never be fetched by
    // path. Drive the real server, not a mock, so both halves of the round trip are exercised.
    let area = TestArea::new("subtree-endpoint-reserved-chars");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/a dir#1/100%/w.txt", "reserved-char dir v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let weird_tree = path_object_hash(&area, "dev", &head, "src/a dir#1/100%");
    let weird_blob = path_object_hash(&area, "dev", &head, "src/a dir#1/100%/w.txt");

    // A sparse franchise scoped away from the reserved-character path: its objects are sealed,
    // never fetched, until the path-addressed endpoint is asked for them directly.
    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/api"]));
    assert!(!object_present(&area, "sparse", &weird_tree), "the path starts sealed, not fetched");

    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let _scope = forklift_core::globals::StorageRootScope::enter(&area.path("sparse"));
    let client = forklift_core::util::remote_utils::RemoteClient::new(&server.url, None).unwrap();

    let bundle = runtime
        .block_on(client.fetch_subtree(&head, "src/a dir#1/100%"))
        .expect("the subtree fetch must not error")
        .expect("the server must resolve the reserved-character path, not 404 on a mangled URL");
    forklift_core::util::bundle_utils::import_bundle_bytes(&bundle).expect("the served subtree imports");

    assert!(object_present(&area, "sparse", &weird_tree),
        "the endpoint delivered the reserved-character subtree object");
    assert!(object_present(&area, "sparse", &weird_blob),
        "the endpoint delivered the reserved-character subtree's blob");
}

/// Build a subtree of `count` uniquely-named, uniquely-hashed blobs directly under one
/// directory `name`, and set the warehouse's `main` head to a parcel rooted over it. Used to
/// exceed the subtree endpoint's per-response object cap without paying for thousands of real
/// `forklift` process calls.
///
/// Stores objects with a plain, non-atomic write rather than the real (fsync'd) write path:
/// safe only because this is a disposable fixture built and consumed within one test process,
/// never exercising crash-recovery. The real path (`LooseObject::store`) fsyncs deliberately —
/// durable-before-destructive — which is exactly why it is too slow to call ten thousand times
/// in one test (empirically, over a minute; this fixture needs milliseconds).
fn build_wide_subtree(warehouse_root: &Path, name: &str, count: usize) -> String {
    use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
    use forklift_core::enums::dir_entry_type::DirEntryType;
    use forklift_core::globals::StorageRootScope;
    use forklift_core::model::blob::Blob;
    use forklift_core::model::object::loose_object::LooseObject;
    use forklift_core::model::parcel::Parcel;
    use forklift_core::model::tree_item::TreeItem;
    use forklift_core::util::{file_utils, pallet_utils};

    let _scope = StorageRootScope::enter(warehouse_root);

    fn store_fast(mut object: LooseObject) -> String {
        let compressed = object.compress().unwrap();
        let (path, file_name) = file_utils::get_path_for_object(&object.hash).unwrap();
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(std::path::Path::new(&path).join(&file_name), compressed).unwrap();
        object.hash
    }

    let mut wide = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
    for i in 0..count {
        let hash = store_fast(LooseObjectBuilder::build_blob(&Blob { content: i.to_string().into_bytes() }));
        wide.add_child(TreeItem::new(format!("f{}", i), hash, DirEntryType::Normal));
    }
    let wide_hash = store_fast(LooseObjectBuilder::build_tree(&wide));

    let mut top = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
    top.add_child(TreeItem::new(name.to_string(), wide_hash, DirEntryType::Tree));
    let top_hash = store_fast(LooseObjectBuilder::build_tree(&top));

    let parcel = Parcel {
        tree_hash: top_hash,
        parents: Vec::new(),
        actions: Vec::new(),
        description: Some("wide subtree fixture".to_string()),
    };
    let parcel_hash = store_fast(LooseObjectBuilder::build_parcel(&parcel));
    pallet_utils::set_pallet_head("main", &parcel_hash).unwrap();

    parcel_hash
}

#[test]
fn the_subtree_endpoint_refuses_an_oversized_closure_and_the_client_falls_back() {
    // Parity with objects/batch's MAX_MISSING_BATCH cap: an uncapped subtree response would
    // buffer an arbitrarily large bundle in memory before it could even check the size. A
    // subtree whose closure exceeds the cap must be refused (422, naming the cap) rather than
    // streamed — and the client must treat that refusal exactly like a 404: fall back to the
    // hash-addressed scoped walk, which has no such single-response limit.
    let area = TestArea::new("subtree-cap");
    let server = Server::start(&area, None);

    // MAX_MISSING_BATCH is 10_000; 10_000 blobs plus their one parent tree is 10_001 objects —
    // one over the cap.
    let head = build_wide_subtree(&area.path("server-root"), "big", 10_000);

    // The server refuses with 422, not a streamed bundle.
    let status = http_status(&server.url, "GET", &format!("/v1/parcels/{}/subtree/big", head), None);
    assert_eq!(status, "HTTP/1.1 422 Unprocessable Entity",
        "an oversized subtree closure is refused, not streamed");

    // The client treats that 422 exactly like a 404: `Ok(None)`, the fallback signal.
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let client = forklift_core::util::remote_utils::RemoteClient::new(&server.url, None).unwrap();
    let result = runtime.block_on(client.fetch_subtree(&head, "big"));
    assert!(result.is_ok(), "an over-cap subtree must not be a client error: {:?}", result.err());
    assert!(result.unwrap().is_none(), "an over-cap subtree signals fallback, exactly like a 404");
}
