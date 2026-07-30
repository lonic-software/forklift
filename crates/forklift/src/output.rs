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
use forklift_core::error::{CoreError, RefusalCode};
use forklift_core::util::{heal_utils, load_guard_utils, merge_utils, query_utils, scope_utils};

/// The output schema version, carried on every JSON envelope as `forklift_json`.
/// It changes only when the envelope or a command's `data` shape changes
/// incompatibly, so an agent can pin it and detect drift.
///
/// Version 2 (this one): `history` entries carry `parents` (always present, `[]` for a
/// root), the `empty_history` error code exists, and `palletize` list entries carry
/// `head`. A consumer reads this field first, before parsing anything else, to know
/// whether the command it is about to run supports the capability it needs — the
/// version bump *is* the capability-detection mechanism, so it is worth pinning rather
/// than sniffing for a field's presence.
pub const SCHEMA_VERSION: &str = "2";

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

/// The densify suggestion (§ densify-pending marker, `forklift_core::util::pack_utils`): shown
/// by `store`, `import-git` and `franchise` whenever the object store carries content a bulk
/// ingest wrote without ever seeing cross-path similarity. One shared string so the three
/// commands never drift apart in wording. Never phrased as "recommended" — redelta is an
/// optional, opt-in, CPU-bound pass, not something owed to every store.
pub const DENSIFY_TIP: &str = "Tip: \"forklift compact --all --redelta\" can significantly \
    shrink this store (one-shot, takes a few minutes on large histories).";

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

/// Classify blob bytes as safely displayable text, for the commands (`show`, `peek`) that
/// report a `binary` signal instead of printing raw bytes. Text means **both** NUL-free
/// (`merge_utils::is_mergeable_text`, the same test `diff`'s line-by-line display uses) **and**
/// valid UTF-8 — a NUL-free blob can still be invalid UTF-8, and a NUL byte is itself valid
/// UTF-8, so neither check alone is sufficient. Only this passing means content is safe to
/// hand to a person or a JSON string: nothing here or downstream ever falls back to a lossy
/// conversion, so no caller can silently mangle non-UTF-8 bytes into fake text.
///
/// # Arguments
/// * `content` - The blob's raw bytes.
///
/// # Returns
/// * `Some(&str)` - The content, when it is safe to display as text.
/// * `None`       - When the content is binary (a NUL byte, or invalid UTF-8).
pub fn blob_text(content: &[u8]) -> Option<&str> {
    if !merge_utils::is_mergeable_text(content) {
        return None;
    }

    std::str::from_utf8(content).ok()
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

/// Defines `ErrorCode` and generates [`ErrorCode::ALL`] from the same variant list, in the
/// same expansion — so the two can never drift apart. Before this macro, `ALL` was a
/// hand-kept-in-sync array next to the enum, guarded only by a separate exhaustive-match
/// function; nothing forced a new variant to be *added to `ALL`*, only to be matched somewhere
/// — which is exactly how the docs generator could still silently miss a code. Deriving `ALL`
/// from the same list the enum itself is built from removes the possibility outright: there is
/// only one list, so there is nothing left to fall out of sync. `as_str`/`exit_code`/
/// `description`/`from_refusal` stay ordinary exhaustive `match` expressions below (the
/// compiler already forces those to cover every variant; the macro would add nothing there).
macro_rules! error_codes {
    ($($(#[$meta:meta])* $variant:ident),+ $(,)?) => {
        /// A stable error classification: a machine-branchable code and a deterministic exit
        /// code (§7.8). The string codes and the exit numbers are a public contract — add,
        /// never repurpose.
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub enum ErrorCode {
            $($(#[$meta])* $variant),+
        }

        impl ErrorCode {
            /// Every variant, oldest-first (the order the docs generator renders). Generated by
            /// [`error_codes!`] from the same list as the enum itself, so a variant can never be
            /// added here without it.
            pub const ALL: [ErrorCode; { [$(ErrorCode::$variant),+].len() }] = [
                $(ErrorCode::$variant),+
            ];
        }
    };
}

// Variant order below is ascending exit code (see `exit_code()`), which is also the order
// `ErrorCode::ALL` renders in — not the historical enum-declaration order (harmless: exit
// codes/strings are the public contract, not a variant's discriminant or declaration position).
error_codes! {
    /// Anything without a more specific classification yet.
    Generic,

    /// The command must run inside a warehouse, and the working directory is not one.
    NotAWarehouse,

    /// The working state blocks the operation: unresolved conflicts, a dirty tree.
    Conflict,

    /// A remote ref moved under a lift (the CAS failed): lower, consolidate, retry.
    Diverged,

    /// Another forklift process holds the warehouse lock.
    WarehouseLocked,

    /// A path argument is outside the bay's materialization scope (§7.6).
    OutOfScope,

    /// A scoped bay's spine path flipped between a directory and a file (§7.6): the scope
    /// is no longer valid there and the operation refuses rather than guess.
    ScopePathTypeChanged,

    /// A whole-tree verb is not (yet) supported in a scoped (sparse) bay (§7.6).
    SparseWorkspace,

    /// A merge in a scoped bay hit an out-of-scope entry that changed on both sides:
    /// there is no content to reconcile, so it refuses rather than guess.
    OutOfScopeConflict,

    /// A lift from a sparse warehouse was aimed at a remote other than its origin: the
    /// out-of-scope closure was only ever proved present on the origin, so it refuses up front.
    NonOriginLift,

    /// `narrow` was asked to drop a subtree that still holds uncommitted work (staged,
    /// unstaged or untracked): it refuses rather than silently discard it.
    NarrowUnclean,

    /// `scope-prune` was asked to free a path a checkout still materializes: freeing it would
    /// break that checkout, so it refuses until the checkout narrows the path away.
    ScopePruneBlocked,

    /// A large chunked file was asked to go into a bundle (chunks are structurally excluded from
    /// every bundle — a bundle never carries them), or a lift was asked to send one to a remote
    /// whose handshake did not advertise chunking support (an old head's `gc` would silently
    /// collect a recipe's chunks). Either case refuses client-side rather than ship content that
    /// can never be materialized where it lands.
    ChunkedTransportUnsupported,

    /// A bundle or lift was asked to send an object above the whole-object ceiling: a
    /// grandfathered giant (authored, or imported via an old-version bundle, before the ceiling
    /// existed) stays readable and checkout-able locally forever, but no migration preserves its
    /// signed identity, so nothing accepts it in transport. Refuses client-side (or at the
    /// bundle-building source) rather than ship something no reader could finish importing.
    OversizedTransportUnsupported,

    /// A lift's commit would need more than one paginated batch (more than `MAX_MISSING_BATCH`
    /// distinct staged objects), and the remote does not advertise support for the pagination
    /// (§9.4b Stage 3, W3 — the additive `more` field shipped with chunking support, so a
    /// pre-chunking remote silently mishandles it). Refused right after negotiation, before a
    /// single byte is uploaded, rather than wasting the whole upload and failing confusingly at
    /// commit time.
    CommitPaginationUnsupported,

    /// A `query` predicate is not acceptable as written: malformed (unparsable JSON, an unknown
    /// field or operator, a value of the wrong type), or past one of the fixed bounds (payload
    /// size, nesting depth, leaf count, `in`-array length, glob length).
    QueryPredicateInvalid,

    /// `history` was asked to walk a pallet that has nothing stacked on it yet: there is no
    /// history to show. Head-only (there is no parcel graph to enter) and scoped to `history`
    /// alone, so it is classified here directly rather than as a `forklift-core` `RefusalCode`.
    EmptyHistory,

    /// `stack` or `park` refused because a `load` that never finished cleanly is still recorded
    /// (`load_guard_utils`): committing the staged inventory now could silently produce an
    /// incomplete result.
    IncompleteLoad,

    /// The automatic entry-heal (`heal_utils`) hit a durability taint it could not resolve on its
    /// own (a vanished, unreadable, or corrupt object, or a torn taint record) and refused rather
    /// than silently trust unproven state.
    DurabilityTaint,
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
            ErrorCode::NonOriginLift        => scope_utils::CODE_NON_ORIGIN_LIFT,
            ErrorCode::NarrowUnclean        => scope_utils::CODE_NARROW_UNCLEAN,
            ErrorCode::ScopePruneBlocked    => scope_utils::CODE_SCOPE_PRUNE_BLOCKED,
            ErrorCode::ChunkedTransportUnsupported => scope_utils::CODE_CHUNKED_TRANSPORT_UNSUPPORTED,
            ErrorCode::OversizedTransportUnsupported => scope_utils::CODE_OVERSIZED_TRANSPORT_UNSUPPORTED,
            ErrorCode::CommitPaginationUnsupported => scope_utils::CODE_COMMIT_PAGINATION_UNSUPPORTED,
            ErrorCode::QueryPredicateInvalid => query_utils::CODE_QUERY_PREDICATE_INVALID,
            ErrorCode::EmptyHistory => "empty_history",
            ErrorCode::IncompleteLoad => load_guard_utils::CODE_INCOMPLETE_LOAD,
            ErrorCode::DurabilityTaint => heal_utils::CODE_DURABILITY_TAINT,
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
            ErrorCode::NonOriginLift        => 11,
            ErrorCode::NarrowUnclean        => 12,
            ErrorCode::ScopePruneBlocked    => 13,
            ErrorCode::ChunkedTransportUnsupported => 14,
            ErrorCode::OversizedTransportUnsupported => 15,
            ErrorCode::CommitPaginationUnsupported => 16,
            // 17 stays reserved for the parked conflicts design — never assign it.
            ErrorCode::QueryPredicateInvalid => 18,
            ErrorCode::EmptyHistory => 19,
            ErrorCode::IncompleteLoad => 20,
            ErrorCode::DurabilityTaint => 21,
        }
    }

    /// A one-line, present-tense description of the condition — the docs generator's
    /// "Meaning" column. Plain prose only: no design section numbers or milestone names (this
    /// text ships in generated user-facing docs).
    pub fn description(&self) -> &'static str {
        match self {
            ErrorCode::Generic => "Anything without a more specific classification yet",
            ErrorCode::NotAWarehouse => "The command needs a warehouse; this directory is none",
            ErrorCode::Conflict => "Working state blocks the operation (unresolved / dirty)",
            ErrorCode::Diverged => "A remote ref moved under a lift — lower, retry",
            ErrorCode::WarehouseLocked => "Another forklift process holds the warehouse lock",
            ErrorCode::OutOfScope => "A path argument is outside a scoped (sparse) bay's scope",
            ErrorCode::ScopePathTypeChanged =>
                "A scoped bay's spine path flipped dir\u{2194}file; scope no longer valid",
            ErrorCode::SparseWorkspace =>
                "A whole-tree verb is not supported in a scoped (sparse) bay yet",
            ErrorCode::OutOfScopeConflict =>
                "A scoped bay merge hit an out-of-scope entry changed on both sides",
            ErrorCode::NonOriginLift =>
                "A sparse workspace tried to lift to a remote other than its origin",
            ErrorCode::NarrowUnclean =>
                "\"narrow\" would delete a subtree that still holds uncommitted work",
            ErrorCode::ScopePruneBlocked =>
                "\"scope-prune\" would free a path a checkout still materializes",
            ErrorCode::ChunkedTransportUnsupported =>
                "A chunked large file can't go into a bundle, or is being lifted to a remote \
                 that doesn't support chunking",
            ErrorCode::OversizedTransportUnsupported =>
                "An object predates the size limit and can't be sent to a remote or bundle",
            ErrorCode::CommitPaginationUnsupported =>
                "A lift needs a paginated commit (many objects) and the remote doesn't support \
                 it yet",
            ErrorCode::QueryPredicateInvalid =>
                "A \"query\" predicate is malformed or exceeds a fixed bound",
            ErrorCode::EmptyHistory =>
                "\"history\" was asked to walk a pallet that has nothing stacked on it yet",
            ErrorCode::IncompleteLoad =>
                "\"stack\" or \"park\" refused because a \"load\" that never finished cleanly is \
                 still recorded",
            ErrorCode::DurabilityTaint =>
                "A durability taint could not be auto-healed (a vanished, unreadable, or \
                 corrupt path, or a torn taint record)",
        }
    }

    /// Map a core [`RefusalCode`] to its CLI `ErrorCode`. This is the single, **exhaustive** point
    /// where the core taxonomy meets the head's exit-code table (§7.4/§7.8): the `match` is total
    /// over `RefusalCode`, so adding a refusal code in `forklift-core` without wiring its exit code
    /// here is a *compile error* — which is the entire point of the type. The two enums are kept
    /// distinct because `ErrorCode` also classifies head-only conditions core can never raise
    /// (`NotAWarehouse`, `WarehouseLocked`, …).
    fn from_refusal(code: RefusalCode) -> ErrorCode {
        match code {
            RefusalCode::OutOfScope => ErrorCode::OutOfScope,
            RefusalCode::OutOfScopeConflict => ErrorCode::OutOfScopeConflict,
            RefusalCode::ScopePathTypeChanged => ErrorCode::ScopePathTypeChanged,
            RefusalCode::SparseWorkspace => ErrorCode::SparseWorkspace,
            RefusalCode::NonOriginLift => ErrorCode::NonOriginLift,
            RefusalCode::NarrowUnclean => ErrorCode::NarrowUnclean,
            RefusalCode::ScopePruneBlocked => ErrorCode::ScopePruneBlocked,
            RefusalCode::ChunkedTransportUnsupported => ErrorCode::ChunkedTransportUnsupported,
            RefusalCode::OversizedTransportUnsupported => ErrorCode::OversizedTransportUnsupported,
            RefusalCode::CommitPaginationUnsupported => ErrorCode::CommitPaginationUnsupported,
            RefusalCode::QueryPredicateInvalid => ErrorCode::QueryPredicateInvalid,
            RefusalCode::IncompleteLoad => ErrorCode::IncompleteLoad,
            RefusalCode::DurabilityTaint => ErrorCode::DurabilityTaint,
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

/// Classify a [`CoreError`] into a `ForkliftError` — the head's error boundary. A
/// [`CoreError::Refusal`] carries a typed [`RefusalCode`] that [`ErrorCode::from_refusal`] maps
/// (exhaustively) to the matching stable code, exit code and next step; a [`CoreError::Other`] is a
/// generic failure with no next step. The classification is by the *type*, not by parsing a string.
impl From<CoreError> for ForkliftError {
    fn from(error: CoreError) -> ForkliftError {
        match error {
            CoreError::Refusal { code, message, next_step } => ForkliftError {
                code: ErrorCode::from_refusal(code),
                message,
                next_step: Some(next_step),
            },
            CoreError::Other(message) => {
                ForkliftError { code: ErrorCode::Generic, message, next_step: None }
            }
        }
    }
}

/// The sentinel this crate frames an `empty_history` refusal with — a **head-only**
/// classification (there is no parcel graph to enter, so `forklift-core` never raises it), kept
/// entirely local rather than routed through the `forklift-core` refusal bridge (that would mean
/// adding a `RefusalCode` for something core cannot itself produce). [`empty_history`] builds the
/// frame at the one call site (`history`'s unborn-pallet check); [`From<String>`](ForkliftError)
/// recognizes it before falling back to the `CoreError` path.
const EMPTY_HISTORY_SENTINEL: &str = "\u{1}empty_history\u{1}";

/// Frame a human message as the `empty_history` error (see [`EMPTY_HISTORY_SENTINEL`]) —
/// `history`'s unborn-pallet call site builds its `Err(String)` with this, so it classifies as
/// [`ErrorCode::EmptyHistory`] (exit 19) instead of the generic fallback.
pub fn empty_history(message: impl Into<String>) -> String {
    format!("{EMPTY_HISTORY_SENTINEL}{}", message.into())
}

/// A bare `Err(String)` from a handler is the `?`-friendly default. Most of `forklift-core` still
/// returns `Result<_, String>`, and a refusal that crossed such a still-String segment arrives here
/// as a sentinel-framed string (the [`forklift_core::error`] bridge shim). Lifting it through
/// [`CoreError`] re-types it — a framed refusal becomes its typed [`CoreError::Refusal`] and
/// classifies exactly as one that stayed typed throughout; a plain string becomes a generic error.
/// A frame whose code this build does not recognize (a newer `forklift-core`) degrades to a generic
/// error with the human message — the raw `\u{1f}` frame never leaks into human/JSON output.
///
/// Checked first: this crate's own [`EMPTY_HISTORY_SENTINEL`] frame, a head-only classification
/// the `forklift-core` bridge above knows nothing about.
impl From<String> for ForkliftError {
    fn from(message: String) -> ForkliftError {
        if let Some(human) = message.strip_prefix(EMPTY_HISTORY_SENTINEL) {
            return ForkliftError { code: ErrorCode::EmptyHistory, message: human.to_string(), next_step: None };
        }

        ForkliftError::from(CoreError::from(message))
    }
}

/// Surface a durability taint left standing by best-effort background auto-maintenance
/// (`commands::maintenance::run_if_due`) — after the triggering command's own work has already
/// succeeded and its result envelope (if any) has already printed. This is *not* the triggering
/// command's own result: it must never be folded into `data`, and it must never appear on
/// stdout, or `--json` stdout would stop being exactly one JSON document (see this module's own
/// doc comment and `docs/MACHINE_INTERFACE.md`). But [`human!`] is a no-op under `--json`, so a
/// bare human-only warning would be invisible to exactly the consumer most likely to need it — an
/// agent that just saw the triggering command exit 0 and has no way to know a *later*, unrelated
/// command may exit 21 (`durability_taint`) for a reason it never caused. This codebase has no
/// existing machine-visible side-channel for a non-fatal diagnostic (the closest precedent,
/// `import-git`'s per-command `warnings` field, is baked into that command's own `data` before it
/// ever emits — not available here, since `run_if_due` runs after the triggering command's own
/// `output::emit`/`output::message` call already printed). Stderr, structured the same way in
/// both modes, is the least-bad channel available: never shares stdout, so the invariant above
/// holds, and a `--json` consumer that also captures stderr gets a real JSON object instead of
/// prose to grep.
///
/// `gate_message` is the exact string [`heal_utils`]'s taint machinery
/// (`taint_utils::gate_check`) returned — reused verbatim, never rebuilt, so this warning can
/// never drift from the wording the entry-heal chokepoint's own `durability_taint` refusal uses
/// for the very same taint.
pub fn warn_standing_taint(gate_message: &str) {
    eprintln!("{}", standing_taint_warning_text(mode(), gate_message));
}

/// The pure half of [`warn_standing_taint`]: builds the exact text for a given mode without
/// touching stderr (or anything else) — split out purely so a test can assert on the produced
/// message/code/next_step directly, in both `Human` and `Json` shapes, without capturing the
/// process's real stderr. [`warn_standing_taint`] is the only production caller; its own
/// behavior (what actually reaches stderr for a real run) is unchanged by this split.
fn standing_taint_warning_text(mode: OutputMode, gate_message: &str) -> String {
    match mode {
        OutputMode::Human => format!(
            "Warning: {} Left standing by automatic background maintenance; the command above \
             already completed successfully. {}",
            gate_message, heal_utils::DURABILITY_TAINT_NEXT_STEP
        ),
        OutputMode::Json => {
            let envelope = serde_json::json!({
                "forklift_json": SCHEMA_VERSION,
                "warning": heal_utils::CODE_DURABILITY_TAINT,
                "message": gate_message,
                "next_step": heal_utils::DURABILITY_TAINT_NEXT_STEP,
            });

            serde_json::to_string(&envelope).unwrap_or_else(|_| format!(
                "{{\"forklift_json\":\"{}\",\"warning\":\"{}\"}}",
                SCHEMA_VERSION, heal_utils::CODE_DURABILITY_TAINT
            ))
        }
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
    fn a_typed_refusal_classifies_and_keeps_the_next_step() {
        // A refusal produced typed (a newly-typed command path) classifies straight from the type.
        let error: ForkliftError = scope_utils::out_of_scope_refusal("src/web").into();

        assert_eq!(error.code.as_str(), scope_utils::CODE_OUT_OF_SCOPE);
        assert_eq!(error.code.exit_code(), 7);
        assert!(!error.message.contains('\u{1f}'));
        assert!(error.next_step.is_some());
    }

    #[test]
    fn a_refusal_that_crossed_a_string_segment_classifies_identically() {
        // The bridge shim: a refusal reframed to a String (as a still-String core walker returns
        // it) must classify the same as one that stayed typed — same code, exit and next step, and
        // the raw `\u{1f}` frame never leaks.
        let framed: String = scope_utils::out_of_scope_refusal("src/web").into();
        assert!(framed.contains('\u{1f}'), "the shim string is a sentinel frame");

        let error: ForkliftError = framed.into();
        assert_eq!(error.code.as_str(), scope_utils::CODE_OUT_OF_SCOPE);
        assert_eq!(error.code.exit_code(), 7);
        assert_eq!(error.next_step.as_deref(), Some(
            "Widen the bay's scope to include the path, or run the command in a full workspace."
        ));
        assert!(!error.message.contains('\u{1f}'), "the raw frame must never leak into the message");
    }

    #[test]
    fn an_unrecognized_framed_code_degrades_generically_without_leaking_the_frame() {
        // A frame whose code this build has never heard of — as if a newer `forklift-core`
        // introduced a code this CLI predates. It must not leak the raw frame; it degrades to a
        // generic error carrying the human message and, folded in, the recovery step (no invented
        // code, but no lost guidance either — this is exactly the forward-compat case).
        let frame = format!(
            "{}some_future_code{}a human explanation{}do this",
            forklift_core::error::REFUSAL_PREFIX,
            forklift_core::error::REFUSAL_FIELD_SEPARATOR,
            forklift_core::error::REFUSAL_FIELD_SEPARATOR,
        );

        let error: ForkliftError = frame.into();

        assert_eq!(error.code.as_str(), "error"); // ErrorCode::Generic
        assert_eq!(error.message, "a human explanation do this");
        assert!(!error.message.contains('\u{1f}'), "the raw frame must never leak into the message");
    }

    #[test]
    fn a_plain_string_is_a_generic_error_with_no_next_step() {
        let error: ForkliftError = "something ordinary went wrong".to_string().into();

        assert_eq!(error.code.as_str(), "error");
        assert_eq!(error.message, "something ordinary went wrong");
        assert!(error.next_step.is_none());
    }

    /// The sentinel frame byte (`\u{1f}`) must never reach a user — not in the human message and
    /// not in the next step — for any refusal code, even when the interpolated text (a path off
    /// disk) carries the byte itself. Rendered through the full boundary (`CoreError` → the framed
    /// String shim → `ForkliftError`), the message and next step stay clean.
    #[test]
    fn no_refusal_ever_renders_the_sentinel_frame_byte() {
        for code in [
            RefusalCode::OutOfScope,
            RefusalCode::OutOfScopeConflict,
            RefusalCode::ScopePathTypeChanged,
            RefusalCode::SparseWorkspace,
            RefusalCode::NonOriginLift,
            RefusalCode::NarrowUnclean,
            RefusalCode::ScopePruneBlocked,
            RefusalCode::ChunkedTransportUnsupported,
            RefusalCode::OversizedTransportUnsupported,
            RefusalCode::CommitPaginationUnsupported,
            RefusalCode::IncompleteLoad,
            RefusalCode::DurabilityTaint,
        ] {
            // Hostile interpolated text, carried across a String segment (the shim), then classified.
            let framed: String = CoreError::refusal(code, "a\u{1f}path", "a\u{1f}step").into();
            let error: ForkliftError = framed.into();

            assert!(!error.message.contains('\u{1f}'), "message leaks the frame for {:?}", code);
            assert!(
                !error.next_step.as_deref().unwrap_or_default().contains('\u{1f}'),
                "next_step leaks the frame for {:?}", code,
            );
        }
    }

    /// Every refusal code maps to the exit code the machine-interface contract (§7.8,
    /// `docs/MACHINE_INTERFACE.md`) pins — the table, asserted. This is the compatibility proof for
    /// the taxonomy: a mis-wired `from_refusal` arm or a changed exit number fails here.
    #[test]
    fn every_refusal_code_maps_to_its_contract_exit_code() {
        let table = [
            (RefusalCode::OutOfScope, "out_of_scope", 7),
            (RefusalCode::ScopePathTypeChanged, "scope_path_type_changed", 8),
            (RefusalCode::SparseWorkspace, "sparse_workspace", 9),
            (RefusalCode::OutOfScopeConflict, "out_of_scope_conflict", 10),
            (RefusalCode::NonOriginLift, "non_origin_lift", 11),
            (RefusalCode::NarrowUnclean, "narrow_unclean", 12),
            (RefusalCode::ScopePruneBlocked, "scope_prune_blocked", 13),
            (RefusalCode::ChunkedTransportUnsupported, "chunked_transport_unsupported", 14),
            (RefusalCode::OversizedTransportUnsupported, "oversized_transport_unsupported", 15),
            (RefusalCode::CommitPaginationUnsupported, "commit_pagination_unsupported", 16),
            (RefusalCode::QueryPredicateInvalid, "query_predicate_invalid", 18),
            (RefusalCode::IncompleteLoad, "incomplete_load", 20),
            (RefusalCode::DurabilityTaint, "durability_taint", 21),
        ];

        for (code, code_str, exit) in table {
            let mapped = ErrorCode::from_refusal(code);
            assert_eq!(mapped.as_str(), code_str, "code string for {:?}", code);
            assert_eq!(mapped.exit_code(), exit, "exit code for {:?}", code);
            // The core code string and the head's code string are the same contract.
            assert_eq!(code.as_str(), code_str, "core/head code strings agree for {:?}", code);
        }
    }

    /// The head-only `empty_history` sentinel (never routed through `forklift-core`) classifies
    /// to its own code, exit 19, and carries no next step.
    #[test]
    fn an_empty_history_sentinel_classifies_with_no_next_step() {
        let error: ForkliftError = empty_history("nothing stacked yet").into();

        assert_eq!(error.code.as_str(), "empty_history");
        assert_eq!(error.code.exit_code(), 19);
        assert_eq!(error.message, "nothing stacked yet");
        assert!(error.next_step.is_none());
    }

    /// The standing-taint warning (`commands::maintenance::run_if_due`'s surfacing of a taint
    /// left behind by a failed auto-maintenance `compact`) in both output modes: `Human` prose
    /// names the marker and the `forklift heal` remedy; `Json` carries the same information as a
    /// self-describing object (`forklift_json`/`warning`/`message`/`next_step`) rather than prose
    /// a machine consumer would have to parse. Both must always name the remedy — the whole point
    /// of this warning existing is that a consumer (human or agent) knows what to do next.
    #[test]
    fn the_standing_taint_warning_names_the_code_and_the_heal_remedy_in_both_modes() {
        let gate_message = "a durability taint is standing under \"/warehouse/.forklift\"; \
            existence cannot be trusted here until it is healed.";

        let human = standing_taint_warning_text(OutputMode::Human, gate_message);
        assert!(human.contains(gate_message), "human text: {}", human);
        assert!(human.contains("forklift heal"), "human text: {}", human);

        let json_text = standing_taint_warning_text(OutputMode::Json, gate_message);
        let value: serde_json::Value = serde_json::from_str(&json_text)
            .unwrap_or_else(|e| panic!("json warning is not valid JSON ({}): {}", e, json_text));
        assert_eq!(value["forklift_json"], SCHEMA_VERSION);
        assert_eq!(value["warning"], heal_utils::CODE_DURABILITY_TAINT);
        assert_eq!(value["message"], gate_message);
        assert!(
            value["next_step"].as_str().unwrap().contains("forklift heal"),
            "next_step: {}", value["next_step"]
        );
    }
}
