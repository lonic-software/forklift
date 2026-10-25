//! Office authority over the transport: who a request is, and what the warehouse's office lets
//! that caller do — move a pallet, upload bytes, sanction a trust re-genesis.
//!
//! One copy for every head. Each rule is a pure function of the caller, the trust anchor and the
//! office state, and does no I/O: the head reads that state itself, because only the head knows
//! which read its commit is conditioned on, and a rule handed a second, independent read could
//! decide against an office the commit never checks. Every refusal is a [`Denied`], which a head
//! answers with `403`.

use crate::util::office_utils::{OfficeState, Role, TrustAnchor, UserRecord};
use crate::util::pallet_utils::{PalletNamespace, PalletRef};

/// Who a request is: the transport-level identity a head resolved from its credential. What the
/// identity may do (roles, pallet grants) is decided against the warehouse's office state.
#[derive(Clone, PartialEq, Eq)]
pub enum Principal {
    /// No authentication is configured on this head: full access.
    Open,

    /// The head's static (shared) token: full access, the operator of the head itself.
    Static,

    /// A per-operator token, bound to this office identifier.
    Operator(String),
}

/// An authority rule's refusal: the message for the error body. A head answers it with `403`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Denied(pub String);

/// Look up an operator's user record in the office, against the trust anchor and office state
/// the caller read. `None` when the warehouse has no trust yet (no office means no roles, and the
/// transport token is the whole gate) and in the bootstrap window (see below); refused when the
/// office has a roster that does not enroll `identifier`. Identifiers match by exact equality.
pub fn office_user_of(identifier: &str,
                      anchor: &Option<TrustAnchor>,
                      office_state: &OfficeState) -> Result<Option<UserRecord>, Denied> {
    if anchor.is_none() {
        return Ok(None);
    }

    // The bootstrap window: the anchor is set but the office itself has not been lifted yet (a
    // lift PUTs trust before it uploads the office chain). With no roster there are no roles to
    // enforce — the token stays the gate, and the office ref update still verifies the chain
    // against the anchor, so only the enrolling operator's office can land.
    if office_state.users.is_empty() {
        return Ok(None);
    }

    office_state.users.iter()
        .find(|user| user.identifier == identifier)
        .cloned()
        .map(Some)
        .ok_or_else(|| Denied(format!("\"{}\" is not enrolled in this warehouse's office.", identifier)))
}

/// May this caller upload bytes (objects, signatures)? Uploads are not pallet-scoped, so any
/// non-reader may upload; the ref update is where pallet grants bite. An operator the office does
/// not enroll is refused like a reader, and with no office user (no trust, or the bootstrap
/// window) the token is the whole gate. [`Principal::Static`] and [`Principal::Open`] always may,
/// and the state they are passed is never looked at.
pub fn may_upload(principal: &Principal,
                  anchor: &Option<TrustAnchor>,
                  office_state: &OfficeState) -> Result<(), Denied> {
    let Principal::Operator(identifier) = principal else {
        return Ok(());
    };

    match office_user_of(identifier, anchor, office_state)? {
        Some(user) if user.role == Role::Reader => Err(Denied(
            format!("\"{}\" is a reader; readers cannot upload.", identifier)
        )),
        _ => Ok(()),
    }
}

/// May this caller move `pallet`'s ref? For an office user, a meta pallet may be moved by anyone
/// but a reader, and a working pallet by a user [`UserRecord::may_write_pallet`] admits. Content
/// authorization is not this rule: the office's own parcels are checked per parcel against the
/// signer's role, and another meta pallet's against its own rules, so no meta pallet gets a pallet
/// grant check here. With no office user (no trust, or the bootstrap window) the token is the
/// whole gate. [`Principal::Static`] and [`Principal::Open`] always may.
pub fn may_move(principal: &Principal,
                anchor: &Option<TrustAnchor>,
                office_state: &OfficeState,
                pallet: &PalletRef) -> Result<(), Denied> {
    let Principal::Operator(identifier) = principal else {
        return Ok(());
    };

    let Some(user) = office_user_of(identifier, anchor, office_state)? else {
        return Ok(());
    };

    let allowed = match pallet.namespace {
        PalletNamespace::Meta => user.role != Role::Reader,
        PalletNamespace::User => user.may_write_pallet(&pallet.name),
    };

    if allowed {
        Ok(())
    } else {
        Err(Denied(format!(
            "\"{}\" ({}) may not move pallet \"{}\".",
            identifier, user.role.as_str(), pallet.to_wire()
        )))
    }
}

/// May this caller replace an existing trust anchor with a re-genesis? Only the operator of the
/// head itself: [`Principal::Static`], or [`Principal::Open`] on a head with no authentication at
/// all. A per-operator token derives its authority from the very chain being replaced. Establishing
/// a first anchor is not this rule; it is not restricted.
pub fn may_regenesis(principal: &Principal) -> Result<(), Denied> {
    match principal {
        Principal::Static | Principal::Open => Ok(()),
        Principal::Operator(_) => Err(Denied(
            "A trust reset (re-genesis) must be sanctioned by the server \
            operator: only the static token may replace the anchor. \
            Per-operator tokens derive their authority from the chain \
            being replaced.".to_string()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::office_utils::IdentityClass;

    fn anchor() -> Option<TrustAnchor> {
        Some(TrustAnchor {
            genesis: "0".repeat(64),
            enabled_at: 0,
            boundary: Vec::new(),
            prior_genesis: None,
            adopts: None,
        })
    }

    fn user(identifier: &str, role: Role, pallets: &[&str]) -> UserRecord {
        UserRecord {
            identifier: identifier.to_string(),
            enrolled_at: 0,
            role,
            pallets: pallets.iter().map(|pallet| pallet.to_string()).collect(),
            identity_root: "k".repeat(64),
            class: IdentityClass::Human,
            supervisor: None,
        }
    }

    /// An office enrolling an admin, a writer granted only `main`, an ungranted writer and a
    /// reader.
    fn office() -> OfficeState {
        OfficeState {
            users: vec![
                user("ada", Role::Admin, &[]),
                user("wes", Role::Writer, &["main"]),
                user("wil", Role::Writer, &[]),
                user("rea", Role::Reader, &[]),
            ],
            keys: Vec::new(),
        }
    }

    fn empty_office() -> OfficeState {
        OfficeState { users: Vec::new(), keys: Vec::new() }
    }

    fn operator(identifier: &str) -> Principal {
        Principal::Operator(identifier.to_string())
    }

    fn denial<T>(result: Result<T, Denied>) -> String {
        match result {
            Ok(_) => panic!("expected a refusal"),
            Err(Denied(message)) => message,
        }
    }

    #[test]
    fn office_user_of_is_none_without_trust_and_in_the_bootstrap_window() {
        // No anchor: even a roster enrolling nobody by that name yields no user, never a refusal.
        assert!(office_user_of("mallory", &None, &office()).unwrap().is_none());
        assert!(office_user_of("mallory", &anchor(), &empty_office()).unwrap().is_none());
    }

    #[test]
    fn office_user_of_finds_an_enrolled_user_and_refuses_an_unenrolled_one() {
        let found = office_user_of("wes", &anchor(), &office()).unwrap().expect("wes is enrolled");
        assert_eq!(found, user("wes", Role::Writer, &["main"]));

        assert_eq!(
            denial(office_user_of("mallory", &anchor(), &office())),
            "\"mallory\" is not enrolled in this warehouse's office."
        );

        // Exact equality: a padded spelling of an enrolled identifier is not that user.
        assert!(office_user_of(" wes", &anchor(), &office()).is_err());
    }

    #[test]
    fn may_upload_refuses_readers_and_unenrolled_operators() {
        assert_eq!(
            denial(may_upload(&operator("rea"), &anchor(), &office())),
            "\"rea\" is a reader; readers cannot upload."
        );
        assert_eq!(
            denial(may_upload(&operator("mallory"), &anchor(), &office())),
            "\"mallory\" is not enrolled in this warehouse's office."
        );

        for identifier in ["ada", "wes", "wil"] {
            assert_eq!(may_upload(&operator(identifier), &anchor(), &office()), Ok(()), "{}", identifier);
        }
    }

    #[test]
    fn may_upload_passes_with_no_office_user_and_for_full_access_principals() {
        assert_eq!(may_upload(&operator("rea"), &None, &office()), Ok(()));
        assert_eq!(may_upload(&operator("mallory"), &anchor(), &empty_office()), Ok(()));

        // A full-access principal is never judged against the office, even one that would
        // refuse every operator.
        assert_eq!(may_upload(&Principal::Static, &anchor(), &office()), Ok(()));
        assert_eq!(may_upload(&Principal::Open, &anchor(), &office()), Ok(()));
    }

    #[test]
    fn may_move_applies_the_grant_list_to_working_pallets_only() {
        let main = PalletRef::user("main");
        let side = PalletRef::user("side");

        assert_eq!(may_move(&operator("wes"), &anchor(), &office(), &main), Ok(()));
        assert_eq!(
            denial(may_move(&operator("wes"), &anchor(), &office(), &side)),
            "\"wes\" (writer) may not move pallet \"side\"."
        );
        assert_eq!(may_move(&operator("wil"), &anchor(), &office(), &side), Ok(()));
        assert_eq!(may_move(&operator("ada"), &anchor(), &office(), &side), Ok(()));
        assert_eq!(
            denial(may_move(&operator("rea"), &anchor(), &office(), &main)),
            "\"rea\" (reader) may not move pallet \"main\"."
        );

        // A meta pallet takes no grant check: a writer granted only `main` may move it.
        assert_eq!(may_move(&operator("wes"), &anchor(), &office(), &PalletRef::meta("office")), Ok(()));
    }

    #[test]
    fn may_move_refuses_a_reader_on_a_meta_pallet_naming_it_in_its_wire_form() {
        assert_eq!(
            denial(may_move(&operator("rea"), &anchor(), &office(), &PalletRef::meta("office"))),
            "\"rea\" (reader) may not move pallet \"@office\"."
        );
        assert_eq!(
            denial(may_move(&operator("mallory"), &anchor(), &office(), &PalletRef::meta("office"))),
            "\"mallory\" is not enrolled in this warehouse's office."
        );
    }

    #[test]
    fn may_move_passes_with_no_office_user_and_for_full_access_principals() {
        let side = PalletRef::user("side");

        assert_eq!(may_move(&operator("rea"), &None, &office(), &side), Ok(()));
        assert_eq!(may_move(&operator("mallory"), &anchor(), &empty_office(), &side), Ok(()));
        assert_eq!(may_move(&Principal::Static, &anchor(), &office(), &side), Ok(()));
        assert_eq!(may_move(&Principal::Open, &anchor(), &office(), &side), Ok(()));
    }

    #[test]
    fn may_regenesis_admits_only_the_full_access_principals() {
        assert_eq!(may_regenesis(&Principal::Static), Ok(()));
        assert_eq!(may_regenesis(&Principal::Open), Ok(()));

        // Even the office's own admin: their authority comes from the chain being replaced.
        assert!(denial(may_regenesis(&operator("ada"))).contains("only the static token may replace the anchor"));
    }
}
