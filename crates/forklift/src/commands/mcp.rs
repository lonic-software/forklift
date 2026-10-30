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
/// * `Ok(())`      - When stdin closes, or the client stops reading stdout.
/// * `Err(String)` - If the root is unusable, or stdin/stdout fails irrecoverably.
pub fn handle_command(root: Option<String>) -> Result<(), String> {
    if let Some(root) = root {
        std::env::set_current_dir(&root)
            .map_err(|e| format!("Error while changing into the warehouse \"{}\": {}", root, e))?;
    }

    // A client that goes away must not kill this server mid-call: the call's child would be
    // orphaned, and die on its own next write to the stdout nobody reads any more. With the
    // signal ignored, the write fails instead, and the server ends once the call has finished.
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

        match write_message(&mut stdout, &response) {
            Ok(()) => {}
            // The client has gone: the connection is over, as it is when stdin closes. Nothing
            // is left to report it to, and stderr may well have gone with it.
            Err(e) if client_gone(&e) => return Ok(()),
            Err(e) => return Err(format!("Error while writing stdout: {}", e)),
        }
    }

    Ok(())
}

/// Whether a failed write means the client has gone: a pipe reports it as a broken pipe, a
/// socket can report a reset or a lost connection instead.
fn client_gone(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(error.kind(), ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::NotConnected)
}

/// Write one JSON-RPC message as a line and flush it.
fn write_message(stdout: &mut std::io::Stdout, message: &Value) -> std::io::Result<()> {
    let text = serde_json::to_string(message).map_err(std::io::Error::other)?;

    writeln!(stdout, "{}", text)?;
    stdout.flush()
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
        "tools/list" => tools()
            .map(|tools| json!({ "tools": tools.iter().map(Tool::definition).collect::<Vec<_>>() }))
            .map_err(|reason| JsonRpcError::internal(&reason)),
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

    let tool = tools().map_err(|reason| JsonRpcError::internal(&reason))?
        .iter().find(|tool| tool.name == name)
        .ok_or_else(|| JsonRpcError::invalid_params(&format!("unknown tool \"{}\"", name)))?;

    let arguments = params.get("arguments").filter(|value| !value.is_null()).cloned().unwrap_or(json!({}));
    if let Some(reason) = tool.refusal(&arguments) {
        return Err(JsonRpcError::invalid_params(&reason));
    }

    // Provenance hardening (§7.2): an injected argument is set from this MCP connection,
    // overriding anything the agent supplied — the harness identifies the tool, not the
    // model's own output. `model` stays the agent's attestation (MCP carries no model id).
    let mut arguments = arguments.as_object().cloned().unwrap_or_default();
    for param in &tool.params {
        let Some(source) = param.injected else { continue };
        let value = match source {
            Injected::Client => state.client.clone(),
            Injected::Session => Some(state.session.clone()),
        };
        match value {
            Some(value) => arguments.insert(param.key.clone(), json!(value)),
            None => arguments.remove(&param.key),
        };
    }

    let args = tool.argv(&arguments).map_err(|reason| JsonRpcError::invalid_params(&reason))?;

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

/// The command for one tool call: this same binary, `--json`, and the given arguments. `--json`
/// goes first because the arguments may end in `--` and positionals, after which it would be a
/// positional too.
fn forklift_command(args: &[String]) -> Result<std::process::Command, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("Cannot locate the forklift binary: {}", e))?;

    let mut command = std::process::Command::new(exe);
    command.arg("--json").args(args);

    Ok(command)
}

/// How often at most a progress notification is sent for one call.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
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

    // The channel ends when the child does: it holds the only write end, and makes it
    // close-on-exec at startup whether or not it accepts it, so nothing it runs inherits it. That
    // matters, since the loop waits for the end with no timeout: a descendant that detaches from
    // its stdio (as `git gc --auto` does) would otherwise keep the channel, and this call, open.
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

// Off macOS and Linux no call reports progress, so the event and notification items that follow
// are unused there and carry `allow(dead_code)` for it.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn run_forklift_reporting(
    args: &[String],
    _progress: &mut ProgressMapper,
    _send: &mut dyn FnMut(f64, Option<String>),
) -> Result<std::process::Output, String> {
    run_forklift(args)
}

/// One event read from the channel.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
#[derive(Debug, PartialEq)]
enum Reported {
    /// `status <phase>`.
    Status(String),

    /// `progress <0-100|?> [text]`.
    Progress { percent: Option<u8>, text: String },
}

/// Parse one channel line. Anything that is not a well-formed `status` or `progress` event is
/// `None`, and is ignored.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
fn parse_event(line: &str) -> Option<Reported> {
    if let Some(phase) = line.strip_prefix("status ") {
        let phase = phase.trim();
        return (!phase.is_empty()).then(|| Reported::Status(phase.to_string()));
    }

    let rest = line.strip_prefix("progress ")?;
    let (value, text) = rest.split_once(' ').unwrap_or((rest, ""));

    // The v1 rule Pult's reference states: ASCII digits after at most one `+`, and a value over 100
    // saturates at any magnitude. With the digits checked, a failed parse can only be overflow.
    let percent = match value {
        "?" => None,
        value => {
            let digits = value.strip_prefix('+').unwrap_or(value);
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            Some(digits.parse::<u8>().map_or(100, |percent| percent.min(100)))
        }
    };

    Some(Reported::Progress { percent, text: text.trim().to_string() })
}

/// Whether a protocol version carries `message` on a progress notification: 2025-06-18 and
/// later. Versions are dates, so they order as strings; anything else carries none.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
fn carries_message(protocol_version: &str) -> bool {
    // Only a well-formed date orders correctly: "draft" sorts after every date, "2025-6-18"
    // before its own month.
    let is_date = protocol_version.len() == 10 && protocol_version.bytes().enumerate()
        .all(|(index, byte)| if index == 4 || index == 7 { byte == b'-' } else { byte.is_ascii_digit() });

    is_date && protocol_version >= "2025-06-18"
}

/// What a notification that only carries new text adds to the last `progress` sent. A power of
/// two, so the sums stay exact.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
const TEXT_ONLY_STEP: f64 = 1.0 / 4096.0;

/// How many text-only notifications fit between two whole values: the steps never reach the
/// next one, so `progress` keeps meaning 100 × resets + percentage.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
const TEXT_ONLY_LIMIT: u32 = 4095;

/// How many of those text that does not change the phase may use. The rest are kept for phase
/// changes, so a run whose text has used up its share still reports a new phase.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
const TEXT_ONLY_TEXT_LIMIT: u32 = TEXT_ONLY_LIMIT / 2;

/// Turns one call's events into progress notifications.
///
/// * No `total` is ever sent: an operation cannot promise it will not start a phase over, and a
///   fraction sent before that would report the whole operation done.
/// * `progress` is 100 × the resets seen + the event's percentage, where a reset is a percentage
///   lower than the previous one received. Each phase starts low again, so a phase advances the
///   value instead of rewinding it.
/// * `progress` strictly increases. An event that would not raise it is still sent if it brings
///   new text, at the last value plus [`TEXT_ONLY_STEP`]; otherwise it is dropped. At most
///   [`TEXT_ONLY_LIMIT`] of those follow one whole value, so they never reach the next one, and
///   only [`TEXT_ONLY_TEXT_LIMIT`] of them may carry text that names no new phase; past either
///   limit, such text is dropped until the value rises.
/// * The phase, the percentage and the text go in `message`, only when the protocol has it.
///   Without it, an event that does not raise the value has nothing to say and is dropped.
/// * At most one notification goes out per [`NOTIFICATION_INTERVAL`], until the run ends.
///
/// Events are folded in as they arrive, and [`ProgressMapper::take`] decides what to send.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
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

    /// Whether a `status` naming a new phase arrived since the last send.
    pending_phase: bool,

    /// Text-only notifications sent since the last whole value.
    text_only: u32,

    /// The last `progress` and `message` sent, and when.
    sent_value: Option<f64>,
    sent_message: Option<String>,
    sent_at: Option<std::time::Instant>,
}

#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
impl ProgressMapper {
    fn new(message: bool) -> ProgressMapper {
        ProgressMapper {
            message,
            resets: 0,
            last_percent: None,
            phase: None,
            pending_value: None,
            pending_message: None,
            pending_phase: false,
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
                // Only a different phase may use the phases' share: a repeated status is text.
                if self.phase.as_deref() != Some(phase.as_str()) {
                    self.pending_phase = true;
                }
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
        let limit = if std::mem::take(&mut self.pending_phase) { TEXT_ONLY_LIMIT } else { TEXT_ONLY_TEXT_LIMIT };

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
                    Some(_) if self.text_only >= limit => return None,
                    Some(sent) => {
                        self.text_only += 1;
                        sent + TEXT_ONLY_STEP
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
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
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
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
fn progress_notification(token: &Value, progress: f64, message: Option<String>) -> Value {
    let mut params = json!({ "progressToken": token, "progress": progress });
    if let Some(message) = message {
        params["message"] = json!(message);
    }

    json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": params })
}

/// The command paths no agent drives, each with why. A path keeps itself and every command
/// below it off the agent surface; every other visible command is a tool, so a new command
/// is an agent tool unless it is listed here.
///
/// `bay` is deliberately absent even though it manages a host working directory: an
/// orchestrator agent hands a sub-agent a task-scoped sandbox by adding a bay. The scope a bay
/// records is advisory local setup, not the agent's security boundary (what an identity may
/// touch is enforced remote-side), and every bay operation is non-destructive: `add` refuses a
/// non-empty directory, and `remove` deletes only the bay's own `.forklift` entry and its
/// bay-state folder, never the materialized working tree.
pub(crate) const HUMAN_ONLY: &[(&str, &str)] = &[
    ("alias", "manages the `fl` shell alias next to this binary, a host concern"),
    ("prepare", "creates a warehouse, which is setup"),
    ("config", "reads and sets configuration, which is setup"),
    ("profile", "manages identity profiles, which is setup"),
    ("franchise", "clones a remote, which is setup"),
    ("import-git", "migrates a git repository in, which is setup"),
    ("export-git", "migrates out to git, which is setup"),
    ("mirror", "pushes to an external git remote under the user's own git credentials"),
    ("office enroll", "establishes the operator's trust, a one-way setup step"),
    ("office keygen", "creates a signing key, which is setup"),
    ("office admit", "enrolls another operator: trust administration"),
    ("office link", "links another device's key: trust administration"),
    ("office authorize", "authorizes a key for another operator: trust administration"),
    ("office role", "changes a user's role and grants: trust administration"),
    ("office rotate", "replaces the operator's keys: trust administration"),
    ("office retire", "revokes a key: trust administration"),
    ("office regenesis", "resets trust, a recovery step"),
    ("office accept-regenesis", "accepts a remote's trust reset, a recovery step"),
    ("self-update", "updates the binary, a host concern"),
    ("self-schema", "describes the CLI for Pult; agents get the tool list instead"),
    ("scope-prune", "destructive, warehouse-wide disk reclamation: an admin verb, never a sub-agent's"),
    ("evict", "warehouse-wide disk reclamation in a cache: an admin verb, like scope-prune"),
    ("remote", "repoints a partial warehouse at another server, which is setup"),
    ("mcp", "this server itself"),
    ("help", "meta"),
    ("version", "meta"),
];

/// The argument combinations no agent passes, by command path and properties, each with why: a
/// call giving every one of the properties is refused. Each argument stays in the tool, since
/// alone or with others it does something an agent may do. A remote pallet's restore is served:
/// like lifting with `recreate`, it brings history back rather than removing any, and the
/// refusal a lift to a pallet deleted on the remote gives names it as the next step.
pub(crate) const HUMAN_ONLY_TOGETHER: &[(&str, &[&str], &str)] = &[
    ("palletize", &["remote", "delete"], "deletes the remote's pallet, which every clone shares: an admin's call, never a sub-agent's"),
];

/// The families whose bare form runs the same operation as one of their subcommands, with that
/// subcommand. The subcommand is the tool; the bare form gets none, so an agent sees one tool
/// per operation.
pub(crate) const BARE_SAME_AS: &[(&str, &str)] = &[
    ("haul", "list"),
    ("office", "list"),
    ("scope", "status"),
    ("tag", "list"),
];

/// The arguments the server sets itself, from the connection: absent from the tool's schema
/// (an agent's value is discarded), present in the command's arguments. A machine-authorship
/// record's harness and session identify the tool that drove the model, which the model's own
/// output cannot be trusted to report.
const INJECTED: &[(&str, &str, Injected)] = &[
    ("manifest provenance", "tool", Injected::Client),
    ("manifest provenance", "session", Injected::Session),
];

/// Where the server takes an [`INJECTED`] argument's value from.
#[derive(Clone, Copy)]
pub(crate) enum Injected {
    /// The client application `initialize` named; the argument is left out when it named none.
    Client,

    /// This connection's session id.
    Session,
}

/// The arguments whose JSON type is not the one their parser implies. Clap reads every value
/// as a word; these take a richer JSON value that is written back into that word.
const TYPED: &[(&str, &str, Typed)] = &[
    ("query", "where", Typed::Object(
        "A JSON predicate tree AND-ed with the other arguments. A node is {\"all\": [nodes]}, \
         {\"any\": [nodes]}, {\"not\": node} or a leaf {\"field\", \"op\", \"value\"}; an \
         unknown field is refused with the list of fields.",
    )),
    ("history", "limit", Typed::Positive),
    ("query", "limit", Typed::Positive),
];

/// A JSON type [`TYPED`] gives an argument.
#[derive(Clone, Copy)]
enum Typed {
    /// A JSON object, passed as its serialization, with the help an agent reads in place of the
    /// command's (which describes the word, not the object).
    Object(&'static str),

    /// An integer of at least 1.
    Positive,
}

/// One tool: the command it runs and the arguments it takes.
pub(crate) struct Tool {
    /// The command path joined by `_` (hyphens too), so a family run bare is its own name.
    pub(crate) name: String,

    /// The command path below the binary (`["tag", "create"]`).
    pub(crate) path: Vec<String>,

    /// The command's long description, else its one-line one.
    description: String,

    /// The arguments in declaration order.
    pub(crate) params: Vec<ToolParam>,

    /// The [`HUMAN_ONLY_TOGETHER`] combinations of this tool's arguments, with why.
    pub(crate) together: Vec<(&'static [&'static str], &'static str)>,
}

/// One tool argument.
pub(crate) struct ToolParam {
    /// The property name: an option's long name with `_` for `-`, a positional's id.
    pub(crate) key: String,

    /// Where its words go in the command.
    form: Form,

    /// The JSON type of one value.
    value: ValueKind,

    /// Whether the command refuses to run without it.
    pub(crate) required: bool,

    /// Set by the server rather than the agent, from this source.
    pub(crate) injected: Option<Injected>,

    /// The argument's help text.
    help: Option<String>,
}

/// Where an argument's words go in the command.
enum Form {
    /// `--long`, when the value is true.
    Switch(String),

    /// `--long=value`, once per value when it repeats. The attached form keeps a value that
    /// begins with `-` a value.
    Option { long: String, repeats: bool },

    /// After `--`, in declaration order, so no value is read as an option.
    Positional { variadic: bool },
}

/// The JSON type of one value (an array of them when the argument repeats).
enum ValueKind {
    Boolean,
    String,
    Choice(Vec<String>),
    Integer(u64),
    Object,
}

/// Every tool, generated once from the command tree.
///
/// # Returns
/// * `Ok(&[Tool])`  - The tools, in the tree's declaration order.
/// * `Err(String)`  - The tree has a command or argument with no tool shape (see [`generate_tools`]).
pub(crate) fn tools() -> Result<&'static [Tool], String> {
    use clap::CommandFactory;

    static TOOLS: std::sync::OnceLock<Result<Vec<Tool>, String>> = std::sync::OnceLock::new();
    TOOLS.get_or_init(|| generate_tools(&crate::cli::Cli::command())).as_deref().map_err(Clone::clone)
}

/// Every tool, generated from the command tree: each visible command path outside
/// [`HUMAN_ONLY`], a family run bare included when it runs without a subcommand and is not in
/// [`BARE_SAME_AS`].
///
/// # Arguments
/// * `root` - The unbuilt command tree.
///
/// # Returns
/// * `Ok(Vec<Tool>)` - The tools, in the tree's declaration order.
/// * `Err(String)`   - A command or argument has no tool shape, two commands make one tool
///   name, or a table entry names nothing.
fn generate_tools(root: &clap::Command) -> Result<Vec<Tool>, String> {
    struct Used {
        human_only: Vec<bool>,
        bare_same_as: Vec<bool>,
    }

    fn walk(command: &clap::Command, path: Vec<String>, tools: &mut Vec<Tool>, used: &mut Used) -> Result<(), String> {
        let shown = path.join(" ");
        if let Some(index) = HUMAN_ONLY.iter().position(|(entry, _)| *entry == shown) {
            used.human_only[index] = true;
            return Ok(());
        }
        if command.has_subcommands() {
            if let Some(arg) = command.get_arguments().find(|arg| !arg.is_global_set()) {
                return Err(format!("`forklift {shown}` has subcommands and its own argument `{}`", arg.get_id()));
            }
            if !command.is_subcommand_required_set() {
                match BARE_SAME_AS.iter().position(|(family, _)| *family == shown) {
                    Some(index) => used.bare_same_as[index] = true,
                    None => tools.push(tool_for(command, &path, Vec::new())?),
                }
            }
            for sub in command.get_subcommands().filter(|sub| !sub.is_hide_set()) {
                let mut sub_path = path.clone();
                sub_path.push(sub.get_name().to_string());
                walk(sub, sub_path, tools, used)?;
            }
            return Ok(());
        }

        let mut params = Vec::new();
        for arg in command.get_arguments().filter(|arg| !arg.is_global_set() && !arg.is_hide_set()) {
            params.push(tool_param(&shown, arg)?);
        }
        let mut keys = std::collections::HashSet::new();
        if let Some(param) = params.iter().find(|param| !keys.insert(param.key.as_str())) {
            return Err(format!("two arguments of `forklift {shown}` make the property `{}`", param.key));
        }
        tools.push(tool_for(command, &path, params)?);
        Ok(())
    }

    let mut tools = Vec::new();
    let mut used = Used {
        human_only: vec![false; HUMAN_ONLY.len()],
        bare_same_as: vec![false; BARE_SAME_AS.len()],
    };
    for command in root.get_subcommands().filter(|command| !command.is_hide_set()) {
        walk(command, vec![command.get_name().to_string()], &mut tools, &mut used)?;
    }

    if let Some(((entry, _), _)) = HUMAN_ONLY.iter().zip(&used.human_only).find(|(_, used)| !**used) {
        return Err(format!("HUMAN_ONLY names `{entry}`, which is not a visible command"));
    }
    attach_together(&mut tools, HUMAN_ONLY_TOGETHER)?;
    for ((family, sub), used) in BARE_SAME_AS.iter().zip(&used.bare_same_as) {
        let path = format!("{family} {sub}");
        if !*used || !tools.iter().any(|tool| tool.path.join(" ") == path) {
            return Err(format!("BARE_SAME_AS names `{family}` as `{path}`, which is not a family run bare and its tool"));
        }
    }
    let tabled = INJECTED.iter().map(|(path, key, _)| (*path, *key)).chain(TYPED.iter().map(|(path, key, _)| (*path, *key)));
    for (path, key) in tabled {
        let found = tools.iter().any(|tool| tool.path.join(" ") == path && tool.params.iter().any(|param| param.key == key));
        if !found {
            return Err(format!("a tool table names `{key}` of `{path}`, which is not an argument of a tool"));
        }
    }
    let mut names = std::collections::HashSet::new();
    if let Some(tool) = tools.iter().find(|tool| !names.insert(tool.name.as_str())) {
        return Err(format!("two commands make the tool name `{}`", tool.name));
    }
    Ok(tools)
}

/// Attaches each combination of `table` (shaped as [`HUMAN_ONLY_TOGETHER`]) to its tool.
///
/// # Returns
/// * `Ok(())`      - Every entry names a tool and two or more distinct arguments of it.
/// * `Err(String)` - An entry names no tool, an argument its tool lacks, or fewer than two
///   distinct arguments (which would refuse an argument alone).
fn attach_together(tools: &mut [Tool], table: &'static [(&'static str, &'static [&'static str], &'static str)]) -> Result<(), String> {
    for (path, keys, why) in table {
        let tool = tools.iter_mut().find(|tool| tool.path.join(" ") == *path)
            .ok_or_else(|| format!("HUMAN_ONLY_TOGETHER names `{path}`, which is not a tool"))?;
        if let Some(key) = keys.iter().find(|key| !tool.params.iter().any(|param| param.key == **key)) {
            return Err(format!("HUMAN_ONLY_TOGETHER names `{key}` of `{path}`, which is not an argument of its tool"));
        }
        if keys.iter().collect::<std::collections::HashSet<_>>().len() < 2 {
            return Err(format!("HUMAN_ONLY_TOGETHER names fewer than two distinct arguments of `{path}`"));
        }
        tool.together.push((keys, why));
    }
    Ok(())
}

/// The tool for the command at `path`, with its arguments. Its description is the command's
/// one-line description, then its long one when it has one (the one-liner carries the git
/// name, "(push)", that the long one does not repeat), then which arguments the server sets.
fn tool_for(command: &clap::Command, path: &[String], params: Vec<ToolParam>) -> Result<Tool, String> {
    let text = |styled: Option<&clap::builder::StyledStr>| styled.map(|text| text.to_string())
        .filter(|text| !text.trim().is_empty());
    let mut description = text(command.get_about())
        .ok_or_else(|| format!("`forklift {}` has no description", path.join(" ")))?;
    if let Some(long) = text(command.get_long_about()) {
        description.push_str("\n\n");
        description.push_str(&long);
    }

    let injected: Vec<String> = params.iter().filter(|param| param.injected.is_some())
        .map(|param| format!("\"{}\"", param.key))
        .collect();
    if !injected.is_empty() {
        description.push_str(&format!(
            "\n\nThe server sets {} itself, from this connection; a value passed for {} is discarded.",
            injected.join(" and "),
            if injected.len() == 1 { "it" } else { "them" },
        ));
    }

    Ok(Tool { name: path.join("_").replace('-', "_"), path: path.to_vec(), description, params, together: Vec::new() })
}

/// The tool argument for `arg` of the command `shown`.
fn tool_param(shown: &str, arg: &clap::Arg) -> Result<ToolParam, String> {
    let refuse = |why: &str| format!("argument `{}` of `forklift {shown}` has no tool shape: {why}", arg.get_id());
    let append = match arg.get_action() {
        clap::ArgAction::SetTrue | clap::ArgAction::Set => false,
        clap::ArgAction::Append => true,
        other => return Err(refuse(&format!("its action is {other:?}"))),
    };

    let (key, form) = if arg.is_positional() {
        (arg.get_id().to_string(), Form::Positional { variadic: append })
    } else {
        let long = arg.get_long().ok_or_else(|| refuse("it has no long name"))?.to_string();
        let key = long.replace('-', "_");
        if matches!(arg.get_action(), clap::ArgAction::SetTrue) {
            (key, Form::Switch(long))
        } else {
            (key, Form::Option { long, repeats: append })
        }
    };

    let typed = TYPED.iter().find(|(path, id, _)| *path == shown && *id == key).map(|(_, _, typed)| *typed);
    let parser = arg.get_value_parser().type_id();
    let other_numbers = [
        std::any::TypeId::of::<u8>(), std::any::TypeId::of::<u16>(), std::any::TypeId::of::<u128>(),
        std::any::TypeId::of::<i8>(), std::any::TypeId::of::<i16>(), std::any::TypeId::of::<i32>(),
        std::any::TypeId::of::<i64>(), std::any::TypeId::of::<i128>(), std::any::TypeId::of::<isize>(),
        std::any::TypeId::of::<f32>(), std::any::TypeId::of::<f64>(),
    ];
    if typed.is_none() && other_numbers.iter().any(|id| parser == *id) {
        return Err(refuse("it parses a number the tool schema has no type for; give it a TYPED entry or a usize, u64 or u32 parser"));
    }
    let choices: Vec<String> = arg.get_possible_values().iter()
        .filter(|value| !value.is_hide_set())
        .map(|value| value.get_name().to_string())
        .collect();
    let value = match (&form, typed) {
        (Form::Switch(_), _) => ValueKind::Boolean,
        (_, Some(Typed::Object(_))) => ValueKind::Object,
        (_, Some(Typed::Positive)) => ValueKind::Integer(1),
        _ if !choices.is_empty() => ValueKind::Choice(choices),
        _ if [std::any::TypeId::of::<usize>(), std::any::TypeId::of::<u64>(), std::any::TypeId::of::<u32>()]
            .iter().any(|id| parser == *id) => ValueKind::Integer(0),
        _ => ValueKind::String,
    };

    let injected = INJECTED.iter().find(|(path, id, _)| *path == shown && *id == key).map(|(_, _, source)| *source);
    Ok(ToolParam {
        key,
        form,
        value,
        required: arg.is_required_set(),
        injected,
        help: match typed {
            Some(Typed::Object(help)) => Some(help.to_string()),
            _ => arg.get_long_help().or_else(|| arg.get_help()).map(|text| text.to_string()),
        },
    })
}

impl Tool {
    /// The definition `tools/list` returns: name, description and a closed input schema with
    /// the arguments the agent passes (an injected one is not among them).
    fn definition(&self) -> Value {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for param in self.params.iter().filter(|param| param.injected.is_none()) {
            let one = match &param.value {
                ValueKind::Boolean => json!({ "type": "boolean" }),
                ValueKind::String => json!({ "type": "string" }),
                ValueKind::Choice(choices) => json!({ "type": "string", "enum": choices }),
                ValueKind::Integer(minimum) => json!({ "type": "integer", "minimum": minimum }),
                ValueKind::Object => json!({ "type": "object" }),
            };
            let mut schema = if param.repeats() { json!({ "type": "array", "items": one }) } else { one };
            if let Some(help) = &param.help {
                schema["description"] = json!(help);
            }
            properties.insert(param.key.clone(), schema);
            if param.required {
                required.push(param.key.clone());
            }
        }
        let mut description = self.description.clone();
        for (keys, why) in &self.together {
            let keys: Vec<String> = keys.iter().map(|key| format!("\"{key}\"")).collect();
            description.push_str(&format!("\n\nAn agent may not pass {} together: that {why}.", keys.join(" with ")));
        }
        json!({
            "name": self.name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            },
        })
    }

    /// Why this tool refuses these arguments, if it does: they are not a JSON object, an
    /// argument is one the tool does not declare, a declared one has the wrong type, or a
    /// required one is missing. A null value is an omitted argument (harnesses fill unset
    /// optionals with it). An injected argument is never refused: the server replaces it.
    ///
    /// Refusing rather than ignoring matters: a dropped filter would answer a broader question
    /// than the one asked, still with ok:true.
    fn refusal(&self, arguments: &Value) -> Option<String> {
        let name = &self.name;
        let Some(arguments) = arguments.as_object() else {
            return Some(format!("the \"{name}\" tool's arguments must be a JSON object"));
        };

        let given = arguments.iter().filter(|(_, value)| !value.is_null());
        for (key, value) in given {
            match self.params.iter().find(|param| param.key == *key) {
                None => return Some(format!("the \"{name}\" tool takes no \"{key}\" argument")),
                Some(param) if param.injected.is_some() => {}
                Some(param) if !param.accepts(value) => {
                    return Some(format!("the \"{name}\" tool's \"{key}\" argument must be {}", param.describe()));
                }
                Some(_) => {}
            }
        }
        let missing = self.params.iter()
            .find(|param| param.required && param.injected.is_none() && arguments.get(&param.key).is_none_or(Value::is_null));
        if let Some(param) = missing {
            return Some(format!("the \"{name}\" tool needs the \"{}\" argument", param.key));
        }

        // A switch passed as false, or an empty list, is not given: it puts nothing in the command.
        let given = |key: &&str| arguments.get(*key)
            .is_some_and(|value| !value.is_null() && *value != Value::Bool(false) && *value != json!([]));
        let refused = self.together.iter().find(|(keys, _)| keys.iter().all(given));
        refused.map(|(keys, why)| {
            let keys: Vec<String> = keys.iter().map(|key| format!("\"{key}\"")).collect();
            format!("the \"{name}\" tool refuses {} together: that {why}; a person runs it", keys.join(" with "))
        })
    }

    /// The command's arguments for these (accepted) arguments: the path, each option as
    /// `--long=value` and each true switch as `--long`, then `--` and the positionals in
    /// declaration order.
    ///
    /// # Returns
    /// * `Ok(Vec<String>)` - The arguments, without `--json`.
    /// * `Err(String)`     - A positional is given while one before it is not: the command
    ///   would read it as the earlier one.
    fn argv(&self, arguments: &serde_json::Map<String, Value>) -> Result<Vec<String>, String> {
        let mut words = self.path.clone();
        let mut positionals = Vec::new();
        let mut absent: Option<&str> = None;
        for param in &self.params {
            let items: Vec<&Value> = match arguments.get(&param.key).filter(|value| !value.is_null()) {
                None => Vec::new(),
                Some(Value::Array(items)) if param.repeats() => items.iter().collect(),
                Some(value) => vec![value],
            };
            match &param.form {
                Form::Switch(long) => {
                    if items.first().and_then(|value| value.as_bool()) == Some(true) {
                        words.push(format!("--{long}"));
                    }
                }
                Form::Option { long, .. } => {
                    for item in items {
                        words.push(format!("--{long}={}", word(item)));
                    }
                }
                Form::Positional { .. } if items.is_empty() => {
                    absent.get_or_insert(&param.key);
                }
                Form::Positional { .. } => {
                    if let Some(absent) = absent {
                        return Err(format!("the \"{}\" argument needs the \"{absent}\" argument before it", param.key));
                    }
                    positionals.extend(items.into_iter().map(word));
                }
            }
        }
        if !positionals.is_empty() {
            words.push("--".to_string());
            words.extend(positionals);
        }
        Ok(words)
    }
}

impl ToolParam {
    /// Whether the argument takes an array of values.
    fn repeats(&self) -> bool {
        matches!(self.form, Form::Option { repeats: true, .. } | Form::Positional { variadic: true })
    }

    /// Whether a (non-null) value has this argument's type.
    fn accepts(&self, value: &Value) -> bool {
        match value.as_array() {
            Some(items) if self.repeats() => items.iter().all(|item| self.value.accepts(item)),
            _ if self.repeats() => false,
            _ => self.value.accepts(value),
        }
    }

    /// The argument's type in words, for a refusal ("a string", "an array of strings").
    pub(crate) fn describe(&self) -> String {
        if self.repeats() {
            format!("an array of {}", self.value.describe(true))
        } else {
            self.value.describe(false)
        }
    }
}

impl ValueKind {
    fn accepts(&self, value: &Value) -> bool {
        match self {
            ValueKind::Boolean => value.is_boolean(),
            ValueKind::String => value.is_string(),
            ValueKind::Choice(choices) => value.as_str().is_some_and(|value| choices.iter().any(|choice| choice == value)),
            ValueKind::Integer(minimum) => value.as_u64().is_some_and(|number| number >= *minimum),
            ValueKind::Object => value.is_object(),
        }
    }

    /// The type in words: "a string", or "strings" when `plural`.
    fn describe(&self, plural: bool) -> String {
        let (one, many) = match self {
            ValueKind::Boolean => ("a boolean".to_string(), "booleans".to_string()),
            ValueKind::String => ("a string".to_string(), "strings".to_string()),
            ValueKind::Choice(choices) => {
                let among = choices.iter().map(|choice| format!("\"{choice}\"")).collect::<Vec<_>>().join(", ");
                (format!("one of {among}"), format!("strings among {among}"))
            }
            ValueKind::Integer(minimum) => (format!("an integer of at least {minimum}"), format!("integers of at least {minimum}")),
            ValueKind::Object => ("an object".to_string(), "objects".to_string()),
        };
        if plural { many } else { one }
    }
}

/// One value as a command word: a string as itself, anything else as its JSON.
fn word(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
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
        let success = br#"{"lonic_json":1,"tool":"forklift","schema":3,"command":"x","ok":true,"data":{}}"#;
        let failure = br#"{"lonic_json":1,"tool":"forklift","schema":3,"command":"x","ok":false,"error":{}}"#;

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
        let stderr = format!("thread 'forklift-main' panicked at src/x.rs:1:1:\nboom\n{}\n", warning);
        let result = super::tool_result(b"partial", stderr.as_bytes(), exited(101));
        assert_eq!(
            result["content"][0]["text"],
            "forklift left no result document: it exited with status 101.\n\nstdout:\npartial\n\nstderr:\nthread 'forklift-main' panicked at src/x.rs:1:1:\nboom",
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
        let success = br#"{"lonic_json":1,"tool":"forklift","schema":3,"command":"x","ok":true,"data":{}}"#;
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

        for line in ["status ", "status", "progress -1", "progress x y", "progress", "step 1 2 x", ""] {
            assert_eq!(super::parse_event(line), None, "{:?} parsed", line);
        }
    }

    #[test]
    fn a_percent_saturates_at_any_magnitude_and_a_non_digit_is_malformed() {
        let percent = |line: &str| match super::parse_event(line) {
            Some(Reported::Progress { percent, .. }) => percent,
            other => panic!("{:?} parsed as {:?}", line, other),
        };
        // Above u8, above u32::MAX, then above u64::MAX: no integer width saves a parse-then-clamp
        // reader from dropping these. Padding is not magnitude.
        for (line, want) in [
            ("progress 101", 100), ("progress 300", 100), ("progress +101", 100),
            ("progress 99999999999", 100), ("progress 99999999999999999999999999", 100),
            ("progress +50", 50), ("progress 0000000000050", 50), ("progress 0000000000101", 100),
        ] {
            assert_eq!(percent(line), Some(want), "{}", line);
        }
        // A sign, a decimal point, a letter or a non-ASCII digit is malformed. Once a parse fails it
        // reads as overflow, so a looser digit check would turn the last two into a full bar.
        for line in ["progress -1", "progress -0", "progress 1.5", "progress x", "progress +", "progress ++5",
                     "progress +-5", "progress \u{663}", "progress \u{ff15}"] {
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

        // A long run of text alone: every one is sent until it has used its share of the steps,
        // and none after that.
        let mut sent = 0;
        for index in 1..=5000u32 {
            mapper.receive(Reported::Progress { percent: None, text: index.to_string() });
            if let Some((value, _)) = mapper.take(start + super::NOTIFICATION_INTERVAL * index) {
                assert!(value > last && value < 11.0, "{} after {}", value, last);
                last = value;
                sent += 1;
            }
        }
        assert_eq!(sent, super::TEXT_ONLY_TEXT_LIMIT);

        // A new phase still gets through, until the steps would reach 11, and none after that.
        let mut phases = 0;
        for index in 5001..=10000u32 {
            mapper.receive(status(&format!("phase {}", index)));
            if let Some((value, _)) = mapper.take(start + super::NOTIFICATION_INTERVAL * index) {
                assert!(value > last && value < 11.0, "{} after {}", value, last);
                last = value;
                phases += 1;
            }
        }
        assert_eq!(sent + phases, super::TEXT_ONLY_LIMIT);

        // The next real percentage is sent as itself, not as one more step.
        mapper.receive(progress(12, "x"));
        assert_eq!(mapper.finish(), Some((12.0, Some("phase 10000: 12% x".into()))));

        // And the steps start again from there.
        mapper.receive(Reported::Progress { percent: None, text: "y".into() });
        assert_eq!(mapper.finish(), Some((12.0 + super::TEXT_ONLY_STEP, Some("phase 10000: y".into()))));

        // A phase seen once does not give later text the phases' share.
        let mut sent = 1;
        for index in 0..5000u32 {
            mapper.receive(Reported::Progress { percent: None, text: index.to_string() });
            sent += u32::from(mapper.finish().is_some());
        }
        assert_eq!(sent, super::TEXT_ONLY_TEXT_LIMIT);
    }

    #[test]
    fn a_reset_or_lost_connection_is_a_client_that_left_like_a_broken_pipe() {
        use std::io::{Error, ErrorKind};
        for kind in [ErrorKind::BrokenPipe, ErrorKind::ConnectionReset, ErrorKind::NotConnected] {
            assert!(super::client_gone(&Error::from(kind)), "{:?}", kind);
        }
        for kind in [ErrorKind::Other, ErrorKind::WriteZero, ErrorKind::Interrupted] {
            assert!(!super::client_gone(&Error::from(kind)), "{:?}", kind);
        }
    }

    #[test]
    fn a_repeated_status_gets_only_the_text_share_of_the_steps() {
        // A repeated status names no new phase, so it is text, and the phases' share is still
        // there for one that does.
        let mut mapper = ProgressMapper::new(true);
        mapper.receive(status("Packing objects"));
        mapper.receive(progress(10, "0"));
        assert_eq!(mapper.finish().map(|sent| sent.0), Some(10.0));
        let mut repeated = 0;
        for index in 0..5000u32 {
            mapper.receive(status("Packing objects"));
            mapper.receive(Reported::Progress { percent: None, text: index.to_string() });
            repeated += u32::from(mapper.finish().is_some());
        }
        assert_eq!(repeated, super::TEXT_ONLY_TEXT_LIMIT);
        mapper.receive(status("Removing packed originals"));
        assert!(mapper.finish().is_some(), "a new phase was dropped after repeated ones");
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

    /// The tool named `name`.
    fn tool(name: &str) -> &'static super::Tool {
        super::tools().unwrap().iter().find(|tool| tool.name == name).unwrap_or_else(|| panic!("no tool {name}"))
    }

    #[test]
    fn every_visible_command_path_is_a_tool_unless_it_is_human_only_or_the_same_as_a_subcommand() {
        let names: Vec<&str> = super::tools().unwrap().iter().map(|tool| tool.name.as_str()).collect();
        for name in ["stocktake", "bay", "bay_add", "park", "park_pop", "manifest", "manifest_provenance",
                     "office_list", "tag_list", "haul_list", "scope_status", "kv_set", "cherry_pick"] {
            assert!(names.contains(&name), "{name} must be a tool");
        }
        for name in ["office", "tag", "haul", "scope", "office_enroll", "office_regenesis", "prepare",
                     "self_schema", "mcp", "help", "kv"] {
            assert!(!names.contains(&name), "{name} must not be a tool");
        }
        assert_eq!(tool("tag_create").path, ["tag", "create"]);

        // A combination no agent passes is refused, and each of its arguments still works alone
        // or with others; the description names it.
        let (palletize, remote) = (tool("palletize"), "remote");
        for allowed in [json!({ "remote": true }), json!({ "name": "p", "delete": true }),
                        json!({ "name": "p", "restore": true }), json!({ "remote": true, "all": true }),
                        json!({ "name": "p", "restore": true, "remote": true }),
                        json!({ "name": "p", "delete": true, "remote": false })] {
            assert_eq!(palletize.refusal(&allowed), None, "{allowed}");
        }
        let refusal = palletize.refusal(&json!({ "name": "p", "remote": true, "delete": true })).unwrap();
        assert!(refusal.starts_with(&format!("the \"palletize\" tool refuses \"{remote}\" with \"delete\" together: that ")),
                "{refusal}");
        let description = palletize.definition()["description"].as_str().unwrap().to_string();
        assert!(description.contains(&format!("An agent may not pass \"{remote}\" with \"delete\" together")), "{description}");

        // The one-line description leads, so its git name survives, and the long one follows.
        let lift = tool("lift").description.clone();
        assert!(lift.starts_with("Upload the current pallet's new parcels to the remote (push)\n\n"), "{lift}");

        // An object argument describes the object, not the command line's word.
        let query = tool("query").definition();
        let where_help = query["inputSchema"]["properties"]["where"]["description"].as_str().unwrap();
        assert!(where_help.starts_with("A JSON predicate tree AND-ed with the other arguments."), "{where_help}");

        // The command line agrees with the tools that a page holds at least one parcel.
        for command in ["history", "query"] {
            assert!(crate::cli::Cli::command().try_get_matches_from(["forklift", command, "--limit", "0"]).is_err());
            assert!(crate::cli::Cli::command().try_get_matches_from(["forklift", command, "--limit", "1"]).is_ok());
        }

        // A command added to the tree is a tool without anything else to do.
        let tree = crate::cli::Cli::command()
            .subcommand(clap::Command::new("brand-new").about("New").arg(clap::Arg::new("thing").long("the-thing")));
        let tools = super::generate_tools(&tree).unwrap();
        let added = tools.iter().find(|tool| tool.name == "brand_new").unwrap();
        assert_eq!(added.params[0].key, "the_thing");
    }

    #[test]
    fn a_tree_the_tables_or_the_tool_shape_cannot_describe_is_refused() {
        let refused = |tree: clap::Command| super::generate_tools(&tree).err().unwrap();
        let cli = crate::cli::Cli::command;

        assert!(refused(cli().mut_subcommand("alias", |alias| alias.hide(true))).contains("HUMAN_ONLY names"));
        assert!(refused(cli().mut_subcommand("haul", |haul| haul.subcommand_required(true))).contains("BARE_SAME_AS names"));
        assert!(refused(cli().mut_subcommand("kv", |kv| kv.arg(clap::Arg::new("x").long("x"))))
            .contains("`forklift kv` has subcommands and its own argument"));
        assert!(refused(cli().subcommand(clap::Command::new("cherry_pick").about("Twin")))
            .contains("two commands make the tool name `cherry_pick`"));
        assert!(refused(cli().subcommand(clap::Command::new("twin").about("Twin")
            .arg(clap::Arg::new("a-b").long("a-b"))
            .arg(clap::Arg::new("a_b").required(true))))
            .contains("make the property `a_b`"));
        assert!(refused(cli().subcommand(clap::Command::new("counted").about("Counted")
            .arg(clap::Arg::new("n").short('n').action(clap::ArgAction::Count))))
            .contains("has no tool shape"));
        assert!(refused(cli().mut_subcommand("history", |history| history.mut_arg("limit", |limit| limit.long("max"))))
            .contains("a tool table names `limit` of"));
        assert!(refused(cli().subcommand(clap::Command::new("mute"))).ends_with("has no description"));
        let error = refused(cli().mut_subcommand("palletize", |palletize| palletize.mut_arg("remote", |remote| remote.hide(true))));
        assert!(error.starts_with("HUMAN_ONLY_TOGETHER names") && error.ends_with("which is not an argument of its tool"), "{error}");
        let mut tools = super::generate_tools(&cli()).unwrap();
        let error = super::attach_together(&mut tools, &[("palletize", &["delete", "delete"], "twice")]).unwrap_err();
        assert!(error.contains("fewer than two distinct arguments"), "{error}");

        // An empty list puts nothing in the command, so it does not complete a combination.
        let tree = cli().subcommand(clap::Command::new("multi").about("Multi")
            .arg(clap::Arg::new("tag").long("tag").action(clap::ArgAction::Append))
            .arg(clap::Arg::new("flag").long("flag").action(clap::ArgAction::SetTrue)));
        let mut tools = super::generate_tools(&tree).unwrap();
        super::attach_together(&mut tools, &[("multi", &["tag", "flag"], "both")]).unwrap();
        let multi = tools.iter().find(|tool| tool.name == "multi").unwrap();
        assert_eq!(multi.refusal(&json!({ "tag": [], "flag": true })), None);
        assert!(multi.refusal(&json!({ "tag": ["a"], "flag": true })).is_some());
        let error = refused(cli().mut_subcommand("palletize", |palletize| palletize.hide(true)));
        assert!(error.starts_with("HUMAN_ONLY_TOGETHER names") && error.ends_with("which is not a tool"), "{error}");
        assert!(refused(cli().subcommand(clap::Command::new("signed").about("Signed")
            .arg(clap::Arg::new("depth").long("depth").value_parser(clap::value_parser!(i64)))))
            .contains("parses a number the tool schema has no type for"));
    }

    #[test]
    fn an_argument_the_tool_does_not_declare_of_the_wrong_type_or_missing_is_refused() {
        let refused = |name: &str, arguments: serde_json::Value| tool(name).refusal(&arguments);

        assert!(refused("history", json!({ "class": "agent", "limit": 5 })).unwrap().contains("\"class\""));
        assert_eq!(refused("query", json!({ "class": "agent", "limit": 5 })), None);
        assert_eq!(refused("history", json!({})), None);
        assert_eq!(refused("manifest_provenance", json!({ "revision": "r", "model": "m", "tool": 5, "session": "s" })), None);

        // A declared argument of the wrong type would otherwise be dropped just the same.
        for (name, arguments) in [
            ("query", json!({ "class": ["agent"] })),
            ("query", json!({ "where": "{}" })),
            ("lower", json!({ "expect": 123 })),
            ("lower", json!({ "keep": ["@kv:a=ours", 1] })),
            ("lower", json!({ "keep": "@kv:a=ours" })),
            ("bay_add", json!({ "name": "b", "scope": "src" })),
            ("history", json!({ "limit": "5" })),
            ("history", json!({ "limit": 0 })),
            ("stocktake", json!({ "summary": "yes" })),
            ("history", json!(["class", "agent"])),
            ("kv_get", json!({ "revision": "main" })),
            ("blame", json!({})),
        ] {
            assert!(refused(name, arguments.clone()).is_some(), "{} {} must be refused", name, arguments);
        }
        assert_eq!(refused("query", json!({ "where": { "field": "class", "op": "eq", "value": "agent" } })), None);
        assert_eq!(refused("lower", json!({ "keep": ["@kv:a=ours"] })), None);

        // A null is an omitted optional, and a required one stays missing.
        assert_eq!(refused("history", json!({ "revision": null, "limit": null, "after": null })), None);
        assert_eq!(refused("kv_get", json!({ "revision": "main", "key": null })).unwrap(),
                   "the \"kv_get\" tool needs the \"key\" argument");
        assert_eq!(refused("history", json!({ "limit": "5" })).unwrap(),
                   "the \"history\" tool's \"limit\" argument must be an integer of at least 1");
        assert_eq!(refused("lower", json!({ "keep": "x" })).unwrap(),
                   "the \"lower\" tool's \"keep\" argument must be an array of strings");

        // An argument with fixed values takes only those.
        let tree = crate::cli::Cli::command().subcommand(clap::Command::new("pick").about("Pick")
            .arg(clap::Arg::new("side").long("side").value_parser(["ours", "theirs"])));
        let tools = super::generate_tools(&tree).unwrap();
        let pick = tools.iter().find(|tool| tool.name == "pick").unwrap();
        assert_eq!(pick.refusal(&json!({ "side": "ours" })), None);
        assert_eq!(pick.refusal(&json!({ "side": "both" })).unwrap(),
                   "the \"pick\" tool's \"side\" argument must be one of \"ours\", \"theirs\"");
    }

    #[test]
    fn the_arguments_become_options_attached_and_positionals_after_a_double_dash() {
        let argv = |name: &str, arguments: serde_json::Value| tool(name).argv(arguments.as_object().unwrap());

        assert_eq!(argv("kv_set", json!({ "revision": "main", "key": "ci", "value": "-green" })).unwrap(),
                   ["kv", "set", "--", "main", "ci", "-green"]);
        assert_eq!(argv("kv_list", json!({})).unwrap(), ["kv", "list"]);
        assert_eq!(argv("lower", json!({ "keep": ["@kv:a/b=ours", "@kv:c=theirs"] })).unwrap(),
                   ["lower", "--keep=@kv:a/b=ours", "--keep=@kv:c=theirs"]);
        assert_eq!(argv("history", json!({ "limit": 5, "oneline": true, "after": null })).unwrap(),
                   ["history", "--limit=5", "--oneline"]);
        assert_eq!(argv("history", json!({ "oneline": false })).unwrap(), ["history"]);
        assert_eq!(argv("query", json!({ "where": { "field": "class" } })).unwrap(),
                   ["query", r#"--where={"field":"class"}"#]);
        assert_eq!(argv("undo", json!({ "as": "-x" })).unwrap(), ["undo", "--as=-x"]);

        // A later positional without the one before it would be read as that one.
        assert_eq!(argv("palletize", json!({ "revision": "r" })).unwrap_err(),
                   "the \"revision\" argument needs the \"name\" argument before it");
    }

    #[test]
    fn an_injected_argument_is_absent_from_the_schema_and_named_in_the_description() {
        let provenance = tool("manifest_provenance").definition();
        let properties = provenance["inputSchema"]["properties"].as_object().unwrap();
        assert!(properties.contains_key("model"));
        assert!(!properties.contains_key("tool") && !properties.contains_key("session"));
        assert!(provenance["description"].as_str().unwrap()
            .ends_with("The server sets \"tool\" and \"session\" itself, from this connection; a value passed for them is discarded."));
        assert_eq!(tool("manifest_provenance").argv(json!({ "revision": "r", "model": "m", "tool": "t", "session": "s" })
            .as_object().unwrap()).unwrap(),
                   ["manifest", "provenance", "--model=m", "--tool=t", "--session=s", "--", "r"]);
    }

    /// What one tool argument binds to when the line parses.
    #[derive(Debug, PartialEq)]
    enum Bound {
        On,
        Values(Vec<String>),
    }

    /// A value for `param` that a careless argv would misread: a string that spells an option and
    /// a global, two of them when the argument repeats.
    fn hostile(param: &super::ToolParam) -> serde_json::Value {
        let one = |n: u32| match &param.value {
            super::ValueKind::Boolean => json!(true),
            super::ValueKind::String => json!(format!("-{}{n} --verbose", param.key)),
            super::ValueKind::Choice(choices) => json!(choices[n as usize % choices.len()]),
            super::ValueKind::Integer(minimum) => json!(minimum + 7 + n as u64),
            super::ValueKind::Object => json!({ "field": "class", "op": "eq", "value": format!("-{n} --verbose") }),
        };
        if param.repeats() { json!([one(1), one(2)]) } else { one(1) }
    }

    /// Run `arguments` through `tool`'s argv and clap, the way a call does. Only clap's own rules
    /// apply here: a cross-argument rule the binary checks after parsing still refuses some
    /// combinations clap accepts, with a usage error. `Ok` means clap
    /// accepted the line and every argument bound exactly to the clap argument it names, with
    /// nothing else and no global set (anything else panics); `Err` is a refusal by argv or by
    /// clap for a combination the command does not accept.
    /// clap's parse of an argv line, with the binary's tree alone: no prefix of a command name
    /// resolves here, so a tool whose argv spelled one would fail.
    fn matches_of(line: &[String]) -> Result<clap::ArgMatches, clap::Error> {
        crate::cli::Cli::command().try_get_matches_from(line)
    }

    /// A prefix the binary itself would expand is no command to the argv check.
    #[test]
    fn the_argv_check_parses_with_prefixes_off() {
        let line = |words: &[&str]| words.iter().map(|word| word.to_string()).collect::<Vec<_>>();
        assert!(matches_of(&line(&["forklift", "--json", "haul", "list"])).is_ok());
        for words in [&["forklift", "--json", "ha", "list"][..], &["forklift", "--json", "haul", "li"][..]] {
            let error = matches_of(&line(words)).map(|_| ()).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand, "{words:?}");
        }
    }

    fn parse(tool: &super::Tool, arguments: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
        use clap::FromArgMatches;
        use clap::parser::ValueSource;

        let argv = tool.argv(arguments)?;
        let line: Vec<String> = ["forklift", "--json"].iter().map(|word| word.to_string()).chain(argv.iter().cloned()).collect();
        let root = crate::cli::Cli::command();
        let mut matches = match matches_of(&line) {
            Ok(matches) => matches,
            Err(error) if matches!(error.kind(), clap::error::ErrorKind::ArgumentConflict
                | clap::error::ErrorKind::MissingRequiredArgument) => return Err(error.to_string()),
            Err(error) => panic!("{line:?}: {error}"),
        };
        crate::cli::Cli::from_arg_matches(&matches).unwrap_or_else(|error| panic!("{line:?}: {error}"));

        for global in root.get_arguments().filter(|arg| arg.get_id() != "json") {
            assert_ne!(matches.value_source(global.get_id().as_str()), Some(ValueSource::CommandLine),
                       "{line:?}: set the global {}", global.get_id());
        }

        let mut leaf = &root;
        for name in &tool.path {
            let (got, sub) = matches.remove_subcommand().unwrap_or_else(|| panic!("{line:?}: no subcommand {name}"));
            assert_eq!(&got, name, "{line:?}");
            matches = sub;
            leaf = leaf.find_subcommand(name).unwrap();
        }
        assert!(matches.subcommand_name().is_none(), "{line:?}: ran a deeper command");
        for global in root.get_arguments().filter(|arg| arg.get_id() != "json") {
            assert_ne!(matches.value_source(global.get_id().as_str()), Some(ValueSource::CommandLine),
                       "{line:?}: a value set the global {}", global.get_id());
        }

        let mut bound = std::collections::BTreeMap::new();
        for arg in leaf.get_arguments().filter(|arg| !arg.is_global_set()) {
            let id = arg.get_id().as_str();
            if matches.value_source(id) != Some(ValueSource::CommandLine) {
                continue;
            }
            let value = if matches!(arg.get_action(), clap::ArgAction::SetTrue) {
                Bound::On
            } else {
                Bound::Values(matches.get_raw(id).unwrap().map(|value| value.to_string_lossy().into_owned()).collect())
            };
            bound.insert(id.to_string(), value);
        }

        // What the arguments name, judged from the clap leaf: an option or switch by its long
        // name, a positional by its id.
        let mut expected = std::collections::BTreeMap::new();
        for (key, value) in arguments.iter().filter(|(_, value)| !value.is_null()) {
            let arg = leaf.get_arguments()
                .find(|arg| match arg.get_long() {
                    Some(long) => long.replace('-', "_") == *key,
                    None => arg.get_id() == key,
                })
                .unwrap_or_else(|| panic!("{line:?}: no clap argument for {key}"));
            let words = |value: &serde_json::Value| match value {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            let value = match value {
                serde_json::Value::Bool(true) => Bound::On,
                serde_json::Value::Bool(false) => continue,
                serde_json::Value::Array(items) => Bound::Values(items.iter().map(words).collect()),
                other => Bound::Values(vec![words(other)]),
            };
            expected.insert(arg.get_id().to_string(), value);
        }
        assert_eq!(bound, expected, "{line:?}");
        Ok(())
    }

    #[test]
    fn every_tool_argument_parses_against_clap_and_binds_where_it_names() {
        for tool in super::tools().unwrap() {
            let required: serde_json::Map<String, serde_json::Value> = tool.params.iter()
                .filter(|param| param.required)
                .map(|param| (param.key.clone(), hostile(param)))
                .collect();
            // The required arguments alone may still need one of a set of others (`peek` takes a
            // hash or `--inventory`); the optional ones below bind with them then.
            let alone = parse(tool, &required);
            if tool.params.iter().all(|param| param.required) {
                alone.unwrap_or_else(|error| panic!("{}: the required arguments alone: {error}", tool.name));
            }

            // Each optional argument binds on its own, or with up to two others it needs (an
            // option that requires another, a positional after an optional one).
            for param in tool.params.iter().filter(|param| !param.required) {
                let with = |others: &[&super::ToolParam]| {
                    let mut arguments = required.clone();
                    for other in others {
                        arguments.insert(other.key.clone(), hostile(other));
                    }
                    parse(tool, &arguments)
                };
                let alone = with(&[param]);
                let others: Vec<&super::ToolParam> = tool.params.iter()
                    .filter(|other| other.key != param.key && !other.required)
                    .collect();
                let paired = || others.iter().enumerate().any(|(index, other)| {
                    with(&[other, param]).is_ok()
                        || others[index + 1..].iter().any(|third| with(&[other, third, param]).is_ok())
                });
                assert!(alone.is_ok() || paired(), "{}: \"{}\" binds in no combination: {:?}", tool.name, param.key, alone);
            }
        }
    }
}
