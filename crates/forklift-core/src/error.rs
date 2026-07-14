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

    /// A lift from a sparse warehouse was aimed at a remote other than its origin: the
    /// out-of-scope closure was only ever proved present on the origin, so it refuses up front.
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
        /// The human explanation. Rendered verbatim by [`fmt::Display`]; control characters are
        /// stripped at construction so it stays frame-safe.
        message: String,
        /// The machine-actionable recovery step.
        next_step: String,
    },

    /// An error without a more specific classification yet — a plain message, no code.
    Other(String),
}

impl CoreError {
    /// Build a classified refusal. Strips ASCII control characters (including `\u{1f}`, the frame
    /// separator) out of the message and next step so the value is safe to render *and* safe to
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
/// matters most and there is no typed field left to carry it in.
impl From<String> for CoreError {
    fn from(message: String) -> CoreError {
        if let Some((code, human, next_step)) = deframe(&message) {
            return match RefusalCode::from_code(code) {
                Some(code) => CoreError::Refusal {
                    code,
                    message: human.to_string(),
                    next_step: next_step.to_string(),
                },
                None => CoreError::Other(with_next_step(human, next_step)),
            };
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
            CoreError::Refusal { code, message, next_step } => {
                frame(code.as_str(), &message, &next_step)
            }
            CoreError::Other(message) => message,
        }
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

/// Strip ASCII control characters (replacing each with a space) so text is frame-safe and
/// render-safe. See [`CoreError::refusal`].
fn sanitize(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// Build the sentinel frame for a refusal. Re-sanitizes defensively so a `Refusal` assembled
/// through the struct literal (bypassing [`CoreError::refusal`]) still frames decodably.
fn frame(code: &str, message: &str, next_step: &str) -> String {
    format!(
        "{}{}{}{}{}{}",
        REFUSAL_PREFIX,
        code,
        REFUSAL_FIELD_SEPARATOR,
        sanitize(message),
        REFUSAL_FIELD_SEPARATOR,
        sanitize(next_step),
    )
}

/// Decode a sentinel frame into `(code, message, next_step)`; `None` for any string that is not a
/// frame. The inverse of [`frame`], and the low-level primitive both the wire (`error_of`) and the
/// String→`CoreError` bridge parse.
pub fn deframe(message: &str) -> Option<(&str, &str, &str)> {
    let rest = message.strip_prefix(REFUSAL_PREFIX)?;
    let mut parts = rest.splitn(3, REFUSAL_FIELD_SEPARATOR);

    let code = parts.next()?;
    let human = parts.next()?;
    let next_step = parts.next()?;

    Some((code, human, next_step))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let framed = frame("some_future_code", "a human explanation", "do this");
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
}
