//! The progress-events channel, end to end: a real pipe handed to the binary at a known fd
//! through `PULT_EVENTS`, read back line by line. The channel exists on macOS and Linux only.
#![cfg(any(target_os = "macos", target_os = "linux"))]

mod channel;

use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

/// Path to the compiled forklift binary (provided by cargo for integration tests).
const FORKLIFT: &str = env!("CARGO_BIN_EXE_forklift");

/// The longest line the emitter may write, newline included.
const MAX_LINE_BYTES: usize = 512;

/// A temporary warehouse directory, with its global configuration and key directory kept
/// outside it, deleted when the test ends.
struct TestWarehouse {
    root: PathBuf,
    home: PathBuf,
}

impl TestWarehouse {
    fn new(name: &str) -> TestWarehouse {
        let root = std::env::temp_dir()
            .join(format!("forklift-events-{}-{}", name, std::process::id()));
        let home = std::env::temp_dir()
            .join(format!("forklift-events-{}-{}-home", name, std::process::id()));

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        TestWarehouse { root, home }
    }

    fn write_file(&self, relative_path: &str, content: &str) {
        let path = self.root.join(relative_path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(FORKLIFT);

        command.args(args)
            .current_dir(&self.root)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("test-keys"))
            .env_remove("PULT_EVENTS")
            .stdin(Stdio::null());

        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Run with `fd` as the child's channel (see [`channel::run_with_channel`]).
    fn run_with_channel(&self, args: &[&str], fd: OwnedFd) -> Output {
        channel::run_with_channel(&mut self.command(args), fd)
    }

    /// Prepare the warehouse with an operator and one stacked parcel, so there is something
    /// to compact.
    fn prepare_with_a_parcel(&self) {
        assert_success(&self.run(&["prepare"]));
        assert_success(&self.run(&["config", "--global", "operator.name", "Test Operator"]));
        assert_success(&self.run(&["config", "--global", "operator.identifier", "test@forklift"]));

        for index in 0..20 {
            self.write_file(&format!("src/file-{}.txt", index), &format!("content {}\n", index));
        }

        assert_success(&self.run(&["load", "."]));
        assert_success(&self.run(&["stack", "first parcel"]));
    }
}

impl Drop for TestWarehouse {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed ({:?}).\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Run `args` with a fresh pipe as the channel and return the output and every line read.
fn run_and_collect(warehouse: &TestWarehouse, args: &[&str]) -> (Output, Vec<String>) {
    let (output, text) = channel::run_and_collect(&mut warehouse.command(args));

    assert!(text.is_empty() || text.ends_with('\n'), "a partial line was written: {:?}", text);

    (output, text.lines().map(str::to_string).collect())
}

/// Every line is one well-formed `status` or `progress` event: never `step` (Forklift never
/// owns the channel), never a control character, never longer than the atomic-write bound.
fn assert_protocol(lines: &[String]) {
    for line in lines {
        assert!(line.len() < MAX_LINE_BYTES, "line over the cap: {:?}", line);
        assert!(!line.chars().any(char::is_control), "control character in {:?}", line);

        if let Some(rest) = line.strip_prefix("status ") {
            assert!(!rest.trim().is_empty(), "a status names its phase: {:?}", line);
        } else if let Some(rest) = line.strip_prefix("progress ") {
            let value = rest.split(' ').next().unwrap();
            assert!(value == "?" || value.parse::<u8>().is_ok_and(|percent| percent <= 100),
                    "not a progress value: {:?}", line);
        } else {
            panic!("not a status or progress event: {:?}", line);
        }
    }
}

/// Within each phase, numeric progress never goes down: a reset is only allowed after a status
/// naming the new phase.
fn assert_monotonic_within_phases(lines: &[String]) {
    let mut high = 0u8;

    for line in lines {
        if line.starts_with("status ") {
            high = 0;
        } else if let Some(Ok(percent)) = line.strip_prefix("progress ")
            .map(|rest| rest.split(' ').next().unwrap().parse::<u8>()) {
            assert!(percent >= high, "progress went from {} down to {} within a phase: {:?}", high, percent, lines);
            high = percent;
        }
    }
}

/// The statuses seen are `known`'s phases in order, and include every phase in `kept`.
///
/// A status still queued when the next status arrives is replaced by it, so a phase with no
/// progress before the next one may legitimately never reach the reader. `kept` names only the
/// phases that cannot be replaced that way: one followed by progress, or the run's last.
fn assert_phases(lines: &[String], known: &[&str], kept: &[&str]) {
    let seen: Vec<&str> = lines.iter().filter_map(|line| line.strip_prefix("status ")).collect();

    let mut remaining = known.iter();
    for phase in &seen {
        assert!(remaining.any(|known| known == phase),
                "status {:?} is unknown or out of order; expected a subsequence of {:?}, saw {:?}",
                phase, known, seen);
    }

    for phase in kept {
        assert!(seen.contains(phase), "missing status {:?} in {:?}", phase, lines);
    }
}

#[test]
fn compact_reports_its_phases_on_the_channel() {
    let warehouse = TestWarehouse::new("compact");
    warehouse.prepare_with_a_parcel();

    let (output, lines) = run_and_collect(&warehouse, &["--json", "compact"]);
    assert_success(&output);

    assert_protocol(&lines);
    assert_monotonic_within_phases(&lines);

    assert_phases(&lines,
                  &["Collecting objects", "Packing objects", "Removing packed originals"],
                  &["Packing objects", "Removing packed originals"]);

    // The channel is separate from stdout, which stays exactly one JSON document.
    let document: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("stdout is not one JSON document ({}): {}",
                                       error, String::from_utf8_lossy(&output.stdout)));
    assert_eq!(document["ok"], true, "{}", document);
}

#[test]
fn import_git_reports_its_phases_on_the_channel() {
    let warehouse = TestWarehouse::new("import-git");

    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&warehouse.root)
            .env("GIT_AUTHOR_NAME", "Ada").env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada").env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .output()
            .expect("git must be installed to run this test");
        assert!(output.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&output.stderr));
    };

    git(&["init", "-q", "-b", "main"]);
    warehouse.write_file("a.txt", "hello\n");
    git(&["add", "-A"]);
    git(&["commit", "-qm", "first commit"]);
    warehouse.write_file("b.txt", "more\n");
    git(&["add", "-A"]);
    git(&["commit", "-qm", "second commit"]);

    assert_success(&warehouse.run(&["prepare"]));
    assert_success(&warehouse.run(&["config", "--global", "operator.name", "Test Operator"]));
    assert_success(&warehouse.run(&["config", "--global", "operator.identifier", "test@forklift"]));

    let (output, lines) = run_and_collect(&warehouse, &["import-git", "."]);
    assert_success(&output);

    assert_protocol(&lines);
    assert_monotonic_within_phases(&lines);

    assert_phases(&lines,
                  &["Converting commits", "Publishing objects", "Building the inventory"],
                  &["Converting commits"]);
}

#[test]
fn a_reader_that_has_gone_away_never_kills_the_operation() {
    let warehouse = TestWarehouse::new("gone");
    warehouse.prepare_with_a_parcel();

    // Close the read end before the child starts: every write it attempts hits a broken pipe.
    let (read_end, write_end) = channel::pipe();
    drop(read_end);

    let output = warehouse.run_with_channel(&["--json", "compact"], write_end);

    assert_eq!(output.status.signal(), None, "the process was killed by a signal: {:?}", output.status);
    assert_success(&output);
    serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .unwrap_or_else(|error| panic!("stdout is not one JSON document ({})", error));
}

#[test]
fn a_socket_whose_peer_has_gone_never_kills_the_operation() {
    let warehouse = TestWarehouse::new("socket-gone");
    warehouse.prepare_with_a_parcel();

    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    drop(theirs);

    let output = warehouse.run_with_channel(&["--json", "compact"], OwnedFd::from(ours));

    assert_eq!(output.status.signal(), None, "the process was killed by a signal: {:?}", output.status);
    assert_success(&output);
}

#[test]
fn children_never_write_into_the_channel() {
    let warehouse = TestWarehouse::new("children");
    warehouse.prepare_with_a_parcel();

    // The MCP server runs every tool call as a child `forklift`. The server itself reports
    // nothing, so any line on the channel came from a child that inherited it.
    let input_path = warehouse.home.join("mcp-input");
    std::fs::write(&input_path, concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#, "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"compact","arguments":{}}}"#, "\n",
    )).unwrap();

    let mut command = warehouse.command(&["mcp"]);
    command.stdin(std::fs::File::open(&input_path).unwrap());
    let (output, text) = channel::run_and_collect(&mut command);
    assert_success(&output);

    let reply = String::from_utf8_lossy(&output.stdout).lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|message| message["id"] == 2)
        .expect("a reply to the compact call");
    assert_eq!(reply["result"]["isError"], false, "compact failed: {}", reply);

    assert_eq!(text, "", "a child wrote into the channel");
}

/// Run `forklift mcp` over one `compact` call, with the protocol version and the call's
/// `_meta` given, and return every message the server wrote, in order.
fn mcp_compact(warehouse: &TestWarehouse, protocol: &str, meta: &str) -> Vec<serde_json::Value> {
    let input_path = warehouse.home.join("mcp-input");
    std::fs::write(&input_path, format!(
        "{}\n{}\n{}\n",
        format_args!(r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"{}"}}}}"#, protocol),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        format_args!(r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"compact","arguments":{{}}{}}}}}"#, meta),
    )).unwrap();

    let mut command = warehouse.command(&["mcp"]);
    command.stdin(std::fs::File::open(&input_path).unwrap());
    let output = command.output().unwrap();
    assert_success(&output);

    String::from_utf8_lossy(&output.stdout).lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// The `notifications/progress` messages, after checking that they all come before the call's
/// response and carry `token`.
fn progress_before_the_response(messages: &[serde_json::Value], token: &serde_json::Value) -> Vec<serde_json::Value> {
    let response = messages.iter().position(|message| message["id"] == 2).expect("a reply to the compact call");
    assert_eq!(messages[response]["result"]["isError"], false, "compact failed: {}", messages[response]);

    let is_progress = |message: &&serde_json::Value| message["method"] == "notifications/progress";
    assert!(!messages[response..].iter().any(|message| is_progress(&message)), "progress after the response: {:?}", messages);

    let notifications: Vec<serde_json::Value> = messages[..response].iter().filter(is_progress).cloned().collect();
    for notification in &notifications {
        assert_eq!(&notification["params"]["progressToken"], token, "{}", notification);
    }
    notifications
}

#[test]
fn an_mcp_call_with_a_progress_token_gets_progress_notifications() {
    let warehouse = TestWarehouse::new("mcp-progress");
    warehouse.prepare_with_a_parcel();

    let messages = mcp_compact(&warehouse, "2025-06-18", r#","_meta":{"progressToken":"p1"}"#);
    let notifications = progress_before_the_response(&messages, &serde_json::json!("p1"));
    assert!(!notifications.is_empty(), "no progress notifications: {:?}", messages);

    // The first goes out at the first event and the last when the run ends, in another phase, so
    // there are always two to compare.
    assert!(notifications.len() >= 2, "one notification leaves nothing to compare: {:?}", notifications);

    // `progress` strictly increases and there is never a `total`.
    let values: Vec<f64> = notifications.iter().map(|n| n["params"]["progress"].as_f64().unwrap()).collect();
    assert!(values.windows(2).all(|pair| pair[0] < pair[1]), "not strictly increasing: {:?}", values);
    assert!(notifications.iter().all(|n| n["params"].get("total").is_none()), "a total was sent: {:?}", notifications);

    // Under 2025-06-18 every notification has a `message`, and the last names the last phase.
    assert!(notifications.iter().all(|n| n["params"]["message"].is_string()), "{:?}", notifications);
    let last = notifications.last().unwrap()["params"]["message"].as_str().unwrap().to_string();
    assert!(last.starts_with("Removing packed originals"), "last message: {:?}", last);
}

#[test]
fn an_mcp_progress_notification_has_no_message_before_protocol_2025_06_18() {
    let warehouse = TestWarehouse::new("mcp-progress-old");
    warehouse.prepare_with_a_parcel();

    let messages = mcp_compact(&warehouse, "2024-11-05", r#","_meta":{"progressToken":7}"#);
    let notifications = progress_before_the_response(&messages, &serde_json::json!(7));
    assert!(!notifications.is_empty(), "no progress notifications: {:?}", messages);
    assert!(notifications.iter().all(|n| n["params"].get("message").is_none()), "{:?}", notifications);
}

#[test]
fn an_mcp_call_without_a_progress_token_gets_no_notifications() {
    let warehouse = TestWarehouse::new("mcp-no-progress");
    warehouse.prepare_with_a_parcel();

    let messages = mcp_compact(&warehouse, "2025-06-18", "");
    assert!(progress_before_the_response(&messages, &serde_json::Value::Null).is_empty(), "{:?}", messages);

    // Nor with a token MCP does not allow: only a string or an integer names a request.
    for token in ["null", "{}", "[1]", "1.5", "true"] {
        let messages = mcp_compact(&warehouse, "2025-06-18", &format!(r#","_meta":{{"progressToken":{}}}"#, token));
        let notifications: Vec<_> = messages.iter().filter(|m| m["method"] == "notifications/progress").collect();
        assert!(notifications.is_empty(), "token {}: {:?}", token, notifications);
    }
}

#[test]
fn a_regular_file_is_never_written_as_a_channel() {
    let warehouse = TestWarehouse::new("regular-file");
    warehouse.prepare_with_a_parcel();

    let path = warehouse.home.join("not-a-channel");
    let file = std::fs::File::create(&path).unwrap();

    let output = warehouse.run_with_channel(&["compact"], OwnedFd::from(file));
    assert_success(&output);

    assert_eq!(std::fs::read(&path).unwrap(), b"", "the emitter wrote to a regular file");
}

#[test]
fn a_standard_stream_is_never_used_as_a_channel() {
    let warehouse = TestWarehouse::new("stdout");
    warehouse.prepare_with_a_parcel();

    for value in ["0", "1", "2"] {
        let output = warehouse.command(&["--json", "compact"])
            .env("PULT_EVENTS", value)
            .output()
            .unwrap();
        assert_success(&output);

        // A stray event on stdout would break the single document.
        let document: serde_json::Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("PULT_EVENTS={}: stdout is not one JSON document ({}): {}",
                                           value, error, String::from_utf8_lossy(&output.stdout)));
        assert_eq!(document["ok"], true);

        assert!(!String::from_utf8_lossy(&output.stderr).contains("status "),
                "PULT_EVENTS={}: an event reached stderr", value);
    }
}

#[test]
fn an_unusable_variable_leaves_the_command_unaffected() {
    let warehouse = TestWarehouse::new("unusable");
    warehouse.prepare_with_a_parcel();

    // Not digits, a sign, whitespace, and a number with nothing open behind it.
    for value in ["", "seven", "+7", " 7", "7 ", "-1", "99999999999999999999", "200"] {
        let output = warehouse.command(&["--json", "compact"])
            .env("PULT_EVENTS", value)
            .output()
            .unwrap();

        assert_success(&output);
        serde_json::from_slice::<serde_json::Value>(&output.stdout)
            .unwrap_or_else(|error| panic!("PULT_EVENTS={:?}: stdout is not one JSON document ({})", value, error));
    }
}

#[test]
fn an_mcp_call_reporting_progress_finishes_after_its_client_has_gone() {
    use std::io::{BufRead, Write};

    let warehouse = TestWarehouse::new("mcp-progress-gone");
    warehouse.prepare_with_a_parcel();
    let pack = warehouse.root.join(".forklift/objects/pack");
    assert!(!pack.exists(), "packed before the call");

    let mut child = warehouse.command(&["mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\"}}\n").unwrap();
    let mut reply = String::new();
    std::io::BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut reply).unwrap();
    assert!(reply.contains("\"id\":1"), "{}", reply);

    // The client is gone before the call: every progress notification and the response fail.
    drop(child.stdout.take());
    stdin.write_all(br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"compact","arguments":{},"_meta":{"progressToken":"p1"}}}"#).unwrap();
    stdin.write_all(b"\n").unwrap();
    drop(stdin);

    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.signal(), None, "killed by a signal: {:?}", output.status);
    assert!(output.status.success(), "{:?}: {}", output.status, String::from_utf8_lossy(&output.stderr));
    assert!(pack.is_dir(), "the call did not run to the end");
}
