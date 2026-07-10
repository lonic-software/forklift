//! Offline verification of a warehouse's signed history.
//!
//! Shared by the `audit` command and by remotes: the server heads run the same
//! verification before committing a ref update, so a remote can never be pushed into a
//! state a local audit would reject.

use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::{Mutex, OnceLock};
use crate::util::office_utils::{OfficeState, TrustAnchor};
use crate::util::{fanout_utils, file_utils, graph_utils, object_utils, sign_utils};

/// Verify the office chain from the genesis forward and return the final office state.
///
/// Office history is linear: the chain is walked head → genesis, then verified forward.
/// Every office parcel must be signed by a key that was active in the *previous* office
/// state — introducing a key and signing with it in the same parcel is only valid at
/// the genesis (that self-signature is the trust-on-first-use anchor).
///
/// # Arguments
/// * `anchor`      - The trust anchor.
/// * `office_head` - The head of the office pallet to verify.
///
/// # Returns
/// * `Ok(OfficeState)` - The verified head state.
/// * `Err(String)`     - If the chain does not reach the genesis, or any office parcel
///                       fails verification.
pub fn verify_office_chain(anchor: &TrustAnchor, office_head: &str) -> Result<OfficeState, String> {
    let mut chain: Vec<String> = Vec::new();
    let mut cursor = office_head.to_string();

    loop {
        chain.push(cursor.clone());

        if cursor == anchor.genesis {
            break;
        }

        let parcel = object_utils::load_parcel(&cursor)?;

        match parcel.parents.first() {
            Some(parent) => cursor = parent.clone(),
            None => return Err(format!(
                "The office chain does not reach the genesis parcel {} (it ends at {}). \
                The warehouse may have been tampered with.",
                anchor.genesis, cursor
            )),
        }
    }

    chain.reverse();

    let mut previous_state: Option<OfficeState> = None;

    for hash in &chain {
        let state = crate::util::office_utils::read_office_state_of(hash)?;

        {
            let lookup_state = previous_state.as_ref().unwrap_or(&state);

            verify_one_signature(hash, lookup_state, "office parcel")?;

            // A revoked key must not extend the office chain: the signer has to be
            // active in the state the signature is checked against.
            let signature = sign_utils::load_parcel_signature(hash)?
                .expect("verify_one_signature loaded this signature");

            let signer_active = lookup_state.find_key(&signature.key_id)
                .map(|key| key.is_active())
                .unwrap_or(false);

            if !signer_active {
                return Err(format!(
                    "The office parcel {} is signed with key {}, which is revoked at \
                    that point. The warehouse may have been tampered with.",
                    hash, signature.key_id
                ));
            }
        }

        verify_new_key_endorsements(previous_state.as_ref(), &state).map_err(|reason| {
            format!("The office parcel {} {}", hash, reason)
        })?;

        previous_state = Some(state);
    }

    Ok(previous_state.expect("the chain contains at least the genesis"))
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
/// * `Ok(())`      - If every new key is properly endorsed.
/// * `Err(String)` - The reason a key is not (phrased to follow "The office parcel X").
fn verify_new_key_endorsements(previous: Option<&OfficeState>,
                               current: &OfficeState) -> Result<(), String> {
    use crate::util::office_utils::{key_endorsement_payload, key_pop_payload, Role};

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
            continue; // Not new; immutability is enforced by the privilege check.
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
        } else {
            // A cross-identity authorization: only an admin's key may (the scope of a
            // key-authorization equals the scope of the authorizer's authority).
            let scope = previous.unwrap_or(current);

            let authorizer_is_admin = scope.find_key(authorized_by)
                .and_then(|admin_key| scope.find_user(&admin_key.operator))
                .map(|admin| admin.role == Role::Admin)
                .unwrap_or(false);

            if !authorizer_is_admin {
                return Err(format!(
                    "adds key {} for \"{}\" authorized by key {} of \"{}\", who is not \
                    an admin here.",
                    key.key_id, key.operator, authorized_by, authorizer.operator
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
/// * `known_verified` - A head that already passed this verification (`None` walks
///                      everything — the offline `audit`).
///
/// # Returns
/// * `Ok((usize, usize))` - The number of verified and legacy (pre-trust) parcels.
/// * `Err(String)`        - If any parcel fails verification.
pub fn verify_pallet_history(head: &str,
                             anchor: &TrustAnchor,
                             office_state: &OfficeState,
                             known_verified: Option<&str>) -> Result<(usize, usize), String> {
    // Phase 1 — discover the parcels to verify: everything reachable from `head` and not
    // from `known_verified`, whose ancestry was verified when it was committed. The walk is
    // bounded by the gap between the two heads, including across a merge (see
    // [`new_parcels`]). Parent edges come from the commit-graph, which is content-addressed
    // (a parcel's hash commits to its parents, so a present record's parents are exactly the
    // real ones) and falls back to decoding the parcel when its record is not yet built — so
    // the discovery set is always complete, and on a graph-warm warehouse it is found
    // without decoding a single parcel body. The bodies are proven present in phase 2
    // instead, in parallel.
    let parcels = new_parcels(head, known_verified)?;

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

    // Per revoked key: the parcels its distrust boundary vouches for (lazy — an
    // all-active-keys history never pays for it).
    let mut distrust_boundaries: HashMap<String, Result<HashSet<String>, String>> = HashMap::new();

    let mut verified = 0usize;
    let mut legacy = 0usize;

    for (index, verdict) in verdicts.into_iter().enumerate() {
        let hash = &parcels[index];

        match verdict? {
            Verdict::Verified => verified += 1,

            // No signature, or a signature by a key the office does not know: after a
            // re-genesis (§8.7) the prior chain's keys are gone, and the parcels they
            // signed are *attested* by the new anchor's boundary pin rather than verified
            // — the same standing as unsigned pre-trust history. Outside the boundary,
            // both are what they always were: tampering.
            Verdict::TrustBoundary(reason) => {
                if legacy_parcels.is_none() {
                    legacy_parcels = Some(collect_reachable_present(&anchor.boundary)?);
                }

                if legacy_parcels.as_ref().unwrap().contains(hash) {
                    legacy += 1;
                } else {
                    return Err(match reason {
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
                    });
                }
            }

            // A revoked key's signature is vouched only within the revocation's distrust
            // boundary (§8.11): exact ancestry, like the trust boundary — a forged or
            // shifted clock changes nothing.
            Verdict::DistrustBoundary(key_id) => {
                let key = office_state.find_key(&key_id)
                    .expect("phase 2 verified this parcel against this key");

                let vouched = distrust_boundaries
                    .entry(key_id.clone())
                    .or_insert_with(|| collect_reachable_present(&key.distrust_boundary))
                    .as_ref()
                    .map_err(|e| e.clone())?
                    .contains(hash);

                if !vouched {
                    return Err(format!(
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
                    ));
                }

                verified += 1;
            }
        }
    }

    Ok((verified, legacy))
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

/// Classify one parcel's signature against the office key registry — the body of the
/// parallel phase. Everything it touches is either immutable (`office_state`) or a
/// per-object read through the shared, already-thread-safe object caches, so it is safe to
/// run on many threads at once.
fn classify_signature(hash: &str, office_state: &OfficeState) -> Result<Verdict, String> {
    let verdict = match sign_utils::load_parcel_signature(hash)? {
        None => Verdict::TrustBoundary(TrustBoundaryReason::Unsigned),

        Some(signature) if office_state.find_key(&signature.key_id).is_none() => {
            Verdict::TrustBoundary(TrustBoundaryReason::UnknownKey(signature.key_id))
        }

        Some(signature) => {
            verify_one_signature(hash, office_state, "parcel")?;

            let key = office_state.find_key(&signature.key_id)
                .expect("verify_one_signature found this key");

            if key.is_active() {
                Verdict::Verified
            } else {
                Verdict::DistrustBoundary(signature.key_id)
            }
        }
    };

    // Prove every parcel's body is present and decodable — the guarantee phase 1 used to
    // give by decoding each parcel for its parents (it now reads parents from the graph
    // instead). Kept after the signature check so a bad signature still fails ahead of a
    // missing body, and confined to this parallel phase so it costs no sequential time.
    object_utils::load_parcel(hash)?;

    Ok(verdict)
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
) -> Result<OfficeState, String> {
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

/// The memo key: which warehouse, under which anchor, at which office head.
fn office_chain_key(anchor: &TrustAnchor, office_head: &str) -> String {
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
/// never the caller's doing. Both server heads map a failure of the memoized verification to
/// `422 Unprocessable`, so returning an error here would tell a client its lift was invalid
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
    let fresh: Option<HashSet<String>> = match known_verified {
        None => None,
        Some(bound) if bound == head => return Ok(Vec::new()),
        Some(bound) => Some(fresh_frontier(head, bound)?),
    };

    // Breadth-first from `head`, so the order — and therefore the first failure an audit
    // reports — is exactly what the unbounded walk produced.
    let mut order: Vec<String> = Vec::new();
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

        for parent in graph_utils::parents(&hash)? {
            queue.push_back(parent);
        }

        order.push(hash);
    }

    Ok(order)
}

/// The set behind [`new_parcels`]: parcels reachable from `head` but not from `bound`.
///
/// A max-heap on the generation number drives the walk, so parcels are settled newest-first
/// and each one's marks are final when it is popped (every parcel that could mark it has a
/// strictly greater generation, hence was popped earlier). The walk stops as soon as nothing
/// fresh is left pending: whatever remains is reachable from `bound`, and so is everything
/// behind it.
fn fresh_frontier(head: &str, bound: &str) -> Result<HashSet<String>, String> {
    let mut walk = Frontier::default();

    walk.mark(head, FRESH)?;
    walk.mark(bound, KNOWN)?;

    // Nothing fresh left pending means nothing new can be discovered: every parcel still on
    // the heap is reachable from `bound`, and so is all of its ancestry.
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

        // Reachable from `bound`: none of it is new, and neither is anything behind it.
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
            let node = graph_utils::node(hash)?;

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

/// Collect every *locally present* parcel reachable from the given heads (the heads
/// included). A head that does not exist here contributes nothing: a trust boundary
/// may name heads this warehouse never had (enrollment includes the remote's heads),
/// and any locally present pre-trust parcel is reachable from a local head anyway.
///
/// # Arguments
/// * `heads` - The starting parcel hashes.
///
/// # Returns
/// * `Ok(HashSet<String>)` - The reachable, locally present parcel hashes.
/// * `Err(String)`         - If a present parcel could not be read.
pub fn collect_reachable_present(heads: &[String]) -> Result<HashSet<String>, String> {
    let mut queue: VecDeque<String> = heads.iter().cloned().collect();
    let mut reachable: HashSet<String> = HashSet::new();

    while let Some(hash) = queue.pop_front() {
        if !file_utils::does_object_exist(&hash)? {
            continue;
        }

        if !reachable.insert(hash.clone()) {
            continue;
        }

        for parent in object_utils::load_parcel(&hash)?.parents {
            queue.push_back(parent);
        }
    }

    Ok(reachable)
}

/// Verify that every new office parcel stays within its signer's privileges.
///
/// `verify_office_chain` proves the chain is *authentic* (signed by then-active keys);
/// this proves it is *authorized*: an admin may change anything, everyone else only
/// their own keys (self-service rotation/retirement). The signer's role is taken from
/// the state *before* the parcel — a parcel cannot grant its own author privileges.
/// The genesis needs no check (it is the trust-on-first-use anchor).
///
/// # Arguments
/// * `anchor`   - The trust anchor.
/// * `old_head` - The already-committed office head (`None` checks back to genesis).
/// * `new_head` - The office head being committed.
///
/// # Returns
/// * `Ok(())`      - If every new parcel is within its signer's privileges.
/// * `Err(String)` - If a parcel exceeds them (or the chain is unreadable).
pub fn verify_office_privileges(anchor: &TrustAnchor,
                                old_head: Option<&str>,
                                new_head: &str) -> Result<(), String> {
    // The office chain is linear: walk new_head down to the committed head (or the
    // genesis), newest first.
    let mut chain: Vec<String> = Vec::new();
    let mut cursor = new_head.to_string();

    loop {
        if Some(cursor.as_str()) == old_head || cursor == anchor.genesis {
            break;
        }

        chain.push(cursor.clone());

        match object_utils::load_parcel(&cursor)?.parents.first() {
            Some(parent) => cursor = parent.clone(),
            None => break,
        }
    }

    for hash in chain.iter().rev() {
        let parent = object_utils::load_parcel(hash)?
            .parents
            .first()
            .cloned()
            .ok_or(format!("The office parcel {} has no parent.", hash))?;

        let previous = crate::util::office_utils::read_office_state_of(&parent)?;
        let current = crate::util::office_utils::read_office_state_of(hash)?;

        let signature = sign_utils::load_parcel_signature(hash)?
            .ok_or(format!("The office parcel {} carries no signature.", hash))?;

        let signer = previous.find_key(&signature.key_id)
            .map(|key| key.operator.clone())
            .ok_or(format!(
                "The office parcel {} is signed with key {}, which is not tracked at \
                that point.",
                hash, signature.key_id
            ))?;

        // Chain invariants that bind admins too (like the last-admin rule): keys are
        // retained forever, their records are immutable, and a revocation is
        // append-once — nobody quietly un-revokes a key or rewrites its boundary.
        verify_key_permanence(&previous, &current).map_err(|reason| format!(
            "The office parcel {} {}.", hash, reason
        ))?;

        let is_admin = previous.find_user(&signer)
            .map(|user| user.role == crate::util::office_utils::Role::Admin)
            .unwrap_or(false);

        if is_admin {
            continue;
        }

        verify_self_service_change(&previous, &current, &signer).map_err(|reason| format!(
            "The office parcel {} (signed by \"{}\", not an admin) {}.",
            hash, signer, reason
        ))?;
    }

    Ok(())
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
    type UserFacts = (String, i64, String, Vec<String>, String);

    let user_facts = |state: &OfficeState| -> Vec<UserFacts> {
        state.users.iter()
            .map(|user| (
                user.identifier.clone(),
                user.enrolled_at,
                user.role.as_str().to_string(),
                user.pallets.clone(),
                user.identity_root.clone(),
            ))
            .collect()
    };

    if user_facts(previous) != user_facts(current) {
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
pub fn verify_parcel_closure(head: &str, known_complete: Option<&str>) -> Result<(), String> {
    // The default check reads blob presence straight from the local object store; the
    // serverless head passes an S3 HEAD instead (see `verify_parcel_closure_with`).
    verify_parcel_closure_with(head, known_complete, &|hash| file_utils::does_object_exist(hash))
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
/// # Arguments
/// * `head`           - The head parcel whose closure is verified.
/// * `known_complete` - A head whose history is already known complete (`None` verifies
///                      all the way down).
/// * `blob_exists`    - Returns whether a file-content blob is present at a hash.
pub fn verify_parcel_closure_with(
    head: &str,
    known_complete: Option<&str>,
    blob_exists: &dyn Fn(&str) -> Result<bool, String>,
) -> Result<(), String> {
    // Only the new segment: the closure behind `known_complete` was proven complete when
    // that head was committed. This walk used to build its prune set with
    // `collect_reachable(known_complete)`, which decoded every parcel body in the ancestry —
    // O(history) on every ref update, however little the lift added.
    let parcels = new_parcels(head, known_complete)
        .map_err(|e| format!("The history behind {} is incomplete: {}", head, e))?;

    let mut visited_trees: HashSet<String> = HashSet::new();

    for hash in &parcels {
        let parcel = object_utils::load_parcel(hash)
            .map_err(|e| format!("The history behind {} is incomplete: {}", head, e))?;

        verify_tree_closure(&parcel.tree_hash, &mut visited_trees, blob_exists)?;
    }

    Ok(())
}

/// Verify that a tree and everything below it is present in the object store.
///
/// # Arguments
/// * `tree_hash`     - The root of the subtree to verify.
/// * `visited_trees` - Trees already verified (shared across parcels of one walk, so
///                     unchanged subtrees are not walked twice).
///
/// # Returns
/// * `Ok(())`      - If the whole subtree is present.
/// * `Err(String)` - If a tree or blob is missing (or unreadable).
fn verify_tree_closure(tree_hash: &str,
                       visited_trees: &mut HashSet<String>,
                       blob_exists: &dyn Fn(&str) -> Result<bool, String>) -> Result<(), String> {
    if !visited_trees.insert(tree_hash.to_string()) {
        return Ok(());
    }

    let tree = object_utils::load_tree(tree_hash)
        .map_err(|e| format!("Tree {} is missing or unreadable: {}", tree_hash, e))?;

    for (_, file) in tree.get_files() {
        if !blob_exists(&file.hash)? {
            return Err(format!(
                "Blob {} (\"{}\" in tree {}) is missing.",
                file.hash, file.name, tree_hash
            ));
        }
    }

    for (_, subtree) in tree.get_subtrees() {
        verify_tree_closure(&subtree.hash, visited_trees, blob_exists)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use crate::util::office_utils::{
        key_endorsement_payload, key_pop_payload, KeyRecord, Role, UserRecord,
    };
    use crate::util::sign_utils::to_hex;

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

        let error = verify_new_key_endorsements(None, &state).unwrap_err();
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
        let error = verify_new_key_endorsements(Some(&previous), &current).unwrap_err();
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

        let error = verify_new_key_endorsements(Some(&previous), &current).unwrap_err();
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

        let error = verify_new_key_endorsements(Some(&current), &next).unwrap_err();
        assert!(error.contains("not an admin"), "{}", error);

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

        let error = verify_new_key_endorsements(Some(&previous), &current).unwrap_err();
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

        let error = verify_new_key_endorsements(Some(&previous), &current).unwrap_err();
        assert!(error.contains("revoked at that point"), "{}", error);
    }

    #[test]
    fn a_pinned_identity_root_must_exist_and_belong_to_the_user() {
        let (state, _, _, _) = genesis_state();

        let broken = OfficeState {
            users: vec![user(ALICE, Role::Admin, "missing-key-id")],
            keys: vec![state.keys[0].clone()],
        };

        let error = verify_new_key_endorsements(None, &broken).unwrap_err();
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
}
