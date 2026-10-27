//! End-to-end tests of the remote stack: a real `forklift-server` process serving a
//! bare warehouse over HTTP, driven by the real CLI — lift, lower, franchise, bundles,
//! trust, and the server-side verification guarantees of the protocol.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use forklift_core::util::progress_utils::Progress;

mod common;
use common::{forklift_command, hold_store_access, server_binary};

/// `forklift-server` with fsync off, for the same reason as [`forklift_command`].
fn server_command() -> Command {
    let mut command = Command::new(server_binary());
    command.env("FORKLIFT_FSYNC", "0");
    command
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
        forklift_command()
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

        let prepared = server_command()
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

        let prepared = server_command()
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

        let prepared = server_command()
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
        match token {
            Some(token) => {
                args.push("--token".to_string());
                args.push(token.to_string());
            }
            // No token on the command line: every caller here that passes `None` is either
            // testing genuinely open (`Principal::Open`) server behavior, or already configures
            // auth another way via `--config` (a file `token`, `[hooks]` authentication, or
            // `--tokens`) — forklift-server no longer infers "open" from an empty auth config
            // (it refuses to start instead), so this harness must say so explicitly. When auth
            // *is* configured some other way, a configured token/tokens/authentication hook
            // always takes precedence over `open` in `check_auth`, so this flag is inert there
            // and load-bearing only for the genuinely-open callers.
            None => args.push("--open".to_string()),
        }

        let mut child = server_command()
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
        let output = server_command()
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

/// Send a bodyless raw GET and return `(status line, response body)` — the GET counterpart of
/// `http_post_json`, used where a test needs to check the refusal's wording, not just its status.
fn http_get(server_url: &str, path: &str) -> (String, String) {
    let address = server_url.strip_prefix("http://").unwrap();
    let mut stream = std::net::TcpStream::connect(address).unwrap();

    write!(
        stream,
        "GET {} HTTP/1.1\r\nHost: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        path, address
    ).unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();

    let status = response.lines().next().unwrap_or_default().to_string();
    let body = response.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();

    (status, body)
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
    http_send_json(server_url, "POST", path, body)
}

/// Send a JSON body with `method` over raw HTTP and return `(status line, response body)`.
fn http_send_json(server_url: &str, method: &str, path: &str, body: &str) -> (String, String) {
    http_send_json_as(server_url, method, path, body, None)
}

/// [`http_send_json`], with `token` as the bearer credential when one is given.
fn http_send_json_as(server_url: &str, method: &str, path: &str, body: &str, token: Option<&str>) -> (String, String) {
    let address = server_url.strip_prefix("http://").unwrap();
    let mut stream = std::net::TcpStream::connect(address).unwrap();

    let auth = token
        .map(|token| format!("Authorization: Bearer {}\r\n", token))
        .unwrap_or_default();

    write!(
        stream,
        "{} {} HTTP/1.1\r\nHost: {}\r\n{}Content-Type: application/json\r\n\
        Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        method, path, address, auth, body.len(), body
    ).unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();

    let status = response.lines().next().unwrap_or_default().to_string();
    let body = response.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();

    (status, body)
}

#[track_caller]
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

/// The `details` of a `trust_changed` refusal under `--json`: exit 22, the error document on
/// stdout.
fn trust_changed_details(output: &Output) -> serde_json::Value {
    assert_eq!(output.status.code(), Some(22), "{}{}", stdout(output), stderr(output));
    let value: serde_json::Value = serde_json::from_str(&stdout(output)).unwrap();
    assert_eq!(value["error"]["code"], "trust_changed", "{}", value);
    value["error"]["details"].clone()
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
    assert_success(&area.forklift(name, &["config", "--global", "operator.identifier", "tester@forklift"]));
    assert_success(&area.forklift(name, &["config", "remote.url", remote_url]));
}

/// FORK-109, at the site the bug actually lived: `lift_pallet_inner`'s candidate loop.
///
/// The loop walks each new parcel's tree against its parents' trees, and shares one
/// `seen_trees` ledger across the whole push — recording a subtree as visited *before* the
/// base-explained check, so a hash pruned once is never collected again. That is correct only
/// if parents are walked before their children. The old code iterated a reversed DFS preorder,
/// which is not a topological order.
///
/// The DAG below is the smallest one that exposes it end to end. `P` introduces `shared/`; `A`
/// and `B` both descend from `P` and leave `shared/` alone; `M` merges them:
///
/// ```text
///   R (remote head) → P → A ┐
///                     └─ B ─┴→ M (local head)
/// ```
///
/// The DFS emits `[M, B, P, A]`, so reversing it walks `A` before its own parent `P`. `A`
/// prunes `shared/` as explained by `P` and marks the hash seen; `P`'s own walk then reaches
/// `shared/` — genuinely new, since `R` has never seen it — finds it already marked, and skips
/// it. The subtree and its blob are never uploaded, the server's closure audit refuses the
/// push, and because the walk is deterministic the retry fails identically.
///
/// A unit test on the ordering function cannot catch a regression here: the loop can be
/// rewritten `for parcel_hash in new_parcels.iter().rev()` and still compile, with every
/// ordering test green. This one fails.
#[test]
fn a_lift_uploads_a_subtree_introduced_by_a_parent_of_both_merge_sides() {
    let area = TestArea::new("fork109-merge-order");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/base.txt", "base\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "base"]));
    assert_success(&area.forklift("a", &["office", "enroll"]));
    assert_success(&area.forklift("a", &["lift"]));

    // P — introduces `shared/`, which neither branch below will touch. Not lifted, so it is
    // part of the push under test.
    area.write_file("a/shared/s.txt", "introduced by P\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "P adds shared/"]));
    let p = pallet_head(&area, "a", "main");

    // B — a second line off P, touching only a root file.
    assert_success(&area.forklift("a", &["palletize", "side", &p])); // palletize also shifts
    area.write_file("a/b.txt", "b\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "B edits b.txt"]));

    // A — back on main, also off P, also touching only a root file.
    assert_success(&area.forklift("a", &["shift", "main"]));
    area.write_file("a/a.txt", "a\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "A edits a.txt"]));

    // M — the merge. Its ancestry re-reaches P through both sides.
    assert_success(&area.forklift("a", &["consolidate", "side"]));

    // The push carries M, A, B and P against a remote still at the base. If `shared/` is
    // dropped from the candidate set, the server's closure audit refuses this.
    let lifted = area.forklift("a", &["lift"]);
    assert_success(&lifted);

    // And the content really arrived: a fresh franchise materializes the subtree P introduced.
    assert_success(&area.forklift(".", &["franchise", &server.url, "verify"]));
    assert_eq!(area.read_file("verify/shared/s.txt"), "introduced by P\n");
    assert_eq!(area.read_file("verify/a.txt"), "a\n");
    assert_eq!(area.read_file("verify/b.txt"), "b\n");
}

/// The ignore file is tracked content, like `.gitignore`: a warehouse that receives a pallet
/// tracking it gets the remote's rules, by either route a fresh warehouse takes onto a remote
/// pallet. Both routes prepare the target first, so a `prepare` that wrote a template ignore file
/// would collide with the tracked one.
#[test]
fn the_ignore_file_travels_with_the_pallet() {
    let area = TestArea::new("ignore-file-travels");
    let server = Server::start(&area, None);

    let rules = "# build output\n^build(/.*)?$\n";
    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/.forkliftignore", rules);
    area.write_file("a/src.txt", "src\n");
    area.write_file("a/build/out.txt", "generated\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "track the ignore rules"]));
    assert_success(&area.forklift("a", &["lift"]));

    // Route 1: franchise into a new directory.
    assert_success(&area.forklift(".", &["franchise", &server.url, "b"]));
    assert_eq!(area.read_file("b/.forkliftignore"), rules);
    assert!(!area.path("b/build").exists(), "an ignored path is never part of the pallet");

    // The received rules are in force: an ignored build folder reads clean.
    area.write_file("b/build/out.txt", "generated in b\n");
    let status = area.forklift("b", &["stocktake"]);
    assert_success(&status);
    assert!(stdout(&status).contains("matches the inventory"), "{}", stdout(&status));

    // An edit to the rules is a change like any other, and reaches the other side.
    let widened = "# build output\n^build(/.*)?$\n\\.log$\n";
    area.write_file("a/.forkliftignore", widened);
    assert_success(&area.forklift("a", &["load", ".forkliftignore"]));
    assert_success(&area.forklift("a", &["stack", "ignore logs"]));
    assert_success(&area.forklift("a", &["lift"]));
    assert_success(&area.forklift("b", &["lower"]));
    assert_eq!(area.read_file("b/.forkliftignore"), widened);
}

/// The other route onto a remote pallet: prepare a fresh warehouse, point it at the remote and
/// lower. A template ignore file from `prepare` would make lower refuse to write the tracked one.
#[test]
fn a_prepared_warehouse_lowers_the_tracked_ignore_file() {
    let area = TestArea::new("ignore-file-lowered");
    let server = Server::start(&area, None);

    let rules = "^build(/.*)?$\n";
    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/.forkliftignore", rules);
    area.write_file("a/src.txt", "src\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "track the ignore rules"]));
    assert_success(&area.forklift("a", &["lift"]));

    prepare_warehouse(&area, "c", &server.url);
    assert_success(&area.forklift("c", &["lower"]));
    assert_eq!(area.read_file("c/.forkliftignore"), rules);

    area.write_file("c/build/out.txt", "generated in c\n");
    let status = area.forklift("c", &["stocktake"]);
    assert_success(&status);
    assert!(stdout(&status).contains("matches the inventory"), "{}", stdout(&status));
}

/// A sparse franchise whose scope leaves out the root holds the root ignore file in scope all
/// the same: it is fetched and materialized, its rules are in force, and an edit to it stacks
/// and travels both ways, while the other root files stay sealed.
#[test]
fn a_sparse_franchise_keeps_the_root_ignore_file_in_scope() {
    let area = TestArea::new("sparse-ignore-file");
    let server = Server::start(&area, None);

    let rules = "^src/build(/.*)?$\n";
    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/.forkliftignore", rules);
    area.write_file("dev/README", "readme v1\n");
    area.write_file("dev/src/a.txt", "a v1\n");
    area.write_file("dev/docs/guide.md", "guide v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src"]));
    assert_eq!(area.read_file("sparse/.forkliftignore"), rules, "the root ignore file is materialized");
    assert!(!area.path("sparse/README").exists(), "another root file stays sealed");
    assert!(!area.path("sparse/docs").exists(), "an out-of-scope folder stays sealed");

    // The scope report names it, apart from the declared prefixes.
    let scope = area.forklift("sparse", &["--json", "scope"]);
    assert_success(&scope);
    let scope_json: serde_json::Value = serde_json::from_str(&stdout(&scope)).unwrap();
    assert_eq!(scope_json["data"]["materialization_scope"], serde_json::json!(["src"]), "{}", stdout(&scope));
    assert_eq!(scope_json["data"]["implicit_scope"], serde_json::json!([".forkliftignore"]), "{}", stdout(&scope));
    assert_eq!(scope_json["data"]["fetch_scope"], serde_json::json!(["src"]), "{}", stdout(&scope));
    assert_eq!(scope_json["data"]["implicit_fetch_scope"], serde_json::json!([".forkliftignore"]), "{}", stdout(&scope));

    // The rules are in force: an ignored build folder inside the scope reads clean.
    area.write_file("sparse/src/build/out.txt", "generated\n");
    let status = area.forklift("sparse", &["stocktake"]);
    assert_success(&status);
    assert!(stdout(&status).contains("matches the inventory"), "{}", stdout(&status));

    // An edit to the rules made in the sparse checkout stacks, lifts and reaches a full one.
    let widened = "^src/build(/.*)?$\n\\.log$\n";
    area.write_file("sparse/.forkliftignore", widened);
    assert_success(&area.forklift("sparse", &["load", ".forkliftignore"]));
    assert_success(&area.forklift("sparse", &["stack", "ignore logs"]));
    assert_success(&area.forklift("sparse", &["lift"]));
    assert_success(&area.forklift("dev", &["lower"]));
    assert_eq!(area.read_file("dev/.forkliftignore"), widened);
    assert_eq!(area.read_file("dev/README"), "readme v1\n", "the sealed root file survives the sparse stack");
    assert_eq!(area.read_file("dev/docs/guide.md"), "guide v1\n");

    // And an edit made on the full side reaches the sparse one.
    let narrowed = "^src/build(/.*)?$\n";
    area.write_file("dev/.forkliftignore", narrowed);
    area.write_file("dev/README", "readme v2\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "drop the log rule"]));
    assert_success(&area.forklift("dev", &["lift"]));
    assert_success(&area.forklift("sparse", &["lower"]));
    assert_eq!(area.read_file("sparse/.forkliftignore"), narrowed);
    assert!(!area.path("sparse/README").exists(), "the lowered root file stays sealed");

    let status = area.forklift("sparse", &["stocktake"]);
    assert_success(&status);
    assert!(stdout(&status).contains("matches the inventory"), "{}", stdout(&status));
}

/// The root ignore file is in scope without being a declared prefix, so the sparse verbs that
/// guard a prefix's shape or contents treat it as an ordinary file: unstaging an edit to it is
/// no type change, and an edit to it does not block narrowing some other prefix away.
#[test]
fn a_sparse_checkout_unstages_the_root_ignore_file_and_narrows_past_an_edit_to_it() {
    let area = TestArea::new("sparse-ignore-file-verbs");
    let server = Server::start(&area, None);

    let rules = "^src/build(/.*)?$\n";
    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/.forkliftignore", rules);
    area.write_file("dev/src/a.txt", "a v1\n");
    area.write_file("dev/docs/guide.md", "guide v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    assert_success(&area.forklift(".",
        &["franchise", &server.url, "sparse", "--only", "src", "--only", "docs"]));

    // A staged edit to the rules unstages back to the head's version, by name and as part of
    // the root folder — the folder form rebuilds the root shard, whose files are on the spine.
    area.write_file("sparse/.forkliftignore", "^src/build(/.*)?$\n\\.log$\n");
    assert_success(&area.forklift("sparse", &["load", ".forkliftignore"]));
    assert_success(&area.forklift("sparse", &["restore", "--staged", ".forkliftignore"]));
    assert_success(&area.forklift("sparse", &["load", ".forkliftignore"]));
    assert_success(&area.forklift("sparse", &["restore", "--staged", "."]));
    let status = area.forklift("sparse", &["--json", "stocktake"]);
    assert_success(&status);
    let json: serde_json::Value = serde_json::from_str(&stdout(&status)).unwrap();
    assert_eq!(json["data"]["staged_count"], serde_json::json!(0), "the edit is unstaged: {}", stdout(&status));
    assert_eq!(json["data"]["unstaged"], serde_json::json!([{"kind": "modified", "path": ".forkliftignore"}]),
        "{}", stdout(&status));

    // The edit is still on disk, unstaged; narrowing "docs" away is not blocked by it.
    assert_success(&area.forklift("sparse", &["narrow", "docs"]));
    assert!(!area.path("sparse/docs").exists(), "the narrowed prefix is de-materialized");
    assert_eq!(area.read_file("sparse/.forkliftignore"), "^src/build(/.*)?$\n\\.log$\n",
        "the unstaged edit to the rules survives the narrow");
}

/// A scope file written before the root ignore file joined every scope is refused on read, rather
/// than read under today's meaning — under which the never-inventoried ignore file would show as
/// a removal and the next stack would commit it.
#[test]
fn a_sparse_warehouse_with_an_unversioned_scope_file_is_refused() {
    let area = TestArea::new("sparse-legacy-scope");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/.forkliftignore", "^src/build(/.*)?$\n");
    area.write_file("dev/src/a.txt", "a v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src"]));

    // Rewrite the recorded scopes in the unversioned form an older Forklift wrote.
    for file in ["sparse/.forklift/config/fetch-scope", "sparse/.forklift/scope"] {
        assert!(area.path(file).exists(), "the sparse franchise records {}", file);
        std::fs::write(area.path(file), "src\n").unwrap();
    }

    for verb in [&["stocktake"][..], &["stack", "nothing"][..]] {
        let refused = area.forklift("sparse", verb);
        assert!(!refused.status.success(), "{:?} must refuse: {}", verb, stdout(&refused));
        assert!(stderr(&refused).contains("older version of Forklift"), "{:?}: {}", verb, stderr(&refused));
        assert!(stderr(&refused).contains("Franchise the remote again"), "{:?}: {}", verb, stderr(&refused));
    }
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

/// A pass-through HTTP proxy in front of a real server that answers every batch fetch with a `404`,
/// as a remote without the batch endpoint would.
struct BatchlessProxy {
    url: String,
    batch_requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl BatchlessProxy {
    fn start(upstream: &str) -> BatchlessProxy {
        let batch_requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&batch_requests);
        let proxy = InterceptingProxy::start(upstream, move |path| {
            path.ends_with("/objects/batch").then(|| {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
            })
        });

        BatchlessProxy { url: proxy.url, batch_requests }
    }
}

/// A pass-through HTTP proxy in front of a real server. A request whose path `intercept` answers
/// gets that answer (a whole HTTP response); everything else is forwarded unchanged, one request
/// per connection (`Connection: close` on the forwarded request, so the server closes after its
/// answer and the copy below ends).
struct InterceptingProxy {
    url: String,
}

type Intercept = dyn Fn(&str, &str) -> Option<Vec<u8>> + Send + Sync;
type Rewrite = dyn Fn(&str) -> String + Send + Sync;

impl InterceptingProxy {
    fn start(upstream: &str, intercept: impl Fn(&str) -> Option<Vec<u8>> + Send + Sync + 'static) -> InterceptingProxy {
        InterceptingProxy::start_seeing(upstream, move |path, _| intercept(path))
    }

    /// As [`InterceptingProxy::start`], with `intercept` also given the request's head.
    fn start_seeing(upstream: &str,
                    intercept: impl Fn(&str, &str) -> Option<Vec<u8>> + Send + Sync + 'static) -> InterceptingProxy {
        InterceptingProxy::launch(upstream, std::sync::Arc::new(intercept), std::sync::Arc::new(str::to_string))
    }

    /// A proxy that intercepts nothing and forwards each request with its path rewritten by
    /// `rewrite`, as an intermediary that normalizes paths before routing does.
    fn start_rewriting(upstream: &str, rewrite: impl Fn(&str) -> String + Send + Sync + 'static) -> InterceptingProxy {
        InterceptingProxy::launch(upstream, std::sync::Arc::new(|_: &str, _: &str| None), std::sync::Arc::new(rewrite))
    }

    fn launch(upstream: &str, intercept: std::sync::Arc<Intercept>, rewrite: std::sync::Arc<Rewrite>) -> InterceptingProxy {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let upstream = upstream.trim_start_matches("http://").trim_end_matches('/').to_string();

        // The acceptor lives for the rest of the test process, like `HookServer`'s.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(downstream) = stream else { continue };
                let upstream = upstream.clone();
                let intercept = std::sync::Arc::clone(&intercept);
                let rewrite = std::sync::Arc::clone(&rewrite);

                std::thread::spawn(move || InterceptingProxy::serve(downstream, &upstream, &*intercept, &*rewrite));
            }
        });

        InterceptingProxy { url }
    }

    fn serve(mut downstream: std::net::TcpStream,
             upstream: &str,
             intercept: &Intercept,
             rewrite: &Rewrite) {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 8192];

        let header_end = loop {
            if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                break position + 4;
            }

            match downstream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            }
        };

        let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let mut lines = head.split("\r\n").filter(|line| !line.is_empty());
        let request_line = lines.next().unwrap_or_default().to_string();
        let headers: Vec<String> = lines
            .filter(|line| !line.to_ascii_lowercase().starts_with("connection:"))
            .map(str::to_string)
            .collect();
        let content_length: usize = headers.iter()
            .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_string()))
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);

        let mut body = buffer[header_end..].to_vec();

        while body.len() < content_length {
            match downstream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
            }
        }

        let path = request_line.split_whitespace().nth(1).unwrap_or_default();

        if let Some(answer) = intercept(path, &head) {
            let _ = downstream.write_all(&answer);
            return;
        }

        let Ok(mut server) = std::net::TcpStream::connect(upstream) else { return };
        let request_line = request_line.replacen(path, &rewrite(path), 1);
        let mut forwarded = format!("{}\r\n{}\r\nConnection: close\r\n\r\n", request_line, headers.join("\r\n")).into_bytes();
        forwarded.extend_from_slice(&body);

        if server.write_all(&forwarded).is_err() {
            return;
        }

        let _ = std::io::copy(&mut server, &mut downstream);
        let _ = downstream.shutdown(std::net::Shutdown::Both);
    }
}

/// A remote whose batch lane closes is named in the command's output, and a healthy one is not
/// (FORK-268). The same change is lowered twice: once straight from the server, once through a proxy
/// that refuses every batch fetch, so the only difference between the runs is the lane.
///
/// Revert: drop `report.batch_lane = client.batch_lane_closure()` from `lower` → the proxied run
/// prints no line and carries no `batch_lane`; set it unconditionally → the direct run carries one.
#[test]
fn lower_names_a_closed_batch_lane_and_a_healthy_lower_does_not() {
    let area = TestArea::new("batch-lane");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/docs/readme.txt", "hello\n");
    area.write_file("a/src/main.txt", "fn main\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "first"]));
    assert_success(&area.forklift("a", &["lift"]));

    assert_success(&area.forklift(".", &["franchise", &server.url, "direct"]));
    assert_success(&area.forklift(".", &["franchise", &server.url, "proxied"]));

    // Two directories and two files change, so the lower's tree and blob waves each miss more
    // than one object and go to the batch lane.
    area.write_file("a/docs/readme.txt", "hello again\n");
    area.write_file("a/src/main.txt", "fn main v2\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "second"]));
    assert_success(&area.forklift("a", &["lift"]));

    let proxy = BatchlessProxy::start(&server.url);
    assert_success(&area.forklift("proxied", &["config", "remote.url", &proxy.url]));

    let fallback = "does not serve batch downloads";

    let direct = area.forklift("direct", &["lower"]);
    assert_success(&direct);
    assert!(stdout(&direct).contains("Lowered pallet \"main\""), "{}", stdout(&direct));
    assert!(!stdout(&direct).contains(fallback), "a healthy lower names no fallback: {}", stdout(&direct));
    assert!(!stdout(&direct).contains("one object at a time"), "{}", stdout(&direct));

    let proxied = area.forklift("proxied", &["--json", "lower"]);
    assert_success(&proxied);
    assert!(
        proxy.batch_requests.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "the lower must have asked the proxy for a batch, or this run proves nothing"
    );
    let document: serde_json::Value = serde_json::from_str(&stdout(&proxied)).unwrap();
    assert_eq!(document["data"]["outcome"], "lowered", "{}", document);
    assert_eq!(document["data"]["batch_lane"], "endpoint_absent", "{}", document);
    assert_eq!(area.read_file("proxied/src/main.txt"), "fn main v2\n");

    // The human form names it in one line; the direct run's JSON carries no field at all.
    area.write_file("a/docs/readme.txt", "hello a third time\n");
    area.write_file("a/src/main.txt", "fn main v3\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "third"]));
    assert_success(&area.forklift("a", &["lift"]));

    let human = area.forklift("proxied", &["lower"]);
    assert_success(&human);
    assert!(stdout(&human).contains(fallback), "{}", stdout(&human));

    let direct_json = area.forklift("direct", &["--json", "lower"]);
    assert_success(&direct_json);
    let document: serde_json::Value = serde_json::from_str(&stdout(&direct_json)).unwrap();
    assert_eq!(document["data"]["outcome"], "lowered", "{}", document);
    assert!(document["data"].get("batch_lane").is_none(), "{}", document);
}

// A lower refused over an untracked file where the remote's head adds one changes nothing, and leaves
// no interrupted operation behind. Revert: raise lower's operation marker before its collision
// check → every later command refuses until heal, and heal completes a lower that never ran.
#[test]
fn a_lower_refused_over_an_untracked_file_leaves_nothing_standing() {
    let area = TestArea::new("lower-collision");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/readme.txt", "hello\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "first"]));
    assert_success(&area.forklift("a", &["lift"]));
    assert_success(&area.forklift(".", &["franchise", &server.url, "b"]));

    area.write_file("a/added.txt", "theirs\n");
    assert_success(&area.forklift("a", &["load", "added.txt"]));
    assert_success(&area.forklift("a", &["stack", "second"]));
    assert_success(&area.forklift("a", &["lift"]));

    area.write_file("b/added.txt", "mine\n");
    let lowered = area.forklift("b", &["lower"]);
    assert!(!lowered.status.success(), "the untracked file must refuse the lower");
    assert_eq!(area.read_file("b/added.txt"), "mine\n");

    let stacked = area.forklift("b", &["stack", "nothing"]);
    assert!(stderr(&stacked).contains("Nothing to stack"), "no operation may stand: {}", stderr(&stacked));
}

// A bay whose pallet another bay already lowered to the remote's head is behind, not up to date.
// Revert: lower's "up to date" check on the pallet's ref ahead of its currency check → it reports
// up to date and the bay's files stay behind.
#[test]
fn lowering_in_a_bay_another_bay_already_lowered_names_the_catch_up() {
    let area = TestArea::new("lower-stale-bay");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/f.txt", "v1\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "first"]));
    assert_success(&area.forklift("a", &["lift"]));
    assert_success(&area.forklift(".", &["franchise", &server.url, "b"]));

    let bay_dir = area.path("b-bay");
    assert_success(&area.forklift("b", &["bay", "add", "w", bay_dir.to_str().unwrap()]));
    assert_success(&area.forklift("b-bay", &["shift", "main"]));

    area.write_file("a/f.txt", "v2\n");
    assert_success(&area.forklift("a", &["load", "f.txt"]));
    assert_success(&area.forklift("a", &["stack", "second"]));
    assert_success(&area.forklift("a", &["lift"]));
    assert_success(&area.forklift("b", &["lower"]));

    let lowered = area.forklift("b-bay", &["lower"]);
    assert!(!lowered.status.success() && stderr(&lowered).contains("forklift shift main"), "{}{}",
        stdout(&lowered), stderr(&lowered));
    assert_eq!(area.read_file("b-bay/f.txt"), "v1\n");
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
fn every_hash_taking_endpoint_refuses_an_uppercase_spelling_as_client_error() {
    // Loose objects live under the lowercase spelling, so a case-sensitive file system missed an
    // uppercase one that a case-insensitive file system or a pack index found. The store now
    // answers for no other spelling, and the server refuses one with a 422 before it gets there,
    // the same answer on every platform, without repeating the value back.
    let area = TestArea::new("uppercase-hash-refused");
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
    assert_success(&area.forklift("a", &["lift"]));

    let upper = head.to_ascii_uppercase();
    assert_ne!(upper, head, "the parcel hash must carry a letter for the probe to mean anything");

    // The control: the canonical spelling is present, so the refusals below are about spelling.
    let (status, body) = http_post_json(&server.url, "/v1/objects/missing",
        &format!("{{\"hashes\":[\"{}\"]}}", head));
    assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, body);
    assert!(!body.contains(&head), "the lifted parcel is present: {}", body);

    let batch = format!("{{\"hashes\":[\"{}\"]}}", upper);
    let targets = format!("{{\"session\":\"lift-x\",\"hashes\":[\"{}\"]}}", upper);
    let responses = [
        ("missing", http_post_json(&server.url, "/v1/objects/missing", &batch)),
        ("upload-targets", http_post_json(&server.url, "/v1/objects/upload-targets", &targets)),
        ("batch", http_post_json(&server.url, "/v1/objects/batch", &batch)),
        ("holds", http_post_json(&server.url, "/v1/holds/main",
                                 &format!("{{\"candidates\":[\"{}\"]}}", upper))),
        ("object", http_get(&server.url, &format!("/v1/objects/{}", upper))),
        ("signature", http_get(&server.url, &format!("/v1/signatures/{}", upper))),
        ("object put", http_send_json(&server.url, "PUT", &format!("/v1/objects/{}", upper), "{}")),
        ("signature put", http_send_json(&server.url, "PUT", &format!("/v1/signatures/{}", upper), "{}")),
    ];
    for (endpoint, (status, body)) in responses {
        assert!(status.starts_with("HTTP/1.1 422"), "{}: {} / {}", endpoint, status, body);
        // The spelling check's own refusal, not another 422 further in.
        assert!(body.contains("not an object hash"), "{}: {}", endpoint, body);
        assert!(!body.contains(&upper), "{}: the client's value is not repeated: {}", endpoint, body);
    }
}

#[test]
fn the_holds_endpoint_answers_held_for_lifted_history_and_not_held_for_the_rest() {
    let area = TestArea::new("holds-endpoint");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    let mut parcels = Vec::new();
    for (round, content) in ["one\n", "two\n"].iter().enumerate() {
        area.write_file("a/doc.txt", content);
        assert_success(&area.forklift("a", &["load", "."]));
        let stacked = area.forklift("a", &["stack", &format!("round {}", round)]);
        assert_success(&stacked);
        parcels.push(stdout(&stacked)
            .split_whitespace()
            .find(|word| word.len() == 64)
            .expect("the stack output carries the parcel hash")
            .to_string());
    }
    assert_success(&area.forklift("a", &["lift"]));

    let stranger = "0".repeat(64);
    let request = format!("{{\"candidates\":[\"{}\",\"{}\",\"{}\"]}}", parcels[0], parcels[1], stranger);

    let (status, body) = http_post_json(&server.url, "/v1/holds/main", &request);
    assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, body);
    let holds: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(holds["holds"][&parcels[0]], "held", "an ancestor of the head: {}", body);
    assert_eq!(holds["holds"][&parcels[1]], "held", "the head itself: {}", body);
    assert_eq!(holds["holds"][&stranger], "not_held", "the whole history was read: {}", body);

    // A pallet the warehouse does not have holds nothing.
    let (status, body) = http_post_json(&server.url, "/v1/holds/elsewhere", &request);
    assert!(status.starts_with("HTTP/1.1 200"), "{} / {}", status, body);
    let holds: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(holds["holds"][&parcels[1]], "not_held", "{}", body);

    // One candidate past the cap, and a name no pallet can carry, are refused.
    let over_cap: Vec<String> = (0..17).map(|n| format!("\"{:064x}\"", n)).collect();
    let (status, body) = http_post_json(&server.url, "/v1/holds/main",
        &format!("{{\"candidates\":[{}]}}", over_cap.join(",")));
    assert!(status.starts_with("HTTP/1.1 422"), "{} / {}", status, body);

    let (status, body) = http_post_json(&server.url, "/v1/holds/../main", &request);
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

    // B has nothing of its own yet: the remote is strictly ahead, which is not a divergence.
    let behind = area.forklift("b", &["--json", "lift"]);
    assert_eq!(behind.status.code(), Some(28), "{}{}", stdout(&behind), stderr(&behind));
    let value: serde_json::Value = serde_json::from_str(&stdout(&behind)).unwrap();
    assert_eq!(value["error"]["code"], "remote_ahead", "{}", value);
    assert_eq!(value["error"]["next_step"], "forklift lower", "{}", value);

    area.write_file("b/base.txt", "b's version\n");
    assert_success(&area.forklift("b", &["load", "base.txt"]));
    assert_success(&area.forklift("b", &["stack", "b work"]));

    // B's lift must be refused: the remote moved. Nothing may be overwritten.
    let refused = area.forklift("b", &["lift"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("diverged") && stderr(&refused).contains("palletize"), "{}", stderr(&refused));

    let refused = area.forklift("b", &["--json", "lift"]);
    assert_eq!(refused.status.code(), Some(5), "{}{}", stdout(&refused), stderr(&refused));
    let value: serde_json::Value = serde_json::from_str(&stdout(&refused)).unwrap();
    assert_eq!(value["error"]["code"], "diverged", "{}", value);
    assert!(value["error"]["next_step"].as_str().unwrap().starts_with("forklift palletize "), "{}", value);

    // B's lower reports the divergence (with the parcels fetched for a consolidate).
    let diverged = area.forklift("b", &["lower"]);
    assert!(!diverged.status.success());
    assert!(stderr(&diverged).contains("diverged"), "{}", stderr(&diverged));

    let diverged = area.forklift("b", &["--json", "lower"]);
    assert_eq!(diverged.status.code(), Some(5), "{}{}", stdout(&diverged), stderr(&diverged));
    let value: serde_json::Value = serde_json::from_str(&stdout(&diverged)).unwrap();
    assert_eq!(value["error"]["code"], "diverged", "{}", value);
}

// Revert: pallet_moved back to an unclassified 409 → the body carries no code.
#[test]
fn a_stale_ref_update_is_a_remote_ahead_refusal_on_the_wire() {
    let area = TestArea::new("stale-ref-update");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/f.txt", "v1\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "first"]));
    assert_success(&area.forklift("a", &["lift"]));
    let first = area.read_file("a/.forklift/pallets/main").trim().to_string();

    area.write_file("a/f.txt", "v2\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "second"]));
    assert_success(&area.forklift("a", &["lift"]));
    let second = area.read_file("a/.forklift/pallets/main").trim().to_string();
    assert_ne!(first, second);

    // A client one parcel behind: it expects "first" while the pallet is at "second".
    let request = format!("{{\"old_head\":\"{}\",\"new_head\":\"{}\"}}", first, first);
    let (status, body) = http_post_json(&server.url, "/v1/pallets/main", &request);
    assert!(status.starts_with("HTTP/1.1 409"), "{} / {}", status, body);

    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["code"], "remote_ahead", "{}", body);
    assert_eq!(value["next_step"], "forklift lower", "{}", body);
    let message = value["error"].as_str().unwrap();
    assert!(message.contains(&second) && !message.contains('\u{1f}'), "the de-framed message: {}", body);
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
    assert!(stderr(&refused).contains("diverged") && stderr(&refused).contains("palletize"), "{}", stderr(&refused));
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
fn manifest_syncs_across_remotes_and_replays_when_diverged() {
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

    // Lowering replays A's note onto B's head (conflict-free: the entries sit at different
    // cells) and lifts the result.
    let lowered = area.forklift("a", &["lower"]);
    assert_success(&lowered);
    assert!(stdout(&lowered).contains("Replayed the local @manifest changes"), "{}", stdout(&lowered));
    assert_success(&area.forklift("a", &["lift"]));

    // Both notes and the approval are now visible from A, and B sees them after lowering.
    let a_view = stdout(&area.forklift("a", &["manifest", "show", "main"]));
    assert!(a_view.contains("from a") && a_view.contains("from b") && a_view.contains("LGTM"), "{}", a_view);

    assert_success(&area.forklift("b", &["lower"]));
    let b_view = stdout(&area.forklift("b", &["manifest", "show", "main"]));
    assert!(b_view.contains("from a") && b_view.contains("from b"), "{}", b_view);

    // The replayed manifest audits clean on both sides.
    assert!(stdout(&area.forklift("b", &["audit", "@manifest"])).contains("verified"));
}

/// The tag rules across a remote: the server admits a tag only from a signer who is an admin
/// as it admits it, while a warehouse adopting tags already admitted keeps one whose tagger has
/// been demoted since; and a name both sides tagged replays only by keeping the remote's tag.
#[test]
fn tags_sync_hold_the_admin_rule_at_admission_and_never_move() {
    let area = TestArea::new("tags-remote");
    area.write_file("tokens.toml", "[operators]\n\"w-token\" = \"worker@forklift\"\n");
    let server = Server::start_with_tokens(&area, Some("root"), "tokens.toml");

    prepare_warehouse(&area, "a", &server.url);
    assert_success(&area.forklift("a", &["config", "remote.token", "root"]));
    area.write_file("a/code.txt", "v1\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "base"]));
    let base = pallet_head(&area, "a", "main");
    area.write_file("a/code.txt", "v2\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "second"]));
    assert_success(&area.forklift("a", &["office", "enroll"]));

    let worker_id = "worker@forklift";
    let worker = keygen_admit_args(&area, worker_id);
    assert_success(&area.forklift("a", &[
        "office", "admit", &worker[0], &worker[1], &worker[2], "--role", "admin",
    ]));
    assert_success(&area.forklift("a", &["lift"]));

    // The worker, an admin for now, tags and publishes a release; a second tag stays local.
    assert_success(&area.forklift(".", &["franchise", &server.url, "b", "--token", "w-token"]));
    assert_success(&area.forklift("b", &["config", "operator.identifier", worker_id]));
    assert_success(&area.forklift("b", &["tag", "create", "v1.0", "main", "-m", "release"]));

    // Before its first lift the remote has no @tags at all, so there is no side to keep.
    let unlifted = area.forklift("b", &["lower", "--keep", "@tags:v1.0=theirs"]);
    assert!(!unlifted.status.success(), "{}", stdout(&unlifted));
    assert!(stderr(&unlifted).contains("@tags is not on the remote yet"), "{}", stderr(&unlifted));

    let lifted = area.forklift("b", &["lift"]);
    assert_success(&lifted);
    assert!(stdout(&lifted).contains("Lifted the @tags pallet"), "{}", stdout(&lifted));
    assert_success(&area.forklift("b", &["tag", "create", "v1.1", "main"]));

    // Meanwhile the admin tags the same name at another parcel. The lower conflicts on it; keeping
    // ours would move a published tag, so only keeping theirs replays.
    assert_success(&area.forklift("a", &["tag", "create", "v1.0", &base]));
    let conflicted = area.forklift("a", &["--json", "lower"]);
    assert_eq!(conflicted.status.code(), Some(24), "{}", stdout(&conflicted));
    let value: serde_json::Value = serde_json::from_str(&stdout(&conflicted)).unwrap();
    assert_eq!(value["error"]["details"]["pallets"][0]["pallet"], "@tags");
    assert_eq!(value["error"]["details"]["pallets"][0]["conflicts"][0]["path"], "v1.0");

    let refused = area.forklift("a", &["lower", "--keep", "@tags:v1.0=ours"]);
    assert!(!refused.status.success(), "keeping ours would move a published tag");
    assert!(stderr(&refused).contains("never changed or removed"), "{}", stderr(&refused));

    assert_success(&area.forklift("a", &["lower", "--keep", "@tags:v1.0=theirs"]));
    let shown = stdout(&area.forklift("a", &["--json", "tag", "show", "v1.0"]));
    let value: serde_json::Value = serde_json::from_str(&shown).unwrap();
    assert_eq!(value["data"]["tagger"], worker_id, "{}", shown);
    assert_ne!(value["data"]["subject"], base.as_str(), "{}", shown);

    // The admin demotes the worker.
    assert_success(&area.forklift("a", &["office", "role", worker_id, "writer"]));
    assert_success(&area.forklift("a", &["lift"]));

    // A fresh franchise adopts the admitted release although its tagger is no longer an admin.
    let franchised = area.forklift(".", &["franchise", &server.url, "c", "--token", "root"]);
    assert_success(&franchised);
    assert!(stdout(&franchised).contains("Adopted the @tags pallet"), "{}", stdout(&franchised));
    assert!(stdout(&area.forklift("c", &["tag", "show", "v1.0"])).contains("release"));

    // The worker's unpublished tag is now refused by the server, and a new one locally.
    assert_success(&area.forklift("b", &["lower"]));
    let refused = area.forklift("b", &["lift"]);
    assert!(!refused.status.success(), "a demoted tagger's tag must not be admitted");
    assert!(stderr(&refused).contains("not an admin"), "{}", stderr(&refused));
    assert!(stderr(&refused).contains("\"lower --keep @tags:<path>=theirs\" drops it"), "{}", stderr(&refused));

    // The way out: lower with theirs at the refused tag. Ours is refused, since nothing on the
    // remote conflicts with it, and so is theirs at a path with no local change.
    let remote_tags = area.read_file("c/.forklift/meta/tags").trim().to_string();
    assert_ne!(area.read_file("b/.forklift/meta/tags").trim(), remote_tags, "b's @tags must start ahead");

    let kept_ours = area.forklift("b", &["lower", "--keep", "@tags:v1.1=ours"]);
    assert!(!kept_ours.status.success(), "{}", stdout(&kept_ours));
    assert!(stderr(&kept_ours).contains("v1.1 is not in conflict"), "{}", stderr(&kept_ours));

    let stale = area.forklift("b", &["lower", "--keep", "@tags:v9.9=theirs"]);
    assert!(!stale.status.success(), "{}", stdout(&stale));
    assert!(stderr(&stale).contains("v9.9 has no local change"), "{}", stderr(&stale));

    let refused = area.forklift("b", &["tag", "create", "v2.0", "main"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("is not an admin"), "{}", stderr(&refused));

    // With its key retired as well the worker can sign nothing, and theirs still drops the tag:
    // a drop signs no parcel.
    let list = stdout(&area.forklift("a", &["office", "list"]));
    let worker_key_id = list.lines()
        .rev()
        .find(|line| line.trim_start().starts_with("key "))
        .and_then(|line| line.split_whitespace().nth(1))
        .expect("office list shows the worker's key")
        .to_string();
    assert_success(&area.forklift("a", &["office", "retire", &worker_key_id]));
    assert_success(&area.forklift("a", &["lift"]));
    assert_success(&area.forklift("b", &["lower"]));

    assert_success(&area.forklift("b", &["lower", "--keep", "@tags:v1.1=theirs"]));
    assert_eq!(area.read_file("b/.forklift/meta/tags").trim(), remote_tags, "theirs returns @tags to the remote's head");
    assert!(!area.forklift("b", &["tag", "show", "v1.1"]).status.success(), "the refused tag is dropped");
    assert!(stdout(&area.forklift("b", &["tag", "show", "v1.0"])).contains("release"), "the admitted tag stays");
    assert_success(&area.forklift("b", &["lift"]));

    // In sync again, a resolution has nothing to resolve.
    let in_sync = area.forklift("b", &["lower", "--keep", "@tags:v1.1=theirs"]);
    assert!(!in_sync.status.success(), "{}", stdout(&in_sync));
    assert!(stderr(&in_sync).contains("@tags holds no local change the remote lacks"), "{}", stderr(&in_sync));

}

#[test]
fn hauls_sync_across_remotes_and_replay_when_diverged() {
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
    assert_success(&area.forklift("a", &["lift"]));
    let first_head = pallet_head(&area, "a", "feat");
    assert_success(&area.forklift("a", &["shift", "main"]));

    let opened = area.forklift("a", &["--json", "haul", "open", "--target", "main", "--source", "feat", "--title", "Feat", "-m", "review"]);
    assert_success(&opened);
    let id = serde_json::from_str::<serde_json::Value>(&stdout(&opened)).unwrap()["data"]["id"]
        .as_str().unwrap().to_string();

    let lifted = area.forklift("a", &["lift"]);
    assert_success(&lifted);
    assert!(stdout(&lifted).contains("Lifted the @haul pallet"), "{}", stdout(&lifted));

    // A franchise carries the haul.
    assert_success(&area.forklift(".", &["franchise", &server.url, "b"]));
    assert!(stdout(&area.forklift("b", &["haul", "show", &id])).contains("Feat"), "b must see the haul");

    // Diverge: B comments and publishes; A comments, and opens a second haul, on the same base
    // without lowering.
    assert_success(&area.forklift("b", &["haul", "comment", &id, "-m", "from b"]));
    assert_success(&area.forklift("b", &["lift"]));

    assert_success(&area.forklift("a", &["haul", "comment", &id, "-m", "from a"]));
    let second = area.forklift("a", &["--json", "haul", "open", "--target", "main", "--source", "feat", "--title", "Again"]);
    assert_success(&second);
    let second = serde_json::from_str::<serde_json::Value>(&stdout(&second)).unwrap()["data"]["id"]
        .as_str().unwrap().to_string();
    let refused = area.forklift("a", &["lift"]);
    assert!(!refused.status.success(), "a diverged @haul lift must be refused");
    assert!(!stderr(&refused).contains("drops it"), "a diverged pallet is lowered, not dropped: {}", stderr(&refused));

    // Keeping theirs at a haul A opened locally drops it whole; A's comment, under a different
    // path from B's, replays onto the remote's and lifts.
    let lowered = area.forklift("a", &["lower", "--keep", &format!("@haul:{}=theirs", second)]);
    assert_success(&lowered);
    assert!(stdout(&lowered).contains("Replayed the local @haul"), "{}", stdout(&lowered));

    let a_view = stdout(&area.forklift("a", &["haul", "show", &id]));
    assert!(a_view.contains("from a") && a_view.contains("from b"), "{}", a_view);
    assert!(!area.forklift("a", &["haul", "show", &second]).status.success(), "the kept-theirs haul is gone");

    // A close racing an update is a conflict on the state cell, named and resolvable: B closes
    // the haul while A moves it to new work.
    assert_success(&area.forklift("b", &["lower"]));
    assert_success(&area.forklift("b", &["haul", "close", &id]));
    assert_success(&area.forklift("b", &["lift"]));

    assert_success(&area.forklift("a", &["shift", "feat"]));
    area.write_file("a/more.txt", "y\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "more feat work"]));
    assert_success(&area.forklift("a", &["shift", "main"]));
    assert_success(&area.forklift("a", &["haul", "update", &id]));

    let conflicted = area.forklift("a", &["--json", "lower"]);
    assert_eq!(conflicted.status.code(), Some(24), "{}", stdout(&conflicted));
    let value: serde_json::Value = serde_json::from_str(&stdout(&conflicted)).unwrap();
    let pallets = &value["error"]["details"]["pallets"];
    assert_eq!(pallets[0]["pallet"], "@haul");
    assert_eq!(pallets[0]["conflicts"][0]["path"], format!("{}/state", id));
    assert_eq!(pallets[0]["conflicts"][0]["theirs"]["value"], format!("closed at head {}", first_head));

    assert_success(&area.forklift("a", &["lower", "--keep", &format!("@haul:{}/state=theirs", id)]));
    let show = |side: &str| serde_json::from_str::<serde_json::Value>(&stdout(&area.forklift(side, &["--json", "haul", "show", &id])))
        .unwrap()["data"].clone();
    assert_eq!((show("a")["status"].as_str(), show("a")["head"].as_str()), (Some("closed"), Some(first_head.as_str())));

    // A merge racing an update is a conflict on the same cell too, never a merge recorded at a
    // head it did not merge: B moves the haul to other work and lifts while A merges the head it
    // has. (B's source is its own `feat`, whose objects A never needs to hold.)
    assert_success(&area.forklift("b", &["haul", "reopen", &id]));
    assert_success(&area.forklift("b", &["palletize", "feat"]));
    area.write_file("b/other.txt", "z\n");
    assert_success(&area.forklift("b", &["load", "."]));
    assert_success(&area.forklift("b", &["stack", "other feat work"]));
    let other_head = pallet_head(&area, "b", "feat");
    assert_success(&area.forklift("b", &["shift", "main"]));
    assert_success(&area.forklift("b", &["haul", "update", &id]));
    assert_success(&area.forklift("b", &["lift"]));

    assert_success(&area.forklift("a", &["haul", "reopen", &id]));
    assert_success(&area.forklift("a", &["haul", "update", &id]));
    let a_head = pallet_head(&area, "a", "feat");
    assert_success(&area.forklift("a", &["haul", "merge", &id]));
    let merge_parcel = show("a")["merge_parcel"].as_str().unwrap().to_string();

    let raced = area.forklift("a", &["--json", "lower"]);
    assert_eq!(raced.status.code(), Some(24), "{}", stdout(&raced));
    let value: serde_json::Value = serde_json::from_str(&stdout(&raced)).unwrap();
    let conflict = &value["error"]["details"]["pallets"][0]["conflicts"][0];
    assert_eq!(conflict["path"], format!("{}/state", id));
    assert_eq!(conflict["ours"]["value"], format!("merged as {} at head {}", merge_parcel, a_head));
    assert_eq!(conflict["theirs"]["value"], format!("open at head {}", other_head));

    // Keeping ours records the merge at the head it merged, over B's move — once the remote's
    // main holds the merge. Until then the replay holds the record back, and a second lower,
    // after A's main is lifted, replays it.
    let held = area.forklift("a", &["lower", "--keep", &format!("@haul:{}/state=ours", id)]);
    assert!(!held.status.success(), "{}", stdout(&held));
    assert!(stderr(&held).contains("records a merge whose parcel the remote's target does not hold yet"), "{}", stderr(&held));
    assert!(stderr(&held).contains("run \"forklift lower\" again"), "{}", stderr(&held));
    assert!(stderr(&held).contains(&format!("\"forklift lower --keep @haul:{}/state=theirs\"", id)), "{}", stderr(&held));

    // Lifted from another pallet, the record would still wait, but @haul has diverged: the lift
    // says so, rather than sending A to lift a target that would not clear it.
    assert_success(&area.forklift("a", &["shift", "feat"]));
    let diverged = area.forklift("a", &["lift"]);
    assert!(!diverged.status.success(), "{}", stdout(&diverged));
    assert!(stderr(&diverged).contains("\"@haul\" and the remote have diverged"), "{}", stderr(&diverged));
    assert!(!stderr(&diverged).contains("was not lifted"), "{}", stderr(&diverged));
    assert_success(&area.forklift("a", &["shift", "main"]));

    let lifted = area.forklift("a", &["lift"]);
    assert!(!lifted.status.success(), "@haul is still diverged: {}", stdout(&lifted));
    assert_eq!(remote_pallet_head(&server, "main").as_deref(), Some(pallet_head(&area, "a", "main").as_str()));

    assert_success(&area.forklift("a", &["lower", "--keep", &format!("@haul:{}/state=ours", id)]));
    let merged = show("a");
    assert_eq!((merged["status"].as_str(), merged["head"].as_str()), (Some("merged"), Some(a_head.as_str())));
    assert_success(&area.forklift("a", &["lift"]));
    assert_success(&area.forklift("b", &["lower"]));
    assert_eq!(show("b")["status"], "merged");

    assert!(stdout(&area.forklift("b", &["audit", "@haul"])).contains("verified"));
}

/// The remote's head of a pallet (`main`, `@haul`), from its handshake.
fn remote_pallet_head(server: &Server, pallet: &str) -> Option<String> {
    let (_, info) = http_get(&server.url, "/v1/warehouse");
    let info: serde_json::Value = serde_json::from_str(&info).unwrap();

    info["pallets"][pallet].as_str().map(str::to_string)
}

/// Open a haul from `feat` into `main` in a warehouse under the area, returning its id.
fn open_haul(area: &TestArea, dir: &str, title: &str) -> String {
    let opened = area.forklift(dir, &["--json", "haul", "open", "--target", "main", "--source", "feat", "--title", title]);
    assert_success(&opened);

    serde_json::from_str::<serde_json::Value>(&stdout(&opened)).unwrap()["data"]["id"].as_str().unwrap().to_string()
}

/// Stack `content` at `path` on the current pallet of a warehouse under the area.
fn stack_file(area: &TestArea, dir: &str, path: &str, content: &str) {
    area.write_file(&format!("{}/{}", dir, path), content);
    assert_success(&area.forklift(dir, &["load", "."]));
    assert_success(&area.forklift(dir, &["stack", path]));
}

/// A warehouse "a" on `server` with an enrolled office, `main` and `feat` lifted, standing on main.
fn haul_warehouse(area: &TestArea, server: &Server) {
    prepare_warehouse(area, "a", &server.url);
    stack_file(area, "a", "base.txt", "base\n");
    assert_success(&area.forklift("a", &["office", "enroll"]));
    assert_success(&area.forklift("a", &["lift"]));

    assert_success(&area.forklift("a", &["palletize", "feat"]));
    stack_file(area, "a", "feat.txt", "x\n");
    assert_success(&area.forklift("a", &["lift"]));
    assert_success(&area.forklift("a", &["shift", "main"]));
}

/// A merge record lifts with its parcel: one lift carries the target and @haul; a record whose
/// parcel is on no target here (its merge undone) goes too; and @haul lifts with nothing stacked.
#[test]
fn a_haul_merge_lifts_with_its_target() {
    let area = TestArea::new("haul-lift-order");
    let server = Server::start(&area, None);
    haul_warehouse(&area, &server);
    let local_haul = || area.read_file("a/.forklift/meta/haul").trim().to_string();

    let first = open_haul(&area, "a", "First");
    assert_success(&area.forklift("a", &["lift"]));
    assert_success(&area.forklift("a", &["haul", "merge", &first]));

    let lifted = area.forklift("a", &["lift"]);
    assert_success(&lifted);
    assert!(stdout(&lifted).contains("Lifted the @haul pallet"), "{}", stdout(&lifted));
    assert_eq!(remote_pallet_head(&server, "main"), Some(pallet_head(&area, "a", "main")));
    assert_eq!(remote_pallet_head(&server, "@haul"), Some(local_haul()));

    // Undone, the merge parcel is on no target here: waiting would bring nothing, so it lifts.
    // A clean haul merge is not journaled, so this one conflicts and a stack completes it.
    assert_success(&area.forklift("a", &["shift", "feat"]));
    stack_file(&area, "a", "base.txt", "feat\n");
    assert_success(&area.forklift("a", &["shift", "main"]));
    let second = open_haul(&area, "a", "Second");
    stack_file(&area, "a", "base.txt", "main\n");
    let main = pallet_head(&area, "a", "main");
    assert_success(&area.forklift("a", &["haul", "merge", &second]));
    stack_file(&area, "a", "base.txt", "resolved\n");
    assert_success(&area.forklift("a", &["haul", "merge", &second]));
    assert_eq!(serde_json::from_str::<serde_json::Value>(&stdout(&area.forklift("a", &["--json", "haul", "show", &second])))
        .unwrap()["data"]["status"], "merged");
    assert_success(&area.forklift("a", &["undo"]));
    assert_eq!(pallet_head(&area, "a", "main"), main, "the undo took main back");

    let lifted = area.forklift("a", &["lift"]);
    assert_success(&lifted);
    assert!(stdout(&lifted).contains("Lifted the @haul pallet"), "{}", stdout(&lifted));
    assert_eq!(remote_pallet_head(&server, "@haul"), Some(local_haul()));

    // With nothing stacked on the working pallet, @haul still lifts. No command stands a bay on a
    // pallet with no head once the warehouse has one, so the position is written by hand.
    open_haul(&area, "a", "Third");
    let before = remote_pallet_head(&server, "@haul");
    let position = area.read_file("a/.forklift/position");
    let header = position.lines().next().unwrap();
    std::fs::write(area.path("a/.forklift/position"), format!("{}\nfresh\n\n", header)).unwrap();

    let unstacked = area.forklift("a", &["lift"]);
    assert!(!unstacked.status.success());
    assert!(stderr(&unstacked).contains("has nothing stacked yet"), "{}", stderr(&unstacked));
    assert!(!stderr(&unstacked).contains("@haul"), "{}", stderr(&unstacked));
    assert_eq!(remote_pallet_head(&server, "@haul"), Some(local_haul()));
    assert_ne!(remote_pallet_head(&server, "@haul"), before, "the new haul went");
}

/// A merge record whose parcel the remote's target does not hold waits: when the target's own
/// lift fails, and when another operator moved the remote's target to a head not held here. The
/// rest still lifts, and the record goes once the target does.
#[test]
fn a_haul_merge_waits_while_the_remote_target_lacks_its_parcel() {
    let area = TestArea::new("haul-held");
    let server = Server::start(&area, None);
    haul_warehouse(&area, &server);

    let first = open_haul(&area, "a", "First");
    assert_success(&area.forklift("a", &["lift"]));
    assert_success(&area.forklift(".", &["franchise", &server.url, "b"]));
    let remote_haul = remote_pallet_head(&server, "@haul");

    // A merges and shifts away; B moves the remote's main; A's lift holds the record.
    assert_success(&area.forklift("a", &["haul", "merge", &first]));
    assert_success(&area.forklift("a", &["shift", "feat"]));
    stack_file(&area, "b", "b.txt", "b\n");
    assert_success(&area.forklift("b", &["lift"]));

    let held = area.forklift("a", &["lift"]);
    assert!(!held.status.success(), "{}", stdout(&held));
    let why = stderr(&held);
    assert!(why.contains("@haul was not lifted"), "{}", why);
    assert!(why.contains(&format!("haul {} merged as", &first[..12])) && why.contains("into \"main\""), "{}", why);
    assert!(why.contains("an operator who may write the target lifts it"), "{}", why);
    assert_eq!(remote_pallet_head(&server, "@haul"), remote_haul, "the remote's @haul did not move");

    // On main, a lower fetches B's head (and refuses the divergence); the lift then merges it,
    // lands main, and the record goes with it.
    assert_success(&area.forklift("a", &["shift", "main"]));
    let _ = area.forklift("a", &["lower"]);
    let lifted = area.forklift("a", &["lift"]);
    assert_success(&lifted);
    assert!(stdout(&lifted).contains("Lifted the @haul pallet"), "{}", stdout(&lifted));
    assert_eq!(remote_pallet_head(&server, "@haul"), Some(area.read_file("a/.forklift/meta/haul").trim().to_string()));

    // The target's own lift fails on an overlap: the record is held, the rest of the meta
    // pallets lift.
    let second = open_haul(&area, "a", "Second");
    assert_success(&area.forklift("a", &["lift"]));
    let remote_haul = remote_pallet_head(&server, "@haul");

    assert_success(&area.forklift("b", &["lower"]));
    stack_file(&area, "b", "base.txt", "from b\n");
    assert_success(&area.forklift("b", &["lift"]));

    stack_file(&area, "a", "base.txt", "from a\n");
    assert_success(&area.forklift("a", &["shift", "feat"]));
    stack_file(&area, "a", "feat2.txt", "z\n");
    assert_success(&area.forklift("a", &["shift", "main"]));
    assert_success(&area.forklift("a", &["haul", "update", &second]));
    assert_success(&area.forklift("a", &["haul", "merge", &second]));
    assert_success(&area.forklift("a", &["tag", "create", "v1.0", "main"]));

    let failed = area.forklift("a", &["lift"]);
    assert!(!failed.status.success(), "{}", stdout(&failed));
    let why = stderr(&failed);
    assert!(why.contains(&format!("haul {} merged as", &second[..12])) && why.contains("into \"main\""), "{}", why);
    assert_eq!(remote_pallet_head(&server, "@haul"), remote_haul, "the remote's @haul did not move");
    assert_eq!(remote_pallet_head(&server, "@tags"), Some(area.read_file("a/.forklift/meta/tags").trim().to_string()));
}

/// A held merge record names a way out for an operator who cannot lift the target: followed as
/// printed, it drops only the record, and the rest of @haul (a comment) replays and lifts.
#[test]
fn a_held_haul_merge_names_a_drop_that_keeps_the_rest_of_the_haul() {
    let area = TestArea::new("haul-held-drop");
    let server = Server::start(&area, None);
    haul_warehouse(&area, &server);

    let id = open_haul(&area, "a", "First");
    assert_success(&area.forklift("a", &["lift"]));
    assert_success(&area.forklift("a", &["haul", "comment", &id, "-m", "kept"]));
    assert_success(&area.forklift("a", &["haul", "merge", &id]));
    assert_success(&area.forklift("a", &["shift", "feat"]));

    let held = area.forklift("a", &["lift"]);
    assert!(!held.status.success(), "{}", stdout(&held));
    let why = stderr(&held);
    let printed = format!("\"forklift lower --keep @haul:{}/state=theirs\"", id);
    assert!(why.contains(&printed), "{}", why);

    let command: Vec<&str> = printed.trim_matches('"').split(' ').skip(1).collect();
    let dropped = area.forklift("a", &command);
    assert_success(&dropped);
    assert!(stdout(&dropped).contains("Replayed the local @haul"), "{}", stdout(&dropped));

    let shown: serde_json::Value = serde_json::from_str(&stdout(&area.forklift("a", &["--json", "haul", "show", &id]))).unwrap();
    assert_eq!(shown["data"]["status"], "open", "the merge record is dropped");
    assert!(stdout(&area.forklift("a", &["haul", "show", &id])).contains("kept"), "the comment stays");
    assert_eq!(remote_pallet_head(&server, "@haul"), Some(area.read_file("a/.forklift/meta/haul").trim().to_string()));
}

/// A remote with no @haul yet gives "lower --keep" no remote side to keep, so a merge held on
/// its first lift names only lifting the target.
#[test]
fn a_held_haul_merge_on_a_remote_without_haul_names_no_drop() {
    let area = TestArea::new("haul-held-first");
    let server = Server::start(&area, None);
    haul_warehouse(&area, &server);
    assert_eq!(remote_pallet_head(&server, "@haul"), None);

    let id = open_haul(&area, "a", "First");
    assert_success(&area.forklift("a", &["haul", "merge", &id]));
    assert_success(&area.forklift("a", &["shift", "feat"]));

    let held = area.forklift("a", &["lift"]);
    assert!(!held.status.success(), "{}", stdout(&held));
    let why = stderr(&held);
    assert!(why.contains(&format!("haul {} merged as", &id[..12])), "{}", why);
    assert!(why.contains("an operator who may write the target lifts it"), "{}", why);
    assert!(!why.contains("--keep"), "{}", why);
    assert_eq!(remote_pallet_head(&server, "@haul"), None);
}

/// A fresh remote admits the whole @haul history against today's office, so a merge whose signer
/// has since lost write on the target is refused there, naming both; re-granted, it goes.
#[test]
fn a_rehost_refuses_a_merge_by_a_writer_since_ungranted_until_regranted() {
    let area = TestArea::new("haul-rehost");
    let server = Server::start(&area, None);
    prepare_warehouse(&area, "a", &server.url);
    stack_file(&area, "a", "base.txt", "base\n");
    assert_success(&area.forklift("a", &["office", "enroll"]));
    assert_success(&area.forklift("a", &["palletize", "feat"]));
    stack_file(&area, "a", "feat.txt", "x\n");
    assert_success(&area.forklift("a", &["shift", "main"]));
    let id = open_haul(&area, "a", "Feat");

    let dev = keygen_admit_args(&area, "dev@forklift");
    assert_success(&area.forklift("a", &["office", "admit", &dev[0], &dev[1], &dev[2], "--role", "writer"]));
    assert_success(&area.forklift("a", &["config", "operator.identifier", "dev@forklift"]));
    assert_success(&area.forklift("a", &["haul", "merge", &id]));
    assert_success(&area.forklift("a", &["config", "operator.identifier", "tester@forklift"]));
    assert_success(&area.forklift("a", &["office", "role", "dev@forklift", "writer", "--pallet", "feat"]));

    let refused = area.forklift("a", &["lift"]);
    assert!(!refused.status.success(), "{}", stdout(&refused));
    assert!(
        stderr(&refused).contains("records a merge into main, but is signed by dev@forklift, who may not write it"),
        "{}", stderr(&refused)
    );
    assert_eq!(remote_pallet_head(&server, "main"), Some(pallet_head(&area, "a", "main")), "the target itself lifted");
    assert_eq!(remote_pallet_head(&server, "@haul"), None);

    assert_success(&area.forklift("a", &["office", "role", "dev@forklift", "writer"]));
    let lifted = area.forklift("a", &["lift"]);
    assert_success(&lifted);
    assert!(stdout(&lifted).contains("Lifted the @haul pallet"), "{}", stdout(&lifted));
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
fn a_bundle_that_lags_the_remote_heads_still_imports_with_its_signatures() {
    // A bundle is rebuilt on the server's own schedule, so the heads may have moved past it. Each
    // of its signature entries names a parcel its own packs carry, so it imports whole, sidecars
    // included, and the walk fetches only what was lifted since.
    let area = TestArea::new("bundle-lagging");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    assert_success(&area.forklift("a", &["office", "enroll"]));
    area.write_file("a/one.txt", "one\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "bundled"]));
    assert_success(&area.forklift("a", &["lift"]));

    server.rebuild_bundle(&area);

    area.write_file("a/two.txt", "two\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "lifted after the bundle"]));
    assert_success(&area.forklift("a", &["lift"]));

    let franchised = area.forklift(".", &["franchise", &server.url, "c"]);
    assert_success(&franchised);
    assert!(stdout(&franchised).contains("Imported the remote's bundle"), "{}", stdout(&franchised));
    assert!(!stdout(&franchised).contains("and 0 signature(s)"),
            "the bundle's signed parcels keep their sidecars: {}", stdout(&franchised));
    assert!(!stdout(&franchised).contains("0 object(s) fetched loose"),
            "the lift after the bundle came through the walk: {}", stdout(&franchised));

    assert_eq!(area.read_file("c/one.txt"), "one\n");
    assert_eq!(area.read_file("c/two.txt"), "two\n");
    assert_success(&area.forklift("c", &["audit", "main"]));
}

/// A whole-warehouse bundle is read only in the version this build writes. A file in another one —
/// an older record stream, or the previous native envelope a server not yet upgraded still serves —
/// is set aside as unsupported, never imported by a reader that could not bound what it stores or
/// check its digest, and the franchise completes through its verified walk. forklift-server itself
/// answers such a file `404`, so a proxy stands in for a head that serves one anyway.
///
/// Revert: treat an unsupported bundle as a failed import in `franchise` → the clone fails → red.
#[test]
fn a_bundle_in_another_version_is_refused_and_franchise_falls_back_to_the_walk() {
    let area = TestArea::new("bundle-foreign-version");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/one.txt", "one\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "walked"]));
    assert_success(&area.forklift("a", &["lift"]));

    for (name, foreign) in [("record-stream", "forklift-bundle 2026-07-11\nnever read"),
                            ("previous-native", "forklift-bundle 2026-07-13\nnever read")] {
        let served = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&served);
        let proxy = InterceptingProxy::start(&server.url, move |path| {
            path.ends_with("/bundles/latest").then(|| {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    foreign.len(), foreign
                ).into_bytes()
            })
        });

        let franchised = area.forklift(".", &["franchise", &proxy.url, name]);
        assert_success(&franchised);
        assert_eq!(served.load(std::sync::atomic::Ordering::SeqCst), 1, "{}: the bundle was offered", name);
        assert!(!stdout(&franchised).contains("Imported the remote's bundle"), "{}", stdout(&franchised));
        assert_eq!(area.read_file(&format!("{}/one.txt", name)), "one\n");
        assert!(!area.path(&format!("{}/.forklift/bundles/latest", name)).exists(),
                "{}: an unsupported bundle is not kept as the warehouse's own", name);
    }
}

/// An upgraded forklift-server still holding a latest bundle built before the upgrade answers it
/// `404`, and the franchise completes through its verified walk without it.
///
/// Revert: treat a `404` for the bundle as a failed download in `franchise` → the clone fails → red.
#[test]
fn a_server_holding_a_previous_version_bundle_still_franchises() {
    let area = TestArea::new("bundle-server-previous-version");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/one.txt", "one\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "walked"]));
    assert_success(&area.forklift("a", &["lift"]));

    server.rebuild_bundle(&area);
    std::fs::write(area.path("server-root/.forklift/bundles/latest"),
                   "forklift-bundle 2026-07-13\nbuilt before the upgrade").unwrap();

    let franchised = area.forklift(".", &["franchise", &server.url, "b"]);
    assert_success(&franchised);
    assert!(!stdout(&franchised).contains("Imported the remote's bundle"), "{}", stdout(&franchised));
    assert_eq!(area.read_file("b/one.txt"), "one\n");
    assert!(!area.path("b/.forklift/bundles/latest").exists());
}

/// A head in front of a real server that answers the bundle itself, from `answer(n, head)` for its
/// `n`th bundle request (the head lowercased), and forwards everything else. Each bundle request's
/// head is kept.
struct BundleStub {
    url: String,
    heads: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl BundleStub {
    fn start(upstream: &str, answer: impl Fn(usize, &str) -> Vec<u8> + Send + Sync + 'static) -> BundleStub {
        let heads = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = std::sync::Arc::clone(&heads);
        let proxy = InterceptingProxy::start_seeing(upstream, move |path, head| {
            path.ends_with("/bundles/latest").then(|| {
                let head = head.to_ascii_lowercase();
                let index = {
                    let mut heads = recorded.lock().unwrap();
                    heads.push(head.clone());
                    heads.len() - 1
                };

                answer(index, &head)
            })
        });

        BundleStub { url: proxy.url, heads }
    }

    fn heads(&self) -> Vec<String> {
        self.heads.lock().unwrap().clone()
    }
}

/// A `200` for `bundle` under `tag`, declaring its whole length but sending only its first `sent`
/// bytes before the connection closes.
fn bundle_answer(tag: &str, bundle: &[u8], sent: usize) -> Vec<u8> {
    let mut answer = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
        bundle.len(), tag
    ).into_bytes();
    answer.extend_from_slice(&bundle[..sent]);
    answer
}

/// The `206` that continues `bundle` from `start` under `tag`.
fn rest_of_bundle_answer(tag: &str, bundle: &[u8], start: usize) -> Vec<u8> {
    let mut answer = format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nETag: {}\r\n\
        Connection: close\r\n\r\n",
        bundle.len() - start, start, bundle.len() - 1, bundle.len(), tag
    ).into_bytes();
    answer.extend_from_slice(&bundle[start..]);
    answer
}

/// The value of `name` in a lowercased request head.
fn request_header(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|line| line.strip_prefix(&format!("{}:", name)).map(|value| value.trim().to_string()))
}

/// The value of `key` in the franchise record of `directory`.
fn franchise_record_value(area: &TestArea, directory: &str, key: &str) -> Option<String> {
    area.read_file(&format!("{}/.forklift/franchise", directory)).lines()
        .find_map(|line| line.strip_prefix(&format!("{}=", key)).map(str::to_string))
}

/// A remote warehouse whose bundle is large enough to cut after several small checkpoints, and
/// that bundle's bytes.
fn warehouse_with_a_bundle(area: &TestArea) -> (Server, Vec<u8>) {
    let server = Server::start(area, None);

    prepare_warehouse(area, "a", &server.url);
    area.write_large_file("a/large.bin", 266, 96 * 1024);
    area.write_file("a/one.txt", "one\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "bundled"]));
    assert_success(&area.forklift("a", &["lift"]));
    server.rebuild_bundle(area);

    let bundle = std::fs::read(area.path("server-root/.forklift/bundles/latest")).unwrap();
    (server, bundle)
}

/// Run `franchise` into `directory`, checkpointing the bundle download every 8 KiB.
fn franchise_checkpointing(area: &TestArea, url: &str, directory: &str) -> Output {
    forklift_command()
        .args(["--json", "franchise", url, directory])
        .current_dir(area.path("."))
        .env("FORKLIFT_GLOBAL_CONFIG", area.path("area-global.toml"))
        .env("FORKLIFT_KEYS_DIR", area.path("shared-keys"))
        .env("FORKLIFT_BUNDLE_CHECKPOINT_BYTES", "8192")
        .output()
        .unwrap()
}

/// A bundle download cut off part-way is kept, synced and recorded, and the next attempt asks for
/// only the rest under the same tag and imports the whole.
///
/// Reverts: `fail()` writing the record without the download's last checkpoint → the record lags
/// the file → red; `prepare_resume` dropping the download → a plain request → red; a plain request
/// on resume → no `Range` → red.
#[test]
fn a_cut_off_bundle_download_is_resumed_for_the_rest_only() {
    let area = TestArea::new("bundle-resume");
    let (server, bundle) = warehouse_with_a_bundle(&area);
    let cut = bundle.len() / 2 + 17;
    let served = bundle.clone();
    let stub = BundleStub::start(&server.url, move |index, _| match index {
        0 => bundle_answer("\"t1\"", &served, cut),
        _ => rest_of_bundle_answer("\"t1\"", &served, cut),
    });

    let failed = franchise_checkpointing(&area, &stub.url, "b");
    assert!(!failed.status.success(), "{}", stdout(&failed));
    assert_eq!(franchise_record_value(&area, "b", "bundle").as_deref(), Some("partial"));
    assert_eq!(franchise_record_value(&area, "b", "bundle.etag").as_deref(), Some("\"t1\""));
    assert_eq!(franchise_record_value(&area, "b", "bundle.total"), Some(bundle.len().to_string()));
    assert_eq!(franchise_record_value(&area, "b", "bundle.length"), Some(cut.to_string()));
    assert_eq!(std::fs::read(area.path("b/.forklift/bundles/partial")).unwrap(), &bundle[..cut]);

    let resumed = franchise_checkpointing(&area, &stub.url, "b");
    assert_success(&resumed);
    assert!(json_of(&resumed)["data"].get("bundle_objects").is_some(), "{}", stdout(&resumed));
    assert_eq!(area.read_file("b/one.txt"), "one\n");
    assert!(area.path("b/.forklift/bundles/latest").is_file());
    assert!(!area.path("b/.forklift/bundles/partial").exists());

    let heads = stub.heads();
    assert_eq!(heads.len(), 2);
    assert_eq!(request_header(&heads[0], "range"), None);
    assert_eq!(request_header(&heads[1], "range"), Some(format!("bytes={}-", cut)));
    assert_eq!(request_header(&heads[1], "if-range").as_deref(), Some("\"t1\""));
}

/// A resumed download that does not reassemble the bundle — its rest from another build under the
/// same tag, or its kept start damaged — is downloaded once more in full within the same attempt,
/// and nothing of it is imported first.
///
/// Reverts: no restart → the attempt fails → red; restart only on the digest error → the damaged
/// header fails the attempt → red.
#[test]
fn a_resumed_bundle_that_does_not_reassemble_is_downloaded_again_in_full() {
    let area = TestArea::new("bundle-resume-spliced");
    let (server, bundle) = warehouse_with_a_bundle(&area);
    let cut = bundle.len() / 2;

    for (name, damage_rest) in [("spliced", true), ("zeroed", false)] {
        let served = bundle.clone();
        let mut other_build = bundle.clone();
        other_build[cut + 5] ^= 0xFF;
        let stub = BundleStub::start(&server.url, move |index, _| match index {
            0 => bundle_answer("\"t1\"", &served, cut),
            1 if damage_rest => rest_of_bundle_answer("\"t1\"", &other_build, cut),
            1 => rest_of_bundle_answer("\"t1\"", &served, cut),
            _ => bundle_answer("\"t1\"", &served, served.len()),
        });

        assert!(!franchise_checkpointing(&area, &stub.url, name).status.success());

        if !damage_rest {
            let partial = area.path(&format!("{}/.forklift/bundles/partial", name));
            let mut kept = std::fs::read(&partial).unwrap();
            kept[..64].fill(0);
            std::fs::write(&partial, kept).unwrap();
        }

        let resumed = franchise_checkpointing(&area, &stub.url, name);
        assert_success(&resumed);
        assert!(json_of(&resumed)["data"].get("bundle_objects").is_some(), "{}: {}", name, stdout(&resumed));

        let heads = stub.heads();
        assert_eq!(heads.len(), 3, "{}", name);
        assert_eq!(request_header(&heads[1], "range"), Some(format!("bytes={}-", cut)), "{}", name);
        assert_eq!(request_header(&heads[2], "range"), None, "{}: downloaded once more in full", name);
    }
}

/// A bundle downloaded in one piece that fails its digest fails the attempt and keeps nothing: it
/// was served whole, so downloading it again would only repeat the failure.
///
/// Revert: restart on every failure → the attempt downloads it again → red.
#[test]
fn a_bundle_failing_its_digest_in_one_piece_fails_the_attempt_and_keeps_nothing() {
    let area = TestArea::new("bundle-one-piece-digest");
    let (server, mut bundle) = warehouse_with_a_bundle(&area);
    let middle = bundle.len() / 2;
    bundle[middle] ^= 0xFF;
    let stub = BundleStub::start(&server.url, move |_, _| bundle_answer("\"t1\"", &bundle, bundle.len()));

    let failed = franchise_checkpointing(&area, &stub.url, "b");
    assert!(!failed.status.success(), "{}", stdout(&failed));
    assert_eq!(stub.heads().len(), 1);
    assert_eq!(franchise_record_value(&area, "b", "bundle").as_deref(), Some("none"));
    assert!(!area.path("b/.forklift/bundles/partial").exists());
    assert!(!area.path("b/.forklift/bundles/latest").exists());
}

/// A complete bundle whose digest holds but whose contents do not import fails the attempt and
/// keeps nothing, whether it came in one piece, was already whole at the attempt's start, or was
/// completed by a `206`; once the head serves a good bundle, the next attempt downloads it afresh.
///
/// Revert: leave the record at partial → the next attempt resumes the bad file → red.
#[test]
fn a_digest_valid_bundle_that_fails_import_keeps_nothing() {
    let area = TestArea::new("bundle-fails-import");
    let (server, good) = warehouse_with_a_bundle(&area);

    // The same bundle with a body byte changed and its digest sealed over the change.
    let digested = good.len() - 32;
    let mut bad = good[..digested].to_vec();
    bad[digested / 2] ^= 0xFF;
    let digest = blake3::hash(&bad);
    bad.extend_from_slice(digest.as_bytes());
    let cut = bad.len() / 2;

    for (name, kept) in [("one-piece", None), ("kept-whole", Some(bad.len())), ("completed", Some(cut))] {
        let (served_bad, served_good) = (bad.clone(), good.clone());
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempt = std::sync::Arc::clone(&attempts);
        let stub = BundleStub::start(&server.url, move |_, head| {
            match (attempt.load(std::sync::atomic::Ordering::SeqCst), request_header(head, "range")) {
                (0, None) if kept.is_some() => bundle_answer("\"t1\"", &served_bad, cut),
                (0, None) => bundle_answer("\"t1\"", &served_bad, served_bad.len()),
                (0, Some(_)) => rest_of_bundle_answer("\"t1\"", &served_bad, cut),
                _ => bundle_answer("\"t2\"", &served_good, served_good.len()),
            }
        });

        if kept.is_some() {
            assert!(!franchise_checkpointing(&area, &stub.url, name).status.success(), "{}", name);
        }

        if kept == Some(bad.len()) {
            // Whole but not yet imported, as a crash between the last byte and the import leaves it.
            std::fs::write(area.path(&format!("{}/.forklift/bundles/partial", name)), &bad).unwrap();
            let record = area.read_file(&format!("{}/.forklift/franchise", name))
                .replace(&format!("bundle.length={}", cut), &format!("bundle.length={}", bad.len()));
            area.write_file(&format!("{}/.forklift/franchise", name), &record);
        }

        let failed = franchise_checkpointing(&area, &stub.url, name);
        assert!(!failed.status.success(), "{}: {}", name, stdout(&failed));
        assert_eq!(franchise_record_value(&area, name, "bundle").as_deref(), Some("none"), "{}", name);
        assert!(stdout(&failed).contains("is corrupt"), "{}: the import failed, not the envelope: {}", name, stdout(&failed));
        assert_eq!(stub.heads().len(), if name == "completed" { 2 } else { 1 }, "{}: a whole kept file needs no request", name);
        assert!(!area.path(&format!("{}/.forklift/bundles/partial", name)).exists(), "{}", name);

        attempts.store(1, std::sync::atomic::Ordering::SeqCst);
        let heads_before = stub.heads().len();
        let finished = franchise_checkpointing(&area, &stub.url, name);
        assert_success(&finished);
        assert!(json_of(&finished)["data"].get("bundle_objects").is_some(), "{}: {}", name, stdout(&finished));
        assert_eq!(request_header(&stub.heads()[heads_before], "range"), None, "{}: requested afresh", name);
    }
}

/// A head that no longer has the bundle an earlier attempt kept part of drops that download, and
/// the franchise completes through its verified walk.
///
/// Revert: skip the bundle phase on a `404` without clearing → the partial and its record stay → red.
#[test]
fn a_kept_bundle_download_the_head_no_longer_has_is_dropped() {
    let area = TestArea::new("bundle-resume-gone");
    let (server, bundle) = warehouse_with_a_bundle(&area);
    let cut = bundle.len() / 2;
    let stub = BundleStub::start(&server.url, move |index, _| match index {
        0 => bundle_answer("\"t1\"", &bundle, cut),
        _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
    });

    assert!(!franchise_checkpointing(&area, &stub.url, "b").status.success());
    assert!(area.path("b/.forklift/bundles/partial").is_file());

    let finished = franchise_checkpointing(&area, &stub.url, "b");
    assert_success(&finished);
    assert!(json_of(&finished)["data"].get("bundle_objects").is_none(), "{}", stdout(&finished));
    assert_eq!(area.read_file("b/one.txt"), "one\n");
    assert!(!area.path("b/.forklift/bundles/partial").exists());
    assert!(!area.path("b/.forklift/bundles/latest").exists());
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

    // B's next sync is refused loudly, pointing at the conscious re-accept, and the refusal
    // names both values: the remote's genesis now and the one B holds.
    let refused = area.forklift("b", &["lower"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("CHANGED"), "{}", stderr(&refused));
    assert!(stderr(&refused).contains("accept-regenesis --confirm"), "{}", stderr(&refused));

    let refused = area.forklift("b", &["--json", "lower"]);
    let details = trust_changed_details(&refused);
    assert_eq!(details["what_changed"], "regenesis");
    assert_eq!(details["subject"], server.url.as_str());
    let current = details["current"].as_str().unwrap().to_string();
    let previous = details["previous"].as_str().unwrap().to_string();
    assert_ne!(current, previous);

    // A pin is checked before the held anchor is compared, so a wrong one names itself.
    let pinned = area.forklift("b", &["--json", "lower", "--expect", &previous]);
    assert_eq!(trust_changed_details(&pinned)["what_changed"], "unexpected");

    // The description alone accepts nothing; it refuses with the same details.
    let dry = area.forklift("b", &["office", "accept-regenesis"]);
    assert!(!dry.status.success());
    assert!(stdout(&dry).contains("CHANGED"), "{}", stdout(&dry));
    let dry = area.forklift("b", &["--json", "office", "accept-regenesis"]);
    assert_eq!(trust_changed_details(&dry)["current"], current.as_str());

    // --confirm binds the acceptance to the value shown: any other value is refused, with
    // fresh details naming the current one, and nothing is accepted.
    let wrong = area.forklift("b", &["--json", "office", "accept-regenesis", "--confirm", &previous]);
    let details = trust_changed_details(&wrong);
    assert_eq!(details["current"], current.as_str());
    assert_eq!(details["previous"], previous.as_str());
    assert!(!area.forklift("b", &["lower"]).status.success(), "a refused confirm accepted nothing");

    assert_success(&area.forklift("b", &["office", "accept-regenesis", "--confirm", &current]));

    // Nothing is pending now. Re-confirming the held value is success; confirming any other is
    // refused, so a caller never reads "nothing to accept" as its value being trusted.
    assert_success(&area.forklift("b", &["office", "accept-regenesis", "--confirm", &current]));
    let stale = area.forklift("b", &["--json", "office", "accept-regenesis", "--confirm", &previous]);
    let details = trust_changed_details(&stale);
    assert_eq!(details["what_changed"], "unexpected");
    assert_eq!(details["current"], current.as_str());
    assert_success(&area.forklift("b", &["lower"]));

    // B's offline audit passes with the prior history attested (legacy).
    let audit = area.forklift("b", &["audit"]);
    assert!(audit.status.success(), "{}", stderr(&audit));
    assert!(stdout(&audit).contains("legacy parcel(s)"), "{}", stdout(&audit));
}

/// Plant crash debris in a warehouse — a torn taint, a parseable prefix with no `END` line — so
/// the next `heal` walks every root, the anchor's `adopts` pin included, and refuses (exit 21) if
/// any root that must be held is absent.
fn plant_torn_taint(area: &TestArea, warehouse: &str) {
    let taint_dir = area.path(warehouse).join(".forklift").join("taint");
    std::fs::create_dir_all(&taint_dir).unwrap();
    std::fs::write(taint_dir.join("taint-99999-0"), b"/forklift-taint 1\nobjects/ab/cdef\n").unwrap();
}

/// Set up warehouse A on a server with static token "root", enrolled and lifted, then re-genesis
/// it and lift the reset. Returns the server; `before_reset` runs after A's first lift.
fn regenesis_on_remote(area: &TestArea, before_reset: impl FnOnce(&Server)) -> Server {
    area.write_file("tokens.toml", concat!(
        "[operators]\n",
        "\"w-token\" = \"worker@forklift\"\n",
    ));
    let server = Server::start_with_tokens(area, Some("root"), "tokens.toml");

    prepare_warehouse(area, "a", &server.url);
    assert_success(&area.forklift("a", &["config", "remote.token", "root"]));
    area.write_file("a/code.txt", "v1\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "base"]));
    assert_success(&area.forklift("a", &["office", "enroll"]));
    assert_success(&area.forklift("a", &["lift"]));

    before_reset(&server);

    std::fs::remove_dir_all(area.path("shared-keys")).unwrap();
    assert_success(&area.forklift("a", &["office", "regenesis", "--confirm"]));
    assert_success(&area.forklift("a", &["lift"]));
    server
}

/// FORK-114: a franchise made after a re-genesis copies the remote's anchor, whose `adopts` names
/// the remote's pre-reset office head — history the new office chain never reaches. The
/// franchise must fetch it, or every `heal` on it reports that pin dangling and refuses forever.
#[test]
fn a_franchise_after_a_regenesis_holds_the_adopted_history_and_heals() {
    let area = TestArea::new("regenesis-franchise-heal");
    let server = regenesis_on_remote(&area, |_| {});

    assert_success(&area.forklift(".", &["franchise", &server.url, "c", "--token", "root"]));
    plant_torn_taint(&area, "c");
    let healed = area.forklift("c", &["heal"]);
    assert!(healed.status.success(),
        "heal must clear on a franchise of a re-genesis'd remote: {:?}\n{}",
        healed.status.code(), stderr(&healed));
}

/// A resume enforces the trust genesis its first attempt saw, as if `--expect` were given again:
/// a remote that re-genesised between the attempts is refused, never adopted, and the token the
/// first attempt was given is reused when the resume omits it.
#[test]
fn a_resume_refuses_a_remote_whose_anchor_changed_since_the_first_attempt() {
    let area = TestArea::new("franchise-resume-regenesis");
    let server = regenesis_on_remote(&area, |server| {
        let failed = area.forklift(".", &["franchise", &server.url, "half", "--token", "root", "--only", "nowhere"]);
        assert_eq!(failed.status.code(), Some(30), "{}{}", stdout(&failed), stderr(&failed));
    });

    let refused = area.forklift(".", &["--json", "franchise", &server.url, "half"]);
    assert_eq!(refused.status.code(), Some(22), "{}{}", stdout(&refused), stderr(&refused));
    let value = json_of(&refused);
    assert_eq!(value["error"]["code"], "trust_changed", "{}", value);
    assert!(value["error"]["message"].as_str().unwrap().contains("is bound to the genesis its first attempt saw"), "{}", value);
    assert!(value["error"]["next_step"].as_str().unwrap().contains("delete \"half\" and franchise again"), "{}", value);
    assert!(area.path("half/.forklift/franchise").is_file(), "a refused resume keeps the directory");

    // A fresh franchise of the same remote adopts the new anchor (with a warning), as always.
    assert_success(&area.forklift(".", &["franchise", &server.url, "fresh", "--token", "root"]));
}

/// FORK-114, the accepting side: a clone whose office fell behind before the reset accepts the
/// re-genesis. Its anchor's `adopts` names the remote's pre-reset office head, which the clone
/// never lowered; accepting must fetch it, exactly as a fresh franchise does.
#[test]
fn accepting_a_regenesis_from_behind_holds_the_adopted_history_and_heals() {
    let area = TestArea::new("regenesis-accept-heal");
    let server = regenesis_on_remote(&area, |server| {
        assert_success(&area.forklift(".", &["franchise", &server.url, "b", "--token", "w-token"]));
        // The office moves on after B's franchise, so B never holds the head the reset adopts.
        let worker = keygen_admit_args(&area, "worker@forklift");
        assert_success(&area.forklift("a", &["office", "admit", &worker[0], &worker[1], &worker[2]]));
        assert_success(&area.forklift("a", &["lift"]));
    });
    let _ = &server;

    let refused = area.forklift("b", &["--json", "lower"]);
    let current = trust_changed_details(&refused)["current"].as_str().unwrap().to_string();
    assert_success(&area.forklift("b", &["office", "accept-regenesis", "--confirm", &current]));

    plant_torn_taint(&area, "b");
    let healed = area.forklift("b", &["heal"]);
    assert!(healed.status.success(),
        "heal must clear after accepting a re-genesis from behind: {:?}\n{}",
        healed.status.code(), stderr(&healed));
}

/// FORK-114, the lifting side: a warehouse that re-genesised before it ever lifted establishes
/// its anchor on a fresh remote. The office lift carries only the new chain, so the adopted
/// history must be uploaded with the anchor — or the remote holds a pin it cannot serve, and
/// every franchise of it fails.
#[test]
fn a_regenesis_lifted_to_a_fresh_remote_carries_its_adopted_history() {
    let area = TestArea::new("regenesis-first-lift");
    let server = Server::start(&area, Some("root"));

    prepare_warehouse(&area, "a", &server.url);
    assert_success(&area.forklift("a", &["config", "remote.token", "root"]));
    area.write_file("a/code.txt", "v1\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "base"]));
    assert_success(&area.forklift("a", &["office", "enroll"]));
    std::fs::remove_dir_all(area.path("shared-keys")).unwrap();
    assert_success(&area.forklift("a", &["office", "regenesis", "--confirm"]));
    assert_success(&area.forklift("a", &["lift"]));

    assert_success(&area.forklift(".", &["franchise", &server.url, "c", "--token", "root"]));
    plant_torn_taint(&area, "c");
    let healed = area.forklift("c", &["heal"]);
    assert!(healed.status.success(),
        "heal must clear on a franchise of a remote first lifted after a re-genesis: {:?}\n{}",
        healed.status.code(), stderr(&healed));
}

/// FORK-262: a sparse franchise never fetched what lies outside its scope, so heal's torn-taint
/// rescan must not record those sealed objects as vanished. Signing is on, so the walk also
/// starts from the trust pins and the meta pallets.
#[test]
fn a_torn_taint_in_a_sparse_franchise_heals_over_its_sealed_objects() {
    let area = TestArea::new("sparse-torn-heal");
    let server = Server::start(&area, Some("root"));

    prepare_warehouse(&area, "dev", &server.url);
    assert_success(&area.forklift("dev", &["config", "remote.token", "root"]));
    area.write_file("dev/README", "readme\n");
    area.write_file("dev/src/a.txt", "a\n");
    area.write_file("dev/docs/guide.md", "guide\n");
    area.write_file("dev/docs/deep/notes.md", "notes\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["office", "enroll"]));
    assert_success(&area.forklift("dev", &["lift"]));

    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--token", "root", "--only", "src"]));
    plant_torn_taint(&area, "sparse");
    let healed = area.forklift("sparse", &["--json", "heal"]);
    assert!(healed.status.success(),
        "heal must clear over the sealed objects of a sparse franchise: {:?}\n{}\n{}",
        healed.status.code(), stdout(&healed), stderr(&healed));
    assert!(!area.path("sparse/.forklift/taint").exists()
        || std::fs::read_dir(area.path("sparse/.forklift/taint")).unwrap().next().is_none(),
        "the taint must be cleared, not replaced by a remainder");
}

/// FORK-114: the server refuses a first anchor whose `adopts` names history it does not hold —
/// it would be a pin it can never serve — and stores nothing.
#[test]
fn the_server_refuses_a_first_anchor_adopting_history_it_does_not_hold() {
    let area = TestArea::new("first-anchor-adopts");
    let server = Server::start(&area, None);

    let missing = "ab".repeat(32);
    let body = serde_json::json!({
        "genesis": "cd".repeat(32), "enabled_at": 0, "boundary": [], "adopts": missing,
    }).to_string();
    let (status, response) = http_send_json(&server.url, "PUT", "/v1/trust", &body);
    assert!(status.contains("422"), "{} {}", status, response);
    assert!(response.contains(&missing), "the refusal must name the pin: {}", response);

    // A held head parcel is not held history: plant only the office head of a real chain, and
    // the parcel's tree and records are still missing.
    prepare_warehouse(&area, "w", &server.url);
    assert_success(&area.forklift("w", &["office", "enroll"]));
    let office_head = std::fs::read_to_string(area.path("w/.forklift/meta/office")).unwrap();
    let office_head = office_head.trim();
    let relative = std::path::Path::new(".forklift/objects").join(&office_head[..2]).join(&office_head[2..]);
    let planted = area.path("server-root").join(&relative);
    std::fs::create_dir_all(planted.parent().unwrap()).unwrap();
    std::fs::copy(area.path("w").join(&relative), &planted).unwrap();
    let body = serde_json::json!({
        "genesis": "cd".repeat(32), "enabled_at": 0, "boundary": [], "adopts": office_head,
    }).to_string();
    let (status, response) = http_send_json(&server.url, "PUT", "/v1/trust", &body);
    assert!(status.contains("422"), "{} {}", status, response);
    assert!(response.contains(office_head), "the refusal must name the pin: {}", response);

    assert_success(&area.forklift(".", &["franchise", &server.url, "d"]));
}

/// Clients refuse a malformed anchor at the handshake, so the server must never store one: trust
/// is a one-way door, and a stored malformed anchor would lock every client out for good.
#[test]
fn the_server_refuses_to_store_a_malformed_trust_anchor() {
    let area = TestArea::new("malformed-anchor");
    let server = Server::start(&area, None);

    let hash = "ab".repeat(32);
    for genesis in ["x".to_string(), hash.to_uppercase(), format!("{}\u{1b}", &hash[..63])] {
        let body = serde_json::json!({ "genesis": genesis, "enabled_at": 0, "boundary": [] }).to_string();
        let (status, response) = http_send_json(&server.url, "PUT", "/v1/trust", &body);
        assert!(status.contains("422"), "{:?}: {} {}", genesis, status, response);
    }

    // Nothing was stored: the handshake still declares no anchor, so a franchise works.
    let (_, info) = http_get(&server.url, "/v1/warehouse");
    let info: serde_json::Value = serde_json::from_str(&info).unwrap();
    assert!(info["trust"].is_null(), "{}", info);
}

#[test]
fn an_expect_pin_is_checked_before_anything_is_fetched_and_first_contact_is_announced() {
    let area = TestArea::new("expect-pin");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/code.txt", "v1\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "base"]));
    assert_success(&area.forklift("a", &["office", "enroll"]));
    assert_success(&area.forklift("a", &["lift"]));

    // A wrong pin refuses before the target exists: nothing downloaded, nothing created.
    let wrong = area.forklift(".", &["--json", "franchise", &server.url, "b", "--expect", "not-it"]);
    let details = trust_changed_details(&wrong);
    assert_eq!(details["what_changed"], "unexpected");
    assert_eq!(details["subject"], server.url.as_str());
    assert!(details.get("previous").is_none(), "a franchise holds no anchor yet: {}", details);
    assert!(!area.path("b").exists(), "a refused pin must not create the target");
    let genesis = details["current"].as_str().unwrap().to_string();

    // The pin is checked first: even a pallet that does not exist refuses as an unexpected
    // anchor, not as a missing pallet — so nothing resolved from it was ever fetched.
    let first = area.forklift(".", &["--json", "franchise", &server.url, "b", "--pallet", "nope", "--expect", "not-it"]);
    assert_eq!(trust_changed_details(&first)["what_changed"], "unexpected");

    // The right pin franchises, and the first contact is still announced — on stderr, as a
    // warning document — and recorded in the report.
    let pinned = area.forklift(".", &["--json", "franchise", &server.url, "b", "--expect", &genesis]);
    assert_success(&pinned);
    let report: serde_json::Value = serde_json::from_str(&stdout(&pinned)).unwrap();
    assert_eq!(report["data"]["adopted_genesis"], genesis.as_str(), "{}", report);
    let warning: serde_json::Value = serde_json::from_str(stderr(&pinned).lines()
        .find(|line| line.contains("trust_adopted")).expect("a trust_adopted warning")).unwrap();
    assert_eq!(warning["details"], serde_json::json!({
        "subject": server.url, "current": genesis, "what_changed": "first_contact",
    }));

    // Lower checks the pin against the held anchor too, and a held anchor is not re-announced.
    let wrong = area.forklift("b", &["--json", "lower", "--expect", "not-it"]);
    let details = trust_changed_details(&wrong);
    assert_eq!(details["current"], genesis.as_str());
    assert_eq!(details["previous"], genesis.as_str());

    let lowered = area.forklift("b", &["lower", "--expect", &genesis]);
    assert_success(&lowered);
    assert!(!stderr(&lowered).contains("FIRST CONTACT"), "{}", stderr(&lowered));
}

/// A remote that still holds an office but no longer declares an anchor had it stripped: an
/// honest server refuses an office lift before an anchor exists and never removes one. Syncing
/// from it would take its history unverified, so both directions refuse before any download,
/// whether or not this warehouse holds an anchor.
#[test]
fn a_remote_whose_anchor_was_stripped_is_refused_before_anything_is_fetched() {
    let area = TestArea::new("stripped-anchor");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/code.txt", "v1\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "base"]));
    assert_success(&area.forklift("a", &["office", "enroll"]));
    assert_success(&area.forklift("a", &["lift"]));
    let genesis = std::fs::read_to_string(area.path("a/.forklift/trust")).unwrap();
    assert!(!genesis.is_empty());

    // Strip the anchor from the remote's storage; the office pallet stays.
    let stripped = area.path("server-root/.forklift/trust");
    assert!(stripped.is_file(), "the server keeps its anchor at {}", stripped.display());
    std::fs::remove_file(&stripped).unwrap();

    let lowered = area.forklift("a", &["--json", "lower"]);
    let details = trust_changed_details(&lowered);
    assert_eq!(details["what_changed"], "removed", "{}", details);
    assert_eq!(details["current"], serde_json::Value::Null);
    assert!(details["previous"].is_string(), "the held genesis is reported: {}", details);

    let franchised = area.forklift(".", &["--json", "franchise", &server.url, "b"]);
    let details = trust_changed_details(&franchised);
    assert_eq!(details["what_changed"], "removed", "{}", details);
    assert!(details.get("previous").is_none(), "a franchise holds no anchor yet: {}", details);
    assert!(!area.path("b").exists(), "a refused franchise must not create the target");

    // Lift refuses the same way rather than re-upload an anchor over the stripped office.
    let lifted = area.forklift("a", &["--json", "lift"]);
    assert_eq!(trust_changed_details(&lifted)["what_changed"], "removed");

    // And the server never stores an anchor over an office it holds without one, whoever sends
    // it: the first client to arrive would otherwise plant its own over a chain it never checked.
    let foreign = serde_json::json!({ "genesis": "cd".repeat(32), "enabled_at": 0, "boundary": [] });
    let (status, response) = http_send_json(&server.url, "PUT", "/v1/trust", &foreign.to_string());
    assert!(status.contains("409"), "{} {}", status, response);
    assert!(!stripped.exists(), "nothing was stored");
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

// A pallet deleted before the revocation is deliberately left outside the distrust boundary: a
// deletion is how a compromised key's work is taken down, and vouching for what a tombstone holds
// would let a restore bring it back past the audit. A restore of such work stays refused.
// Revert: take the remote's tombstone heads into the boundary → the restore passes the audit.
#[test]
fn a_revocation_does_not_vouch_for_work_the_remote_keeps_under_a_tombstone() {
    let area = TestArea::new("revoke-tombstone");

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

    // The worker lifts a side pallet, which is then deleted on the remote: only its tombstone
    // holds the worker's parcel there.
    assert_success(&area.forklift(".", &["franchise", &server.url, "b", "--token", "w-token"]));
    assert_success(&area.forklift("b", &["config", "operator.identifier", "worker@forklift"]));
    assert_success(&area.forklift("b", &["palletize", "feat"]));
    area.write_file("b/code.txt", "v2 by worker\n");
    assert_success(&area.forklift("b", &["load", "code.txt"]));
    assert_success(&area.forklift("b", &["stack", "side work"]));
    assert_success(&area.forklift("b", &["lift"]));
    assert_success(&area.forklift("a", &["palletize", "--delete", "feat", "--remote"]));

    // The admin revokes the worker's key: the boundary is the remote's live heads, not the tombstone.
    assert_success(&area.forklift("a", &["lower"]));
    let list = stdout(&area.forklift("a", &["office", "list"]));
    let worker_key_id = list.lines()
        .rev()
        .find(|line| line.trim_start().starts_with("key "))
        .and_then(|line| line.split_whitespace().nth(1))
        .expect("office list shows the worker's key")
        .to_string();
    assert_success(&area.forklift("a", &["office", "retire", &worker_key_id, "--compromised"]));
    assert_success(&area.forklift("a", &["lift"]));

    let restored = area.forklift("a", &["palletize", "--restore", "feat", "--remote"]);
    assert!(!restored.status.success(), "the deleted work is not vouched for: {}", stdout(&restored));
    assert!(stderr(&restored).contains("distrust boundary"), "{}", stderr(&restored));
    assert!(!area.path("server-root/.forklift/pallets/feat").exists(), "the pallet stays deleted");
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

    let prepared = server_command()
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
    let refused = server_command()
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
    let output = server_command()
        .args(["gc", "--root", &root_str])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("deleted 0"),
            "{}", String::from_utf8_lossy(&output.stdout));
    assert!(orphan.exists());

    // ...with the grace period at zero it is swept, and live history survives.
    let output = server_command()
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
fn gc_is_refused_while_another_process_writes_the_store() {
    // FORK-150: gc's grace period protects objects written recently, but a writer that dedups
    // against an old unreachable object writes nothing. So gc takes the store exclusively before
    // it computes the live set, and is refused while any writer holds it — here a CLI command in
    // another bay, stood in for by the test process's shared hold on an unserved root.
    //
    // Revert: drop the store-access acquire from `collect_garbage` → the sweep deletes the
    // orphan → red.
    let area = TestArea::new("gc-store-access");
    std::fs::create_dir_all(area.path("w")).unwrap();
    assert_success(&area.forklift("w", &["prepare"]));

    let orphan = area.path("w/.forklift/objects/aa/orphan-object");
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, b"junk").unwrap();
    let root_str = area.path("w").to_str().unwrap().to_string();

    let writer = hold_store_access(&area.path("w"), false);
    let refused = server_command()
        .args(["gc", "--root", &root_str, "--grace-hours", "0"])
        .output()
        .unwrap();
    assert!(!refused.status.success(), "gc must be refused while another process writes the store");
    assert!(stderr(&refused).contains("using the object store"),
        "the refusal should say the store is in use: {}", stderr(&refused));
    assert!(orphan.exists(), "a refused gc must not have swept anything");

    drop(writer);
    let swept = server_command()
        .args(["gc", "--root", &root_str, "--grace-hours", "0"])
        .output()
        .unwrap();
    assert!(swept.status.success(), "{}", stderr(&swept));
    assert!(stdout(&swept).contains("deleted 1"), "{}", stdout(&swept));
    assert!(!orphan.exists(), "with no writer, gc sweeps the orphan");
}

#[test]
fn a_warehouse_created_by_put_is_served_under_the_serve_lock() {
    // A warehouse first reached through `PUT /warehouses/W` must be served like any other: under
    // the serve lock and the shared store-access hold, so gc, a full repack and scope-prune are
    // refused on it while the server runs. `put_warehouse` used to register the lockless handle it
    // ran `prepare` with, and every later request reused it.
    //
    // Revert: `put_warehouse` registers its transient handle again → gc runs on the served
    // warehouse and sweeps the orphan → red.
    let area = TestArea::new("put-serve-lock");
    let server = Server::start_multi(&area, Some("admin"));

    assert_eq!(http_status(&server.url, "PUT", "/warehouses/w", Some("admin")), "HTTP/1.1 201 Created");
    assert_eq!(http_status(&server.url, "GET", "/warehouses/w/v1/warehouse", Some("admin")), "HTTP/1.1 200 OK");
    // A re-PUT of a warehouse being served stays idempotent.
    assert_eq!(http_status(&server.url, "PUT", "/warehouses/w", Some("admin")), "HTTP/1.1 200 OK");

    let root = area.path("warehouses-base/w");
    let orphan = root.join(".forklift/objects/aa/orphan-object");
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, b"junk").unwrap();

    let refused = server_command()
        .args(["gc", "--root", root.to_str().unwrap(), "--grace-hours", "0"])
        .output()
        .unwrap();
    assert!(!refused.status.success(), "gc must be refused on a warehouse the server serves");
    assert!(orphan.exists(), "a refused gc must not have swept anything");
    assert_eq!(http_status(&server.url, "GET", "/warehouses/w/v1/warehouse", Some("admin")), "HTTP/1.1 200 OK",
        "the warehouse is still served");
}

#[test]
fn a_second_server_and_gc_are_refused_while_serving_but_bundle_is_allowed() {
    let area = TestArea::new("serve-lock");
    let server = Server::start(&area, None);
    let root_str = area.path("server-root").to_str().unwrap().to_string();

    // A second server on the same root is refused up front (one serving process per root). The
    // acquire happens before the serve loop, so this fails fast rather than blocking. --open:
    // this is a raw `Command`, not `Server::spawn`, so it needs its own explicit opt-out of
    // authentication to reach the serve-lock refusal this test is actually about, rather than the
    // auth-config refusal (checked even earlier).
    let second = server_command()
        .args(["serve", "--root", &root_str, "--addr", "127.0.0.1:0", "--open"])
        .output()
        .unwrap();
    assert!(!second.status.success(),
            "a second server on the same root must be refused");
    assert!(String::from_utf8_lossy(&second.stderr).contains("locked by another forklift process"),
            "second-server refusal should name the serve lock: {}",
            String::from_utf8_lossy(&second.stderr));

    // gc is likewise refused while serving.
    let gc = server_command()
        .args(["gc", "--root", &root_str])
        .output()
        .unwrap();
    assert!(!gc.status.success(), "gc must be refused while serving");

    // bundle, by contrast, is deliberately *allowed* against a live server: it never deletes an
    // object, writes atomically, and a stale bundle is self-healing.
    let bundle = server_command()
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
                    "/resolve" => ("200 OK", r#"{"names":{"tester@forklift":"Remote Tester","someone-else":"Someone Else"}}"#),
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

    let prepared = server_command()
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

/// FORK-304: a pallet name is the rest of the path, so a tail that is no valid name is refused
/// with a 422 before the admission hook is asked, on the ref update and the delete alike; a valid
/// slash-named tail does reach the hook, whole.
#[test]
fn a_tail_naming_no_valid_pallet_never_reaches_the_admission_hook() {
    let area = TestArea::new("hooks-bad-tail");
    let hooks = HookServer::start();
    let server = start_hooked_server(&area, &hooks, "a shared hook secret");
    let token = Some("hook-token");
    let hash = "b".repeat(64);
    let update = format!("{{\"old_head\":null,\"new_head\":\"{}\"}}", hash);

    let admitted = |action: &str| -> Vec<String> {
        hooks.recorded().into_iter()
            .filter(|request| request.path == "/admission")
            .map(|request| String::from_utf8_lossy(&request.body).to_string())
            .filter(|body| body.contains(&format!("\"action\":\"{}\"", action)))
            .collect()
    };

    // The control: a valid name reaches the hook on both routes, so the silence below is the
    // parse's doing, not a hook that is never asked.
    let (status, body) = http_send_json_as(&server.url, "POST", "/v1/pallets/feature/x", &update, token);
    assert!(!status.starts_with("HTTP/1.1 404"), "{} / {}", status, body);
    let status = http_status(&server.url, "DELETE", &format!("/v1/pallets/feature/x?old_head={}", hash), token);
    assert!(!status.starts_with("HTTP/1.1 404"), "{}", status);
    let updates = admitted("ref_update");
    assert!(updates.len() == 1 && updates[0].contains("\"pallet\":\"feature/x\""), "{:?}", updates);
    let deletes = admitted("ref_delete");
    assert!(deletes.len() == 1 && deletes[0].contains("\"pallet\":\"feature/x\""), "{:?}", deletes);

    for tail in ["feature/..", "..%2Fmain", "feature/./x", "feature/X", "feature//x", "feature/x/", "/main"] {
        let (status, body) =
            http_send_json_as(&server.url, "POST", &format!("/v1/pallets/{}", tail), &update, token);
        assert!(status.starts_with("HTTP/1.1 422"), "update {}: {} / {}", tail, status, body);
        let status = http_status(&server.url, "DELETE", &format!("/v1/pallets/{}?old_head={}", tail, hash), token);
        assert!(status.starts_with("HTTP/1.1 422"), "delete {}: {}", tail, status);
        let (status, body) = http_send_json_as(&server.url, "POST", &format!("/v1/holds/{}", tail),
            &format!("{{\"candidates\":[\"{}\"]}}", hash), token);
        assert!(status.starts_with("HTTP/1.1 422"), "holds {}: {} / {}", tail, status, body);
    }
    assert_eq!(admitted("ref_update").len(), 1, "a refused name was put to the hook: {:?}", admitted("ref_update"));
    assert_eq!(admitted("ref_delete").len(), 1, "a refused name was put to the hook: {:?}", admitted("ref_delete"));

    for (method, path) in [("POST", "/v1/pallets/"), ("POST", "/v1/holds/")] {
        let (status, body) = http_send_json_as(&server.url, method, path, &update, token);
        assert!(status.starts_with("HTTP/1.1 404"), "{} {}: {} / {}", method, path, status, body);
    }
    let status = http_status(&server.url, "DELETE", &format!("/v1/pallets/?old_head={}", hash), token);
    assert!(status.starts_with("HTTP/1.1 404"), "{}", status);
}

#[test]
fn an_unreachable_authentication_hook_fails_closed() {
    let area = TestArea::new("hooks-closed");

    let root = area.path("server-root");
    let prepared = server_command()
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
    let prepared = server_command()
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

    // The hook's answer cap scales with the identifiers asked for, so the server bounds that
    // count — distinct identifiers, since a repeated one is asked about once.
    let many: Vec<String> = (0..=forklift_core::model::remote::MAX_RESOLVE_IDENTIFIERS)
        .map(|i| format!("op-{}", i)).collect();
    let (status, body) = http_post_json(
        &server.url, "/v1/resolve", &serde_json::json!({ "identifiers": many }).to_string()
    );
    assert!(status.contains("422"), "{} {}", status, body);
    assert!(body.contains(&format!("names {} distinct identifiers", forklift_core::model::remote::MAX_RESOLVE_IDENTIFIERS + 1)), "{}", body);

    let repeated = vec!["tester@forklift"; 20_000];
    let (status, body) = http_post_json(
        &server.url, "/v1/resolve", &serde_json::json!({ "identifiers": repeated }).to_string()
    );
    assert!(status.contains("200"), "a repeated identifier counts once: {} {}", status, body);
    assert!(body.contains("Remote Tester"), "{}", body);

    // This directory names tester@forklift whatever it is asked; a request that did not ask about
    // that identifier gets the name it did ask about (so the answer was read) and not that one.
    let (status, body) = http_post_json(
        &server.url, "/v1/resolve", &serde_json::json!({ "identifiers": ["someone-else"] }).to_string()
    );
    assert!(status.contains("200"), "{} {}", status, body);
    assert!(body.contains("Someone Else"), "the directory's answer was read: {}", body);
    assert!(!body.contains("Remote Tester"), "a name nobody asked about reaches the client: {}", body);

    // Resolution never fails a command: point the remote at a dead port and history
    // degrades to the pseudonymous id instead of erroring.
    assert_success(&area.forklift("a", &["config", "remote.url", "http://127.0.0.1:9"]));
    let history = area.forklift("a", &["history"]);
    assert_success(&history);
    assert!(!stdout(&history).contains("Remote Tester"), "{}", stdout(&history));
    assert!(stdout(&history).contains("tester@forklift"), "{}", stdout(&history));
}

/// The resolve request limit is part of the request's contract, so a server with no directory
/// configured enforces it too: a client cannot pass against one server and fail against another.
#[test]
fn a_server_without_a_resolution_hook_still_bounds_resolve_identifiers() {
    let area = TestArea::new("resolve-limit-no-hook");
    let server = Server::start(&area, None);

    let many: Vec<String> = (0..=forklift_core::model::remote::MAX_RESOLVE_IDENTIFIERS)
        .map(|i| format!("op-{}", i)).collect();
    let (status, body) = http_post_json(
        &server.url, "/v1/resolve", &serde_json::json!({ "identifiers": many }).to_string()
    );
    assert!(status.contains("422"), "{} {}", status, body);

    let (status, body) = http_post_json(
        &server.url, "/v1/resolve", &serde_json::json!({ "identifiers": ["op-1"] }).to_string()
    );
    assert!(status.contains("200") && body.contains("\"names\":{}"), "{} {}", status, body);
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
            .block_on(forklift_core::util::remote_utils::fetch_history(&client, head, &Progress::NONE))
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
fn a_partial_clone_missing_an_unrelated_boundary_head_still_reads_signer_boundary_as_vouched() {
    // The presence guard, end to end: a distrust boundary can name a head this store never has
    // at all, and that absent head must never turn a parcel a *present* boundary head already
    // vouches for into "unresolved" — let alone "suspect" — even though `audit_utils`'s own
    // gap-tolerance is any-gap, not narrowed to interior gaps (PR #121 round 1 tried narrowing
    // to interior-only and PR #121 round 2 reverted it — see `audit_utils::verify_pallet_
    // history`'s `boundary_gap` doc: an absent head can genuinely be the thing that would have
    // vouched for some OTHER parcel, so the walk-wide gap flag has to stay any-gap).
    //
    // What actually makes THIS parcel read "vouched" rather than "unresolved" is
    // `QueryContext::fill_boundary` asking `vouched` before `resolvable` (see that method's
    // doc): the agent's main-line parcel is vouched by `main`'s own present boundary head
    // directly, so `vouched` answers `true` regardless of the unrelated, absent `side`-pallet
    // head elsewhere in the same boundary — `resolvable` is never even consulted for this
    // parcel. This was `a_partial_clone_missing_a_boundary_head_reads_signer_boundary_as_
    // unresolved`, which pinned the PRE-REORDER behaviour: `fill_boundary` used to ask
    // `resolvable` unconditionally, so a single absent head anywhere in the boundary — however
    // irrelevant to this parcel — made every parcel under that key read "unresolved". The
    // fixture is unchanged; only the assertions invert.
    //
    // Construction note (a deviation from the original sketch, which tried to manufacture
    // this by un-stacking a side-pallet parcel with `undo` *after* retiring past it): a
    // pallet that is simply never lifted is simpler, and arguably the more realistic shape
    // besides, since `office retire` snapshots *every* current pallet head regardless of
    // what's ever reached the remote: a purely local, never-pushed pallet's head lands in a
    // distrust boundary exactly the same as a published one's.
    //
    // So: a second local-only pallet ("side") supplies the boundary head that a plain
    // (non-sparse) franchise of `main` alone will never even hear about, let alone fetch —
    // it is flatly absent on the clone, not merely "not reachable from here". That absence
    // is irrelevant to the agent's main-line parcel, which `main`'s own present boundary
    // head vouches for regardless — on both the origin and the clone.
    let area = TestArea::new("boundary-vouched-past-an-absent-head");
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
        clone_entry["signer"]["boundary"], "vouched",
        "the orphaned side-pallet boundary head is absent but irrelevant: `main`'s own \
         present boundary head already vouches for the agent parcel, so an absent, unrelated \
         HEAD must not read as unresolved (let alone suspect): {}",
        clone_report
    );

    // A `signer.boundary eq suspect` predicate must not match, and `eq vouched` must (the
    // predicate only ever compares against "vouched" or "suspect" — "unresolved" is not a
    // valid comparison value at all, since an unresolved boundary reads Unknown for this leaf
    // and never matches either literal). This test no longer reaches that path at all: after
    // the vouched-first reorder BOTH the origin and the clone read "vouched" here, which is
    // the point of the fixture. The Unknown arm is covered by the distrust construction
    // further down, whose parcel is genuinely unvouched behind an absent boundary head.
    for (value, must_match) in [("suspect", false), ("vouched", true)] {
        let where_clause = format!(
            r#"{{"field":"signer.boundary","op":"eq","value":"{}"}}"#,
            value
        );
        let report = area.forklift("clone", &["--json", "query", "main", "--where", &where_clause]);
        assert_success(&report);
        let report: serde_json::Value = serde_json::from_str(&stdout(&report)).unwrap();
        let matches: std::collections::HashSet<String> = report["data"]["matches"]
            .as_array().unwrap().iter()
            .map(|entry| entry["parcel"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            matches.contains(&agent_parcel), must_match,
            "signer.boundary eq {}: expected match = {}: {}",
            value, must_match, report
        );
    }
}

#[test]
fn a_partial_clone_missing_an_unrelated_boundary_head_still_audits_the_vouched_parcel() {
    // `audit` must not refuse just because *some* boundary head is missing — only when the
    // parcel under test cannot be vouched *without* it. `audit`'s own asymmetric, vouched-first
    // ordering (below, `verify_pallet_history`'s `DistrustBoundary` arm) means it never even
    // asks whether the boundary is resolvable in the common case: `vouched` decides it directly.
    // `query`'s `fill_boundary` now asks in the same order (see that method's doc) — before
    // this fix it asked `resolvable` unconditionally, which made the *whole boundary's*
    // resolvability (not just what this parcel needs) the gate, over-firing "unresolved" for
    // parcels an absent, unrelated head had nothing to do with; see
    // `a_partial_clone_missing_an_unrelated_boundary_head_still_reads_signer_boundary_as_vouched`,
    // which shares this fixture and pins that fix directly. This construction reuses that same
    // partial-clone fixture (a never-lifted local "side" pallet's head sits in the boundary
    // alongside main's own) to pin the `audit`-side analogue: the agent's main-line parcel is
    // vouched by main's own boundary snapshot alone, so `audit` must pass on the clone even
    // though `side`'s head never arrived there — the missing head is irrelevant to this parcel,
    // and an audit that refused anyway would be a regression.
    //
    // The complementary case — a boundary that genuinely *cannot* be resolved for the parcel
    // under test, where `audit` must refuse honestly rather than allege tampering — is exercised
    // both directly against `verify_pallet_history` in `audit_utils::tests` (a hand-built
    // `KeyRecord` and object store, chosen there because a shipped `office retire` always folds
    // the audited pallet's OWN current head into the boundary, so that particular entry is
    // trivially present through ordinary tooling) and, end to end through real CLI/server
    // machinery, by `retiring_a_signer_then_lifting_only_a_replacement_pallet_refuses_instead_
    // of_accusing` below (Construction D): there, the audited parcel's own pallet is never
    // lifted at all, so even its own retirement-time boundary entry is absent on the server —
    // the "same pallet's own snapshot is always present" shape this fixture relies on is a
    // property of THIS construction (the audited pallet, main, is the one that keeps getting
    // lifted), not a universal guarantee; a pallet that stops being the one advanced and lifted
    // can lose even its own boundary entry.
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

/// Construction T (PR #121 round 2): the trust-arm honest refusal, reproduced end to end
/// against the ORIGIN SERVER from two entirely ordinary commands — `office enroll`,
/// `palletize`, `lift` — never a hand-built fixture and never a deleted object. This is the
/// direct falsifier of PR #121 round 1's interior-only narrowing: under that narrowing this
/// exact, unremarkable sequence made the origin ACCUSE a genuinely pre-trust, legacy parcel of
/// tampering, and the defect was not confined to clones or the AWS Lambda head — it fired here
/// from the origin server itself, round-tripped through two ordinary commands.
///
/// `w` stacks three unsigned parcels on `main` (U1 <- U2 <- U3, no office yet), enrolls with no
/// remote configured (`office enroll`'s boundary is then purely local: exactly `[U3]`, per
/// `commands/office.rs`'s `enroll`), then `palletize`s "feature" at U1 — an OLDER ancestor of
/// `main`'s current head, not `main`'s head itself. Only now is a fresh remote configured, and
/// `lift` (current pallet: "feature") uploads U1's own closure, which does NOT include U2 or U3
/// (they are U1's descendants, not its ancestors).
///
/// `push_local_trust` (`remote_utils.rs`) sends only the anchor DTO; the server's `put_trust`
/// (`forklift-server/src/server.rs`) writes it with no presence check on any boundary head. So
/// the server ends up holding `anchor.boundary = [U3]` with U3 itself absent — a HEAD gap. When
/// the "feature" ref update is then audited server-side
/// (`verify_pallet_history(U1, anchor, office_state, None)`), U1 is unsigned (pre-office) and
/// the boundary walk cannot even start (its one entry, U3, is absent) — under any-gap semantics
/// the server refuses honestly, naming U3; under the reverted interior-only narrowing it would
/// accuse U1 of tampering instead, because a head-only gap never set `boundary_gap` there.
///
/// Resolution: lifting `main` too (its own closure includes U1, U2 and U3) puts U3 on the
/// server. Retrying the very same `feature` lift then succeeds — the identical command that
/// just refused — and a fresh clone franchised from the server afterwards audits `main` as
/// fully legacy: the definitive, resolved answer.
#[test]
fn enrolling_then_lifting_to_a_fresh_remote_with_an_unlifted_other_pallet_refuses_instead_of_accusing() {
    let area = TestArea::new("construction-t");
    let server = Server::start(&area, None);

    // `w` is prepared with NO remote configured yet — `office enroll` below must see no
    // remote and skip the remote-heads union entirely, so the boundary is exactly `[U3]`.
    std::fs::create_dir_all(area.path("w")).unwrap();
    assert_success(&area.forklift("w", &["prepare"]));
    assert_success(&area.forklift("w", &["config", "--global", "operator.identifier", "tester@forklift"]));

    // U1, U2, U3: three unsigned, pre-office stacks on `main`.
    area.write_file("w/app.txt", "v1\n");
    assert_success(&area.forklift("w", &["load", "."]));
    assert_success(&area.forklift("w", &["stack", "U1"]));
    let u1 = pallet_head(&area, "w", "main");

    area.write_file("w/app.txt", "v2\n");
    assert_success(&area.forklift("w", &["load", "."]));
    assert_success(&area.forklift("w", &["stack", "U2"]));

    area.write_file("w/app.txt", "v3\n");
    assert_success(&area.forklift("w", &["load", "."]));
    assert_success(&area.forklift("w", &["stack", "U3"]));
    let u3 = pallet_head(&area, "w", "main");

    assert_success(&area.forklift("w", &["office", "enroll"]));
    let anchor_boundary = area.read_file("w/.forklift/trust");
    assert!(
        anchor_boundary.contains(&format!("boundary = [\"{}\"]", u3)),
        "the fixture requires the boundary to be exactly [U3]: {}",
        anchor_boundary
    );

    // "feature" starts at U1 — an ancestor of `main`'s current head, not the head itself — so
    // lifting it never touches U2 or U3.
    assert_success(&area.forklift("w", &["palletize", "feature", &u1]));

    // The remote is configured only now, AFTER enroll: it plays no part in the boundary.
    assert_success(&area.forklift("w", &["config", "remote.url", &server.url]));

    let first_lift = area.forklift("w", &["lift"]);
    let out = format!("{}{}", stdout(&first_lift), stderr(&first_lift));

    println!("CONSTRUCTION T, first lift: exit = {:?}\n{}", first_lift.status.code(), out.trim());

    assert!(
        !first_lift.status.success(),
        "the origin server must refuse: it cannot resolve the trust boundary for U1. output: {}",
        out
    );
    assert!(
        !out.to_lowercase().contains("tampered"),
        "no tampering accusation is expected — the server never proved U1 was stacked after \
        trust, it simply cannot resolve the boundary. output: {}",
        out
    );
    assert!(out.contains(&u1), "names the parcel under audit {}: {}", u1, out);
    assert!(out.contains(&u3), "names the missing boundary parcel {}: {}", u3, out);

    // Resolution: lift `main` too, so U3 (and U2) reach the server.
    assert_success(&area.forklift("w", &["shift", "main"]));
    assert_success(&area.forklift("w", &["lift"]));

    // The identical command that just refused now succeeds.
    assert_success(&area.forklift("w", &["shift", "feature"]));
    let second_lift = area.forklift("w", &["lift"]);
    assert!(
        second_lift.status.success(),
        "with U3 now on the server, the same lift must succeed: {}",
        stderr(&second_lift)
    );

    // The definitive, resolved answer: a fresh clone franchised from the server now audits
    // `main` as fully legacy.
    let franchised = area.forklift(".", &["franchise", &server.url, "clone"]);
    assert_success(&franchised);
    let audit_on_clone = area.forklift("clone", &["audit"]);
    assert_success(&audit_on_clone);
    assert!(
        stdout(&audit_on_clone).contains("3 legacy parcel(s) predate trust and are unsigned"),
        "{}",
        stdout(&audit_on_clone)
    );
}

/// Construction D (PR #121 round 2) — the distrust-arm twin of Construction T above, and a
/// REGRESSION rather than a fresh defect: `main`'s own `unresolved_head` already returned the
/// first of ALL absent references (confirmed by reading
/// `git show main:crates/forklift-core/src/util/audit_utils.rs`), so this exact sequence was
/// already handled soundly before PR #121 round 1 narrowed it to interior-only gaps.
///
/// `w` is trusted, with admin `admin@forklift` and agent key `A`. `side` is palletized from
/// `main`'s current head (call it M1); as `A`, two parcels are stacked on `side`: A0, then A1
/// (`side`'s new head). Back on `main` (still at M1, untouched), the admin retires `A`:
/// `revocation_boundary` (`commands/office.rs`) snapshots every LOCAL pallet head at that
/// moment — `main`'s M1 and `side`'s A1 — so `A.distrust_boundary = [M1, A1]`. A THIRD pallet,
/// "tmp", is then palletized at A0 (an ancestor of A1, not A1 itself) and lifted: the office
/// parcel and "tmp" (closure of A0) reach the server, but `side` — and so A1 — never do.
///
/// M1 being present is irrelevant to A0: reachability only ever follows `.parents` (backward),
/// and A0 is a DESCENDANT of M1 (on a branch M1 never leads to), not an ancestor, so a present
/// M1 can never vouch for it either way. A1 is the entry that would matter — A0 is its direct
/// parent — and it is exactly the one absent, a HEAD gap the walk cannot see past. Server-side,
/// `verify_pallet_history(A0, anchor, office_state, None)` classifies A0 as
/// `Verdict::DistrustBoundary(A)`; `vouched` reads `false` (M1's own ancestry never reaches A0);
/// under any-gap semantics `unresolved_head` names the absent A1 and the server refuses
/// honestly; under the reverted interior-only narrowing, A1's absence is a head-only gap that
/// never lands in `interior_gaps`, so `unresolved_head` reports `None` and the server accuses
/// A0 of tampering instead — a genuinely pre-revocation parcel branded a forged backdate.
///
/// Resolution: lifting `side` (bringing A1, and so A0's presence via A1's own ancestry, fully
/// onto the server) and retrying the identical `tmp` lift succeeds. A clone franchised scoped
/// to "tmp" alone fetches A1 as the boundary its take's verdict needs; with A1 removed again it
/// reads A0 as `unresolved`, and fetching A1's history directly
/// into that same clone (`remote_utils::fetch_history`, the in-process idiom used elsewhere in
/// this file) flips the SAME query's answer to `vouched` — the resolved, definitive read.
#[test]
fn retiring_a_signer_then_lifting_only_a_replacement_pallet_refuses_instead_of_accusing() {
    let area = TestArea::new("construction-d");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "w", &server.url);
    assert_success(&area.forklift("w", &["config", "--global", "operator.identifier", "admin@forklift"]));

    area.write_file("w/base.txt", "base\n");
    assert_success(&area.forklift("w", &["load", "."]));
    assert_success(&area.forklift("w", &["stack", "base"]));
    assert_success(&area.forklift("w", &["office", "enroll"]));
    assert_success(&area.forklift("w", &["lift"]));
    let m1 = pallet_head(&area, "w", "main");

    // Admit the agent.
    assert_success(&area.forklift("w", &["config", "--global", "operator.identifier", "agent@forklift"]));
    let keygen = area.forklift("w", &["office", "keygen"]);
    assert_success(&keygen);
    let admit_args: Vec<String> = stdout(&keygen)
        .lines()
        .find(|line| line.trim_start().starts_with("office admit "))
        .expect("keygen prints the admit line")
        .split_whitespace()
        .skip(2)
        .map(str::to_string)
        .collect();
    assert_success(&area.forklift("w", &["config", "--global", "operator.identifier", "admin@forklift"]));
    assert_success(&area.forklift("w", &[
        "office", "admit", &admit_args[0], &admit_args[1], &admit_args[2],
        "--agent", "--supervisor", "admin@forklift",
    ]));

    // "side" starts at M1 (main's current head). As the agent, stack A0 then A1 on it.
    assert_success(&area.forklift("w", &["palletize", "side"]));
    assert_success(&area.forklift("w", &["config", "--global", "operator.identifier", "agent@forklift"]));
    area.write_file("w/side.txt", "a0\n");
    assert_success(&area.forklift("w", &["load", "."]));
    assert_success(&area.forklift("w", &["stack", "A0"]));
    let a0 = pallet_head(&area, "w", "side");

    area.write_file("w/side.txt", "a1\n");
    assert_success(&area.forklift("w", &["load", "."]));
    assert_success(&area.forklift("w", &["stack", "A1"]));
    let a1 = pallet_head(&area, "w", "side");

    // Back on `main` (still exactly M1 — untouched since it was lifted above), retire the
    // agent's key: the distrust boundary snapshots every LOCAL pallet head right now, main's
    // and side's alike.
    assert_success(&area.forklift("w", &["config", "--global", "operator.identifier", "admin@forklift"]));
    assert_success(&area.forklift("w", &["shift", "main"]));
    assert_eq!(pallet_head(&area, "w", "main"), m1, "main must be untouched since its own lift");

    let listing = area.forklift("w", &["--json", "office", "list"]);
    assert_success(&listing);
    let listing: serde_json::Value = serde_json::from_str(&stdout(&listing)).unwrap();
    let agent_key_id = listing["data"]["users"]
        .as_array().unwrap().iter()
        .find(|user| user["identifier"] == "agent@forklift")
        .expect("the agent is enrolled")
        ["keys"][0]["key_id"]
        .as_str().unwrap().to_string();
    assert_success(&area.forklift("w", &["office", "retire", &agent_key_id, "--offline"]));

    // "tmp" starts at A0 — `side`'s own earlier parcel, never its current head — and is lifted.
    // `side` itself is never lifted, so A1 never reaches the server.
    assert_success(&area.forklift("w", &["palletize", "tmp", &a0]));

    let first_lift = area.forklift("w", &["lift"]);
    let out = format!("{}{}", stdout(&first_lift), stderr(&first_lift));

    println!("CONSTRUCTION D, first lift: exit = {:?}\n{}", first_lift.status.code(), out.trim());

    assert!(
        !first_lift.status.success(),
        "the origin server must refuse: it cannot resolve the distrust boundary for A0. \
        output: {}",
        out
    );
    assert!(
        !out.to_lowercase().contains("tampered"),
        "no tampering accusation is expected — the server never proved A0 was signed after \
        the revocation, it simply cannot resolve the boundary. output: {}",
        out
    );
    assert!(out.contains(&a0), "names the parcel under audit {}: {}", a0, out);
    assert!(out.contains(&a1), "names the missing boundary parcel {}: {}", a1, out);

    // Resolution: lift `side` too, so A1 (and A0's presence via A1's own ancestry) reach the
    // server.
    assert_success(&area.forklift("w", &["shift", "side"]));
    assert_success(&area.forklift("w", &["lift"]));

    // The identical command that just refused now succeeds.
    assert_success(&area.forklift("w", &["shift", "tmp"]));
    let second_lift = area.forklift("w", &["lift"]);
    assert!(
        second_lift.status.success(),
        "with A1 now on the server, the same lift must succeed: {}",
        stderr(&second_lift)
    );

    // A clone scoped to "tmp" alone fetches A1 even so: verifying the take of A0 needs the
    // distrust boundary, and the take fetches what its verdict lacks. Removing A1 from the clone
    // reproduces the SAME unresolved read, pinning that it is genuinely about A1's absence, not
    // some artifact of the origin.
    let franchised = area.forklift(".", &["franchise", &server.url, "tmp-clone", "--pallet", "tmp"]);
    assert_success(&franchised);
    assert!(object_present(&area, "tmp-clone", &a1), "the franchise fetches the distrust boundary A1");
    std::fs::remove_file(object_store_path(&area, "tmp-clone", &a1)).unwrap();

    let query_before = area.forklift("tmp-clone", &["--json", "query", "tmp"]);
    assert_success(&query_before);
    let report: serde_json::Value = serde_json::from_str(&stdout(&query_before)).unwrap();
    let a0_entry = report["data"]["matches"].as_array().unwrap().iter()
        .find(|entry| entry["parcel"] == a0)
        .unwrap_or_else(|| panic!("no match for A0 on the tmp-only clone: {}", report));
    assert_eq!(
        a0_entry["signer"]["boundary"], "unresolved",
        "a clone that never fetched `side`'s A1 must read A0 as unresolved, not vouched: {}",
        report
    );

    // The dead branch this leg exists to cover: `Leaf::SignerBoundary`'s
    // `Some(Boundary::Unresolved) => Truth::Unknown` (`query_utils.rs`) — an `Unresolved`
    // parcel must match NEITHER literal a `--where` predicate can even ask for ("unresolved"
    // itself is refused at parse time, so this is the only way to reach that arm at all).
    for value in ["vouched", "suspect"] {
        let where_clause = format!(r#"{{"field":"signer.boundary","op":"eq","value":"{}"}}"#, value);
        let filtered = area.forklift("tmp-clone", &["--json", "query", "tmp", "--where", &where_clause]);
        assert_success(&filtered);
        let filtered: serde_json::Value = serde_json::from_str(&stdout(&filtered)).unwrap();
        let matched: std::collections::HashSet<String> = filtered["data"]["matches"]
            .as_array().unwrap().iter()
            .map(|entry| entry["parcel"].as_str().unwrap().to_string())
            .collect();
        assert!(
            !matched.contains(&a0),
            "an unresolved boundary must match neither \"eq {}\" (Unknown, not True): {}",
            value, filtered
        );
    }

    // Fetch A1's own ancestry directly into the SAME clone (the in-process idiom used
    // elsewhere in this file — `remote_utils::fetch_history`), without a second franchise or
    // a `side` pallet ref: this puts A1 (and so A0, already present) fully in reach and flips
    // the identical query's answer.
    {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _scope = forklift_core::globals::StorageRootScope::enter(&area.path("tmp-clone"));
        let client = forklift_core::util::remote_utils::RemoteClient::new(&server.url, None).unwrap();
        runtime
            .block_on(forklift_core::util::remote_utils::fetch_history(&client, &a1, &Progress::NONE))
            .expect("fetch A1's ancestry");
    }

    let query_after = area.forklift("tmp-clone", &["--json", "query", "tmp"]);
    assert_success(&query_after);
    let report: serde_json::Value = serde_json::from_str(&stdout(&query_after)).unwrap();
    let a0_entry = report["data"]["matches"].as_array().unwrap().iter()
        .find(|entry| entry["parcel"] == a0)
        .unwrap_or_else(|| panic!("no match for A0 after fetching A1: {}", report));
    assert_eq!(
        a0_entry["signer"]["boundary"], "vouched",
        "with A1's ancestry now present, the identical query must read A0 as vouched: {}",
        report
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

/// A sparse warehouse "sparse", scoped to `src/api`, whose `lower` was refused as diverged: the
/// remote head, lifted from a full clone "up", changes `src/web` too, so the refused lower fetched
/// it with `src/web` sealed. Returns that head and its `src/web/w.txt` blob.
fn sparse_after_a_diverged_lower(area: &TestArea, server: &Server) -> (String, String) {
    prepare_warehouse(area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/api"]));
    assert_success(&area.forklift(".", &["franchise", &server.url, "up"]));

    area.write_file("up/src/api/a.txt", "api v2 from up\n");
    area.write_file("up/src/web/w.txt", "web v2 from up\n");
    assert_success(&area.forklift("up", &["load", "."]));
    assert_success(&area.forklift("up", &["stack", "up edits both"]));
    assert_success(&area.forklift("up", &["lift"]));

    let up_head = pallet_head(area, "up", "main");
    let web_blob = path_object_hash(area, "up", &up_head, "src/web/w.txt");

    area.write_file("sparse/src/api/a.txt", "api v2 from sparse\n");
    assert_success(&area.forklift("sparse", &["load", "."]));
    assert_success(&area.forklift("sparse", &["stack", "sparse edits api"]));

    let lowered = area.forklift("sparse", &["lower"]);
    assert!(!lowered.status.success() && stderr(&lowered).contains("diverged"), "{}", stderr(&lowered));
    assert!(object_present(area, "sparse", &up_head), "the refused lower fetched the remote head");
    assert!(!object_present(area, "sparse", &web_blob), "with src/web sealed");

    (up_head, web_blob)
}

// Revert: expand skips the fetched-heads ledger → the remote head's src/web stays sealed, so the
// palletize makes a ref whose closure lacks it.
#[test]
fn expand_fetches_the_widened_content_of_a_head_a_refused_lower_fetched() {
    let area = TestArea::new("sparse-expand-ledger");
    let server = Server::start(&area, None);
    let (up_head, web_blob) = sparse_after_a_diverged_lower(&area, &server);

    assert_success(&area.forklift("sparse", &["expand", "src/web"]));
    assert_success(&area.forklift("sparse", &["palletize", &format!("main-remote-{}", &up_head[..12]), &up_head]));

    assert!(object_present(&area, "sparse", &web_blob), "the palletized head's widened content is present");
}

// Revert: let the ref compare-and-set skip the closure check → the palletize lands on a head
// whose src/web is missing.
#[test]
fn expand_leaves_a_fetched_head_the_remote_can_no_longer_serve_unpalletizable() {
    let area = TestArea::new("sparse-expand-unservable");
    let server = Server::start(&area, None);
    let (up_head, web_blob) = sparse_after_a_diverged_lower(&area, &server);

    std::fs::remove_file(object_store_path(&area, "server-root", &web_blob)).unwrap();

    let expanded = area.forklift("sparse", &["expand", "src/web"]);
    assert_success(&expanded);

    let refused = area.forklift("sparse", &["palletize", "x", &up_head]);
    assert!(!refused.status.success(), "an incomplete head is no new head: {}", stdout(&refused));
    assert!(stderr(&refused).contains("not completely present"), "{}", stderr(&refused));
    assert!(!area.path("sparse/.forklift/pallets/x").exists());
}

// Revert: walk every closure before checking the added paths → the unservable ref refuses the
// expand first, hiding the typo.
#[test]
fn expand_checks_its_paths_before_walking_other_histories() {
    let area = TestArea::new("sparse-expand-typo");
    let server = Server::start(&area, None);
    let (up_head, web_blob) = sparse_after_a_diverged_lower(&area, &server);

    assert_success(&area.forklift("sparse", &["palletize", "x", &up_head]));
    assert_success(&area.forklift("sparse", &["shift", "main"]));
    std::fs::remove_file(object_store_path(&area, "server-root", &web_blob)).unwrap();

    let refused = area.forklift("sparse", &["expand", "src/web", "src/wbe"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("\"src/wbe\" is not a directory"), "{}", stderr(&refused));
}

// Revert: franchise fetches through the scope on disk, which does not exist yet → the whole
// tree is fetched and nothing is recorded.
#[test]
fn a_sparse_franchise_records_its_head_and_fetches_only_its_scope() {
    let area = TestArea::new("sparse-franchise-ledger");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let web_blob = path_object_hash(&area, "dev", &head, "src/web/w.txt");

    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/api"]));

    assert!(!object_present(&area, "sparse", &web_blob), "src/web stays sealed");
    let ledger = area.read_file("sparse/.forklift/fetched-heads");
    assert!(ledger.contains(&format!("{} main", head)), "{}", ledger);
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
fn narrow_refuses_to_delete_an_edited_tracked_file_a_rule_covers() {
    // A rule covers only untracked paths, so an edit to a tracked file it matches is still
    // uncommitted work that narrow's unconditional delete would destroy.
    let area = TestArea::new("sparse-narrow-ignored-tracked");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/docs/gen.txt", "gen v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    assert_success(&area.forklift(".",
        &["franchise", &server.url, "sparse", "--only", "src/api", "--only", "docs"]));

    std::fs::write(area.path("sparse/.forkliftignore"), "^docs/gen\\.txt$\n").unwrap();
    std::fs::write(area.path("sparse/docs/gen.txt"), "gen v2 (unsaved)\n").unwrap();

    let refused = area.forklift("sparse", &["--json", "narrow", "docs"]);
    assert_eq!(refused.status.code(), Some(12), "narrow_unclean exits 12: {}", stderr(&refused));
    assert_eq!(area.read_file("sparse/docs/gen.txt"), "gen v2 (unsaved)\n",
        "the edit must survive a refused narrow");
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
fn a_sparse_audit_checks_each_path_of_a_shared_spine_tree() {
    // Which entries a spine walk descends depends on the tree's path, not its hash. With scope
    // {a/x, b/y} over identical directories `a` and `b`, the walk at `a` checks only `a/x`; the
    // walk at `b` must still run to check `b/y`, even though the same tree was already walked as
    // spine at `a`. A missing object under `b/y` must fail the audit.
    let area = TestArea::new("sparse-audit-shared-spine");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    for dir in ["a", "b"] {
        area.write_file(&format!("dev/{}/x/x.txt", dir), "x\n");
        area.write_file(&format!("dev/{}/y/y.txt", dir), "y\n");
    }
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "pre-trust base"]));
    assert_success(&area.forklift("dev", &["office", "enroll"]));
    area.write_file("dev/note.txt", "signed\n");
    assert_success(&area.forklift("dev", &["load", "note.txt"]));
    assert_success(&area.forklift("dev", &["stack", "signed parcel"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    assert_eq!(path_object_hash(&area, "dev", &head, "a"), path_object_hash(&area, "dev", &head, "b"),
        "the fixture needs `a` and `b` to be one tree object");
    let y_blob = path_object_hash(&area, "dev", &head, "b/y/y.txt");

    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "a/x", "--only", "b/y"]));
    assert_success(&area.forklift("sparse", &["audit"]));

    // The blob under `b/y` is the only object in scope at `b` and nowhere else.
    std::fs::remove_file(object_store_path(&area, "sparse", &y_blob)).unwrap();

    let audit = area.forklift("sparse", &["audit"]);
    assert!(!audit.status.success(),
        "a missing in-scope blob under the second path of a shared spine tree must fail the audit: {}",
        stdout(&audit));
    assert!(format!("{}{}", stdout(&audit), stderr(&audit)).contains(&y_blob),
        "the failure names the missing blob: {}{}", stdout(&audit), stderr(&audit));
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

    // The uncomputable filter stays unknown, and storing unknown over unknown changes nothing, so
    // asking again must not rewrite the shard: a shard is replaced by renaming a new file over
    // it, which gives it a new inode.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let shard = area.path("sparse/.forklift/graph").join(&sparse_head[..2]);
        let inode = std::fs::metadata(&shard).expect("the first query recorded the parcel").ino();
        assert_eq!(forklift_core::util::graph_utils::path_maybe_changed(&sparse_head, "src/api/a.txt"), Ok(true));
        assert_eq!(std::fs::metadata(&shard).unwrap().ino(), inode,
            "a filter that stays unknown must not rewrite its shard on every query");
    }
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

/// A remote holding `src/api/a.txt` and `docs/guide.md` (v1), lifted from "dev", and a sparse
/// franchise "sparse" fetching both paths. Returns the server, which must outlive the test.
fn sparse_with_docs(area: &TestArea) -> Server {
    let server = Server::start(area, None);

    prepare_warehouse(area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/docs/guide.md", "guide v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse",
        "--only", "src/api", "--only", "docs"]));

    server
}

#[test]
fn scope_prune_keeps_work_never_lifted_and_frees_it_once_lifted() {
    // FORK-251: a change stacked under the pruned path and never lifted exists only here. The
    // prune keeps it, and the parent tree its lift compares against, names the pallet, and says
    // how to free it. The parent's blob, which that lift never reads, is freed. Lifting then works, a second prune frees both sides, and expand
    // fetches the lifted change back.
    let area = TestArea::new("scope-prune-unlifted");
    let _server = sparse_with_docs(&area);

    let base = pallet_head(&area, "sparse", "main");
    let v1_tree = path_object_hash(&area, "sparse", &base, "docs");
    let v1_blob = path_object_hash(&area, "sparse", &base, "docs/guide.md");

    std::fs::write(area.path("sparse/docs/guide.md"), "guide v2, never lifted\n").unwrap();
    assert_success(&area.forklift("sparse", &["load", "."]));
    assert_success(&area.forklift("sparse", &["stack", "local docs edit"]));

    let head = pallet_head(&area, "sparse", "main");
    let v2_tree = path_object_hash(&area, "sparse", &head, "docs");
    let v2_blob = path_object_hash(&area, "sparse", &head, "docs/guide.md");

    assert_success(&area.forklift("sparse", &["narrow", "docs"]));

    let pruned = area.forklift("sparse", &["--json", "scope-prune", "docs"]);
    assert_success(&pruned);
    let report: serde_json::Value = serde_json::from_str(&stdout(&pruned)).unwrap();
    assert_eq!(report["data"]["freed"], 1, "{}", stdout(&pruned));
    assert_eq!(report["data"]["kept_unlifted"], 3, "{}", stdout(&pruned));
    assert_eq!(report["data"]["unlifted_pallets"], serde_json::json!(["main"]), "{}", stdout(&pruned));

    for kept in [&v1_tree, &v2_tree, &v2_blob] {
        assert!(object_present(&area, "sparse", kept), "unlifted work is kept: {}", kept);
    }
    assert!(!object_present(&area, "sparse", &v1_blob), "the parent's blob is not read by the lift");

    // The fetch scope did narrow: docs is pruned, only its content stays until it is lifted.
    let rerun = area.forklift("sparse", &["scope-prune", "--dry-run", "docs"]);
    assert_success(&rerun);
    assert!(stdout(&rerun).contains("kept because lifting history the remote does not hold"), "{}", stdout(&rerun));
    assert!(stdout(&rerun).contains("\"main\""), "the pallet to lift is named: {}", stdout(&rerun));
    assert!(stdout(&rerun).contains("scope-prune docs\" again"), "the rerun is named: {}", stdout(&rerun));

    assert_success(&area.forklift("sparse", &["lift"]));

    let freed = area.forklift("sparse", &["--json", "scope-prune", "docs"]);
    assert_success(&freed);
    let report: serde_json::Value = serde_json::from_str(&stdout(&freed)).unwrap();
    assert_eq!(report["data"]["kept_unlifted"], 0, "{}", stdout(&freed));
    for gone in [&v1_tree, &v2_tree, &v2_blob] {
        assert!(!object_present(&area, "sparse", gone), "lifted history is freed: {}", gone);
    }

    assert_success(&area.forklift("sparse", &["expand", "docs"]));
    assert!(object_present(&area, "sparse", &v2_blob), "expand fetches the lifted change back");
}

#[test]
fn scope_prune_frees_history_the_remote_confirms_it_still_holds() {
    // FORK-254: the remote moved on from the head this warehouse was franchised from, so its
    // current head is absent here and reaches nothing. The franchise recorded that head, the
    // prune asks the remote about it, and the remote confirms it still holds it, so the history
    // it reaches is freed without a lower.
    let area = TestArea::new("scope-prune-remote-ahead");
    let _server = sparse_with_docs(&area);

    let base = pallet_head(&area, "sparse", "main");
    let docs_blob = path_object_hash(&area, "sparse", &base, "docs/guide.md");
    assert_eq!(held_heads(&area, "sparse"), vec![format!("{} main", base)], "franchise records its head");

    area.write_file("dev/src/api/a.txt", "api v2\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "ahead"]));
    assert_success(&area.forklift("dev", &["lift"]));

    assert_success(&area.forklift("sparse", &["narrow", "docs"]));

    let pruned = area.forklift("sparse", &["--json", "scope-prune", "docs"]);
    assert_success(&pruned);
    let report: serde_json::Value = serde_json::from_str(&stdout(&pruned)).unwrap();
    assert_eq!(report["data"]["kept_unlifted"], 0, "{}", stdout(&pruned));
    assert!(!object_present(&area, "sparse", &docs_blob), "history the remote confirms is freed");
}

// Revert: take the handshake's live pallets alone as the remote's heads → the deleted pallet's
// lifted work reads as never lifted and is kept.
#[test]
fn scope_prune_frees_work_the_remote_keeps_under_a_tombstone() {
    let area = TestArea::new("scope-prune-tombstone");
    let _server = sparse_with_docs(&area);

    assert_success(&area.forklift("sparse", &["palletize", "side"]));
    std::fs::write(area.path("sparse/docs/guide.md"), "guide on side\n").unwrap();
    assert_success(&area.forklift("sparse", &["load", "."]));
    assert_success(&area.forklift("sparse", &["stack", "side docs edit"]));
    let side = pallet_head(&area, "sparse", "side");
    let side_blob = path_object_hash(&area, "sparse", &side, "docs/guide.md");
    assert_success(&area.forklift("sparse", &["lift"]));
    assert_success(&area.forklift("sparse", &["shift", "main"]));

    assert_success(&area.forklift("dev", &["palletize", "--delete", "side", "--remote"]));
    assert_success(&area.forklift("sparse", &["narrow", "docs"]));

    let pruned = area.forklift("sparse", &["--json", "scope-prune", "docs"]);
    assert_success(&pruned);
    let report = json_of(&pruned);
    assert_eq!(report["data"]["kept_unlifted"], 0, "{}", report);
    assert!(!object_present(&area, "sparse", &side_blob), "the remote keeps the deleted pallet's history");
}

#[test]
fn scope_prune_asks_and_lifts_a_pallet_named_with_a_slash() {
    // FORK-297: a pallet name may carry `/`, and the per-pallet routes take it whole. The lift
    // moves `feature/x` on the remote, and the prune's holds check of it is answered, so the
    // history the remote confirms under that name is freed.
    let area = TestArea::new("scope-prune-slash-pallet");
    let server = sparse_with_docs(&area);

    let base = pallet_head(&area, "sparse", "main");
    let docs_blob = path_object_hash(&area, "sparse", &base, "docs/guide.md");

    // Both remote heads move past anything the sparse warehouse holds.
    area.write_file("dev/src/api/a.txt", "api v2\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "ahead"]));
    assert_success(&area.forklift("dev", &["lift"]));
    assert_success(&area.forklift("dev", &["palletize", "feature/x"]));
    assert_success(&area.forklift("dev", &["shift", "feature/x"]));
    area.write_file("dev/src/api/a.txt", "api v3\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "feature work"]));
    let feature = pallet_head(&area, "dev", "feature/x");
    assert_success(&area.forklift("dev", &["lift"]));
    assert_eq!(remote_pallet_head(&server, "feature/x").as_deref(), Some(feature.as_str()));

    // The only recorded head is under `feature/x`, so freeing the docs blob takes that pallet's
    // holds answer.
    let record = area.path("sparse/.forklift/held-heads");
    let header = std::fs::read_to_string(&record).unwrap().lines().next().unwrap().to_string();
    std::fs::write(&record, format!("{}\n{} feature/x\n", header, base)).unwrap();
    assert_eq!(held_heads(&area, "sparse"), vec![format!("{} feature/x", base)]);
    assert_success(&area.forklift("sparse", &["narrow", "docs"]));

    let pruned = area.forklift("sparse", &["--json", "scope-prune", "docs"]);
    assert_success(&pruned);
    assert!(!stderr(&pruned).contains("holds_unanswered"), "the holds check was answered: {}", stderr(&pruned));
    assert!(!object_present(&area, "sparse", &docs_blob), "history the remote confirms is freed");
}

/// What stands between the client and forklift-server in [`a_slash_named_pallet_crosses`]. Each
/// mode witnesses one side: behind `Decoding`, a server route that takes only one segment splits
/// the name; behind `Refusing`, a client that sends `feature%2Fx` is turned away. A client sending
/// `%2F` passes the decoding proxy, since forklift-server's routes take the decoded segments.
enum SlashProxy {
    /// Decodes `%2F` to `/` before forwarding, as the serverless head's API gateway does.
    Decoding,
    /// Refuses any request whose path holds `%2F`, as a proxy with encoded slashes disallowed does.
    Refusing,
}

/// FORK-304: lift `feature/x`, have the remote answer its holds check, and delete it, all through
/// `proxy`. forklift-server alone is no witness: it decodes `%2F` inside one segment, so a name
/// sent as `feature%2Fx` reaches it whole either way.
fn a_slash_named_pallet_crosses(proxy: SlashProxy, name: &str) {
    let area = TestArea::new(name);
    let server = Server::start(&area, None);
    let proxy = match proxy {
        SlashProxy::Decoding => InterceptingProxy::start_rewriting(&server.url, |path| {
            path.replace("%2F", "/").replace("%2f", "/")
        }),
        SlashProxy::Refusing => InterceptingProxy::start(&server.url, |path| {
            path.to_ascii_uppercase().contains("%2F").then(|| {
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
            })
        }),
    };

    prepare_warehouse(&area, "dev", &proxy.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/docs/guide.md", "guide v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));
    assert_success(&area.forklift(".", &["franchise", &proxy.url, "sparse", "--only", "src/api", "--only", "docs"]));
    let base = pallet_head(&area, "sparse", "main");
    let docs_blob = path_object_hash(&area, "sparse", &base, "docs/guide.md");

    // The lift moves `feature/x` on the remote, not some prefix of it.
    area.write_file("dev/src/api/a.txt", "api v2\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "ahead"]));
    assert_success(&area.forklift("dev", &["lift"]));
    assert_success(&area.forklift("dev", &["palletize", "feature/x"]));
    assert_success(&area.forklift("dev", &["shift", "feature/x"]));
    area.write_file("dev/src/api/a.txt", "api v3\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "feature work"]));
    let feature = pallet_head(&area, "dev", "feature/x");
    assert_success(&area.forklift("dev", &["lift"]));
    assert_eq!(remote_pallet_head(&server, "feature/x").as_deref(), Some(feature.as_str()));

    // The only recorded head is under `feature/x`, so freeing the docs blob takes that pallet's
    // holds answer.
    let record = area.path("sparse/.forklift/held-heads");
    let header = std::fs::read_to_string(&record).unwrap().lines().next().unwrap().to_string();
    std::fs::write(&record, format!("{}\n{} feature/x\n", header, base)).unwrap();
    assert_success(&area.forklift("sparse", &["narrow", "docs"]));
    let pruned = area.forklift("sparse", &["--json", "scope-prune", "docs"]);
    assert_success(&pruned);
    assert!(!stderr(&pruned).contains("holds_unanswered"), "the holds check was answered: {}", stderr(&pruned));
    assert!(!object_present(&area, "sparse", &docs_blob), "history the remote confirms is freed");

    assert_success(&area.forklift("dev", &["shift", "main"]));
    assert_success(&area.forklift("dev", &["palletize", "--delete", "feature/x", "--remote"]));
    assert_eq!(remote_pallet_head(&server, "feature/x"), None, "the delete removed feature/x");
    assert_eq!(remote_pallet_head(&server, "main").as_deref(), Some(pallet_head(&area, "dev", "main").as_str()));
}

#[test]
fn a_slash_named_pallet_crosses_a_proxy_that_decodes_encoded_slashes() {
    a_slash_named_pallet_crosses(SlashProxy::Decoding, "slash-pallet-decoding-proxy");
}

#[test]
fn a_slash_named_pallet_crosses_a_proxy_that_refuses_encoded_slashes() {
    a_slash_named_pallet_crosses(SlashProxy::Refusing, "slash-pallet-refusing-proxy");
}

/// Point the served root's `main` at `head`, as an edit on the server's disk would, or a pallet
/// re-created there after its tombstone was dropped.
fn move_served_main(area: &TestArea, head: &str) {
    std::fs::write(area.path("server-root/.forklift/pallets/main"), format!("{}\n", head)).unwrap();
}

#[test]
fn scope_prune_frees_nothing_on_a_forged_held_head() {
    // An entry is only a candidate: one naming a head the remote never took, here this
    // warehouse's own unlifted head, is answered `not_held` and frees nothing.
    let area = TestArea::new("scope-prune-forged-held-head");
    let _server = sparse_with_docs(&area);

    std::fs::write(area.path("sparse/docs/guide.md"), "guide v2, never lifted\n").unwrap();
    assert_success(&area.forklift("sparse", &["load", "."]));
    assert_success(&area.forklift("sparse", &["stack", "local docs edit"]));
    let head = pallet_head(&area, "sparse", "main");
    let v2_blob = path_object_hash(&area, "sparse", &head, "docs/guide.md");

    let record = area.path("sparse/.forklift/held-heads");
    let mut text = std::fs::read_to_string(&record).unwrap();
    text.push_str(&format!("{} main\n", head));
    std::fs::write(&record, text).unwrap();
    assert_eq!(held_heads(&area, "sparse").last(), Some(&format!("{} main", head)));

    assert_success(&area.forklift("sparse", &["narrow", "docs"]));
    let pruned = area.forklift("sparse", &["--json", "scope-prune", "docs"]);
    assert_success(&pruned);
    let report: serde_json::Value = serde_json::from_str(&stdout(&pruned)).unwrap();
    assert_eq!(report["data"]["kept_unlifted"], 3, "{}", stdout(&pruned));
    assert!(object_present(&area, "sparse", &v2_blob), "a forged entry frees nothing");
}

#[test]
fn scope_prune_frees_nothing_the_remote_moved_back_past_or_started_over() {
    // A remote ref is not forward-only. The remote served `ahead` and this warehouse recorded it;
    // then the served `main` moves back to `base`, and later to a head of another lineage (a
    // pallet re-created under the name after its tombstone was dropped). Neither reaches `ahead`,
    // so the remote answers `not_held` and the history only `ahead` reaches is kept both times.
    let area = TestArea::new("scope-prune-remote-moved-back");
    let _server = sparse_with_docs(&area);
    let base = pallet_head(&area, "sparse", "main");

    area.write_file("dev/src/api/a.txt", "api v2\n");
    area.write_file("dev/docs/guide.md", "guide v2\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "ahead"]));
    assert_success(&area.forklift("dev", &["lift"]));
    let ahead = pallet_head(&area, "dev", "main");

    assert_success(&area.forklift("dev", &["palletize", "side", &base]));
    area.write_file("dev/src/api/a.txt", "api side\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "side"]));
    assert_success(&area.forklift("dev", &["lift"]));
    let side = pallet_head(&area, "dev", "side");

    assert_success(&area.forklift("sparse", &["lower"]));
    assert_eq!(held_heads(&area, "sparse"), vec![format!("{} main", ahead)]);
    let v2_blob = path_object_hash(&area, "sparse", &ahead, "docs/guide.md");
    assert_success(&area.forklift("sparse", &["narrow", "docs"]));

    for (moved_to, why) in [(&base, "a head moved back"), (&side, "a re-created pallet")] {
        move_served_main(&area, moved_to);
        let pruned = area.forklift("sparse", &["--json", "scope-prune", "--dry-run", "docs"]);
        assert_success(&pruned);
        let report: serde_json::Value = serde_json::from_str(&stdout(&pruned)).unwrap();
        assert!(report["data"]["kept_unlifted"].as_u64().unwrap() > 0, "{}: {}", why, stdout(&pruned));
    }

    assert_success(&area.forklift("sparse", &["scope-prune", "docs"]));
    assert!(object_present(&area, "sparse", &v2_blob), "history the remote no longer holds is kept");
}

#[test]
fn scope_prune_refuses_and_changes_nothing_on_a_held_heads_record_that_does_not_parse() {
    // A record that does not parse is an error, never an empty record: read as empty it would only
    // free less, but it would hide that the record is broken.
    let area = TestArea::new("scope-prune-corrupt-held-heads");
    let _server = sparse_with_docs(&area);
    let base = pallet_head(&area, "sparse", "main");
    let docs_blob = path_object_hash(&area, "sparse", &base, "docs/guide.md");

    std::fs::write(area.path("sparse/.forklift/held-heads"), "not a held-heads record\n").unwrap();
    assert_success(&area.forklift("sparse", &["narrow", "docs"]));
    let scope_file = area.path("sparse/.forklift/config/fetch-scope");
    let scope_before = std::fs::read(&scope_file).unwrap();

    let refused = area.forklift("sparse", &["scope-prune", "docs"]);
    assert!(!refused.status.success(), "a corrupt record refuses the prune");
    assert!(stderr(&refused).contains("held-heads") && stderr(&refused).contains("removing it is safe"),
            "the refusal names the record and the way out: {}", stderr(&refused));
    assert!(object_present(&area, "sparse", &docs_blob), "nothing is freed");
    assert_eq!(std::fs::read(&scope_file).unwrap(), scope_before, "the fetch scope is unchanged");
}

#[test]
fn scope_prune_frees_nothing_beyond_the_current_heads_when_the_remote_cannot_answer_holds() {
    // A remote too old to know the holds endpoint answers 404: the recorded head is not confirmed,
    // so the prune frees only what the remote's current heads reach, as before the record.
    let area = TestArea::new("scope-prune-holds-404");
    let server = Server::start(&area, None);
    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/docs/guide.md", "guide v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = std::sync::Arc::clone(&asked);
    let proxy = InterceptingProxy::start(&server.url, move |path| {
        path.contains("/v1/holds/").then(|| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        })
    });
    assert_success(&area.forklift(".", &["franchise", &proxy.url, "sparse",
        "--only", "src/api", "--only", "docs"]));
    let base = pallet_head(&area, "sparse", "main");
    let docs_blob = path_object_hash(&area, "sparse", &base, "docs/guide.md");

    area.write_file("dev/src/api/a.txt", "api v2\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "ahead"]));
    assert_success(&area.forklift("dev", &["lift"]));

    assert_success(&area.forklift("sparse", &["narrow", "docs"]));
    let pruned = area.forklift("sparse", &["--json", "scope-prune", "docs"]);
    assert_success(&pruned);
    assert!(asked.load(std::sync::atomic::Ordering::SeqCst) > 0, "the prune asked the remote");
    let report: serde_json::Value = serde_json::from_str(&stdout(&pruned)).unwrap();
    assert_eq!(report["data"]["freed"], 0, "{}", stdout(&pruned));
    assert!(object_present(&area, "sparse", &docs_blob), "an unanswered holds check confirms nothing");
    assert!(stderr(&pruned).contains("\"warning\":\"holds_unanswered\""), "the pallet left unconfirmed is named: {}",
            stderr(&pruned));

    // The way out the warning names: after a lower the remote's current head is here and reaches it.
    assert_success(&area.forklift("sparse", &["lower"]));
    assert_success(&area.forklift("sparse", &["scope-prune", "docs"]));
    assert!(!object_present(&area, "sparse", &docs_blob), "after a lower the prune frees it");
}

/// The entries of warehouse `name`'s held-heads record, as `<head> <pallet>` lines (the header
/// skipped); none when the record is absent.
fn held_heads(area: &TestArea, name: &str) -> Vec<String> {
    match std::fs::read_to_string(area.path(&format!("{}/.forklift/held-heads", name))) {
        Ok(text) => text.lines().skip(1).map(str::to_string).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => panic!("the held-heads record could not be read: {}", e),
    }
}

#[test]
fn a_landed_lift_lower_or_franchise_records_its_head_and_a_refused_lift_records_nothing() {
    // FORK-254: lift records the head the remote accepted, replacing the recorded heads it descends
    // from; lower and franchise record the head they were served. A lift the remote refuses records nothing, since
    // the record is written only after the compare-and-set.
    let area = TestArea::new("held-heads-record");
    let server = sparse_with_docs(&area);
    assert_success(&area.forklift(".", &["franchise", &server.url, "clone"]));

    let base = pallet_head(&area, "dev", "main");
    assert_eq!(held_heads(&area, "dev"), vec![format!("{} main", base)]);

    area.write_file("dev/src/api/a.txt", "api v2\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "ahead"]));
    assert_success(&area.forklift("dev", &["lift"]));
    let ahead = pallet_head(&area, "dev", "main");
    assert_ne!(ahead, base);
    assert_eq!(held_heads(&area, "dev"), vec![format!("{} main", ahead)], "the new head replaces its ancestor");

    // The same file as dev's change, so the lift cannot auto-merge past the remote's head.
    area.write_file("sparse/src/api/a.txt", "api local\n");
    assert_success(&area.forklift("sparse", &["load", "."]));
    assert_success(&area.forklift("sparse", &["stack", "local"]));
    let refused = area.forklift("sparse", &["lift"]);
    assert!(!refused.status.success(), "the remote diverged, so the lift is refused: {}", stdout(&refused));
    assert_eq!(held_heads(&area, "sparse"), vec![format!("{} main", base)],
               "the franchise recorded its head, and a refused lift records nothing");

    assert_success(&area.forklift("clone", &["lower"]));
    assert_eq!(pallet_head(&area, "clone", "main"), ahead);
    assert_eq!(held_heads(&area, "clone"), vec![format!("{} main", ahead)], "lower records the head it adopted");

    // The record only names heads to ask about, so removing it is a way out of one that does not
    // parse; an up-to-date lower writes it again.
    std::fs::remove_file(area.path("clone/.forklift/held-heads")).unwrap();
    assert_success(&area.forklift("clone", &["lower"]));
    assert_eq!(held_heads(&area, "clone"), vec![format!("{} main", ahead)], "an up-to-date lower records the head too");
}

#[test]
fn scope_prune_refuses_and_changes_nothing_when_the_remote_is_unreachable() {
    // Without the remote's heads the prune cannot tell what exists only here, so it frees nothing
    // and does not narrow the fetch scope either.
    let area = TestArea::new("scope-prune-no-remote");
    let server = sparse_with_docs(&area);

    let head = pallet_head(&area, "sparse", "main");
    let docs_blob = path_object_hash(&area, "sparse", &head, "docs/guide.md");

    assert_success(&area.forklift("sparse", &["narrow", "docs"]));
    let scope_file = area.path("sparse/.forklift/config/fetch-scope");
    let scope_before = std::fs::read(&scope_file).unwrap();

    drop(server);

    let refused = area.forklift("sparse", &["scope-prune", "docs"]);
    assert!(!refused.status.success(), "an unreachable remote refuses the prune");
    assert!(stderr(&refused).contains("never frees work that exists only here; nothing was changed"),
        "{}", stderr(&refused));
    assert!(object_present(&area, "sparse", &docs_blob), "nothing is freed");
    assert_eq!(std::fs::read(&scope_file).unwrap(), scope_before, "the fetch scope is not narrowed");
}

#[test]
fn scope_prune_refuses_and_changes_nothing_when_the_remote_trust_changed() {
    // A remote whose genesis changed is not the one this warehouse trusts, so its heads say
    // nothing about what is safe to free: the prune refuses as a lower would, freeing nothing.
    let area = TestArea::new("scope-prune-regenesis");
    let _server = regenesis_on_remote(&area, |server| {
        area.write_file("a/docs/guide.md", "guide v1\n");
        area.write_file("a/src/x.txt", "x v1\n");
        assert_success(&area.forklift("a", &["load", "."]));
        assert_success(&area.forklift("a", &["stack", "paths"]));
        assert_success(&area.forklift("a", &["lift"]));
        assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--token", "root",
            "--only", "docs", "--only", "src"]));
    });

    let head = pallet_head(&area, "sparse", "main");
    let docs_blob = path_object_hash(&area, "sparse", &head, "docs/guide.md");

    assert_success(&area.forklift("sparse", &["narrow", "docs"]));

    let refused = area.forklift("sparse", &["scope-prune", "docs"]);
    assert!(!refused.status.success(), "a changed remote genesis refuses the prune");
    assert!(stderr(&refused).contains("CHANGED"), "{}", stderr(&refused));
    assert!(stderr(&refused).contains("nothing was changed"), "{}", stderr(&refused));
    assert!(object_present(&area, "sparse", &docs_blob), "nothing is freed");
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
fn scope_prune_is_refused_while_another_process_writes_the_store() {
    // FORK-150: scope-prune computes what to keep from the fetch scope and every bay's work, then
    // frees the rest. A writer in another bay — a load that dedups against a planned object, an
    // expand widening the fetch scope — must not run between the two, so scope-prune takes the
    // store exclusively before it reads anything, dry run included, and is refused while any
    // writer holds it. The test process stands in for that writer with a shared hold.
    //
    // Revert: the handler skips the exclusive acquire → the prune runs, frees the objects and
    // narrows the fetch scope → red.
    let area = TestArea::new("scope-prune-store-access");
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

    let writer = hold_store_access(&area.path("sparse"), false);

    for args in [&["--json", "scope-prune", "src/web"][..], &["--json", "scope-prune", "src/web", "--dry-run"][..]] {
        let refused = area.forklift("sparse", args);
        assert!(!refused.status.success(), "{:?} must be refused while another process writes the store", args);
        let said = format!("{}{}", stdout(&refused), stderr(&refused));
        assert!(said.contains("using the object store"), "the refusal should say the store is in use: {}", said);
    }

    assert!(object_present(&area, "sparse", &web_tree), "a refused prune frees no object");
    assert!(object_present(&area, "sparse", &web_blob), "a refused prune frees no object");
    let scope = area.forklift("sparse", &["--json", "scope"]);
    let scope_json: serde_json::Value = serde_json::from_str(&stdout(&scope)).unwrap();
    assert_eq!(scope_json["data"]["fetch_scope"], serde_json::json!(["src/api", "src/web"]),
        "a refused prune leaves the fetch scope unchanged: {}", stdout(&scope));

    // Once the writer is done, the same prune runs and frees them.
    drop(writer);
    assert_success(&area.forklift("sparse", &["scope-prune", "src/web"]));
    assert!(!object_present(&area, "sparse", &web_blob), "with no writer, the prune frees the object");
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
    {
        let _scope = forklift_core::globals::StorageRootScope::enter(&area.path("sparse"));
        let narrowed = forklift_core::util::scope_utils::MaterializationScope::from_prefixes(["src/api"]);
        forklift_core::util::scope_utils::set_fetch_scope(&narrowed).unwrap();
    }
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

    // Drive the client straight against the endpoint. The storage scope is thread-local and the
    // client imports on a blocking pool thread, so the objects landing in `sparse` (not in
    // whatever the pool thread's working directory resolves to) is what proves the import
    // re-entered this thread's scope.
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let _scope = forklift_core::globals::StorageRootScope::enter(&area.path("sparse"));
    let client = forklift_core::util::remote_utils::RemoteClient::new(&server.url, None).unwrap();

    // The endpoint resolves src/web to its subtree and streams the closure, which the client
    // imports as it arrives — the sealed objects land hash-verified like any bundle.
    runtime
        .block_on(client.fetch_subtree(&head, "src/web", &web_tree))
        .expect("the subtree fetch and its import must not error")
        .expect("a present remote serves the subtree, not a 404");

    assert!(object_present(&area, "sparse", &web_tree), "the endpoint delivered the subtree object");
    assert!(object_present(&area, "sparse", &web_blob), "the endpoint delivered the subtree's blob");

    // A path the remote cannot resolve answers 404, which the client returns as `None` — the
    // signal to fall back to the hash-addressed walk. An older remote without the route 404s
    // identically, so the fallback path is the same one this returns.
    let missing = runtime
        .block_on(client.fetch_subtree(&head, "does/not/exist", &web_tree))
        .expect("a 404 is not an error");
    assert!(missing.is_none(), "an unresolved path signals the client to fall back");
}

#[test]
fn a_subtree_response_imports_in_full_when_the_client_already_holds_a_tree_inside_it() {
    // The server sends a subtree's whole closure, including trees the client already holds. Such
    // a record is skipped, but its tree must still widen what the response may carry — loaded
    // from the client's store — or the objects below it would be refused as unrequested.
    let area = TestArea::new("subtree-endpoint-held-tree");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    area.write_file("dev/src/web/inner/x.txt", "inner v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let web_tree = path_object_hash(&area, "dev", &head, "src/web");
    let web_blob = path_object_hash(&area, "dev", &head, "src/web/w.txt");
    let inner_tree = path_object_hash(&area, "dev", &head, "src/web/inner");

    // Scoped to src/web/inner: the client holds that tree and its blob, but not src/web's file.
    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/web/inner"]));
    assert!(object_present(&area, "sparse", &inner_tree), "the scoped tree is held");
    assert!(!object_present(&area, "sparse", &web_blob), "src/web's own file starts sealed");

    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let _scope = forklift_core::globals::StorageRootScope::enter(&area.path("sparse"));
    let client = forklift_core::util::remote_utils::RemoteClient::new(&server.url, None).unwrap();

    runtime
        .block_on(client.fetch_subtree(&head, "src/web", &web_tree))
        .expect("a closure overlapping what the client holds must import")
        .expect("a present remote serves the subtree, not a 404");

    assert!(object_present(&area, "sparse", &web_blob), "the endpoint delivered src/web's file");
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

    runtime
        .block_on(client.fetch_subtree(&head, "src/a dir#1/100%", &weird_tree))
        .expect("the subtree fetch and its import must not error")
        .expect("the server must resolve the reserved-character path, not 404 on a mangled URL");

    assert!(object_present(&area, "sparse", &weird_tree),
        "the endpoint delivered the reserved-character subtree object");
    assert!(object_present(&area, "sparse", &weird_blob),
        "the endpoint delivered the reserved-character subtree's blob");
}

/// Build a subtree one entry over the subtree endpoint's per-response cap directly under one
/// directory `name`, and set the warehouse's `main` head to a parcel rooted over it: a directory
/// of [`MAX_MISSING_BATCH`] blobs, which with the directory's own tree makes a closure of one
/// more hash than the cap allows.
///
/// Only the two trees and the parcel are stored. The blobs are named by their real hashes but
/// never written: the closure walk reads trees, records blob hashes without opening them, and
/// stops at the cap, so the files would never be read, and writing ten thousand of them took
/// most of a minute on Windows. The warehouse this leaves is not a valid one, which is why the
/// helper builds only an over-cap subtree: an under-cap closure would go on to bundle the
/// missing blobs and fail.
fn build_over_cap_subtree_without_blobs(warehouse_root: &Path, name: &str) -> String {
    use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
    use forklift_core::enums::dir_entry_type::DirEntryType;
    use forklift_core::globals::StorageRootScope;
    use forklift_core::model::blob::Blob;
    use forklift_core::model::object::loose_object::LooseObject;
    use forklift_core::model::parcel::Parcel;
    use forklift_core::model::remote::MAX_MISSING_BATCH;
    use forklift_core::model::tree_item::TreeItem;
    use forklift_core::util::pallet_utils;

    let _scope = StorageRootScope::enter(warehouse_root);

    fn store(mut object: LooseObject) -> String {
        object.store().unwrap();
        object.hash
    }

    let mut wide = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
    for i in 0..MAX_MISSING_BATCH {
        let hash = LooseObjectBuilder::build_blob(&Blob { content: i.to_string().into_bytes() }).hash;
        wide.add_child(TreeItem::new(format!("f{}", i), hash, DirEntryType::Normal));
    }
    let wide_hash = store(LooseObjectBuilder::build_tree(&wide));

    let mut top = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
    top.add_child(TreeItem::new(name.to_string(), wide_hash, DirEntryType::Tree));
    let top_hash = store(LooseObjectBuilder::build_tree(&top));

    let parcel = Parcel {
        tree_hash: top_hash,
        parents: Vec::new(),
        actions: Vec::new(),
        description: Some("over-cap subtree fixture".to_string()),
    };
    let parcel_hash = store(LooseObjectBuilder::build_parcel(&parcel));
    // Straight to the ref file: the check every real ref write runs would refuse a head whose
    // blobs this store does not hold.
    std::fs::write(pallet_utils::get_pallets_folder().join("main"), format!("{}\n", parcel_hash)).unwrap();

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

    // MAX_MISSING_BATCH blob hashes plus their one parent tree is a closure one entry over the
    // cap.
    let head = build_over_cap_subtree_without_blobs(&area.path("server-root"), "big");

    // The server refuses with 422, not a streamed bundle — same status and wording whether the
    // walk is bounded internally (the fix) or only checked after it completes (the defect this
    // guards): the cap changes what the walk pays for, never what the caller observes.
    let (status, body) = http_get(&server.url, &format!("/v1/parcels/{}/subtree/big", head));
    assert_eq!(status, "HTTP/1.1 422 Unprocessable Entity",
        "an oversized subtree closure is refused, not streamed");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        json["error"],
        serde_json::json!(format!(
            "The subtree at \"big\" has more than {} objects; fetch it with the hash-addressed \
            walk instead.",
            forklift_core::model::remote::MAX_MISSING_BATCH
        )),
        "unexpected refusal body: {}", body
    );

    // The client treats that 422 exactly like a 404: `Ok(None)`, the fallback signal.
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let client = forklift_core::util::remote_utils::RemoteClient::new(&server.url, None).unwrap();
    // The refusal comes before any record, so the subtree hash the response would be admitted
    // against is never consulted.
    let result = runtime.block_on(client.fetch_subtree(&head, "big", &"0".repeat(64)));
    assert!(result.is_ok(), "an over-cap subtree must not be a client error: {:?}", result.err());
    assert!(result.unwrap().is_none(), "an over-cap subtree signals fallback, exactly like a 404");
}

// ---------------------------------------------------------------------------------------------
// §3.2 — heal-driven refetch: `forklift heal` fetches from a configured remote itself, inside
// the locked verb, to close the "forklift lower" wedge (a vanished-still-referenced object's
// only named remedy was blocked by the very chokepoint the refusal fired from). See
// `recovery_utils`'s module doc comment (the "Heal-driven refetch" section) for the design; the
// two-pass mechanism (`attempt_heal_driven_refetch`) is documented on that function directly,
// including the empirical finding that a bare `fetch_history_scoped` reuse does not actually
// recover the wedge's own motivating shape.
// ---------------------------------------------------------------------------------------------

/// The path of the taint directory in a warehouse under the area — the remote-harness
/// counterpart of `cli.rs`'s `taint_dir_path` (that file drives a `TestWarehouse`, this one a
/// `TestArea` directory; the on-disk layout `taint_dir_path_under` resolves is identical either
/// way).
fn taint_dir_path(area: &TestArea, dir: &str) -> PathBuf {
    forklift_core::util::taint_utils::taint_dir_path_under(&area.path(dir))
}

/// Hand-plant a complete (non-torn) taint file recording `paths` — a crash-before-heal
/// simulation, mirroring `cli.rs`'s helper of the same name.
fn plant_taint(area: &TestArea, dir: &str, paths: &[&str]) {
    let dir_path = taint_dir_path(area, dir);
    std::fs::create_dir_all(&dir_path).unwrap();
    std::fs::write(dir_path.join("taint-1-0"), forklift_core::util::taint_utils::taint_file_bytes(paths)).unwrap();
}

/// Every regular taint file's recorded lines, unioned (dropping the header and the `END`
/// terminator) — used to inspect exactly what a recovery attempt left standing. The paths these
/// tests plant are plain object paths, which escaping leaves as they are.
fn recorded_taint_paths(area: &TestArea, dir: &str) -> Vec<String> {
    let dir_path = taint_dir_path(area, dir);
    if !dir_path.exists() {
        return Vec::new();
    }

    let mut paths = Vec::new();
    for entry in std::fs::read_dir(&dir_path).unwrap() {
        let entry = entry.unwrap();
        if !entry.path().is_file() {
            continue;
        }
        let content = std::fs::read_to_string(entry.path()).unwrap();
        paths.extend(content.lines()
            .filter(|line| *line != "END" && !line.starts_with("/forklift-taint "))
            .map(|line| line.to_string()));
    }
    paths
}

/// The root-relative loose-object path (`objects/<2 hex>/<rest>`) a hash lives at.
fn loose_object_relative_path(hash: &str) -> String {
    format!("objects/{}/{}", &hash[..2], &hash[2..])
}

#[test]
fn heal_driven_refetch_recovers_a_dangling_object_the_remote_has() {
    // The wedge itself (memo §1.1): before this slice, this exact scenario refused naming
    // "forklift lower" as the remedy — but `lower` hits the same taint and refuses before it
    // ever reaches its own fetch. `heal` must now drive the fetch itself and clear.
    //
    // This fixture is deliberately the "already fully published, nothing has changed since"
    // shape: the blob vanishes from the *current* pallet's own, already-lifted head. A first
    // draft of this feature that only reused `fetch_history_scoped` was found — by reproducing
    // exactly this scenario by hand — to NOT recover this shape at all: that walk never
    // re-descends into a parcel's tree once the parcel is judged already complete (reachable
    // from a local ref), which the current pallet's own unchanged head always is. See
    // `attempt_heal_driven_refetch`'s doc comment for the fix (a second, targeted pass).
    //
    // Mutation: remove the heal-driven refetch (make `resolve_the_rest` skip
    // `attempt_heal_driven_refetch`, always treating the remote as unconfigured) → `heal`
    // refuses on the recoverable object (exit 21) → red.
    let area = TestArea::new("heal-refetch-wedge");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/a.txt", "hello, this is durable content\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base parcel"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let blob = path_object_hash(&area, "dev", &head, "a.txt");
    let relative = loose_object_relative_path(&blob);

    std::fs::remove_file(object_store_path(&area, "dev", &blob)).unwrap();
    plant_taint(&area, "dev", &[&relative]);

    let status = area.forklift("dev", &["--json", "heal"]);
    assert_success(&status);
    let value: serde_json::Value = serde_json::from_str(&stdout(&status)).unwrap();
    assert_eq!(value["data"]["was_tainted"], true);
    assert!(
        value["data"]["resolved"].as_array().unwrap().iter().any(|entry| entry == &blob),
        "the recovered hash must be reported resolved: {}", value
    );

    assert!(recorded_taint_paths(&area, "dev").is_empty(), "the taint must be fully cleared");
    assert!(object_present(&area, "dev", &blob), "the blob must actually be back on disk");

    // The warehouse is fully usable again, including the write path the taint exists to protect.
    assert_success(&area.forklift("dev", &["stocktake"]));
}

#[test]
fn heal_driven_refetch_clears_only_the_hash_the_remote_actually_has() {
    // §3.2 D3: a remainder over two hashes, the remote genuinely has only one — `heal` must
    // clear exactly the recoverable one and leave the other exactly as dangling, never
    // optimistically clearing the whole taint just because *a* fetch succeeded.
    //
    // Mutation: clear the whole remainder once any fetch attempt returns without a hard error,
    // instead of re-verifying each hash independently via `raw_object_present` → the still-
    // missing hash is silently dropped from the taint → red.
    let area = TestArea::new("heal-refetch-partial");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/a.txt", "recoverable content\n");
    area.write_file("dev/b.txt", "unrecoverable content\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "two files"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let blob_a = path_object_hash(&area, "dev", &head, "a.txt");
    let blob_b = path_object_hash(&area, "dev", &head, "b.txt");

    // The remote's own copy of b is gone too — genuinely unrecoverable, unlike a.
    std::fs::remove_file(object_store_path(&area, "server-root", &blob_b)).unwrap();

    std::fs::remove_file(object_store_path(&area, "dev", &blob_a)).unwrap();
    std::fs::remove_file(object_store_path(&area, "dev", &blob_b)).unwrap();

    let relative_a = loose_object_relative_path(&blob_a);
    let relative_b = loose_object_relative_path(&blob_b);
    plant_taint(&area, "dev", &[&relative_a, &relative_b]);

    let status = area.forklift("dev", &["--json", "heal"]);
    assert_eq!(status.status.code(), Some(21));
    let value: serde_json::Value = serde_json::from_str(&stdout(&status)).unwrap();
    assert_eq!(value["error"]["code"], "durability_taint");
    assert!(
        value["error"]["message"].as_str().unwrap().contains(&blob_b),
        "the still-dangling hash must be named: {}", value
    );

    // Exactly b's path remains recorded; a's is gone — no more, no less.
    assert_eq!(recorded_taint_paths(&area, "dev"), vec![relative_b.clone()],
        "the remainder must be exactly the one unrecovered hash");

    assert!(object_present(&area, "dev", &blob_a), "the recoverable object must actually be back");
    assert!(!object_present(&area, "dev", &blob_b), "the genuinely-unrecoverable object stays gone");
}

// Heal's own refetch stores go through the ordinary write path, which self-trips
// `taint_recheck`'s standing-taint success re-check, because the taint being healed stands for
// the entire run. With exactly one missing object the first (and only) store's `Err` is masked
// by `resolve_the_rest`'s unconditional `raw_object_present` recheck (D3), so
// `heal_driven_refetch_clears_only_the_hash_the_remote_actually_has` above never exercises the
// N≥2 path. This test does — it pins `SelfTripExemptionGuard` (a process-global RAII counter
// consulted only in `taint_recheck`'s success arm), armed for the whole of
// `attempt_heal_driven_refetch`.
#[test]
fn heal_driven_refetch_recovers_every_object_when_more_than_one_is_missing() {
    // The batch path's bundle import stores its records one at a time, each with its own
    // `?` — the first object's store self-trips `taint_recheck` against the still-standing taint
    // and the `Err` aborts the whole call before the second object is even attempted.
    // `resolve_the_rest`'s recovery verdict is decided by a post-fetch `raw_object_present`
    // recheck regardless of the fetch call's own `Ok`/`Err`, so whichever object *did* land is
    // still correctly reported recovered — the bug is invisible with only one missing object,
    // which is exactly the shape of the test above. With two, only one lands.
    //
    // Mutation: revert `SelfTripExemptionGuard` (or arm it somewhere that does not span both
    // `attempt_heal_driven_refetch` passes and every store inside them) → one of the two objects
    // self-trips its own success re-check and is reported still dangling → red.
    let area = TestArea::new("heal-refetch-multi");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/a.txt", "first recoverable content\n");
    area.write_file("dev/b.txt", "second recoverable content\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "two files"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let blob_a = path_object_hash(&area, "dev", &head, "a.txt");
    let blob_b = path_object_hash(&area, "dev", &head, "b.txt");

    // Both blobs are genuinely gone locally — the remote (which received both via `lift`) is
    // the only thing that can restore them.
    std::fs::remove_file(object_store_path(&area, "dev", &blob_a)).unwrap();
    std::fs::remove_file(object_store_path(&area, "dev", &blob_b)).unwrap();

    let relative_a = loose_object_relative_path(&blob_a);
    let relative_b = loose_object_relative_path(&blob_b);
    // Planted on disk via the durable-taint helper, not through the in-process gate —
    // `gate_check` is deliberately left unexempted by the fix, so this test must not accidentally
    // depend on gate behavior to reproduce the bug.
    plant_taint(&area, "dev", &[&relative_a, &relative_b]);

    let status = area.forklift("dev", &["--json", "heal"]);
    assert_success(&status);
    let value: serde_json::Value = serde_json::from_str(&stdout(&status)).unwrap();
    let resolved = value["data"]["resolved"].as_array().unwrap();
    assert!(resolved.iter().any(|entry| entry == &blob_a),
        "the first recoverable hash must be reported resolved: {}", value);
    assert!(resolved.iter().any(|entry| entry == &blob_b),
        "the second recoverable hash must be reported resolved too, not just the first: {}", value);

    assert!(recorded_taint_paths(&area, "dev").is_empty(), "the taint must be fully cleared");
    assert!(object_present(&area, "dev", &blob_a), "the first object must actually be back on disk");
    assert!(object_present(&area, "dev", &blob_b), "the second object must actually be back on disk");

    // The warehouse is fully usable again, including the write path the taint exists to protect.
    assert_success(&area.forklift("dev", &["stocktake"]));
}

// The self-trip bug's *other* direction (see the module-level §3.2 doc comment on
// `attempt_heal_driven_refetch`): pass 1 (`fetch_history`/`fetch_history_scoped`, run to bring
// each pallet's real history locally up to date before the closure walk runs) stores every
// history object it fetches through the identical taint-recheck-gated path pass 2 does. This
// direction is strictly worse than the multi-object test above — that one over-refuses (loud,
// exit 21 on a recoverable object); this one silently drops a durable-taint guarantee: an
// ancestor commit pass 1 fails to restore makes the closure walk's presence-tolerant descent
// (I3 — a missing parent parcel is skipped, not treated as an error) skip straight past it, so a
// leaf referenced *only* through that ancestor's tree is misclassified unreferenced and cleared
// even though it is still genuinely dangling.
#[test]
fn heal_pass_one_restoring_a_missing_ancestor_keeps_the_closure_walk_from_falsely_clearing_a_still_referenced_leaf() {
    // Deterministic fixture, one pallet ("main"), two lifts:
    //   H1 (parent: none)  — tree has a.txt -> the leaf hash under test.
    //   H2 (parent: H1)    — a.txt is gone from the tree; c.txt (unrelated) takes its place.
    // The leaf is referenced *only* through H1's tree, never H2's — so the closure walk can only
    // find it by walking H2's ancestry back into H1.
    //
    // Both H1 and H2 (the parcel/commit objects themselves, not their trees or blobs) are then
    // deleted locally. Deleting only H1 is not enough to exercise pass 1 at all:
    // `fetch_history`'s own "already known complete" bound (`is_known_complete`) checks the
    // *current* pallet head (H2) first, and since H2 is dev's own exact local ref, a present H2
    // is judged complete on the spot — its own parcel is never even loaded to discover H1 as a
    // parent, so the walk never reaches H1 to notice it is missing. Deleting H2 too makes the
    // walk's very first frontier node fail its presence check, forcing it to actually fetch H2,
    // load its parcel, and cascade into checking (and, once the fix's guard is armed, restoring)
    // H1 — the same call shape `fetch_missing_objects` and the batch import use elsewhere,
    // just reached through history discovery instead of a direct hash target. Neither H1 nor H2
    // was ever locally re-fetched before this `heal` run — this is genuinely pass 1's own
    // restoration, not something already sitting on disk.
    //
    // The leaf itself is deleted from *both* sides — locally and from the server's own object
    // store (`object_store_path(&area, "server-root", ...)`, the same direct-server-deletion
    // idiom `heal_driven_refetch_clears_only_the_hash_the_remote_actually_has` above uses) — so
    // it is genuinely, permanently unrecoverable regardless of pass 1 or pass 2. That isolates
    // exactly the thing this test pins: not whether the leaf's *bytes* come back (they never can
    // here), but whether the closure walk correctly reports it still referenced (dangling) rather
    // than wrongly clearing it because an ancestor it needed to walk through never made it back.
    //
    // Mutation: revert the `SelfTripExemptionGuard` early-return in `taint_recheck` → H2's own
    // store self-trips against the leaf's still-standing taint; `fetch_history`'s wave-fetch `?`
    // aborts before `load_parcel(H2)` is ever called, so H1 is never even attempted → the closure
    // walk hits H2 (present, restored despite the `Err` — the bytes land before the recheck
    // refuses), descends into H1 (absent, I3-tolerated: skipped, not erred), never finds the
    // leaf's reference → the leaf is wrongly reported resolved and the taint clears → red.
    let area = TestArea::new("heal-pass1-ancestry");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/a.txt", "the leaf: referenced only by the older ancestor's tree\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base with the leaf"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let h1 = pallet_head(&area, "dev", "main");
    let leaf = path_object_hash(&area, "dev", &h1, "a.txt");

    std::fs::remove_file(area.path("dev/a.txt")).unwrap();
    area.write_file("dev/c.txt", "unrelated newer content\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "drop the leaf, add something else"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let h2 = pallet_head(&area, "dev", "main");
    assert_ne!(h1, h2, "the second lift must have actually moved the head");

    // The ancestry object under test — present on the remote (dev pushed it via the first lift),
    // genuinely absent locally going into the run.
    std::fs::remove_file(object_store_path(&area, "dev", &h1)).unwrap();
    // The current head must go missing too — see the doc comment above on why, or pass 1 never
    // even looks at H1.
    std::fs::remove_file(object_store_path(&area, "dev", &h2)).unwrap();

    // The leaf: unrecoverable from either side, on purpose (see the doc comment above).
    std::fs::remove_file(object_store_path(&area, "dev", &leaf)).unwrap();
    std::fs::remove_file(object_store_path(&area, "server-root", &leaf)).unwrap();

    let relative_leaf = loose_object_relative_path(&leaf);
    // Planted on disk via the durable-taint helper, not through the in-process gate — same
    // discipline as the other tests in this file.
    plant_taint(&area, "dev", &[&relative_leaf]);

    let status = area.forklift("dev", &["--json", "heal"]);
    assert_eq!(status.status.code(), Some(21),
        "the leaf is genuinely unrecoverable and still referenced through H1's restored \
        ancestry — heal must report it dangling, never clear it: {}", stdout(&status));
    let value: serde_json::Value = serde_json::from_str(&stdout(&status)).unwrap();
    assert_eq!(value["error"]["code"], "durability_taint");
    assert!(value["error"]["message"].as_str().unwrap().contains(&leaf),
        "the still-dangling leaf must be named: {}", value);

    // Not falsely cleared: the leaf's own taint is still standing, exactly as recorded.
    assert_eq!(recorded_taint_paths(&area, "dev"), vec![relative_leaf],
        "the leaf's taint must still stand — a false clear would leave this empty");

    // The ancestor pass 1 needed to restore for the closure walk to see the leaf's reference at
    // all: confirm it actually landed, proving this is pass 1's own doing, not luck.
    assert!(object_present(&area, "dev", &h1), "pass 1 must have restored the missing ancestor");
    assert!(object_present(&area, "dev", &h2), "pass 1 must have restored the current head too");
}

#[test]
fn heal_driven_refetch_treats_a_locally_repacked_object_as_recovered() {
    // §3.2 D2: the post-fetch presence check must accept "present in a pack" as recovered for a
    // recorded *loose* path, not just a bare loose read-back. A remote-fetched object always
    // lands loose on the client regardless of how the remote itself stores it (the wire format
    // is one compressed blob either way), so the shape D2 actually protects is local: an object
    // `compact` repacked out from under a standing taint's recorded loose path — the pre-existing
    // I4 mechanism (`file_utils::raw_object_present`, packed-or-loose) `resolve_the_rest`'s
    // raw-presence filter already used before this slice. This test drives it with a remote
    // configured (unlike the equivalent `heal_utils.rs` unit test), pinning that §3.2's new
    // refetch step does not regress it.
    //
    // Mutation: weaken the raw-presence filter to a bare loose read-back (drop the pack check) →
    // the object re-reports "vanished" → red.
    let area = TestArea::new("heal-refetch-packed");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/a.txt", "content that gets repacked locally\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base parcel"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let blob = path_object_hash(&area, "dev", &head, "a.txt");
    let relative = loose_object_relative_path(&blob);

    // Repack while nothing is tainted yet — `compact` needs a clean existence check.
    assert_success(&area.forklift("dev", &["compact"]));
    assert!(!object_store_path(&area, "dev", &blob).exists(),
        "sanity: the loose copy is gone after compact");

    plant_taint(&area, "dev", &[&relative]);

    let status = area.forklift("dev", &["--json", "heal"]);
    assert_success(&status);
    let value: serde_json::Value = serde_json::from_str(&stdout(&status)).unwrap();
    // A `RecoveredPacked` verdict (I4) is reported by its recorded *path*, not its bare hash
    // (`recovery_utils::HealOutcome.resolved` formats `attempt.recovered_packed` — a set of
    // `PathBuf`s — via `display_paths`; contrast the ordinary vanished-and-unreferenced case
    // reported by hash in the other tests in this file). It also resolves via `run`'s own
    // `attempt.all_clean()` fast path here (restage_object's own I4 check catches it before
    // `resolve_the_rest` even runs), which is why the mutation for this test lives in
    // `heal_utils::restage_object`'s ENOENT branch, not `resolve_the_rest`'s raw-presence filter
    // — this test's own contribution is confirming that fast path still fires correctly with a
    // remote configured and the new refetch machinery wired in around it.
    assert!(
        value["data"]["resolved"].as_array().unwrap().iter().any(|entry| entry == &relative),
        "the pack-recovered path must be reported resolved: {}", value
    );
    assert!(recorded_taint_paths(&area, "dev").is_empty(), "the taint must be fully cleared");
}

#[test]
fn heal_driven_refetch_leaves_the_taint_intact_on_a_failed_fetch() {
    // §3.2 D3: a fetch that errors (here, an unreachable remote — the same deterministic
    // failure shape `expand_recovers_after_an_unreachable_remote_leaves_the_scope_unchanged`
    // uses, no real network flakiness) must leave the remainder exactly as it was; `heal` must
    // never partial-clear on the strength of merely having *attempted* a fetch.
    //
    // Mutation: treat "the fetch attempt ran without a hard panic" as grounds to clear, instead
    // of re-confirming each candidate hash via `raw_object_present` → the taint clears despite
    // nothing having actually landed → red.
    let area = TestArea::new("heal-refetch-unreachable");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/a.txt", "content the remote genuinely has\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base parcel"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let blob = path_object_hash(&area, "dev", &head, "a.txt");
    let relative = loose_object_relative_path(&blob);

    std::fs::remove_file(object_store_path(&area, "dev", &blob)).unwrap();

    // Point at an unreachable remote (a closed local port) so the fetch itself fails, before any
    // taint exists yet — planting the taint against an already-broken remote configuration.
    assert_success(&area.forklift("dev", &["config", "remote.url", "http://127.0.0.1:9"]));
    plant_taint(&area, "dev", &[&relative]);

    let status = area.forklift("dev", &["--json", "heal"]);
    assert_eq!(status.status.code(), Some(21));

    assert_eq!(recorded_taint_paths(&area, "dev"), vec![relative],
        "the remainder must be exactly unchanged — nothing was actually recovered");
    assert!(!object_store_path(&area, "dev", &blob).exists(),
        "nothing was actually fetched by the failed attempt");
}

#[test]
fn heal_names_an_unreadable_fetch_scope_instead_of_blaming_connectivity() {
    // A fetch scope this build refuses skips the user pallets' history fetches. Re-running heal
    // cannot fix that, so the remedy must name the scope file and its way out, not only the
    // "once connectivity is restored" wording a transient failure gets, and must keep the
    // reproduce-it remedy, since the remote was still asked for the object by hash.
    //
    // Mutation: map the skipped fetch to `ConsultedWithErrors` → the remedy never names the
    // scope file → red.
    let area = TestArea::new("heal-refetch-unreadable-scope");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/a.txt", "content the remote has\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base parcel"]));
    assert_success(&area.forklift("dev", &["lift"]));

    // A second parcel the remote never saw, so no pass can bring its blob back.
    area.write_file("dev/b.txt", "content only this warehouse ever had\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "local parcel"]));

    let head = pallet_head(&area, "dev", "main");
    let blob = path_object_hash(&area, "dev", &head, "b.txt");
    let relative = loose_object_relative_path(&blob);

    std::fs::remove_file(object_store_path(&area, "dev", &blob)).unwrap();
    area.write_file("dev/.forklift/config/fetch-scope", "src\n");
    plant_taint(&area, "dev", &[&relative]);

    let status = area.forklift("dev", &["heal"]);
    assert_eq!(status.status.code(), Some(21));

    let output = format!("{}{}", stdout(&status), stderr(&status));
    assert!(output.contains("fetch-scope") && output.contains("older version of Forklift"),
        "the remedy must name the unreadable scope file: {}", output);
    assert!(output.contains("Franchise the remote again"),
        "the remedy must name the way out: {}", output);
    assert!(output.contains("checked the configured remote") && output.contains("\"forklift load\" then \"forklift stack\""),
        "the remote was still asked by hash, and reproducing it is still a way out: {}", output);
}

#[test]
fn heal_driven_refetch_recovers_a_hash_referenced_only_by_a_non_current_pallet() {
    // §3.2 D1: heal must not restrict its recovery to the current pallet's own history — a
    // vanished hash can be referenced only by a *different*, non-current pallet's published
    // head (the closure walk already treats every pallet head as a durable ref root; the
    // refetch must reach as far as that walk does).
    //
    // Mutation: remove the heal-driven refetch entirely → the object belongs to neither the
    // current pallet's tree nor anything else this warehouse would otherwise fetch → `heal`
    // refuses → red. (In this implementation the targeted, hash-addressed second pass is what
    // actually recovers it — see `attempt_heal_driven_refetch`'s doc comment: it fetches by hash
    // directly, so it does not need to know which pallet a hash belongs to at all, which
    // subsumes rather than narrows D1's own "every pallet head" goal.)
    let area = TestArea::new("heal-refetch-cross-pallet");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/main.txt", "main content\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "main parcel"]));
    assert_success(&area.forklift("dev", &["lift"]));

    // A second, published pallet: its head is a durable ref (a closure-walk root) exactly like
    // `main`'s, but it is never the *current* pallet when `heal` runs below.
    assert_success(&area.forklift("dev", &["palletize", "other"]));
    area.write_file("dev/other.txt", "other content\n");
    assert_success(&area.forklift("dev", &["load", "other.txt"]));
    assert_success(&area.forklift("dev", &["stack", "other parcel"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let other_head = pallet_head(&area, "dev", "other");
    let other_blob = path_object_hash(&area, "dev", &other_head, "other.txt");
    let relative = loose_object_relative_path(&other_blob);

    // Back to `main` before the object vanishes — `heal` runs from here, not from `other`.
    assert_success(&area.forklift("dev", &["shift", "main"]));

    std::fs::remove_file(object_store_path(&area, "dev", &other_blob)).unwrap();
    plant_taint(&area, "dev", &[&relative]);

    let status = area.forklift("dev", &["--json", "heal"]);
    assert_success(&status);
    let value: serde_json::Value = serde_json::from_str(&stdout(&status)).unwrap();
    assert!(
        value["data"]["resolved"].as_array().unwrap().iter().any(|entry| entry == &other_blob),
        "a hash referenced only by a non-current pallet must still be recovered: {}", value
    );
    assert!(recorded_taint_paths(&area, "dev").is_empty());
}

#[test]
fn heal_driven_refetch_force_fetches_a_corrupt_present_object() {
    // §3.2 D4: a corrupt-present recorded path (bytes present, but wrong — a hash mismatch) is
    // never picked up by an ordinary dedup-aware fetch, since `does_object_exist` already says
    // "yes" for it. `heal` must delete-then-fetch it: force it to genuinely vanish first, so the
    // ordinary fetch then pulls a good copy in behind it.
    //
    // Mutation: skip the delete-then-fetch bridge (attempt the refetch with the corrupt bytes
    // still in place) → the dedup check skips it as "already present" → it never gets a chance
    // to recover and stays in the remainder, still corrupt → red.
    let area = TestArea::new("heal-refetch-corrupt");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    let genuine_content = "the genuine content, already published to the remote\n";
    area.write_file("dev/a.txt", genuine_content);
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base parcel"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let blob = path_object_hash(&area, "dev", &head, "a.txt");
    let relative = loose_object_relative_path(&blob);
    let object_path = object_store_path(&area, "dev", &blob);

    // Corrupt the local copy in place (not valid zstd at all) — the remote's own copy is
    // untouched and good.
    std::fs::write(&object_path, b"not the real compressed bytes at all").unwrap();

    plant_taint(&area, "dev", &[&relative]);

    let status = area.forklift("dev", &["--json", "heal"]);
    assert_success(&status);
    let value: serde_json::Value = serde_json::from_str(&stdout(&status)).unwrap();
    // A recovered D4 corrupt candidate is reported by its recorded *path* (see
    // `resolve_the_rest`'s corrupt-candidate loop, which pushes `relative`, not the hash —
    // mirroring the `shard_vanished`/`RecoveredPacked` convention, not the ordinary
    // vanished-and-unreferenced one).
    assert!(
        value["data"]["resolved"].as_array().unwrap().iter().any(|entry| entry == &relative),
        "the force-fetched path must be reported resolved: {}", value
    );
    assert!(recorded_taint_paths(&area, "dev").is_empty(), "the taint must be fully cleared");

    // The good content is actually back — not merely "present", but correct.
    let content = {
        let _scope = forklift_core::globals::StorageRootScope::enter(&area.path("dev"));
        forklift_core::util::object_utils::load_blob(&blob).expect("the restored object must load").content
    };
    assert_eq!(content, genuine_content.as_bytes());
}

#[test]
fn a_failed_handshake_leaves_no_target_directory_behind() {
    // The historical defect, reproduced live against a real remote: franchise created
    // `.forklift`/`.forkliftignore` in the target directory nine lines before the first thing
    // that talks to the remote (`RemoteClient::fetch_info`), so a wrong token left a wedged,
    // half-initialized directory behind. The handshake must now happen before any filesystem
    // mutation, so a bad token must not even create the target directory.
    let area = TestArea::new("failed-handshake-no-leftover");
    let server = Server::start(&area, Some("right-token"));

    let dir = area.path("attempt1");
    assert!(!dir.exists());

    let failed = area.forklift(".", &[
        "franchise", &server.url, "attempt1", "--token", "wrong-token",
    ]);
    assert!(!failed.status.success(), "a bad token must fail the franchise");
    assert!(stderr(&failed).contains("401"), "{}", stderr(&failed));

    assert!(!dir.exists(),
        "a franchise whose handshake failed must not create the target directory at all: {:?}", dir);
}

#[test]
fn a_retry_after_a_failed_handshake_succeeds_into_the_same_directory() {
    // Pins the actual user-facing bug (verbatim from the field): before the fix, a franchise
    // whose handshake failed had already dirtied the target with .forklift/.forkliftignore, so
    // an immediate retry with the correct token into the *same* directory was refused — "...is
    // not empty" — even though nothing had ever synced there.
    let area = TestArea::new("retry-after-failed-handshake");
    let server = Server::start(&area, Some("right-token"));

    prepare_warehouse(&area, "dev", &server.url);
    assert_success(&area.forklift("dev", &["config", "remote.token", "right-token"]));
    area.write_file("dev/code.txt", "v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let dir = area.path("attempt1");
    assert!(!dir.exists());

    // First attempt: wrong token, the handshake fails.
    let failed = area.forklift(".", &[
        "franchise", &server.url, "attempt1", "--token", "wrong-token",
    ]);
    assert!(!failed.status.success());
    assert!(stderr(&failed).contains("401"), "{}", stderr(&failed));

    // Immediate retry, same directory, correct token: must succeed, not "is not empty".
    let retried = area.forklift(".", &[
        "franchise", &server.url, "attempt1", "--token", "right-token",
    ]);
    assert_success(&retried);
    assert_eq!(area.read_file("attempt1/code.txt"), "v1\n");
}

/// A remote with `src/api/a.txt` and `src/web/b.txt` stacked and lifted from "dev".
fn remote_with_two_subtrees(area: &TestArea, server: &Server) {
    prepare_warehouse(area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/b.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));
}

#[test]
fn a_failed_franchise_keeps_its_directory_and_a_corrected_resume_finishes() {
    // "src/ap" (missing the final "i") names nothing in the head: refused only after the
    // handshake and the scoped history fetch, by which point objects are in the store.
    let area = TestArea::new("franchise-resume-corrected");
    let server = Server::start(&area, None);
    remote_with_two_subtrees(&area, &server);

    let failed = area.forklift(".", &["--json", "franchise", &server.url, "typo", "--only", "src/ap"]);
    assert_eq!(failed.status.code(), Some(30), "{}{}", stdout(&failed), stderr(&failed));
    let value = json_of(&failed);
    assert_eq!(value["error"]["code"], "franchise_incomplete", "{}", value);
    assert_eq!(value["error"]["details"]["cause_code"], "only_prefix_not_found", "{}", value);
    assert_eq!(value["error"]["details"]["retry"], "change", "{}", value);
    assert_eq!(value["error"]["details"]["directory"], "typo", "{}", value);
    assert_eq!(value["error"]["details"]["remote"], server.url.as_str(), "{}", value);
    assert!(value["error"]["message"].as_str().unwrap().contains("\"src/ap\" is not a directory"), "{}", value);

    assert!(area.path("typo/.forklift/franchise").is_file(), "the record must stand until the franchise finishes");
    assert!(area.path("typo/.forklift/objects").is_dir(), "what was fetched must be kept");

    // The corrected resume needs no other options and finishes in place.
    let resumed = area.forklift(".", &["--json", "franchise", &server.url, "typo", "--only", "src/api"]);
    assert_success(&resumed);
    assert_eq!(area.read_file("typo/src/api/a.txt"), "api v1\n");
    assert!(!area.path("typo/src/web").exists(), "the resume franchises at its own scope");
    assert!(!area.path("typo/.forklift/franchise").exists(), "a finished franchise clears its record");
    assert_success(&area.forklift("typo", &["status"]));
}

#[test]
fn a_resume_keeps_the_objects_an_earlier_attempt_fetched() {
    // A full franchise of the same remote fetches every object loose (the test server has no
    // bundle). A resume after a sparse attempt fetched the spine fetches only what it lacks.
    let area = TestArea::new("franchise-resume-keeps-objects");
    let server = Server::start(&area, None);
    remote_with_two_subtrees(&area, &server);

    let fresh = area.forklift(".", &["--json", "franchise", &server.url, "fresh"]);
    assert_success(&fresh);
    let fresh_fetched = json_of(&fresh)["data"]["fetched_objects"].as_u64().unwrap();

    let failed = area.forklift(".", &["franchise", &server.url, "kept", "--only", "src/ap"]);
    assert_eq!(failed.status.code(), Some(30), "{}{}", stdout(&failed), stderr(&failed));

    let resumed = area.forklift(".", &["--json", "franchise", &server.url, "kept"]);
    assert_success(&resumed);
    let resumed_fetched = json_of(&resumed)["data"]["fetched_objects"].as_u64().unwrap();

    assert!(resumed_fetched < fresh_fetched,
        "the resume must not refetch what the first attempt stored: {} vs {}", resumed_fetched, fresh_fetched);
    assert_eq!(area.read_file("kept/src/api/a.txt"), "api v1\n");
    assert_eq!(area.read_file("kept/src/web/b.txt"), "web v1\n");
    assert_success(&area.forklift("kept", &["status"]));
}

#[test]
fn every_other_command_refuses_inside_an_unfinished_franchise() {
    let area = TestArea::new("franchise-incomplete-gate");
    let server = Server::start(&area, None);
    remote_with_two_subtrees(&area, &server);

    let failed = area.forklift(".", &["franchise", &server.url, "half", "--only", "src/ap"]);
    assert_eq!(failed.status.code(), Some(30), "{}{}", stdout(&failed), stderr(&failed));

    for args in [&["--json", "status"][..], &["--json", "heal"], &["--json", "prepare"], &["--json", "audit"]] {
        let refused = area.forklift("half", args);
        assert_eq!(refused.status.code(), Some(30), "{:?}: {}{}", args, stdout(&refused), stderr(&refused));
        let value = json_of(&refused);
        assert_eq!(value["error"]["code"], "franchise_incomplete", "{:?}: {}", args, value);
        assert_eq!(value["error"]["details"]["cause_code"], "only_prefix_not_found", "{:?}: {}", args, value);
        assert!(value["error"]["next_step"].as_str().unwrap().contains("--only src/ap"), "{:?}: {}", args, value);
    }

    // From a folder inside the working directory too.
    std::fs::create_dir_all(area.path("half/sub")).unwrap();
    let nested = area.forklift("half/sub", &["status"]);
    assert_eq!(nested.status.code(), Some(30), "{}{}", stdout(&nested), stderr(&nested));
}

/// A franchise stopped before its warehouse marker was written is still an unfinished franchise,
/// never a folder that is not a warehouse: the franchise check comes before the marker's.
#[test]
fn an_unfinished_franchise_without_a_warehouse_marker_is_still_franchise_incomplete() {
    let area = TestArea::new("franchise-incomplete-unmarked");
    let server = Server::start(&area, None);
    remote_with_two_subtrees(&area, &server);

    let failed = area.forklift(".", &["franchise", &server.url, "half", "--only", "src/ap"]);
    assert_eq!(failed.status.code(), Some(30), "{}{}", stdout(&failed), stderr(&failed));
    let _ = std::fs::remove_file(area.path("half/.forklift/warehouse"));
    assert!(!area.path("half/.forklift/warehouse").exists() && area.path("half/.forklift/franchise").is_file());

    for args in [&["--json", "status"][..], &["--json", "prepare"]] {
        let refused = area.forklift("half", args);
        let value = json_of(&refused);
        assert_eq!(value["error"]["code"], "franchise_incomplete", "{:?}: {}", args, value);
    }
}

#[test]
fn a_resume_of_another_remote_is_refused_before_it_connects() {
    let area = TestArea::new("franchise-resume-other-remote");
    let server = Server::start(&area, None);
    remote_with_two_subtrees(&area, &server);

    let failed = area.forklift(".", &["franchise", &server.url, "half", "--only", "src/ap"]);
    assert_eq!(failed.status.code(), Some(30), "{}{}", stdout(&failed), stderr(&failed));
    let record = std::fs::read(area.path("half/.forklift/franchise")).unwrap();

    // Nothing listens there: a refusal that names the recorded remote proves no connection was tried.
    let refused = area.forklift(".", &["--json", "franchise", "http://127.0.0.1:9", "half"]);
    assert_eq!(refused.status.code(), Some(30), "{}{}", stdout(&refused), stderr(&refused));
    let value = json_of(&refused);
    assert!(value["error"]["message"].as_str().unwrap().contains("not of http://127.0.0.1:9"), "{}", value);
    assert_eq!(std::fs::read(area.path("half/.forklift/franchise")).unwrap(), record, "the record is untouched");

    // A trailing slash is the same remote.
    let resumed = area.forklift(".", &["franchise", &format!("{}/", server.url), "half"]);
    assert_success(&resumed);
}

#[test]
fn a_failed_franchise_into_a_nested_target_keeps_it_and_its_ancestors() {
    let area = TestArea::new("franchise-nested-kept");
    let server = Server::start(&area, None);
    remote_with_two_subtrees(&area, &server);

    let failed = area.forklift(".", &["franchise", &server.url, "a/b/c", "--only", "src/ap"]);
    assert_eq!(failed.status.code(), Some(30), "{}{}", stdout(&failed), stderr(&failed));
    assert!(area.path("a/b/c/.forklift/franchise").is_file());

    assert_success(&area.forklift(".", &["franchise", &server.url, "a/b/c"]));
    assert_eq!(area.read_file("a/b/c/src/web/b.txt"), "web v1\n");
}

#[test]
fn a_pre_existing_target_keeps_a_failed_franchise_and_a_non_empty_one_is_refused() {
    let area = TestArea::new("franchise-preexisting-kept");
    let server = Server::start(&area, None);
    remote_with_two_subtrees(&area, &server);

    std::fs::create_dir_all(area.path("empty")).unwrap();
    let failed = area.forklift(".", &["franchise", &server.url, "empty", "--only", "src/ap"]);
    assert_eq!(failed.status.code(), Some(30), "{}{}", stdout(&failed), stderr(&failed));
    assert_success(&area.forklift(".", &["franchise", &server.url, "empty"]));

    // A directory holding anything franchise did not put there is never admitted.
    area.write_file("busy/notes.txt", "mine\n");
    let refused = area.forklift(".", &["franchise", &server.url, "busy"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("is not empty"), "{}", stderr(&refused));
    assert_eq!(area.read_file("busy/notes.txt"), "mine\n");

    // Nor is a finished warehouse: a franchise that completed is not resumed over.
    let again = area.forklift(".", &["franchise", &server.url, "empty"]);
    assert!(!again.status.success());
    assert!(stderr(&again).contains("is not empty"), "{}", stderr(&again));
}

#[test]
fn a_pallet_typo_is_refused_before_anything_is_created() {
    // PR #84 review, finding 6: a --pallet typo is checked against the handshake's own answer
    // (info.pallets) *before* anything is fetched or mutated — unlike the --only tree check
    // (which genuinely needs the fetched tree and cannot move this early), the handshake already
    // carries the full pallet map, so this is a pure in-memory check. Refused this early, the
    // target directory is never even created — strictly stronger than "cleaned up after a
    // failure", and it means a typo costs nothing to retry.
    let area = TestArea::new("pallet-typo-no-leftover");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/code.txt", "v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let dir = area.path("typo-pallet");
    assert!(!dir.exists());

    let failed = area.forklift(".", &[
        "franchise", &server.url, "typo-pallet", "--pallet", "does-not-exist",
    ]);
    assert!(!failed.status.success(), "a --pallet typo must be refused");
    assert!(stderr(&failed).contains("no pallet \"does-not-exist\""), "{}", stderr(&failed));

    assert!(!dir.exists(),
        "a --pallet typo must be refused before the target directory is ever created: {:?}", dir);
}

#[test]
fn a_pallet_typo_is_refused_before_a_broken_bundle_would_otherwise_fail_first() {
    // Pins the *ordering* specifically, which the previous test does not: "the pallet typo is
    // refused" holds whether the pallet is checked before or after the fetch, so that assertion
    // alone cannot tell the two apart. Here
    // the remote's bundle is deliberately corrupted — a recognized header, an unparseable
    // payload — so importing it is a hard, unrelated failure. With the pallet check moved before
    // any fetch, a --pallet typo is reported and the broken bundle is never even downloaded.
    // Before this fix, the bundle install ran first in `franchise_into_prepared_target` and its
    // own hard failure would have been reported instead, masking the pallet typo entirely.
    let area = TestArea::new("pallet-typo-before-broken-bundle");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/code.txt", "v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    server.rebuild_bundle(&area);

    // A recognized header (so it is not gracefully skipped as an unsupported version) over a
    // garbage payload with no digest (so import refuses it outright: a hard failure, not
    // "nothing to import").
    let bundle_path = area.path("server-root/.forklift/bundles/latest");
    std::fs::write(&bundle_path, format!(
        "{}\nthis is not a valid pack section table",
        forklift_core::util::bundle_utils::BUNDLE_HEADER
    )).unwrap();

    let dir = area.path("typo-vs-broken-bundle");

    let failed = area.forklift(".", &[
        "franchise", &server.url, "typo-vs-broken-bundle", "--pallet", "does-not-exist",
    ]);
    assert!(!failed.status.success());
    assert!(stderr(&failed).contains("no pallet \"does-not-exist\""),
        "the pallet typo must be reported, not the (also broken) bundle: {}", stderr(&failed));
    assert!(!dir.exists());
}

#[test]
fn franchise_ignores_an_unrelated_ambient_warehouses_tor_setting() {
    // PR #84 review, finding 5: moving the handshake before the cwd switch means it now runs in
    // whatever directory the operator happened to invoke franchise from — which may be a
    // pre-existing, unrelated warehouse with its own `remote.tor`/`remote.torProxy`. Inheriting
    // that warehouse's Tor policy for a brand-new clone elsewhere would be a cwd-dependent,
    // security-relevant surprise, so the initial handshake reads *global* configuration only.
    //
    // "outer" sets remote.tor = on (route every remote through Tor) and remote.torProxy to a
    // port nothing listens on, at WAREHOUSE scope. If the initial handshake ever inherited this,
    // routing the plain http:// test server through that dead proxy would make it unreachable.
    let area = TestArea::new("franchise-tor-scope-independent");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/code.txt", "v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    std::fs::create_dir_all(area.path("outer")).unwrap();
    assert_success(&area.forklift("outer", &["prepare"]));
    assert_success(&area.forklift("outer", &["config", "remote.tor", "on"]));
    assert_success(&area.forklift("outer", &["config", "remote.torProxy", "socks5h://127.0.0.1:9"]));

    // Run from inside "outer": the plain http:// test server must still be reached directly.
    let franchised = area.forklift("outer", &["franchise", &server.url, "sub"]);
    assert_success(&franchised);
    assert_eq!(area.read_file("outer/sub/code.txt"), "v1\n");
}

#[test]
fn a_failure_after_the_bundle_install_keeps_it_and_the_resume_skips_the_download() {
    // A non-sparse franchise installs the remote's bundle as native packs and reads them in this
    // same process (adopting trust walks the enrolled office's history, which the bundle covers),
    // so they are mapped when a later step fails. The trigger is a second parcel the
    // already-built bundle does not cover, whose object the remote then no longer has: the
    // incremental walk fails *after* the bundle is installed and read. The finished import is
    // remembered, and a resume once the remote is whole again does not download it a second time.
    let area = TestArea::new("post-handshake-cleanup-with-packs");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/a.txt", "first parcel\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "pre-trust base"]));
    assert_success(&area.forklift("dev", &["office", "enroll"]));

    // Built now: covers the office enrollment and this first parcel only.
    server.rebuild_bundle(&area);

    // A second parcel, lifted after the bundle was built — the bundle cannot cover it.
    area.write_file("dev/b.txt", "second parcel, not in the bundle\n");
    assert_success(&area.forklift("dev", &["load", "b.txt"]));
    assert_success(&area.forklift("dev", &["stack", "second parcel"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let head = pallet_head(&area, "dev", "main");
    let blob_b = path_object_hash(&area, "dev", &head, "b.txt");

    // The remote's own copy is now genuinely gone: whatever the incremental walk retries, it
    // cannot recover an object the far end no longer has.
    std::fs::remove_file(object_store_path(&area, "server-root", &blob_b)).unwrap();

    let failed = area.forklift(".", &["franchise", &server.url, "packed"]);
    assert_eq!(failed.status.code(), Some(30),
        "franchise must fail when a required object has vanished from the remote: {}", stderr(&failed));
    assert!(area.read_file("packed/.forklift/franchise").contains("bundle=imported"),
        "{}", area.read_file("packed/.forklift/franchise"));

    let blob = object_store_path(&area, "dev", &blob_b);
    std::fs::copy(&blob, object_store_path(&area, "server-root", &blob_b)).unwrap();

    let resumed = area.forklift(".", &["--json", "franchise", &server.url, "packed"]);
    assert_success(&resumed);
    let value = json_of(&resumed);
    assert!(value["data"].get("bundle_objects").is_none(), "the imported bundle is kept, not fetched again: {}", value);
    assert_eq!(area.read_file("packed/b.txt"), "second parcel, not in the bundle\n");
}

#[test]
fn franchise_into_an_absolute_target_path_succeeds() {
    // PR #84 review round 3, finding 1: `claim_target`'s round-2 shape walked path components
    // one at a time via `std::fs::create_dir`, tolerating only `ErrorKind::AlreadyExists`. On an
    // absolute path, `Path::components()` yields a `Prefix`/`RootDir` pair before the first real
    // component (on Windows: `Prefix("C:")` then `RootDir`, building up "C:\" as the second
    // `current` the loop tried to create); `CreateDirectoryW` on a drive root (or a UNC share
    // root) returns `PermissionDenied`, not `AlreadyExists`, so that loop refused every absolute
    // Windows target before ever contacting the remote.
    //
    // Every other franchise test in this file passes a relative target, which cannot exercise
    // this at all: POSIX's `mkdir` on an existing root or intermediate directory (`/`, `/Users`,
    // …) always reports `EEXIST`, so this defect shape is invisible on macOS/Linux CI — the
    // reviewer verified this directly. This test is the missing axis; its ability to discriminate
    // the fix is Windows-specific in the same way the mmap-registry probe's was (see
    // `pack_utils::an_mmapd_pack_does_not_block_its_own_directory_removal`), and was confirmed
    // red (against the round-2 per-component `claim_target`, temporarily reinstated) and green
    // (against the current one) on `windows-latest` directly — see the PR discussion for both
    // runs, since this file's own suite cannot reproduce the distinction locally either way.
    //
    // `TestArea::path` already returns an absolute path (it's rooted at `std::env::temp_dir()`),
    // so no extra resolution is needed here — passing it straight to `franchise` as `directory`
    // is exactly the case that broke.
    let area = TestArea::new("franchise-absolute-target");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/code.txt", "v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));

    let absolute_target = area.path("cloned-absolute");
    assert!(absolute_target.is_absolute(), "the test itself must exercise an absolute path");

    let franchised = area.forklift(".", &[
        "franchise", &server.url, absolute_target.to_str().unwrap(),
    ]);
    assert_success(&franchised);
    assert_eq!(std::fs::read_to_string(absolute_target.join("code.txt")).unwrap(), "v1\n");
}

/// `lift` and `lower` report their phases on a `PULT_EVENTS` channel (the channel itself — fd
/// validation, a vanished reader — is covered in `tests/events.rs`).
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[path = "channel/mod.rs"]
mod channel;

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod progress_events {
    use super::*;

    /// Run the CLI in `relative_dir` with a fresh pipe as its channel, returning the output and
    /// the status lines read from it.
    fn forklift_with_events(area: &TestArea, relative_dir: &str, args: &[&str]) -> (Output, Vec<String>) {
        let mut command = forklift_command();
        command.args(args)
            .current_dir(area.path(relative_dir))
            .env("FORKLIFT_GLOBAL_CONFIG", area.path("area-global.toml"))
            .env("FORKLIFT_KEYS_DIR", area.path("shared-keys"));

        let (output, text) = channel::run_and_collect(&mut command);
        let statuses = text.lines()
            .filter_map(|line| line.strip_prefix("status ").map(str::to_string))
            .collect();

        (output, statuses)
    }

    #[test]
    fn lift_and_lower_report_their_phases() {
        let area = TestArea::new("events");
        let server = Server::start(&area, None);

        prepare_warehouse(&area, "a", &server.url);
        area.write_file("a/readme.txt", "hello\n");
        assert_success(&area.forklift("a", &["load", "."]));
        assert_success(&area.forklift("a", &["stack", "first parcel"]));

        // Uploading is followed by per-object progress, and updating the remote ends the run, so
        // neither can be replaced by the status after it.
        let (lifted, statuses) = forklift_with_events(&area, "a", &["lift"]);
        assert_success(&lifted);
        for phase in ["Uploading objects", "Updating the remote"] {
            assert!(statuses.iter().any(|status| status == phase), "missing {:?} in {:?}", phase, statuses);
        }

        assert_success(&area.forklift(".", &["franchise", &server.url, "b"]));

        area.write_file("a/readme.txt", "hello, twice\n");
        area.write_file("a/new.txt", "new\n");
        assert_success(&area.forklift("a", &["load", "."]));
        assert_success(&area.forklift("a", &["stack", "second parcel"]));
        assert_success(&area.forklift("a", &["lift"]));

        // Writing files is followed by per-file progress and the rebuild ends the run, so
        // neither can be replaced by the status after it.
        let (lowered, statuses) = forklift_with_events(&area, "b", &["lower"]);
        assert_success(&lowered);
        for phase in ["Writing files", "Rebuilding the inventory"] {
            assert!(statuses.iter().any(|status| status == phase), "missing {:?} in {:?}", phase, statuses);
        }
        assert_eq!(area.read_file("b/new.txt"), "new\n");
    }
}

/// A lower across a case-only rename removes the old spelling before it writes the new one. On a
/// case-insensitive file system the two name one file, so writing `Foo` first and removing `foo`
/// after would delete the file it just wrote. On a case-sensitive one the rename is ordinary.
#[test]
fn a_lower_across_a_case_only_rename_keeps_the_file() {
    let area = TestArea::new("case-only-rename");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/foo", "lower\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "first"]));
    assert_success(&area.forklift("a", &["lift"]));
    assert_success(&area.forklift(".", &["franchise", &server.url, "b"]));

    std::fs::remove_file(area.path("a/foo")).unwrap();
    area.write_file("a/Foo", "upper\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "rename foo to Foo"]));
    assert_success(&area.forklift("a", &["lift"]));

    assert_success(&area.forklift("b", &["lower"]));
    let names: Vec<String> = std::fs::read_dir(area.path("b")).unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(names.contains(&"Foo".to_string()) && !names.contains(&"foo".to_string()), "b holds {:?}", names);
    assert_eq!(area.read_file("b/Foo"), "upper\n");

    let status = area.forklift("b", &["stocktake"]);
    assert_success(&status);
    assert!(stdout(&status).contains("matches the inventory"), "{}", stdout(&status));
}

/// The full hash of a revision in a warehouse.
fn parcel_hash(area: &TestArea, dir: &str, revision: &str) -> String {
    let set = area.forklift(dir, &["--json", "kv", "list", revision]);
    assert_success(&set);
    let value: serde_json::Value = serde_json::from_str(&stdout(&set)).unwrap();

    value["data"]["subject"].as_str().expect("a scoped listing names its subject").to_string()
}

#[test]
fn kv_needs_an_office_and_refuses_readers() {
    let area = TestArea::new("kv-office");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/code.txt", "v1\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "base"]));

    // No office: a write is refused before anything is stacked.
    let refused = area.forklift("a", &["kv", "set", "main", "ci", "green"]);
    assert!(!refused.status.success(), "a kv write without an office must be refused");
    assert!(stderr(&refused).contains("office"), "{}", stderr(&refused));
    assert!(stdout(&area.forklift("a", &["kv", "list"])).contains("No custom metadata"));

    assert_success(&area.forklift("a", &["office", "enroll"]));

    // A reader may not write, though reading stays open to it.
    let reader = keygen_admit_args(&area, "reader@forklift");
    assert_success(&area.forklift("a", &[
        "office", "admit", &reader[0], &reader[1], &reader[2], "--role", "reader",
    ]));
    assert_success(&area.forklift("a", &["kv", "set", "main", "ci", "green"]));

    assert_success(&area.forklift("a", &["config", "operator.identifier", "reader@forklift"]));
    let refused = area.forklift("a", &["kv", "set", "main", "ci", "red"]);
    assert!(!refused.status.success(), "a reader's kv write must be refused");
    assert!(stderr(&refused).contains("reader"), "{}", stderr(&refused));
    let refused = area.forklift("a", &["kv", "unset", "main", "ci"]);
    assert!(!refused.status.success(), "a reader's kv unset must be refused");
    assert_eq!(stdout(&area.forklift("a", &["kv", "get", "main", "ci"])).trim(), "green");

    // Nor may it deliver, which writes a pallet and a manifest entry: the target stays put.
    assert_success(&area.forklift("a", &["config", "--unset", "operator.identifier"]));
    assert_success(&area.forklift("a", &["palletize", "draft"]));
    area.write_file("a/code.txt", "v2\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "draft work"]));
    let main_head = pallet_head(&area, "a", "main");

    assert_success(&area.forklift("a", &["config", "operator.identifier", "reader@forklift"]));
    let refused = area.forklift("a", &["deliver", "main"]);
    assert!(!refused.status.success(), "a reader's deliver must be refused");
    assert!(stderr(&refused).contains("reader cannot deliver"), "{}", stderr(&refused));
    assert_eq!(pallet_head(&area, "a", "main"), main_head, "the target must not move");
}

#[test]
fn kv_syncs_replays_disjoint_changes_and_resolves_conflicts_with_keep() {
    let area = TestArea::new("kv-remote");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/code.txt", "v1\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "base"]));
    assert_success(&area.forklift("a", &["office", "enroll"]));

    let subject = parcel_hash(&area, "a", "main");

    // Set, read and lift — @kv rides along with the working pallet.
    assert_success(&area.forklift("a", &["kv", "set", "main", "ci", "green"]));
    assert_eq!(stdout(&area.forklift("a", &["kv", "get", "main", "ci"])).trim(), "green");
    let again = area.forklift("a", &["kv", "set", "main", "ci", "green"]);
    assert_success(&again);
    assert!(stdout(&again).contains("nothing to change"), "{}", stdout(&again));

    let lifted = area.forklift("a", &["lift"]);
    assert_success(&lifted);
    assert!(stdout(&lifted).contains("Lifted the @kv pallet"), "{}", stdout(&lifted));

    // A franchise carries it, readable by the full hash too.
    assert_success(&area.forklift(".", &["franchise", &server.url, "b"]));
    assert_eq!(stdout(&area.forklift("b", &["kv", "get", &subject, "ci"])).trim(), "green");

    // Disjoint divergence: B sets one key and lifts, A another without lowering. A's lift is
    // refused for @kv alone, and lowering replays A's change onto the remote's and lifts it.
    assert_success(&area.forklift("b", &["kv", "set", "main", "from-b", "b"]));
    assert_success(&area.forklift("b", &["lift"]));

    assert_success(&area.forklift("a", &["kv", "set", "main", "from-a", "a"]));
    let refused = area.forklift("a", &["lift"]);
    assert!(!refused.status.success(), "a diverged @kv lift must be refused");
    assert!(stderr(&refused).contains("lower"), "{}", stderr(&refused));
    // A refusal frame reaching the joined error would print its code as a word of the message.
    assert!(!stderr(&refused).contains("remote_ahead"), "a meta failure carries no refusal frame: {}", stderr(&refused));

    let lowered = area.forklift("a", &["lower"]);
    assert_success(&lowered);
    assert!(stdout(&lowered).contains("Replayed the local @kv changes"), "{}", stdout(&lowered));

    assert_success(&area.forklift("b", &["lower"]));
    for dir in ["a", "b"] {
        let listing = stdout(&area.forklift(dir, &["kv", "list", "main"]));
        assert!(listing.contains("from-a = a") && listing.contains("from-b = b") && listing.contains("ci = green"),
                "{}: {}", dir, listing);
    }

    // A conflict: both change the same key. Lowering lifts nothing for @kv and exits 24,
    // naming the path and both sides.
    assert_success(&area.forklift("b", &["kv", "set", "main", "ci", "red"]));
    assert_success(&area.forklift("b", &["lift"]));
    assert_success(&area.forklift("a", &["kv", "set", "main", "ci", "amber"]));

    let conflicted = area.forklift("a", &["--json", "lower"]);
    assert_eq!(conflicted.status.code(), Some(24), "{}", stdout(&conflicted));
    let value: serde_json::Value = serde_json::from_str(&stdout(&conflicted)).unwrap();
    assert_eq!(value["error"]["code"], "meta_conflict");
    let pallets = &value["error"]["details"]["pallets"];
    assert_eq!(pallets[0]["pallet"], "@kv");
    assert_eq!(pallets[0]["conflicts"][0]["path"], format!("{}/ci", subject));
    assert_eq!(pallets[0]["conflicts"][0]["ours"]["value"], "amber");
    assert_eq!(pallets[0]["conflicts"][0]["theirs"]["value"], "red");
    assert_eq!(stdout(&area.forklift("a", &["kv", "get", "main", "ci"])).trim(), "amber",
               "a conflicted lower leaves the local @kv as it was");

    // Keeps that cannot apply are refused: a pallet without resolutions, a malformed side.
    let refused = area.forklift("a", &["lower", "--keep", "@office:main=ours"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("takes no resolutions"), "{}", stderr(&refused));
    let refused = area.forklift("a", &["lower", "--keep", &format!("@kv:{}/ci=mine", subject)]);
    assert!(!refused.status.success());

    // A meta pallet is spelled with its "@", as everywhere else: the bare name is refused and
    // the refusal names the spelling that works, though this keep would otherwise resolve.
    let bare = area.forklift("a", &["lower", "--keep", &format!("kv:{}/ci=ours", subject)]);
    assert!(!bare.status.success(), "a keep naming the pallet without its @ must be refused");
    assert!(stderr(&bare).contains(&format!("--keep @kv:{}/ci=ours", subject)), "{}", stderr(&bare));
    assert_eq!(stdout(&area.forklift("b", &["kv", "get", "main", "ci"])).trim(), "red",
               "nothing was lifted for the refused keep");

    // Keeping ours lifts the local value over the remote's.
    let kept = area.forklift("a", &["lower", "--keep", &format!("@kv:{}/ci=ours", subject)]);
    assert_success(&kept);
    assert!(stdout(&kept).contains("Replayed"), "{}", stdout(&kept));
    assert_success(&area.forklift("b", &["lower"]));
    assert_eq!(stdout(&area.forklift("b", &["kv", "get", "main", "ci"])).trim(), "amber");

    // Keeping theirs takes the remote's value.
    assert_success(&area.forklift("b", &["kv", "set", "main", "ci", "blue"]));
    assert_success(&area.forklift("b", &["lift"]));
    assert_success(&area.forklift("a", &["kv", "set", "main", "ci", "violet"]));
    let kept = area.forklift("a", &["lower", "--keep", &format!("@kv:{}/ci=theirs", subject)]);
    assert_success(&kept);
    assert_eq!(stdout(&area.forklift("a", &["kv", "get", "main", "ci"])).trim(), "blue");

    // A keep for a pallet that did not diverge is refused rather than silently ignored.
    let stale = area.forklift("a", &["lower", "--keep", &format!("@kv:{}/ci=theirs", subject)]);
    assert!(!stale.status.success(), "a keep with nothing to resolve must be refused");

    // On a pallet only this side moved, theirs drops the change it names and replays the rest.
    assert_success(&area.forklift("a", &["kv", "set", "main", "drop", "x"]));
    assert_success(&area.forklift("a", &["kv", "set", "main", "stay", "y"]));
    let kept = area.forklift("a", &["lower", "--keep", &format!("@kv:{}/drop=theirs", subject)]);
    assert_success(&kept);
    assert!(stdout(&kept).contains("Replayed"), "{}", stdout(&kept));
    assert!(!area.forklift("a", &["kv", "get", "main", "drop"]).status.success(), "the named change is dropped");
    assert_success(&area.forklift("b", &["lower"]));
    assert_eq!(stdout(&area.forklift("b", &["kv", "get", "main", "stay"])).trim(), "y",
               "the rest is replayed onto the remote");
    assert!(!area.forklift("b", &["kv", "get", "main", "drop"]).status.success());

    // Unset removes the key; the log shows every change, newest first.
    assert_success(&area.forklift("a", &["kv", "unset", "main", "ci"]));
    assert!(!area.forklift("a", &["kv", "get", "main", "ci"]).status.success());
    let log = stdout(&area.forklift("a", &["kv", "log", "main", "ci"]));
    let lines: Vec<&str> = log.lines().collect();
    assert!(lines[0].ends_with("removed") && lines[1].ends_with("= blue"), "{}", log);
    assert!(log.contains("= green"), "{}", log);
    assert_success(&area.forklift("a", &["lift"]));
}

#[test]
fn a_conflicted_kv_pallet_does_not_hold_back_the_working_pallet() {
    let area = TestArea::new("kv-isolation");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    area.write_file("a/code.txt", "v1\n");
    assert_success(&area.forklift("a", &["load", "."]));
    assert_success(&area.forklift("a", &["stack", "base"]));
    assert_success(&area.forklift("a", &["office", "enroll"]));
    assert_success(&area.forklift("a", &["kv", "set", "main", "ci", "green"]));
    assert_success(&area.forklift("a", &["lift"]));
    assert_success(&area.forklift(".", &["franchise", &server.url, "b"]));
    let base = parcel_hash(&area, "a", "main");

    // B moves the working pallet and @kv; A changes the same key without lowering.
    area.write_file("b/code.txt", "v2\n");
    assert_success(&area.forklift("b", &["load", "."]));
    assert_success(&area.forklift("b", &["stack", "second"]));
    assert_success(&area.forklift("b", &["kv", "set", &base, "ci", "red"]));
    assert_success(&area.forklift("b", &["lift"]));
    assert_success(&area.forklift("a", &["kv", "set", "main", "ci", "amber"]));

    // The lower still brings the working pallet down, then reports the @kv conflict, naming the
    // head the working pallet lowered to.
    let lowered = area.forklift("a", &["--json", "lower"]);
    assert_eq!(lowered.status.code(), Some(24), "{}{}", stdout(&lowered), stderr(&lowered));
    assert_eq!(area.read_file("a/code.txt"), "v2\n", "the working pallet must lower despite @kv");
    let value: serde_json::Value = serde_json::from_str(&stdout(&lowered)).unwrap();
    assert_eq!(value["error"]["details"]["lowered"]["pallet"], "main");
    assert_eq!(value["error"]["details"]["lowered"]["head"], parcel_hash(&area, "b", "main"));

    let again = area.forklift("a", &["lower"]);
    assert_eq!(again.status.code(), Some(24));
    assert!(stdout(&again).contains("ours") && stdout(&again).contains("theirs"), "{}", stdout(&again));

    // When the working pallet itself fails, its error leads and still names the conflict.
    assert_success(&area.forklift("a", &["palletize", "side"]));
    let failed = area.forklift("a", &["lower"]);
    assert!(!failed.status.success());
    assert_ne!(failed.status.code(), Some(24), "the working pallet's failure outranks the conflict");
    assert!(stderr(&failed).contains("no pallet") && stderr(&failed).contains("@kv conflicted"), "{}", stderr(&failed));
}

/// A bay's working directory holds a `.forklift` folder too, but `forklift-server` neither
/// prepares nor serves one as a warehouse: prepare would fill the bay's folder with warehouse
/// pieces, and serve would hand out a directory that is not a warehouse.
#[test]
fn the_server_neither_prepares_nor_serves_a_bay() {
    let area = TestArea::new("server-bay-root");
    let bay = area.path("bay");
    let forklift = bay.join(".forklift");
    std::fs::create_dir_all(&forklift).unwrap();
    std::fs::write(forklift.join("redirect"), "redirect").unwrap();
    let bay_str = bay.to_str().unwrap();

    let prepared = server_command().args(["prepare", "--root", bay_str]).output().unwrap();
    assert_eq!(std::fs::read_dir(&forklift).unwrap().count(), 1, "prepare must create nothing in the bay's folder");
    assert!(stderr(&prepared).contains("belongs to a bay"), "{}", stderr(&prepared));
    assert!(!prepared.status.success());

    // Waited on with a deadline: a server that took the bay for a warehouse would serve it forever.
    let mut serving = server_command().args(["serve", "--root", bay_str, "--addr", "127.0.0.1:0", "--open"])
        .stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while serving.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            let _ = serving.kill();
            let _ = serving.wait();
            panic!("forklift-server served a bay as a warehouse");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let served = serving.wait_with_output().unwrap();
    assert!(stderr(&served).contains("is a bay's working directory"), "{}", stderr(&served));
    assert!(!served.status.success());
}

// ---- Remote pallets: `lower <pallet>`, `palletize --remote`, the pallet_not_found refusal ----

/// Warehouse "a" lifts `main` (base.txt), "b" franchises it, then "a" lifts `feat` (feat.txt) and
/// stands on main again. "b" has no `feat` ref; returns the remote's feat head.
fn remote_with_a_side_pallet(area: &TestArea, server: &Server) -> String {
    prepare_warehouse(area, "a", &server.url);
    stack_file(area, "a", "base.txt", "base\n");
    assert_success(&area.forklift("a", &["lift"]));
    assert_success(&area.forklift(".", &["franchise", &server.url, "b"]));

    assert_success(&area.forklift("a", &["palletize", "feat"]));
    stack_file(area, "a", "feat.txt", "feat v1\n");
    assert_success(&area.forklift("a", &["lift"]));
    assert_success(&area.forklift("a", &["shift", "main"]));

    assert!(!area.path("b/.forklift/pallets/feat").exists());
    pallet_head(area, "a", "feat")
}

/// Stack `content` at `path` on `pallet` in "a" and lift it, leaving "a" on main.
fn advance_remote(area: &TestArea, pallet: &str, path: &str, content: &str) {
    assert_success(&area.forklift("a", &["shift", pallet]));
    stack_file(area, "a", path, content);
    assert_success(&area.forklift("a", &["lift"]));
    assert_success(&area.forklift("a", &["shift", "main"]));
}

fn json_of(output: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(output)).unwrap_or_else(|_| panic!("{}{}", stdout(output), stderr(output)))
}

// Revert: route a named lower through the working pallet's path → the bay moves onto feat.
#[test]
fn lowering_another_pallet_creates_its_ref_and_leaves_the_bay_alone() {
    let area = TestArea::new("lower-named-create");
    let server = Server::start(&area, None);
    let feat = remote_with_a_side_pallet(&area, &server);

    let position = area.read_file("b/.forklift/position");
    area.write_file("b/base.txt", "unloaded edit\n");

    let lowered = area.forklift("b", &["--json", "lower", "feat"]);
    assert_success(&lowered);
    let value = json_of(&lowered);
    assert_eq!(value["data"]["outcome"], "created", "{}", value);
    assert_eq!(value["data"]["pallet"], "feat", "{}", value);

    assert_eq!(pallet_head(&area, "b", "feat"), feat);
    assert_eq!(area.read_file("b/.forklift/position"), position, "the bay stays where it was");
    assert_eq!(area.read_file("b/base.txt"), "unloaded edit\n", "the bay's files are untouched");
    assert!(!area.path("b/feat.txt").exists());

    area.write_file("b/base.txt", "base\n");
    assert_success(&area.forklift("b", &["shift", "feat"]));
    assert_eq!(area.read_file("b/feat.txt"), "feat v1\n");
}

// Revert: drop the ahead check → the named lower moves b's feat back to the remote's head.
#[test]
fn lowering_another_pallet_fast_forwards_keeps_ahead_and_refuses_diverged() {
    let area = TestArea::new("lower-named-outcomes");
    let server = Server::start(&area, None);
    remote_with_a_side_pallet(&area, &server);
    assert_success(&area.forklift("b", &["lower", "feat"]));

    advance_remote(&area, "feat", "feat.txt", "feat v2\n");
    let lowered = area.forklift("b", &["--json", "lower", "feat"]);
    assert_success(&lowered);
    assert_eq!(json_of(&lowered)["data"]["outcome"], "lowered");
    assert_eq!(pallet_head(&area, "b", "feat"), pallet_head(&area, "a", "feat"));

    let again = area.forklift("b", &["--json", "lower", "feat"]);
    assert_eq!(json_of(&again)["data"]["outcome"], "up_to_date");

    assert_success(&area.forklift("b", &["shift", "feat"]));
    stack_file(&area, "b", "local.txt", "local\n");
    assert_success(&area.forklift("b", &["shift", "main"]));
    let ahead_head = pallet_head(&area, "b", "feat");

    let ahead = area.forklift("b", &["--json", "lower", "feat"]);
    assert_success(&ahead);
    assert_eq!(json_of(&ahead)["data"]["outcome"], "ahead");
    assert_eq!(pallet_head(&area, "b", "feat"), ahead_head);

    advance_remote(&area, "feat", "feat.txt", "feat v3\n");
    let diverged = area.forklift("b", &["--json", "lower", "feat"]);
    assert_eq!(diverged.status.code(), Some(5), "{}{}", stdout(&diverged), stderr(&diverged));
    let value = json_of(&diverged);
    assert_eq!(value["error"]["code"], "diverged");
    assert_eq!(value["error"]["next_step"], "forklift shift feat; then forklift lower", "{}", value);
    assert_eq!(pallet_head(&area, "b", "feat"), ahead_head, "a diverged lower moves nothing");
}

// Revert: write the ref before fetching → the failed lower leaves a ref over a missing closure.
#[test]
fn a_named_lower_whose_fetch_fails_leaves_no_ref() {
    let area = TestArea::new("lower-named-fetch-fails");
    let server = Server::start(&area, None);
    let feat = remote_with_a_side_pallet(&area, &server);

    let blob = path_object_hash(&area, "a", &feat, "feat.txt");
    std::fs::remove_file(object_store_path(&area, "server-root", &blob)).unwrap();

    let failed = area.forklift("b", &["lower", "feat"]);
    assert!(!failed.status.success(), "{}", stdout(&failed));
    assert!(!area.path("b/.forklift/pallets/feat").exists(), "no ref before the closure is complete");
}

// Revert: report every local head as same → feat/side relations collapse.
#[test]
fn palletize_remote_lists_the_remotes_pallets_against_this_warehouse() {
    let area = TestArea::new("palletize-remote");
    let server = Server::start(&area, None);
    remote_with_a_side_pallet(&area, &server);
    assert_success(&area.forklift("a", &["office", "enroll"]));
    assert_success(&area.forklift("a", &["palletize", "side"]));
    stack_file(&area, "a", "side.txt", "side v1\n");
    assert_success(&area.forklift("a", &["lift"]));
    assert_success(&area.forklift("a", &["shift", "main"]));

    assert_success(&area.forklift("b", &["lower", "feat"]));
    assert_success(&area.forklift("b", &["lower", "side"]));
    stack_file(&area, "b", "local.txt", "local\n");
    advance_remote(&area, "side", "side.txt", "side v2\n");
    assert_success(&area.forklift("a", &["palletize", "fresh"]));
    assert_success(&area.forklift("a", &["lift"]));
    assert_success(&area.forklift("a", &["shift", "main"]));

    let pallets_before = std::fs::read_dir(area.path("b/.forklift/pallets")).unwrap().count();
    let objects_before = std::fs::read_dir(area.path("b/.forklift/objects")).unwrap().count();

    let listed = area.forklift("b", &["--json", "palletize", "--remote"]);
    assert_success(&listed);
    let value = json_of(&listed);
    let relation = |name: &str| value["data"]["pallets"].as_array().unwrap().iter()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("no {} in {}", name, value))["relation"].clone();

    assert_eq!(relation("main"), "local_ahead", "{}", value);
    assert_eq!(relation("feat"), "same", "{}", value);
    assert_eq!(relation("side"), "differs", "{}", value);
    assert_eq!(relation("fresh"), "absent", "{}", value);
    assert!(value["data"].get("meta").is_none(), "meta pallets only with --all: {}", value);
    assert!(value["data"]["pallets"].as_array().unwrap().iter().all(|e| !e["name"].as_str().unwrap().starts_with('@')));

    let all = json_of(&area.forklift("b", &["--json", "palletize", "--remote", "--all"]));
    assert!(all["data"]["meta"].as_array().unwrap().iter().any(|e| e["name"] == "@office"), "{}", all);

    assert_eq!(std::fs::read_dir(area.path("b/.forklift/pallets")).unwrap().count(), pallets_before);
    assert_eq!(std::fs::read_dir(area.path("b/.forklift/objects")).unwrap().count(), objects_before);
    assert!(!area.path("b/.forklift/pallets/fresh").exists(), "listing creates no ref");
}

/// The `--json` error document of a failed command, after checking its exit code.
fn refusal_of(output: &Output, exit: i32) -> serde_json::Value {
    assert_eq!(output.status.code(), Some(exit), "{}{}", stdout(output), stderr(output));
    json_of(output)["error"].clone()
}

/// The remote's head of meta pallet `name`, if it has one, read through `dir`'s remote listing.
fn remote_meta_head(area: &TestArea, dir: &str, name: &str) -> Option<String> {
    let listed = json_of(&area.forklift(dir, &["--json", "palletize", "--remote", "--all"]));
    listed["data"]["meta"].as_array()?.iter()
        .find(|entry| entry["name"] == name)
        .map(|entry| entry["remote_head"].as_str().unwrap().to_string())
}

// Revert: drop the tombstone arm of `acknowledged_tombstone` → the remote refuses the lift after
// the upload, naming the restore, not "--recreate"; drop the acknowledgement from the creation →
// the remote refuses "--recreate".
#[test]
fn a_pallet_deleted_on_the_remote_is_listed_and_lifts_back_only_acknowledged() {
    let area = TestArea::new("remote-delete-recreate");
    let server = Server::start(&area, None);
    let feat = remote_with_a_side_pallet(&area, &server);
    assert_success(&area.forklift("b", &["lower", "feat"]));

    let deleted = area.forklift("a", &["--json", "palletize", "--delete", "feat", "--remote"]);
    assert_success(&deleted);
    let value = json_of(&deleted);
    assert_eq!(value["data"]["head"], feat.as_str(), "{}", value);
    assert_eq!(value["data"]["local_head"], feat.as_str(), "the local pallet is untouched: {}", value);
    assert!(!area.path("server-root/.forklift/pallets/feat").exists(), "the remote's ref is gone");
    assert_eq!(pallet_head(&area, "a", "feat"), feat, "and the local one stays");
    assert_eq!(value["data"]["already"], false, "{}", value);

    // A rerun, as after a delete whose answer was lost, finds it deleted and reports it done.
    let again = area.forklift("a", &["--json", "palletize", "--delete", "feat", "--remote"]);
    assert_success(&again);
    let value = json_of(&again);
    assert_eq!(value["data"]["already"], true, "{}", value);
    assert_eq!(value["data"]["head"], feat.as_str(), "{}", value);

    let listed = json_of(&area.forklift("b", &["--json", "palletize", "--remote"]));
    assert!(listed["data"]["pallets"].as_array().unwrap().iter().all(|entry| entry["name"] != "feat"), "{}", listed);
    assert_eq!(listed["data"]["deleted"][0]["name"], "feat", "{}", listed);
    assert_eq!(listed["data"]["deleted"][0]["head"], feat.as_str(), "{}", listed);
    assert_eq!(listed["data"]["deleted"][0]["local_head"], feat.as_str(), "{}", listed);

    let lowered = refusal_of(&area.forklift("b", &["--json", "lower", "feat"]), 29);
    assert!(lowered["message"].as_str().unwrap().contains("was deleted"), "{}", lowered);
    assert_eq!(lowered["next_step"], "forklift palletize --restore feat --remote", "{}", lowered);

    // This clone's work descends from the deleted head: "--recreate" re-creates the pallet with it.
    assert_success(&area.forklift("b", &["shift", "feat"]));
    stack_file(&area, "b", "more.txt", "more\n");
    // An office change waits to lift too: the refusal comes before the office uploads.
    let office_before = remote_meta_head(&area, "b", "@office");
    assert_success(&area.forklift("b", &["office", "enroll"]));
    let refused = refusal_of(&area.forklift("b", &["--json", "lift"]), 35);
    assert_eq!(refused["code"], "pallet_deleted_on_remote", "{}", refused);
    assert_eq!(refused["next_step"], "forklift lift --recreate", "{}", refused);
    assert!(!area.path("server-root/.forklift/pallets/feat").exists(), "nothing was lifted");
    assert_eq!(remote_meta_head(&area, "b", "@office"), office_before, "not even the office");

    assert_success(&area.forklift("b", &["lift", "--recreate"]));
    assert_eq!(pallet_head(&area, "server-root", "feat"), pallet_head(&area, "b", "feat"));
    let listed = json_of(&area.forklift("b", &["--json", "palletize", "--remote"]));
    assert!(listed["data"].get("deleted").is_none(), "the re-creation cleared the tombstone: {}", listed);
}

// Revert: send no tombstone with the remote restore → the remote refuses it; offer "--recreate" to
// work that does not descend → the remote refuses that lift instead of the client.
#[test]
fn a_stale_clone_restores_a_pallet_deleted_on_the_remote_rather_than_re_creating_it() {
    let area = TestArea::new("remote-delete-restore");
    let server = Server::start(&area, None);
    let feat = remote_with_a_side_pallet(&area, &server);
    assert_success(&area.forklift("a", &["palletize", "--delete", "feat", "--remote"]));

    // "b" never lowered "feat": its own "feat" starts from main, which the deleted head descends from.
    assert_success(&area.forklift("b", &["palletize", "feat"]));
    stack_file(&area, "b", "other.txt", "other\n");
    for args in [&["--json", "lift"][..], &["--json", "lift", "--recreate"]] {
        let refused = refusal_of(&area.forklift("b", args), 35);
        assert_eq!(refused["next_step"], "forklift palletize --restore feat --remote", "{:?}: {}", args, refused);
        assert!(refused["message"].as_str().unwrap().contains("does not descend"), "{}", refused);
    }

    let restored = area.forklift("b", &["--json", "palletize", "--restore", "feat", "--remote"]);
    assert_success(&restored);
    assert_eq!(json_of(&restored)["data"]["head"], feat.as_str());
    assert_eq!(pallet_head(&area, "server-root", "feat"), feat, "the remote has it back at the deleted head");

    let again = area.forklift("b", &["palletize", "--restore", "feat", "--remote"]);
    assert!(!again.status.success() && stderr(&again).contains("is not deleted"), "{}{}", stdout(&again), stderr(&again));
    let named = area.forklift("b", &["palletize", "feat", "--remote"]);
    assert_eq!(named.status.code(), Some(2), "a usage error: {}", stderr(&named));
    assert!(stderr(&named).contains("needs \"--delete\" or \"--restore\""), "{}", stderr(&named));
}

// Revert: drop the capability check → the delete is sent, and reaches the server behind the proxy.
#[test]
fn a_remote_whose_handshake_lists_no_tombstones_is_never_sent_a_delete() {
    let area = TestArea::new("remote-delete-unsupported");
    let server = Server::start(&area, None);
    let feat = remote_with_a_side_pallet(&area, &server);
    let main = pallet_head(&area, "a", "main");

    // The handshake an older head sends: no "tombstones".
    let handshake = serde_json::json!({
        "protocol": forklift_core::model::remote::PROTOCOL_VERSION,
        "default_pallet": "main",
        "pallets": { "main": main, "feat": feat },
        "trust": null,
    }).to_string();
    let deletes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = std::sync::Arc::clone(&deletes);
    let proxy = InterceptingProxy::start_seeing(&server.url, move |path, head| {
        if head.starts_with("DELETE") {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        path.ends_with("/v1/warehouse").then(|| format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            handshake.len(), handshake
        ).into_bytes())
    });
    assert_success(&area.forklift("a", &["config", "remote.url", &proxy.url]));

    for verb in ["--delete", "--restore"] {
        let refused = area.forklift("a", &["palletize", verb, "feat", "--remote"]);
        assert!(!refused.status.success(), "{}", stdout(&refused));
        assert!(stderr(&refused).contains("does not support deleting pallets"), "{}", stderr(&refused));
    }
    assert_eq!(deletes.load(std::sync::atomic::Ordering::SeqCst), 0, "nothing was sent");
    assert!(area.path("server-root/.forklift/pallets/feat").exists());
}

// Revert: walk only the handshake's live pallets in the delete's containment → the local delete
// refuses work the remote still keeps under its tombstone.
#[test]
fn a_local_delete_counts_work_the_remote_keeps_under_a_tombstone_as_held() {
    let area = TestArea::new("remote-delete-containment");
    let server = Server::start(&area, None);
    remote_with_a_side_pallet(&area, &server);
    assert_success(&area.forklift("b", &["lower", "feat"]));
    assert_success(&area.forklift("a", &["palletize", "--delete", "feat", "--remote"]));

    let deleted = area.forklift("b", &["--json", "palletize", "--delete", "feat"]);
    assert_success(&deleted);
    assert_eq!(json_of(&deleted)["data"]["contained_by"], "the remote", "{}", stdout(&deleted));
}

// Revert: name `palletize` whatever the remote → the remote warehouse's next step is not lower.
#[test]
fn a_missing_pallet_is_a_coded_refusal_naming_lower_only_with_a_remote() {
    let area = TestArea::new("pallet-not-found");
    let server = Server::start(&area, None);
    remote_with_a_side_pallet(&area, &server);
    assert_success(&area.forklift("a", &["office", "enroll"]));
    assert_success(&area.forklift("a", &["lift"]));
    assert_success(&area.forklift("b", &["lower"]));

    for args in [&["shift", "nope"][..], &["consolidate", "nope"], &["audit", "nope"],
                 &["haul", "open", "--target", "nope", "--source", "main", "--title", "t"],
                 &["haul", "open", "--target", "main", "--source", "nope", "--title", "t"]] {
        let mut json = vec!["--json"];
        json.extend_from_slice(args);
        let refused = area.forklift("b", &json);
        assert_eq!(refused.status.code(), Some(29), "{:?}: {}{}", args, stdout(&refused), stderr(&refused));
        let value = json_of(&refused);
        assert_eq!(value["error"]["code"], "pallet_not_found", "{:?}: {}", args, value);
        assert_eq!(value["error"]["next_step"], "forklift lower nope", "{:?}: {}", args, value);
    }

    let refused = area.forklift("b", &["--json", "audit", "@haul"]);
    assert_eq!(refused.status.code(), Some(29), "{}{}", stdout(&refused), stderr(&refused));
    assert_eq!(json_of(&refused)["error"]["next_step"], "forklift lower", "a bare lower syncs meta pallets");

    std::fs::create_dir_all(area.path("solo")).unwrap();
    assert_success(&area.forklift("solo", &["prepare"]));
    stack_file(&area, "solo", "x.txt", "x\n");
    let refused = area.forklift("solo", &["--json", "shift", "nope"]);
    assert_eq!(refused.status.code(), Some(29), "{}{}", stdout(&refused), stderr(&refused));
    let value = json_of(&refused);
    assert_eq!(value["error"]["next_step"], "forklift palletize nope", "{}", value);
    assert!(!value["error"]["message"].as_str().unwrap().contains("lower"), "{}", value);
}

// Revert: list every remote pallet in the refusal → "@office" is named.
#[test]
fn lowering_a_pallet_the_remote_lacks_names_only_user_pallets() {
    let area = TestArea::new("lower-named-missing");
    let server = Server::start(&area, None);
    remote_with_a_side_pallet(&area, &server);
    assert_success(&area.forklift("a", &["office", "enroll"]));
    assert_success(&area.forklift("a", &["lift"]));

    let refused = area.forklift("b", &["lower", "nope"]);
    assert!(!refused.status.success());
    let message = stderr(&refused);
    assert!(message.contains("feat") && message.contains("palletize --remote"), "{}", message);
    assert!(!message.contains('@'), "{}", message);
    assert!(!message.contains("FIRST CONTACT"), "refused before the remote's trust is adopted: {}", message);

    let refused = area.forklift("b", &["--json", "lower", "nope"]);
    assert_eq!(refused.status.code(), Some(29), "{}{}", stdout(&refused), stderr(&refused));
    let value = json_of(&refused);
    assert_eq!((value["error"]["code"].as_str(), value["error"]["next_step"].as_str()),
        (Some("pallet_not_found"), Some("forklift palletize --remote")), "{}", value);

    // A pin the remote fails is refused as such, not as the remote's missing pallet.
    let pinned = area.forklift("b", &["--json", "lower", "nope", "--expect", &"0".repeat(64)]);
    assert!(!pinned.status.success());
    assert_ne!(json_of(&pinned)["error"]["code"], "pallet_not_found", "{}", stdout(&pinned));

    // The current pallet the remote lacks is the same condition, with the remedy at that site.
    assert_success(&area.forklift("b", &["palletize", "local-only"]));
    let bare = area.forklift("b", &["--json", "lower"]);
    assert_eq!(bare.status.code(), Some(29), "{}{}", stdout(&bare), stderr(&bare));
    assert_eq!(json_of(&bare)["error"]["next_step"], "forklift lift");
}

// Revert: judge only the handshake's pin before the missing-pallet refusal → it reports the
// other warehouse's pallet list instead of its changed anchor.
#[test]
fn lowering_a_missing_pallet_from_a_remote_with_another_anchor_refuses_the_anchor() {
    let area = TestArea::new("lower-named-missing-anchor");
    let server = Server::start(&area, None);
    remote_with_a_side_pallet(&area, &server);
    // B enrolls an anchor of its own before A publishes one, so the remote's is a different one —
    // unpinned, the handshake alone does not tell them apart.
    assert_success(&area.forklift("b", &["office", "enroll"]));
    assert_success(&area.forklift("a", &["office", "enroll"]));
    assert_success(&area.forklift("a", &["lift"]));

    let refused = area.forklift("b", &["--json", "lower", "nope"]);
    trust_changed_details(&refused);
}

// Revert: drop the unborn-bay check → the lower creates feat under the bay standing on it.
#[test]
fn a_named_lower_does_not_create_a_pallet_a_bay_stands_on_unborn() {
    let area = TestArea::new("lower-named-unborn-bay");
    let server = Server::start(&area, None);
    remote_with_a_side_pallet(&area, &server);

    let bay_dir = area.path("b-bay");
    assert_success(&area.forklift("b", &["bay", "add", "w", bay_dir.to_str().unwrap()]));
    // No command puts a bay on an unborn pallet other than a fresh warehouse's first, so the
    // position is written by hand.
    let position = area.read_file("b/.forklift/bays/w/position");
    let header = position.lines().next().unwrap();
    std::fs::write(area.path("b/.forklift/bays/w/position"), format!("{}\nfeat\n\n", header)).unwrap();

    let refused = area.forklift("b", &["lower", "feat"]);
    assert!(!refused.status.success(), "{}", stdout(&refused));
    assert!(stderr(&refused).contains("Bay \"w\" stands on pallet \"feat\""), "{}", stderr(&refused));
    assert!(!area.path("b/.forklift/pallets/feat").exists());
}

// Revert: let the named lower re-point the bay → the other bay's stack is not behind.
#[test]
fn a_bay_on_a_pallet_another_bay_lowered_is_behind_until_it_shifts() {
    let area = TestArea::new("lower-named-other-bay");
    let server = Server::start(&area, None);
    remote_with_a_side_pallet(&area, &server);
    assert_success(&area.forklift("b", &["lower", "feat"]));

    let bay_dir = area.path("b-bay");
    assert_success(&area.forklift("b", &["bay", "add", "w", bay_dir.to_str().unwrap()]));
    assert_success(&area.forklift("b-bay", &["shift", "feat"]));

    advance_remote(&area, "feat", "feat.txt", "feat v2\n");
    assert_success(&area.forklift("b", &["lower", "feat"]));
    assert_eq!(area.read_file("b-bay/feat.txt"), "feat v1\n");

    area.write_file("b-bay/other.txt", "other\n");
    assert_success(&area.forklift("b-bay", &["load", "."]));
    let behind = area.forklift("b-bay", &["stack", "behind"]);
    assert_eq!(behind.status.code(), Some(26), "{}{}", stdout(&behind), stderr(&behind));
    assert!(stderr(&behind).contains("forklift shift feat"), "{}", stderr(&behind));

    std::fs::remove_file(bay_dir.join("other.txt")).unwrap();
    assert_success(&area.forklift("b-bay", &["load", "."]));
    assert_success(&area.forklift("b-bay", &["shift", "feat"]));
    assert_eq!(area.read_file("b-bay/feat.txt"), "feat v2\n");
}

// Revert: send `lower <current>` down the named path → the bay's files stay at v1.
#[test]
fn lowering_the_current_pallet_by_name_is_a_bare_lower() {
    let area = TestArea::new("lower-named-current");
    let server = Server::start(&area, None);
    remote_with_a_side_pallet(&area, &server);

    advance_remote(&area, "main", "base.txt", "base v2\n");
    let lowered = area.forklift("b", &["--json", "lower", "main"]);
    assert_success(&lowered);
    assert_eq!(json_of(&lowered)["data"]["outcome"], "lowered");
    assert_eq!(area.read_file("b/base.txt"), "base v2\n", "the bay moves with its pallet");
}

// Revert: fetch the named pallet unscoped → src/web arrives before the expand.
#[test]
fn a_sparse_warehouse_lowers_another_pallet_within_scope_and_expand_widens_it() {
    let area = TestArea::new("lower-named-sparse");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "dev", &server.url);
    area.write_file("dev/src/api/a.txt", "api v1\n");
    area.write_file("dev/src/web/w.txt", "web v1\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "base"]));
    assert_success(&area.forklift("dev", &["lift"]));
    assert_success(&area.forklift(".", &["franchise", &server.url, "sparse", "--only", "src/api"]));

    assert_success(&area.forklift("dev", &["palletize", "feat"]));
    area.write_file("dev/src/web/w.txt", "web v2 on feat\n");
    assert_success(&area.forklift("dev", &["load", "."]));
    assert_success(&area.forklift("dev", &["stack", "feat edits web"]));
    assert_success(&area.forklift("dev", &["lift"]));
    let feat = pallet_head(&area, "dev", "feat");
    let web_blob = path_object_hash(&area, "dev", &feat, "src/web/w.txt");

    assert_success(&area.forklift("sparse", &["lower", "feat"]));
    assert_eq!(pallet_head(&area, "sparse", "feat"), feat);
    assert!(!object_present(&area, "sparse", &web_blob), "the out-of-scope change stays sealed");

    assert_success(&area.forklift("sparse", &["expand", "src/web"]));
    assert!(object_present(&area, "sparse", &web_blob), "expand widens the lowered pallet's history too");
}

// Revert: skip the remote ask in the delete's containment → the lowered pallet refuses without
// "--force".
#[test]
fn a_lowered_pallet_the_remote_holds_deletes_without_force() {
    let area = TestArea::new("delete-remote-held");
    let server = Server::start(&area, None);
    let feat = remote_with_a_side_pallet(&area, &server);
    assert_success(&area.forklift("b", &["lower", "feat"]));

    let deleted = area.forklift("b", &["--json", "palletize", "--delete", "feat"]);
    assert_success(&deleted);
    let value = json_of(&deleted);
    assert_eq!(value["data"]["contained_by"], "the remote", "{}", value);
    assert_eq!(value["data"]["head"], feat.as_str(), "{}", value);
    assert!(!area.path("b/.forklift/pallets/feat").exists());

    // Work the remote does not hold is refused.
    assert_success(&area.forklift("b", &["undo"]));
    assert_success(&area.forklift("b", &["shift", "feat"]));
    stack_file(&area, "b", "local.txt", "local\n");
    assert_success(&area.forklift("b", &["shift", "main"]));
    let refused = area.forklift("b", &["palletize", "--delete", "feat"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("Neither does the remote"), "{}", stderr(&refused));
}

#[test]
fn scope_prune_keeps_unlifted_history_behind_a_deleted_pallet_for_its_undo() {
    let area = TestArea::new("scope-prune-deleted");
    let _server = sparse_with_docs(&area);

    assert_success(&area.forklift("sparse", &["palletize", "side"]));
    std::fs::write(area.path("sparse/docs/guide.md"), "guide on side, never lifted\n").unwrap();
    assert_success(&area.forklift("sparse", &["load", "."]));
    assert_success(&area.forklift("sparse", &["stack", "side docs edit"]));
    let head = pallet_head(&area, "sparse", "side");
    let blob = path_object_hash(&area, "sparse", &head, "docs/guide.md");
    assert_success(&area.forklift("sparse", &["shift", "main"]));
    assert_success(&area.forklift("sparse", &["palletize", "--delete", "side", "--force"]));

    assert_success(&area.forklift("sparse", &["narrow", "docs"]));
    let pruned = area.forklift("sparse", &["--json", "scope-prune", "docs"]);
    assert_success(&pruned);
    let report = json_of(&pruned);
    assert!(report["data"]["kept_for_deleted"].as_u64().unwrap() > 0, "{}", report);
    assert!(object_present(&area, "sparse", &blob), "the deleted pallet's unlifted work is kept");

    assert_success(&area.forklift("sparse", &["undo"]));
    assert_eq!(pallet_head(&area, "sparse", "side"), head);
}

// A remote whose trust changed is not asked what it holds: the delete refuses before reading its
// heads, which still name the pallet's head.
#[test]
fn a_delete_refuses_a_remote_whose_trust_changed_instead_of_asking_it() {
    let area = TestArea::new("delete-remote-regenesis");
    let _server = regenesis_on_remote(&area, |server| {
        assert_success(&area.forklift("a", &["palletize", "feat"]));
        stack_file(&area, "a", "feat.txt", "feat v1\n");
        assert_success(&area.forklift("a", &["lift"]));
        assert_success(&area.forklift("a", &["shift", "main"]));
        assert_success(&area.forklift(".", &["franchise", &server.url, "b", "--token", "root"]));
        assert_success(&area.forklift("b", &["lower", "feat"]));
    });

    let refused = area.forklift("b", &["palletize", "--delete", "feat"]);
    assert!(!refused.status.success(), "{}", stdout(&refused));
    assert!(stderr(&refused).contains("CHANGED"), "{}", stderr(&refused));
    assert!(stderr(&refused).contains("nothing was deleted"), "{}", stderr(&refused));
    assert!(area.path("b/.forklift/pallets/feat").exists());
}

/// The message of a `history_unverified` refusal under `--json`: exit 34, the error document on
/// stdout.
#[track_caller]
fn history_unverified_message(output: &Output) -> String {
    assert_eq!(output.status.code(), Some(34), "{}{}", stdout(output), stderr(output));
    let value: serde_json::Value = serde_json::from_str(&stdout(output)).unwrap();
    assert_eq!(value["error"]["code"], "history_unverified", "{}", value);
    value["error"]["message"].as_str().unwrap().to_string()
}

/// The message of a refused franchise whose cause is `history_unverified`: a franchise reports
/// any failure as `franchise_incomplete` (exit 30), keeping what it fetched for a resume.
#[track_caller]
fn franchise_history_unverified_message(output: &Output) -> String {
    assert_eq!(output.status.code(), Some(30), "{}{}", stdout(output), stderr(output));
    let value: serde_json::Value = serde_json::from_str(&stdout(output)).unwrap();
    assert_eq!(value["error"]["details"]["cause_code"], "history_unverified", "{}", value);
    value["error"]["message"].as_str().unwrap().to_string()
}

/// A remote whose main was signed under an anchor, then stripped of both its anchor and its
/// office, and moved by an anchorless client "c" to an unsigned parcel. "b" franchised while it
/// was whole, so it holds the anchor and stands at the signed head.
struct StrippedRemote {
    server: Server,
    /// The signed head "b" stands at.
    signed: String,
    /// The unsigned head the remote now serves for main.
    forged: String,
    /// The stripped files and what they held, to put the anchor and office back.
    stripped: Vec<(PathBuf, Vec<u8>)>,
}

impl StrippedRemote {
    fn new(area: &TestArea) -> StrippedRemote {
        let server = Server::start(area, None);

        prepare_warehouse(area, "a", &server.url);
        stack_file(area, "a", "code.txt", "v1\n");
        assert_success(&area.forklift("a", &["office", "enroll"]));
        stack_file(area, "a", "code.txt", "v2\n");
        assert_success(&area.forklift("a", &["lift"]));

        assert_success(&area.forklift(".", &["franchise", &server.url, "b"]));
        assert!(area.path("b/.forklift/trust").is_file());
        let signed = pallet_head(area, "b", "main");

        let trust = area.path("server-root/.forklift/trust");
        let office: Vec<PathBuf> = walk(&area.path("server-root/.forklift")).into_iter()
            .filter(|entry| entry.file_name().is_some_and(|name| name == "office"))
            .collect();
        assert_eq!(office.len(), 1, "{:?}", office);

        let mut stripped = Vec::new();
        for path in [trust, office[0].clone()] {
            stripped.push((path.clone(), std::fs::read(&path).unwrap()));
            std::fs::remove_file(&path).unwrap();
        }

        // An anchorless client lifts an unsigned parcel, and the anchorless server takes it.
        assert_success(&area.forklift(".", &["franchise", &server.url, "c"]));
        assert!(!area.path("c/.forklift/trust").exists());
        stack_file(area, "c", "code.txt", "forged\n");
        assert_success(&area.forklift("c", &["lift"]));
        let forged = pallet_head(area, "c", "main");
        assert_eq!(remote_pallet_head(&server, "main"), Some(forged.clone()));

        StrippedRemote { server, signed, forged, stripped }
    }

    /// Put the stripped anchor and office back: the remote serves its anchor again, over a main
    /// that does not verify against it.
    fn restore(&self) {
        for (path, bytes) in &self.stripped {
            std::fs::write(path, bytes).unwrap();
        }
    }
}

/// Whether an interrupted operation's marker stands in a warehouse under the area.
fn operation_standing(area: &TestArea, dir: &str) -> bool {
    let _scope = forklift_core::globals::StorageRootScope::enter(&area.path(dir));
    forklift_core::util::operation_utils::read().unwrap().is_some()
}

/// A warehouse holding an anchor takes nothing a stripped remote serves past it: a lower of the
/// working pallet refuses before its first durable record, leaving the ref, the working files and
/// no operation marker behind, and so does a lower of a pallet by name. Revert: let `verify_take`
/// return the preconditions before judging, and the lower fast-forwards b to the forged parcel.
#[test]
fn a_lower_refuses_history_a_stripped_remote_serves_and_takes_nothing() {
    let area = TestArea::new("take-stripped-lower");
    let remote = StrippedRemote::new(&area);

    let refused = area.forklift("b", &["--json", "lower"]);
    let message = history_unverified_message(&refused);
    assert!(message.contains("carries no signature"), "{}", message);
    assert!(message.contains(&remote.forged), "{}", message);
    assert!(message.contains("Nothing was taken."), "{}", message);
    assert_eq!(pallet_head(&area, "b", "main"), remote.signed);
    assert_eq!(area.read_file("b/code.txt"), "v2\n");
    assert!(!operation_standing(&area, "b"), "a refused take raises no operation");

    assert_success(&area.forklift("b", &["palletize", "side"]));
    let named = area.forklift("b", &["--json", "lower", "main"]);
    history_unverified_message(&named);
    assert_eq!(pallet_head(&area, "b", "main"), remote.signed);
    assert_eq!(area.read_file("b/code.txt"), "v2\n");

    // Nor does a warehouse that never held an anchor stop taking from one that never had one.
    assert_success(&area.forklift(".", &["franchise", &remote.server.url, "d"]));
    assert_eq!(pallet_head(&area, "d", "main"), remote.forged);
    assert_success(&area.forklift("c", &["lower"]));
}

/// What a refused lower fetched is in the store, but adopting it by hash refuses the same way:
/// a pallet created at it, and a lift whose auto-merge would take it. Revert: the same mutation
/// of `verify_take`, and the palletize creates the pallet at the forged parcel.
#[test]
fn a_take_by_hash_or_by_auto_merge_refuses_unverified_history() {
    let area = TestArea::new("take-stripped-hash");
    let remote = StrippedRemote::new(&area);

    history_unverified_message(&area.forklift("b", &["--json", "lower"]));
    assert!(object_present(&area, "b", &remote.forged), "the refused lower fetched the forged parcel");

    let created = area.forklift("b", &["--json", "palletize", "forged", &remote.forged]);
    let message = history_unverified_message(&created);
    assert!(message.contains("was not moved to it"), "{}", message);
    assert!(!area.path("b/.forklift/pallets/forged").exists());

    // With the anchor back, the lift finds the remote ahead under the same anchor and office.
    remote.restore();
    stack_file(&area, "b", "other.txt", "local\n");
    let local = pallet_head(&area, "b", "main");
    let lifted = area.forklift("b", &["--json", "lift"]);
    history_unverified_message(&lifted);
    assert_eq!(pallet_head(&area, "b", "main"), local);
    assert_eq!(area.read_file("b/code.txt"), "v2\n");
    assert_eq!(remote_pallet_head(&remote.server, "main"), Some(remote.forged.clone()));
}

/// At first contact the anchor a remote serves is checked against what it serves before anything
/// is installed: a franchise, a sparse one, and a lower in a warehouse holding no anchor refuse,
/// and leave no anchor, office or pallet behind. Revert: the same mutation of `verify_take`, and
/// the franchise installs the anchor over the forged main.
#[test]
fn first_contact_refuses_history_that_does_not_verify_against_the_served_anchor() {
    let area = TestArea::new("take-first-contact");
    let remote = StrippedRemote::new(&area);
    assert_success(&area.forklift(".", &["franchise", &remote.server.url, "e"]));
    remote.restore();

    let franchised = area.forklift(".", &["--json", "franchise", &remote.server.url, "d"]);
    let message = franchise_history_unverified_message(&franchised);
    assert!(message.contains("the trust anchor the remote serves"), "{}", message);
    assert!(!area.path("d/.forklift/trust").exists(), "no anchor installed");
    assert!(!area.path("d/.forklift/pallets/main").exists(), "no pallet adopted");
    assert!(!walk(&area.path("d/.forklift")).iter().any(|entry| entry.starts_with(area.path("d/.forklift/meta"))),
            "no office or other meta pallet adopted");

    let sparse = area.forklift(".", &["--json", "franchise", &remote.server.url, "s", "--only", "code.txt"]);
    franchise_history_unverified_message(&sparse);
    assert!(!area.path("s/.forklift/trust").exists(), "no anchor installed");

    // "e" franchised the stripped remote, so it holds no anchor and stands at the forged head;
    // the restored anchor does not vouch for it, and a lower installs nothing.
    let lowered = area.forklift("e", &["--json", "lower"]);
    history_unverified_message(&lowered);
    assert!(!area.path("e/.forklift/trust").exists(), "no anchor installed");
    assert!(!walk(&area.path("e/.forklift")).iter().any(|entry| entry.file_name().is_some_and(|name| name == "office")),
            "no office installed");
}

/// A partial client lowering a post-enrollment pallet whose unsigned pre-enrollment ancestry is
/// bounded only by another pallet's boundary head fetches that boundary and takes it; the take
/// then audits clean and leaves nothing for heal. A remote that cannot serve the boundary refuses,
/// naming it. Revert: let `verify_take` refuse a gap without fetching, and `lower y` refuses.
#[test]
fn a_partial_client_fetches_the_trust_boundary_it_lacks() {
    let area = TestArea::new("take-boundary-gap");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    stack_file(&area, "a", "code.txt", "v1\n");
    assert_success(&area.forklift("a", &["lift"]));

    assert_success(&area.forklift("a", &["palletize", "side"]));
    stack_file(&area, "a", "code.txt", "s1\n");
    let s1 = pallet_head(&area, "a", "side");
    stack_file(&area, "a", "code.txt", "s2\n");
    let s2 = pallet_head(&area, "a", "side");
    assert_success(&area.forklift("a", &["lift"]));

    assert_success(&area.forklift("a", &["shift", "main"]));
    assert_success(&area.forklift("a", &["office", "enroll"]));
    assert_success(&area.forklift("a", &["lift"]));

    assert_success(&area.forklift("a", &["palletize", "y", &s1]));
    stack_file(&area, "a", "code.txt", "y\n");
    assert_success(&area.forklift("a", &["lift"]));

    assert_success(&area.forklift(".", &["franchise", &server.url, "b"]));
    assert!(!object_present(&area, "b", &s2), "the franchise did not fetch side");
    assert_success(&area.forklift("b", &["lower", "y"]));
    assert_success(&area.forklift("b", &["audit", "y"]));
    let healed = area.forklift("b", &["heal"]);
    assert_success(&healed);
    assert!(stdout(&healed).contains("Nothing to heal"), "{}", stdout(&healed));

    // A remote that no longer holds the boundary head cannot vouch for y.
    assert_success(&area.forklift(".", &["franchise", &server.url, "c"]));
    std::fs::remove_file(object_store_path(&area, "server-root", &s2)).unwrap();
    let refused = area.forklift("c", &["--json", "lower", "y"]);
    let message = history_unverified_message(&refused);
    assert!(message.contains(&s2), "{}", message);
    assert!(!area.path("c/.forklift/pallets/y").exists());
}

/// Work stacked here before the remote enrolled is not vouched for by the anchor a later lower
/// installs: the install records it, and a take never skips its history as a known-verified head.
/// A head the anchor does vouch for stays known. Revert: let a held take count every local head as
/// known, and `palletize copy` adopts the unsigned head.
#[test]
fn an_anchor_taken_from_a_remote_does_not_vouch_for_local_work_it_never_saw() {
    let area = TestArea::new("take-unvouched-local");
    let server = Server::start(&area, None);

    prepare_warehouse(&area, "a", &server.url);
    stack_file(&area, "a", "code.txt", "v1\n");
    assert_success(&area.forklift("a", &["lift"]));
    let base = pallet_head(&area, "a", "main");

    assert_success(&area.forklift(".", &["franchise", &server.url, "w"]));
    assert_success(&area.forklift("w", &["palletize", "feature"]));
    stack_file(&area, "w", "code.txt", "local\n");
    let local = pallet_head(&area, "w", "feature");
    assert_success(&area.forklift("w", &["shift", "main"]));

    assert_success(&area.forklift("a", &["office", "enroll"]));
    assert_success(&area.forklift("a", &["lift"]));

    let lowered = area.forklift("w", &["lower"]);
    assert_success(&lowered);
    assert!(stderr(&lowered).contains("Adopted the remote's trust anchor") || stdout(&lowered).contains("Adopted the remote's trust anchor"),
            "{}{}", stdout(&lowered), stderr(&lowered));
    // The command says which pallet it could not vouch for, and only that one.
    let warning = stderr(&lowered);
    assert!(warning.contains("pallet(s) \"feature\""), "{}", warning);
    assert!(!warning.contains("\"main\""), "{}", warning);
    let recorded = area.read_file("w/.forklift/unvouched-heads");
    assert!(recorded.contains(&local), "{}", recorded);
    assert!(!recorded.contains(&base), "{}", recorded);

    let refused = area.forklift("w", &["--json", "palletize", "copy", &local]);
    let message = history_unverified_message(&refused);
    assert!(message.contains(&local), "{}", message);
    assert!(!area.path("w/.forklift/pallets/copy").exists());

    assert_success(&area.forklift("w", &["palletize", "kept", &base]));
}

/// Stand an interrupted lower of b's main from `from` to `to` — interrupted after it wrote the
/// files and before it wrote the ref, so b holds `to`'s files with the ref and position at `from` —
/// recording the office its take was verified against: the one b holds, or another.
fn interrupt_lower(area: &TestArea, from: &str, to: &str, office_moved: bool) {
    use forklift_core::util::{audit_utils, object_utils, operation_utils, pallet_utils};
    use forklift_core::util::operation_utils::{Operation, OperationKind};
    use forklift_core::util::pallet_utils::{BayPosition, OfficePrecondition, RefPreconditions};

    let _scope = forklift_core::globals::StorageRootScope::enter(&area.path("b"));
    let (_, office) = audit_utils::verified_local_trust().unwrap().expect("b holds an anchor");
    pallet_utils::update_pallet_head("main", Some(to), from, &RefPreconditions::NONE).unwrap();
    pallet_utils::write_position(&BayPosition::new("main", Some(from.to_string()))).unwrap();

    let mut operation = Operation::new(
        OperationKind::Lower,
        BayPosition::new("main", Some(from.to_string())),
        Some(object_utils::load_parcel(from).unwrap().tree_hash),
        BayPosition::new("main", Some(to.to_string())),
    );
    operation.target_tree = Some(object_utils::load_parcel(to).unwrap().tree_hash);
    operation.expected = Some(from.to_string());
    let preconditions = office.preconditions();
    operation.office = match office_moved {
        true => OfficePrecondition::At(Some("0".repeat(64))),
        false => preconditions.office,
    };
    operation.anchor = preconditions.anchor;
    operation_utils::raise(&operation).unwrap();
    operation_utils::forget_raised_for_tests();
}

/// Heal rolls an interrupted lower forward only under the office and anchor its take was verified
/// against: with the office it recorded still standing it finishes the take, and with the office
/// moved since it undoes it. Revert: let heal's roll-forward write with no preconditions, and the
/// moved office is rolled forward too.
#[test]
fn heal_finishes_an_interrupted_lower_only_under_the_office_it_was_verified_against() {
    let area = TestArea::new("take-heal");
    let server = Server::start(&area, None);
    prepare_warehouse(&area, "a", &server.url);
    stack_file(&area, "a", "code.txt", "v1\n");
    assert_success(&area.forklift("a", &["office", "enroll"]));
    assert_success(&area.forklift("a", &["lift"]));
    assert_success(&area.forklift(".", &["franchise", &server.url, "b"]));
    let from = pallet_head(&area, "b", "main");

    stack_file(&area, "a", "code.txt", "v2\n");
    assert_success(&area.forklift("a", &["lift"]));
    assert_success(&area.forklift("b", &["lower"]));
    let to = pallet_head(&area, "b", "main");

    for (office_moved, head, content) in [(false, &to, "v2\n"), (true, &from, "v1\n")] {
        assert_success(&area.forklift("b", &["shift", "main"]));
        interrupt_lower(&area, &from, &to, office_moved);
        assert!(operation_standing(&area, "b"));

        let healed = area.forklift("b", &["heal"]);
        assert_success(&healed);
        assert!(!operation_standing(&area, "b"));
        assert_eq!(&pallet_head(&area, "b", "main"), head, "office moved: {}; {}", office_moved, stdout(&healed));
        assert_eq!(area.read_file("b/code.txt"), content, "office moved: {}", office_moved);
    }
}

/// "a", an admin, holding a worker's lifted `feat` and the worker's open haul of it, with main
/// diverged so merging `feat` conflicts. Returns the server, the haul id and the worker's head.
fn worker_feat(area: &TestArea) -> (Server, String, String) {
    area.write_file("tokens.toml", concat!(
        "[operators]\n",
        "\"w-token\" = \"worker@forklift\"\n",
    ));
    let server = Server::start_with_tokens(area, Some("root"), "tokens.toml");

    prepare_warehouse(area, "a", &server.url);
    assert_success(&area.forklift("a", &["config", "remote.token", "root"]));
    stack_file(area, "a", "code.txt", "v1\n");
    assert_success(&area.forklift("a", &["office", "enroll"]));
    let worker = keygen_admit_args(area, "worker@forklift");
    assert_success(&area.forklift("a", &["office", "admit", &worker[0], &worker[1], &worker[2]]));
    assert_success(&area.forklift("a", &["lift"]));

    assert_success(&area.forklift(".", &["franchise", &server.url, "b", "--token", "w-token"]));
    assert_success(&area.forklift("b", &["config", "operator.identifier", "worker@forklift"]));
    assert_success(&area.forklift("b", &["palletize", "feat"]));
    stack_file(area, "b", "code.txt", "worker\n");
    assert_success(&area.forklift("b", &["lift"]));
    assert_success(&area.forklift("b", &["shift", "main"]));
    let id = open_haul(area, "b", "Worker");
    assert_success(&area.forklift("b", &["lift"]));
    let worked = pallet_head(area, "b", "feat");

    // a takes both while the worker's key is trusted.
    assert_success(&area.forklift("a", &["lower", "feat"]));
    assert_success(&area.forklift("a", &["lower"]));
    stack_file(area, "a", "code.txt", "admin\n");

    (server, id, worked)
}

/// Retire the worker's key as compromised in "a", once a holds no head at the worker's parcel —
/// the haul, closed meanwhile, would keep `feat` — so the distrust boundary does not hold it.
fn retire_worker_beyond_feat(area: &TestArea, id: &str) {
    assert_success(&area.forklift("a", &["haul", "close", id]));
    assert_success(&area.forklift("a", &["palletize", "--delete", "feat"]));

    let list = stdout(&area.forklift("a", &["office", "list"]));
    let worker_key_id = list.lines().rev()
        .find(|line| line.trim_start().starts_with("key "))
        .and_then(|line| line.split_whitespace().nth(1))
        .expect("office list shows the worker's key")
        .to_string();
    assert_success(&area.forklift("a", &["office", "retire", &worker_key_id, "--compromised", "--offline"]));
}

/// A merge signed here makes the merged history this warehouse's, so the stack completing a
/// conflicted consolidation judges the merged head under the office it signs under, not the one
/// the consolidation began under: a worker's parcel, vouched for then, refuses once the worker's
/// key is retired with a boundary that does not hold it. Revert: drop the stack's re-verify, and
/// the stack signs the merge.
#[test]
fn a_conflicted_consolidation_is_judged_under_the_office_its_stack_signs_under() {
    let area = TestArea::new("take-compromised-stack");
    let (_server, id, worked) = worker_feat(&area);
    let main = pallet_head(&area, "a", "main");

    let consolidated = area.forklift("a", &["consolidate", "feat"]);
    assert_success(&consolidated);
    assert!(stdout(&consolidated).contains("conflict: code.txt"), "{}", stdout(&consolidated));
    retire_worker_beyond_feat(&area, &id);

    area.write_file("a/code.txt", "resolved\n");
    assert_success(&area.forklift("a", &["load", "code.txt"]));
    let stacked = area.forklift("a", &["--json", "stack", "resolved"]);
    let message = history_unverified_message(&stacked);
    assert!(message.contains(&worked), "{}", message);
    assert!(message.contains("Nothing was stacked."), "{}", message);
    assert_eq!(pallet_head(&area, "a", "main"), main);
}

/// A haul merge judges the proposed head before merging anything: the worker's parcel refuses
/// once the worker's key is retired with a boundary that does not hold it. Revert: drop haul
/// merge's verify, and the merge goes ahead.
#[test]
fn a_haul_merge_refuses_a_head_its_office_does_not_vouch_for() {
    let area = TestArea::new("take-compromised-haul");
    let (_server, id, worked) = worker_feat(&area);
    let main = pallet_head(&area, "a", "main");

    retire_worker_beyond_feat(&area, &id);
    assert_success(&area.forklift("a", &["haul", "reopen", &id]));
    let merged = area.forklift("a", &["--json", "haul", "merge", &id]);
    let message = history_unverified_message(&merged);
    assert!(message.contains(&worked), "{}", message);
    assert_eq!(pallet_head(&area, "a", "main"), main);
    assert_eq!(area.read_file("a/code.txt"), "admin\n");
}

/// At first contact the office a remote serves is verified against the anchor it serves before
/// anything is installed: an anchor from another warehouse over this one's office refuses, and
/// leaves no anchor or office behind. Revert: drop `prepare_remote_trust`'s office-chain check,
/// and the franchise installs the foreign anchor.
#[test]
fn first_contact_refuses_an_office_its_served_anchor_does_not_root() {
    let area = TestArea::new("take-foreign-anchor");
    let server = Server::start(&area, None);
    prepare_warehouse(&area, "a", &server.url);
    stack_file(&area, "a", "code.txt", "v1\n");
    assert_success(&area.forklift("a", &["office", "enroll"]));
    assert_success(&area.forklift("a", &["lift"]));

    // Another warehouse enrolls an office of its own; its anchor replaces the remote's.
    prepare_warehouse(&area, "x", "http://127.0.0.1:1");
    stack_file(&area, "x", "code.txt", "x\n");
    assert_success(&area.forklift("x", &["office", "enroll", "--offline"]));
    std::fs::copy(area.path("x/.forklift/trust"), area.path("server-root/.forklift/trust")).unwrap();

    let franchised = area.forklift(".", &["--json", "franchise", &server.url, "d"]);
    let message = franchise_history_unverified_message(&franchised);
    assert!(message.contains("The office history"), "{}", message);
    assert!(!area.path("d/.forklift/trust").exists(), "no anchor installed");
    assert!(!walk(&area.path("d/.forklift")).iter().any(|entry| entry.starts_with(area.path("d/.forklift/meta"))),
            "no office installed");
}

/// An anchored remote whose default pallet is unborn still franchises: there is no history to
/// judge, so the anchor and office are installed and the pallet starts unborn.
#[test]
fn an_anchored_remote_with_an_unborn_default_pallet_franchises() {
    let area = TestArea::new("take-unborn");
    let server = Server::start(&area, None);
    prepare_warehouse(&area, "a", &server.url);
    stack_file(&area, "a", "code.txt", "v1\n");
    assert_success(&area.forklift("a", &["office", "enroll"]));
    assert_success(&area.forklift("a", &["palletize", "feature"]));
    stack_file(&area, "a", "code.txt", "feature\n");
    assert_success(&area.forklift("a", &["lift"]));
    assert_eq!(remote_pallet_head(&server, "main"), None);

    let franchised = area.forklift(".", &["franchise", &server.url, "b"]);
    assert_success(&franchised);
    assert!(stdout(&franchised).contains("starts unborn"), "{}", stdout(&franchised));
    assert!(area.path("b/.forklift/trust").is_file());
    assert_success(&area.forklift("b", &["lower", "feature"]));
}

fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() { out.extend(walk(&path)); } else { out.push(path); }
    }
    out
}
