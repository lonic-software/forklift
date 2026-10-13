//! The trust idiom's shared vocabulary: what a refusal or a warning says about a trust anchor, in
//! the structured form a machine consumer acts on (`error.details`, or a warning's `details`, under
//! `--json`; `docs/MACHINE_INTERFACE.md`).
//!
//! A trust anchor is never adopted or changed silently. First contact adopts it loudly (the
//! caller warns with [`first_contact`]'s details); a remote declaring a *different* anchor from the
//! one held is refused with [`RefusalCode::TrustChanged`], whose details name the remote's value
//! *now* (`current`) and the one this warehouse holds (`previous`). A remote declaring *no* anchor
//! is not refused by a sync — it is also a warehouse whose holder enrolled but has not lifted yet —
//! only by an `--expect` pin. The one override,
//! `office accept-regenesis --confirm <value>`, succeeds only when its value still equals
//! `current` — so what is accepted is exactly what a human or agent was shown.

use serde::Serialize;
use crate::error::{CoreError, RefusalCode};
use crate::model::remote::TrustAnchorDto;

/// The `--json` warning code for a trust anchor adopted on first contact.
pub const WARNING_TRUST_ADOPTED: &str = "trust_adopted";

/// What happened to a trust anchor: the `what_changed` of [`TrustDetails`]. A closed set — a
/// consumer branches on it, never on the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustChange {
    /// No anchor was held; the remote's was adopted. A warning, never a refusal.
    FirstContact,

    /// The remote's anchor was reset and names the held one as its prior: a re-genesis of the
    /// chain this warehouse trusts. Accepted only by `office accept-regenesis --confirm <current>`.
    Regenesis,

    /// The remote's anchor neither is nor names the held one: another warehouse, tampering, or a
    /// second reset this warehouse never saw. Nothing overrides it.
    Unrelated,

    /// The remote's anchor is not the value pinned with `--expect` (`current` is `null` when the
    /// remote has no anchor at all).
    Unexpected,
}

/// The structured detail of a trust refusal or warning, in its documented key order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TrustDetails {
    /// What the anchor belongs to: the remote, as its URL.
    pub subject: String,

    /// The remote's genesis now — the value an override must name.
    pub current: Option<String>,

    /// The genesis this warehouse holds, when it holds one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,

    pub what_changed: TrustChange,
}

impl TrustDetails {
    /// The details as a JSON value, for an error or a warning document.
    pub fn to_value(&self) -> serde_json::Value {
        // A struct of strings and a unit enum: serializing it cannot fail.
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// The details of a first-contact adoption of `genesis` from `subject`.
pub fn first_contact(subject: &str, genesis: &str) -> TrustDetails {
    TrustDetails {
        subject: subject.to_string(),
        current: Some(genesis.to_string()),
        previous: None,
        what_changed: TrustChange::FirstContact,
    }
}

/// The refusal for a remote whose anchor differs from the one this warehouse holds: a re-genesis
/// when the remote's anchor names the held genesis as its prior, otherwise an unrelated anchor.
///
/// # Arguments
/// * `subject`       - The remote's URL.
/// * `local_genesis` - The genesis this warehouse holds.
/// * `remote`        - The remote's anchor, already known to differ from `local_genesis`.
/// * `refusing`      - The closing sentence naming what is refused (e.g. "Refusing to sync.").
pub fn anchor_changed(subject: &str, local_genesis: &str, remote: &TrustAnchorDto, refusing: &str) -> CoreError {
    let names_prior = remote.prior_genesis.as_deref() == Some(local_genesis);
    let what_changed = if names_prior { TrustChange::Regenesis } else { TrustChange::Unrelated };
    let details = TrustDetails {
        subject: subject.to_string(),
        current: Some(remote.genesis.clone()),
        previous: Some(local_genesis.to_string()),
        what_changed,
    };

    if names_prior {
        refusal(
            format!(
                "The remote's trust anchor CHANGED (re-genesis): new genesis {} replaces this \
                warehouse's {}. This changes who controls the warehouse. {}",
                remote.genesis, local_genesis, refusing
            ),
            accept_next_step(&remote.genesis),
            details,
        )
    } else {
        refusal(
            format!(
                "The remote's trust anchor CHANGED: its genesis {} neither is nor names this \
                warehouse's {}. This is another warehouse, tampering, or a second reset this \
                warehouse never saw. {}",
                remote.genesis, local_genesis, refusing
            ),
            UNRELATED_NEXT_STEP,
            details,
        )
    }
}

/// The refusal for an `accept-regenesis --confirm` whose value is not the remote's genesis now:
/// the anchor moved after it was shown, or the value was mistyped. Fresh details carry the value
/// as it is now.
///
/// # Arguments
/// * `subject`       - The remote's URL.
/// * `local_genesis` - The genesis this warehouse holds.
/// * `remote`        - The remote's anchor now (a re-genesis of `local_genesis`).
/// * `confirmed`     - The value the caller confirmed.
pub fn confirm_mismatch(subject: &str, local_genesis: &str, remote: &TrustAnchorDto, confirmed: &str) -> CoreError {
    refusal(
        format!(
            "The confirmed genesis {} is not the remote's current re-genesis {}: it changed \
            after it was shown, or the value was mistyped. Nothing was accepted.",
            confirmed, remote.genesis
        ),
        accept_next_step(&remote.genesis),
        TrustDetails {
            subject: subject.to_string(),
            current: Some(remote.genesis.clone()),
            previous: Some(local_genesis.to_string()),
            what_changed: TrustChange::Regenesis,
        },
    )
}

/// Check an out-of-band pin (`--expect`) against the remote's anchor from the handshake, before
/// anything is fetched. Exact match only: a pin is a security check, and a prefix would weaken it.
///
/// # Arguments
/// * `subject`       - The remote's URL.
/// * `remote`        - The remote's anchor, from the handshake.
/// * `local_genesis` - The genesis this warehouse holds, if any (reported as `previous`).
/// * `expected`      - The pinned genesis.
///
/// # Returns
/// * `Ok(())`         - The remote's genesis is the pinned one.
/// * `Err(CoreError)` - A `trust_changed` refusal, `what_changed: "unexpected"`.
pub fn check_expected(subject: &str,
                      remote: Option<&TrustAnchorDto>,
                      local_genesis: Option<&str>,
                      expected: &str) -> Result<(), CoreError> {
    let current = remote.map(|anchor| anchor.genesis.as_str());

    if current == Some(expected) {
        return Ok(());
    }

    let message = match current {
        Some(genesis) => format!(
            "The remote's trust anchor is genesis {}, not the expected {}. Refusing before \
            anything is fetched.",
            genesis, expected
        ),
        None => format!(
            "The remote has no trust anchor, but genesis {} was expected. Refusing before \
            anything is fetched.",
            expected
        ),
    };

    Err(refusal(
        message,
        "Verify out of band which genesis this remote should have before re-running.",
        TrustDetails {
            subject: subject.to_string(),
            current: current.map(str::to_string),
            previous: local_genesis.map(str::to_string),
            what_changed: TrustChange::Unexpected,
        },
    ))
}

/// A `trust_changed` refusal. The message ends with the next step, as every refusal's does: a
/// human sees only the message.
fn refusal(message: String, next_step: impl Into<String>, details: TrustDetails) -> CoreError {
    let next_step = next_step.into();
    CoreError::refusal_with_details(
        RefusalCode::TrustChanged,
        format!("{} {}", message, next_step),
        next_step,
        details.to_value(),
    )
}

/// The next step of a re-genesis refusal: the one override, bound to the value shown.
fn accept_next_step(genesis: &str) -> String {
    format!(
        "Verify genesis {} out of band, then accept exactly it: \"forklift office \
        accept-regenesis --confirm {}\".",
        genesis, genesis
    )
}

/// The next step of an unrelated-anchor refusal, which has no override.
const UNRELATED_NEXT_STEP: &str =
    "Check that the remote is the warehouse you meant. No override accepts an unrelated anchor.";

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor(genesis: &str, prior: Option<&str>) -> TrustAnchorDto {
        TrustAnchorDto {
            genesis: genesis.to_string(),
            enabled_at: 0,
            boundary: Vec::new(),
            prior_genesis: prior.map(str::to_string),
            adopts: None,
        }
    }

    fn refusal_parts(error: CoreError) -> (RefusalCode, String, String, serde_json::Value) {
        match error {
            CoreError::Refusal { code, message, next_step, details } =>
                (code, message, next_step, details.expect("a trust refusal carries details")),
            CoreError::Other(other) => panic!("expected a refusal, got Other({:?})", other),
        }
    }

    /// The documented shape, asserted against literals rather than against the serializer's own
    /// output for a sibling, so a renamed key or a changed enum spelling goes red.
    #[test]
    fn a_regenesis_names_the_remote_value_now_and_the_held_one() {
        let error = anchor_changed("http://remote", "held", &anchor("fresh", Some("held")), "Refusing to sync.");
        let (code, message, next_step, details) = refusal_parts(error);

        assert_eq!(code, RefusalCode::TrustChanged);
        assert_eq!(code.as_str(), "trust_changed");
        assert_eq!(details, serde_json::json!({
            "subject": "http://remote",
            "current": "fresh",
            "previous": "held",
            "what_changed": "regenesis",
        }));
        assert!(message.contains("CHANGED"), "{}", message);
        assert!(next_step.contains("accept-regenesis --confirm fresh"), "{}", next_step);
    }

    #[test]
    fn an_anchor_that_does_not_name_the_held_one_is_unrelated_and_offers_no_override() {
        let error = anchor_changed("http://remote", "held", &anchor("other", Some("someone-else")), "Refusing to lift.");
        let (_, _, next_step, details) = refusal_parts(error);

        assert_eq!(details["what_changed"], "unrelated");
        assert_eq!(details["current"], "other");
        assert!(!next_step.contains("--confirm"), "{}", next_step);
    }

    #[test]
    fn a_confirm_mismatch_carries_the_current_value_not_the_confirmed_one() {
        let error = confirm_mismatch("http://remote", "held", &anchor("fresh", Some("held")), "stale");
        let (_, message, _, details) = refusal_parts(error);

        assert_eq!(details["current"], "fresh");
        assert_eq!(details["what_changed"], "regenesis");
        assert!(message.contains("stale"), "{}", message);
    }

    #[test]
    fn an_expect_pin_passes_only_on_an_exact_match() {
        let remote = anchor("abcdef", None);
        assert_eq!(check_expected("u", Some(&remote), None, "abcdef"), Ok(()));

        let (_, _, _, details) = refusal_parts(check_expected("u", Some(&remote), None, "abc").unwrap_err());
        assert_eq!(details, serde_json::json!({
            "subject": "u", "current": "abcdef", "what_changed": "unexpected",
        }));

        let (_, _, _, details) = refusal_parts(check_expected("u", None, Some("held"), "abcdef").unwrap_err());
        assert_eq!(details, serde_json::json!({
            "subject": "u", "current": null, "previous": "held", "what_changed": "unexpected",
        }));
    }

    #[test]
    fn trust_details_survive_the_string_bridge() {
        let original = anchor_changed("http://remote", "held", &anchor("fresh", Some("held")), "Refusing.");
        let framed: String = original.clone().into();
        assert_eq!(CoreError::from(framed), original);
    }
}
