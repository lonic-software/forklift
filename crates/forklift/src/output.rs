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
//!   [`human!`](crate::human) macro. Progress for a program goes to the `PULT_EVENTS`
//!   channel ([`events`](crate::events)), never to stdout.
//!
//! Nothing else prints to stdout. That is what keeps `--json` output a single valid
//! JSON document.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use serde::Serialize;
use forklift_core::error::{CoreError, RefusalCode};
use forklift_core::util::{heal_utils, load_guard_utils, merge_utils, query_utils, scope_utils, trust_utils};

/// The version of the envelope every Lonic tool shares, carried as `lonic_json`. A consumer
/// detects the contract with this one key; it versions the header (`lonic_json`, `tool`,
/// `schema`, `command`, `ok`, `data`/`error`) and the stderr warning shape, never a
/// command's `data`.
pub const LONIC_JSON: u32 = 1;

/// The producing tool's name, carried as `tool` on every document. Mandatory because a
/// consumer of `pult <command>` cannot otherwise tell Forklift's document from Pult's.
pub const TOOL: &str = "forklift";

/// Forklift's payload schema version, carried as `schema`. It changes only when a command's
/// `data` shape or the error-code registry changes incompatibly, so an agent can pin it and
/// detect drift.
///
/// Version 3 (this one): `lower` and `franchise` report the adopted trust anchor as
/// `adopted_genesis` (present only on first contact) in place of the `adopted_anchor` flag, and
/// the `trust_changed` error code exists, with `details`, as does `remote_protocol_violation`. Version 2: `history` entries carry
/// `parents` (always present, `[]` for a root), the `empty_history` error code exists, and
/// `palletize` list entries carry `head`. A consumer reads this field before relying on a field, to know whether the
/// command it is about to run supports the capability it needs — the version bump *is*
/// the capability-detection mechanism, so it is worth pinning rather than sniffing for a
/// field's presence.
pub const SCHEMA_VERSION: u32 = 3;

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

static COMMAND: OnceLock<Option<String>> = OnceLock::new();

/// Record the command this invocation runs, as clap resolved it (the canonical top-level
/// name, never an alias), for the `command` field of every document the process writes —
/// success and failure alike, so the two can never name it differently. `None` when the
/// command line names no command at all. Called once, from `main`; a second call is ignored.
pub fn set_command(command: Option<&str>) {
    let _ = COMMAND.set(command.map(str::to_string));
}

/// One `--json` stdout document. A struct rather than a `serde_json::Map`, because serde_json
/// here is built without `preserve_order` and its `Map` sorts keys — which would put `command`
/// first. Fields serialize in declaration order, so every document starts with the header a
/// consumer gates on: `lonic_json`, `tool`, `schema`, `command`, `ok`, then exactly one of
/// `data` (success) or `error` (failure). `command` is `null` only when the command line names
/// no known command: a usage error without one, or top-level `--help` / `--version`.
#[derive(Serialize)]
struct Document<'a, T: Serialize> {
    lonic_json: u32,
    tool: &'static str,
    schema: u32,
    command: Option<&'a str>,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<&'a T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorBody<'a>>,
}

/// The `error` object of a failure document, in its documented key order.
#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'static str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_step: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<&'a serde_json::Value>,
}

/// Print one document to stdout. Pretty-printed: the consumer is a program, but a human
/// debugging the pipe reads it too, and JSON parsers do not care about whitespace. The
/// fallback for the serializer failure that cannot happen here (`data` is already a `Value`) is
/// a failure, never an `ok: true` without `data`.
fn print_document<T: Serialize>(data: Option<&T>, error: Option<ErrorBody>) {
    let ok = error.is_none();
    let command = COMMAND.get().and_then(|command| command.as_deref());
    let document = Document {
        lonic_json: LONIC_JSON,
        tool: TOOL,
        schema: SCHEMA_VERSION,
        command,
        ok,
        data,
        error,
    };

    match serde_json::to_string_pretty(&document) {
        Ok(text) => println!("{}", text),
        Err(_) => println!(
            "{{\"lonic_json\":{},\"tool\":\"{}\",\"schema\":{},\"command\":{},\"ok\":false,\"error\":{{\"code\":\"{}\",\"message\":\"The result could not be serialized.\"}}}}",
            LONIC_JSON, TOOL, SCHEMA_VERSION, serde_json::json!(command), ErrorCode::Generic.as_str()
        ),
    }
}

/// A command's successful result: serializable for `--json`, renderable as prose for
/// a person. The `Serialize` shape *is* the public JSON schema — treat a change to it
/// as a schema change (bump [`SCHEMA_VERSION`]).
pub trait CommandOutput: Serialize {
    /// Print the human-readable form to stdout.
    fn render_human(&self);
}

/// Emit a command's structured result in the active mode.
pub fn emit<T: CommandOutput>(value: &T) {
    match mode() {
        OutputMode::Human => value.render_human(),
        OutputMode::Json => print_envelope(value),
    }
}

/// Emit a result whose only content is a human sentence (`{ "message": "…" }` in
/// JSON). For commands that report an outcome rather than data.
pub fn message(text: impl Into<String>) {
    let text = text.into();

    match mode() {
        OutputMode::Human => println!("{}", text),
        OutputMode::Json => print_envelope(&serde_json::json!({ "message": text })),
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

/// Print the success envelope: the header with `ok: true`, then `data`. `ok` says that `data`
/// is the command's result; it does not report the exit status.
fn print_envelope<T: Serialize>(data: &T) {
    match data_value(data) {
        Ok(value) => print_document(Some(&value), None),
        Err(error) => {
            RESULT_UNSERIALIZABLE.store(true, Ordering::Relaxed);
            report_error(&error);
        }
    }
}

/// Set when a command's result could not be serialized: its document was a failure, so `main`
/// must not exit 0 after a command that otherwise returned `Ok`.
static RESULT_UNSERIALIZABLE: AtomicBool = AtomicBool::new(false);

/// Whether this process reported a result it could not serialize (see [`print_envelope`]).
pub fn result_unserializable() -> bool {
    RESULT_UNSERIALIZABLE.load(Ordering::Relaxed)
}

/// A command's result as the document's `data`. Through `serde_json::Value` on purpose: its map
/// sorts keys, so a `HashMap` inside a result serializes in the same order on every run rather
/// than in the process's random hash order. A result that cannot be serialized is a failure,
/// never an `ok: true` document without `data`.
///
/// # Arguments
/// * `data` - The command's result.
///
/// # Returns
/// * `Ok(Value)`          - The result, with every map's keys in sorted order.
/// * `Err(ForkliftError)` - A generic failure, when the result does not serialize.
fn data_value<T: Serialize>(data: &T) -> Result<serde_json::Value, ForkliftError> {
    serde_json::to_value(data).map_err(|error| ForkliftError {
        code: ErrorCode::Generic,
        // The command ran: a caller must not read this failure as "nothing happened" and retry.
        message: format!("The command completed, but its result could not be serialized: {}", error),
        next_step: None,
        details: None,
    })
}

/// Report a failed command in the active mode, to stderr (human) or stdout (JSON —
/// the error is still the command's one result document). Called from `main`, for a
/// command's own failure and for a usage error alike.
pub fn report_error(error: &ForkliftError) {
    match mode() {
        OutputMode::Human => eprintln!("{}", error.message),
        OutputMode::Json => print_document::<()>(None, Some(ErrorBody {
            code: error.code.as_str(),
            message: &error.message,
            next_step: error.next_step.as_deref(),
            details: error.details.as_ref(),
        })),
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

    /// The command line could not be parsed: an unknown command, a missing or invalid
    /// argument. Raised by the argument parser before any command runs.
    Usage,

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

    /// A remote's trust anchor is not the one this warehouse holds or the command named.
    /// `error.details` carries `{subject, current, previous?, what_changed}`.
    TrustChanged,

    /// A remote answered with something the protocol never produces (such as a non-canonical
    /// pallet head). Not a transport failure: retrying gets the same answer.
    RemoteProtocolViolation,
}

impl ErrorCode {
    /// The stable string an agent branches on.
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::Generic              => "error",
            ErrorCode::Usage                => "usage",
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
            ErrorCode::TrustChanged => RefusalCode::TrustChanged.as_str(),
            ErrorCode::RemoteProtocolViolation => RefusalCode::RemoteProtocolViolation.as_str(),
        }
    }

    /// The process exit code — deterministic per class, so a script branches without
    /// parsing prose (§7.8). `0` is success; `2` is a usage error, the meaning every Lonic tool
    /// shares.
    pub fn exit_code(&self) -> i32 {
        match self {
            ErrorCode::Generic              => 1,
            ErrorCode::Usage                => 2,
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
            ErrorCode::TrustChanged => 22,
            ErrorCode::RemoteProtocolViolation => 23,
        }
    }

    /// A one-line, present-tense description of the condition — the docs generator's
    /// "Meaning" column. Plain prose only: no design section numbers or milestone names (this
    /// text ships in generated user-facing docs).
    pub fn description(&self) -> &'static str {
        match self {
            ErrorCode::Generic => "Anything without a more specific classification yet",
            ErrorCode::Usage =>
                "The command line could not be parsed (an unknown command, a missing or invalid \
                 argument)",
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
            ErrorCode::TrustChanged =>
                "A remote's trust anchor is not the one this warehouse holds or the command \
                 named; the details carry its current value",
            ErrorCode::RemoteProtocolViolation =>
                "A remote answered with something the protocol never produces; it is \
                 misbehaving or corrupt, and retrying gets the same answer",
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
            RefusalCode::TrustChanged => ErrorCode::TrustChanged,
            RefusalCode::RemoteProtocolViolation => ErrorCode::RemoteProtocolViolation,
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

    /// Code-specific structured detail, emitted as `error.details` under `--json` when present.
    pub details: Option<serde_json::Value>,
}

impl ForkliftError {
    /// A classified error with a next step.
    pub fn new(code: ErrorCode, message: impl Into<String>, next_step: impl Into<String>) -> ForkliftError {
        ForkliftError {
            code,
            message: message.into(),
            next_step: Some(next_step.into()),
            details: None,
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
            CoreError::Refusal { code, message, next_step, details } => ForkliftError {
                code: ErrorCode::from_refusal(code),
                message,
                next_step: Some(next_step),
                details,
            },
            CoreError::Other(message) => {
                ForkliftError { code: ErrorCode::Generic, message, next_step: None, details: None }
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
            return ForkliftError { code: ErrorCode::EmptyHistory, message: human.to_string(), next_step: None, details: None };
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
        OutputMode::Json => warning_envelope(
            heal_utils::CODE_DURABILITY_TAINT,
            gate_message,
            heal_utils::DURABILITY_TAINT_NEXT_STEP,
            None,
        ),
    }
}

/// The `--json` warning code for background maintenance that could not even decide whether it
/// was due. Its commonest cause is a configuration file that does not parse, but it is **not**
/// a config-only code: `auto_compaction_action` also fails on the object-store scans it makes to
/// answer the question (`estimate_loose_count`, `count_pack_files`), so a permission error, an
/// exhausted descriptor table or a dead mount under `.forklift/objects` reaches it too. Treat it
/// as "the decision could not be made", and read `message` for which.
pub const CODE_MAINTENANCE_UNAVAILABLE: &str = "maintenance_unavailable";

/// What to do about [`CODE_MAINTENANCE_UNAVAILABLE`]. Deliberately does not promise the cause is
/// permanent: a broken configuration file is, a transient I/O fault is not, and this code covers
/// both.
///
/// It does have to name an **action**, which is the part that was missing: `next_step` is one of
/// the fields every stderr warning carries (`docs/MACHINE_INTERFACE.md`), and a consumer
/// that surfaces it to a human had nothing to surface — only a reassurance about the current
/// state, while the thing to actually do sat in the reported cause. Pointing at that cause is
/// the most this field can honestly say, because which fault it is only the cause knows.
///
/// It is worded to read correctly in **both** modes, because the `Human` arm prints it too. An
/// earlier draft said "resolve what the message names", which is a field a `--json` consumer can
/// look up and a human reader has never heard of.
pub const MAINTENANCE_UNAVAILABLE_NEXT_STEP: &str =
    "Resolve the reported cause; packing resumes on the next mutating command. The command's \
     own work is unaffected.";

/// Surface background auto-maintenance that could not run at all
/// (`commands::maintenance::run_if_due`), on the same stderr side-channel — and for the same
/// reasons — as [`warn_standing_taint`].
///
/// Auto-maintenance is best-effort: a *failed* compaction stays quiet unless it left a taint
/// standing. This is the different case — maintenance never got as far as deciding whether it
/// was **due**, so nothing was attempted and nothing can report on it.
///
/// The reason that deserves a warning where a failed compaction does not: this outcome is
/// invisible in every other surface. The store simply stops being packed, on every command,
/// with nothing said on any of them. When the cause is a configuration file that does not parse
/// it also does not clear on its own; when it is an I/O fault under `.forklift/objects` it may.
/// Either way the message names what actually failed, which is the part a reader can act on.
pub fn warn_maintenance_unavailable(error: &str) {
    eprintln!("{}", maintenance_unavailable_warning_text(mode(), error));
}

/// The pure half of [`warn_maintenance_unavailable`], split out for the same reason
/// [`standing_taint_warning_text`] is:
/// `the_maintenance_unavailable_warning_carries_the_cause_in_both_modes` asserts the produced
/// text in both modes without capturing the process's real stderr. The end-to-end `--json` shape
/// is covered separately by `cli.rs`; the `Human` arm is covered only here, which is the gap
/// that test closes.
fn maintenance_unavailable_warning_text(mode: OutputMode, error: &str) -> String {
    match mode {
        OutputMode::Human => format!(
            "Warning: background object-store maintenance was skipped. {} {}",
            error, MAINTENANCE_UNAVAILABLE_NEXT_STEP
        ),
        OutputMode::Json => warning_envelope(
            CODE_MAINTENANCE_UNAVAILABLE,
            error,
            MAINTENANCE_UNAVAILABLE_NEXT_STEP,
            None,
        ),
    }
}

/// Announce a trust anchor adopted on first contact (`lower`, `franchise`). Adoption is never
/// silent: this names the adopted value, so a reader can check it out of band, and under `--json`
/// it is a warning document on stderr whose `details` carry the value — the command's own result
/// stays the one document on stdout.
pub fn warn_trust_adopted(details: &trust_utils::TrustDetails) {
    eprintln!("{}", trust_adopted_warning_text(mode(), details));
}

/// The pure half of [`warn_trust_adopted`], split out like [`standing_taint_warning_text`] so a
/// test can assert both modes without capturing stderr.
fn trust_adopted_warning_text(mode: OutputMode, details: &trust_utils::TrustDetails) -> String {
    // Both values come from the remote (or its URL): stripped of control characters, so a
    // hostile genesis cannot rewrite the terminal line a human is asked to verify.
    let genesis = forklift_core::error::sanitize(details.current.as_deref().unwrap_or_default());
    let subject = forklift_core::error::sanitize(&details.subject);
    let message = format!(
        "FIRST CONTACT with {}: adopted its trust anchor, genesis {}.",
        subject, genesis
    );
    let next_step = format!(
        "Verify that genesis out of band. To check it before anything is fetched, pass \
         --expect {} to franchise or lower.",
        genesis
    );

    match mode {
        OutputMode::Human => format!("Warning: {} {}", message, next_step),
        OutputMode::Json => warning_envelope(
            trust_utils::WARNING_TRUST_ADOPTED, &message, &next_step, Some(&details.to_value()),
        ),
    }
}

/// The one `--json` warning envelope, shared by every stderr warning so the shape a consumer
/// parses (`lonic_json`, `tool`, `warning`, `message`, `next_step`, and `details` for the codes
/// that define one) is defined once — including the hand-built fallback for the serializer
/// failure that cannot actually happen here.
fn warning_envelope(code: &str, message: &str, next_step: &str, details: Option<&serde_json::Value>) -> String {
    // A struct, not `json!`: serde_json's `Map` sorts keys, and the header leads.
    #[derive(Serialize)]
    struct Warning<'a> {
        lonic_json: u32,
        tool: &'static str,
        warning: &'a str,
        message: &'a str,
        next_step: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        details: Option<&'a serde_json::Value>,
    }

    let envelope = Warning { lonic_json: LONIC_JSON, tool: TOOL, warning: code, message, next_step, details };

    serde_json::to_string(&envelope).unwrap_or_else(|_| format!(
        "{{\"lonic_json\":{},\"tool\":\"{}\",\"warning\":\"{}\"}}",
        LONIC_JSON, TOOL, code
    ))
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
    fn a_result_serializes_in_key_order_or_fails_as_a_generic_error() {
        // A HashMap's own order is random per process; `data` must not inherit it.
        let map: std::collections::HashMap<String, u32> =
            (0..64).map(|i| (format!("k{:02}", i), i)).collect();
        let value = data_value(&map).ok().expect("a string-keyed map serializes");
        let text = serde_json::to_string(&value).unwrap();
        let keys: Vec<&str> = text.match_indices("\"k").map(|(at, _)| &text[at + 1..at + 4]).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys.len(), 64);
        assert_eq!(keys, sorted);

        // A map whose keys are not strings cannot be a JSON object: a failure, not `ok: true`.
        let unserializable: std::collections::HashMap<(u8, u8), u8> = [((1, 2), 3)].into_iter().collect();
        let error = data_value(&unserializable).err().expect("a non-string key cannot serialize");
        assert_eq!(error.code.as_str(), "error");
        assert_eq!(error.code.exit_code(), 1);
    }

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
            (RefusalCode::TrustChanged, "trust_changed", 22),
            (RefusalCode::RemoteProtocolViolation, "remote_protocol_violation", 23),
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
    /// self-describing object (`lonic_json`/`tool`/`warning`/`message`/`next_step`) rather than prose
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
        assert_eq!(value["lonic_json"], 1);
        assert_eq!(value["tool"], "forklift");
        assert_eq!(value["warning"], heal_utils::CODE_DURABILITY_TAINT);
        assert_eq!(value["message"], gate_message);
        assert!(
            value["next_step"].as_str().unwrap().contains("forklift heal"),
            "next_step: {}", value["next_step"]
        );
    }

    /// The sibling of the test above, for the second warning on the same channel. The `Human` arm
    /// has no other coverage anywhere — `cli.rs` exercises `--json` end-to-end — so without this
    /// a change that dropped the cause from the human line would ship green.
    #[test]
    fn the_maintenance_unavailable_warning_carries_the_cause_in_both_modes() {
        let error = "Configuration file \"/warehouse/.forklift/config/warehouse.toml\" is not \
            valid: \"maintenance.loose\" must be a string, not integer.";

        let human = maintenance_unavailable_warning_text(OutputMode::Human, error);
        assert!(human.contains(error), "the human line must carry the cause: {}", human);
        assert!(human.contains(MAINTENANCE_UNAVAILABLE_NEXT_STEP), "human text: {}", human);

        // The two assertions above compare the text against its own
        // constant, so they hold for *any* wording — including one that reads as nonsense to the
        // reader this arm exists for. One mechanical part of "it reads correctly in both modes"
        // can be pinned: the shared text is printed verbatim to a human, so it may not direct
        // them to a `--json` envelope field they have no way to see. An earlier wording said
        // "resolve what the message names"; `message` is a field name, not anything on screen.
        //
        let json_text = maintenance_unavailable_warning_text(OutputMode::Json, error);
        let value: serde_json::Value = serde_json::from_str(&json_text)
            .unwrap_or_else(|e| panic!("json warning is not valid JSON ({}): {}", e, json_text));
        assert_eq!(value["lonic_json"], 1);
        assert_eq!(value["tool"], "forklift");
        assert_eq!(value["warning"], CODE_MAINTENANCE_UNAVAILABLE);
        assert_eq!(value["message"], error);
        assert_eq!(value["next_step"], MAINTENANCE_UNAVAILABLE_NEXT_STEP);

        // The rule is "no field a human cannot see on screen", matched ignoring case and with
        // `_` also read as a space ("next step"). The fields are taken from the envelope this
        // warning actually emits, not listed by hand, so a field added later is covered without
        // anyone remembering to add it here. `message` is covered although it is an ordinary
        // word, because the human line never labels its message — "Message" at the start of a
        // sentence is the same mistake as "message". `warning` alone is exempt: the human line
        // starts "Warning:", so "this warning repeats" points at something visible.
        let lowered = MAINTENANCE_UNAVAILABLE_NEXT_STEP.to_lowercase();
        let fields: Vec<&String> = value.as_object().unwrap().keys()
            .filter(|field| field.as_str() != "warning")
            .collect();
        assert!(fields.len() >= 3, "the envelope's fields must be in scope: {fields:?}");

        for field in fields {
            for spelling in [field.clone(), field.replace('_', " ")] {
                assert!(
                    !lowered.contains(&spelling),
                    "the next step is printed to humans too, so it must not name the envelope \
                     field {spelling:?}: {}",
                    MAINTENANCE_UNAVAILABLE_NEXT_STEP
                );
            }
        }

        // `docs/MACHINE_INTERFACE.md` promises a consumer exactly these five fields on every
        // stderr warning. Asserted against a literal list, not against the other warning's key
        // set — both are built by `warning_envelope`, so comparing them to each other would be
        // true however that helper changed, and would pin nothing at all.
        let expected = ["lonic_json", "message", "next_step", "tool", "warning"];

        for text in [
            json_text,
            standing_taint_warning_text(OutputMode::Json, "a taint is standing."),
        ] {
            let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
            let mut fields: Vec<&str> = parsed.as_object().unwrap().keys().map(String::as_str).collect();
            fields.sort();
            assert_eq!(fields, expected, "documented warning shape changed: {text}");
        }
    }

    #[test]
    fn the_trust_adopted_warning_names_the_genesis_in_both_modes() {
        let details = trust_utils::first_contact("http://remote", "abc123");

        let human = trust_adopted_warning_text(OutputMode::Human, &details);
        assert!(human.starts_with("Warning: FIRST CONTACT with http://remote"), "{}", human);
        assert!(human.contains("--expect abc123"), "the pin must be copyable: {}", human);

        let hostile = trust_utils::first_contact("http://remote", "abc\u{1b}]0;spoof\u{7}\ndef");
        let human = trust_adopted_warning_text(OutputMode::Human, &hostile);
        assert!(!human.chars().any(char::is_control), "a remote value reached the terminal raw: {:?}", human);

        // The five fields every warning carries, plus `details` for this code — a literal, so a
        // renamed key goes red.
        let json = trust_adopted_warning_text(OutputMode::Json, &details);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let mut fields: Vec<&str> = parsed.as_object().unwrap().keys().map(String::as_str).collect();
        fields.sort();
        assert_eq!(fields, ["details", "lonic_json", "message", "next_step", "tool", "warning"], "{json}");
        assert_eq!(parsed["warning"], "trust_adopted");
        assert_eq!(parsed["details"], serde_json::json!({
            "subject": "http://remote", "current": "abc123", "what_changed": "first_contact",
        }));
    }
}
