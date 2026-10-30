//! Offline verification of a warehouse's signed history.
//!
//! Shared by the `audit` command and by remotes: the server heads run the same
//! verification before committing a ref update, so a remote can never be pushed into a
//! state a local audit would reject.

use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::cell::RefCell;
use std::sync::{Mutex, OnceLock};
use crate::model::tree_item::TreeItem;
use crate::util::office_utils::{KeyRecord, OfficeState, TrustAnchor};
use crate::util::scope_utils::{PathScope, ScopeClass};
use crate::util::{cache_miss, fanout_utils, file_utils, graph_utils, object_utils, office_utils, pallet_utils, recovery_utils, sign_utils};
use crate::util::sealed_utils::SealedSet;
use crate::util::pallet_utils::{AnchorPrecondition, OfficePrecondition, PalletNamespace, PalletsPrecondition, RefPreconditions};

/// Why an office chain did not verify. The heads answer the two classes differently: an
/// inauthentic chain is bad input, while an authentic parcel that oversteps its signer's
/// privileges is something that signer may not do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfficeChainError {
    /// The chain is unreadable or does not reach the genesis, a parcel's signature does not
    /// verify against a then-active key, or a key it adds lacks valid proofs (its
    /// proof-of-possession, its endorsement, its chain to the identity root).
    Inauthentic(String),
    /// Every parcel up to `parcel` is authentic, but `parcel` breaks key permanence, exceeds
    /// its signer's privileges, or adds a key for another user endorsed by a non-admin.
    Unauthorized {
        /// The parcel that oversteps, so a head can tell whether a lift brought it.
        parcel: String,
        /// The refusal, phrased for the user.
        message: String,
    },
}

impl OfficeChainError {
    /// The refusal, phrased for the user.
    pub fn message(&self) -> &str {
        match self {
            OfficeChainError::Inauthentic(message) | OfficeChainError::Unauthorized { message, .. } => message,
        }
    }
}

impl std::fmt::Display for OfficeChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for OfficeChainError {}

impl From<OfficeChainError> for String {
    fn from(error: OfficeChainError) -> String {
        error.message().to_string()
    }
}

/// Verify the office chain from the genesis forward and return the final office state.
///
/// Office history is linear: the chain is walked head → genesis, then verified forward.
/// Each parcel is checked for two things:
///
/// * **Authentic**: signed by a key that was active in the *previous* office state, with
///   every key it adds carrying valid proofs (proof-of-possession, endorsement, chain to the
///   identity root). Introducing a key and signing with it in the same
///   parcel is only valid at the genesis (that self-signature is the trust-on-first-use
///   anchor).
/// * **Authorized**: within its signer's privileges, as of the previous state, so a parcel
///   cannot grant its own author anything. An admin may change anything; everyone else only
///   their own keys (self-service rotation, retirement, device linking), and only an admin's
///   key endorses a key for another user. Key permanence binds admins too: keys are retained
///   forever, their records are immutable, and a revocation is append-once. The genesis needs
///   no such check: it has no earlier state to hold privileges in, so a genesis that breaks
///   these rules is inauthentic instead.
///
/// The walk stops at the first parcel that fails, in chain order, whichever the class. A parcel
/// after a refused one has no valid state to be judged against (an overstep that revoked a key
/// would make an honest later signature with it look forged), so only the first failure can
/// be named truthfully.
///
/// Every caller (the `audit` command, a client adopting or reading the office, and both
/// server heads on admission) runs this one walk, so history a server admitted and history a
/// client verifies offline are held to the same rules.
///
/// # Arguments
/// * `anchor`      - The trust anchor.
/// * `office_head` - The head of the office pallet to verify.
///
/// # Returns
/// * `Ok(OfficeState)`                       - The verified head state.
/// * `Err(OfficeChainError::Inauthentic(_))`  - If the chain does not reach the genesis, or
///                                              a parcel is unreadable or fails its signature
///                                              or endorsement checks.
/// * `Err(OfficeChainError::Unauthorized{..})` - If, before any inauthentic parcel, a parcel
///                                              breaks key permanence, exceeds its signer's
///                                              privileges, or adds a key endorsed beyond
///                                              its authorizer's authority.
pub fn verify_office_chain(anchor: &TrustAnchor, office_head: &str) -> Result<OfficeState, OfficeChainError> {
    use OfficeChainError::{Inauthentic, Unauthorized};

    let mut chain: Vec<String> = Vec::new();
    let mut cursor = office_head.to_string();

    loop {
        chain.push(cursor.clone());

        if cursor == anchor.genesis {
            break;
        }

        let parcel = object_utils::load_parcel(&cursor).map_err(Inauthentic)?;

        match parcel.parents.first() {
            Some(parent) => cursor = parent.clone(),
            None => return Err(Inauthentic(format!(
                "The office chain does not reach the genesis parcel {} (it ends at {}). \
                The warehouse may have been tampered with.",
                anchor.genesis, cursor
            ))),
        }
    }

    chain.reverse();

    let mut previous_state: Option<OfficeState> = None;

    for hash in &chain {
        let state = crate::util::office_utils::read_office_state_of(hash).map_err(Inauthentic)?;

        let lookup_state = previous_state.as_ref().unwrap_or(&state);

        verify_one_signature(hash, lookup_state, "office parcel").map_err(Inauthentic)?;

        // A revoked key must not extend the office chain: the signer has to be
        // active in the state the signature is checked against.
        let signature = sign_utils::load_parcel_signature(hash).map_err(Inauthentic)?
            .expect("verify_one_signature loaded this signature");

        let signer = lookup_state.find_key(&signature.key_id)
            .filter(|key| key.is_active())
            .map(|key| key.operator.clone())
            .ok_or_else(|| Inauthentic(format!(
                "The office parcel {} is signed with key {}, which is revoked at \
                that point. The warehouse may have been tampered with.",
                hash, signature.key_id
            )))?;

        let overstep = |reason: String| Unauthorized {
            parcel: hash.clone(),
            message: format!("The office parcel {} {}", hash, reason),
        };

        verify_new_key_endorsements(previous_state.as_ref(), &state).map_err(|refusal| match refusal {
            KeyRefusal::Proof(reason) => Inauthentic(format!("The office parcel {} {}", hash, reason)),
            KeyRefusal::Scope(reason) => overstep(reason),
        })?;

        if let Some(previous) = &previous_state {
            verify_parcel_privileges(previous, &state, &signer)
                .map_err(|reason| overstep(format!("{}.", reason)))?;
        }

        previous_state = Some(state);
    }

    Ok(previous_state.expect("the chain contains at least the genesis"))
}

/// Check that one office parcel stays within its signer's privileges, as of the state
/// before it: the key invariants bind everyone, and a non-admin may touch only their own keys.
///
/// # Arguments
/// * `previous` - The office state before the parcel.
/// * `current`  - The office state the parcel records.
/// * `signer`   - The operator whose key signed the parcel, resolved in `previous`.
///
/// # Returns
/// * `Ok(())`      - If the change is within the signer's privileges.
/// * `Err(String)` - The reason it is not (phrased to follow "The office parcel X").
fn verify_parcel_privileges(previous: &OfficeState,
                            current: &OfficeState,
                            signer: &str) -> Result<(), String> {
    // Chain invariants that bind admins too: keys are retained forever, their records are
    // immutable, and a revocation is append-once, so nobody quietly un-revokes a key or
    // rewrites its boundary.
    verify_key_permanence(previous, current)?;

    let is_admin = previous.find_user(signer)
        .map(|user| user.role == crate::util::office_utils::Role::Admin)
        .unwrap_or(false);

    if is_admin {
        return Ok(());
    }

    verify_self_service_change(previous, current, signer).map_err(|reason| format!(
        "(signed by \"{}\", not an admin) {}", signer, reason
    ))
}

/// Verify the sigchain endorsements of every key a new office state introduces
/// (§8.5/8.6 of the design). A key is valid only if it carries a proof-of-possession
/// by itself plus an endorsement by an authorizer whose authority covers it: one of
/// the operator's own keys chaining to the identity root (self-endorsement is valid
/// only for the root itself), or an admin's key (an admin-authorized key, scoped to
/// this office).
///
/// # Arguments
/// * `previous` - The office state before the parcel (`None` for the genesis).
/// * `current`  - The office state the parcel records.
///
/// # Returns
/// * `Ok(())`                   - If every new key is properly endorsed.
/// * `Err(KeyRefusal::Proof(_))` - If a key's proofs do not hold up, or, at the genesis, its
///                                endorser's authority does not cover it.
/// * `Err(KeyRefusal::Scope(_))` - If a key is properly signed, but endorsed by someone whose
///                                authority (in the previous state) does not cover it.
///
/// Both are phrased to follow "The office parcel X".
fn verify_new_key_endorsements(previous: Option<&OfficeState>,
                               current: &OfficeState) -> Result<(), KeyRefusal> {
    use crate::util::office_utils::Role;

    // The scope check is the one privilege rule here: every other failure is a proof that
    // does not hold up.
    verify_new_key_proofs(previous, current).map_err(KeyRefusal::Proof)?;

    for key in &current.keys {
        if previous.map_or(false, |state| state.find_key(&key.key_id).is_some()) {
            continue;
        }

        let authorized_by = &key.authorized_by;

        // Present and of a tracked user: the proofs above established both.
        let authorizer = current.find_key(authorized_by).expect("verified by the proofs");

        if authorized_by == &key.key_id || authorizer.operator == key.operator {
            continue;
        }

        // A cross-identity authorization (§8.6): only an admin's key may (the scope of a
        // key-authorization equals the scope of the authorizer's authority).
        let scope = previous.unwrap_or(current);

        let authorizer_is_admin = scope.find_key(authorized_by)
            .and_then(|admin_key| scope.find_user(&admin_key.operator))
            .map(|admin| admin.role == Role::Admin)
            .unwrap_or(false);

        if !authorizer_is_admin {
            let reason = format!(
                "adds key {} for \"{}\" authorized by key {} of \"{}\", who is not \
                an admin here.",
                key.key_id, key.operator, authorized_by, authorizer.operator
            );

            // The genesis has no earlier state to hold a privilege in: it is simply malformed.
            return Err(match previous {
                Some(_) => KeyRefusal::Scope(reason),
                None => KeyRefusal::Proof(reason),
            });
        }
    }

    Ok(())
}

/// Why [`verify_new_key_endorsements`] refused a key.
#[derive(Debug)]
enum KeyRefusal {
    /// Its proofs do not hold up: the parcel is inauthentic.
    Proof(String),
    /// Its endorser had no authority over it: a privilege overstep.
    Scope(String),
}

/// The proofs behind every key a new office state introduces: its identity-root pin, its
/// proof-of-possession, and an endorsement by a then-active key that, for one of the
/// operator's own keys, chains to their identity root. Whose authority covers a
/// cross-identity endorsement is checked by [`verify_new_key_endorsements`]: a privilege
/// refusal after the genesis, and a proof refusal at it.
fn verify_new_key_proofs(previous: Option<&OfficeState>,
                         current: &OfficeState) -> Result<(), String> {
    use crate::util::office_utils::{key_endorsement_payload, key_pop_payload};

    // The identity root pinned in a user record must actually be one of their keys.
    for user in &current.users {
        let root_ok = current.find_key(&user.identity_root)
            .map(|key| key.operator == user.identifier)
            .unwrap_or(false);

        if !root_ok {
            return Err(format!(
                "pins identity root {} for \"{}\", but no such key of theirs is tracked.",
                user.identity_root, user.identifier
            ));
        }
    }

    for key in &current.keys {
        if previous.map_or(false, |state| state.find_key(&key.key_id).is_some()) {
            // Not new. Its id is the hash of the public key endorsed when it was added (a
            // record stores no id; the office reader derives it), and its other fields are
            // held immutable by `verify_key_permanence`, which the chain walk also runs.
            continue;
        }

        let user = current.find_user(&key.operator).ok_or(format!(
            "adds key {} for \"{}\", who has no user record.",
            key.key_id, key.operator
        ))?;

        let root_id = &user.identity_root;
        let (authorized_by, endorsement, pop) =
            (&key.authorized_by, &key.endorsement, &key.proof_of_possession);

        // The proof-of-possession: the key holder signed for this operator themselves.
        let pop_signature = sign_utils::from_hex(pop).map_err(|_| format!(
            "carries a malformed proof-of-possession on key {}.", key.key_id
        ))?;

        let pop_valid = sign_utils::verify_message(
            &key.public_key,
            &key_pop_payload(&key.public_key, &key.operator),
            &pop_signature
        )?;

        if !pop_valid {
            return Err(format!(
                "adds key {} whose proof-of-possession does not verify. The warehouse \
                may have been tampered with.",
                key.key_id
            ));
        }

        // The endorsement: signed by the authorizing key — which must have been
        // active when this parcel introduced the key (a revoked key endorses no one;
        // rotation is fine, since the old key is active in the *previous* state).
        let authorizer = current.find_key(authorized_by).ok_or(format!(
            "adds key {} authorized by key {}, which is not tracked.",
            key.key_id, authorized_by
        ))?;

        let authorizer_active_then = previous
            .and_then(|state| state.find_key(authorized_by))
            .unwrap_or(authorizer)
            .is_active();

        if !authorizer_active_then {
            return Err(format!(
                "adds key {} authorized by key {}, which is revoked at that point.",
                key.key_id, authorized_by
            ));
        }

        let endorsement_signature = sign_utils::from_hex(endorsement).map_err(|_| format!(
            "carries a malformed endorsement on key {}.", key.key_id
        ))?;

        let endorsement_valid = sign_utils::verify_message(
            &authorizer.public_key,
            &key_endorsement_payload(&key.public_key, &key.operator, authorized_by, key.issued_at),
            &endorsement_signature
        )?;

        if !endorsement_valid {
            return Err(format!(
                "adds key {} whose endorsement by key {} does not verify. The \
                warehouse may have been tampered with.",
                key.key_id, authorized_by
            ));
        }

        // The authorization scope (§8.6): whose authority covers this key?
        if authorized_by == &key.key_id {
            // Self-endorsement creates identity from nothing — valid only for the
            // identity root (the trust-on-first-use genesis of the identity).
            if root_id != &key.key_id {
                return Err(format!(
                    "adds key {} as self-endorsed, but only the identity root may be \
                    (theirs is {}).",
                    key.key_id, root_id
                ));
            }
        } else if authorizer.operator == key.operator {
            // A sigchain endorsement by one of the operator's own keys: it must chain
            // to the identity root (same-parcel cycles must not manufacture validity).
            if !chains_to_identity_root(key, root_id, previous, current) {
                return Err(format!(
                    "adds key {} whose endorsement chain does not reach the identity \
                    root {}.",
                    key.key_id, root_id
                ));
            }
        }
    }

    Ok(())
}

/// Whether a key's endorsement chain reaches the operator's identity root, following
/// `authorized_by` links through the operator's own keys. A link that lands on a key
/// already present in the previous state terminates the walk successfully (that key
/// was verified when it was added); a cross-operator link terminates it too (the
/// admin-authorization of that key is verified in its own right). Cycle-safe.
fn chains_to_identity_root(key: &crate::util::office_utils::KeyRecord,
                           root_id: &str,
                           previous: Option<&OfficeState>,
                           current: &OfficeState) -> bool {
    let mut visited: HashSet<&str> = HashSet::new();
    let mut cursor = key;

    loop {
        if cursor.key_id == root_id
            || previous.map_or(false, |state| state.find_key(&cursor.key_id).is_some())
        {
            return true;
        }

        if !visited.insert(&cursor.key_id) {
            return false; // A cycle of new keys endorsing each other.
        }

        let authorized_by = &cursor.authorized_by;

        if authorized_by == &cursor.key_id {
            return cursor.key_id == root_id;
        }

        match current.find_key(authorized_by) {
            Some(next) if next.operator == cursor.operator => cursor = next,
            // A cross-operator link: that key's own admin-authorization check vouches.
            Some(_) => return true,
            None => return false,
        }
    }
}

/// Verify every parcel reachable from a pallet head, stopping at an already-verified
/// one.
///
/// Everything reachable from the trust boundary (the pallet heads recorded at
/// enrollment) is the pre-trust history and may be unsigned. The boundary is exact
/// ancestry — timestamps have second granularity and can be forged, so they never
/// decide a security question.
///
/// `known_verified` makes the walk incremental (the remote's ref update): everything
/// reachable from a committed head was verified when that head was committed, so none of
/// that ancestry is walked. The audit is O(new parcels) — for a merge too, whose second
/// parent rejoins below `known_verified`: [`new_parcels`] excludes the shared ancestry by
/// generation number rather than by stopping at a single hash. The boundary set is only
/// collected when an unsigned parcel actually turns up, so the common all-signed lift never
/// pays for it.
///
/// # Arguments
/// * `head`           - The pallet head to verify from.
/// * `anchor`         - The trust anchor (its boundary separates the legacy history).
/// * `office_state`   - The verified office state (the key registry).
/// * `known_verified` - Heads that already passed this verification, or were signed here
///                      (empty walks everything — the offline `audit`, a clone).
///
/// # Returns
/// * `Ok((usize, usize))`    - The number of verified and legacy (pre-trust) parcels.
/// * `Err(HistoryRefusal)`   - If any parcel fails verification; its `gap` names the boundary
///                             parcel this store lacks when that, not the parcel, is why. A walk
///                             that could not read what it needed is marked `read_failure`.
pub fn verify_pallet_history(head: &str,
                             anchor: &TrustAnchor,
                             office_state: &OfficeState,
                             known_verified: &[&str]) -> Result<(usize, usize), HistoryRefusal> {
    // Phase 1 — discover the parcels to verify: everything reachable from `head` and not
    // from `known_verified`, whose ancestry was verified when it was committed. The walk is
    // bounded by the gap between the two heads, including across a merge (see
    // [`new_parcels`]). Parent edges come from the commit-graph, which is content-addressed
    // (a parcel's hash commits to its parents, so a present record's parents are exactly the
    // real ones) and falls back to decoding the parcel when its record is not yet built — so
    // the discovery set is always complete, and on a graph-warm warehouse it is found
    // without decoding a single parcel body. The bodies are proven present in phase 2
    // instead, in parallel.
    let parcels = new_parcels_with_edges(head, (!known_verified.is_empty()).then_some(known_verified))?.0;

    // Phase 2 — verify every parcel's signature. Each check is independent: it runs
    // against the immutable `office_state` key registry, not against any neighbour, so the
    // parcels fan out across the cores. The work per parcel is a signature-sidecar read, a
    // parcel-body read and an ed25519 verify; the reads share the object caches, so the
    // scaling is real but sub-linear (measured ~2.4x on 18 cores, read-bound, not the
    // near-linear a pure-CPU loop would give — see docs/PARALLELIZATION_PLAN.md). The
    // decisions that need a shared, lazily-built reachability closure — is an unsigned or
    // revoked-key parcel inside its boundary? — are deferred (a verdict names the
    // boundary) so this loop stays lock-free.
    let verdicts = verify_signatures(&parcels, office_state);

    // Phase 3 — resolve the deferred boundary decisions and tally, walking the verdicts in
    // discovery (breadth-first) order so the first failure reported is exactly the one the
    // serial walk would have reported. The boundary closures are still built lazily, so an
    // all-signed, all-active history never pays to collect them.
    let mut legacy_parcels: Option<HashSet<String>> = None;

    // The first reference the trust-boundary walk found absent, if any (`None` once the walk
    // has run and crossed no gap at all). Read off the very same walk that builds
    // `legacy_parcels` — never a separate pre-scan — exactly as the distrust arm below reads
    // `unresolved_head` off `DistrustBoundaryMemo`'s own walk. A gap here means the walk's
    // negative on some other parcel is unproven rather than false: the difference between
    // naming a missing object and accusing an operator of tampering.
    //
    // Deliberately ANY absent reference the walk met — a head or an interior ancestor alike,
    // never narrowed to interior-only. A present-only reachability walk answers "is P reachable
    // from the boundary", and a positive is always sound (only content-committed `.parents` are
    // followed, so a present record's parents are exactly the real ones). A negative is sound
    // only if the walk met NO absent reference at all: once it crosses one absent head b', this
    // store holds nothing that bounds `anc*(b')` — a present head only ever tells you about its
    // *own* ancestry. There is no such thing as an "unrelated" absent head here: relatedness to
    // P is precisely the question a boundary walk exists to answer, so an absent head can
    // absolutely be the one thing that would have put P inside the boundary. Narrowing this to
    // interior-only gaps was tried (PR #121 round 1) and reverted (round 2): it produced false
    // tampering accusations from two ordinary sequences of shipped commands — an ordinary
    // `office enroll` + `pallet create` + `lift` to a fresh remote (the trust arm, server-side), and
    // an ordinary `office retire` + `pallet create` + `lift` that never lifts the retired signer's
    // side pallet (the distrust arm) — see `crates/forklift/tests/remote.rs`'s
    // `enrolling_then_lifting_to_a_fresh_remote_with_an_unlifted_other_pallet_refuses_instead_of_accusing`
    // and `retiring_a_signer_then_lifting_only_a_replacement_pallet_refuses_instead_of_accusing`,
    // both of which reproduce the false accusation on the *origin* server, not merely a clone.
    let mut boundary_gap: Option<String> = None;

    // Per revoked key: the parcels its distrust boundary vouches for (lazy — an
    // all-active-keys history never pays for it). Shared with the query engine's
    // `signer.boundary` predicate via [`DistrustBoundaryMemo`]; this use is unmodified.
    let mut distrust_boundaries = DistrustBoundaryMemo::new();

    let mut verified = 0usize;
    let mut legacy = 0usize;

    for (index, verdict) in verdicts.into_iter().enumerate() {
        let hash = &parcels[index];

        match verdict.map_err(HistoryRefusal::verdict)? {
            Verdict::Verified => verified += 1,

            // No signature, or a signature by a key the office does not know: after a
            // re-genesis (§8.7) the prior chain's keys are gone, and the parcels they
            // signed are *attested* by the new anchor's boundary pin rather than verified
            // — the same standing as unsigned pre-trust history. Outside the boundary,
            // both are what they always were: tampering — unless this store cannot actually
            // resolve the boundary itself (a gap below), in which case it is not this
            // store's place to accuse.
            Verdict::TrustBoundary(reason) => {
                if legacy_parcels.is_none() {
                    // `_noting_gaps` rather than the plain wrapper: the wrapper is literally
                    // `Ok(collect_reachable_present_noting_gaps(heads)?.0)`, so the gap list
                    // is already computed by the walk this arm was running anyway and would
                    // otherwise be thrown away. Keeping it costs nothing extra. Do NOT change
                    // the wrapper itself — `gc_utils` and `prune_utils` call it too, and
                    // neither wants this behaviour.
                    let (reachable, gaps) = collect_reachable_present_noting_gaps(&anchor.boundary)?;

                    legacy_parcels = Some(reachable);
                    boundary_gap = gaps.into_iter().next();
                }

                if legacy_parcels.as_ref().unwrap().contains(hash) {
                    legacy += 1;
                } else if let Some(missing) = boundary_gap.as_ref() {
                    // The same discipline the distrust arm applies ~15 lines below: a
                    // present-only reachability walk only ever *grows* as more ancestry
                    // becomes present, so a positive is trustworthy and a negative is
                    // ambiguous once the walk has crossed a gap. With one standing, this
                    // parcel's absence from the closure may be exactly what the missing
                    // object would have explained — so this is a store that cannot answer,
                    // not a warehouse that was tampered with.
                    //
                    // Still a refusal (nothing here is silenced), and it does not promise
                    // that supplying `missing` alone resolves it: `gaps` can hold more than
                    // one entry and only the first is ever named here, so an interior
                    // ancestor further back may be missing too — supplying this one object
                    // can simply uncover the next gap on a rerun.
                    return Err(HistoryRefusal::gap(BoundaryGap::Trust { missing: missing.clone() }, format!(
                        "Parcel {} {}, and this store cannot resolve the trust boundary \
                        (boundary parcel {} is not present locally), so it cannot tell \
                        whether this parcel predates trust or was stacked after it. Verify \
                        against a store with the full history.",
                        hash,
                        match &reason {
                            TrustBoundaryReason::Unsigned =>
                                "carries no signature".to_string(),
                            TrustBoundaryReason::UnknownKey(key_id) => format!(
                                "is signed with key {}, which is not tracked in the office",
                                key_id
                            ),
                        },
                        missing
                    )));
                } else {
                    return Err(HistoryRefusal::verdict(match reason {
                        TrustBoundaryReason::Unsigned => format!(
                            "Parcel {} was stacked after trust was established but carries no \
                            signature. The warehouse may have been tampered with.",
                            hash
                        ),
                        TrustBoundaryReason::UnknownKey(key_id) => format!(
                            "Parcel {} is signed with key {}, which is not tracked in the \
                            office. The warehouse may have been tampered with.",
                            hash, key_id
                        ),
                    }));
                }
            }

            // A revoked key's signature is vouched only within the revocation's distrust
            // boundary (§8.11): exact ancestry, like the trust boundary — a forged or
            // shifted clock changes nothing.
            Verdict::DistrustBoundary(key_id) => {
                let key = office_state.find_key(&key_id)
                    .expect("phase 2 verified this parcel against this key");

                let vouched = distrust_boundaries.vouched(key, hash)?;

                // `vouched`'s reachability walk only ever *grows* as more of the boundary's
                // ancestry becomes present, so a `true` here is trustworthy regardless of what
                // else is missing — this is the common case (this store, this server, has
                // everything that actually matters for this parcel) and it must stay cheap: an
                // ordinary lift must not pay a presence check, let alone fail one, just because
                // a revocation's boundary also names some unrelated pallet's head this store
                // was never going to have (that pallet may simply never be lifted).
                //
                // A `false` is the ambiguous case: it might be the genuine "outside the
                // boundary" verdict, or it might be an artifact of a gap this store's own walk
                // ran into — a missing boundary head, or a missing interior ancestor behind
                // one — either of which could have been exactly what would have vouched for
                // this parcel. `unresolved_head` names that gap when there is one; it reads off
                // the very walk `vouched` just ran, so this never re-derives reachability or
                // pays for a second pass.
                if !vouched {
                    if let Some(missing) = distrust_boundaries.unresolved_head(key)? {
                        let gap = BoundaryGap::Distrust { key_id: key_id.clone(), missing: missing.clone() };

                        return Err(HistoryRefusal::gap(gap, format!(
                            "Parcel {} is signed with revoked key {}: this store cannot \
                            resolve the revocation's distrust boundary (boundary parcel {} is \
                            not present locally), so it cannot tell whether this parcel \
                            predates the revocation or the key kept signing after it. Verify \
                            against a store with the full history.",
                            hash, key_id, missing
                        )));
                    }

                    return Err(HistoryRefusal::verdict(format!(
                        "Parcel {} is signed with key {}, which is revoked \
                        ({}), and the parcel is outside the revocation's \
                        distrust boundary. The warehouse may have been tampered \
                        with — or the key's holder kept signing after the \
                        revocation.",
                        hash,
                        key_id,
                        key.revocation_reason
                            .map(|reason| reason.as_str())
                            .unwrap_or("no recorded reason")
                    )));
                }

                verified += 1;
            }
        }
    }

    Ok((verified, legacy))
}

/// Why [`verify_pallet_history`] refused: the message it always gave, and — when what this store
/// lacks, not the parcel itself, is the reason — the boundary it could not resolve, so a take can
/// fetch that boundary and judge once more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryRefusal {
    /// The refusal, as `audit` prints it.
    pub message: String,

    /// The boundary this store could not resolve, if that is why.
    pub gap: Option<BoundaryGap>,

    /// No verdict was reached: the walk could not read what it needed (the message says what).
    /// A parcel whose signature or body does not check out is a verdict, not this.
    pub read_failure: bool,
}

/// A boundary [`verify_pallet_history`] could not resolve, because its walk crossed a parcel this
/// store does not hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoundaryGap {
    /// The trust anchor's boundary; `missing` is the first absent parcel its walk met.
    Trust { missing: String },

    /// The distrust boundary of the revoked key `key_id`; `missing` as above.
    Distrust { key_id: String, missing: String },
}

impl HistoryRefusal {
    fn gap(gap: BoundaryGap, message: String) -> HistoryRefusal {
        HistoryRefusal { message, gap: Some(gap), read_failure: false }
    }

    fn verdict(message: String) -> HistoryRefusal {
        HistoryRefusal { message, gap: None, read_failure: false }
    }
}

/// A read the walk could not complete, raised through `?`.
impl From<String> for HistoryRefusal {
    fn from(message: String) -> HistoryRefusal {
        HistoryRefusal { message, gap: None, read_failure: true }
    }
}

impl From<HistoryRefusal> for String {
    fn from(refusal: HistoryRefusal) -> String {
        refusal.message
    }
}

/// The verdict [`verify_signatures`] reaches for one parcel — the independent, parallel
/// part of the audit. The ed25519 signature check (the expensive part) is already done; a
/// verdict that names a boundary defers a *reachability* test to the serial phase 3,
/// because that test reads a shared, lazily-built closure.
enum Verdict {
    /// A valid signature by a key active in the office at audit time. Verified outright.
    Verified,

    /// No usable signature (none, or one by an untracked key) — resolve against the trust
    /// boundary as possible pre-trust history.
    TrustBoundary(TrustBoundaryReason),

    /// A valid signature by a *revoked* key — resolve against that revocation's distrust
    /// boundary. Carries the revoked key's id.
    DistrustBoundary(String),
}

/// Why a parcel fell to the trust-boundary check, kept so phase 3 reproduces the exact
/// message the serial walk gave.
enum TrustBoundaryReason {
    /// The parcel carries no signature at all.
    Unsigned,

    /// The parcel is signed by a key the office does not track (carries the key id).
    UnknownKey(String),
}

/// Verify each parcel's signature, fanning the work across the cores once there is enough
/// of it. Returns one verdict per parcel, positionally aligned with `parcels`, so the
/// caller resolves boundary decisions and reports failures in discovery order. A hard
/// failure (a signature that does not verify, an unreadable sidecar) is the `Err` in that
/// parcel's slot.
fn verify_signatures(parcels: &[String],
                     office_state: &OfficeState) -> Vec<Result<Verdict, String>> {
    // Below this many parcels the ed25519 verifies are cheaper than the threads that would
    // share them; stay on the calling thread.
    const PARALLEL_THRESHOLD: usize = 256;

    if parcels.len() < PARALLEL_THRESHOLD {
        return parcels.iter().map(|hash| classify_signature(hash, office_state)).collect();
    }

    // See `fanout_utils::fanout_map` for the fan-out idiom (chunking, worker count, and the
    // storage-scope re-entry every worker needs — the server head serves more than one
    // warehouse).
    fanout_utils::fanout_map(parcels, |hash| classify_signature(hash, office_state))
}

/// The forge-proof trust classification of one parcel's signature — the only primitive a
/// caller may label "verified". It verifies the actual Ed25519 signature over the parcel
/// hash against the office's recorded public key AND checks the key's active/revoked
/// status; the weaker resolvers (a sidecar `key_id` looked up without verifying, or the
/// parcel body's self-declared operator) are attribution sugar, never verification.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SignatureTrust {
    /// A valid signature by a key active in the office.
    Verified {
        /// The signing key's id.
        key_id: String,
    },

    /// A valid signature by a *revoked* key: cryptographically sound, but the key has been
    /// retired — never to be flattened into `Verified`.
    SignedRevoked {
        /// The revoked signing key's id.
        key_id: String,
    },

    /// The parcel carries no signature at all.
    Unsigned,

    /// Signed, but by a key the office does not track.
    UnknownKey {
        /// The claimed signing key's id (untracked, so unverifiable).
        key_id: String,
    },
}

/// Classify one parcel's trust against the office key registry: verify the signature (when
/// one exists and its key is tracked) and report the key's active/revoked status. Also
/// proves the parcel body itself is present and decodable — a missing or corrupt parcel is
/// the `Err`, never a soft verdict. Everything it touches is either immutable
/// (`office_state`) or a per-object read through the shared, already-thread-safe object
/// caches, so it is safe to run on many threads at once.
///
/// A caller that has already loaded the parcel body (a history walk resolving identities
/// for parcels it just decoded) should use [`classify_signature_trust`] instead: parcels
/// deliberately bypass the shared read cache, so the presence proof here is a second full
/// disk read + decode, not a cache hit.
pub fn classify_parcel_trust(hash: &str, office_state: &OfficeState) -> Result<SignatureTrust, String> {
    let trust = classify_signature_trust(hash, office_state)?;

    // Prove the parcel's body is present and decodable — for the audit, the guarantee
    // phase 1 used to give by decoding each parcel for its parents (it now reads parents
    // from the graph instead). Kept after the signature check so a bad signature still
    // fails ahead of a missing body.
    object_utils::load_parcel(hash)?;

    Ok(trust)
}

/// The signature half of [`classify_parcel_trust`]: classify the parcel's signature without
/// re-reading the parcel body. For callers that already hold (and so already proved) the
/// body — the presence guarantee is theirs to keep.
pub fn classify_signature_trust(hash: &str, office_state: &OfficeState) -> Result<SignatureTrust, String> {
    let trust = match sign_utils::load_parcel_signature(hash)? {
        None => SignatureTrust::Unsigned,

        Some(signature) if office_state.find_key(&signature.key_id).is_none() => {
            SignatureTrust::UnknownKey { key_id: signature.key_id }
        }

        Some(signature) => {
            verify_one_signature(hash, office_state, "parcel")?;

            let key = office_state.find_key(&signature.key_id)
                .expect("verify_one_signature found this key");

            if key.is_active() {
                SignatureTrust::Verified { key_id: signature.key_id }
            } else {
                SignatureTrust::SignedRevoked { key_id: signature.key_id }
            }
        }
    };

    Ok(trust)
}

/// Classify one parcel's signature against the office key registry — the body of the
/// parallel phase, mapping the shared [`classify_parcel_trust`] onto the audit's
/// boundary-resolution verdicts.
fn classify_signature(hash: &str, office_state: &OfficeState) -> Result<Verdict, String> {
    Ok(match classify_parcel_trust(hash, office_state)? {
        SignatureTrust::Verified { .. } => Verdict::Verified,
        SignatureTrust::SignedRevoked { key_id } => Verdict::DistrustBoundary(key_id),
        SignatureTrust::Unsigned => Verdict::TrustBoundary(TrustBoundaryReason::Unsigned),
        SignatureTrust::UnknownKey { key_id } => {
            Verdict::TrustBoundary(TrustBoundaryReason::UnknownKey(key_id))
        }
    })
}

/// Verified office chains, remembered per `(warehouse, anchor, office head)`.
static VERIFIED_OFFICE_CHAINS: OnceLock<Mutex<OfficeChainMemo>> = OnceLock::new();

/// How many verified chains to remember before evicting the least-recently-used one to
/// make room for a new key. A hosting server can carry many more than sixteen tenants, so
/// this bound is about keeping the memo small, not about how many warehouses are expected.
const MAX_MEMOIZED_OFFICE_CHAINS: usize = 16;

/// One remembered chain verification, tagged with when it was last touched.
///
/// "When" is a logical clock local to the memo, not a wall-clock timestamp: every hit and
/// every insert draws the next tick from a counter that only ever increases, so entries can
/// be ordered by recency without depending on the system clock.
struct MemoEntry {
    state: OfficeState,
    last_used: u64,
}

/// A bounded memo of verified office chains, keyed by `(warehouse, anchor, office head)`
/// (see [`office_chain_key`]).
///
/// At capacity, an insert of a new key evicts the single least-recently-used entry rather
/// than clearing the whole memo. A server hosts as many warehouses as it has tenants, and
/// each lands its own key here — clearing everything on the seventeenth distinct key would
/// evict every other tenant's verified state along with it, degrading past the point of
/// having no memo at all: constant recompute, plus the lock contention of a map that never
/// gets to stay warm. Evicting one entry keeps the other tenants' memoized state intact.
struct OfficeChainMemo {
    entries: HashMap<String, MemoEntry>,
    clock: u64,
}

impl OfficeChainMemo {
    fn new() -> Self {
        OfficeChainMemo { entries: HashMap::new(), clock: 0 }
    }

    // Only the tests below inspect size directly; production code only ever hits, inserts
    // or clears the memo.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.clock = 0;
    }

    /// Look up `key`, marking it most-recently-used on a hit.
    fn get(&mut self, key: &str) -> Option<OfficeState> {
        let tick = self.next_tick();
        let entry = self.entries.get_mut(key)?;
        entry.last_used = tick;

        Some(entry.state.clone())
    }

    /// Remember `state` under `key`, marking it most-recently-used.
    ///
    /// If the memo is already at [`MAX_MEMOIZED_OFFICE_CHAINS`] and `key` is not already
    /// present, the entry with the smallest `last_used` is evicted first to make room — a
    /// linear scan over at most sixteen entries, which is cheap enough not to need anything
    /// fancier (a heap, an intrusive list) at this size.
    fn insert(&mut self, key: String, state: OfficeState) {
        if self.entries.len() >= MAX_MEMOIZED_OFFICE_CHAINS && !self.entries.contains_key(&key) {
            let lru_key = self.entries.iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(k, _)| k.clone());

            if let Some(lru_key) = lru_key {
                self.entries.remove(&lru_key);
            }
        }

        let tick = self.next_tick();
        self.entries.insert(key, MemoEntry { state, last_used: tick });
    }

    fn next_tick(&mut self) -> u64 {
        let tick = self.clock;
        self.clock += 1;

        tick
    }
}

/// An office verified against the trust anchor, with what it was read from: the office head
/// and the trust file's bytes. A write judged against the office is conditioned on both through
/// [`VerifiedOffice::preconditions`], so it lands only while the office it was judged against
/// still stands.
#[derive(Clone)]
pub struct VerifiedOffice {
    /// The verified office state.
    pub state: OfficeState,

    /// The office still at the head it was read from, the anchor still the bytes it was decoded
    /// from.
    preconditions: RefPreconditions,
}

impl VerifiedOffice {
    /// The preconditions of a write judged against this office: the office still at its head,
    /// the anchor still these bytes.
    pub fn preconditions(&self) -> RefPreconditions {
        self.preconditions.clone()
    }

    /// The office head this state was verified at (`None` for an office never read from disk).
    pub fn head(&self) -> Option<&str> {
        match &self.preconditions.office {
            OfficePrecondition::At(Some(head)) => Some(head),
            _ => None,
        }
    }

    /// An office state that was never read from disk, for tests that judge writes against an
    /// in-memory office: its writes condition on nothing.
    #[cfg(test)]
    pub(crate) fn unconditioned(state: OfficeState) -> VerifiedOffice {
        VerifiedOffice { state, preconditions: RefPreconditions::NONE }
    }
}

impl std::ops::Deref for VerifiedOffice {
    type Target = OfficeState;

    fn deref(&self) -> &OfficeState {
        &self.state
    }
}

/// This warehouse's office, verified against its trust anchor. A meta pallet's writes
/// are judged against the verified office, never the unverified local read.
pub fn verified_local_office() -> Result<VerifiedOffice, String> {
    Ok(verified_local_trust()?
        .ok_or("Trust is not established for this warehouse yet; use \"forklift office enroll\" first.".to_string())?
        .1)
}

/// This warehouse's trust anchor and its office verified against it, read together so the office
/// is judged against exactly the anchor returned — or `None` when no anchor is held.
pub fn verified_local_trust() -> Result<Option<(TrustAnchor, VerifiedOffice)>, String> {
    let Some((anchor, anchor_bytes)) = office_utils::read_trust_anchor_with_bytes()? else {
        return Ok(None);
    };
    let head = pallet_utils::get_meta_pallet_head(office_utils::OFFICE_PALLET_NAME)?
        .ok_or("The office pallet is unborn; use \"forklift office enroll\" first.".to_string())?;

    let state = verify_office_chain_memoized(&anchor, &head)?;

    let office = VerifiedOffice {
        state,
        preconditions: RefPreconditions {
            office: OfficePrecondition::At(Some(head)),
            anchor: AnchorPrecondition::Bytes(anchor_bytes),
            pallets: PalletsPrecondition::NotConsumed,
        },
    };

    Ok(Some((anchor, office)))
}

/// [`verify_office_chain`], remembered for the life of the process.
///
/// A server head runs the chain verification on *every* trusted ref update — including lifts
/// of ordinary pallets, which only consume the resulting key registry and do not move the
/// office head. That work is pure: the office objects are content-addressed and immutable, so
/// the same head under the same anchor always verifies to the same state. Memoizing it turns
/// an O(office history) signature walk per lift into one per office head.
///
/// **The warehouse root is part of the key on purpose.** Without it a multi-warehouse server
/// could hand a verified state to a warehouse whose object store does not hold that chain at
/// all — the same tenant-boundary mistake a scratch shared across warehouses would make. The
/// whole anchor is folded in too, not just its genesis: a re-genesis changes the boundary.
///
/// Use this from a long-lived head. The `audit` command verifies once and exits, so it calls
/// [`verify_office_chain`] directly and never consults a memo.
///
/// The memo (see [`OfficeChainMemo`]) holds at most [`MAX_MEMOIZED_OFFICE_CHAINS`] entries and
/// evicts the least-recently-used one to make room for a new key, so a busy tenant's state
/// stays warm and only idle tenants age out.
pub fn verify_office_chain_memoized(
    anchor: &TrustAnchor,
    office_head: &str,
) -> Result<OfficeState, OfficeChainError> {
    let key = office_chain_key(anchor, office_head);
    let memo = VERIFIED_OFFICE_CHAINS.get_or_init(|| Mutex::new(OfficeChainMemo::new()));

    if let Some(state) = lock_memo(memo).get(&key) {
        return Ok(state);
    }

    // Verified outside the lock: a slow chain must not block the other warehouses.
    let state = verify_office_chain(anchor, office_head)?;

    lock_memo(memo).insert(key, state.clone());

    Ok(state)
}

/// How a head answers an office lift whose chain did not verify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfficeLiftRefusal {
    /// The lift brought a parcel that oversteps its signer's privileges: a `403`.
    Overstep(String),
    /// The chain is inauthentic, or the overstep is in the office already held: a `422`.
    Unprocessable(String),
}

/// Classify an office lift's refusal as the lifter's own overstep or not.
///
/// The walk runs from the genesis, so the unauthorized parcel it names may be in the history
/// the warehouse already held rather than in the lifted segment. Office history is linear, so
/// the held head's first-parent chain is exactly that history. A parcel found in it (or a
/// chain that cannot be read to tell) is not this lift's doing, and the refusal says so. Any
/// other parcel came with the lift, including across a regenesis, where the held office
/// grew from an earlier genesis and shares no parcels with the new chain.
///
/// # Arguments
/// * `error`     - The lifted head's refusal.
/// * `held_head` - The office head before the lift (`None` for the first office lift).
pub fn classify_office_lift_refusal(error: OfficeChainError, held_head: Option<&str>) -> OfficeLiftRefusal {
    match error {
        OfficeChainError::Inauthentic(message) => OfficeLiftRefusal::Unprocessable(message),
        OfficeChainError::Unauthorized { parcel, message } => {
            if held_head.map_or(false, |head| office_history_holds(head, &parcel)) {
                OfficeLiftRefusal::Unprocessable(format!(
                    "{} That parcel is already in this warehouse's office, so this lift did not \
                    bring it, and no office lift can verify on top of it.",
                    message
                ))
            } else {
                OfficeLiftRefusal::Overstep(message)
            }
        }
    }
}

/// Whether `parcel` is in the first-parent history of the office head `head`. A parcel that
/// cannot be read counts as held: nothing can then be pinned on the lift.
fn office_history_holds(head: &str, parcel: &str) -> bool {
    let mut cursor = head.to_string();

    loop {
        if cursor == parcel {
            return true;
        }

        match object_utils::load_parcel(&cursor) {
            Ok(loaded) => match loaded.parents.first() {
                Some(parent) => cursor = parent.clone(),
                None => return false,
            },
            Err(_) => return true,
        }
    }
}

/// The memo key: which warehouse, under which anchor, at which office head.
pub(crate) fn office_chain_key(anchor: &TrustAnchor, office_head: &str) -> String {
    format!(
        "{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
        crate::globals::forklift_root().to_string_lossy(),
        anchor.genesis,
        anchor.enabled_at,
        anchor.boundary.join(","),
        anchor.prior_genesis.as_deref().unwrap_or(""),
        anchor.adopts.as_deref().unwrap_or(""),
        office_head
    )
}

/// Take the memo, recovering from a poisoned lock rather than failing.
///
/// A poisoned mutex means some thread panicked while holding it — an internal fault, and
/// never the caller's doing. Both server heads map a failure of the memoized verification to a
/// client-side refusal (`422`, or `403` for a privilege overstep), so returning an error here
/// would tell a client its lift was invalid
/// because the server had a bug. And there is nothing to protect: this is a cache of results
/// that can always be recomputed. So the poison is cleared, whatever was in the memo is
/// dropped, and the next verification simply repopulates it.
fn lock_memo(
    memo: &Mutex<OfficeChainMemo>,
) -> std::sync::MutexGuard<'_, OfficeChainMemo> {
    match memo.lock() {
        Ok(chains) => chains,
        Err(poisoned) => {
            memo.clear_poison();

            let mut chains = poisoned.into_inner();
            chains.clear();

            chains
        }
    }
}

/// Reachable from `head`.
const FRESH: u8 = 1;

/// Reachable from `known_verified` — already audited when that head was committed.
const KNOWN: u8 = 2;

/// Every parcel reachable from `head` that is **not** reachable from `known_verified`: the
/// new segment of a lift, in breadth-first order from `head`.
///
/// This is the one ancestry walk the audit needs, and its cost is the *gap* between the two
/// heads — not the length of history. The lever is the commit-graph's generation numbers
/// (§B): a parcel's generation is one more than its parents' maximum, so a parent's
/// generation is strictly less than its child's. Visiting parcels in descending generation
/// order therefore guarantees that when a parcel is reached, every parcel that could reach
/// *it* has already been visited — so its "reachable from head" / "reachable from the
/// verified head" marks are final on arrival, and the walk can stop the moment no
/// unvisited parcel is still marked fresh.
///
/// It replaces two walks that were both O(history) on every lift:
///
/// * `collect_reachable(known_verified)`, which decoded every parcel body in the verified
///   head's ancestry just to build a prune set; and
/// * a breadth-first discovery that stopped only at the *exact* `known_verified` hash. That
///   is the right frontier for a linear lift, where the verified head is the unique
///   boundary — but a merge's boundary is the merge-base *set*, which one hash cannot
///   express, so a merge walked below the fork point and re-verified ancestry that was
///   audited when `known_verified` was committed.
///
/// Excluding that ancestry is sound on exactly the invariant the incremental audit already
/// rests on: everything reachable from a committed head was verified when it was committed.
/// A creation (`known_verified: None`) still walks the whole history.
///
/// # Arguments
/// * `head`           - The parcel whose new ancestry to collect.
/// * `known_verified` - A head already known good (`None` collects everything).
///
/// # Returns
/// * `Ok(Vec<String>)` - The new parcels, breadth-first from `head`.
/// * `Err(String)`     - If a parcel is in neither the commit-graph nor the object store.
pub fn new_parcels(head: &str, known_verified: Option<&str>) -> Result<Vec<String>, String> {
    Ok(new_parcels_with_edges(head, known_verified.as_ref().map(std::slice::from_ref))?.0)
}

/// [`new_parcels`], also returning the parent edges the walk read on its way.
///
/// The edges are not a convenience: `graph_utils::parents` falls back to decoding the parcel when
/// the commit-graph has no record, so a caller that needs them too — the closure audit, whose
/// candidate bases *are* the parents — would otherwise decode every parcel of the segment twice.
/// Returning them makes the audit's ordering and its candidate bases share one view of who a
/// parent is, which is the property the ordering's soundness actually rests on.
fn new_parcels_with_edges(head: &str, known_verified: Option<&[&str]>)
                          -> Result<(Vec<String>, HashMap<String, Vec<String>>), String> {
    let fresh: Option<HashSet<String>> = match known_verified {
        None => None,
        Some(bounds) if bounds.contains(&head) => return Ok((Vec::new(), HashMap::new())),
        Some(bounds) => Some(fresh_frontier(head, bounds)?),
    };

    // Breadth-first from `head`, so the order — and therefore the first failure reported — is
    // exactly what the unbounded walk produced. Callers that need a different order sort this;
    // `verify_parcel_closure_with` does, but only for the incremental gate, precisely so that the
    // unbounded audit's reported failure stays the one this order picks.
    let mut order: Vec<String> = Vec::new();
    let mut edges: HashMap<String, Vec<String>> = HashMap::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();

    queue.push_back(head.to_string());

    while let Some(hash) = queue.pop_front() {
        if fresh.as_ref().is_some_and(|fresh| !fresh.contains(&hash)) {
            continue;
        }

        if !visited.insert(hash.clone()) {
            continue;
        }

        let parents = graph_utils::parents(&hash)?;

        for parent in &parents {
            queue.push_back(parent.clone());
        }

        edges.insert(hash.clone(), parents);
        order.push(hash);
    }

    Ok((order, edges))
}

/// The set behind [`new_parcels`]: parcels reachable from `head` but from none of `bounds`.
///
/// A max-heap on the generation number drives the walk, so parcels are settled newest-first
/// and each one's marks are final when it is popped (every parcel that could mark it has a
/// strictly greater generation, hence was popped earlier). The walk stops as soon as nothing
/// fresh is left pending: whatever remains is reachable from a bound, and so is everything
/// behind it.
///
/// A parcel reached only as known history that can be read neither from the commit-graph nor
/// from the store is left out rather than failing the walk: it vouches for nothing, so what it
/// would have bounded is walked as fresh instead, and a parcel the fresh side reaches still fails
/// on the same read.
fn fresh_frontier(head: &str, bounds: &[&str]) -> Result<HashSet<String>, String> {
    let mut walk = Frontier::default();

    walk.mark(head, FRESH)?;

    for bound in bounds {
        walk.mark(bound, KNOWN)?;
    }

    // Nothing fresh left pending means nothing new can be discovered: every parcel still on
    // the heap is reachable from a bound, and so is all of its ancestry.
    while walk.fresh_pending > 0 {
        let Some((_, hash)) = walk.heap.pop() else {
            break;
        };

        if !walk.settled.insert(hash.clone()) {
            continue;
        }

        let marks = walk.marks[&hash];

        if marks & FRESH != 0 {
            walk.fresh_pending -= 1;
        }

        // Reachable from a bound: none of it is new, and neither is anything behind it.
        let inherited = if marks & KNOWN != 0 {
            KNOWN
        } else {
            walk.fresh.insert(hash.clone());
            FRESH
        };

        for parent in walk.parents_of[&hash].clone() {
            walk.mark(&parent, inherited)?;
        }
    }

    Ok(walk.fresh)
}

/// The bookkeeping of [`fresh_frontier`].
#[derive(Default)]
struct Frontier {
    /// The `FRESH`/`KNOWN` bits per parcel. Final once the parcel is settled.
    marks: HashMap<String, u8>,

    /// Parent edges, read from the commit-graph as each parcel is first seen.
    parents_of: HashMap<String, Vec<String>>,

    /// Unsettled parcels, newest generation first.
    heap: BinaryHeap<(u32, String)>,

    /// Parcels already popped; their marks will not change again.
    settled: HashSet<String>,

    /// The answer: fresh and not known.
    fresh: HashSet<String>,

    /// How many unsettled parcels carry `FRESH` — the walk's reason to keep going.
    fresh_pending: usize,
}

impl Frontier {
    /// Add `flag` to `hash`, enqueueing it under its generation the first time it is seen.
    fn mark(&mut self, hash: &str, flag: u8) -> Result<(), String> {
        let before = self.marks.get(hash).copied();

        self.marks.insert(hash.to_string(), before.unwrap_or(0) | flag);

        // Newly fresh: one more parcel worth walking for. A parcel can only gain marks
        // before it settles, so this never counts a settled parcel.
        if flag == FRESH && before.unwrap_or(0) & FRESH == 0 {
            self.fresh_pending += 1;
        }

        if before.is_none() {
            let node = match graph_utils::node(hash) {
                Ok(node) => node,
                Err(_) if flag == KNOWN => {
                    self.marks.remove(hash);
                    return Ok(());
                }
                Err(e) => return Err(e),
            };

            self.parents_of.insert(hash.to_string(), node.parents);
            self.heap.push((node.generation, hash.to_string()));
        }

        Ok(())
    }
}

/// Collect every parcel reachable from the given heads (the heads included).
///
/// The audit no longer uses this — see [`new_parcels`], which is bounded. It remains the
/// right primitive for the callers that genuinely need the whole set (bundle building, pack
/// reachability, `deliver`), and it decodes parcel bodies on purpose there: those callers go
/// on to read the objects, so a commit-graph record would not save them the read *and* would
/// not prove the object is present.
///
/// # Arguments
/// * `heads` - The starting parcel hashes.
///
/// # Returns
/// * `Ok(HashSet<String>)` - The reachable parcel hashes.
/// * `Err(String)`         - If a parcel could not be read.
pub fn collect_reachable(heads: &[String]) -> Result<HashSet<String>, String> {
    let mut queue: VecDeque<String> = heads.iter().cloned().collect();
    let mut reachable: HashSet<String> = HashSet::new();

    while let Some(hash) = queue.pop_front() {
        if !reachable.insert(hash.clone()) {
            continue;
        }

        for parent in object_utils::load_parcel(&hash)?.parents {
            queue.push_back(parent);
        }
    }

    Ok(reachable)
}

/// Collect every *locally present* parcel reachable from the given heads (the heads included),
/// and report every reference the walk found absent — a head itself, or a parent edge pointing
/// at an object this store doesn't have — in the order the walk first met them. A head that
/// does not exist here contributes nothing: a trust boundary may name heads this warehouse never
/// had (enrollment includes the remote's heads).
///
/// A head-only presence pre-scan is not enough to know whether a walk's answer is complete:
/// every one of a key's boundary heads can be present while the walk still runs into a gap
/// *behind* one of them (an interior ancestor this store never fetched or has since lost),
/// and a plain "not reachable" and "not present" look identical from outside the walk. This
/// variant makes that distinction the walk's own business, so a caller — [`DistrustBoundaryMemo`]
/// — can tell "genuinely outside the boundary" from "this store cannot answer" without
/// re-deriving reachability from scratch or trusting a pre-scan that only ever looked at the
/// starting heads.
///
/// # Arguments
/// * `heads` - The starting parcel hashes.
///
/// # Returns
/// * `Ok((reachable, gaps))` - `reachable` is the present, reachable set; `gaps` is every
///   absent hash the walk actually referenced, deduplicated, first-encountered order.
/// * `Err(String)`           - If a present parcel could not be read.
pub fn collect_reachable_present_noting_gaps(
    heads: &[String],
) -> Result<(HashSet<String>, Vec<String>), String> {
    collect_reachable_noting_gaps(heads, file_utils::does_object_exist)
}

/// The parcel walk behind [`collect_reachable_present_noting_gaps`] and
/// `gc_utils::collect_reachable_for_sweep`, which differ only in what counts as present: a read
/// may treat an unreadable parcel as a gap, a deleting sweep may not (FORK-193). One walk, so the
/// two can never drift.
///
/// # Arguments
/// * `heads`      - The starting parcel hashes.
/// * `is_present` - Whether a parcel is here to read. An error aborts the walk.
///
/// # Returns
/// * `Ok((reachable, gaps))` - As for [`collect_reachable_present_noting_gaps`].
/// * `Err(String)`           - If `is_present` failed, or a present parcel could not be read.
pub(crate) fn collect_reachable_noting_gaps(
    heads: &[String],
    is_present: impl Fn(&str) -> Result<bool, String>,
) -> Result<(HashSet<String>, Vec<String>), String> {
    let mut queue: VecDeque<String> = heads.iter().cloned().collect();
    let mut reachable: HashSet<String> = HashSet::new();
    let mut gaps: Vec<String> = Vec::new();
    let mut seen_gaps: HashSet<String> = HashSet::new();

    while let Some(hash) = queue.pop_front() {
        // A merge's parents are queued once per child edge: settle a repeat before asking presence.
        if reachable.contains(&hash) {
            continue;
        }

        if !is_present(&hash)? {
            if seen_gaps.insert(hash.clone()) {
                gaps.push(hash);
            }

            continue;
        }

        reachable.insert(hash.clone());

        for parent in object_utils::load_parcel(&hash)?.parents {
            queue.push_back(parent);
        }
    }

    Ok((reachable, gaps))
}

/// Per-revoked-key memo of a distrust boundary's vouched parcel set: the reachability walk
/// ([`collect_reachable_present_noting_gaps`]) runs at most once per distinct key, not once per parcel
/// that key signed. Shared between `audit`'s phase-3 resolution (below) and the query
/// engine's `signer.boundary` predicate (`query_utils::QueryContext`) — the walk-and-
/// membership arithmetic must never drift between the two.
///
/// **Presence-guarded, asymmetrically, and gap-aware.** A partial store may be missing part
/// of a key's distrust-boundary ancestry — not only a boundary head itself, but any interior
/// ancestor the walk would otherwise have to cross to decide a parcel's membership. Neither
/// kind of gap can ever be dismissed as "irrelevant" up front: relatedness to the parcel under
/// test is precisely what the walk exists to determine (see `verify_pallet_history`'s
/// `boundary_gap` doc for the full argument — it applies here verbatim). Either kind of gap
/// can only ever shrink the vouched set the walk computes, never grow it, so a `true` from
/// [`Self::vouched`] is trustworthy no matter what else is missing; a `false` is ambiguous —
/// genuinely outside the boundary, or an artifact of this store's own gaps — and
/// [`Self::resolvable`]/[`Self::unresolved_head`] are the tie-breaker for exactly that case.
/// Both read off the *same* walk [`Self::vouched`] runs (via
/// [`collect_reachable_present_noting_gaps`]), so a head-only pre-scan can never miss a gap
/// that only shows up once the walk actually crosses it.
///
/// Both callers ask `vouched` first and consult `resolvable`/`unresolved_head` only once
/// `vouched` has already answered `false` — never the reverse. `audit`'s phase 3 (below,
/// shared with every server's ref-update check) has always worked this way: an ordinary lift
/// whose boundary happens to name some unrelated, never-to-be-lifted pallet's head must not
/// pay for (or fail) a presence check it does not need. The query engine's `fill_boundary`
/// used to check `resolvable` unconditionally instead, on the theory that a read-only reporting
/// tool should rather label a subtle case "unresolved" a little too eagerly than ever risk
/// reading a `true` that later turns out to have been lucky — but that reasoning proves too
/// much: `resolvable` is boundary-wide, not per-parcel, so it went `Unresolved` for a parcel a
/// present head plainly vouches for whenever *any other* part of the same boundary had a gap,
/// which is exactly the "irrelevant" mistake this doc's first paragraph rules out. `vouched`'s
/// own reachability walk already carries the same monotonic guarantee (`true` can never be a
/// false positive), so asking it first loses no safety and fixes the over-fire; `query` now
/// matches `audit`'s ordering (see `query_utils`'s `fill_boundary` doc for the query-side
/// history of this bug).
#[derive(Default)]
pub struct DistrustBoundaryMemo {
    vouched_sets: HashMap<String, HashSet<String>>,

    /// Per key id: every absent hash the walk that built `vouched_sets` actually referenced
    /// (a boundary head, or an interior ancestor), in the order first encountered. Empty means
    /// the walk crossed no gap at all. Built in the same pass as the matching `vouched_sets`
    /// entry — never derived from a separate, heads-only pre-scan.
    gaps: HashMap<String, Vec<String>>,
}

impl DistrustBoundaryMemo {
    pub fn new() -> DistrustBoundaryMemo {
        DistrustBoundaryMemo::default()
    }

    /// Whether `key`'s distrust boundary is fully resolvable on this store: the walk that
    /// decides `vouched` crossed no gap at all (no absent boundary head, no absent interior
    /// ancestor). Memoized per key, and it *is* the walk — the first call for a given key
    /// (whether this, [`Self::unresolved_head`], or [`Self::vouched`]) builds and caches it;
    /// every call after is a lookup.
    ///
    /// A caller only ever needs this once [`Self::vouched`] has already answered `false` for
    /// a parcel — that `false` could be genuine, or it could be this store's own
    /// incompleteness (see the struct docs).
    pub fn resolvable(&mut self, key: &KeyRecord) -> Result<bool, String> {
        Ok(self.gaps_of(key)?.is_empty())
    }

    /// The first gap [`Self::resolvable`] found for `key`, if any — a boundary head or an
    /// interior ancestor this store does not have — so a caller's refusal can name an actual
    /// missing parcel rather than just say "unresolved". `None` when the boundary is fully
    /// resolvable (i.e. when `resolvable` is `true`).
    pub fn unresolved_head(&mut self, key: &KeyRecord) -> Result<Option<String>, String> {
        Ok(self.gaps_of(key)?.first().cloned())
    }

    /// Whether `parcel` sits inside `key`'s distrust boundary (§8.11): the boundary's
    /// vouched set is computed and cached on first use for this key, then it's a plain
    /// membership check on every call after.
    ///
    /// The walk that builds it treats any gap — an absent boundary head, or an absent
    /// interior ancestor it runs into partway — as "not reachable", which can only shrink
    /// what it reports as vouched. A `true` answer is safe to trust as-is; a caller that gets
    /// `false` back and needs to know whether that reflects a real gap should consult
    /// [`Self::resolvable`]/[`Self::unresolved_head`] next (see the struct docs) — they read
    /// off this same walk, not a separate one.
    pub fn vouched(&mut self, key: &KeyRecord, parcel: &str) -> Result<bool, String> {
        self.ensure_walked(key)?;

        Ok(self.vouched_sets.get(&key.key_id).unwrap().contains(parcel))
    }

    /// Build (once per key id, memoized) the reachable-and-present set alongside the gaps the
    /// same walk crossed — the single source both [`Self::vouched`] and
    /// [`Self::resolvable`]/[`Self::unresolved_head`] read from.
    fn ensure_walked(&mut self, key: &KeyRecord) -> Result<(), String> {
        if self.vouched_sets.contains_key(&key.key_id) {
            return Ok(());
        }

        let (vouched, gaps) = collect_reachable_present_noting_gaps(&key.distrust_boundary)?;

        self.vouched_sets.insert(key.key_id.clone(), vouched);
        self.gaps.insert(key.key_id.clone(), gaps);

        Ok(())
    }

    fn gaps_of(&mut self, key: &KeyRecord) -> Result<&[String], String> {
        self.ensure_walked(key)?;

        Ok(self.gaps.get(&key.key_id).unwrap())
    }
}

/// Check the key invariants no office parcel may break, no matter who signed it:
/// keys are never removed, their identifying fields never change, and revocation is
/// append-once (a revoked key stays revoked, with the reason and distrust boundary it
/// was revoked with).
fn verify_key_permanence(previous: &OfficeState, current: &OfficeState) -> Result<(), String> {
    for key in &previous.keys {
        let Some(kept) = current.find_key(&key.key_id) else {
            return Err(format!("removes key {}; keys are retained forever", key.key_id));
        };

        if kept.operator != key.operator
            || kept.public_key != key.public_key
            || kept.issued_at != key.issued_at
            || kept.authorized_by != key.authorized_by
            || kept.endorsement != key.endorsement
            || kept.proof_of_possession != key.proof_of_possession
        {
            return Err(format!("alters key {}; key records are immutable", key.key_id));
        }

        if !key.is_active()
            && (kept.retired_at != key.retired_at
                || kept.revocation_reason != key.revocation_reason
                || kept.distrust_boundary != key.distrust_boundary)
        {
            return Err(format!(
                "alters the revocation of key {}; a revocation is append-once",
                key.key_id
            ));
        }

        if key.is_active() && !kept.is_active() && kept.revocation_reason.is_none() {
            return Err(format!(
                "revokes key {} without a reason; revocations carry one",
                key.key_id
            ));
        }
    }

    Ok(())
}

/// Check that an office change only touches the signer's own keys: the user records
/// are untouched, no foreign key changes, and added keys belong to the signer.
/// (The universal key invariants live in `verify_key_permanence`.)
fn verify_self_service_change(previous: &OfficeState,
                              current: &OfficeState,
                              signer: &str) -> Result<(), String> {
    // Compare whole records, not a hand-picked projection of fields: `UserRecord`
    // derives `PartialEq` precisely so a field added there is protected here by
    // default, rather than by remembering to extend a parallel tuple (FORK-76 — a
    // five-of-seven-field projection let `class` and `supervisor` drift unguarded).
    // Every legitimate self-service change (key rotation, retirement, device linking)
    // only ever touches `OfficeState::keys`; no `UserRecord` field is self-service.
    if previous.users != current.users {
        return Err("changes user records; only admins may".to_string());
    }

    for key in &previous.keys {
        // Permanence (existence, immutability, append-once revocation) already held;
        // self-service adds: you only revoke your own keys.
        let revocation_changed = current.find_key(&key.key_id)
            .map(|kept| kept.retired_at != key.retired_at
                || kept.revocation_reason != key.revocation_reason
                || kept.distrust_boundary != key.distrust_boundary)
            .unwrap_or(true);

        if revocation_changed && key.operator != signer {
            return Err(format!(
                "retires key {} of \"{}\"; only admins may touch others' keys",
                key.key_id, key.operator
            ));
        }
    }

    for key in &current.keys {
        if previous.find_key(&key.key_id).is_none() && key.operator != signer {
            return Err(format!(
                "adds key {} for \"{}\"; only admins may add others' keys",
                key.key_id, key.operator
            ));
        }
    }

    Ok(())
}

/// Verify one parcel's signature against a key registry.
///
/// # Arguments
/// * `parcel_hash` - The parcel to verify.
/// * `state`       - The office state whose keys are acceptable.
/// * `kind`        - What is being verified (for error messages).
///
/// # Returns
/// * `Ok(())`      - If the signature is valid.
/// * `Err(String)` - If the sidecar is missing, the key is unknown, or the signature
///                   does not verify.
pub fn verify_one_signature(parcel_hash: &str,
                            state: &OfficeState,
                            kind: &str) -> Result<(), String> {
    let signature = sign_utils::load_parcel_signature(parcel_hash)?
        .ok_or(format!("The {} {} carries no signature.", kind, parcel_hash))?;

    let key = state.find_key(&signature.key_id)
        .ok_or(format!(
            "The {} {} is signed with key {}, which is not tracked (at that point) in \
            the office.",
            kind, parcel_hash, signature.key_id
        ))?;

    let is_valid = sign_utils::verify_parcel_signature(
        &key.public_key,
        parcel_hash,
        &signature.signature
    )?;

    if !is_valid {
        return Err(format!(
            "The signature of {} {} does not verify against key {}. The warehouse may \
            have been tampered with.",
            kind, parcel_hash, signature.key_id
        ));
    }

    Ok(())
}

/// Refuse a first trust anchor whose `adopts` names history the warehouse does not completely
/// hold. Every head that establishes an anchor runs this: a first anchor arrives over an unborn
/// office, so nothing else ties its `adopts` to held history — yet every warehouse that adopts
/// the anchor later fetches that history from here, and roots it with
/// `PinAbsencePolicy::Loss`. The head's object being present is not enough: a parcel or record
/// lost behind it is just as unservable.
///
/// # Arguments
/// * `adopts`         - The anchor's `adopts` pin (`None` passes: nothing is adopted).
/// * `verify_closure` - The head's whole-history check ([`verify_parcel_closure`] with no
///                      bound, or the same walk over the head's own object store).
///
/// # Returns
/// * `Ok(())`      - If nothing is adopted, or the adopted history is completely present.
/// * `Err(String)` - The refusal, naming the pin and what is missing.
pub fn require_adopted_history(
    adopts: Option<&str>,
    verify_closure: &dyn Fn(&str) -> Result<(), String>,
) -> Result<(), String> {
    let Some(adopts) = adopts else {
        return Ok(());
    };

    verify_closure(adopts).map_err(|missing| format!(
        "The trust anchor adopts office head {}, but this warehouse does not hold that history \
        completely ({}). Lift that history before the anchor.",
        adopts, missing
    ))
}

/// Verify that the history behind a head is completely present: every parcel from
/// `head` back to `known_complete` (exclusive), and the full tree/blob closure of each
/// of those parcels. A ref must never point at missing history — a remote runs this
/// before committing a ref update.
///
/// Everything reachable from `known_complete` is assumed complete (it was verified when
/// that head was committed), so only the new slice is walked — including across a merge
/// that rejoins below it. Until 2026-07-09 this held for trees and blobs but not for parcel
/// bodies: the prune set was built with `collect_reachable(known_complete)`, which decoded
/// every one of them. It no longer touches them, which is what makes an incremental lift
/// O(new parcels) instead of O(history).
///
/// The consequence, stated plainly: a store that has *lost* a parcel behind
/// `known_complete` no longer fails here. It never failed on a lost tree or blob behind it
/// either — that ancestry is trusted, by the same induction the signature audit uses. The
/// full `audit` command (`known_complete: None`) is what proves the whole history present.
///
/// # Arguments
/// * `head`           - The head whose history to verify.
/// * `known_complete` - A head whose history is already known to be complete (`None`
///                      verifies all the way down).
///
/// # Returns
/// * `Ok(())`      - If everything is present.
/// * `Err(String)` - If a parcel, tree or blob is missing (or unreadable).
///
/// The walk is the meta namespace's, so it does not refuse a meta-shaped root: the callers check a
/// trust anchor's adopted office chain, or prove a history present whatever its shape. A user
/// pallet's ref update goes through [`verify_parcel_closure_for_write`] with its namespace.
pub fn verify_parcel_closure(head: &str, known_complete: Option<&str>) -> Result<(), String> {
    verify_local_closure(head, known_complete, PalletNamespace::Meta).map_err(|error| error.to_string())
}

/// [`verify_parcel_closure_with`] over the local object store.
fn verify_local_closure(head: &str,
                        known_complete: Option<&str>,
                        namespace: PalletNamespace) -> Result<(), ClosureError> {
    // The default checks read straight from the local object store; the serverless head passes an
    // S3 HEAD for blob presence, a bounded-concurrency batch of S3 HEADs for the chunk-presence
    // seam, a store-backed recipe read for the chunk descent, and a store-backed tree read for the
    // base-tree prune (see `verify_parcel_closure_with`).
    verify_parcel_closure_with(
        head,
        known_complete,
        namespace,
        &|hash| file_utils::does_object_exist(hash),
        &local_chunks_missing,
        &|hash| object_utils::recipe_chunk_hashes(hash),
        &|hash| object_utils::load_tree(hash),
    )
}

/// [`verify_parcel_closure`] for a ref update of a pallet in `namespace`, returning the proof a ref
/// write accepts in place of verifying the same head again
/// ([`pallet_utils::update_pallet_head_verified`]). The walk is unscoped, so the proof is for the
/// full scope: in a sparse warehouse the write verifies again at its own. A user pallet's walk also
/// refuses a meta-shaped root (see [`verify_parcel_closure_with`]), and the proof records the
/// namespace it was walked for, so a meta walk's proof never lets a user-pallet write skip that,
/// and `known_complete`, which a write accepts only as the head its compare-and-set expects.
///
/// # Returns
/// * `Ok(VerifiedClosure)` - Everything is present.
/// * `Err(ClosureError)`   - If a parcel, tree or blob is missing (or unreadable), or a user
///                           history holds a meta-shaped root.
pub fn verify_parcel_closure_for_write(head: &str,
                                       known_complete: Option<&str>,
                                       namespace: PalletNamespace) -> Result<pallet_utils::VerifiedClosure, ClosureError> {
    verify_local_closure(head, known_complete, namespace)?;

    Ok(pallet_utils::VerifiedClosure::new(head, PathScope::full(), namespace, known_complete))
}

/// [`verify_parcel_closure`], with the leaf-blob presence check made pluggable.
///
/// Parcels and trees are always read (and parsed) from the object store — the walk
/// cannot proceed without them. File-content blobs, by contrast, are never *read* here,
/// only checked for presence, so their existence check is the one seam a non-filesystem
/// head varies: the AWS serverless head mirrors the parcels and trees it must parse into
/// a scratch `.forklift`, but leaves the (large, many) working blobs in object storage
/// and answers this check with an S3 `HEAD` (DESIGN.html §4.6).
///
/// For a user pallet the walk also refuses any new parcel whose root tree has the shape of
/// metadata history (see [`ClosureError::MetaShapedRoot`]).
///
/// # Arguments
/// * `head`               - The head parcel whose closure is verified.
/// * `known_complete`     - A head whose history is already known complete (`None` verifies
///                          all the way down).
/// * `namespace`          - The namespace of the pallet whose ref is to hold `head`.
/// * `blob_exists`        - Returns whether a file-content blob — or a recipe object, a chunked
///                          file's tree-entry — is present at a hash. The plain, per-hash presence
///                          seam, left untouched; the many-per-recipe chunk presence goes through
///                          `chunks_missing` instead.
/// * `chunks_missing`     - The bulk chunk-presence seam (§9.4b W4): given a recipe's full
///                          chunk-hash list, returns the subset the store lacks. A `*Chunked` file
///                          entry names a recipe whose chunks are reachable only *through* it, so
///                          the closure walk hands the whole list here in one call and fails
///                          **non-tolerantly** if any come back missing — a ref must never advance
///                          over a chunked file whose chunks never reached the store. A local head
///                          answers it with a serial filesystem scan; the AWS head answers it with a
///                          bounded-concurrency batch of S3 `HEAD`s, so a changed multi-gigabyte
///                          file's thousands of chunks verify in a second or two rather than one
///                          slow round trip apiece under API Gateway's hard timeout.
/// * `load_recipe_chunks` - Returns the ordered chunk hashes of a recipe (the list `chunks_missing`
///                          probes). The local path reads the recipe from the object store; so does
///                          the AWS head, which is why the read is a parameter and not a hard-coded
///                          load. A recipe is a file-entry object, so the audit scratch holds one
///                          only when the mirror was told to take file blobs — which it is for a
///                          meta pallet and is not for a working one. Reading through the parameter
///                          is correct in both modes; assuming either is not.
/// * `load_base_tree`     - Reads a **candidate base's** tree for the subtree prune (§9.4b W1): a
///                          new parcel's subtree that some candidate carries at the same path is
///                          skipped whole. The candidates are `known_complete`'s subtree and the
///                          parcel's own immediate parents' — already-audited history in the first
///                          case, a member of this same call that the parents-first order has
///                          already audited in the second. On the AWS head, a working pallet's
///                          `known_complete` tree is *not* in the audit scratch — the mirror
///                          clears its full-expansion flag at that one *hash*, so the bound
///                          alone is spared — and this must read it from object storage. An
///                          in-segment parent's trees *are* mirrored, and the re-fetch is the
///                          accepted price of one seam rather than two. Neither half is a rule
///                          to lean on: the office chain gets its own unbounded mirror, so on a
///                          meta push the bound's tree may be present after all. Read through
///                          this parameter and the mode stops mattering. On the self-host head
///                          everything comes from the local store. Never called for a creation
///                          (`known_complete: None`), which takes no candidates at all.
pub fn verify_parcel_closure_with(
    head: &str,
    known_complete: Option<&str>,
    namespace: PalletNamespace,
    blob_exists: &dyn Fn(&str) -> Result<bool, String>,
    chunks_missing: &dyn Fn(&[String]) -> Result<Vec<String>, String>,
    load_recipe_chunks: &dyn Fn(&str) -> Result<Vec<String>, String>,
    load_base_tree: &dyn Fn(&str) -> Result<TreeItem, String>,
) -> Result<(), ClosureError> {
    // A remote must hold the whole closure of every head it accepts: the push gate tolerates no
    // recorded absence.
    let seams = ClosureSeams { blob_exists, chunks_missing, load_recipe_chunks, load_base_tree, sealed: None };

    verify_closure_core(
        head, known_complete.as_ref().map(std::slice::from_ref), known_complete, &PathScope::full(),
        false, namespace == PalletNamespace::User, &seams,
    )
}

/// Why a closure walk refused a head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClosureError {
    /// A parcel, tree, blob or chunk of the history is missing or unreadable; the message names it.
    Incomplete(String),

    /// A new parcel of a user pallet's history has a root tree holding a `.forklift` entry: the
    /// shape of office and meta history, which heal and scope prune walk in full whatever the
    /// fetch scope. No fetch can mend it, so it is never reported as missing history.
    MetaShapedRoot {
        /// The parcel whose root tree has that shape.
        parcel: String,
    },
}

impl From<String> for ClosureError {
    fn from(missing: String) -> ClosureError {
        ClosureError::Incomplete(missing)
    }
}

impl std::fmt::Display for ClosureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClosureError::Incomplete(missing) => f.write_str(missing),
            ClosureError::MetaShapedRoot { parcel } => write!(
                f,
                "Parcel {} holds a \"{}\" entry at its root, which only metadata history may hold.",
                parcel, crate::globals::FOLDER_NAME_FORKLIFT_ROOT
            ),
        }
    }
}

/// The object-store reads [`verify_closure_core`] varies by head (see
/// [`verify_parcel_closure_with`] for each one).
struct ClosureSeams<'a> {
    blob_exists: &'a dyn Fn(&str) -> Result<bool, String>,
    chunks_missing: &'a dyn Fn(&[String]) -> Result<Vec<String>, String>,
    load_recipe_chunks: &'a dyn Fn(&str) -> Result<Vec<String>, String>,
    load_base_tree: &'a dyn Fn(&str) -> Result<TreeItem, String>,
    /// The sealed-by-hash record, for the audit and the client's ref gate; `None` on the push
    /// gate, which tolerates no recorded absence.
    sealed: Option<&'a SealedTolerance<'a>>,
}

/// The sealed-by-hash record a walk holds absences against, and the absent objects it held. The
/// audit and the client's ref gate pass one; a remote's push gate never does, since a remote must
/// hold the whole closure of every head it accepts.
pub struct SealedTolerance<'a> {
    record: &'a SealedSet,
    held: RefCell<HashSet<String>>,
}

impl<'a> SealedTolerance<'a> {
    pub fn new(record: &'a SealedSet) -> SealedTolerance<'a> {
        SealedTolerance { record, held: RefCell::new(HashSet::new()) }
    }

    /// How many distinct absent objects the walk held by hash alone.
    pub fn held(&self) -> usize {
        self.held.borrow().len()
    }

    fn is_empty(&self) -> bool {
        self.record.is_empty()
    }

    /// Whether the absent object `hash` is held by the record, tallying it if so. Call only at an
    /// absence: a recorded object that is present seals nothing.
    fn holds_absent(&self, hash: &str) -> bool {
        let held = self.record.contains(hash);
        if held {
            self.held.borrow_mut().insert(hash.to_string());
        }
        held
    }
}

/// [`SealedTolerance::holds_absent`], where `None` holds nothing.
fn holds_absent(sealed: Option<&SealedTolerance>, hash: &str) -> bool {
    sealed.is_some_and(|sealed| sealed.holds_absent(hash))
}

/// The closure walk behind every verifier here.
///
/// * `bounds` - The heads whose history is already known complete. `None` is the unbounded walk
///              (the audit, and the control the tests use): every parcel, breadth-first from
///              `head`, each tree walked whole. `Some` walks only the parcels no bound reaches,
///              parents first, pruning each tree against its parents' (an empty set walks every
///              parcel that way).
/// * `base`   - A bound whose root tree is a candidate base for every parcel of the segment, as
///              well as each parcel's own parents. Must be one of `bounds`.
/// * `scope`    - The warehouse fetch scope: content outside it is sealed, never loaded. A full
///                scope walks everything.
/// * `reverify` - The `audit --full` content level, threaded down to each chunked file.
/// * `refuse_meta_roots` - Refuse a parcel of the segment whose root tree is meta-shaped: set for a
///                         user pallet's ref update.
fn verify_closure_core(
    head: &str,
    bounds: Option<&[&str]>,
    base: Option<&str>,
    scope: &PathScope,
    reverify: bool,
    refuse_meta_roots: bool,
    seams: &ClosureSeams,
) -> Result<(), ClosureError> {
    let ClosureSeams { blob_exists, chunks_missing, load_recipe_chunks, load_base_tree, sealed } = *seams;

    // The walk reads to learn what the store holds, and a failed read is what it reports: it
    // fetches nothing, or a cache's gate would fetch the history it holds by hash.
    let _unfetched = cache_miss::Unfetched::enter();

    // Only the new segment: the closure behind each bound was proven complete when that head
    // was written. This walk used to build its prune set with `collect_reachable` from the bound,
    // which decoded every parcel body in the ancestry — O(history) on every ref update, however
    // little the lift added.
    let (parcels, parents_of) = new_parcels_with_edges(head, bounds)
        .map_err(|e| format!("The history behind {} is incomplete: {}", head, e))?;

    // Parents first — but only for the incremental gate, and for the same reason the parent bases
    // below are gated: it is the parent bases that need the order, and with `bounds: None` there
    // are none. `new_parcels` returns the segment breadth-first *from head*, so a child
    // precedes the parent that explains it — the order the ordinary audit reports failures in, and
    // deliberately left alone there. This walk needs the opposite, because a parcel's own parents
    // are candidate bases below and may only explain content their own top-level audit has already
    // settled.
    //
    // Reordering the unbounded walk would change nothing about what it *accepts* (its base set is
    // empty at every parcel), only which of several missing objects it names first — which is a
    // documented property of that mode, is what the tests using it as a control read, and is not
    // this change's to alter. So the unbounded walk keeps its order, and pays for no sort. The
    // one thing that rides with the gate is the cycle check, and it is cycle *refusal*, not merely
    // cycle naming: against the ungated form an unbounded audit of a cyclic record view turns Err
    // into Ok. Against what ships today it forfeits nothing — the replaced code had no cycle
    // check in either mode — but the honest statement is the stronger one. Such an audit still
    // terminates (the sweep's `visited` set closes it) and still closure-checks every parcel it
    // reaches; it just no longer refuses on the cycle itself.
    //
    // Which leaves the refusal on the push gate and not on the command chartered to find
    // corruption: `forklift audit` goes through `verify_parcel_closure_scoped`, which builds its
    // segment with `new_parcels` and so has no edges to check, gate or no gate. That is not this
    // change's doing — that path never had a cycle check — but it is where one belongs, and
    // putting it there is its own change with its own test (FORK-111).
    //
    // The edges come through `graph_utils::parents`, the same accessor `new_parcels` swept with,
    // so the prune cannot see a parent edge the sweep did not — and they are read once here and
    // reused as the candidate bases below.
    //
    // Ordered by a topological sort of the segment against its own edges, deliberately *not* by
    // commit-graph generation number.
    //
    // Not for cost: at the AWS gate the scratch mirrors ancestry parcel bodies, so `ensure`'s loads
    // are local decodes, and `fresh_frontier` above already calls `node` on every parcel it first
    // sees — an ordering that read generations would mostly be paying a bill already paid. The
    // reason is correctness. `graph_utils::node` returns a *stored* record without validating it
    // and self-heals only a miss, so ascending-generation is topological exactly as far as those
    // records are right. Sorting against the edges the membership sweep itself read cannot
    // disagree with membership even in principle, which is a stronger discharge of the same
    // obligation. That it also costs no additional read is a bonus, not the argument.
    let ordered = match bounds {
        Some(_) => parents_first(&parcels, &parents_of).ok_or_else(|| {
            format!("The history behind {} has a cycle; the warehouse is corrupt.", head)
        })?,
        None => parcels,
    };

    // The candidate bases a parcel's tree is pruned against (§9.4b W1): `base`'s root tree, **and**
    // the parcel's own immediate parents' root trees.
    //
    // Soundness — a subtree byte-identical to a candidate's at the same path has, by content-
    // addressing, the identical closure. For `base` (one of the bounds) that closure was proven
    // present when it was written; for a parent inside this segment it was proven by that parent's own
    // top-level audit, which the parents-first order has already run. Completeness — a subtree no
    // candidate explains is walked (once, deduped by `visited_trees`), so the audit never advances
    // a ref over content whose presence neither an accepted push nor an earlier-audited member of
    // this same call has established.
    //
    // Why the union rather than replacing the root: dropping `base` is not never-worse.
    // Content can move away and come back, so a parcel that reverts a path to `base`'s
    // content is explained free today and would cost a full descent under a parents-only prune.
    // Keeping both makes the skip set a superset of the current one by construction.
    // `.ok()` is the walk's *third* mirror-dependent local read, and the quietest: the boundary-
    // parent arm below at least names itself, and the per-member load is loud (it errors). This one
    // fails to a `None` that reads as "creation, no candidates". On a linear push the sole parent
    // is usually `base`, so the arm below re-reads this same parcel and fails the same way —
    // the candidate list ends up empty and every push walks whole. Tolerant in the safe direction
    // (more is verified, never less) but invisible.
    //
    // The obligation this creates is *not* "cover the tolerant reads". The membership sweep's own
    // body reads — `graph_utils::node`'s `ensure` and `graph_utils::parents`'s load — reach
    // below the bound too and are **not** tolerant, so bounding the ancestry mirror to exactly the
    // reads named here would break the audit at its first step rather than degrade it. Tolerance
    // sorts these reads by how their failure *presents*, loud or silent; it does not sort them by
    // which ones the mirror has to keep answering. It has to answer all of them.
    let base_root: Option<String> = match base {
        Some(known) => object_utils::load_parcel(known).ok().map(|parcel| parcel.tree_hash),
        None => None,
    };

    // Trees whose entire closure is proven present, and (tree, path) pairs walked only as the
    // spine of a sparse scope (see `verify_tree_closure_scoped`).
    let mut visited_trees: HashSet<String> = HashSet::new();
    let mut spine_seen: HashSet<(String, String)> = HashSet::new();

    // Each parcel's tree hash, remembered as it is audited. The parents-first order means every
    // in-segment parent has already passed through this loop by the time a child needs its tree,
    // so a candidate base costs a map lookup rather than a re-decode of a parcel this walk has
    // already read. Only boundary parents — the fork points just below a bound, which the segment
    // excludes — fall through to a load.
    let mut tree_of: HashMap<String, String> = HashMap::new();

    // The root trees whose shape has been checked, so a root shared by several parcels is read once.
    let mut roots_checked: HashSet<String> = HashSet::new();

    // Seed with `base`: it is the boundary parent of every linear push, and its tree was
    // already loaded above for `base_root`. Without this the common case pays a second read of the
    // same parcel purely to learn a hash already in hand.
    if let (Some(known), Some(root)) = (base, base_root.as_ref()) {
        tree_of.insert(known.to_string(), root.clone());
    }

    for hash in &ordered {
        let parcel = object_utils::load_parcel(hash)
            .map_err(|e| format!("The history behind {} is incomplete: {}", head, e))?;

        tree_of.insert(hash.clone(), parcel.tree_hash.clone());

        // Parents through `graph_utils::parents` — the same accessor `new_parcels` swept with, so
        // the prune cannot see a parent edge the sweep did not. Reading the parcel body directly
        // here would be a second, uncorroborated view of "who is a parent", and a parent visible to
        // one and not the other is exactly the case the soundness induction has no third arm for.
        // Each parent's tree is read tolerantly: a candidate that cannot be resolved contributes no
        // explanations, so the walk verifies more rather than less.
        let mut bases: Vec<String> = base_root.iter().cloned().collect();

        // Parent bases are the incremental gate's optimization, and only its. With no `bounds`
        // this call is the unbounded walk — the one an operator runs to prove a
        // warehouse whole, and the one these tests use as the control that a deleted object really
        // was deleted. Pruning it against parents would still be *sound* by the same induction
        // (a root parcel has no parents and is walked whole, and every child only skips what an
        // already-audited ancestor carries), but it would no longer walk every object, and a
        // control that prunes is not a control. Left full on purpose.
        if bounds.is_some() {
            for parent in parents_of.get(hash).into_iter().flatten() {
                match tree_of.get(parent) {
                    Some(tree) => bases.push(tree.clone()),
                    // A boundary parent, and the walk's *second* store seam — this one local, not
                    // the `load_base_tree` closure the candidate trees come through. On the AWS
                    // head it resolves only because `scratch::materialize` mirrors parcel bodies
                    // for the whole ancestry, below the bound included. That is an obligation on
                    // the scratch, not a coincidence: bound the ancestry mirror (an obvious
                    // future move — it is O(history) `GET`s on a cold scratch) and this arm
                    // starts failing, tolerantly, dropping the boundary parent's base on every
                    // merge push and silently widening the walk. See `base_root` above for why
                    // that obligation covers every body read below the bound, not just the two
                    // that fail quietly.
                    // Memoized like an in-segment parent's, so a fork point shared by n children
                    // of this segment is decoded once rather than once per edge naming it.
                    None => if let Ok(parent_parcel) = object_utils::load_parcel(parent) {
                        tree_of.insert(parent.clone(), parent_parcel.tree_hash.clone());
                        bases.push(parent_parcel.tree_hash);
                    },
                }
            }
        }

        // `bases` holds *tree* hashes, so what collapses here is two candidates naming the same
        // tree — not a parent that happens to be `base`. The first parcel of a linear
        // push is the obvious case (its only parent *is* the bound), but so is any later parcel
        // whose ancestors left the root tree untouched. Without this, every changed level below
        // such a parcel would load the identical base tree twice — a read apiece on the AWS head.
        //
        // What the dedupe does not remove: where the trees genuinely differ — the ordinary case
        // for `N2..Nk` in a k-parcel segment, and for both sides of a merge — each changed level
        // costs two base reads where the single-base prune cost one. That is the union's price,
        // paid where the union is what buys the pruning. Dedupe rather than special-case any of
        // these shapes: a merge whose sides share a tree at some path hits the same waste one level
        // down.
        bases.sort();
        bases.dedup();

        // Heal and scope prune walk a meta-shaped root tree in full whatever the scope, and heal
        // roots every parcel of a history it reaches, so a user history may take that shape at no
        // parcel it newly admits. Checked on the root itself, not inside the tree walk below, which
        // skips a tree it has already seen as a subtree. A root some candidate base carries is
        // admitted history's, which passed this check when it was admitted. Any other root is read
        // from the local store, as the walk reads it: the AWS head's scratch mirrors every tree of
        // the segment, and the one tree it spares, the bound's root, is a candidate base.
        if refuse_meta_roots && !bases.contains(&parcel.tree_hash) && roots_checked.insert(parcel.tree_hash.clone()) {
            refuse_meta_shaped_root(hash, &parcel.tree_hash, blob_exists, sealed)?;
        }

        // At a full scope this is the plain tree walk (the root is in scope); in a sparse
        // warehouse it stops at each out-of-scope boundary. The commit gate presence-checks;
        // content re-verification is `audit --full` only.
        verify_tree_closure_scoped(
            &parcel.tree_hash, "", &bases, &mut visited_trees, &mut spine_seen,
            blob_exists, chunks_missing, load_recipe_chunks, load_base_tree, scope, reverify, sealed,
        )?;
    }

    Ok(())
}

/// Refuse `parcel` when its root tree `tree_hash` is meta-shaped: the test heal's and scope prune's
/// walks apply at a root ([`recovery_utils::walks_in_full`]), so the two cannot disagree. A root the
/// sealed-by-hash record holds absent cannot be read, and passes: the remote it came from checked it.
fn refuse_meta_shaped_root(parcel: &str,
                           tree_hash: &str,
                           object_exists: &dyn Fn(&str) -> Result<bool, String>,
                           sealed: Option<&SealedTolerance>) -> Result<(), ClosureError> {
    if sealed_absent(sealed, tree_hash, object_exists)? {
        return Ok(());
    }

    let tree = object_utils::load_tree(tree_hash)
        .map_err(|e| format!("Tree {} is missing or unreadable: {}", tree_hash, e))?;

    match recovery_utils::walks_in_full(&tree, "") {
        true => Err(ClosureError::MetaShapedRoot { parcel: parcel.to_string() }),
        false => Ok(()),
    }
}

/// Order a push's new segment so every parcel follows the parents that are in the segment with it.
///
/// Kahn's algorithm over the induced subgraph. Only in-segment parents constrain anything: a parent
/// outside it is at or below `known_complete`, which the audit already trusts. A `BTreeSet` ready
/// set makes the emitted order a function of the DAG alone, so which failure an audit reports
/// stays reproducible across runs. `None` means the segment contains a cycle.
///
/// The standing premise, stated because it is the one thing here that is assumed rather than
/// established: a parent *outside* the segment is trusted on the strength of being reachable from
/// `known_complete`, and that reachability is decided from commit-graph records this walk does not
/// validate. Under a wrong record a boundary parent could explain content nothing has audited —
/// the union then hides more than the single-base prune it replaces. That is accepted rather than
/// designed away, because the boundary parent is exactly what explains the common case this prune
/// exists to make cheap; the exposure is the commit graph's correctness, which the audit already
/// leans on to decide segment membership at all.
fn parents_first(segment: &[String],
                 parents_of: &HashMap<String, Vec<String>>) -> Option<Vec<String>> {
    let in_segment: HashSet<&str> = segment.iter().map(String::as_str).collect();
    let mut pending: HashMap<&str, usize> = HashMap::new();
    let mut children_of: HashMap<&str, Vec<&str>> = HashMap::new();

    for hash in segment {
        let mut count = 0;

        for parent in parents_of.get(hash).into_iter().flatten() {
            if in_segment.contains(parent.as_str()) {
                children_of.entry(parent.as_str()).or_default().push(hash.as_str());
                count += 1;
            }
        }

        pending.insert(hash.as_str(), count);
    }

    let mut ready: std::collections::BTreeSet<&str> = pending.iter()
        .filter(|(_, count)| **count == 0)
        .map(|(hash, _)| *hash)
        .collect();
    let mut ordered: Vec<String> = Vec::with_capacity(segment.len());

    while let Some(hash) = ready.pop_first() {
        ordered.push(hash.to_string());

        for child in children_of.get(hash).into_iter().flatten() {
            // Not `?`: `None` is this function's cycle channel, and a missing key is not a cycle.
            // Every key in `children_of` was inserted from `segment`, which also seeded `pending`,
            // so a miss is a bug here — and reporting it as "the warehouse is corrupt" would send
            // an operator to scrub a warehouse that is fine.
            let count = pending.get_mut(*child)
                .expect("every in-segment child has a pending count");
            *count -= 1;
            if *count == 0 {
                ready.insert(child);
            }
        }
    }

    (ordered.len() == segment.len()).then_some(ordered)
}

/// [`verify_parcel_closure`], scoped to a sparse warehouse's fetch scope.
///
/// A sparse warehouse holds the full tree/blob closure only *within* its fetch scope; every
/// out-of-scope subtree, file and symlink is sealed by the hash a signed parcel commits, not
/// downloaded. This walk verifies exactly what is present — in-scope trees are loaded (and so
/// re-hashed on the content-addressed read) and their blobs presence-checked, exactly as a full
/// closure does — and stops at each out-of-scope boundary rather than erroring on the object it
/// deliberately never fetched. Passing a full (unrestricted) `scope` makes it identical to
/// [`verify_parcel_closure`].
///
/// Object presence is a warehouse property (what was fetched at all), so the walk is bounded by
/// the warehouse fetch scope, never a narrower bay checkout scope: an object fetched but
/// not materialized in *this* bay is still present in the shared store and is verified here.
///
/// # Arguments
/// * `head`           - The head parcel whose closure is verified.
/// * `known_complete` - A head already known complete (`None` verifies down to the genesis).
/// * `scope`          - The warehouse fetch scope the store was fetched against.
/// * `reverify`       - The `audit --full` content level (§9.4b): when `false`, an in-scope chunk
///                      is presence-checked (a normal audit — bounded, no bytes re-read); when
///                      `true`, every in-scope chunk's bytes are re-read (re-hashed on the
///                      content-addressed load) and each fully-present chunked file is re-assembled
///                      to verify `Blake3(assembled) == recipe.content_hash` — the one integrity
///                      claim a normal audit never checks (the W3 residual). Streamed, one chunk in
///                      memory. Out-of-scope recipes/chunks stay sealed either way.
///
/// # Returns
/// * `Ok(())`      - If everything in the fetch scope is present (out-of-scope content stays sealed).
/// * `Err(String)` - If an in-scope parcel, tree or blob is missing (or a tree/chunk is corrupt, or
///                   under `reverify` a chunk fails its content-address or a recipe its content hash).
pub fn verify_parcel_closure_scoped(
    head: &str,
    known_complete: Option<&str>,
    scope: &PathScope,
    reverify: bool,
    sealed: Option<&SealedTolerance>,
) -> Result<(), String> {
    // A local audit reads everything from the local object store. The bulk chunk-presence seam
    // probes it serially (a presence check is a microsecond filesystem lookup); the AWS head
    // overrides the *shape* with parallel S3 HEADs. An out-of-scope recipe is sealed, never
    // loaded, so its chunks are never even named — the store invariant "recipe absent ⟹ chunks
    // absent" holds.
    // An object recorded as sealed by hash is held by hash alone, like content outside the scope:
    // absent, it is tolerated and not descended.
    let seams = ClosureSeams {
        blob_exists: &|hash| file_utils::does_object_exist(hash),
        chunks_missing: &local_chunks_missing,
        load_recipe_chunks: &|hash| object_utils::recipe_chunk_hashes(hash),
        load_base_tree: &|hash| object_utils::load_tree(hash),
        sealed,
    };

    verify_closure_core(
        head, known_complete.as_ref().map(std::slice::from_ref), known_complete, scope, reverify, false, &seams,
    ).map_err(|error| error.to_string())
}

/// Verify the closure of `head` before a ref is moved to it, in this warehouse: every parcel from
/// `head` back to history some ref already reaches, and each one's trees, blobs and chunks within
/// the fetch scope `scope`. A ref must never point at missing history; this is the client's gate
/// for that, as [`verify_parcel_closure`] is a remote's.
///
/// Everything reachable from one of `known_complete` (the current ref heads) is trusted: every ref
/// head was verified when it was written, and a sparse warehouse widens its fetch scope only after
/// walking every ref head at the widened scope. `base` (the ref's own current head, one of
/// `known_complete`) is also a candidate base for every new parcel's tree, as each parcel's own
/// parents are. A parcel no ref reaches is walked down to its root.
///
/// An absent object `sealed` records is held by hash, as content outside the scope is: a cache
/// fetch records the root trees it skips before the ref that names them moves. A recorded object
/// that is present seals nothing, so a missing object beneath it still fails.
///
/// # Arguments
/// * `head`           - The parcel the ref is about to hold.
/// * `known_complete` - The ref heads whose history is known complete.
/// * `base`           - The ref's current head, when it has one.
/// * `scope`          - The warehouse fetch scope.
/// * `sealed`         - The sealed-by-hash record, or `None` to tolerate no recorded absence.
///
/// # Returns
/// * `Ok(())`            - Everything within the scope is present or held by the record.
/// * `Err(ClosureError)` - What is missing (or unreadable), or the parcel whose root is
///                         meta-shaped: the ref is a user pallet's, whose history may not take that
///                         shape.
pub fn verify_ref_head_closure(head: &str,
                               known_complete: &[String],
                               base: Option<&str>,
                               scope: &PathScope,
                               sealed: Option<&SealedSet>) -> Result<(), ClosureError> {
    let bounds: Vec<&str> = known_complete.iter().map(String::as_str).collect();
    let tolerance = sealed.map(SealedTolerance::new);
    let seams = ClosureSeams {
        blob_exists: &|hash| file_utils::does_object_exist(hash),
        chunks_missing: &local_chunks_missing,
        load_recipe_chunks: &|hash| object_utils::recipe_chunk_hashes(hash),
        load_base_tree: &|hash| object_utils::load_tree(hash),
        sealed: tolerance.as_ref(),
    };

    verify_closure_core(head, Some(&bounds), base, scope, false, true, &seams)
}

/// [`verify_tree_closure`], threaded with the three-valued sparse classifier so an out-of-scope
/// boundary is sealed (skipped) rather than loaded.
///
/// * **InScope** `prefix` — everything below is in scope too, so the whole closure is present:
///   verify it fully via [`verify_tree_closure`], against the same candidate bases, sharing (and
///   populating) the fully-verified `verified` memo.
/// * **OutOfScope** `prefix` — sealed by the hash the parent spine tree already commits; never
///   loaded, never descended.
/// * **Spine** `prefix` — an ancestor of an in-scope path: walk it, descending only the entries
///   that lead to an in-scope leaf and carrying the out-of-scope siblings forward by their
///   sealed hash. A spine walk verifies only part of the tree, so it must never record the tree
///   in `verified` — that memo means "entire closure present", which a spine walk cannot claim.
///
/// The base-tree prune holds on the spine as it does in scope: a spine tree some candidate base
/// carries at the same path has, by content-addressing, the same in-scope content, which that
/// base's own verification already found present at this scope. It is skipped, and not recorded
/// in `verified`; an entry a base carries at the same hash is skipped likewise.
///
/// # Arguments
/// * `prefix`      - The warehouse path of `tree_hash` (`""` at the root), classified by `scope`.
/// * `bases`       - The candidate bases' trees at this same path (see [`verify_tree_closure`]).
/// * `verified`    - Trees whose full closure is already proven present (populated by the full walk).
/// * `spine_seen`  - (tree, path) pairs already walked as spine in this pass.
/// * `reverify`    - The `audit --full` content level, threaded down to each in-scope chunked file.
// The four dyn-closure/set arguments, the classifier scope, the two-level path/prefix pair and the
// `reverify` level are each distinct and threaded through the recursion; a parameter object would
// only obscure them (as in `remote_utils::collect_changed_closure`).
#[allow(clippy::too_many_arguments)]
fn verify_tree_closure_scoped(tree_hash: &str,
                              prefix: &str,
                              bases: &[String],
                              verified: &mut HashSet<String>,
                              spine_seen: &mut HashSet<(String, String)>,
                              blob_exists: &dyn Fn(&str) -> Result<bool, String>,
                              chunks_missing: &dyn Fn(&[String]) -> Result<Vec<String>, String>,
                              load_recipe_chunks: &dyn Fn(&str) -> Result<Vec<String>, String>,
                              load_base_tree: &dyn Fn(&str) -> Result<TreeItem, String>,
                              scope: &PathScope,
                              reverify: bool,
                              sealed: Option<&SealedTolerance>) -> Result<(), String> {
    match scope.classify(prefix) {
        ScopeClass::InScope => {
            return verify_tree_closure(
                tree_hash, bases, verified, blob_exists, chunks_missing, load_recipe_chunks,
                load_base_tree, reverify, sealed,
            )
        }
        ScopeClass::OutOfScope => return Ok(()),
        ScopeClass::Spine => {}
    }

    // A spine tree already proven fully present elsewhere needs no re-walk, nor does one already
    // walked as spine at this same path, nor one a candidate base carries here. The same tree at
    // another path is walked again: the scope selects different entries there.
    if bases.iter().any(|base| base == tree_hash)
        || verified.contains(tree_hash)
        || !spine_seen.insert((tree_hash.to_string(), prefix.to_string()))
    {
        return Ok(());
    }

    if sealed_absent(sealed, tree_hash, blob_exists)? {
        return Ok(());
    }

    // Loading re-hashes the object (content-addressed read), so a corrupted spine tree fails here.
    let tree = object_utils::load_tree(tree_hash)
        .map_err(|e| format!("Tree {} is missing or unreadable: {}", tree_hash, e))?;
    let (base_files, base_subtrees) = base_entries(bases, load_base_tree);

    for (name, file) in tree.get_files() {
        let path = child_path(prefix, &file.name);
        if scope.classify(&path) == ScopeClass::OutOfScope {
            continue; // sealed by hash, never fetched
        }
        if base_files.get(name).is_some_and(|hashes| hashes.contains(&file.hash)) {
            continue; // a base carries it here: present by the base's own verification
        }
        if !blob_exists(&file.hash)? {
            if holds_absent(sealed, &file.hash) {
                continue; // held by hash alone: a chunked file's recipe seals its chunks too
            }
            return Err(format!(
                "Blob {} (\"{}\" in tree {}) is missing.",
                file.hash, file.name, tree_hash
            ));
        }

        // An in-scope chunked file on the spine: its recipe is present (checked just above), so
        // descend it and verify its chunks at the audit's content level, exactly as the
        // fully-in-scope walk does.
        if file.item_type.is_chunked() {
            verify_chunked_file(&file.hash, reverify, sealed, load_recipe_chunks, chunks_missing)?;
        }
    }

    for (name, subtree) in tree.get_subtrees() {
        let path = child_path(prefix, &subtree.name);
        if scope.classify(&path) == ScopeClass::OutOfScope {
            continue; // sealed by hash, never descended
        }
        verify_tree_closure_scoped(
            &subtree.hash, &path, base_subtrees.get(name).map_or(&[][..], Vec::as_slice), verified,
            spine_seen, blob_exists, chunks_missing, load_recipe_chunks, load_base_tree, scope, reverify,
            sealed,
        )?;
    }

    Ok(())
}

/// The candidate bases' entries one level down, by name and unioned: the file hashes each base
/// carries under a name, and the distinct subtree hashes. Read tolerantly per base: a base that
/// cannot be read contributes no explanations, so the walk verifies more rather than less.
#[allow(clippy::type_complexity)]
fn base_entries(bases: &[String],
                load_base_tree: &dyn Fn(&str) -> Result<TreeItem, String>)
                -> (HashMap<String, HashSet<String>>, HashMap<String, Vec<String>>) {
    let mut base_files: HashMap<String, HashSet<String>> = HashMap::new();
    let mut base_subtrees: HashMap<String, Vec<String>> = HashMap::new();

    for base in bases {
        let Ok(base_tree) = load_base_tree(base) else { continue };

        for (name, file) in base_tree.get_files() {
            base_files.entry(name.clone()).or_default().insert(file.hash.clone());
        }
        for (name, subtree) in base_tree.get_subtrees() {
            let candidates = base_subtrees.entry(name.clone()).or_default();

            // Two candidate bases carrying the identical child at this name would otherwise each
            // be loaded again one level down.
            if !candidates.contains(&subtree.hash) {
                candidates.push(subtree.hash.clone());
            }
        }
    }

    (base_files, base_subtrees)
}

/// The warehouse path of a tree entry named `name` under directory `prefix` (`""` = the root).
pub(crate) fn child_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", prefix, name)
    }
}

/// Verify that a tree and everything below it is present in the object store, **pruning any
/// subtree some candidate base already carries at that path, and any whose hash this walk has
/// already settled anywhere** (§9.4b W1).
///
/// Those are two prunes, and only the first is path-keyed. A hash settled once — by matching a
/// base at its own path, or by being walked in full — is settled for every later occurrence of
/// that hash at any path, because a closure is a property of content and content is what a hash
/// names. So a directory copied to a second path is free once its source has been settled, and
/// descended in full when the copy is reached first (sibling order decides which, and the
/// relocation test asserts both directions rather than the flattering one).
///
/// **The prune, and its soundness invariant.** [`verify_closure_core`] bounds *which
/// parcels* this runs for (only the new segment no bound reaches); this bounds *how much of
/// each new parcel's tree* is walked. A subtree whose hash equals a candidate base's subtree at the
/// same path is skipped whole — the invariant that makes this sound: *a subtree hash identical to
/// one whose closure has already been established has, by content-addressing, the identical
/// closure.* The establishing act differs by candidate: for the walk's `base` (one of its bounds) it
/// was proven when that head was written; for an immediate parent it was proven by that parent's own top-level
/// audit earlier in this same call, which is what the parents-first order guarantees.
/// Re-checking it here would not be meaningless — it would catch bit-rot the store suffered
/// *since* that commit — but that guarantee was never this walk's job: store durability between
/// commits is a store property (`gc`/`audit` re-prove it independently, and a periodic
/// `audit --full` re-reads content precisely because a push never re-scrubs it), not
/// something the commit-gate closure check was ever chartered to
/// re-verify on every push. This is the same induction the incremental audit already rests on (a
/// parcel lost behind a bound does not fail this walk either), extended from parcels down
/// through the tree.
///
/// Before the prune, every push re-presence-checked **every** blob in the entire tree, not just
/// the changed ones, and the recipe→chunk descent (W4) multiplied that by a chunked file's chunk
/// count: a maximal 64 MiB recipe lists ~987,000 chunks, so a push touching a directory that
/// merely *contained* such a file — even when that file itself did not change — cost ~a million
/// synchronous presence checks (an S3 `HEAD` apiece on the AWS head) inside the commit Lambda's
/// own invocation. With the prune, an unchanged chunked file's subtree is pruned by pure hash
/// comparison, loading neither its tree nor its recipe and checking none of its chunks. It mirrors
/// `collect_changed_closure`'s client-side prune, which settles a hash before checking it for the
/// same reason this walk does. The two candidate sets are deliberately *not* the same: the client
/// prunes against the immediate parents only, this walk against those **plus** the bound's
/// (see [`verify_closure_core`] on why the union rather than a replacement). The server's
/// set is the superset, so nothing the client uploads is refused here for want of an explanation.
///
/// **Completeness (W4 preserved).** A subtree or file that is explained by no candidate base is
/// walked exactly as before — a changed chunked file still descends its recipe and presence-checks
/// every chunk **non-tolerantly**. The audit never advances a ref over content whose presence
/// neither an accepted push nor an earlier-audited member of the same call has established.
///
/// That sentence replaces an accepted-push-only one, and the difference is the whole point of the
/// candidate set. A base is no longer only the bound's subtree: it is that **plus** the
/// parcel's own immediate parents' subtrees at the same path. A parent inside this same push
/// vouches for content only once its own top-level audit has run, which is why
/// [`verify_closure_core`] orders the segment parents-first — the marking discipline below
/// is sound under that order and not otherwise.
///
/// # Arguments
/// * `tree_hash`        - The root of the subtree to verify.
/// * `base_tree_hashes` - The candidate bases' subtrees at this same path: the walk's `base`'s, and
///                        each immediate parent's. Empty for a creation or a newly-introduced path.
///                        A match against **any** of them prunes the subtree without a load.
/// * `visited_trees`    - Trees already settled (shared across parcels of one walk). Recorded
///                        **before** the base check, matching `collect_changed_closure`: the same
///                        subtree hash can recur at another path in the same walk, and that
///                        recurrence must be recognized. Sound only parents-first — a child that
///                        marked a parent-explained hash would otherwise short-circuit the
///                        explaining parent's own audit.
/// * `load_base_tree`   - Reads a candidate base's tree (from the local store, or object storage on
///                        the AWS head) so a changed subtree's children can be compared to the
///                        bases'; tolerant per base, because a base is either already-audited
///                        ancestry or a parcel this same call has audited.
/// * `reverify`         - The `audit --full` content level for a changed chunked file (see
///                        [`verify_chunked_file`]); the commit gate always passes `false`.
///
/// # Returns
/// * `Ok(())`      - If the whole (changed) subtree is present.
/// * `Err(String)` - If a tree or blob is missing (or unreadable).
#[allow(clippy::too_many_arguments)]
fn verify_tree_closure(tree_hash: &str,
                       base_tree_hashes: &[String],
                       visited_trees: &mut HashSet<String>,
                       blob_exists: &dyn Fn(&str) -> Result<bool, String>,
                       chunks_missing: &dyn Fn(&[String]) -> Result<Vec<String>, String>,
                       load_recipe_chunks: &dyn Fn(&str) -> Result<Vec<String>, String>,
                       load_base_tree: &dyn Fn(&str) -> Result<TreeItem, String>,
                       reverify: bool,
                       sealed: Option<&SealedTolerance>)
                       -> Result<(), String> {
    // An explicit stack rather than recursion: a ref write runs this walk, and a tree nested a
    // few thousand levels deep would otherwise overflow the thread's stack.
    let mut pending: Vec<(String, Vec<String>)> = vec![(tree_hash.to_string(), base_tree_hashes.to_vec())];

    while let Some((tree_hash, base_tree_hashes)) = pending.pop() {
        // Record the visit before checking base-explained, not after — the same discipline
        // `collect_changed_closure` states on the client side. Content-addressing means one subtree
        // hash can recur at another path in the same walk (a merge adopting one side's subtree under
        // two names), and a hash settled once needs no second descent. Marking before the check is
        // what makes the recurrence free; it is sound only because `verify_closure_core`
        // processes the segment parents-first, so a hash a child pruned against a parent is one that
        // parent's own audit has already settled.
        // Held by hash alone: tolerated here, but never memoized as a closure proven present.
        if sealed_absent(sealed, &tree_hash, blob_exists)? {
            continue;
        }

        if !visited_trees.insert(tree_hash.clone()) {
            continue;
        }

        // Explained by some candidate base at this path: identical hash ⟹ identical closure ⟹ present,
        // either because a bound vouched for it or because an earlier-audited parcel of this
        // same call did. Pruned without loading any tree — this is the whole W1 saving, and it is
        // exactly where an untouched large chunked file's ~million chunks are NOT re-presence-checked.
        if base_tree_hashes.contains(&tree_hash) {
            continue;
        }

        let tree = object_utils::load_tree(&tree_hash)
            .map_err(|e| format!("Tree {} is missing or unreadable: {}", tree_hash, e))?;

        // The candidate bases' entries at this path, by name, unioned — a child is explained if *any*
        // base carries that name at that hash. Loaded only for a tree no base explained whole, and
        // tolerantly per base: a base that cannot be read contributes no explanations, so the walk
        // verifies more rather than less, which is always sound.
        //
        // Sound is not free. A base is trusted ancestry or a parcel this same call just audited, so on
        // the AWS head a failure here means a transient store error, not a missing object — and the
        // `continue` converts that blip into a full descent of everything the base would have
        // explained, up to an untouched multi-gigabyte file's recipe and every chunk `HEAD` under the
        // 29 s gateway ceiling. The operator then sees a timeout rather than the store error that
        // caused it. Two to three bases per parcel instead of one scales the chance of hitting it, so
        // the tolerance is a deliberate trade of a rare slow refusal against a wrong fast one, not a
        // free win.
        let (base_files, mut base_subtrees) = base_entries(&base_tree_hashes, load_base_tree);

        for (name, file) in tree.get_files() {
            // Base-explained: this exact file hash sits at this path under some candidate base, so its
            // blob — and, for a chunked file, its whole recipe→chunk closure — is present by induction.
            // Skipping it is what makes an unchanged file cost nothing on a push.
            if base_files.get(name).is_some_and(|hashes| hashes.contains(&file.hash)) {
                continue;
            }

            if !blob_exists(&file.hash)? {
                if holds_absent(sealed, &file.hash) {
                    continue; // held by hash alone: a chunked file's recipe seals its chunks too
                }
                return Err(format!(
                    "Blob {} (\"{}\" in tree {}) is missing.",
                    file.hash, file.name, tree_hash
                ));
            }

            // A *changed* chunked file's tree-entry hash names a recipe; its chunks are reachable only
            // through the recipe. Non-tolerant on purpose — this is the commit-gate closure audit
            // (§9.4b W4): a signed ref must never advance over a chunked file whose chunks are not all
            // present, or the file becomes silently unmaterializable the moment someone fetches it.
            // (gc's own descent is presence-*tolerant*; this one is the opposite, exactly because a
            // ref move is a durability promise a fetch is not.) The prune above only reaches this for
            // a file no candidate base already vouches for, so W4 is never weakened.
            if file.item_type.is_chunked() {
                verify_chunked_file(&file.hash, reverify, sealed, load_recipe_chunks, chunks_missing)?;
            }
        }

        // Pushed in reverse so they are walked by name, as a recursive walk would: a subtree hash
        // recurring under a later name is free once the earlier one settled it.
        let children: Vec<_> = tree.get_subtrees()
            .map(|(name, subtree)| (subtree.hash.clone(), base_subtrees.remove(name).unwrap_or_default()))
            .collect();
        pending.extend(children.into_iter().rev());
    }

    Ok(())
}

/// Verify a chunked file's chunks at the audit's content level.
///
/// * **Presence** (`reverify = false`, the commit gate and a normal audit) — presence-check every
///   chunk non-tolerantly via [`verify_recipe_chunks_present`], reading no chunk bytes. This is the
///   §9.4b W4 commit-gate guarantee (a ref never advances over a chunked file whose chunks are not
///   all present) and the bounded cost a normal audit pays.
/// * **Re-verify** (`reverify = true`, `audit --full`) — stream-assemble the whole file: every
///   chunk's bytes are re-read (and so re-hashed on the content-addressed load, catching on-disk
///   corruption a presence check cannot) *and* the recipe's own assembly claim is checked,
///   `Blake3(assembled) == recipe.content_hash` — the one integrity property a normal audit never
///   proves (the W3 residual). Bounded to one chunk in memory, streamed to a sink. Reads chunks from
///   the local store, so it is the `audit --full` path only, never a remote commit gate.
///
/// A chunk recorded as sealed by hash is tolerated absent on both paths. A file one of whose chunks
/// is held that way cannot be assembled, so re-verify re-reads and re-hashes its present chunks
/// instead, and its recipe's content hash goes unchecked.
///
/// # Arguments
/// * `recipe_hash`        - The chunked file's tree-entry hash (a recipe).
/// * `reverify`           - `true` re-reads and re-assembles; `false` presence-checks.
/// * `sealed`             - The sealed-by-hash record, or `None` on the push gate.
/// * `load_recipe_chunks` - Reads the recipe's chunk hashes.
/// * `chunks_missing`     - Bulk chunk-presence probe: given the recipe's chunk-hash list, returns
///                          the subset the store lacks.
fn verify_chunked_file(
    recipe_hash: &str,
    reverify: bool,
    sealed: Option<&SealedTolerance>,
    load_recipe_chunks: &dyn Fn(&str) -> Result<Vec<String>, String>,
    chunks_missing: &dyn Fn(&[String]) -> Result<Vec<String>, String>,
) -> Result<(), String> {
    // Absent chunks the record does not hold; those it holds are tolerated.
    let unheld = |mut missing: Vec<String>| -> Vec<String> {
        missing.retain(|chunk| !holds_absent(sealed, chunk));
        missing
    };

    if !reverify {
        return verify_recipe_chunks_present(recipe_hash, load_recipe_chunks, &|chunks| Ok(unheld(chunks_missing(chunks)?)));
    }

    if sealed.is_none_or(SealedTolerance::is_empty) {
        object_utils::assemble_chunked_file(recipe_hash, &mut std::io::sink())?;
        return Ok(());
    }

    let chunks = load_recipe_chunks(recipe_hash)
        .map_err(|e| format!("Recipe {} is missing or unreadable: {}", recipe_hash, e))?;
    let absent = chunks_missing(&chunks)?;

    if absent.is_empty() {
        object_utils::assemble_chunked_file(recipe_hash, &mut std::io::sink())?;
        return Ok(());
    }

    report_missing_chunks(recipe_hash, &chunks, &|_| Ok(unheld(absent.clone())))?;

    // Every absent chunk is held by hash, so the file cannot be assembled: re-read the present
    // ones, each once, re-hashed on its content-addressed load (the same read assembly makes).
    let mut skip: HashSet<&str> = absent.iter().map(String::as_str).collect();
    for chunk in &chunks {
        if !skip.insert(chunk.as_str()) {
            continue;
        }
        object_utils::load_chunk(chunk)
            .map_err(|e| format!("Chunk {} of recipe {} is corrupt or unreadable: {}", chunk, recipe_hash, e))?;
    }

    Ok(())
}

/// Whether a tree the walk is about to load is recorded as sealed by hash and absent: held by hash
/// alone, so neither it nor anything beneath it is checked. A recorded tree that is present seals
/// nothing and is walked. Probes presence only for a recorded hash.
fn sealed_absent(sealed: Option<&SealedTolerance>,
                 tree_hash: &str,
                 object_exists: &dyn Fn(&str) -> Result<bool, String>) -> Result<bool, String> {
    let Some(sealed) = sealed.filter(|sealed| sealed.record.contains(tree_hash)) else {
        return Ok(false);
    };
    Ok(!object_exists(tree_hash)? && sealed.holds_absent(tree_hash))
}

/// How many absent chunk hashes a closure-check failure names before summarizing the rest as
/// "(and N more)". A maximal 64 MiB recipe lists ~987k chunks, so an all-missing failure must not
/// build a megabyte-long message; a handful of names is enough for an operator to act on, and the
/// count is always reported exactly regardless.
const MAX_NAMED_MISSING_CHUNKS: usize = 32;

/// The serial chunk-presence probe every non-AWS head uses. A chunk is present iff its object is on
/// disk (or in a pack) — a microsecond lookup, so there is nothing to parallelize; it returns the
/// absent subset, in the given order. The AWS serverless head overrides this *shape* with a
/// bounded-concurrency batch of S3 `HEAD`s ([`crate`]-external: `ObjectStore::objects_missing`),
/// where a probe is a network round trip and a large recipe lists thousands of chunks — the whole
/// reason the seam is a bulk `&[hash] -> missing` call rather than a per-hash existence check.
fn local_chunks_missing(chunks: &[String]) -> Result<Vec<String>, String> {
    let mut missing = Vec::new();

    for chunk in chunks {
        if !file_utils::does_object_exist(chunk)? {
            missing.push(chunk.clone());
        }
    }

    Ok(missing)
}

/// Presence-check a recipe's chunks non-tolerantly, in **one bulk probe**: load the recipe's chunk
/// list (via the caller's store-appropriate reader) and hand the whole list to `chunks_missing`,
/// which returns the subset the store lacks. Any missing chunk fails the walk, naming the absent
/// ones. The recipe's own presence is the caller's responsibility (it is checked as an ordinary
/// file-entry object before this).
///
/// The bulk shape is the one seam each head fills differently, and the reason this is not a
/// per-chunk existence loop. A local/self-host head answers it with a serial filesystem/pack scan
/// (see [`local_chunks_missing`]); the AWS serverless head answers it with a bounded-concurrency
/// batch of S3 `HEAD`s, so a changed large file's thousands of chunks are verified in a second or
/// two rather than one slow round trip apiece behind API Gateway's hard timeout. The walk itself
/// stays store-agnostic: it only ever hands over the full chunk list and asks which are absent.
///
/// Called from every walk that reaches this recipe's tree entry, whether or not the recipe changed
/// in the parcel being verified — the W1 subtree prune (see [`verify_tree_closure`]) is what keeps
/// an *unchanged* large file from being probed at all; this bulk form is what keeps a *changed* one
/// affordable.
///
/// # Arguments
/// * `recipe_hash`        - The recipe (a chunked file's tree-entry hash) whose chunks are checked.
/// * `load_recipe_chunks` - Reads the recipe's ordered chunk hashes (local store, or object store).
/// * `chunks_missing`     - Given a recipe's full chunk-hash list, returns the subset the store
///                          lacks (in any order). Non-tolerant: a non-empty result fails the walk.
fn verify_recipe_chunks_present(
    recipe_hash: &str,
    load_recipe_chunks: &dyn Fn(&str) -> Result<Vec<String>, String>,
    chunks_missing: &dyn Fn(&[String]) -> Result<Vec<String>, String>,
) -> Result<(), String> {
    let chunks = load_recipe_chunks(recipe_hash)
        .map_err(|e| format!("Recipe {} is missing or unreadable: {}", recipe_hash, e))?;

    report_missing_chunks(recipe_hash, &chunks, chunks_missing)
}

/// The half of [`verify_recipe_chunks_present`] after the recipe is read: ask `chunks_missing`
/// which of `chunks` the store lacks, and fail naming them. For a caller that has already read the
/// recipe itself, to tell an unreadable one apart from one with absent chunks.
pub(crate) fn report_missing_chunks(
    recipe_hash: &str,
    chunks: &[String],
    chunks_missing: &dyn Fn(&[String]) -> Result<Vec<String>, String>,
) -> Result<(), String> {
    let mut missing = chunks_missing(chunks)?;

    if missing.is_empty() {
        return Ok(());
    }

    // Deterministic, bounded reporting. The bulk probe may return the absent chunks in any order
    // (the AWS head's parallel `HEAD`s finish out of order), so sort and de-duplicate for a stable
    // message, and name at most `MAX_NAMED_MISSING_CHUNKS` of them: an all-missing maximal recipe
    // would otherwise build a message of ~987k hashes. The reported *count* is always exact.
    missing.sort_unstable();
    missing.dedup();

    let shown: Vec<&str> =
        missing.iter().take(MAX_NAMED_MISSING_CHUNKS).map(String::as_str).collect();
    let overflow = missing.len() - shown.len();
    let and_more = if overflow > 0 { format!(" (and {} more)", overflow) } else { String::new() };

    Err(format!(
        "Recipe {} is missing {} of its {} chunk(s): {}{}. The chunked file cannot be materialized.",
        recipe_hash,
        missing.len(),
        chunks.len(),
        shown.join(", "),
        and_more,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use crate::util::office_utils::{
        key_endorsement_payload, key_pop_payload, KeyRecord, Role, UserRecord,
    };
    use crate::util::sign_utils::to_hex;
    use crate::util::pallet_utils::RefUpdateError;

    /// The office's current head: the base each test's office parcel stacks on.
    fn office_head() -> Option<String> {
        crate::util::pallet_utils::get_meta_pallet_head(crate::util::office_utils::OFFICE_PALLET_NAME).unwrap()
    }

    const ALICE: &str = "a11ce000-0000-4000-8000-00000000a11c";
    const BOB: &str = "b0b00000-0000-4000-8000-000000000b0b";

    /// A deterministic keypair for tests (no key files involved).
    fn keypair(seed: u8) -> (SigningKey, String, String) {
        let signing_key = SigningKey::from_bytes(&[seed; 32]);
        let public_hex = to_hex(signing_key.verifying_key().as_bytes());
        let key_id = crate::util::sign_utils::key_id_for_public_key(
            signing_key.verifying_key().as_bytes()
        );

        (signing_key, key_id, public_hex)
    }

    fn user(identifier: &str, role: Role, identity_root: &str) -> UserRecord {
        UserRecord {
            identifier: identifier.to_string(),
            enrolled_at: 1,
            role,
            pallets: Vec::new(),
            identity_root: identity_root.to_string(),
            class: crate::util::office_utils::IdentityClass::Human,
            supervisor: None,
        }
    }

    /// A fully endorsed key record, signed in-memory.
    fn endorsed_key(operator: &str,
                    key: &SigningKey,
                    key_id: &str,
                    public_hex: &str,
                    authorizer: &SigningKey,
                    authorized_by: &str) -> KeyRecord {
        let pop = key.sign(&key_pop_payload(public_hex, operator));
        let endorsement = authorizer.sign(
            &key_endorsement_payload(public_hex, operator, authorized_by, 1)
        );

        KeyRecord {
            key_id: key_id.to_string(),
            operator: operator.to_string(),
            public_key: public_hex.to_string(),
            issued_at: 1,
            retired_at: None,
            revocation_reason: None,
            distrust_boundary: Vec::new(),
            authorized_by: authorized_by.to_string(),
            endorsement: to_hex(&endorsement.to_bytes()),
            proof_of_possession: to_hex(&pop.to_bytes()),
        }
    }

    /// The genesis shape: one admin whose identity root is self-endorsed.
    fn genesis_state() -> (OfficeState, SigningKey, String, String) {
        let (key, key_id, public_hex) = keypair(7);
        let root = endorsed_key(ALICE, &key, &key_id, &public_hex, &key, &key_id);

        let state = OfficeState {
            users: vec![user(ALICE, Role::Admin, &key_id)],
            keys: vec![root],
        };

        (state, key, key_id, public_hex)
    }

    #[test]
    fn a_self_endorsed_identity_root_verifies_at_genesis() {
        let (state, _, _, _) = genesis_state();

        assert!(verify_new_key_endorsements(None, &state).is_ok());
    }

    #[test]
    fn a_self_endorsed_key_that_is_not_the_root_is_rejected() {
        let (mut state, _, root_id, _) = genesis_state();

        // A second key of Alice's endorsing itself: identity from nothing.
        let (rogue, rogue_id, rogue_hex) = keypair(9);
        state.keys.push(endorsed_key(ALICE, &rogue, &rogue_id, &rogue_hex, &rogue, &rogue_id));

        let error = match verify_new_key_endorsements(None, &state) {
            Err(KeyRefusal::Proof(reason)) => reason,
            other => panic!("expected a proof refusal, got {:?}", other),
        };
        assert!(error.contains("only the identity root"), "{}", error);
        assert!(error.contains(&root_id), "{}", error);
    }

    #[test]
    fn a_key_endorsed_by_the_operators_own_key_chains_to_the_root() {
        let (previous, root_key, root_id, _) = genesis_state();

        let (device2, device2_id, device2_hex) = keypair(11);
        let mut current = OfficeState {
            users: vec![user(ALICE, Role::Admin, &root_id)],
            keys: vec![
                previous.keys[0].clone(),
                endorsed_key(ALICE, &device2, &device2_id, &device2_hex, &root_key, &root_id),
            ],
        };

        assert!(verify_new_key_endorsements(Some(&previous), &current).is_ok());

        // Tampering with the endorsement is detected.
        current.keys[1].endorsement = "00".repeat(64);
        let error = match verify_new_key_endorsements(Some(&previous), &current) {
            Err(KeyRefusal::Proof(reason)) => reason,
            other => panic!("expected a proof refusal, got {:?}", other),
        };
        assert!(error.contains("does not verify"), "{}", error);
    }

    #[test]
    fn a_cycle_of_new_keys_cannot_manufacture_validity() {
        let (previous, _, root_id, _) = genesis_state();

        // Two new keys of Alice's endorsing each other — neither chains to the root.
        let (k1, k1_id, k1_hex) = keypair(21);
        let (k2, k2_id, k2_hex) = keypair(22);

        let current = OfficeState {
            users: vec![user(ALICE, Role::Admin, &root_id)],
            keys: vec![
                previous.keys[0].clone(),
                endorsed_key(ALICE, &k1, &k1_id, &k1_hex, &k2, &k2_id),
                endorsed_key(ALICE, &k2, &k2_id, &k2_hex, &k1, &k1_id),
            ],
        };

        let error = match verify_new_key_endorsements(Some(&previous), &current) {
            Err(KeyRefusal::Proof(reason)) => reason,
            other => panic!("expected a proof refusal, got {:?}", other),
        };
        assert!(error.contains("does not reach the identity root"), "{}", error);
    }

    #[test]
    fn an_admin_may_authorize_another_operators_key_but_a_writer_may_not() {
        let (previous, admin_key, admin_key_id, _) = genesis_state();

        // Bob is admitted with a root endorsed by the admin.
        let (bob, bob_id, bob_hex) = keypair(31);
        let current = OfficeState {
            users: vec![
                user(ALICE, Role::Admin, &previous.keys[0].key_id),
                user(BOB, Role::Writer, &bob_id),
            ],
            keys: vec![
                previous.keys[0].clone(),
                endorsed_key(BOB, &bob, &bob_id, &bob_hex, &admin_key, &admin_key_id),
            ],
        };

        assert!(verify_new_key_endorsements(Some(&previous), &current).is_ok());

        // A writer authorizing a third operator's key is rejected: the scope of a
        // key-authorization equals the scope of the authorizer's authority.
        let carol = "ca201000-0000-4000-8000-00000000ca20";
        let (carol_key, carol_id, carol_hex) = keypair(32);

        let mut next = OfficeState {
            users: vec![
                user(ALICE, Role::Admin, &previous.keys[0].key_id),
                user(BOB, Role::Writer, &bob_id),
                user(carol, Role::Writer, &carol_id),
            ],
            keys: vec![
                current.keys[0].clone(),
                current.keys[1].clone(),
                endorsed_key(carol, &carol_key, &carol_id, &carol_hex, &bob, &bob_id),
            ],
        };

        let error = match verify_new_key_endorsements(Some(&current), &next) {
            Err(KeyRefusal::Scope(reason)) => reason,
            other => panic!("expected a scope refusal, got {:?}", other),
        };
        assert!(error.contains("not an admin"), "{}", error);

        // At the genesis there is no earlier state to hold a privilege in, so the same
        // endorsement makes the state malformed rather than an overstep.
        match verify_new_key_endorsements(None, &next) {
            Err(KeyRefusal::Proof(reason)) => assert!(reason.contains("not an admin"), "{}", reason),
            other => panic!("expected a proof refusal at the genesis, got {:?}", other),
        }

        // The same admission endorsed by the admin passes.
        next.keys[2] = endorsed_key(carol, &carol_key, &carol_id, &carol_hex, &admin_key, &admin_key_id);
        assert!(verify_new_key_endorsements(Some(&current), &next).is_ok());
    }

    #[test]
    fn a_stolen_proof_of_possession_cannot_be_reattributed() {
        // The PoP binds a key to its operator id: taking a consenting key and
        // enrolling it under a different id must fail verification.
        let (previous, admin_key, admin_key_id, _) = genesis_state();

        let (bob, bob_id, bob_hex) = keypair(41);
        let mut key = endorsed_key(BOB, &bob, &bob_id, &bob_hex, &admin_key, &admin_key_id);

        // Mallory re-attributes Bob's key (with Bob's genuine PoP) to her own id.
        let mallory = "ma110000-0000-4000-8000-0000000000ma";
        key.operator = mallory.to_string();
        key.endorsement = to_hex(&admin_key.sign(
            &key_endorsement_payload(&bob_hex, mallory, &admin_key_id, 1)
        ).to_bytes());

        let current = OfficeState {
            users: vec![
                user(ALICE, Role::Admin, &previous.keys[0].key_id),
                user(mallory, Role::Writer, &bob_id),
            ],
            keys: vec![previous.keys[0].clone(), key],
        };

        let error = match verify_new_key_endorsements(Some(&previous), &current) {
            Err(KeyRefusal::Proof(reason)) => reason,
            other => panic!("expected a proof refusal, got {:?}", other),
        };
        assert!(error.contains("proof-of-possession does not verify"), "{}", error);
    }

    #[test]
    fn revocations_are_append_once_and_carry_a_reason_for_everyone() {
        use crate::util::office_utils::RevocationReason;

        let (previous, _, root_id, _) = genesis_state();

        let mut revoked = previous.keys[0].clone();
        revoked.retired_at = Some(2);
        revoked.revocation_reason = Some(RevocationReason::Compromise);
        revoked.distrust_boundary = vec!["head-a".to_string()];

        let revoked_state = OfficeState {
            users: vec![user(ALICE, Role::Admin, &root_id)],
            keys: vec![revoked.clone()],
        };

        // Revoking without a reason is refused (even for admins — this check binds
        // every office parcel).
        let mut reasonless = revoked.clone();
        reasonless.revocation_reason = None;
        reasonless.distrust_boundary = Vec::new();
        let error = verify_key_permanence(&previous, &OfficeState {
            users: vec![user(ALICE, Role::Admin, &root_id)],
            keys: vec![reasonless],
        }).unwrap_err();
        assert!(error.contains("without a reason"), "{}", error);

        // A recorded revocation can never be lifted…
        let error = verify_key_permanence(&revoked_state, &previous).unwrap_err();
        assert!(error.contains("append-once"), "{}", error);

        // …nor can its boundary be quietly rewritten.
        let mut widened = revoked.clone();
        widened.distrust_boundary = vec!["head-a".to_string(), "head-b".to_string()];
        let error = verify_key_permanence(&revoked_state, &OfficeState {
            users: vec![user(ALICE, Role::Admin, &root_id)],
            keys: vec![widened],
        }).unwrap_err();
        assert!(error.contains("append-once"), "{}", error);

        // An identical revocation carried forward is fine.
        assert!(verify_key_permanence(&revoked_state, &revoked_state).is_ok());

        // Removing the key entirely is refused.
        let error = verify_key_permanence(&revoked_state, &OfficeState {
            users: vec![user(ALICE, Role::Admin, &root_id)],
            keys: Vec::new(),
        }).unwrap_err();
        assert!(error.contains("retained forever"), "{}", error);
    }

    #[test]
    fn a_revoked_key_cannot_endorse_new_keys() {
        use crate::util::office_utils::RevocationReason;

        let (mut previous, admin_key, admin_key_id, _) = genesis_state();

        // A second, still-active key for Alice so the state stays plausible.
        let (active, active_id, active_hex) = keypair(61);
        previous.keys.push(endorsed_key(ALICE, &active, &active_id, &active_hex, &admin_key, &admin_key_id));

        // The root is revoked…
        previous.keys[0].retired_at = Some(2);
        previous.keys[0].revocation_reason = Some(RevocationReason::Retirement);
        previous.keys[0].distrust_boundary = vec!["head".to_string()];

        // …and then "endorses" a new key: refused.
        let (newkey, new_id, new_hex) = keypair(62);
        let current = OfficeState {
            users: previous.users.clone(),
            keys: vec![
                previous.keys[0].clone(),
                previous.keys[1].clone(),
                endorsed_key(ALICE, &newkey, &new_id, &new_hex, &admin_key, &admin_key_id),
            ],
        };

        let error = match verify_new_key_endorsements(Some(&previous), &current) {
            Err(KeyRefusal::Proof(reason)) => reason,
            other => panic!("expected a proof refusal, got {:?}", other),
        };
        assert!(error.contains("revoked at that point"), "{}", error);
    }

    #[test]
    fn a_pinned_identity_root_must_exist_and_belong_to_the_user() {
        let (state, _, _, _) = genesis_state();

        let broken = OfficeState {
            users: vec![user(ALICE, Role::Admin, "missing-key-id")],
            keys: vec![state.keys[0].clone()],
        };

        let error = match verify_new_key_endorsements(None, &broken) {
            Err(KeyRefusal::Proof(reason)) => reason,
            other => panic!("expected a proof refusal, got {:?}", other),
        };
        assert!(error.contains("pins identity root"), "{}", error);
    }

    /// A poisoned memo is an internal fault, never a client's. Recover from it rather than
    /// reporting it: both server heads turn a failure of the memoized office verification
    /// into a `422`, which would tell a client its lift was invalid because the server had
    /// a bug. Nothing is lost — the memo only holds results that can be recomputed.
    #[test]
    fn a_poisoned_office_memo_recovers_instead_of_failing() {
        let memo = VERIFIED_OFFICE_CHAINS.get_or_init(|| Mutex::new(OfficeChainMemo::new()));

        // Panic while holding the lock, exactly as an internal fault would.
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = memo.lock().unwrap();
            panic!("a thread died holding the office memo");
        }));

        assert!(panicked.is_err());
        assert!(memo.is_poisoned(), "the lock is poisoned");

        // The memo is taken anyway, emptied, and usable again.
        {
            let mut chains = lock_memo(memo);
            assert!(chains.is_empty(), "a recovered memo starts clean");
            chains.insert("key".to_string(), OfficeState { users: Vec::new(), keys: Vec::new() });
        }

        assert!(!memo.is_poisoned(), "the poison is cleared, so the memo caches again");
        assert_eq!(lock_memo(memo).len(), 1, "and the entry survives the next lock");

        lock_memo(memo).clear();
    }

    /// A blank office state, cheap to construct, for tests that only care about the memo's
    /// bookkeeping and not about what it stores.
    fn blank_office_state() -> OfficeState {
        OfficeState { users: Vec::new(), keys: Vec::new() }
    }

    /// A server hosting more warehouses than [`MAX_MEMOIZED_OFFICE_CHAINS`] must never grow
    /// the memo past that bound — this is exercised against a locally constructed memo, not
    /// the process-global one, so it stays deterministic under parallel test execution.
    #[test]
    fn the_office_memo_never_exceeds_its_capacity() {
        let mut memo = OfficeChainMemo::new();

        for i in 0..(MAX_MEMOIZED_OFFICE_CHAINS * 2) {
            memo.insert(format!("key-{i}"), blank_office_state());
            assert!(
                memo.len() <= MAX_MEMOIZED_OFFICE_CHAINS,
                "the memo grew past capacity after inserting key-{i}"
            );
        }

        assert_eq!(memo.len(), MAX_MEMOIZED_OFFICE_CHAINS);
    }

    /// At capacity, a new key must evict only the least-recently-used entry — not the whole
    /// memo — and a recent hit must protect an entry from being that victim.
    #[test]
    fn the_office_memo_evicts_only_the_least_recently_used_entry() {
        let mut memo = OfficeChainMemo::new();

        for i in 0..MAX_MEMOIZED_OFFICE_CHAINS {
            memo.insert(format!("key-{i}"), blank_office_state());
        }

        // Touch "key-0" so it is now the most-recently-used; "key-1" becomes the least.
        assert!(memo.get("key-0").is_some());

        // One more distinct key, at capacity, forces exactly one eviction.
        memo.insert("key-new".to_string(), blank_office_state());

        assert_eq!(
            memo.len(),
            MAX_MEMOIZED_OFFICE_CHAINS,
            "capacity is preserved, not cleared"
        );
        assert!(memo.get("key-0").is_some(), "the recently-hit entry survives");
        assert!(memo.get("key-1").is_none(), "the least-recently-used entry was evicted");
        assert!(memo.get("key-new").is_some(), "the new entry was inserted");

        // Every other entry from the original fill is untouched — this was not a clear-all.
        for i in 2..MAX_MEMOIZED_OFFICE_CHAINS {
            assert!(memo.get(&format!("key-{i}")).is_some(), "key-{i} should not have been evicted");
        }
    }

    /// A fresh warehouse root for one test, entered as the active storage-root scope for its
    /// lifetime (mirrors `object_utils`'s own test fixture — kept local here since that one
    /// is private to its module).
    struct Scratch {
        _scope: crate::globals::StorageRootScope,
        root: std::path::PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "forklift-audit-test-{}-{}-{}", name, std::process::id(), id
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = crate::globals::StorageRootScope::enter(&root);
            Scratch { _scope: scope, root }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A minimal revoked `KeyRecord` for [`DistrustBoundaryMemo`] tests: only `key_id` and
    /// `distrust_boundary` matter to `resolvable`/`unresolved_head`, so everything else is a
    /// cheap placeholder.
    fn revoked_key(key_id: &str, distrust_boundary: Vec<String>) -> KeyRecord {
        KeyRecord {
            key_id: key_id.to_string(),
            operator: "someone".to_string(),
            public_key: "00".repeat(32),
            issued_at: 1,
            retired_at: Some(2),
            revocation_reason: Some(crate::util::office_utils::RevocationReason::Retirement),
            distrust_boundary,
            authorized_by: key_id.to_string(),
            endorsement: "00".repeat(64),
            proof_of_possession: "00".repeat(64),
        }
    }

    /// `resolvable` reads `true` only when every boundary head is actually present in the
    /// object store — not merely a plausible-looking hash — and `unresolved_head` names the
    /// first one it found missing. Boundary heads must be real, loadable parcels here (not
    /// arbitrary bytes at a matching hash): the walk behind `resolvable` parses each present
    /// node to keep walking its ancestry (see [`collect_reachable_present_noting_gaps`]), so a
    /// non-parcel object at a "present" hash would fail to decode instead of just counting as
    /// present.
    #[test]
    fn distrust_boundary_memo_resolvable_requires_every_head_present() {
        let _scratch = Scratch::new("distrust-boundary-resolvable");

        let present = store_parcel(Vec::new());
        let absent = object_utils::hash_object_bytes(b"a boundary head nobody ever fetched");

        let mut memo = DistrustBoundaryMemo::new();

        // Every head present: resolvable, and there is nothing missing to name.
        let all_present = revoked_key("key-all-present", vec![present.clone()]);
        assert!(memo.resolvable(&all_present).unwrap());
        assert_eq!(memo.unresolved_head(&all_present).unwrap(), None);

        // One head missing: not resolvable, and the missing one is named exactly.
        let missing_one = revoked_key("key-missing-one", vec![present.clone(), absent.clone()]);
        assert!(!memo.resolvable(&missing_one).unwrap());
        assert_eq!(memo.unresolved_head(&missing_one).unwrap(), Some(absent.clone()));

        // Memoized: a second call for the same key id does not re-derive a different answer
        // (there is nothing to change to, but this also proves the memo doesn't panic on a
        // repeat lookup for a key already resolved either way).
        assert!(memo.resolvable(&all_present).unwrap());
        assert!(!memo.resolvable(&missing_one).unwrap());
    }

    /// Build and store a minimal parcel, returning its hash. `actions` are irrelevant to
    /// signature/ancestry verification, so an empty description and no actions keep this
    /// cheap; `tree_hash` is never read by `verify_pallet_history` either, so a plausible-
    /// looking placeholder is fine.
    fn store_parcel(parents: Vec<String>) -> String {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::model::parcel::Parcel;

        let parcel = Parcel {
            tree_hash: "e".repeat(64),
            parents,
            actions: Vec::new(),
            description: None,
        };
        let mut object = LooseObjectBuilder::build_parcel(&parcel);
        let hash = object.hash.clone();
        object.store().unwrap();

        hash
    }

    /// Sign `hash` with `key` under `key_id` and store the sidecar, so `classify_parcel_trust`
    /// finds and verifies it against the matching `KeyRecord`.
    fn sign_and_store(hash: &str, key_id: &str, key: &SigningKey) {
        let raw = key.sign(hash.as_bytes());

        sign_utils::store_parcel_signature(hash, &sign_utils::ParcelSignature {
            key_id: key_id.to_string(),
            signature: raw.to_bytes().to_vec(),
        }).unwrap();
    }

    /// The genuine "unresolved" case, exercised directly against [`verify_pallet_history`]
    /// (the function `audit` and every server's ref-update check share): a parcel signed by a
    /// revoked key whose distrust boundary names a head this store has never had at all.
    ///
    /// This is deliberately *not* built through the CLI/clone machinery the way the
    /// sibling `remote.rs` tests are (see
    /// `a_partial_clone_missing_an_unrelated_boundary_head_still_audits_the_vouched_parcel`'s
    /// doc for why that path cannot reach this case): `office retire` always folds the
    /// audited pallet's own current head into the boundary, and that head — being the pallet
    /// under audit's own history — is guaranteed present and, being at or after any
    /// legitimately pre-revocation parcel on that same pallet, always sufficient to vouch it
    /// on its own. A boundary can only ever fail to resolve, without that trivial rescue,
    /// when every head it names is absent — which a hand-built `KeyRecord` can express
    /// directly, without fighting that structural rescue.
    #[test]
    fn a_boundary_head_absent_from_this_store_refuses_honestly_instead_of_alleging_tampering() {
        let _scratch = Scratch::new("distrust-boundary-unresolved-audit");

        let (admin_key, admin_id, admin_hex) = keypair(51);
        let (agent_key, agent_id, agent_hex) = keypair(52);

        let root = endorsed_key(ALICE, &admin_key, &admin_id, &admin_hex, &admin_key, &admin_id);
        let mut agent = endorsed_key(BOB, &agent_key, &agent_id, &agent_hex, &admin_key, &admin_id);

        // A boundary head this store never had — no object stored at this hash, ever.
        let never_fetched = object_utils::hash_object_bytes(b"a boundary head this store never fetched");
        agent.retired_at = Some(2);
        agent.revocation_reason = Some(crate::util::office_utils::RevocationReason::Compromise);
        agent.distrust_boundary = vec![never_fetched.clone()];

        let office_state = OfficeState {
            users: vec![user(ALICE, Role::Admin, &admin_id), user(BOB, Role::Writer, &agent_id)],
            keys: vec![root, agent],
        };

        // The target: a real, signed, present parcel — the revocation, not the parcel body,
        // is what makes this ambiguous.
        let target = store_parcel(Vec::new());
        sign_and_store(&target, &agent_id, &agent_key);

        let anchor = TrustAnchor {
            genesis: "0".repeat(64),
            enabled_at: 0,
            boundary: Vec::new(),
            prior_genesis: None,
            adopts: None,
        };

        let refusal = verify_pallet_history(&target, &anchor, &office_state, &[]).unwrap_err();
        let error = refusal.message;

        assert_eq!(refusal.gap, Some(BoundaryGap::Distrust { key_id: agent_id.clone(), missing: never_fetched.clone() }),
                   "the gap is typed, so a take can fetch the boundary and judge again");
        assert!(error.contains(&target), "names the parcel under question: {}", error);
        assert!(error.contains(&agent_id), "names the revoked key: {}", error);
        assert!(
            error.contains(&never_fetched),
            "names the specific absent boundary parcel: {}",
            error
        );
        assert!(
            !error.to_lowercase().contains("tampered"),
            "an unresolved boundary is a store limitation, not evidence of tampering: {}",
            error
        );
    }

    /// The case a heads-only presence pre-scan cannot catch: every one of the key's recorded
    /// `distrust_boundary` heads is present, but an *interior* ancestor the walk must cross to
    /// reach the signed parcel is not. Before the walk itself tracked gaps, a pre-scan of just
    /// the boundary heads would see nothing wrong (the one head named is right there) and
    /// `vouched` would come back `false` from a walk that quietly gave up at the gap — exactly
    /// reproducing the bug this fix round closed: a genuinely-in-boundary parcel misreported
    /// as tampering, purely because this store lost (or never had) something *behind* the
    /// boundary head, not the head itself.
    #[test]
    fn an_absent_interior_ancestor_behind_a_present_boundary_head_is_unresolved_not_suspect() {
        let _scratch = Scratch::new("distrust-boundary-interior-gap");

        let (admin_key, admin_id, admin_hex) = keypair(71);
        let (agent_key, agent_id, agent_hex) = keypair(72);

        let root = endorsed_key(ALICE, &admin_key, &admin_id, &admin_hex, &admin_key, &admin_id);
        let mut agent = endorsed_key(BOB, &agent_key, &agent_id, &agent_hex, &admin_key, &admin_id);

        // target <- missing_ancestor <- boundary_head: `target` genuinely sits inside the
        // boundary's ancestry, but the walk can never prove it — `missing_ancestor` is never
        // stored (as if lost, or never fetched), so a walk starting at the present
        // `boundary_head` runs out of road one hop short of `target`.
        let target = store_parcel(Vec::new());

        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::model::parcel::Parcel;

        let missing_ancestor_parcel = Parcel {
            tree_hash: "e".repeat(64),
            parents: vec![target.clone()],
            actions: Vec::new(),
            description: None,
        };
        // Computed, but deliberately never `.store()`d.
        let missing_ancestor = LooseObjectBuilder::build_parcel(&missing_ancestor_parcel).hash;

        let boundary_head = store_parcel(vec![missing_ancestor.clone()]);

        agent.retired_at = Some(2);
        agent.revocation_reason = Some(crate::util::office_utils::RevocationReason::Compromise);
        agent.distrust_boundary = vec![boundary_head];

        let office_state = OfficeState {
            users: vec![user(ALICE, Role::Admin, &admin_id), user(BOB, Role::Writer, &agent_id)],
            keys: vec![root, agent],
        };

        sign_and_store(&target, &agent_id, &agent_key);

        let anchor = TrustAnchor {
            genesis: "0".repeat(64),
            enabled_at: 0,
            boundary: Vec::new(),
            prior_genesis: None,
            adopts: None,
        };

        let refusal = verify_pallet_history(&target, &anchor, &office_state, &[]).unwrap_err();
        let error = refusal.message;

        assert_eq!(refusal.gap, Some(BoundaryGap::Distrust { key_id: agent_id.clone(), missing: missing_ancestor.clone() }),
                   "the gap is typed and names the interior parcel");
        assert!(error.contains(&target), "names the parcel under question: {}", error);
        assert!(
            error.contains(&missing_ancestor),
            "names the interior gap the walk actually hit, not just the (present) boundary \
             head: {}",
            error
        );
        assert!(
            !error.to_lowercase().contains("tampered"),
            "a gap behind a present boundary head is a store limitation, not evidence of \
             tampering: {}",
            error
        );
    }

    /// The bulk chunk-presence seam (§9.4b W4): passes when the store lacks nothing, and fails
    /// **non-tolerantly** naming *every* absent chunk when it does — the semantics the AWS head's
    /// parallel S3 probe and the local serial one must both honour. Driven through synthetic
    /// closures with hundreds of chunks — far more than a content-driven test can afford to
    /// materialize (a chunk is up to 4 MiB) — which is exactly the seam each head fills for real.
    #[test]
    fn the_bulk_chunk_check_names_every_missing_chunk_and_passes_when_all_present() {
        let recipe = "f".repeat(64);
        let chunks: Vec<String> = (0..500u32).map(|i| format!("{:064x}", i)).collect();

        let load_recipe_chunks = |_: &str| -> Result<Vec<String>, String> { Ok(chunks.clone()) };

        // Nothing missing: the bulk probe returns an empty set and the walk passes.
        let none_missing = |_: &[String]| -> Result<Vec<String>, String> { Ok(Vec::new()) };
        verify_recipe_chunks_present(&recipe, &load_recipe_chunks, &none_missing)
            .expect("with no chunk missing the presence check passes");

        // A handful missing, scattered through the list (including the last, which a walk that
        // stopped at the first absent chunk would never report).
        let victims = vec![chunks[3].clone(), chunks[128].clone(), chunks[499].clone()];
        let probed = victims.clone();
        let some_missing = move |asked: &[String]| -> Result<Vec<String>, String> {
            // A store only ever names hashes it was actually asked about.
            Ok(asked.iter().filter(|h| probed.contains(h)).cloned().collect())
        };

        let err = verify_recipe_chunks_present(&recipe, &load_recipe_chunks, &some_missing)
            .expect_err("a missing chunk fails the closure check non-tolerantly");

        assert!(err.contains("missing"), "{}", err);
        for victim in &victims {
            assert!(err.contains(victim), "every missing chunk is named: {}", err);
        }
        assert!(err.contains("missing 3 of its 500"), "the exact count is reported: {}", err);
    }

    /// A failure that would name more than [`MAX_NAMED_MISSING_CHUNKS`] chunks caps the named list
    /// and summarizes the rest, so a pathological recipe (a maximal one lists ~987k chunks) cannot
    /// balloon the error message — while the reported count stays exact.
    #[test]
    fn the_bulk_chunk_check_caps_the_named_list_but_reports_the_exact_count() {
        let recipe = "e".repeat(64);
        let total = 1000usize;
        let chunks: Vec<String> = (0..total as u32).map(|i| format!("{:064x}", i)).collect();

        let load_recipe_chunks = |_: &str| -> Result<Vec<String>, String> { Ok(chunks.clone()) };
        let all_missing = |asked: &[String]| -> Result<Vec<String>, String> { Ok(asked.to_vec()) };

        let err = verify_recipe_chunks_present(&recipe, &load_recipe_chunks, &all_missing)
            .expect_err("every chunk missing must fail");

        assert!(err.contains(&format!("missing {} of its {} chunk(s)", total, total)), "{}", err);
        // Exactly `MAX_NAMED_MISSING_CHUNKS` are named, and the remainder is summarized.
        assert!(
            err.contains(&format!("(and {} more)", total - MAX_NAMED_MISSING_CHUNKS)),
            "the overflow is summarized rather than dumped: {}",
            err
        );
    }

    // -------------------------------------------------------------------------------
    // Office privileges in the chain walk — user-record permanence (FORK-76), FORK-220
    //
    // `verify_self_service_change` is the only governance check a non-admin's office
    // parcel faces (an admin's signature short-circuits past it entirely, see
    // `verify_parcel_privileges`). These tests build real, disk-signed office chains —
    // through the same `office_utils`/`sign_utils` calls the CLI's `office` commands
    // use — so a fixture that never actually reached the non-admin branch could not
    // pass by accident, and a hostile parcel is signed with the attacker's own real key,
    // not a stand-in.
    // -------------------------------------------------------------------------------

    /// Serializes every test here that touches `FORKLIFT_KEYS_DIR` (a process-global
    /// environment variable, not thread-local) so two tests never point the process at
    /// different key directories at once. Mirrors the same guard `forklift-server`'s own
    /// office-chain test fixture uses, for the same reason.
    static PRIVILEGE_KEYS_ENV_LOCK: Mutex<()> = Mutex::new(());

    /// A real, disk-backed office chain: a genesis admin with a self-endorsed identity
    /// root (exactly what `office enroll` produces), trust established.
    struct PrivilegeFixture {
        _scope: crate::globals::StorageRootScope,
        // `None` when a caller is holding `PRIVILEGE_KEYS_ENV_LOCK` itself in an outer
        // scope that must outlive this fixture (see `build`'s doc comment) — otherwise
        // `Some`, and the fixture releases the lock as part of its own drop, in field
        // order, right after `Drop::drop` restores `FORKLIFT_KEYS_DIR`.
        _keys_lock: Option<std::sync::MutexGuard<'static, ()>>,
        root: std::path::PathBuf,
        anchor: TrustAnchor,
        admin: crate::model::operator::Operator,
        admin_key_id: String,
        // What `FORKLIFT_KEYS_DIR` held before this fixture pointed it at its own
        // (about-to-be-deleted) directory — restored in `Drop` so a later test in this
        // binary never inherits a path this fixture already removed. `FORKLIFT_KEYS_DIR`
        // is process-global (see `PRIVILEGE_KEYS_ENV_LOCK`), so leaving it pointed at a
        // removed directory would flake the next signer that runs without the lock.
        previous_keys_dir: Option<String>,
    }

    impl PrivilegeFixture {
        fn new(name: &str) -> PrivilegeFixture {
            let keys_lock = PRIVILEGE_KEYS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            PrivilegeFixture::build(name, Some(keys_lock))
        }

        /// The body of `new`, taking the `PRIVILEGE_KEYS_ENV_LOCK` guard as a
        /// parameter instead of acquiring one itself, so a caller can choose who ends
        /// up owning it:
        ///
        /// - `new` passes `Some(lock)` (the normal case): the fixture takes ownership
        ///   and holds it for its own lifetime, releasing it in field-drop order right
        ///   after `Drop::drop` restores `FORKLIFT_KEYS_DIR` — unchanged from before.
        /// - A caller that must keep observing `FORKLIFT_KEYS_DIR` (both its own
        ///   writes and this fixture's) after the fixture is gone passes `None` and
        ///   keeps its own guard alive in an outer scope instead. This is what the
        ///   restore-on-drop falsifier below does: it asserts on `FORKLIFT_KEYS_DIR`
        ///   both right after construction and after `drop(fixture)`, and every one of
        ///   those assertions — not just construction — needs the lock held, or a
        ///   concurrent fixture from another test in this binary can be mid-flight and
        ///   change the value out from under the assertion. Handing the fixture `None`
        ///   means `drop(fixture)` only drops the fixture, not the lock; the caller's
        ///   own guard, still alive in its own scope, keeps every assertion covered
        ///   and is released only when that scope ends.
        fn build(
            name: &str,
            keys_lock: Option<std::sync::MutexGuard<'static, ()>>,
        ) -> PrivilegeFixture {
            let root = std::env::temp_dir().join(format!(
                "forklift-privilege-test-{}-{}", name, std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            let keys_dir = root.join("keys");
            std::fs::create_dir_all(&keys_dir).unwrap();
            let previous_keys_dir = std::env::var("FORKLIFT_KEYS_DIR").ok();
            std::env::set_var("FORKLIFT_KEYS_DIR", &keys_dir);

            let scope = crate::globals::StorageRootScope::enter(&root);
            crate::util::warehouse_utils::prepare_warehouse(crate::util::warehouse_utils::PrepareMode::Full).unwrap();

            let admin = crate::model::operator::Operator {
                identifier: ALICE.to_string(),
            };
            let (admin_key_id, admin_pub) = sign_utils::generate_keypair(&admin.identifier).unwrap();
            let pop = crate::util::office_utils::sign_key_pop(
                &admin_key_id, &admin_pub, &admin.identifier
            ).unwrap();
            let root_key = crate::util::office_utils::endorse_key(
                &admin_pub, &admin.identifier, &admin_key_id, &pop, 1_700_000_000
            ).unwrap();

            let state = OfficeState {
                users: vec![user(ALICE, Role::Admin, &admin_key_id)],
                keys: vec![root_key],
            };

            let genesis = crate::util::office_utils::stack_office_parcel(
                &state, &admin, "genesis".to_string(), &admin_key_id, office_head().as_deref(),
                &crate::util::pallet_utils::RefPreconditions::NONE,
            ).unwrap();

            let anchor = TrustAnchor {
                genesis,
                enabled_at: 1_700_000_000,
                boundary: Vec::new(),
                prior_genesis: None,
                adopts: None,
            };

            crate::util::office_utils::write_trust_anchor(&anchor).unwrap();

            PrivilegeFixture {
                _scope: scope, _keys_lock: keys_lock, root, anchor, admin, admin_key_id,
                previous_keys_dir,
            }
        }

        /// Admit an operator (admin-signed — a legitimate admission) and return their
        /// key id and the office parcel hash of the admission itself (the head right
        /// after it — the "previous" state the parcel under test builds on).
        fn admit(&self,
                identifier: &str,
                role: Role,
                class: crate::util::office_utils::IdentityClass,
                supervisor: Option<String>) -> (String, String) {
            let (key_id, public_key) = sign_utils::generate_keypair(identifier).unwrap();
            let pop = crate::util::office_utils::sign_key_pop(&key_id, &public_key, identifier).unwrap();
            let key = crate::util::office_utils::endorse_key(
                &public_key, identifier, &self.admin_key_id, &pop, 1_700_000_001
            ).unwrap();

            let mut state = crate::util::office_utils::read_office_state().unwrap();
            state.users.push(UserRecord {
                identifier: identifier.to_string(),
                enrolled_at: 1_700_000_001,
                role,
                pallets: Vec::new(),
                identity_root: key_id.clone(),
                class,
                supervisor,
            });
            state.keys.push(key);

            let head = crate::util::office_utils::stack_office_parcel(
                &state, &self.admin, format!("admit {}", identifier), &self.admin_key_id, office_head().as_deref(),
                &crate::util::pallet_utils::RefPreconditions::NONE,
            ).unwrap();

            (key_id, head)
        }
    }

    impl Drop for PrivilegeFixture {
        fn drop(&mut self) {
            match &self.previous_keys_dir {
                Some(previous) => std::env::set_var("FORKLIFT_KEYS_DIR", previous),
                None => std::env::remove_var("FORKLIFT_KEYS_DIR"),
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// The trust file's bytes as they stand.
    fn anchor_bytes() -> Vec<u8> {
        crate::util::office_utils::read_trust_anchor_bytes().unwrap().unwrap()
    }

    // Revert: `stack_meta_edits` writes with `RefPreconditions::NONE` → the edit lands under an
    // office that no longer stands.
    #[test]
    fn a_meta_write_judged_against_an_office_that_moved_is_refused_and_writes_nothing() {
        use crate::util::{kv_utils, meta_tree};

        let fixture = PrivilegeFixture::new("meta-office-moved");
        let office = verified_local_office().unwrap();
        fixture.admit(BOB, Role::Writer, crate::util::office_utils::IdentityClass::Human, None);
        let moved = office_head();

        let edit = meta_tree::set_cell(
            &kv_utils::cell_path(&"c".repeat(64), "ci"), &fixture.admin_key_id, kv_utils::KV_TEXT_SCHEMA,
            &kv_utils::text_value("green"),
        ).unwrap();
        let refused = meta_tree::stack_meta_edits(
            kv_utils::KV_PALLET_NAME, &kv_utils::KvRules, &office, &[edit], &fixture.admin, "ci",
            &meta_tree::MetaSigner::local(&fixture.admin_key_id),
        );

        assert_eq!(crate::util::pallet_utils::get_meta_pallet_head(kv_utils::KV_PALLET_NAME).unwrap(), None,
            "the meta pallet must be unchanged");
        let error = refused.expect_err("an office moved since it was verified must refuse the write");
        assert_eq!(error, RefUpdateError::OfficeMoved { found: moved }.to_string());
    }

    // Revert: `Signing::preconditions` gives `NotConsumed` for a signed choice → the parcel lands
    // signed under an office that no longer stands.
    #[test]
    fn a_signed_write_whose_office_moved_after_the_signing_choice_is_refused() {
        let fixture = PrivilegeFixture::new("signed-office-moved");
        let signing = crate::util::stack_utils::resolve_signing_key(&fixture.admin).unwrap();
        assert_eq!(signing.key_id(), Some(fixture.admin_key_id.as_str()));
        fixture.admit(BOB, Role::Writer, crate::util::office_utils::IdentityClass::Human, None);

        let refused = signing.write_head("main", None, &crate::util::pallet_utils::tests::stub_parcel("1"));

        assert_eq!(crate::util::pallet_utils::get_pallet_head("main").unwrap(), None, "the pallet must be unchanged");
        assert_eq!(refused, Err(RefUpdateError::OfficeMoved { found: office_head() }));
    }

    // Revert: `resolve_signing_key` reads the office without verifying it against the anchor → a
    // key is chosen from an office the anchor does not govern.
    #[test]
    fn a_signing_choice_refuses_an_office_the_anchor_does_not_govern() {
        let fixture = PrivilegeFixture::new("signing-unverified-office");
        let old_office = office_head().unwrap();

        // A new-chain anchor beside the old-chain office.
        crate::util::office_utils::install_trust(&crate::util::office_utils::TrustInstall {
            anchor: &TrustAnchor {
                genesis: "f".repeat(64),
                enabled_at: 1_700_000_002,
                boundary: Vec::new(),
                prior_genesis: Some(fixture.anchor.genesis.clone()),
                adopts: Some(old_office.clone()),
            },
            replaces: Some(&anchor_bytes()),
            office: Some(&old_office),
            new_office: None,
            pallets: crate::util::pallet_utils::PalletsPrecondition::NotConsumed,
        }).unwrap();

        let error = crate::util::stack_utils::resolve_signing_key(&fixture.admin).err()
            .expect("an office the anchor does not govern must not choose a key");

        assert_eq!(Err(error), verified_local_office().map(|_| ()), "the refusal is the audit's own");
    }

    // Revert: an office writer passes `AnchorPrecondition::NotConsumed` → the office moves under
    // an anchor its parcel was not judged against.
    #[test]
    fn an_office_write_judged_against_an_anchor_that_was_replaced_is_refused() {
        let fixture = PrivilegeFixture::new("office-anchor-moved");
        let head = office_head();
        let read = anchor_bytes();
        let state = crate::util::office_utils::read_office_state().unwrap();

        crate::util::office_utils::install_trust(&crate::util::office_utils::TrustInstall {
            anchor: &TrustAnchor {
                genesis: "f".repeat(64),
                enabled_at: 1_700_000_002,
                boundary: Vec::new(),
                prior_genesis: Some(fixture.anchor.genesis.clone()),
                adopts: head.clone(),
            },
            replaces: Some(&read),
            office: head.as_deref(),
            new_office: None,
            pallets: crate::util::pallet_utils::PalletsPrecondition::NotConsumed,
        }).unwrap();

        let refused = crate::util::office_utils::stack_office_parcel(
            &state, &fixture.admin, "late".to_string(), &fixture.admin_key_id, head.as_deref(),
            &crate::util::pallet_utils::RefPreconditions {
                office: crate::util::pallet_utils::OfficePrecondition::At(head.clone()),
                anchor: crate::util::pallet_utils::AnchorPrecondition::Bytes(read),
                pallets: crate::util::pallet_utils::PalletsPrecondition::NotConsumed,
            },
        );

        assert_eq!(office_head(), head, "the office must be unchanged");
        assert_eq!(refused, Err(RefUpdateError::AnchorMoved));
    }

    /// Falsifies the leak this fixture used to have: `FORKLIFT_KEYS_DIR` is
    /// process-global, and this fixture points it at a directory its own `Drop`
    /// deletes. Without restoring the prior value first, the process would be left
    /// with `FORKLIFT_KEYS_DIR` pointing at a removed path after the fixture goes
    /// away — flaking any later signer in this binary that runs without
    /// `PRIVILEGE_KEYS_ENV_LOCK`. Covers both directions: a prior value to restore,
    /// and no prior value (must end up unset again, not set to `""`).
    ///
    /// Every assertion below — including the ones after `drop(fixture)` — runs while
    /// this test's own `keys_lock` is held. Passing `PrivilegeFixture::build` `None`
    /// keeps that guard here instead of handing it to the fixture, so `drop(fixture)`
    /// releases only the fixture, not the lock: a concurrent fixture from another
    /// test in this binary cannot become mid-flight and change `FORKLIFT_KEYS_DIR`
    /// out from under an assertion. (An earlier version of this test handed the lock
    /// to the fixture and read `FORKLIFT_KEYS_DIR` after dropping it — unlocked. That
    /// version's own assertions could flake for the exact reason this test exists.)
    #[test]
    fn dropping_privilege_fixture_restores_the_previous_keys_dir_env_var() {
        const SENTINEL: &str = "/does/not/exist/forklift-restore-sentinel";

        // Case 1: FORKLIFT_KEYS_DIR pointed somewhere else before the fixture.
        {
            let keys_lock = PRIVILEGE_KEYS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var("FORKLIFT_KEYS_DIR", SENTINEL);
            let fixture = PrivilegeFixture::build("restore-check-was-set", None);
            assert_ne!(
                std::env::var("FORKLIFT_KEYS_DIR").ok(), Some(SENTINEL.to_string()),
                "fixture construction should have repointed FORKLIFT_KEYS_DIR at its own directory"
            );
            drop(fixture);
            assert_eq!(
                std::env::var("FORKLIFT_KEYS_DIR").ok(), Some(SENTINEL.to_string()),
                "dropping the fixture must restore FORKLIFT_KEYS_DIR to what it held before construction"
            );
            drop(keys_lock);
        }

        // Case 2: FORKLIFT_KEYS_DIR was not set at all before the fixture.
        {
            let keys_lock = PRIVILEGE_KEYS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::remove_var("FORKLIFT_KEYS_DIR");
            let fixture = PrivilegeFixture::build("restore-check-was-unset", None);
            drop(fixture);
            assert!(
                std::env::var("FORKLIFT_KEYS_DIR").is_err(),
                "dropping the fixture must leave FORKLIFT_KEYS_DIR unset if it started unset"
            );
            drop(keys_lock);
        }
    }

    /// Build a fixture with `BOB` admitted as a plain writer, apply `mutate` to Bob's own
    /// user record, sign the resulting parcel with Bob's own (real, disk) key, and return
    /// the refusal `verify_office_chain` raises for it.
    fn refuse_self_service_user_edit(name: &str, mutate: impl FnOnce(&mut UserRecord)) -> String {
        let fixture = PrivilegeFixture::new(name);
        let (bob_key_id, admitted_head) = fixture.admit(
            BOB, Role::Writer, crate::util::office_utils::IdentityClass::Human, None
        );
        let bob = crate::model::operator::Operator { identifier: BOB.to_string() };

        // The domain is non-empty: the chain up to the admission verifies.
        assert!(verify_office_chain(&fixture.anchor, &admitted_head).is_ok());

        let mut state = crate::util::office_utils::read_office_state().unwrap();
        let bob_record = state.users.iter_mut().find(|record| record.identifier == BOB)
            .expect("bob was just admitted");
        mutate(bob_record);

        let hostile = crate::util::office_utils::stack_office_parcel(
            &state, &bob, "self-service edit".to_string(), &bob_key_id, office_head().as_deref(),
            &crate::util::pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        let error = match verify_office_chain(&fixture.anchor, &hostile) {
            Err(OfficeChainError::Unauthorized { message: reason, .. }) => reason,
            other => panic!("a non-admin editing their own user record must be refused as \
                unauthorized: {:?}", other.err()),
        };

        // Proves the fixture actually reached `verify_self_service_change` (the
        // non-admin branch) rather than short-circuiting at the admin check: this exact
        // wording, naming the non-admin signer, is only produced by that branch (see
        // `verify_parcel_privileges`'s error-wrapping around it).
        assert!(error.contains("not an admin"), "{}", error);
        assert!(error.contains(BOB), "{}", error);
        assert!(error.contains("changes user records"), "{}", error);

        error
    }

    /// Falsifier 1: a non-admin relabeling their own `class` (e.g. laundering
    /// agent-authored work as human-authored) must be refused.
    #[test]
    fn a_non_admin_flipping_their_own_identity_class_is_refused() {
        refuse_self_service_user_edit("class-flip", |user| {
            user.class = crate::util::office_utils::IdentityClass::Agent;
        });
    }

    /// Falsifier 2: a non-admin repointing their own `supervisor` (attributing their
    /// actions to an operator who never agreed to supervise them) must be refused.
    #[test]
    fn a_non_admin_repointing_their_supervisor_is_refused() {
        refuse_self_service_user_edit("supervisor-flip", |user| {
            user.supervisor = Some("mallory-never-agreed-to-this".to_string());
        });
    }

    /// Falsifier 3: the class stays closed for a field beyond `class` and `supervisor`.
    /// `pallets` stands in here as a third, unrelated field; the mechanism that catches
    /// it is structural, not a per-field list — `UserRecord` derives `PartialEq`, so a
    /// field added to the struct in the future is compared automatically, the same way
    /// `pallets` is compared today, without touching `verify_self_service_change` at all.
    #[test]
    fn a_non_admin_changing_any_protected_user_field_is_refused() {
        refuse_self_service_user_edit("pallets-flip", |user| {
            user.pallets = vec!["stolen-pallet".to_string()];
        });
    }

    /// Falsifier 4 (over-tightened check): an admin-signed `class`/`supervisor` change
    /// still passes — the fix must not touch the admin short-circuit.
    #[test]
    fn an_admin_may_change_a_users_class_or_supervisor() {
        let fixture = PrivilegeFixture::new("admin-class-change");
        fixture.admit(
            BOB, Role::Writer, crate::util::office_utils::IdentityClass::Human, None
        );

        let mut state = crate::util::office_utils::read_office_state().unwrap();
        for record in state.users.iter_mut() {
            if record.identifier == BOB {
                record.class = crate::util::office_utils::IdentityClass::Bot;
                record.supervisor = None;
            }
        }

        let approved = crate::util::office_utils::stack_office_parcel(
            &state, &fixture.admin, "admin reclassifies bob".to_string(), &fixture.admin_key_id, office_head().as_deref(),
            &crate::util::pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        assert!(verify_office_chain(&fixture.anchor, &approved).is_ok());
    }

    /// Falsifier 5 (over-tightened check): a non-admin's self-service key rotation still
    /// passes — rotation touches only `OfficeState::keys`, never `UserRecord`.
    #[test]
    fn a_non_admin_self_service_key_rotation_still_passes() {
        let fixture = PrivilegeFixture::new("writer-key-rotation");
        let (bob_key_id, _) = fixture.admit(
            BOB, Role::Writer, crate::util::office_utils::IdentityClass::Human, None
        );
        let bob = crate::model::operator::Operator { identifier: BOB.to_string() };

        let (new_key_id, new_public) = sign_utils::generate_keypair(BOB).unwrap();
        let pop = crate::util::office_utils::sign_key_pop(&new_key_id, &new_public, BOB).unwrap();
        // A rotation authorized by the key it replaces — exactly what `office rotate` does.
        let new_key = crate::util::office_utils::endorse_key(
            &new_public, BOB, &bob_key_id, &pop, 1_700_000_002
        ).unwrap();

        let mut state = crate::util::office_utils::read_office_state().unwrap();
        for key in state.keys.iter_mut() {
            if key.operator == BOB && key.is_active() {
                key.retired_at = Some(1_700_000_002);
                key.revocation_reason = Some(crate::util::office_utils::RevocationReason::Retirement);
                // Boundary content is irrelevant to this check (`verify_key_permanence`
                // only forbids a *later* rewrite of an already-recorded boundary); empty
                // is fine for isolating the check under test.
                key.distrust_boundary = Vec::new();
            }
        }
        state.keys.push(new_key);

        let rotated = crate::util::office_utils::stack_office_parcel(
            &state, &bob, "bob rotates his own key".to_string(), &bob_key_id, office_head().as_deref(),
            &crate::util::pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        assert!(verify_office_chain(&fixture.anchor, &rotated).is_ok());
    }

    /// Falsifier 6 (over-tightened check): a non-admin parcel that changes nothing about
    /// user records still passes — here, linking a second device key without retiring
    /// anything (a pure key addition, `OfficeState::users` untouched byte-for-byte).
    #[test]
    fn a_non_admin_parcel_that_touches_no_user_record_still_passes() {
        let fixture = PrivilegeFixture::new("writer-link-device");
        let (bob_key_id, _) = fixture.admit(
            BOB, Role::Writer, crate::util::office_utils::IdentityClass::Human, None
        );
        let bob = crate::model::operator::Operator { identifier: BOB.to_string() };

        let (device_key_id, device_public) = sign_utils::generate_keypair(BOB).unwrap();
        let pop = crate::util::office_utils::sign_key_pop(&device_key_id, &device_public, BOB).unwrap();
        let device_key = crate::util::office_utils::endorse_key(
            &device_public, BOB, &bob_key_id, &pop, 1_700_000_002
        ).unwrap();

        let mut state = crate::util::office_utils::read_office_state().unwrap();
        state.keys.push(device_key);

        let linked = crate::util::office_utils::stack_office_parcel(
            &state, &bob, "bob links a second device".to_string(), &bob_key_id, office_head().as_deref(),
            &crate::util::pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        assert!(verify_office_chain(&fixture.anchor, &linked).is_ok());
    }

    /// FORK-74, at the site the client and `audit` rely on: a writer's office parcel that
    /// files its own public key under the admin's key id is refused by `verify_office_chain`.
    /// Before the fix the chain verified, and the resulting state resolved the admin's key id
    /// to the writer's key — so the writer's signatures would be attributed to the admin.
    /// It is refused as inauthentic, at parse time, before any privilege check runs.
    #[test]
    fn an_office_parcel_rebinding_a_key_id_to_another_public_key_is_refused() {
        let fixture = PrivilegeFixture::new("rebind-key-id");
        let (bob_key_id, admitted_head) = fixture.admit(
            BOB, Role::Writer, crate::util::office_utils::IdentityClass::Human, None
        );

        // The domain is non-empty: the admitted chain itself verifies.
        assert!(verify_office_chain(&fixture.anchor, &admitted_head).is_ok());

        let mut state = crate::util::office_utils::read_office_state().unwrap();
        let bob_public = state.find_key(&bob_key_id).unwrap().public_key.clone();
        let admin_key = state.keys.iter_mut()
            .find(|key| key.key_id == fixture.admin_key_id)
            .expect("the admin's root key is tracked");
        admin_key.public_key = bob_public;

        // The writer refuses this state, so file the records by hand, as a hostile client could.
        let (users, keys) = crate::util::office_utils::tests::naive_record_entries(&state);
        let hostile = crate::util::office_utils::tests::plant_office(
            &users, &keys, vec![admitted_head.clone()], Some(&bob_key_id)
        );

        let error = match verify_office_chain(&fixture.anchor, &hostile) {
            Err(OfficeChainError::Inauthentic(reason)) => reason,
            other => panic!("a key id rebound to another public key must not verify: {:?}", other.err()),
        };
        assert!(error.contains(&fixture.admin_key_id), "the error must name the key: {}", error);
        assert!(error.contains(&format!("keys/{}.toml", bob_key_id)),
            "the error must name the file the record belongs in: {}", error);
    }
    /// FORK-220: a writer promoting themselves to admin is refused by the walk every client and
    /// `audit` run, not only by a head's admission check. Before the fix the chain verified.
    #[test]
    fn a_writer_promoting_themselves_to_admin_is_refused() {
        refuse_self_service_user_edit("self-promotion", |user| {
            user.role = Role::Admin;
        });
    }

    /// FORK-220: reverting someone else's revocation is refused, whoever signs it. A writer
    /// trips key permanence the same way an admin does, so the second parcel (an admin's, from
    /// the same base) shows the rule is permanence and not the self-service check.
    #[test]
    fn un_revoking_another_users_key_is_refused_for_a_writer_and_an_admin() {
        const CAROL: &str = "ca401000-0000-4000-8000-0000000ca401";

        let fixture = PrivilegeFixture::new("unrevoke-foreign");
        let (bob_key_id, _) = fixture.admit(
            BOB, Role::Writer, crate::util::office_utils::IdentityClass::Human, None
        );
        let (carol_key_id, _) = fixture.admit(
            CAROL, Role::Writer, crate::util::office_utils::IdentityClass::Human, None
        );
        let bob = crate::model::operator::Operator { identifier: BOB.to_string() };

        // The admin revokes Carol's key: a legitimate change, so the domain is non-empty.
        let mut revoked = crate::util::office_utils::read_office_state().unwrap();
        let carol_key = revoked.keys.iter_mut().find(|key| key.key_id == carol_key_id).unwrap();
        carol_key.retired_at = Some(1_700_000_003);
        carol_key.revocation_reason = Some(crate::util::office_utils::RevocationReason::Compromise);
        carol_key.distrust_boundary = Vec::new();

        let revoked_head = crate::util::office_utils::stack_office_parcel(
            &revoked, &fixture.admin, "revoke carol".to_string(), &fixture.admin_key_id, office_head().as_deref(),
            &crate::util::pallet_utils::RefPreconditions::NONE,
        ).unwrap();
        assert!(verify_office_chain(&fixture.anchor, &revoked_head).is_ok());

        let mut restored = revoked.clone();
        let carol_key = restored.keys.iter_mut().find(|key| key.key_id == carol_key_id).unwrap();
        carol_key.retired_at = None;
        carol_key.revocation_reason = None;

        for (actor, key_id) in [(&bob, &bob_key_id), (&fixture.admin, &fixture.admin_key_id)] {
            let hostile = crate::util::office_utils::build_and_sign_office_parcel(
                &restored, actor, "un-revoke carol".to_string(), key_id, vec![revoked_head.clone()]
            ).unwrap();

            match verify_office_chain(&fixture.anchor, &hostile) {
                Err(OfficeChainError::Unauthorized { message: reason, .. }) => {
                    assert!(reason.contains(&hostile), "{}", reason);
                    assert!(reason.contains(&format!("alters the revocation of key {}", carol_key_id)),
                        "{}", reason);
                }
                other => panic!("{} un-revoking another user's key must be refused as \
                    unauthorized: {:?}", actor.identifier, other.err()),
            }
        }
    }

    /// FORK-220: the walk checks every parcel, not only the newest, so a violation stays refused
    /// under later, legitimate parcels. A client verifies a head it never saw grow.
    #[test]
    fn a_privilege_violation_below_legitimate_parcels_is_refused() {
        const CAROL: &str = "ca401000-0000-4000-8000-0000000ca401";

        let fixture = PrivilegeFixture::new("buried-promotion");
        let (bob_key_id, bob_admitted) = fixture.admit(
            BOB, Role::Writer, crate::util::office_utils::IdentityClass::Human, None
        );
        let bob = crate::model::operator::Operator { identifier: BOB.to_string() };

        let mut promoted = crate::util::office_utils::read_office_state().unwrap();
        promoted.users.iter_mut().find(|user| user.identifier == BOB).unwrap().role = Role::Admin;

        let hostile = crate::util::office_utils::stack_office_parcel(
            &promoted, &bob, "bob promotes himself".to_string(), &bob_key_id, office_head().as_deref(),
            &crate::util::pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        // An admin admission on top: legitimate in itself.
        let (_, top) = fixture.admit(
            CAROL, Role::Writer, crate::util::office_utils::IdentityClass::Human, None
        );
        assert_ne!(top, hostile);

        match verify_office_chain(&fixture.anchor, &top) {
            Err(OfficeChainError::Unauthorized { message: reason, .. }) => {
                assert!(reason.contains(&hostile), "the refusal must name the violating parcel: {}", reason);
                assert!(reason.contains("changes user records"), "{}", reason);
            }
            other => panic!("a buried self-promotion must be refused: {:?}", other.err()),
        }

        // A head pins the overstep on a lift only when the lift brought it: lifting `top` onto
        // a held `hostile` is not the lifter's doing (and says so), while lifting it onto the
        // clean admission, or as the first office, is.
        let refuse = |held: Option<&str>| classify_office_lift_refusal(
            verify_office_chain(&fixture.anchor, &top).err().expect("refused above"), held
        );
        match refuse(Some(&hostile)) {
            OfficeLiftRefusal::Unprocessable(message) => {
                assert!(message.contains(&hostile), "{}", message);
                assert!(message.contains("already in this warehouse's office"), "{}", message);
            }
            other => panic!("a held overstep is not the lift's: {:?}", other),
        }
        assert!(matches!(refuse(Some(&bob_admitted)), OfficeLiftRefusal::Overstep(_)));
        assert!(matches!(refuse(None), OfficeLiftRefusal::Overstep(_)));
    }

    /// FORK-220: the walk names the first parcel that fails and nothing after it. Here Bob's
    /// overstep revokes Carol's key, and Carol then signs honestly with that key: judged against
    /// the overstepped state her parcel would look forged, so the refusal must name Bob's.
    #[test]
    fn an_overstep_is_named_rather_than_the_honest_parcel_it_makes_look_forged() {
        const CAROL: &str = "ca401000-0000-4000-8000-0000000ca401";

        let fixture = PrivilegeFixture::new("overstep-then-honest");
        let (bob_key_id, _) = fixture.admit(
            BOB, Role::Writer, crate::util::office_utils::IdentityClass::Human, None
        );
        let (carol_key_id, _) = fixture.admit(
            CAROL, Role::Writer, crate::util::office_utils::IdentityClass::Human, None
        );
        let bob = crate::model::operator::Operator { identifier: BOB.to_string() };
        let carol = crate::model::operator::Operator { identifier: CAROL.to_string() };

        let before = crate::util::office_utils::read_office_state().unwrap();
        let mut revoked = before.clone();
        let carol_key = revoked.keys.iter_mut().find(|key| key.key_id == carol_key_id).unwrap();
        carol_key.retired_at = Some(1_700_000_003);
        carol_key.revocation_reason = Some(crate::util::office_utils::RevocationReason::Compromise);

        let overstep = crate::util::office_utils::stack_office_parcel(
            &revoked, &bob, "bob revokes carol".to_string(), &bob_key_id, office_head().as_deref(),
            &crate::util::pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        // Carol's own key is really active: only the refused parcel says otherwise.
        let honest = crate::util::office_utils::build_and_sign_office_parcel(
            &revoked, &carol, "carol's parcel".to_string(), &carol_key_id, vec![overstep.clone()]
        ).unwrap();

        match verify_office_chain(&fixture.anchor, &honest) {
            Err(OfficeChainError::Unauthorized { parcel, message }) => {
                assert_eq!(parcel, overstep);
                assert!(message.contains(&format!("retires key {}", carol_key_id)), "{}", message);
            }
            other => panic!("the refusal must name Bob's overstep: {:?}", other.err()),
        }

        // An inauthentic chain is never the lifter's overstep.
        let forged = OfficeChainError::Inauthentic("forged".to_string());
        assert!(matches!(classify_office_lift_refusal(forged, None), OfficeLiftRefusal::Unprocessable(_)));
    }

    /// FORK-220: endorsing another user's key is a privilege, so a key a writer endorsed for
    /// someone else is refused as unauthorized even when an admin signs the parcel that adds it.
    #[test]
    fn a_writer_endorsing_another_users_key_is_refused_as_unauthorized() {
        const CAROL: &str = "ca401000-0000-4000-8000-0000000ca401";

        let fixture = PrivilegeFixture::new("writer-endorses-carol");
        let (bob_key_id, _) = fixture.admit(
            BOB, Role::Writer, crate::util::office_utils::IdentityClass::Human, None
        );
        let (_, carol_admitted) = fixture.admit(
            CAROL, Role::Writer, crate::util::office_utils::IdentityClass::Human, None
        );
        assert!(verify_office_chain(&fixture.anchor, &carol_admitted).is_ok());

        let (key_id, public_key) = sign_utils::generate_keypair(CAROL).unwrap();
        let pop = crate::util::office_utils::sign_key_pop(&key_id, &public_key, CAROL).unwrap();
        let mut state = crate::util::office_utils::read_office_state().unwrap();
        state.keys.push(crate::util::office_utils::endorse_key(
            &public_key, CAROL, &bob_key_id, &pop, 1_700_000_002
        ).unwrap());

        let hostile = crate::util::office_utils::stack_office_parcel(
            &state, &fixture.admin, "add a key for carol".to_string(), &fixture.admin_key_id, office_head().as_deref(),
            &crate::util::pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        match verify_office_chain(&fixture.anchor, &hostile) {
            Err(OfficeChainError::Unauthorized { message: reason, .. }) => {
                assert!(reason.contains(&hostile), "{}", reason);
                assert!(reason.contains("who is not an admin here"), "{}", reason);
            }
            other => panic!("a writer's endorsement of another user's key must be refused as \
                unauthorized: {:?}", other.err()),
        }
    }
}
