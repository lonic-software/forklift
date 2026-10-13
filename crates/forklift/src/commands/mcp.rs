//! `forklift mcp` — the Model Context Protocol server (DESIGN.html §7.4): the whole
//! command surface as native MCP tools, so an agent calls schema-typed tools instead
//! of shelling out and scraping prose.
//!
//! Transport is the MCP stdio convention: newline-delimited JSON-RPC 2.0 on
//! stdin/stdout. A `tools/call` re-invokes this same binary with `--json` and captures
//! its envelope, so every tool reuses the exact structured output (and the warehouse
//! lock, and the exit-code taxonomy) the CLI already produces — one source of truth.
//!
//! Requests are answered one at a time, on this one thread. A call that asked for progress
//! (`_meta.progressToken`) gets its `notifications/progress` written by that same thread while
//! the child runs, and always before the call's response, so none can follow it.

// Off macOS and Linux no call reports progress, so the event and notification machinery is unused there.
#![cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]

use std::io::{BufRead, Write};
use serde_json::{json, Value};

/// The MCP protocol version this server defaults to when a client does not pin one.
/// The `initialize` handshake echoes the client's version when it sends one.
const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Run the MCP server: read JSON-RPC messages line by line, answer each request, and
/// exit cleanly when stdin closes (the client disconnected).
///
/// Runs until stdin reaches EOF; never requires a warehouse itself (each tool call
/// enters one as its own process, from this server's working directory).
///
/// # Arguments
/// * `root` - The warehouse to serve. When set, the server changes into it so every
///            tool subprocess resolves that warehouse; otherwise the working directory
///            (discovered upward, as usual) is used — the natural fit when an MCP
///            client launches the server inside a warehouse.
///
/// # Returns
/// * `Ok(())`      - When stdin closes.
/// * `Err(String)` - If the root is unusable, or stdin/stdout fails irrecoverably.
pub fn handle_command(root: Option<String>) -> Result<(), String> {
    if let Some(root) = root {
        std::env::set_current_dir(&root)
            .map_err(|e| format!("Error while changing into the warehouse \"{}\": {}", root, e))?;
    }

    // A client that goes away must not kill this server mid-call: the call's child would be
    // orphaned, and die on its own next write to the stdout nobody reads any more. With the
    // signal ignored, the write fails instead, and the loop ends once the call has finished.
    // Each child restores the default disposition in its own `main`.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    // State kept for the life of this connection — see `ServerState`.
    let mut state = ServerState::new();

    for line in stdin.lock().lines() {
        let line = line.map_err(|e| format!("Error while reading stdin: {}", e))?;

        if line.trim().is_empty() {
            continue;
        }

        // A notification sent mid-call that cannot be written is not fatal on its own: the
        // response that follows hits the same stdout and reports the failure.
        let mut notify = |notification: &Value| {
            let _ = write_message(&mut stdout, notification);
        };

        let Some(response) = handle_message(&line, &mut state, &mut notify) else {
            // A notification (no id) gets no response.
            continue;
        };

        write_message(&mut stdout, &response)?;
    }

    Ok(())
}

/// Write one JSON-RPC message as a line and flush it.
fn write_message(stdout: &mut std::io::Stdout, message: &Value) -> Result<(), String> {
    let text = serde_json::to_string(message)
        .map_err(|e| format!("Error while encoding a response: {}", e))?;

    writeln!(stdout, "{}", text).map_err(|e| format!("Error while writing stdout: {}", e))?;
    stdout.flush().map_err(|e| format!("Error while flushing stdout: {}", e))
}

/// State the server keeps for the life of one stdio connection. It exists to **harden
/// provenance** (§7.2): the `tool` (the client that drove the model) and the `session` are
/// taken from the connection here, not from the model's own tool-call arguments, so an agent
/// cannot self-report a false tool or session. The `model` stays agent-attested — MCP carries
/// no model identity, so nothing at the transport can supply or verify it.
struct ServerState {
    /// A session id for this connection. One server process serves one stdio client, so a
    /// process maps 1:1 to a session.
    session: String,

    /// The client application from `initialize` (`name` or `name/version`), once it has
    /// identified itself. Used as the provenance `tool`.
    client: Option<String>,

    /// The protocol version `initialize` settled on. It decides whether a progress notification
    /// may carry a `message`.
    protocol_version: String,
}

impl ServerState {
    fn new() -> ServerState {
        // Unique per server process (a process is one connection). Not security-bearing —
        // just a stable, connection-scoped label the agent cannot forge into a provenance.
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0);

        ServerState {
            session: format!("mcp-{:x}-{:x}", std::process::id(), millis),
            client: None,
            protocol_version: DEFAULT_PROTOCOL_VERSION.to_string(),
        }
    }
}

/// Handle one JSON-RPC message; `None` for a notification (which gets no reply) or an
/// unparseable line we choose to ignore rather than crash the session.
///
/// `notify` writes a notification to the client; a `tools/call` uses it for progress.
fn handle_message(line: &str, state: &mut ServerState, notify: &mut dyn FnMut(&Value)) -> Option<Value> {
    let message: Value = serde_json::from_str(line).ok()?;

    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str)?;

    // A message without an id is a notification: act on nothing, answer nothing.
    let id = id?;

    let params = message.get("params").cloned().unwrap_or(json!({}));

    let result = match method {
        "initialize" => Ok(initialize(&params, state)),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => call_tool(&params, state, notify),
        other => Err(JsonRpcError::method_not_found(other)),
    };

    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error.to_value() }),
    })
}

/// Build the `initialize` result: echo the client's protocol version (or the default),
/// advertise the tools capability, and name the server. Also captures the client identity
/// so provenance can record the `tool` from the connection instead of the agent's word.
fn initialize(params: &Value, state: &mut ServerState) -> Value {
    if let Some(info) = params.get("clientInfo") {
        if let Some(name) = info.get("name").and_then(Value::as_str) {
            state.client = Some(match info.get("version").and_then(Value::as_str) {
                Some(version) => format!("{}/{}", name, version),
                None => name.to_string(),
            });
        }
    }

    let protocol_version = params.get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);

    state.protocol_version = protocol_version.to_string();

    json!({
        "protocolVersion": protocol_version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "forklift", "version": SERVER_VERSION },
    })
}

/// Dispatch a `tools/call`: map the tool and its arguments to a `forklift … --json`
/// invocation, run it, and return the envelope as the tool result (see [`tool_result`]). When the
/// request carries `_meta.progressToken`, the run's progress events are sent to the client as
/// `notifications/progress` through `notify` while it runs.
fn call_tool(params: &Value, state: &ServerState, notify: &mut dyn FnMut(&Value)) -> Result<Value, JsonRpcError> {
    let name = params.get("name").and_then(Value::as_str)
        .ok_or_else(|| JsonRpcError::invalid_params("the tool name is required"))?;

    let mut arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    // Provenance hardening (§7.2): the `tool` and `session` are set from this MCP connection,
    // overriding anything the agent supplied — the harness identifies the tool, not the
    // model's own output. `model` stays the agent's attestation (MCP carries no model id).
    if name == "manifest_provenance" {
        if let Some(object) = arguments.as_object_mut() {
            object.insert("session".to_string(), json!(state.session));
            if let Some(client) = &state.client {
                object.insert("tool".to_string(), json!(client));
            } else {
                object.remove("tool");
            }
        }
    }

    let args = build_args(name, &arguments)
        .map_err(|reason| JsonRpcError::invalid_params(&reason))?;

    // MCP allows a string or an integer; anything else names no request a client could match.
    let token = params.get("_meta").and_then(|meta| meta.get("progressToken"))
        .filter(|token| token.is_string() || token.is_i64() || token.is_u64());

    let output = match token {
        Some(token) => {
            let mut progress = ProgressMapper::new(carries_message(&state.protocol_version));
            run_forklift_reporting(&args, &mut progress, &mut |value, message| {
                notify(&progress_notification(token, value, message));
            })
        }
        None => run_forklift(&args),
    }.map_err(|reason| JsonRpcError::internal(&reason))?;

    Ok(tool_result(&output.stdout, &output.stderr, output.status))
}

/// Build a tool result from a finished run.
///
/// * The first text item is the run's `--json` document. `isError` follows its `ok`, not the
///   exit status: a command may exit non-zero with `ok: true` to report a negative outcome as
///   data.
/// * A run that left no document — killed by a signal, say — has nothing to forward. Its first
///   item says so and names the exit status, and it is always an error.
/// * Every stderr line that is a warning document follows, one item each. Other stderr is not
///   forwarded.
///
/// # Arguments
/// * `stdout` - The run's standard output.
/// * `stderr` - The run's standard error.
/// * `status` - How the run ended.
///
/// # Returns
/// * `Value` - The `tools/call` result.
fn tool_result(stdout: &[u8], stderr: &[u8], status: std::process::ExitStatus) -> Value {
    // A document is an object with a boolean `ok`; anything else on stdout is not one.
    let ok = serde_json::from_slice::<Value>(stdout).ok()
        .and_then(|document| document.get("ok").and_then(Value::as_bool));

    let mut content = vec![match ok {
        Some(_) => json!({ "type": "text", "text": String::from_utf8_lossy(stdout) }),
        None => json!({ "type": "text", "text": no_document_text(stdout, stderr, status) }),
    }];

    for line in String::from_utf8_lossy(stderr).lines() {
        if is_warning_document(line) {
            content.push(json!({ "type": "text", "text": line }));
        }
    }

    json!({ "content": content, "isError": ok != Some(true) })
}

/// The first item of a result with no document: how the run ended, then the end of whatever it
/// did write, which for a panic is the only account of what happened.
fn no_document_text(stdout: &[u8], stderr: &[u8], status: std::process::ExitStatus) -> String {
    let mut text = format!("forklift left no result document: it {}.", describe_exit(status));

    let stdout = String::from_utf8_lossy(stdout);
    if !stdout.trim().is_empty() {
        text.push_str("\n\nstdout:\n");
        text.push_str(tail(stdout.trim_end()));
    }

    let stderr = String::from_utf8_lossy(stderr);
    let stderr: Vec<&str> = stderr.lines().filter(|line| !is_warning_document(line)).collect();
    let stderr = stderr.join("\n");
    if !stderr.trim().is_empty() {
        text.push_str("\n\nstderr:\n");
        text.push_str(tail(stderr.trim_end()));
    }

    text
}

/// The last [`TAIL_BYTES`] of `text`, cut on a character boundary.
fn tail(text: &str) -> &str {
    let mut start = text.len().saturating_sub(TAIL_BYTES);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// How much of a stream a result with no document quotes, from its end.
const TAIL_BYTES: usize = 4096;

/// Whether a stderr line is a warning document: a JSON object carrying `lonic_json` and a string
/// `warning`.
fn is_warning_document(line: &str) -> bool {
    serde_json::from_str::<Value>(line).ok()
        .is_some_and(|value| value.get("lonic_json").is_some() && value.get("warning").is_some_and(Value::is_string))
}

/// How a run ended, as the tail of a sentence: "exited with status 3", "was killed by signal 9".
fn describe_exit(status: std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exited with status {}", code);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        if let Some(signal) = status.signal() {
            return format!("was killed by signal {}", signal);
        }
    }

    "ended without an exit status".to_string()
}

/// Run this same binary with the given arguments plus `--json`, capturing its output.
fn run_forklift(args: &[String]) -> Result<std::process::Output, String> {
    forklift_command(args)?.output()
        .map_err(|e| format!("Error while running \"forklift {}\": {}", args.join(" "), e))
}

/// The command for one tool call: this same binary, the given arguments, and `--json`.
fn forklift_command(args: &[String]) -> Result<std::process::Command, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("Cannot locate the forklift binary: {}", e))?;

    let mut command = std::process::Command::new(exe);
    command.args(args).arg("--json");

    Ok(command)
}

/// How often at most a progress notification is sent for one call.
const NOTIFICATION_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// [`run_forklift`], with a progress-events channel the server owns: the child gets the write
/// end of a fresh pipe through `PULT_EVENTS`, and each event it reports goes into `progress`.
/// What `progress` decides to send goes to `send`, the last of it after the child has exited and
/// before this returns.
///
/// The channel exists on macOS and Linux only, as the child's emitter does; elsewhere this is
/// [`run_forklift`].
///
/// # Arguments
/// * `args`    - The forklift arguments.
/// * `progress` - Takes each event the child reports.
/// * `send`     - Sends one notification: `progress` and `message`.
///
/// # Returns
/// * `Ok(Output)`  - The finished run.
/// * `Err(String)` - If the channel could not be set up, or the child could not be run.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn run_forklift_reporting(
    args: &[String],
    progress: &mut ProgressMapper,
    send: &mut dyn FnMut(f64, Option<String>),
) -> Result<std::process::Output, String> {
    use std::io::{BufRead, Read};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::sync::mpsc::RecvTimeoutError;

    let failed = |e: std::io::Error| format!("Error while running \"forklift {}\": {}", args.join(" "), e);

    let (reader, writer) = std::io::pipe().map_err(failed)?;

    // The child must find the write end above the standard streams, and still open after exec.
    // So it is duplicated to a number of at least 3, close-on-exec here, and only the child
    // clears that flag, after the fork, so no other process this server runs inherits it.
    let duplicate = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate == -1 {
        return Err(failed(std::io::Error::last_os_error()));
    }
    let child_end = unsafe { OwnedFd::from_raw_fd(duplicate) };
    drop(writer);

    let mut command = forklift_command(args)?;
    command.env("PULT_EVENTS", duplicate.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // SAFETY: the closure only calls `fcntl`, which is async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(duplicate, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command.spawn().map_err(failed)?;

    // Only the child may hold the write end now, so the reader sees end-of-file when it exits.
    drop(child_end);

    let drain = |stream: Option<Box<dyn Read + Send>>| std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut stream) = stream {
            let _ = stream.read_to_end(&mut bytes);
        }
        bytes
    });
    let stdout = drain(child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
    let stderr = drain(child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>));

    let (sender, events) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(reader).lines() {
            let Ok(line) = line else { break };
            if let Some(event) = parse_event(&line) {
                if sender.send(event).is_err() {
                    break;
                }
            }
        }
    });

    // The channel ends when the child does: it holds the only write end, and keeps it from
    // anything it runs. Something that held it anyway would hold the child's stdout too, which
    // the joins below wait on regardless, so a timeout here would buy nothing.
    loop {
        match events.recv_timeout(NOTIFICATION_INTERVAL / 2) {
            Ok(event) => progress.receive(event),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if let Some((value, message)) = progress.take(std::time::Instant::now()) {
            send(value, message);
        }
    }

    let status = child.wait().map_err(failed)?;
    if let Some((value, message)) = progress.finish() {
        send(value, message);
    }

    Ok(std::process::Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn run_forklift_reporting(
    args: &[String],
    _progress: &mut ProgressMapper,
    _send: &mut dyn FnMut(f64, Option<String>),
) -> Result<std::process::Output, String> {
    run_forklift(args)
}

/// One event read from the channel.
#[derive(Debug, PartialEq)]
enum Reported {
    /// `status <phase>`.
    Status(String),

    /// `progress <0-100|?> [text]`.
    Progress { percent: Option<u8>, text: String },
}

/// Parse one channel line. Anything that is not a well-formed `status` or `progress` event is
/// `None`, and is ignored.
fn parse_event(line: &str) -> Option<Reported> {
    if let Some(phase) = line.strip_prefix("status ") {
        let phase = phase.trim();
        return (!phase.is_empty()).then(|| Reported::Status(phase.to_string()));
    }

    let rest = line.strip_prefix("progress ")?;
    let (value, text) = rest.split_once(' ').unwrap_or((rest, ""));

    let percent = match value {
        "?" => None,
        value => Some(value.parse::<u8>().ok().filter(|percent| *percent <= 100)?),
    };

    Some(Reported::Progress { percent, text: text.trim().to_string() })
}

/// Whether a protocol version carries `message` on a progress notification: 2025-06-18 and
/// later. Versions are dates, so they order as strings; anything else carries none.
fn carries_message(protocol_version: &str) -> bool {
    // Only a well-formed date orders correctly: "draft" sorts after every date, "2025-6-18"
    // before its own month.
    let is_date = protocol_version.len() == 10 && protocol_version.bytes().enumerate()
        .all(|(index, byte)| if index == 4 || index == 7 { byte == b'-' } else { byte.is_ascii_digit() });

    is_date && protocol_version >= "2025-06-18"
}

/// What a notification that only carries new text adds to the last `progress` sent. A power of
/// two, so the sums stay exact.
const TEXT_ONLY_STEP: f64 = 1.0 / 4096.0;

/// How many text-only notifications fit between two whole values: the steps never reach the
/// next one, so `progress` keeps meaning 100 × resets + percentage.
const TEXT_ONLY_LIMIT: u32 = 4095;

/// Turns one call's events into progress notifications.
///
/// * No `total` is ever sent: an operation cannot promise it will not start a phase over, and a
///   fraction sent before that would report the whole operation done.
/// * `progress` is 100 × the resets seen + the event's percentage, where a reset is a percentage
///   lower than the previous one received. Each phase starts low again, so a phase advances the
///   value instead of rewinding it.
/// * `progress` strictly increases. An event that would not raise it is still sent if it brings
///   new text, at the last value plus [`TEXT_ONLY_STEP`]; otherwise it is dropped. At most
///   [`TEXT_ONLY_LIMIT`] of those follow one whole value, so they never reach the next one;
///   past that, text is dropped until the value rises.
/// * The phase, the percentage and the text go in `message`, only when the protocol has it.
///   Without it, an event that does not raise the value has nothing to say and is dropped.
/// * At most one notification goes out per [`NOTIFICATION_INTERVAL`], until the run ends.
///
/// Events are folded in as they arrive, and [`ProgressMapper::take`] decides what to send.
struct ProgressMapper {
    /// Whether notifications may carry `message`.
    message: bool,

    /// Percentages that went down, counted so far.
    resets: u32,

    /// The last percentage received.
    last_percent: Option<u8>,

    /// The phase the last `status` named.
    phase: Option<String>,

    /// The highest value received since the last send.
    pending_value: Option<f64>,

    /// The newest text received since the last send.
    pending_message: Option<String>,

    /// Text-only notifications sent since the last whole value.
    text_only: u32,

    /// The last `progress` and `message` sent, and when.
    sent_value: Option<f64>,
    sent_message: Option<String>,
    sent_at: Option<std::time::Instant>,
}

impl ProgressMapper {
    fn new(message: bool) -> ProgressMapper {
        ProgressMapper {
            message,
            resets: 0,
            last_percent: None,
            phase: None,
            pending_value: None,
            pending_message: None,
            text_only: 0,
            sent_value: None,
            sent_message: None,
            sent_at: None,
        }
    }

    /// Fold in one event.
    fn receive(&mut self, event: Reported) {
        match event {
            Reported::Status(phase) => {
                self.pending_message = Some(phase.clone());
                self.phase = Some(phase);
            }
            Reported::Progress { percent, text } => {
                if let Some(percent) = percent {
                    if self.last_percent.is_some_and(|last| percent < last) {
                        self.resets += 1;
                    }
                    self.last_percent = Some(percent);

                    let value = 100.0 * f64::from(self.resets) + f64::from(percent);
                    self.pending_value = Some(self.pending_value.map_or(value, |pending| pending.max(value)));
                }

                self.pending_message = Some(compose_message(self.phase.as_deref(), percent, &text));
            }
        }
    }

    /// What to send at `now`, if anything: `(progress, message)`. Nothing until
    /// [`NOTIFICATION_INTERVAL`] has passed since the last send; what was received meanwhile is
    /// kept for the next call.
    fn take(&mut self, now: std::time::Instant) -> Option<(f64, Option<String>)> {
        if self.sent_at.is_some_and(|at| now.duration_since(at) < NOTIFICATION_INTERVAL) {
            return None;
        }

        let sent = self.decide();
        if sent.is_some() {
            self.sent_at = Some(now);
        }
        sent
    }

    /// What to send once the run has ended, whatever the interval.
    fn finish(&mut self) -> Option<(f64, Option<String>)> {
        self.decide()
    }

    /// What to send of what was received, if anything. Clears what was received.
    fn decide(&mut self) -> Option<(f64, Option<String>)> {
        let value = self.pending_value.take();
        let message = self.pending_message.take().filter(|_| self.message);

        // Received values are whole, and what was sent is a whole value plus less than one, so
        // a received value raises it exactly when it exceeds the sent value's whole part.
        let value = match (value, self.sent_value) {
            (Some(value), Some(sent)) if value > sent => {
                self.text_only = 0;
                value
            }
            (Some(value), None) => value,
            _ => {
                // Nothing raises the value: only new text is worth a notification.
                if message.is_none() || message == self.sent_message {
                    return None;
                }
                match self.sent_value {
                    None => 0.0,
                    Some(_) if self.text_only >= TEXT_ONLY_LIMIT => return None,
                    Some(sent) => {
                        self.text_only += 1;
                        sent.floor() + f64::from(self.text_only) * TEXT_ONLY_STEP
                    }
                }
            }
        };

        self.sent_value = Some(value);
        if message.is_some() {
            self.sent_message = message.clone();
        }

        Some((value, message))
    }
}

/// The `message` for a progress event: "<phase>: <percent>% <text>", leaving out what is absent.
fn compose_message(phase: Option<&str>, percent: Option<u8>, text: &str) -> String {
    let detail = match (percent, text.is_empty()) {
        (Some(percent), true) => format!("{}%", percent),
        (Some(percent), false) => format!("{}% {}", percent, text),
        (None, _) => text.to_string(),
    };

    match (phase, detail.is_empty()) {
        (Some(phase), true) => phase.to_string(),
        (Some(phase), false) => format!("{}: {}", phase, detail),
        (None, _) => detail,
    }
}

/// A `notifications/progress` message.
fn progress_notification(token: &Value, progress: f64, message: Option<String>) -> Value {
    let mut params = json!({ "progressToken": token, "progress": progress });
    if let Some(message) = message {
        params["message"] = json!(message);
    }

    json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": params })
}

/// Map an MCP tool name and its arguments to the forklift CLI arguments. Keeping the
/// mapping here (not a per-command concern) means the tool surface is one auditable
/// list, and every tool inherits the CLI's own validation.
fn build_args(name: &str, arguments: &Value) -> Result<Vec<String>, String> {
    let string_arg = |key: &str| arguments.get(key).and_then(Value::as_str).map(str::to_string);
    let bool_arg = |key: &str| arguments.get(key).and_then(Value::as_bool).unwrap_or(false);

    let require = |key: &str| string_arg(key)
        .ok_or_else(|| format!("the \"{}\" argument is required", key));

    let mut args = Vec::new();

    match name {
        "stocktake" => {
            args.push("stocktake".to_string());
            if bool_arg("summary") {
                args.push("--summary".to_string());
            }
        }
        "stack" => {
            args.push("stack".to_string());
            if let Some(description) = string_arg("description") {
                args.push(description);
            }
        }
        "load" => {
            args.push("load".to_string());
            args.push(require("path")?);
        }
        "remove" => {
            args.push("remove".to_string());
            args.push(require("path")?);
        }
        "unload" => {
            args.push("unload".to_string());
            args.push(require("path")?);
        }
        "diff" => {
            args.push("diff".to_string());
            if bool_arg("staged") {
                args.push("--staged".to_string());
            }
            if let Some(targets) = arguments.get("targets").and_then(Value::as_array) {
                for target in targets {
                    if let Some(target) = target.as_str() {
                        args.push(target.to_string());
                    }
                }
            }
        }
        "history" => {
            args.push("history".to_string());
            if let Some(revision) = string_arg("revision") {
                args.push(revision);
            }
            if let Some(class) = string_arg("class") {
                args.push("--class".to_string());
                args.push(class);
            }
            // Bounded, cursor-pageable walk: `limit` caps the page, and `after` (the
            // `next` cursor from a previous page's data) resumes — so an agent reads
            // history in batches instead of the whole graph.
            if let Some(limit) = arguments.get("limit").and_then(Value::as_u64) {
                args.push("--limit".to_string());
                args.push(limit.to_string());
            }
            if let Some(after) = string_arg("after") {
                args.push("--after".to_string());
                args.push(after);
            }
        }
        "query" => {
            args.push("query".to_string());
            if let Some(revision) = string_arg("revision") {
                args.push(revision);
            }
            for flag in [
                "from", "class", "supervisor", "signer", "grep", "model", "tool", "tag",
                "touches", "where", "after",
            ] {
                if let Some(value) = string_arg(flag) {
                    args.push(format!("--{}", flag));
                    args.push(value);
                }
            }
            // The predicate tree may also arrive as a JSON object (the natural MCP form);
            // it is re-serialized into the same --where the CLI takes.
            if let Some(predicate) = arguments.get("where").filter(|value| value.is_object()) {
                args.push("--where".to_string());
                args.push(predicate.to_string());
            }
            for flag in ["unsupervised", "recorded", "merges", "no_merges"] {
                if arguments.get(flag).and_then(Value::as_bool).unwrap_or(false) {
                    args.push(format!("--{}", flag.replace('_', "-")));
                }
            }
            if let Some(limit) = arguments.get("limit").and_then(Value::as_u64) {
                args.push("--limit".to_string());
                args.push(limit.to_string());
            }
        }
        "conflicts" => args.push("conflicts".to_string()),
        "scope" => args.push("scope".to_string()),
        "expand" => {
            args.push("expand".to_string());
            let paths = arguments.get("paths").and_then(Value::as_array)
                .ok_or("the \"paths\" argument is required")?;
            for path in paths {
                if let Some(path) = path.as_str() {
                    args.push(path.to_string());
                }
            }
        }
        "narrow" => {
            args.push("narrow".to_string());
            let paths = arguments.get("paths").and_then(Value::as_array)
                .ok_or("the \"paths\" argument is required")?;
            for path in paths {
                if let Some(path) = path.as_str() {
                    args.push(path.to_string());
                }
            }
        }
        "store" => args.push("store".to_string()),
        "compact" => {
            args.push("compact".to_string());
            if bool_arg("all") {
                args.push("--all".to_string());
            }
        }
        "audit" => {
            args.push("audit".to_string());
            if let Some(pallet) = string_arg("pallet") {
                args.push(pallet);
            }
        }
        "shift" => {
            args.push("shift".to_string());
            args.push(require("pallet")?);
        }
        "consolidate" => {
            args.push("consolidate".to_string());
            args.push(require("pallet")?);
        }
        "palletize" => {
            args.push("palletize".to_string());
            if let Some(pallet) = string_arg("name") {
                args.push(pallet);
                if let Some(revision) = string_arg("revision") {
                    args.push(revision);
                }
            } else if bool_arg("all") {
                // Listing: include the meta pallets (@office, …).
                args.push("--all".to_string());
            }
        }
        "restore" => {
            args.push("restore".to_string());
            if bool_arg("staged") {
                args.push("--staged".to_string());
            }
            args.push(require("path")?);
        }
        "undo" => args.push("undo".to_string()),
        "heal" => args.push("heal".to_string()),
        "lift" => args.push("lift".to_string()),
        "lower" => args.push("lower".to_string()),
        "office_list" => args.push("office".to_string()),
        "bay_add" => {
            args.push("bay".to_string());
            args.push("add".to_string());
            args.push(require("name")?);
            if let Some(path) = string_arg("path") {
                args.push(path);
            }
            if let Some(scope) = arguments.get("scope").and_then(Value::as_array) {
                for prefix in scope {
                    if let Some(prefix) = prefix.as_str() {
                        args.push("--scope".to_string());
                        args.push(prefix.to_string());
                    }
                }
            }
        }
        "bay_list" => args.push("bay".to_string()),
        "bay_remove" => {
            args.push("bay".to_string());
            args.push("remove".to_string());
            args.push(require("name")?);
        }
        "peek" => {
            args.push("peek".to_string());
            match (string_arg("object"), string_arg("inventory")) {
                (Some(object), _) => args.push(object),
                (None, Some(inventory)) => {
                    args.push("--inventory".to_string());
                    args.push(inventory);
                }
                (None, None) => return Err("pass either \"object\" or \"inventory\"".to_string()),
            }
        }
        "blame" => {
            args.push("blame".to_string());
            args.push(require("path")?);
            if let Some(rev) = string_arg("rev") {
                args.push("--rev".to_string());
                args.push(rev);
            }
        }
        "show" => {
            args.push("show".to_string());
            args.push(require("target")?);
        }
        "cherry_pick" => {
            args.push("cherry-pick".to_string());
            args.push(require("revision")?);
            if let Some(message) = string_arg("message") {
                args.push("--message".to_string());
                args.push(message);
            }
        }
        "deliver" => {
            args.push("deliver".to_string());
            args.push(require("target")?);
            if let Some(message) = string_arg("message") {
                args.push("--message".to_string());
                args.push(message);
            }
        }
        "park" => args.push("park".to_string()),
        "park_list" => {
            args.push("park".to_string());
            args.push("list".to_string());
        }
        "park_pop" => {
            args.push("park".to_string());
            args.push("pop".to_string());
        }
        "tag_create" => {
            args.push("tag".to_string());
            args.push("create".to_string());
            args.push(require("name")?);
            if let Some(revision) = string_arg("revision") {
                args.push(revision);
            }
            if let Some(message) = string_arg("message") {
                args.push("--message".to_string());
                args.push(message);
            }
        }
        "tag_show" => {
            args.push("tag".to_string());
            args.push("show".to_string());
            args.push(require("name")?);
        }
        "tag_list" => {
            args.push("tag".to_string());
            args.push("list".to_string());
        }
        "manifest_note" => {
            args.push("manifest".to_string());
            args.push("note".to_string());
            args.push(require("revision")?);
            args.push("--message".to_string());
            args.push(require("message")?);
        }
        "manifest_approve" => {
            args.push("manifest".to_string());
            args.push("approve".to_string());
            args.push(require("revision")?);
            if let Some(message) = string_arg("message") {
                args.push("--message".to_string());
                args.push(message);
            }
        }
        "manifest_provenance" => {
            args.push("manifest".to_string());
            args.push("provenance".to_string());
            args.push(require("revision")?);
            args.push("--model".to_string());
            args.push(require("model")?);
            if let Some(tool) = string_arg("tool") {
                args.push("--tool".to_string());
                args.push(tool);
            }
            if let Some(session) = string_arg("session") {
                args.push("--session".to_string());
                args.push(session);
            }
            if let Some(transcript) = string_arg("transcript") {
                args.push("--transcript".to_string());
                args.push(transcript);
            }
            if let Some(message) = string_arg("message") {
                args.push("--message".to_string());
                args.push(message);
            }
        }
        "manifest_show" => {
            args.push("manifest".to_string());
            args.push("show".to_string());
            args.push(require("revision")?);
        }
        "haul_open" => {
            args.push("haul".to_string());
            args.push("open".to_string());
            args.push("--target".to_string());
            args.push(require("target")?);
            if let Some(source) = string_arg("source") {
                args.push("--source".to_string());
                args.push(source);
            }
            args.push("--title".to_string());
            args.push(require("title")?);
            if let Some(message) = string_arg("message") {
                args.push("--message".to_string());
                args.push(message);
            }
        }
        "haul_list" => {
            args.push("haul".to_string());
            args.push("list".to_string());
            if let Some(state) = string_arg("state") {
                args.push("--state".to_string());
                args.push(state);
            }
        }
        "haul_show" => {
            args.push("haul".to_string());
            args.push("show".to_string());
            args.push(require("id")?);
        }
        "haul_comment" => {
            args.push("haul".to_string());
            args.push("comment".to_string());
            args.push(require("id")?);
            args.push("--message".to_string());
            args.push(require("message")?);
        }
        "haul_review" => {
            args.push("haul".to_string());
            args.push("review".to_string());
            args.push(require("id")?);
            if bool_arg("request_changes") {
                args.push("--request-changes".to_string());
            }
            if bool_arg("comment") {
                args.push("--comment".to_string());
            }
            if let Some(message) = string_arg("message") {
                args.push("--message".to_string());
                args.push(message);
            }
        }
        "haul_merge" => {
            args.push("haul".to_string());
            args.push("merge".to_string());
            args.push(require("id")?);
        }
        "haul_close" => {
            args.push("haul".to_string());
            args.push("close".to_string());
            args.push(require("id")?);
        }
        "haul_reopen" => {
            args.push("haul".to_string());
            args.push("reopen".to_string());
            args.push(require("id")?);
        }
        other => return Err(format!("unknown tool \"{}\"", other)),
    }

    Ok(args)
}

/// The tool definitions returned by `tools/list`: name, one-line description, and a
/// JSON Schema for the arguments. The set mirrors the read/act commands agents drive.
fn tool_definitions() -> Value {
    let object = |properties: Value, required: Value| json!({
        "type": "object",
        "properties": properties,
        "required": required,
    });

    let string = json!({ "type": "string" });
    let boolean = json!({ "type": "boolean" });
    let integer = json!({ "type": "integer", "minimum": 1 });

    json!([
        tool("stocktake", "Report the current pallet and the staged/unstaged changes. Pass summary=true for counts only.",
            object(json!({ "summary": boolean }), json!([]))),
        tool("stack", "Stack the inventory as a new parcel (commit) on the current pallet.",
            object(json!({ "description": string }), json!([]))),
        tool("load", "Stage a file or directory (its changes) into the inventory.",
            object(json!({ "path": string }), json!(["path"]))),
        tool("remove", "Stage a file or directory for removal (the working directory is not touched).",
            object(json!({ "path": string }), json!(["path"]))),
        tool("unload", "Unstage a file or directory: reset it to the pallet head, keeping the working directory as it is (the inverse of load).",
            object(json!({ "path": string }), json!(["path"]))),
        tool("diff", "Show changed files. Default: working directory vs inventory. staged=true: inventory vs head. targets: two revisions to compare, optionally plus a path.",
            object(json!({ "staged": boolean, "targets": { "type": "array", "items": string } }), json!([]))),
        tool("history", "Walk the parcel history newest first from a revision (default: the current pallet). Read it in pages with limit (max parcels) plus after (the data.next cursor from the previous page); class filters by author identity class (human|agent|bot|service).",
            object(json!({ "revision": string, "class": string, "limit": integer, "after": string }), json!([]))),
        tool("query", "Query parcel history on its signed dimensions - identity class, supervisor, signing key, recorded machine-authorship provenance (model/tool), signed tags - plus parcel facts (dates, description glob, merge-ness, touched paths). Identity answers are verified by default (the signature is checked; the office is joined off the verified signer, never the parcel's own claim); recorded=true opts into the cheap self-declared reading, labeled. A signed-revoked match carries signer.boundary - vouched (inside the revocation's vouched history), suspect (outside it: a forged backdate, or the key's holder kept signing after the revocation), or unresolved (a partial clone is missing one of the boundary's heads, so the question cannot be answered here - never treat this as suspect) - filterable via a where leaf on signer.boundary (values vouched or suspect only). A parcel with no provenance recorded at all never matches model/tool in either direction; an untagged parcel plainly does not carry tag. touches compares against the first parent only, so a merge whose result at the path already matches its first parent does not match even if its other parent touched it. Compose or/not/nesting via `where` (a JSON predicate tree: {all|any|not} over {field,op,value}). Scope the walk with revision and from - that, not limit, bounds verification work. Page with limit plus after (the data.next cursor).",
            object(json!({ "revision": string, "from": string, "class": string, "unsupervised": boolean, "supervisor": string, "signer": string, "grep": string, "model": string, "tool": string, "tag": string, "touches": string, "recorded": boolean, "merges": boolean, "no_merges": boolean, "where": { "type": "object" }, "limit": integer, "after": string }), json!([]))),
        tool("conflicts", "List the files an unresolved consolidation left in conflict, with each side as a content address.",
            object(json!({}), json!([]))),
        tool("compact", "Compact the object store: pack loose objects (delta-compressed) into a few dense pack files. Safe to run anytime; worth running after a large import. Pass all=true for a full repack that also rewrites existing packs, dropping unreachable garbage and consolidating.",
            object(json!({ "all": boolean }), json!([]))),
        tool("store", "Report object-store health: loose vs packed object counts, pack files and how delta-dense they are, on-disk sizes, and whether an incremental compaction or a repack is due. Read-only (the counterpart of compact).",
            object(json!({}), json!([]))),
        tool("scope", "Report the sparse-workspace scope: this bay's materialization scope (the subtrees it works on) and the warehouse fetch scope. Read-only.",
            object(json!({}), json!([]))),
        tool("expand", "Widen a sparse warehouse's fetch scope and download the newly in-scope subtree(s) across history from the remote. A full warehouse already holds everything.",
            object(json!({ "paths": { "type": "array", "items": string } }), json!(["paths"]))),
        tool("narrow", "Shrink this checkout's materialization scope: drop subtree path(s) and de-materialize their files. Frees nothing in the object store — the content stays reachable history.",
            object(json!({ "paths": { "type": "array", "items": string } }), json!(["paths"]))),
        tool("audit", "Verify the warehouse's signed history offline (a pallet, default: the current one).",
            object(json!({ "pallet": string }), json!([]))),
        tool("shift", "Switch the working directory to another pallet (checkout).",
            object(json!({ "pallet": string }), json!(["pallet"]))),
        tool("consolidate", "Merge another pallet into the current one.",
            object(json!({ "pallet": string }), json!(["pallet"]))),
        tool("palletize", "Create a pallet (branch) at an optional revision, or list pallets when name is omitted (all=true also lists the meta pallets).",
            object(json!({ "name": string, "revision": string, "all": boolean }), json!([]))),
        tool("restore", "Restore a path from the inventory (or, with staged=true, unstage it).",
            object(json!({ "path": string, "staged": boolean }), json!(["path"]))),
        tool("undo", "Undo the last stack on the current pallet: move the head to the parent, keeping the changes staged (like git reset --soft HEAD~1). Refuses merge parcels.",
            object(json!({}), json!([]))),
        tool("heal", "Resolve a standing durability taint (durability_taint refusals, exit 21) that automatic recovery could not clear on its own: restage what it can, then walk every durable ref source to tell a vanished-but-unreferenced object (dropped) from one still referenced (reported, with remedies). A torn taint cannot be resolved this way and needs heavyweight recovery instead.",
            object(json!({}), json!([]))),
        tool("lift", "Push the current pallet's new parcels to the configured remote.",
            object(json!({}), json!([]))),
        tool("lower", "Pull and fast-forward the current pallet from the configured remote.",
            object(json!({}), json!([]))),
        tool("office_list", "List the enrolled operators and their keys.",
            object(json!({}), json!([]))),
        tool("bay_add", "Open a bay: a new working directory bound to this warehouse, checked out to a new pallet named after it (branched from the current head). Pass scope to open a scoped (sparse) bay materializing only those subtree(s) — the tool for an orchestrator to hand a sub-agent a task-scoped sandbox. path defaults to a sibling of the warehouse.",
            object(json!({ "name": string, "path": string, "scope": { "type": "array", "items": string } }), json!(["name"]))),
        tool("bay_list", "List the bays: their names, working directories and current pallets.",
            object(json!({}), json!([]))),
        tool("bay_remove", "De-register a bay: remove its local bookkeeping (the redirect and bay state forklift created). The bay's pallet and materialized files are kept.",
            object(json!({ "name": string }), json!(["name"]))),
        tool("peek", "Inspect an object by hash (object), or a folder's inventory (inventory).",
            object(json!({ "object": string, "inventory": string }), json!([]))),
        tool("blame", "Attribute each line of a file to the parcel that last changed it (rev: at a revision, default the current head).",
            object(json!({ "path": string, "rev": string }), json!(["path"]))),
        tool("show", "Print a file's content at a revision in one call. target is \"<revision>:<path>\" (a pallet name, an @-qualified meta pallet, or a parcel hash, then a colon, then the path). Reports binary content or a chunked large file's metadata (content hash, size, chunk count) instead of raw bytes.",
            object(json!({ "target": string }), json!(["target"]))),
        tool("cherry_pick", "Apply a single parcel's change onto the current pallet as a new parcel.",
            object(json!({ "revision": string, "message": string }), json!(["revision"]))),
        tool("deliver", "Squash the current draft pallet onto a target pallet as one clean signed parcel, keeping the trail. Needs an enrolled key.",
            object(json!({ "target": string, "message": string }), json!(["target"]))),
        tool("park", "Park the work in progress (tracked staged + unstaged changes) and reset to the pallet head.",
            object(json!({}), json!([]))),
        tool("park_list", "List the parked parcels, newest first.",
            object(json!({}), json!([]))),
        tool("park_pop", "Re-apply the most recently parked changes (staged) and drop them from the list.",
            object(json!({}), json!([]))),
        tool("tag_create", "Create a signed tag at a revision (admin only); tag names are immutable.",
            object(json!({ "name": string, "revision": string, "message": string }), json!(["name"]))),
        tool("tag_show", "Show one tag in full (subject, tagger, message).",
            object(json!({ "name": string }), json!(["name"]))),
        tool("tag_list", "List every tag.",
            object(json!({}), json!([]))),
        tool("manifest_note", "Record a signed free-form note about a parcel. Needs an enrolled key.",
            object(json!({ "revision": string, "message": string }), json!(["revision", "message"]))),
        tool("manifest_approve", "Record a signed approval (sign-off) of a parcel. Needs an enrolled key.",
            object(json!({ "revision": string, "message": string }), json!(["revision"]))),
        tool("manifest_provenance", "Record signed machine-authorship provenance for a parcel: which model produced it (the AI-traceability record). The tool and session are set by the MCP server from this connection — you cannot self-report them; model is your attestation. Needs an enrolled key.",
            object(json!({ "revision": string, "model": string, "transcript": string, "message": string }), json!(["revision", "model"]))),
        tool("manifest_show", "Show the manifest entries (approvals, notes, provenance) attached to a parcel.",
            object(json!({ "revision": string }), json!(["revision"]))),
        tool("haul_open", "Open a merge proposal (pull request) from a source pallet into a target.",
            object(json!({ "target": string, "source": string, "title": string, "message": string }), json!(["target", "title"]))),
        tool("haul_list", "List merge proposals (state: open (default) | merged | closed | all).",
            object(json!({ "state": string }), json!([]))),
        tool("haul_show", "Show one merge proposal in full.",
            object(json!({ "id": string }), json!(["id"]))),
        tool("haul_comment", "Add a comment to a merge proposal.",
            object(json!({ "id": string, "message": string }), json!(["id", "message"]))),
        tool("haul_review", "Record a signed review of a haul (approves by default; request_changes or comment for the other verdicts). Needs an enrolled key.",
            object(json!({ "id": string, "request_changes": boolean, "comment": boolean, "message": string }), json!(["id"]))),
        tool("haul_merge", "Merge a proposal (consolidate its source into the target).",
            object(json!({ "id": string }), json!(["id"]))),
        tool("haul_close", "Close a merge proposal without merging.",
            object(json!({ "id": string }), json!(["id"]))),
        tool("haul_reopen", "Reopen a closed merge proposal.",
            object(json!({ "id": string }), json!(["id"]))),
    ])
}

/// One tool definition.
fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": input_schema })
}

/// A JSON-RPC error (a subset of the codes; enough for this server's surface).
struct JsonRpcError {
    code: i64,
    message: String,
}

impl JsonRpcError {
    fn method_not_found(method: &str) -> JsonRpcError {
        JsonRpcError { code: -32601, message: format!("Unknown method \"{}\".", method) }
    }

    fn invalid_params(reason: &str) -> JsonRpcError {
        JsonRpcError { code: -32602, message: format!("Invalid params: {}.", reason) }
    }

    fn internal(reason: &str) -> JsonRpcError {
        JsonRpcError { code: -32603, message: reason.to_string() }
    }

    fn to_value(&self) -> Value {
        json!({ "code": self.code, "message": self.message })
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::{ProgressMapper, Reported};
    use serde_json::json;

    /// A finished run's status, as `wait` would report it.
    #[cfg(unix)]
    fn exited(code: i32) -> std::process::ExitStatus {
        std::os::unix::process::ExitStatusExt::from_raw(code << 8)
    }

    #[cfg(unix)]
    #[test]
    fn a_tool_error_is_read_from_ok_not_from_the_exit_status() {
        let success = br#"{"lonic_json":1,"tool":"forklift","schema":2,"command":"x","ok":true,"data":{}}"#;
        let failure = br#"{"lonic_json":1,"tool":"forklift","schema":2,"command":"x","ok":false,"error":{}}"#;

        // A negative outcome reported as data: non-zero exit, but `data` is the result.
        assert_eq!(super::tool_result(success, b"", exited(1))["isError"], false);
        assert_eq!(super::tool_result(failure, b"", exited(0))["isError"], true);
        assert_eq!(super::tool_result(success, b"", exited(0))["isError"], false);
        assert_eq!(super::tool_result(failure, b"", exited(1))["isError"], true);

        // The document itself is the first item, unchanged.
        let result = super::tool_result(success, b"", exited(0));
        assert_eq!(result["content"][0]["text"], std::str::from_utf8(success).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn a_run_with_no_document_is_an_error_naming_how_it_ended() {
        let killed: std::process::ExitStatus = std::os::unix::process::ExitStatusExt::from_raw(9);
        let result = super::tool_result(b"", b"", killed);
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["text"], "forklift left no result document: it was killed by signal 9.");

        // Even a zero exit: the contract promises a document, and there is none to show.
        let result = super::tool_result(b"", b"", exited(0));
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["text"], "forklift left no result document: it exited with status 0.");

        // What the run did write is quoted, warnings aside, so a panic is not lost.
        let warning = r#"{"lonic_json":1,"tool":"forklift","warning":"w","message":"m","next_step":"n"}"#;
        let stderr = format!("thread 'main' panicked at src/x.rs:1:1:\nboom\n{}\n", warning);
        let result = super::tool_result(b"partial", stderr.as_bytes(), exited(101));
        assert_eq!(
            result["content"][0]["text"],
            "forklift left no result document: it exited with status 101.\n\nstdout:\npartial\n\nstderr:\nthread 'main' panicked at src/x.rs:1:1:\nboom",
        );
        assert_eq!(result["content"][1]["text"], warning);

        // Only the end of a long stream, cut on a character boundary.
        let long = format!("{}é{}", "a".repeat(10_000), "b".repeat(super::TAIL_BYTES - 1));
        let quoted = super::tail(&long);
        assert_eq!(quoted, &long[long.len() - super::TAIL_BYTES + 1..]);

        // JSON that is not a document — no boolean `ok` — is not one either.
        let result = super::tool_result(br#"{"ok":"yes"}"#, b"", exited(3));
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["text"], "forklift left no result document: it exited with status 3.\n\nstdout:\n{\"ok\":\"yes\"}");
    }

    #[cfg(unix)]
    #[test]
    fn only_warning_documents_are_forwarded_from_stderr() {
        let success = br#"{"lonic_json":1,"tool":"forklift","schema":2,"command":"x","ok":true,"data":{}}"#;
        let warning = r#"{"lonic_json":1,"tool":"forklift","warning":"maintenance_unavailable","message":"m","next_step":"n"}"#;
        let stderr = format!(
            "Packing objects...\n{}\n{}\n{}\nnot json\n{}\n",
            warning,
            r#"{"lonic_json":1,"tool":"forklift","message":"no warning code"}"#,
            r#"{"warning":"x","message":"no lonic_json"}"#,
            warning,
        );

        let result = super::tool_result(success, stderr.as_bytes(), exited(0));
        let texts: Vec<&str> = result["content"].as_array().unwrap().iter()
            .map(|item| item["text"].as_str().unwrap())
            .collect();
        assert_eq!(texts, vec![std::str::from_utf8(success).unwrap(), warning, warning]);

        // Warnings ride along with a run that left no document, too.
        let result = super::tool_result(b"", warning.as_bytes(), exited(1));
        assert_eq!(result["content"].as_array().unwrap().len(), 2);
        assert_eq!(result["content"][1]["text"], warning);
    }

    #[test]
    fn channel_lines_parse_to_events_and_malformed_ones_do_not() {
        assert_eq!(super::parse_event("status Packing objects"), Some(Reported::Status("Packing objects".into())));
        assert_eq!(super::parse_event("progress 40"), Some(Reported::Progress { percent: Some(40), text: "".into() }));
        assert_eq!(super::parse_event("progress 100 a b"), Some(Reported::Progress { percent: Some(100), text: "a b".into() }));
        assert_eq!(super::parse_event("progress ? item"), Some(Reported::Progress { percent: None, text: "item".into() }));

        for line in ["status ", "status", "progress 101", "progress -1", "progress x y", "progress", "step 1 2 x", ""] {
            assert_eq!(super::parse_event(line), None, "{:?} parsed", line);
        }
    }

    #[test]
    fn the_message_field_starts_at_protocol_2025_06_18() {
        assert!(!super::carries_message("2024-11-05"));
        assert!(!super::carries_message("2025-03-26"));
        assert!(super::carries_message("2025-06-18"));
        assert!(super::carries_message("2025-11-25"));

        // Not a date, so not ordered: no `message`, whichever way the string happens to sort.
        for version in ["draft", "DRAFT-2026", "2025-6-18x", "9999-99-9x", "2025/06/18", ""] {
            assert!(!super::carries_message(version), "{:?}", version);
        }
    }

    #[test]
    fn text_only_steps_stop_short_of_the_next_whole_value() {
        let start = std::time::Instant::now();
        let mut mapper = ProgressMapper::new(true);

        mapper.receive(progress(10, "0"));
        let mut last = mapper.take(start).unwrap().0;
        assert_eq!(last, 10.0);

        // A long run of text alone: every one is sent until the steps would reach 11, and none
        // after that.
        let mut sent = 0;
        for index in 1..=5000u32 {
            mapper.receive(Reported::Progress { percent: None, text: index.to_string() });
            if let Some((value, _)) = mapper.take(start + super::NOTIFICATION_INTERVAL * index) {
                assert!(value > last && value < 11.0, "{} after {}", value, last);
                last = value;
                sent += 1;
            }
        }
        assert_eq!(sent, super::TEXT_ONLY_LIMIT);

        // The next real percentage is sent as itself, not as one more step.
        mapper.receive(progress(12, "x"));
        assert_eq!(mapper.finish(), Some((12.0, Some("12% x".into()))));

        // And the steps start again from there.
        mapper.receive(Reported::Progress { percent: None, text: "y".into() });
        assert_eq!(mapper.finish(), Some((12.0 + super::TEXT_ONLY_STEP, Some("y".into()))));
    }

    fn progress(percent: u8, text: &str) -> Reported {
        Reported::Progress { percent: Some(percent), text: text.to_string() }
    }

    fn status(phase: &str) -> Reported {
        Reported::Status(phase.to_string())
    }

    /// Feed each event and take after each one, as if every event arrived a full interval apart.
    fn sent(mapper: &mut ProgressMapper, events: Vec<Reported>) -> Vec<(f64, Option<String>)> {
        let start = std::time::Instant::now();
        events.into_iter().enumerate().filter_map(|(index, event)| {
            mapper.receive(event);
            mapper.take(start + super::NOTIFICATION_INTERVAL * index as u32)
        }).collect()
    }

    #[test]
    fn a_lower_percentage_counts_as_a_reset_and_advances_the_value() {
        let mut mapper = ProgressMapper::new(false);
        let values: Vec<f64> = sent(&mut mapper, vec![
            progress(10, ""), progress(90, ""), progress(5, ""), progress(60, ""), progress(0, ""),
        ]).into_iter().map(|(value, _)| value).collect();

        assert_eq!(values, vec![10.0, 90.0, 105.0, 160.0, 200.0]);
    }

    #[test]
    fn the_value_strictly_increases_and_a_repeat_with_nothing_new_is_dropped() {
        let mut mapper = ProgressMapper::new(true);
        let notifications = sent(&mut mapper, vec![
            progress(40, "a"), progress(40, "a"), progress(40, "b"), progress(41, "b"),
        ]);

        assert_eq!(notifications, vec![
            (40.0, Some("40% a".into())),
            (40.0 + super::TEXT_ONLY_STEP, Some("40% b".into())),
            (41.0, Some("41% b".into())),
        ]);
    }

    #[test]
    fn status_and_indeterminate_events_go_in_the_message_only_when_the_protocol_has_one() {
        let events = || vec![status("Collecting objects"), progress(0, ""), Reported::Progress { percent: None, text: "x".into() }, status("Packing objects"), progress(30, "y")];

        let mut with = ProgressMapper::new(true);
        let step = super::TEXT_ONLY_STEP;
        assert_eq!(sent(&mut with, events()), vec![
            (0.0, Some("Collecting objects".into())),
            (step, Some("Collecting objects: 0%".into())),
            (2.0 * step, Some("Collecting objects: x".into())),
            (3.0 * step, Some("Packing objects".into())),
            (30.0, Some("Packing objects: 30% y".into())),
        ]);

        // Without `message`, only a raised value is worth sending.
        let mut without = ProgressMapper::new(false);
        assert_eq!(sent(&mut without, events()), vec![(0.0, None), (30.0, None)]);
    }

    #[test]
    fn events_folded_between_sends_keep_the_highest_value_and_the_newest_text() {
        let mut mapper = ProgressMapper::new(true);
        mapper.receive(progress(70, "a"));
        mapper.receive(progress(10, "b"));
        mapper.receive(status("Next"));
        assert_eq!(mapper.take(std::time::Instant::now()), Some((110.0, Some("Next".into()))));

        // Nothing received since: nothing to send.
        assert_eq!(mapper.finish(), None);
    }

    #[test]
    fn at_most_one_notification_goes_out_per_interval_until_the_run_ends() {
        let interval = super::NOTIFICATION_INTERVAL;
        let start = std::time::Instant::now();
        let mut mapper = ProgressMapper::new(false);

        mapper.receive(progress(10, ""));
        assert_eq!(mapper.take(start), Some((10.0, None)));

        // Too soon: held, not lost.
        mapper.receive(progress(20, ""));
        assert_eq!(mapper.take(start + interval / 2), None);
        mapper.receive(progress(30, ""));
        assert_eq!(mapper.take(start + interval - std::time::Duration::from_millis(1)), None);
        assert_eq!(mapper.take(start + interval), Some((30.0, None)));

        // The end of the run sends what is held, however soon.
        mapper.receive(progress(40, ""));
        assert_eq!(mapper.take(start + interval), None);
        assert_eq!(mapper.finish(), Some((40.0, None)));
    }

    #[test]
    fn a_notification_carries_the_token_and_never_a_total() {
        let notification = super::progress_notification(&json!(7), 12.5, Some("m".into()));
        assert_eq!(notification, json!({
            "jsonrpc": "2.0", "method": "notifications/progress",
            "params": { "progressToken": 7, "progress": 12.5, "message": "m" },
        }));

        let notification = super::progress_notification(&json!("t"), 1.0, None);
        assert_eq!(notification["params"], json!({ "progressToken": "t", "progress": 1.0 }));
    }

    /// CLI commands intentionally **not** exposed as MCP tools: warehouse/identity setup,
    /// host-machine concerns, and meta commands an agent does not drive through the tool
    /// surface. Every other CLI command MUST have a matching tool — the test below enforces
    /// it, so adding a CLI command without an MCP tool (or an allow-list entry) fails CI.
    ///
    /// `bay` is deliberately **not** on this list even though it manages a host working
    /// directory: §7.6's agent story is an orchestrator agent creating task-scoped sandboxes
    /// for sub-agents over MCP, and `bay` (`bay_add`/`bay_list`/`bay_remove`) is how it does
    /// that. The scope a bay records is advisory local setup, not the agent's own security
    /// boundary — enforcement of what an identity may touch lives remote-side (the server's
    /// role-and-transport authorization), not in the client's bay bookkeeping. Every bay
    /// operation is non-destructive: `add` refuses
    /// onto a non-empty directory, and `remove` only deletes forklift's own redirect file and
    /// bay-state folder, never the materialized working tree or anything the agent didn't
    /// create — so nothing here needs a tighter gate than the rest of the surface.
    const HUMAN_ONLY: &[&str] = &[
        "alias",       // manage the `fl` shell alias next to this binary (host concern)
        "prepare",     // create a warehouse (setup)
        "config",      // read/set configuration (setup)
        "profile",     // manage identity profiles (setup)
        "franchise",   // clone a remote (setup)
        "import-git",  // migrate a git repo in (setup)
        "export-git",  // migrate out to git (setup)
        "self-update", // update the binary (host concern)
        "scope-prune", // destructive, warehouse-wide disk reclamation — an admin verb, never a sub-agent's
        "mcp",         // this server itself
        "help",        // meta
        "version",     // meta
    ];

    #[test]
    fn every_cli_command_is_an_mcp_tool_or_explicitly_human_only() {
        let definitions = super::tool_definitions();
        let tools: Vec<&str> = definitions
            .as_array().unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();

        let cli = crate::cli::Cli::command();

        // Every CLI command is exposed as a tool (named for it, or a `<command>_<sub>` tool)
        // or is on the human-only allow-list — but never neither, and never both.
        for command in cli.get_subcommands() {
            let name = command.get_name();

            // Hidden dev-only diagnostics (e.g. `__docgen`, feature-gated and never part of a
            // release build) are not part of the CLI/MCP contract this test enforces.
            if name.starts_with("__") {
                continue;
            }

            let prefix = name.replace('-', "_");
            let covered = tools.iter().any(|tool| *tool == prefix || tool.starts_with(&format!("{}_", prefix)));
            let human_only = HUMAN_ONLY.contains(&name);

            assert!(
                covered || human_only,
                "CLI command `{name}` has no MCP tool and is not in HUMAN_ONLY. The MCP is the \
                 agent surface and must match the CLI: add a tool (build_args + tool_definitions) \
                 or, if it is genuinely human-only, add `{name}` to HUMAN_ONLY with a reason."
            );
            assert!(
                !(covered && human_only),
                "CLI command `{name}` is both an MCP tool and human-only — remove it from HUMAN_ONLY."
            );
        }

        // No stale allow-list entries: each names a real CLI command.
        let names: std::collections::HashSet<&str> =
            cli.get_subcommands().map(|command| command.get_name()).collect();
        for entry in HUMAN_ONLY {
            assert!(names.contains(entry), "HUMAN_ONLY lists `{entry}`, which is not a CLI command");
        }
    }
}
