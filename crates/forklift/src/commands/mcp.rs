//! `forklift mcp` — the Model Context Protocol server (DESIGN.html §7.4): the whole
//! command surface as native MCP tools, so an agent calls schema-typed tools instead
//! of shelling out and scraping prose.
//!
//! Transport is the MCP stdio convention: newline-delimited JSON-RPC 2.0 on
//! stdin/stdout. A `tools/call` re-invokes this same binary with `--json` and captures
//! its envelope, so every tool reuses the exact structured output (and the warehouse
//! lock, and the exit-code taxonomy) the CLI already produces — one source of truth.

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

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    // State kept for the life of this connection — see `ServerState`.
    let mut state = ServerState::new();

    for line in stdin.lock().lines() {
        let line = line.map_err(|e| format!("Error while reading stdin: {}", e))?;

        if line.trim().is_empty() {
            continue;
        }

        let Some(response) = handle_message(&line, &mut state) else {
            // A notification (no id) gets no response.
            continue;
        };

        let text = serde_json::to_string(&response)
            .map_err(|e| format!("Error while encoding a response: {}", e))?;

        writeln!(stdout, "{}", text).map_err(|e| format!("Error while writing stdout: {}", e))?;
        stdout.flush().map_err(|e| format!("Error while flushing stdout: {}", e))?;
    }

    Ok(())
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
        }
    }
}

/// Handle one JSON-RPC message; `None` for a notification (which gets no reply) or an
/// unparseable line we choose to ignore rather than crash the session.
fn handle_message(line: &str, state: &mut ServerState) -> Option<Value> {
    let message: Value = serde_json::from_str(line).ok()?;

    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str)?;

    // A message without an id is a notification: act on nothing, answer nothing.
    let id = id?;

    let params = message.get("params").cloned().unwrap_or(json!({}));

    let result = match method {
        "initialize" => Ok(initialize(&params, state)),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => call_tool(&params, state),
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

    json!({
        "protocolVersion": protocol_version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "forklift", "version": SERVER_VERSION },
    })
}

/// Dispatch a `tools/call`: map the tool and its arguments to a `forklift … --json`
/// invocation, run it, and return the envelope as the tool result. A command that
/// exits non-zero is reported as an MCP tool error (`isError: true`) carrying the
/// error envelope — the agent sees the stable `code`/`next_step`, not a crash.
fn call_tool(params: &Value, state: &ServerState) -> Result<Value, JsonRpcError> {
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

    let output = run_forklift(&args)
        .map_err(|reason| JsonRpcError::internal(&reason))?;

    let text = String::from_utf8_lossy(&output.stdout).to_string();

    Ok(json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": !output.status.success(),
    }))
}

/// Run this same binary with the given arguments plus `--json`, capturing its output.
fn run_forklift(args: &[String]) -> Result<std::process::Output, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("Cannot locate the forklift binary: {}", e))?;

    let mut command = std::process::Command::new(exe);
    command.args(args).arg("--json");

    command.output().map_err(|e| format!("Error while running \"forklift {}\": {}", args.join(" "), e))
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
        tool("unload", "Stage a file or directory for removal.",
            object(json!({ "path": string }), json!(["path"]))),
        tool("diff", "Show changed files. Default: working directory vs inventory. staged=true: inventory vs head. targets: two revisions to compare, optionally plus a path.",
            object(json!({ "staged": boolean, "targets": { "type": "array", "items": string } }), json!([]))),
        tool("history", "Walk the parcel history newest first from a revision (default: the current pallet). Read it in pages with limit (max parcels) plus after (the data.next cursor from the previous page); class filters by author identity class (human|agent|bot|service).",
            object(json!({ "revision": string, "class": string, "limit": integer, "after": string }), json!([]))),
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
