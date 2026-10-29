//! The core error type (DESIGN.html §7.4 error taxonomy) and its stable refusal codes.
//!
//! `forklift-core` never prints and cannot depend on a head's presentation types, yet some of
//! its failures are **classified refusals** an agent must be able to branch on — a path outside a
//! scoped bay, a chunked file that cannot ride a bundle, a lift to the wrong remote. [`CoreError`]
//! is where that classification lives: a [`CoreError::Refusal`] carries a typed [`RefusalCode`]
//! (one of the stable codes below) plus the human message and the machine-actionable next step; a
//! [`CoreError::Other`] is everything not yet classified — a plain error with no code.
//!
//! ## The migration frontier
//!
//! Historically a refusal travelled from core to the CLI as a **sentinel-framed string** (a
//! `\u{1f}`-delimited frame the CLI decoded at its error boundary), because there was no shared
//! error type. [`CoreError`] is that type. The refusal *constructors* (in
//! [`scope_utils`](crate::util::scope_utils) and [`bundle_utils`](crate::util::bundle_utils)) now
//! return a typed `CoreError::Refusal` — the sentinel string is no longer the source of truth.
//!
//! It is not gone from every path, though. Migrating a function to `Result<_, CoreError>` is
//! cheap where it *produces* a refusal directly (the command entry points, the remote/bundle
//! guards); it is expensive across the deep tree/merge/shift walkers, which return
//! `Result<_, String>` and would each need every one of their many `Err` sites rewritten. Those
//! deep walkers stay String-typed for now. To let a typed refusal cross such a still-String
//! segment losslessly, the sentinel framing survives as a **bridge shim**, invoked only by the
//! two `From` conversions here:
//!
//! * [`From<CoreError> for String`](#impl-From<CoreError>-for-String) re-frames a `Refusal` into
//!   the sentinel encoding when a typed error is `?`-propagated (or `.into()`-converted) into a
//!   `Result<_, String>` caller, and
//! * [`From<String> for CoreError`](#impl-From<String>-for-CoreError) decodes that frame back into
//!   a typed `Refusal` when the String re-enters a `CoreError`-typed caller (and reads any plain
//!   string as `Other`).
//!
//! Because both directions round-trip, `?` works seamlessly across the frontier and the code is
//! never lost — a refusal born typed, framed to cross a String walker, and re-lifted at the
//! command boundary classifies exactly as one that stayed typed throughout. On the fully-typed
//! paths (a refusal produced and consumed without ever touching a `Result<_, String>` segment) the
//! string encoding is never invoked at all. The head decodes the *type*, not the string:
//! `forklift`'s `From<CoreError> for ForkliftError` matches [`RefusalCode`] exhaustively, so adding
//! a code without wiring its exit code is a compile error.

use std::fmt;

/// The framing that marks a refusal string so a String-typed segment can carry it losslessly and
/// the boundary can classify it without parsing prose. `\u{1f}` (ASCII Unit Separator) is not
/// inherently absent from a message or a warehouse path — it is kept out: [`CoreError::refusal`]
/// strips control characters (including `\u{1f}`) from the message and next step at construction,
/// and `WarehousePath::from_user_input` rejects control characters in a user-supplied path at
/// input validation. That sanitization is what keeps the framing unambiguous; a plain error that
/// still happens to contain a stray `\u{1f}` (built by hand, bypassing those guards) simply fails
/// to `strip_prefix` and degrades to [`CoreError::Other`] rather than misparsing. This is the
/// bridge-shim encoding described in the module docs — not a wire format and not something a head
/// renders.
pub const REFUSAL_PREFIX: &str = "\u{1f}scope\u{1f}";

/// The field separator inside a [`REFUSAL_PREFIX`] frame (also `\u{1f}`).
pub const REFUSAL_FIELD_SEPARATOR: char = '\u{1f}';

/// Declare `RefusalCode` once: each arm's doc comment, variant name and stable code string, in a
/// single list that expands into the enum, `as_str`, **and** [`ALL_CODES`] together.
///
/// This is the mechanism that makes the taxonomy's completeness a compile-time or deterministic-
/// test property rather than a hopeful comment. Before this macro, the enum, its `as_str` match
/// and a hand-written `ALL_CODES` array were three separate lists a contributor had to update in
/// lockstep by discipline alone; `as_str`'s match was compiler-enforced exhaustive (the compiler
/// catches a forgotten `as_str` arm), but nothing forced `ALL_CODES` to keep up — a variant added
/// to the enum and to `as_str` but left out of `ALL_CODES` compiled cleanly, and
/// `RefusalCode::from_code` (which searches `ALL_CODES`) would then silently never recognize that
/// code's string. Worse, the round-trip test iterated `ALL_CODES` too, so it passed — vacuously,
/// proving nothing about the variant it never saw. The forgotten variant's code would still
/// *display* correctly locally, but the moment it crossed a still-`String` migration-frontier
/// segment (see the module docs), `CoreError::from(String)` would fail to recognize the reframed
/// code and silently degrade it to `Other` — exactly the classification loss this taxonomy exists
/// to prevent, and exactly the kind of regression a green test suite would not catch.
///
/// With the enum, `as_str` and `ALL_CODES` generated from one macro invocation, there is no second
/// list to forget: a variant that does not appear in the invocation below does not exist at all
/// (not in the enum, so every other `match` on `RefusalCode` still forces it in), and a variant
/// that does appear is in `ALL_CODES` by construction. The set is still **add-only**: introduce a
/// variant (i.e. a new line below), never repurpose one.
macro_rules! refusal_codes {
    (
        $(
            $(#[$doc:meta])*
            $variant:ident => $code:literal
        ),+ $(,)?
    ) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub enum RefusalCode {
            $(
                $(#[$doc])*
                $variant,
            )+
        }

        impl RefusalCode {
            /// The stable code string an agent branches on. `const` so callers can pin it in a
            /// `const` (the `scope_utils::CODE_*` re-exports do).
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(RefusalCode::$variant => $code,)+
                }
            }
        }

        /// Every [`RefusalCode`] variant, generated by the same `refusal_codes!` invocation that
        /// defines the enum and `as_str` — see that macro's doc comment for why this, and not a
        /// hand-maintained array, is what makes [`RefusalCode::from_code`] and the round-trip test
        /// exhaustive in fact rather than in intent.
        const ALL_CODES: &[RefusalCode] = &[$(RefusalCode::$variant),+];
    };
}

refusal_codes! {
    /// A path argument is outside the bay's materialization scope (§7.6).
    OutOfScope => "out_of_scope",

    /// A merge in a scoped bay hit an out-of-scope entry that changed on both sides: there is no
    /// content to reconcile, so it refuses rather than guess.
    OutOfScopeConflict => "out_of_scope_conflict",

    /// A scoped bay's spine path flipped between a directory and a file (§7.6): the scope is no
    /// longer valid there and the operation refuses rather than guess.
    ScopePathTypeChanged => "scope_path_type_changed",

    /// A whole-tree verb is not (yet) supported in a scoped (sparse) bay (§7.6).
    SparseWorkspace => "sparse_workspace",

    /// A lift from a partial warehouse (franchised with `--only` or `--cache`) was aimed at a
    /// remote other than its origin: the closure it does not hold was only ever proved present on
    /// the origin, so it refuses up front.
    NonOriginLift => "non_origin_lift",

    /// `narrow` was asked to drop a subtree that still holds uncommitted work (staged, unstaged or
    /// untracked): it refuses rather than silently discard it.
    NarrowUnclean => "narrow_unclean",

    /// `scope-prune` was asked to free a path a checkout still materializes: freeing it would
    /// break that checkout, so it refuses until the checkout narrows the path away.
    ScopePruneBlocked => "scope_prune_blocked",

    /// A large chunked file was asked to go into a bundle (chunks are structurally excluded from
    /// every bundle), or a lift was asked to send one to a remote whose handshake did not
    /// advertise chunking support. Either case refuses client-side rather than ship content that
    /// can never be materialized where it lands.
    ChunkedTransportUnsupported => "chunked_transport_unsupported",

    /// A bundle or lift was asked to send an object above the whole-object ceiling: a
    /// grandfathered giant stays readable and checkout-able locally forever, but no migration
    /// preserves its signed identity, so nothing accepts it in transport.
    OversizedTransportUnsupported => "oversized_transport_unsupported",

    /// A lift's commit would need more than one paginated batch, and the remote does not advertise
    /// support for the pagination — refused right after negotiation, before a byte is uploaded.
    CommitPaginationUnsupported => "commit_pagination_unsupported",

    /// A `query` predicate is not acceptable as written: malformed (unparsable JSON, an unknown
    /// field or operator, a value of the wrong type), or past one of the fixed bounds (payload
    /// size, nesting depth, leaf count, `in`-array length, glob length). One code for all of them:
    /// every case is "the query you asked for is not acceptable as written."
    QueryPredicateInvalid => "query_predicate_invalid",

    /// `stack` or `park` was asked to durably commit the staged inventory while a `load` that
    /// started but never finished cleanly (a mid-walk crash, or an error return partway through)
    /// is still recorded (`load_guard_utils`): the staged content may be missing whatever that
    /// load never got to. Refuses rather than silently publish an incomplete parcel; the remedy
    /// is always the same cheap re-load, or abandoning the load with `restore --staged`. Also
    /// every command that changes the bay or commits staged content, while an operation that
    /// rewrites the whole staging area was interrupted (`operation_utils`); there the remedy is
    /// `forklift heal`, which finishes or undoes it.
    IncompleteLoad => "incomplete_load",

    /// The storage-scope entry-heal (`heal_utils`) found a durability taint it could not resolve
    /// automatically: a write's final path(s) were left visible after a failed durability sync
    /// (or a later re-check found one already standing) and, on this attempt, one or more of them
    /// vanished, failed to read back, no longer matched their own content hash, or the taint's own
    /// record was itself incomplete (torn). The taint is left standing in every case.
    DurabilityTaint => "durability_taint",

    /// A remote's trust anchor is not the one this warehouse holds, or not the one the command
    /// named (an `--expect` pin, an `accept-regenesis --confirm` value). Never adopted silently:
    /// the refusal's details carry the remote's value now, so an override can name exactly it
    /// (`trust_utils`).
    TrustChanged => "trust_changed",

    /// A remote answered with something the protocol never produces, such as a pallet head that
    /// is not the canonical spelling of an object hash. A misbehaving or corrupt remote, not a
    /// transport failure: retrying gets the same answer.
    RemoteProtocolViolation => "remote_protocol_violation",

    /// A meta pallet changed differently on both sides at the same path since the two last
    /// agreed. Nothing is lifted for that pallet; the refusal's details name each path with both
    /// values and their signers, and `lower --keep` resolves them (`meta_tree`).
    MetaConflict => "meta_conflict",

    /// `stack` or `park` found a staged file whose content the object store no longer holds (for a
    /// chunked file, its recipe or any chunk) — lost after the `load`, to a crash or a manual
    /// deletion. A parcel naming it would pass `audit` and fail only at a later `restore`, so no
    /// parcel is made. The remedy is to unstage the path and load it again from its working copy.
    StagedContentMissing => "staged_content_missing",

    /// This bay stands on a parcel its pallet has since moved past: another bay or process
    /// stacked, lowered or merged onto the pallet, before this command or while it ran. Nothing
    /// was moved. The next step catches the bay up (`shift <pallet>`), or, with a consolidation or
    /// cherry-pick in progress, names how to abort it first.
    Behind => "behind",

    /// `deliver` was aimed at a target holding changes the draft has not seen; delivering would
    /// undo them. The next step consolidates the target into the draft.
    TargetNotCovered => "target_not_covered",

    /// The remote's pallet holds parcels this warehouse has not lowered yet: found before a lift,
    /// or the remote's compare-and-set refused one because the pallet moved while it ran. The next
    /// step is `lower`, which fast-forwards, or reports `Diverged` when both sides moved.
    RemoteAhead => "remote_ahead",

    /// The local pallet and the remote's both hold parcels the other lacks. Nothing was lifted or
    /// lowered; the next step names how to merge the two.
    Diverged => "diverged",

    /// A command named a pallet this warehouse holds no ref for — or, for `lower`, one the remote
    /// does not have. The next step is `lower <name>` when a remote is configured (the remote may
    /// have it), else `palletize <name>`; for `lower <name>`, `palletize --remote`, and for a bare
    /// `lower`, `lift`.
    PalletNotFound => "pallet_not_found",

    /// The directory holds a franchise that has not finished: an attempt failed, was killed, or
    /// is still running. Every command but `franchise` refuses to enter it; running the same
    /// `franchise` again resumes it, keeping what was fetched (`franchise_utils`). The refusal's
    /// details carry `{directory, remote, cause_code, retry}`.
    FranchiseIncomplete => "franchise_incomplete",

    /// A sparse franchise's `--only` prefix names no directory in the pallet's head on the remote.
    /// Retrying gets the same answer: the prefix (or the pallet) has to change.
    OnlyPrefixNotFound => "only_prefix_not_found",

    /// A lift's commit named a blob the remote holds neither in its staging area nor in the
    /// warehouse: the upload was discarded (the remote rejected it as corrupt or too large) or never
    /// arrived. Retrying the commit gets the same answer; running the lift again uploads the blob
    /// afresh.
    StagedUploadDiscarded => "staged_upload_discarded",

    /// A `forklift mirror` run left at least one mirrored pallet out of step with git: refused,
    /// failed or busy. The others went through. The refusal's details carry the run's report
    /// (`{url, pallets}`), each pallet's message naming its own step.
    MirrorIncomplete => "mirror_incomplete",

    /// History a remote served does not verify against the trust anchor this warehouse holds (or,
    /// at first contact, the one the remote serves): a parcel is unsigned or signed by an unknown
    /// key after the trust boundary, signed by a revoked key outside its distrust boundary, or sits
    /// behind a boundary neither this warehouse nor the remote can resolve. Nothing was adopted.
    HistoryUnverified => "history_unverified",

    /// A directory a command was to use as a warehouse is not one: its `.forklift` folder holds no
    /// warehouse marker, it is a bay's, it is missing, or a bay's redirect names something that is
    /// not a warehouse. Refused before any lock is taken; the next step names the root it refused.
    NotAWarehouse => "not_a_warehouse",

    /// The remote has the pallet only as a tombstone: it was deleted there, and the remote keeps
    /// the name reserved for the history its tombstone holds. A lift re-creates it only by
    /// acknowledging the deletion (`lift --recreate`) from work that descends from that head; the
    /// next step otherwise brings the pallet back there (`palletize --restore <name> --remote`).
    PalletDeletedOnRemote => "pallet_deleted_on_remote",

    /// A cache warehouse (franchised with `--cache`) does not hold content the command reads, and
    /// could not fetch it because its origin could not be reached. The next step names the origin
    /// to wait for.
    CacheMiss => "cache_miss",
}

impl RefusalCode {
    /// Parse a stable code string back into a [`RefusalCode`]. `None` for any string that is not a
    /// known code — an unknown code from a newer peer (across the wire or a version boundary)
    /// degrades to an unclassified error rather than being invented into a variant this build has
    /// no exit code for. (Returns `Option`, not the `FromStr` trait's `Result`, so it is a named
    /// method — a bad code is expected, not an error to propagate.)
    pub fn from_code(code: &str) -> Option<RefusalCode> {
        ALL_CODES.iter().copied().find(|candidate| candidate.as_str() == code)
    }

    /// Every variant, generated with the enum (see [`refusal_codes!`]): what a head's own
    /// exhaustiveness tests iterate, so a new code cannot slip past them.
    pub fn all() -> &'static [RefusalCode] {
        ALL_CODES
    }
}

/// A `forklift-core` failure: either a classified [`RefusalCode`] refusal (with the human message
/// and a machine-actionable next step) or an unclassified [`CoreError::Other`]. See the module
/// docs for how it travels the migration frontier.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CoreError {
    /// A stable, machine-branchable refusal.
    Refusal {
        /// The stable classification.
        code: RefusalCode,
        /// The human explanation. Rendered verbatim by [`fmt::Display`]; [sanitized](sanitize) at
        /// construction so it stays frame-safe (every control character but tab becomes a space).
        message: String,
        /// The machine-actionable recovery step.
        next_step: String,
        /// Code-specific structured detail, for the codes whose contract defines one (a head emits
        /// it as `error.details` under `--json`).
        details: Option<serde_json::Value>,
    },

    /// An error without a more specific classification yet — a plain message, no code.
    Other(String),
}

impl CoreError {
    /// Build a classified refusal. [Sanitizes](sanitize) every control character but tab (including
    /// `\u{1f}`, the frame separator) out of the message and next step so the value is safe to render *and* safe to
    /// carry through the bridge shim regardless of where the interpolated text came from — a path
    /// read back off disk (a tree or inventory entry) can carry a control character that
    /// `WarehousePath::from_user_input`'s guard never saw.
    pub fn refusal(
        code: RefusalCode,
        message: impl Into<String>,
        next_step: impl Into<String>,
    ) -> CoreError {
        CoreError::Refusal {
            code,
            message: sanitize(&message.into()),
            next_step: sanitize(&next_step.into()),
            details: None,
        }
    }

    /// Build a classified refusal that carries structured detail as well — see
    /// [`CoreError::refusal`] for the sanitizing of the two texts. The detail needs none: the frame
    /// carries it as compact JSON, which escapes every control character.
    pub fn refusal_with_details(
        code: RefusalCode,
        message: impl Into<String>,
        next_step: impl Into<String>,
        details: serde_json::Value,
    ) -> CoreError {
        CoreError::Refusal {
            code,
            message: sanitize(&message.into()),
            next_step: sanitize(&next_step.into()),
            details: Some(details),
        }
    }
}

impl fmt::Display for CoreError {
    /// Renders the human message only — a `Refusal` shows its `message` (no code, no next step, no
    /// frame), an `Other` shows its string. This is byte-for-byte what a head prints for a failure.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoreError::Refusal { message, .. } => f.write_str(message),
            CoreError::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for CoreError {}

/// A plain string is an unclassified error — unless it is a sentinel-framed refusal that a
/// still-String segment carried across the migration frontier, in which case it is decoded back
/// into the typed `Refusal` it was framed from (see the module docs). A frame whose code this
/// build does not recognize (a newer peer's code) keeps its human message and recovery step but
/// loses the machine code — degrading to `Other` rather than leaking the raw `\u{1f}` frame or
/// inventing a variant. The next step is folded into the message (rather than dropped) because
/// this is exactly the forward-compat case — a newer peer's refusal — where recovery guidance
/// matters most and there is no typed field left to carry it in. A frame whose details do not
/// parse degrades the same way: a refusal is never decoded without the details its code promises.
impl From<String> for CoreError {
    fn from(message: String) -> CoreError {
        if let Some((code, human, next_step, details)) = deframe_with_details(&message) {
            return match (RefusalCode::from_code(code), details) {
                (Some(code), Ok(details)) => CoreError::Refusal {
                    code,
                    message: human.to_string(),
                    next_step: next_step.to_string(),
                    details,
                },
                _ => CoreError::Other(with_next_step(human, next_step)),
            };
        }

        // A frame a caller wrapped (`format!("Could not …: {}", e)`) claims no code: text from a
        // remote or off disk can hold a frame too, and only a frame that starts the string is
        // known to be this process's. Each one is rendered readable, its next step folded in.
        if let Some(rendered) = render_wrapped_frames(&message) {
            return CoreError::Other(rendered);
        }

        CoreError::Other(message)
    }
}

impl From<&str> for CoreError {
    fn from(message: &str) -> CoreError {
        CoreError::from(message.to_string())
    }
}

/// Lower a `CoreError` to a `String` so a typed refusal can cross a `Result<_, String>` segment
/// (the migration frontier) without losing its code: a `Refusal` re-frames into the sentinel
/// encoding, an `Other` is its plain message. A later [`From<String>`](CoreError) re-lifts the
/// frame. This is the bridge shim — a fully-typed path never invokes it.
impl From<CoreError> for String {
    fn from(error: CoreError) -> String {
        match error {
            CoreError::Refusal { code, message, next_step, details } => {
                frame(code.as_str(), &message, &next_step, details.as_ref())
            }
            CoreError::Other(message) => message,
        }
    }
}

/// Append `extra` prose to an error's human-visible message, without corrupting a framed refusal
/// the way a naive `format!("{} {}", error, extra)` would: [`deframe`] takes the frame's *third*
/// field to end-of-string, so text appended to the raw string lands in `next_step`, not the
/// message — and a head's human-mode renderer prints `message` only (`next_step` is JSON-only), so
/// the appended note would be silently invisible to a human operator. This re-lifts `error`
/// through [`CoreError::from`] first, appends to whichever field is actually the rendered message,
/// then lowers back to `String` (reframing a `Refusal`) — built entirely from the existing public
/// `CoreError` API, no new framing primitive. A plain (unframed) `error` round-trips as
/// `CoreError::Other` and is handled the same way, so this is safe to call unconditionally on any
/// `Result<_, String>` error.
pub fn append_to_message(error: String, extra: &str) -> String {
    match CoreError::from(error) {
        CoreError::Refusal { code, message, next_step, details } => CoreError::Refusal {
            code,
            message: sanitize(&format!("{} {}", message, extra)),
            next_step,
            details,
        }.into(),
        CoreError::Other(message) => format!("{} {}", message, extra),
    }
}

/// Prefix an error's human-visible message with `context`, keeping a framed refusal's code, next
/// step and details: the counterpart of [`append_to_message`] for the "Could not …: why" shape.
pub fn prefix_message(context: &str, error: String) -> String {
    match CoreError::from(error) {
        CoreError::Refusal { code, message, next_step, details } => CoreError::Refusal {
            code,
            message: sanitize(&format!("{}: {}", context, message)),
            next_step,
            details,
        }.into(),
        CoreError::Other(message) => format!("{}: {}", context, message),
    }
}

/// `message` with every frame inside it rendered as its message and next step, or `None` when it
/// holds none. A frame runs to the next frame's prefix; what the caller wrote around it keeps its
/// lines (a frame's own fields were sanitized when it was built). A prefix whose code is not a
/// code word is not a frame, and is left as it is.
fn render_wrapped_frames(message: &str) -> Option<String> {
    let mut rendered = String::new();
    let mut any = false;
    let mut rest = message;

    while let Some(at) = rest.find(REFUSAL_PREFIX) {
        rendered.push_str(&rest[..at]);

        let tail = &rest[at + REFUSAL_PREFIX.len()..];
        let end = tail.find(REFUSAL_PREFIX).unwrap_or(tail.len());
        let mut fields = tail[..end].splitn(4, REFUSAL_FIELD_SEPARATOR);

        match (fields.next(), fields.next(), fields.next()) {
            (Some(code), Some(human), Some(next_step))
                if !code.is_empty() && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') => {
                rendered.push_str(&with_next_step(human, next_step));
                rendered.push_str(fields.next().map(after_detail).unwrap_or_default());
                any = true;
            }
            _ => {
                rendered.push_str(REFUSAL_PREFIX);
                rendered.push_str(&tail[..end]);
            }
        }

        rest = &tail[end..];
    }

    rendered.push_str(rest);
    any.then_some(rendered)
}

/// What follows a frame's detail field, `text`: the detail is one JSON value, and anything after it
/// was written by a caller that wrapped the frame.
fn after_detail(text: &str) -> &str {
    let mut values = serde_json::Deserializer::from_str(text).into_iter::<serde_json::Value>();

    match values.next() {
        Some(Ok(_)) => &text[values.byte_offset()..],
        _ => "",
    }
}

/// Fold a would-be-dropped next step into the human message, for the degrade-to-`Other` paths
/// that have no typed field left to carry it in. Next steps in this codebase read as complete
/// sentences (e.g. "Move them out of the way (or load and stack them) first."), so a space is
/// enough to join them naturally; an empty next step folds to no-op.
fn with_next_step(human: &str, next_step: &str) -> String {
    if next_step.is_empty() {
        human.to_string()
    } else {
        format!("{} {}", human, next_step)
    }
}

/// Whether `c` must not reach a line of output: a control character, a character some readers
/// treat as a line break (U+2028, U+2029), or a bidirectional formatting character, which can
/// make the displayed text read differently from what it is.
///
/// Control characters cover what a terminal acts on: ESC opens every ANSI/SGR sequence, so with
/// it gone the rest (`[31m`, …) is inert text and no escape grammar needs parsing; a bare `\r`
/// returns the cursor so later text overwrites earlier; an embedded `\n` splits one line into two.
/// A tab is the one control character kept: it only advances the cursor over blank cells, so it
/// can neither overwrite nor reorder text, and indented record text keeps its layout.
fn is_unsafe_in_a_line(c: char) -> bool {
    (c.is_control() && c != '\t')
        || matches!(c, '\u{2028}' | '\u{2029}' | '\u{061C}' | '\u{200E}' | '\u{200F}'
                       | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Replace every [unsafe character](is_unsafe_in_a_line) with a space, so text is frame-safe and
/// render-safe. A head applies it to the rendered copy of any value that came from a record or a
/// remote, never to what `--json` emits: escaping is a terminal concern, and a consumer of the
/// JSON needs the real content. Replaced rather than dropped, so a column width computed before
/// the call still matches what prints, and two words are never welded into a third. Text that
/// may span lines goes through [`sanitize_lines`] instead; see [`CoreError::refusal`] for the
/// frame side.
pub fn sanitize(text: &str) -> String {
    text.chars().map(|c| if is_unsafe_in_a_line(c) { ' ' } else { c }).collect()
}

/// [`sanitize`] each line of `text` and rejoin them with `\n`, so multi-line text keeps its
/// lines while each line is inert. For a sink that prints one block (an error, a warning, a
/// body); a render that indents or prefixes each line splits with [`str::lines`] itself.
pub fn sanitize_lines(text: &str) -> String {
    text.lines().map(sanitize).collect::<Vec<_>>().join("\n")
}

/// Build the sentinel frame for a refusal. Re-sanitizes defensively so a `Refusal` assembled
/// through the struct literal (bypassing [`CoreError::refusal`]) still frames decodably. Detail,
/// when there is any, is a fourth field of compact JSON — which escapes every control character,
/// the separator included, so the field needs no sanitizing of its own.
fn frame(code: &str, message: &str, next_step: &str, details: Option<&serde_json::Value>) -> String {
    let mut framed = format!(
        "{}{}{}{}{}{}",
        REFUSAL_PREFIX,
        code,
        REFUSAL_FIELD_SEPARATOR,
        sanitize(message),
        REFUSAL_FIELD_SEPARATOR,
        sanitize(next_step),
    );

    if let Some(details) = details {
        framed.push(REFUSAL_FIELD_SEPARATOR);
        framed.push_str(&details.to_string());
    }

    framed
}

/// Decode a sentinel frame into `(code, message, next_step)`; `None` for any string that is not a
/// frame. The inverse of [`frame`] without its detail, and the low-level primitive the servers'
/// wire mapping parses — the wire carries no detail, so a detail field never reaches `next_step`.
pub fn deframe(message: &str) -> Option<(&str, &str, &str)> {
    deframe_with_details(message).map(|(code, human, next_step, _)| (code, human, next_step))
}

/// Decode a sentinel frame in full: [`deframe`]'s three fields, plus the detail when the frame
/// carries any — `Err(())` when it does not parse as JSON, for the caller to refuse to decode a
/// refusal from (never a refusal quietly missing the details its code promises).
#[allow(clippy::type_complexity)]
fn deframe_with_details(message: &str) -> Option<(&str, &str, &str, Result<Option<serde_json::Value>, ()>)> {
    let rest = message.strip_prefix(REFUSAL_PREFIX)?;
    let mut parts = rest.splitn(4, REFUSAL_FIELD_SEPARATOR);

    let code = parts.next()?;
    let human = parts.next()?;
    let next_step = parts.next()?;
    let details = match parts.next() {
        None => Ok(None),
        Some(text) => serde_json::from_str(text).map(Some).map_err(|_| ()),
    };

    Some((code, human, next_step, details))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_whose_details_do_not_parse_is_not_decoded_as_a_refusal() {
        let framed = format!("{}{}{{not json", frame("trust_changed", "changed", "verify", None), REFUSAL_FIELD_SEPARATOR);

        // The texts survive and the code is dropped, as for an unknown code; the raw frame never
        // reaches output.
        assert_eq!(CoreError::from(framed), CoreError::Other("changed verify".to_string()));
    }

    /// Proves the round trip for every variant the `refusal_codes!` invocation actually declares —
    /// real exhaustiveness now that `ALL_CODES` is generated from that same invocation (see its doc
    /// comment): a variant added to the enum necessarily appears in `ALL_CODES` too, so this loop
    /// cannot silently skip it the way it could when `ALL_CODES` was a separate, hand-written list.
    #[test]
    fn as_str_and_from_code_round_trip_for_every_code() {
        for code in ALL_CODES.iter().copied() {
            assert_eq!(RefusalCode::from_code(code.as_str()), Some(code), "{}", code.as_str());
        }

        assert_eq!(RefusalCode::from_code("some_future_code"), None);
        assert_eq!(RefusalCode::from_code(""), None);
    }

    #[test]
    fn a_refusal_displays_its_human_message_only() {
        let error = CoreError::refusal(RefusalCode::OutOfScope, "the message", "the next step");
        assert_eq!(error.to_string(), "the message");
        assert!(!error.to_string().contains('\u{1f}'));
    }

    #[test]
    fn a_typed_refusal_round_trips_through_the_bridge_shim() {
        let original = CoreError::refusal(RefusalCode::NarrowUnclean, "human msg", "do this");

        // Lower to String (as a Result<_, String> caller would via `?`/`.into()`), then re-lift.
        let framed: String = original.clone().into();
        assert!(framed.starts_with(REFUSAL_PREFIX), "framed as a sentinel: {:?}", framed);

        let relifted: CoreError = framed.into();
        assert_eq!(relifted, original, "the code and text survive the round trip");
    }

    #[test]
    fn a_plain_string_is_an_unclassified_other() {
        let error: CoreError = "something ordinary went wrong".to_string().into();
        assert_eq!(error, CoreError::Other("something ordinary went wrong".to_string()));
    }

    #[test]
    fn an_unknown_framed_code_keeps_its_message_and_next_step_but_drops_the_code() {
        // A frame whose code this build does not know (a newer peer). It must not leak the raw
        // frame; it degrades to Other, but the recovery guidance (next_step) survives folded into
        // the message rather than being silently dropped (no invented variant, no code).
        let framed = frame("some_future_code", "a human explanation", "do this", None);
        let error: CoreError = framed.into();

        assert_eq!(error, CoreError::Other("a human explanation do this".to_string()));
        assert!(!error.to_string().contains('\u{1f}'));
    }

    #[test]
    fn the_question_mark_operator_crosses_the_frontier_both_ways() {
        // A deep function that stays `Result<_, String>` and produces a refusal (framed via the
        // `.into()` shim), and a caller that is `Result<_, CoreError>` and `?`s it.
        fn deep_string_layer() -> Result<(), String> {
            Err(CoreError::refusal(RefusalCode::OutOfScope, "outside scope", "widen it").into())
        }
        fn typed_caller() -> Result<(), CoreError> {
            deep_string_layer()?; // String -> CoreError via `From<String>` re-lifts the frame
            Ok(())
        }
        match typed_caller().unwrap_err() {
            CoreError::Refusal { code, .. } => assert_eq!(code, RefusalCode::OutOfScope),
            other => panic!("the code must survive the String round trip, got {:?}", other),
        }

        // And the reverse: a typed function `?`'d inside a `Result<_, String>` caller (reframes).
        fn typed_layer() -> Result<(), CoreError> {
            Err(CoreError::refusal(RefusalCode::NarrowUnclean, "unclean", "stack it"))
        }
        fn string_caller() -> Result<(), String> {
            typed_layer()?; // CoreError -> String via `From<CoreError>` reframes
            Ok(())
        }
        let framed = string_caller().unwrap_err();
        match CoreError::from(framed) {
            CoreError::Refusal { code, .. } => assert_eq!(code, RefusalCode::NarrowUnclean),
            other => panic!("the code must survive the reframe, got {:?}", other),
        }
    }

    #[test]
    fn append_to_message_extends_the_message_not_the_next_step() {
        // PR #84 review, findings 2/3: appending via a naive `format!("{} extra", framed)` lands
        // the extra text in `next_step` (deframe's third field takes the rest of the string), not
        // in `message` — invisible in human mode, which prints `message` only. This is the
        // falsifier: red under that naive approach, green under `append_to_message`.
        let original = CoreError::refusal(
            RefusalCode::OversizedTransportUnsupported, "too big", "shrink it",
        );
        let framed: String = original.into();

        let extended = append_to_message(framed, "(additionally, cleanup failed)");
        let relifted: CoreError = extended.into();

        match relifted {
            CoreError::Refusal { code, message, next_step, .. } => {
                assert_eq!(code, RefusalCode::OversizedTransportUnsupported);
                assert_eq!(message, "too big (additionally, cleanup failed)");
                assert_eq!(next_step, "shrink it", "next_step must be untouched");
            }
            other => panic!("expected a refusal to survive, got {:?}", other),
        }
    }

    // Revert: decode only a frame that starts the string → the wrapped frame's separators reach
    // the message; decode its code too → the wrapped frame claims `cache_miss`; sanitize the
    // whole message → the caller's line break is lost; drop what follows a detail → so is its
    // closing sentence; decode only the first frame → the second's words are lost; take any
    // prefix for a frame → the text that is not one is reshaped.
    #[test]
    fn a_wrapped_frame_reads_as_plain_text_and_claims_no_code() {
        let framed: String = CoreError::refusal(RefusalCode::CacheMiss, "object x is missing", "Wait for it.").into();
        let wrapped = format!("The tree could not be read: {}. No parcel was made.", framed.trim_end_matches('.'));

        match CoreError::from(wrapped) {
            CoreError::Other(message) => assert_eq!(
                message, "The tree could not be read: object x is missing Wait for it. No parcel was made."
            ),
            other => panic!("a wrapped frame claims no code: {:?}", other),
        }

        // A frame with detail keeps what the caller wrote after it, and the caller's lines.
        let detailed: String = CoreError::refusal_with_details(
            RefusalCode::CacheMiss, "object y is missing", "Wait.", serde_json::json!({"hash": "y"})
        ).into();
        let wrapped = format!("Two reads failed:\n{} Nothing was written.", detailed);

        match CoreError::from(wrapped) {
            CoreError::Other(message) => assert_eq!(message, "Two reads failed:\nobject y is missing Wait. Nothing was written."),
            other => panic!("a wrapped frame claims no code: {:?}", other),
        }

        // Each of two frames is rendered, and a prefix with no code word before them is left.
        let first: String = CoreError::refusal(RefusalCode::CacheMiss, "a is missing", "Wait for A.").into();
        let second: String = CoreError::refusal(RefusalCode::CacheMiss, "b is missing", "Wait for B.").into();
        let not_a_frame = format!("{}Not A Code{}x{}y", REFUSAL_PREFIX, REFUSAL_FIELD_SEPARATOR, REFUSAL_FIELD_SEPARATOR);
        let wrapped = format!("Read {} then:\n{}\n{}", not_a_frame, first, second);

        match CoreError::from(wrapped) {
            CoreError::Other(message) => assert_eq!(
                message, format!("Read {} then:\na is missing Wait for A.\nb is missing Wait for B.", not_a_frame)
            ),
            other => panic!("wrapped frames claim no code: {:?}", other),
        }
    }

    #[test]
    fn append_to_message_extends_a_plain_unframed_error() {
        let extended = append_to_message("plain error".to_string(), "(and more)");
        assert_eq!(CoreError::from(extended), CoreError::Other("plain error (and more)".to_string()));
    }

    #[test]
    fn a_refusal_built_from_hostile_text_is_frame_safe() {
        // A path sourced from disk can carry the frame separator itself; sanitizing at
        // construction keeps both the render and the frame decodable.
        let error = CoreError::refusal(RefusalCode::OutOfScope, "src/\u{1f}api", "fix it");
        let CoreError::Refusal { message, .. } = &error else { panic!("expected a refusal") };
        assert!(!message.contains('\u{1f}'), "sanitized at construction: {:?}", message);
        assert_eq!(message, "src/ api", "the control char becomes a space, not removed");

        // And it still round-trips through the shim.
        let framed: String = error.clone().into();
        let relifted: CoreError = framed.into();
        assert_eq!(relifted, error);
    }

    #[test]
    fn sanitize_neutralizes_every_unsafe_character_and_nothing_else() {
        // ANSI needs no grammar: with the ESC byte gone, the rest of the sequence is inert text.
        assert_eq!(sanitize("\x1b[31mHIDDEN\x1b[0m"), " [31mHIDDEN [0m");

        // Cursor return, line splits (ASCII and Unicode), NUL and DEL each become one space.
        for unsafe_char in ['\r', '\n', '\0', '\u{7f}', '\u{85}', '\u{2028}', '\u{2029}'] {
            assert_eq!(sanitize(&format!("a{}b", unsafe_char)), "a b", "{:?}", unsafe_char);
        }

        // Every bidirectional formatting character, so text cannot display reordered.
        for bidi in ['\u{061C}', '\u{200E}', '\u{200F}', '\u{202A}', '\u{202E}', '\u{2066}', '\u{2069}'] {
            assert_eq!(sanitize(&format!("a{}b", bidi)), "a b", "{:?}", bidi);
        }

        // Printable text passes untouched, non-ASCII included: this is not an ASCII filter, and
        // the neighbours of each excluded range survive.
        for kept in ["héllo wörld", "日本語", "\u{2027}\u{202F}\u{2065}\u{206A}", "", "plain", "\tindented"] {
            assert_eq!(sanitize(kept), kept);
        }
    }

    #[test]
    fn sanitize_lines_keeps_the_lines_and_neutralizes_each() {
        assert_eq!(sanitize_lines("one \x1b[2J\ntwo\r\n\tthree\u{202E}"), "one  [2J\ntwo\n\tthree ");
        assert_eq!(sanitize_lines("single"), "single");
    }
}
