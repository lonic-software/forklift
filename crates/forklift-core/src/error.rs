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

use std::borrow::Cow;
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
    /// A path argument is outside the bay's checkout scope (§7.6).
    OutOfScope => "out_of_scope",

    /// A merge in a scoped bay hit an out-of-scope entry that changed on both sides: there is no
    /// content to reconcile, so it refuses rather than guess.
    OutOfScopeConflict => "out_of_scope_conflict",

    /// A scoped bay's spine path flipped between a directory and a file (§7.6): the scope is no
    /// longer valid there and the operation refuses rather than guess.
    ScopePathTypeChanged => "scope_path_type_changed",

    /// A whole-tree verb is not (yet) supported in a scoped (sparse) bay (§7.6).
    SparseWorkspace => "sparse_workspace",

    /// A partial warehouse (cloned with `--only` or `--cache`) was asked to talk to a remote
    /// other than its origin, or its origin record is missing: the content it does not hold was
    /// only ever proved present on the origin, so every fetch, lift and read of remote heads from
    /// it refuses up front.
    NonOriginRemote => "non_origin_remote",

    /// `scope narrow` was asked to drop a subtree that still holds uncommitted work (staged, unstaged or
    /// untracked): it refuses rather than silently discard it.
    NarrowUnclean => "narrow_unclean",

    /// `scope prune` was asked to free a path a checkout still materializes: freeing it would
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
    /// is always the same cheap re-load, or abandoning the load with `unload`. Also
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
    /// step is `lower`, which fast-forwards, or merges the remote's head in when both sides moved.
    RemoteAhead => "remote_ahead",

    /// The local pallet and the remote's both hold parcels the other lacks. Nothing was lifted or
    /// lowered; the next step names how to merge the two.
    Diverged => "diverged",

    /// A command named a pallet this warehouse holds no ref for — or, for `lower`, one the remote
    /// does not have. The next step is `lower <name>` when a remote is configured (the remote may
    /// have it), else `pallet create <name>`; for `lower <name>`, `pallet list --remote`, and for a bare
    /// `lower`, `lift`.
    PalletNotFound => "pallet_not_found",

    /// The directory holds a clone that has not finished: an attempt failed, was killed, or
    /// is still running. Every command but `clone` refuses to enter it; running the same
    /// `clone` again resumes it, keeping what was fetched (`franchise_utils`). The refusal's
    /// details carry `{directory, remote, cause_code, retry}`.
    CloneIncomplete => "clone_incomplete",

    /// A sparse clone's `--only` prefix names no directory in the pallet's head on the remote.
    /// Retrying gets the same answer: the prefix (or the pallet) has to change.
    OnlyPrefixNotFound => "only_prefix_not_found",

    /// A lift's commit named a blob the remote holds neither in its staging area nor in the
    /// warehouse: the upload was discarded (the remote rejected it as corrupt or too large) or never
    /// arrived. Retrying the commit gets the same answer; running the lift again uploads the blob
    /// afresh.
    StagedUploadDiscarded => "staged_upload_discarded",

    /// A `forklift git mirror` run left at least one mirrored pallet out of step with git: refused,
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
    /// next step otherwise brings the pallet back there (`pallet restore <name> --remote`).
    PalletDeletedOnRemote => "pallet_deleted_on_remote",

    /// A cache warehouse (cloned with `--cache`) does not hold content the command reads, and
    /// could not fetch it because its origin could not be reached. The next step names the origin
    /// to wait for.
    CacheMiss => "cache_miss",

    /// A move of a partial warehouse's origin was started and has not finished: until it does,
    /// nothing fetches, lifts, reads the remote's heads or writes the records the move checks
    /// against the new server. The next step finishes the move (`remote move <url>`), or abandons
    /// one that has not started rewriting (`remote abort`).
    OriginMovePending => "origin_move_pending",

    /// A move of a partial warehouse's origin found the new server lacking content this warehouse
    /// relies on its origin to hold, so it left the origin where it was. The next step copies the
    /// old server's warehouse to the new one, server-side, and runs the move again.
    OriginMoveIncomplete => "origin_move_incomplete",

    /// A destructive command was run without `--confirm` while it had something to destroy: work
    /// in the working tree, content held here, or the trust guarantee. It showed what it would
    /// destroy and changed nothing; the details carry that preview, and running it again with
    /// `--confirm` destroys it. `lower` refuses whenever a resolution names `theirs`, before it
    /// knows whether that drops anything.
    ConfirmationRequired => "confirmation_required",

    /// The warehouse is in an older format than the one this build reads. A build reads exactly one
    /// format, so nothing was read or written. The next step upgrades it in place.
    WarehouseFormatOlder => "warehouse_format_older",

    /// The warehouse is in a newer format than the one this build reads: a newer Forklift upgraded
    /// it, and upgrades are one-way. Nothing was read or written. The next step updates this binary.
    WarehouseFormatNewer => "warehouse_format_newer",

    /// An upgrade of this warehouse was started and did not finish, so what it holds may be part
    /// one format and part another. Every command but the upgrade refuses until it has; running the
    /// upgrade again resumes it.
    WarehouseUpgradeIncomplete => "warehouse_upgrade_incomplete",

    /// Another process is upgrading this warehouse right now. Nothing waits on it; the next step is
    /// to run the command again once it finishes.
    WarehouseUpgradeRunning => "warehouse_upgrade_running",

    /// The warehouse's format lease file is missing and this user cannot create it, so no command can
    /// be kept safe from an upgrade. The next step creates the file or restores write access.
    WarehouseLeaseMissing => "warehouse_lease_missing",

    /// An upgrade was refused because another process has the warehouse open: a command, a server
    /// serving it, or another upgrade. Nothing was changed; the next step is to let them finish, then
    /// run the upgrade again.
    WarehouseInUse => "warehouse_in_use",

    /// The remote's head of a pallet holds an entry named like the warehouse's metadata folder, so
    /// no client can check it out. Nothing was changed; the next step repairs the head into one
    /// without it.
    HeadNotMaterializable => "head_not_materializable",

    /// The working state blocks the command: staged or unstaged changes to tracked files that a
    /// merge, a pick or a shift (an undo of one, or a pallet created at a revision) would have to
    /// write over. Nothing was changed; the next step sets the changes aside.
    Conflict => "conflict",

    /// `lower` merged the remote's diverged head into the pallet and the merge conflicts: the
    /// markers are written and the consolidation is left in progress. The refusal's details name
    /// the conflicted paths; the next step completes the merge or abandons it.
    MergeConflicts => "merge_conflicts",
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
    #[track_caller]
    pub fn refusal(
        code: RefusalCode,
        message: impl Into<String>,
        next_step: impl Into<String>,
    ) -> CoreError {
        let (message, next_step) = (sanitize(&message.into()), sanitize(&next_step.into()));
        debug_assert!(!restates_next_step(&message, &next_step), "A refusal's message restates its next step: {:?}", message);
        CoreError::Refusal {
            code,
            message,
            next_step,
            details: None,
        }
    }

    /// Build a classified refusal that carries structured detail as well — see
    /// [`CoreError::refusal`] for the sanitizing of the two texts. The detail needs none: the frame
    /// carries it as compact JSON, which escapes every control character.
    #[track_caller]
    pub fn refusal_with_details(
        code: RefusalCode,
        message: impl Into<String>,
        next_step: impl Into<String>,
        details: serde_json::Value,
    ) -> CoreError {
        let (message, next_step) = (sanitize(&message.into()), sanitize(&next_step.into()));
        debug_assert!(!restates_next_step(&message, &next_step), "A refusal's message restates its next step: {:?}", message);
        CoreError::Refusal {
            code,
            message,
            next_step,
            details: Some(details),
        }
    }

    /// A refusal relaying a remote's own words — its message, or a caller's text built around it.
    /// [Sanitized](sanitize) as [`CoreError::refusal`] is, but without its debug check that the
    /// message does not restate the step: that check is about this build's wording, and a remote
    /// (another version, another head) may word its message any way it likes.
    pub fn remote_refusal(
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
}

impl fmt::Display for CoreError {
    /// Renders the error as one line of text — a `Refusal` shows its `message` with its next step
    /// [folded in](with_next_step) (no code, no frame), an `Other` shows its string. A message no
    /// longer carries its own step, so text that embeds an error (a clone's failure naming its
    /// cause) keeps the cause's way out this way. A head prints a failure through
    /// [`human_failure_text`] instead, which puts the next step on a line of its own.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoreError::Refusal { message, next_step, .. } => f.write_str(&with_next_step(message, next_step)),
            CoreError::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for CoreError {}

impl CoreError {
    /// The error as a head prints it in human mode: see [`human_failure_text`]. An `Other` has no
    /// next step; one that degraded from a refusal already carries it, folded into its text.
    pub fn human_text(&self) -> String {
        match self {
            CoreError::Refusal { message, next_step, .. } => human_failure_text(message, Some(next_step)),
            CoreError::Other(message) => human_failure_text(message, None),
        }
    }
}

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
                    message: human.into_owned(),
                    next_step: next_step.to_string(),
                    details,
                },
                _ => CoreError::Other(with_next_step(&human, next_step)),
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

/// Append `extra` prose to an error's human-visible message, sanitized as a frame's fields are. A
/// naive `format!("{} {}", error, extra)` on a framed refusal reads the same once decoded, since
/// [`deframe`] takes what follows a frame's detail into its message; this is the typed spelling of
/// it, and the one that sanitizes the extra text before it is framed. This re-lifts `error`
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
    split_detail(text).map_or("", |(_, rest)| rest)
}

/// A frame's detail field, `text`, split into its detail (`None` for `null`) and the text a caller
/// wrote after it; `Err` when it does not open with a JSON value. `null` is read by hand: a JSON
/// stream takes a literal run straight into text (`null.`) for a malformed value. Every other
/// detail is an object, which ends itself.
fn split_detail(text: &str) -> Result<(Option<serde_json::Value>, &str), ()> {
    if let Some(rest) = text.strip_prefix("null") {
        return Ok((None, rest));
    }

    let mut values = serde_json::Deserializer::from_str(text).into_iter::<serde_json::Value>();

    match values.next() {
        Some(Ok(serde_json::Value::Null)) => Ok((None, &text[values.byte_offset()..])),
        Some(Ok(details)) => Ok((Some(details), &text[values.byte_offset()..])),
        _ => Err(()),
    }
}

/// Fold a next step into a message, for a text with no field of its own to carry it: a refusal
/// degraded to `Other` (an unknown code, a frame a caller wrapped) and a failure reported as one
/// line among others. The step keeps its [`NEXT_STEP_LABEL`], since many are bare commands, and
/// stays on the message's line, because the text may sit inside a longer one. It is closed in
/// parentheses, so whatever a caller wrote after it does not read as part of the step. A blank
/// step folds to the message alone, and so does a step the message already carries (a remote's
/// wording, which this build does not control).
pub fn with_next_step(human: &str, next_step: &str) -> String {
    match next_step.trim() {
        step if step.is_empty() || human.contains(step) => human.to_string(),
        step => format!("{} ({}{})", human, NEXT_STEP_LABEL, step),
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

/// Whether `message` carries `next_step` (a blank step never counts). A head prints the step on a
/// line of its own under the message, so a message that carries it too shows it twice: this
/// build's refusals assert they do not, in debug builds, in [`CoreError::refusal`] and
/// [`CoreError::refusal_with_details`]. A refusal carrying a remote's own words is not this build's
/// wording, and is built with [`CoreError::remote_refusal`], which does not check.
pub fn restates_next_step(message: &str, next_step: &str) -> bool {
    let step = next_step.trim();
    !step.is_empty() && message.contains(step)
}

/// The label [`human_failure_text`] and [`with_next_step`] put before a next step. A next step is
/// often a bare command (`forklift lower`), and printed unlabelled under an error it reads as the
/// command that failed.
pub const NEXT_STEP_LABEL: &str = "Next step: ";

/// What a head prints for a failure in human mode: the message, [sanitized](sanitize_lines), then
/// the next step on one line of its own after [`NEXT_STEP_LABEL`], [sanitized](sanitize) to that
/// one line. A missing or blank next step adds no line. A person at a terminal sees the same
/// recovery a `--json` consumer reads from `next_step`.
pub fn human_failure_text(message: &str, next_step: Option<&str>) -> String {
    let message = sanitize_lines(message);

    match next_step.map(str::trim).filter(|step| !step.is_empty()) {
        Some(step) => format!("{}\n{}{}", message, NEXT_STEP_LABEL, sanitize(step)),
        None => message,
    }
}

/// Build the sentinel frame for a refusal. Re-sanitizes defensively so a `Refusal` assembled
/// through the struct literal (bypassing [`CoreError::refusal`]) still frames decodably. The
/// detail is a fourth field of compact JSON, `null` when there is none — which escapes every
/// control character, the separator included, so the field needs no sanitizing of its own. It is
/// always written, because it is where the next step ends: a caller that wraps the frame
/// (`format!("…: {}. More.", e)`) writes after it, and without the field that text would read as
/// part of the step.
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

    framed.push(REFUSAL_FIELD_SEPARATOR);
    framed.push_str(&details.map_or_else(|| "null".to_string(), |details| details.to_string()));
    framed
}

/// Decode a sentinel frame into `(code, message, next_step)`; `None` for any string that is not a
/// frame. The inverse of [`frame`] without its detail, and the low-level primitive the servers'
/// wire mapping parses — the wire carries no detail, so a detail field never reaches `next_step`.
/// Text a caller wrote after the frame (`format!("{} More.", framed)`) joins the message: it is
/// about the failure, and is no part of the step or the detail. A frame inside that text is
/// rendered as a wrapped one is, and the whole is [sanitized](sanitize) to one line, since a
/// refusal's message is one line.
pub fn deframe(message: &str) -> Option<(&str, Cow<'_, str>, &str)> {
    deframe_with_details(message).map(|(code, human, next_step, _)| (code, human, next_step))
}

/// Decode a sentinel frame in full: [`deframe`]'s three fields, plus the detail when the frame
/// carries any — `Err(())` when it does not parse as JSON, for the caller to refuse to decode a
/// refusal from (never a refusal quietly missing the details its code promises).
#[allow(clippy::type_complexity)]
fn deframe_with_details(message: &str) -> Option<(&str, Cow<'_, str>, &str, Result<Option<serde_json::Value>, ()>)> {
    let rest = message.strip_prefix(REFUSAL_PREFIX)?;
    let mut parts = rest.splitn(4, REFUSAL_FIELD_SEPARATOR);

    let code = parts.next()?;
    let human = parts.next()?;
    let next_step = parts.next()?;
    let (details, after) = match parts.next().map(split_detail) {
        None => (Ok(None), ""),
        Some(Ok((details, after))) => (Ok(details), after),
        Some(Err(())) => (Err(()), ""),
    };
    let human = match after.trim() {
        "" => Cow::Borrowed(human),
        _ => {
            let after = render_wrapped_frames(after).map_or(Cow::Borrowed(after), Cow::Owned);
            Cow::Owned(sanitize(&format!("{}{}", human, after)))
        }
    };

    Some((code, human, next_step, details))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_whose_details_do_not_parse_is_not_decoded_as_a_refusal() {
        let framed = format!(
            "{}trust_changed{}changed{}verify{}{{not json",
            REFUSAL_PREFIX, REFUSAL_FIELD_SEPARATOR, REFUSAL_FIELD_SEPARATOR, REFUSAL_FIELD_SEPARATOR,
        );

        // The texts survive and the code is dropped, as for an unknown code; the raw frame never
        // reaches output.
        assert_eq!(CoreError::from(framed), CoreError::Other("changed (Next step: verify)".to_string()));
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
    fn a_refusal_displays_its_message_and_folded_step_without_its_frame() {
        let error = CoreError::refusal(RefusalCode::OutOfScope, "the message", "the next step");
        assert_eq!(error.to_string(), "the message (Next step: the next step)");
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

        assert_eq!(error, CoreError::Other("a human explanation (Next step: do this)".to_string()));
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
        // The extra text joins `message`, never `next_step`, where it would print under the
        // "Next step" label and reach a `--json` consumer as the recovery.
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
    // prefix for a frame → the text that is not one is reshaped; frame no detail when there is
    // none → the caller's closing sentence reads as part of the next step.
    #[test]
    fn a_wrapped_frame_reads_as_plain_text_and_claims_no_code() {
        let framed: String = CoreError::refusal(RefusalCode::CacheMiss, "object x is missing", "Wait for it.").into();
        let wrapped = format!("The tree could not be read: {}. No parcel was made.", framed);

        match CoreError::from(wrapped) {
            CoreError::Other(message) => assert_eq!(
                message, "The tree could not be read: object x is missing (Next step: Wait for it.). No parcel was made."
            ),
            other => panic!("a wrapped frame claims no code: {:?}", other),
        }

        // A frame with detail keeps what the caller wrote after it, and the caller's lines.
        let detailed: String = CoreError::refusal_with_details(
            RefusalCode::CacheMiss, "object y is missing", "Wait.", serde_json::json!({"hash": "y"})
        ).into();
        let wrapped = format!("Two reads failed:\n{} Nothing was written.", detailed);

        match CoreError::from(wrapped) {
            CoreError::Other(message) => assert_eq!(message, "Two reads failed:\nobject y is missing (Next step: Wait.) Nothing was written."),
            other => panic!("a wrapped frame claims no code: {:?}", other),
        }

        // Each of two frames is rendered, and a prefix with no code word before them is left.
        let first: String = CoreError::refusal(RefusalCode::CacheMiss, "a is missing", "Wait for A.").into();
        let second: String = CoreError::refusal(RefusalCode::CacheMiss, "b is missing", "Wait for B.").into();
        let not_a_frame = format!("{}Not A Code{}x{}y", REFUSAL_PREFIX, REFUSAL_FIELD_SEPARATOR, REFUSAL_FIELD_SEPARATOR);
        let wrapped = format!("Read {} then:\n{}\n{}", not_a_frame, first, second);

        match CoreError::from(wrapped) {
            CoreError::Other(message) => assert_eq!(
                message, format!("Read {} then:\na is missing (Next step: Wait for A.)\nb is missing (Next step: Wait for B.)", not_a_frame)
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
    fn a_failure_prints_its_next_step_on_a_labelled_line_of_its_own() {
        assert_eq!(human_failure_text("It failed.", Some("forklift lower")), "It failed.\nNext step: forklift lower");

        // No step, or a blank one, adds no line: `ForkliftError::new` wraps any step in `Some`.
        assert_eq!(human_failure_text("It failed.", None), "It failed.");
        assert_eq!(human_failure_text("It failed.", Some("")), "It failed.");
        assert_eq!(human_failure_text("It failed.", Some(" \t")), "It failed.");

        // The step is made inert like the message: it can quote a path or a remote's answer.
        assert_eq!(human_failure_text("a\x1b[2J", Some("b\x1b[2J")), "a [2J\nNext step: b [2J");

        // The step keeps to its one labelled line; the message keeps its lines.
        assert_eq!(human_failure_text("one\ntwo", Some("three\nfour")), "one\ntwo\nNext step: three four");
    }

    #[test]
    fn every_error_prints_through_the_same_human_text() {
        let refusal = CoreError::refusal(RefusalCode::OutOfScope, "It failed.", "forklift lower");
        assert_eq!(refusal.human_text(), "It failed.\nNext step: forklift lower");

        // An `Other` has no step, and is made inert like a refusal: it can quote a path or a peer.
        assert_eq!(CoreError::Other("a\x1b[2J\nb".to_string()).human_text(), "a [2J\nb");
    }

    /// Text a caller writes after a frame that starts the string (`format!("{} More.", e)`) joins
    /// the message, and the refusal keeps its code, step and detail. Revert: read the detail field
    /// whole → the detail does not parse, so the text and the code are both lost; join the text
    /// as it is → a second frame in it prints its framing words and `null`.
    #[test]
    fn text_after_a_leading_frame_joins_its_message() {
        let framed: String = CoreError::refusal(RefusalCode::CacheMiss, "object x is missing.", "Wait for it.").into();

        assert_eq!(
            CoreError::from(format!("{} The undo entry is kept.\nRun it again.", framed)),
            CoreError::refusal(RefusalCode::CacheMiss, "object x is missing. The undo entry is kept. Run it again.", "Wait for it."),
        );

        let detailed: String = CoreError::refusal_with_details(
            RefusalCode::CacheMiss, "object y is missing.", "Wait.", serde_json::json!({"hash": "y"})
        ).into();

        assert_eq!(
            CoreError::from(format!("{} Nothing was written.", detailed)),
            CoreError::refusal_with_details(
                RefusalCode::CacheMiss, "object y is missing. Nothing was written.", "Wait.", serde_json::json!({"hash": "y"})
            ),
        );
        assert_eq!(deframe(&format!("{} More.", framed)).map(|(_, human, _)| human.into_owned()), Some("object x is missing. More.".to_string()));

        // A second frame in that text is rendered, never shown raw.
        let second: String = CoreError::refusal(RefusalCode::OutOfScope, "b is missing.", "Wait for B.").into();
        assert_eq!(
            CoreError::from(format!("{} Also: {}", framed, second)),
            CoreError::refusal(RefusalCode::CacheMiss, "object x is missing. Also: b is missing. (Next step: Wait for B.)", "Wait for it."),
        );
    }

    #[test]
    fn a_folded_next_step_keeps_its_label_on_the_message_line() {
        assert_eq!(with_next_step("It failed.", "forklift lower"), "It failed. (Next step: forklift lower)");
        assert_eq!(with_next_step("It failed.", " "), "It failed.");

        // A frame a caller wrapped keeps the label too.
        let wrapped = format!("Could not lift: {}", String::from(CoreError::refusal(RefusalCode::OutOfScope, "It failed.", "forklift lower")));
        assert_eq!(CoreError::from(wrapped), CoreError::Other("Could not lift: It failed. (Next step: forklift lower)".to_string()));
    }

    #[test]
    fn sanitize_lines_keeps_the_lines_and_neutralizes_each() {
        assert_eq!(sanitize_lines("one \x1b[2J\ntwo\r\n\tthree\u{202E}"), "one  [2J\ntwo\n\tthree ");
        assert_eq!(sanitize_lines("single"), "single");
    }

    /// A message that carries its own next step would print it twice, so this build's refusals
    /// may not: the debug check in the constructors refuses one. Revert: drop either check → red.
    #[test]
    #[cfg(debug_assertions)]
    fn a_refusal_whose_message_restates_its_step_is_refused() {
        let restated = || CoreError::refusal(RefusalCode::Behind, "Behind. Run \"forklift shift main\".", " forklift shift main ");
        let detailed = || CoreError::refusal_with_details(
            RefusalCode::TrustChanged, "Changed. Verify it.", "Verify it.", serde_json::json!({}),
        );

        assert!(std::panic::catch_unwind(restated).is_err());
        assert!(std::panic::catch_unwind(detailed).is_err());

        // The domain is not empty: a message that does not carry its step, or a blank step, passes.
        CoreError::refusal(RefusalCode::Behind, "Behind.", "forklift shift main");
        CoreError::refusal(RefusalCode::Behind, "Behind.", "  ");
        assert!(!restates_next_step("anything", ""));
        assert!(restates_next_step("Run x now.", " x "));
    }

    // Revert: Display shows the message alone → text embedding a refusal loses its way out.
    #[test]
    fn an_embedded_refusal_keeps_its_next_step() {
        let cause = CoreError::refusal(RefusalCode::IncompleteLoad, "Interrupted.", "forklift heal");
        assert_eq!(format!("clone failed: {}", cause), "clone failed: Interrupted. (Next step: forklift heal)");

        // A refusal relaying a remote's words is built unchecked, whatever they say.
        let remote = CoreError::remote_refusal(RefusalCode::Behind, "Behind. Run forklift lower.", "forklift lower");
        // Folded, a step the message already carries is not said twice.
        assert_eq!(remote.to_string(), "Behind. Run forklift lower.");
        assert_eq!(CoreError::Other("plain".into()).to_string(), "plain");
    }
}
