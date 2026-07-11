//! The machine-facing output layer (DESIGN.html §7.4): a process-wide `--json` mode,
//! a versioned envelope, and the stable error/exit-code taxonomy (§7.8).
//!
//! Every command emits its result through one of the helpers here, so an agent parses
//! a single shape regardless of the command, and a human reads prose. The rules:
//!
//! * A command with real structured data implements [`CommandOutput`] and calls
//!   [`emit`] — human mode renders prose, JSON mode wraps the data in the envelope.
//! * A command whose result is just a sentence calls [`message`].
//! * Progress lines (silent under `--json`, they are not part of the result) use the
//!   [`human!`](crate::human) macro.
//!
//! Nothing else prints to stdout. That is what keeps `--json` output a single valid
//! JSON document.

use std::sync::OnceLock;
use serde::Serialize;
use forklift_core::util::scope_utils;

/// The output schema version, carried on every JSON envelope as `forklift_json`.
/// It changes only when the envelope or a command's `data` shape changes
/// incompatibly, so an agent can pin it and detect drift.
pub const SCHEMA_VERSION: &str = "1";

/// How the process renders its output.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Prose on stdout, for a person.
    Human,

    /// One JSON envelope on stdout, for a program.
    Json,
}

static MODE: OnceLock<OutputMode> = OnceLock::new();

/// Set the output mode for the process. Called once, from `main`, before any command
/// runs; a second call is ignored (the mode is a one-way switch).
pub fn set_mode(mode: OutputMode) {
    let _ = MODE.set(mode);
}

/// The active output mode (`Human` until `set_mode` runs).
pub fn mode() -> OutputMode {
    *MODE.get().unwrap_or(&OutputMode::Human)
}

/// Whether the process is emitting JSON.
pub fn is_json() -> bool {
    mode() == OutputMode::Json
}

/// A command's successful result: serializable for `--json`, renderable as prose for
/// a person. The `Serialize` shape *is* the public JSON schema — treat a change to it
/// as a schema change (bump [`SCHEMA_VERSION`]).
pub trait CommandOutput: Serialize {
    /// Print the human-readable form to stdout.
    fn render_human(&self);
}

/// Emit a command's structured result in the active mode.
pub fn emit<T: CommandOutput>(command: &str, value: &T) {
    match mode() {
        OutputMode::Human => value.render_human(),
        OutputMode::Json => print_envelope(command, value),
    }
}

/// Emit a result whose only content is a human sentence (`{ "message": "…" }` in
/// JSON). For commands that report an outcome rather than data.
pub fn message(command: &str, text: impl Into<String>) {
    let text = text.into();

    match mode() {
        OutputMode::Human => println!("{}", text),
        OutputMode::Json => {
            print_envelope(command, &serde_json::json!({ "message": text }))
        }
    }
}

/// Render a byte count in a human-friendly binary unit (B/KiB/MiB/GiB/TiB). The object store's
/// scale makes raw byte counts noise, so the size-reporting commands (`compact`, `store`) show
/// this instead. JSON output always carries the exact byte count, never this string.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;

    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

/// Print the success envelope: `{ forklift_json, command, ok: true, data }`.
fn print_envelope<T: Serialize>(command: &str, data: &T) {
    let envelope = serde_json::json!({
        "forklift_json": SCHEMA_VERSION,
        "command": command,
        "ok": true,
        "data": data,
    });

    // Pretty-printed: the consumer is a program, but a human debugging the pipe reads
    // it too, and JSON parsers do not care about whitespace.
    match serde_json::to_string_pretty(&envelope) {
        Ok(text) => println!("{}", text),
        Err(_) => println!("{{\"forklift_json\":\"{}\",\"ok\":true}}", SCHEMA_VERSION),
    }
}

/// Report a failed command in the active mode, to stderr (human) or stdout (JSON —
/// the error is still the command's one result document). Called once, from `main`.
pub fn report_error(error: &ForkliftError) {
    match mode() {
        OutputMode::Human => eprintln!("{}", error.message),
        OutputMode::Json => {
            let mut fields = serde_json::Map::new();
            fields.insert("code".to_string(), serde_json::json!(error.code.as_str()));
            fields.insert("message".to_string(), serde_json::json!(error.message));

            if let Some(next_step) = &error.next_step {
                fields.insert("next_step".to_string(), serde_json::json!(next_step));
            }

            let envelope = serde_json::json!({
                "forklift_json": SCHEMA_VERSION,
                "ok": false,
                "error": serde_json::Value::Object(fields),
            });

            match serde_json::to_string_pretty(&envelope) {
                Ok(text) => println!("{}", text),
                Err(_) => println!(
                    "{{\"forklift_json\":\"{}\",\"ok\":false}}", SCHEMA_VERSION
                ),
            }
        }
    }
}

/// A stable error classification: a machine-branchable code and a deterministic exit
/// code (§7.8). The string codes and the exit numbers are a public contract — add,
/// never repurpose.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// Anything without a more specific classification yet.
    Generic,

    /// The command must run inside a warehouse, and the working directory is not one.
    NotAWarehouse,

    /// Another forklift process holds the warehouse lock.
    WarehouseLocked,

    /// The working state blocks the operation: unresolved conflicts, a dirty tree.
    Conflict,

    /// A remote ref moved under a lift (the CAS failed): lower, consolidate, retry.
    Diverged,

    /// A path argument is outside the bay's materialization scope (§7.6).
    OutOfScope,

    /// A merge in a scoped bay hit an out-of-scope entry that changed on both sides:
    /// there is no content to reconcile, so it refuses rather than guess.
    OutOfScopeConflict,

    /// A scoped bay's spine path flipped between a directory and a file (§7.6): the scope
    /// is no longer valid there and the operation refuses rather than guess.
    ScopePathTypeChanged,

    /// A whole-tree verb is not (yet) supported in a scoped (sparse) bay (§7.6).
    SparseWorkspace,
}

impl ErrorCode {
    /// The stable string an agent branches on.
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::Generic              => "error",
            ErrorCode::NotAWarehouse        => "not_a_warehouse",
            ErrorCode::WarehouseLocked      => "warehouse_locked",
            ErrorCode::Conflict             => "conflict",
            ErrorCode::Diverged             => "diverged",
            ErrorCode::OutOfScope           => scope_utils::CODE_OUT_OF_SCOPE,
            ErrorCode::OutOfScopeConflict   => scope_utils::CODE_OUT_OF_SCOPE_CONFLICT,
            ErrorCode::ScopePathTypeChanged => scope_utils::CODE_SCOPE_PATH_TYPE_CHANGED,
            ErrorCode::SparseWorkspace      => scope_utils::CODE_SPARSE_WORKSPACE,
        }
    }

    /// The process exit code — deterministic per class, so a script branches without
    /// parsing prose (§7.8). `2` is reserved for clap's usage errors; `0` is success.
    pub fn exit_code(&self) -> i32 {
        match self {
            ErrorCode::Generic              => 1,
            ErrorCode::NotAWarehouse        => 3,
            ErrorCode::Conflict             => 4,
            ErrorCode::Diverged             => 5,
            ErrorCode::WarehouseLocked      => 6,
            ErrorCode::OutOfScope           => 7,
            ErrorCode::ScopePathTypeChanged => 8,
            ErrorCode::SparseWorkspace      => 9,
            ErrorCode::OutOfScopeConflict   => 10,
        }
    }

    /// Map a scope-refusal code (a `scope_utils::CODE_*` string) to its `ErrorCode`.
    fn from_scope_code(code: &str) -> Option<ErrorCode> {
        match code {
            _ if code == scope_utils::CODE_OUT_OF_SCOPE           => Some(ErrorCode::OutOfScope),
            _ if code == scope_utils::CODE_OUT_OF_SCOPE_CONFLICT  => Some(ErrorCode::OutOfScopeConflict),
            _ if code == scope_utils::CODE_SCOPE_PATH_TYPE_CHANGED => Some(ErrorCode::ScopePathTypeChanged),
            _ if code == scope_utils::CODE_SPARSE_WORKSPACE       => Some(ErrorCode::SparseWorkspace),
            _ => None,
        }
    }
}

/// A command failure: a classification, the human message, and — for the codes where
/// there is a clear one — a machine-actionable next step (§7.4: agents recover
/// instead of flailing).
pub struct ForkliftError {
    pub code: ErrorCode,
    pub message: String,
    pub next_step: Option<String>,
}

impl ForkliftError {
    /// A classified error with a next step.
    pub fn new(code: ErrorCode, message: impl Into<String>, next_step: impl Into<String>) -> ForkliftError {
        ForkliftError {
            code,
            message: message.into(),
            next_step: Some(next_step.into()),
        }
    }
}

/// A bare `Err(String)` from a handler is a generic failure with no next step — the
/// `?`-friendly default. Scope refusals (§7.6) are the exception: `forklift-core` cannot
/// build a `ForkliftError` (it never prints, and the type is CLI-local), so it frames the
/// refusal as a sentinel-tagged string that this conversion decodes into a classified error
/// with the matching stable code, exit code and next step.
///
/// A frame whose `code` this build does not recognize (e.g. a newer `forklift-core` added a
/// scope code this CLI predates) still decodes — the frame itself is well-formed — so it
/// still uses the decoded human message and next step rather than falling through to the
/// raw framed string, which would leak the `\u{1f}` field separators into human/JSON output.
/// It just classifies as `Generic` instead of the (unknown) specific code. Only a string that
/// is not a scope refusal at all — `decode_refusal` returns `None` — is treated as a plain
/// generic error verbatim.
impl From<String> for ForkliftError {
    fn from(message: String) -> ForkliftError {
        if let Some((code, human, next_step)) = scope_utils::decode_refusal(&message) {
            let code = ErrorCode::from_scope_code(code).unwrap_or(ErrorCode::Generic);

            return ForkliftError {
                code,
                message: human.to_string(),
                next_step: Some(next_step.to_string()),
            };
        }

        ForkliftError { code: ErrorCode::Generic, message, next_step: None }
    }
}

/// Print a progress or prose line in human mode; do nothing under `--json` (progress
/// is not part of the result document). Use for status chatter, never for the result.
#[macro_export]
macro_rules! human {
    ($($arg:tt)*) => {
        if !$crate::output::is_json() {
            println!($($arg)*);
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_known_scope_code_classifies_and_keeps_the_next_step() {
        let refusal = scope_utils::out_of_scope_refusal("src/web");
        let error: ForkliftError = refusal.into();

        assert_eq!(error.code.as_str(), scope_utils::CODE_OUT_OF_SCOPE);
        assert!(!error.message.contains('\u{1f}'));
        assert!(error.next_step.is_some());
    }

    #[test]
    fn an_unrecognized_scope_code_still_decodes_instead_of_leaking_the_raw_frame() {
        // A well-formed frame whose code this build has never heard of — as if a newer
        // `forklift-core` introduced a scope code this CLI predates.
        let frame = scope_utils::refusal("some_future_code", "a human explanation", "do this");

        let error: ForkliftError = frame.into();

        assert_eq!(error.code.as_str(), "error"); // ErrorCode::Generic
        assert_eq!(error.message, "a human explanation");
        assert_eq!(error.next_step.as_deref(), Some("do this"));
        assert!(!error.message.contains('\u{1f}'), "the raw frame must never leak into the message");
    }

    #[test]
    fn a_plain_string_is_a_generic_error_with_no_next_step() {
        let error: ForkliftError = "something ordinary went wrong".to_string().into();

        assert_eq!(error.code.as_str(), "error");
        assert_eq!(error.message, "something ordinary went wrong");
        assert!(error.next_step.is_none());
    }
}
