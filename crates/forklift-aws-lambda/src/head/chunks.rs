//! Deleting the chunks of an applied redaction's recipe on the AWS head (FORK-411).
//!
//! A recipe's chunks may be shared with any other file, in any version of any pallet's history, so
//! a chunk is deleted only once a census of everything the warehouse still names finds no other
//! recipe naming it. The AWS head has no lock over its writers, so the census runs in fenced
//! rounds instead, and a round deletes only what its census found unnamed while the fence held.
//!
//! # The ledger
//!
//! One item (`chunk-ledger`) journals every applied recipe whose deletion is not done, written in
//! the same transaction as the redaction item that learns its content is a recipe. Each entry
//! runs through three states:
//!
//! 1. `owed`: its chunks have not been deleted yet. Until its first round settles, every chunk of
//!    it is withheld from serving.
//! 2. `late`: a round deleted its chunks. One more round, due once every presigned upload of the
//!    content has expired, deletes any chunk such an upload landed after the first.
//! 3. `recipe_deleting`: its chunks are done; the recipe object itself goes, with no fence and no
//!    census. The redaction item then records `recipe_deleted`, and the entry leaves the ledger.
//!
//! A recipe whose delete S3 refused keeps its state, and its next round backs off from an hour,
//! doubling to a day.
//!
//! # The fence
//!
//! One item (`chunk-fence`) holds the round: a version that only increases, the phase, when the
//! round started, and the round's recipes, read from the fence alone for the rest of the round.
//! No item reads as `down` at version 0.
//!
//! 1. **Announced.** A request that finds the fence down and a ledger entry due raises it. While
//!    a round runs, no promotion writes a chunk of one of its recipes. A promotion that read the
//!    fence down ends within one invocation, so the census waits that out first: nothing it
//!    judges can still be landing.
//! 2. **Census.** Every ref update commits on the fence's exact bytes, so one that read it before
//!    the census began fails its commit, and none starts while the census or the delete runs.
//!    The census walks every pallet's whole history, tombstones and meta pallets included, and the
//!    head every unmerged haul proposes, and records which of the round's chunks some recipe that
//!    is not applied names. It resumes across invocations from a work object.
//! 3. **Deleting.** The finished census is frozen with a write that never replaces an existing
//!    object, and the deleter deletes, under a lease on the fence, every round chunk it found
//!    unnamed. It settles each recipe, then lowers the fence.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use forklift_core::util::haul_utils::{self, HAUL_PALLET_NAME};
use forklift_core::util::pallet_utils::PalletNamespace;
use forklift_core::util::{file_utils, object_utils};

use super::redaction::{epoch_seconds, late_sweep_due, Budget, INVOCATION_CEILING, LATE_SWEEP_MARGIN, SWEEP_LEASE_MARGIN};
use super::Head;
use crate::error::{HeadError, HeadResult};
use crate::scratch::{materialize_state, Mirror};
use crate::store::{ItemKey, ItemWrite, ObjectStore, RefStore};

/// The version of the fence item this build writes and reads.
const FENCE_VERSION: u64 = 1;

/// The version of the ledger item this build writes and reads.
const LEDGER_VERSION: u64 = 1;

/// The version of a round's census work objects this build writes and reads.
const CENSUS_VERSION: u64 = 1;

/// How long an announced round waits before its census starts, in seconds: past every promotion
/// that read the fence down before the round started.
pub(super) const ROUND_WAIT: u64 = INVOCATION_CEILING + LATE_SWEEP_MARGIN;

/// How long after a refused delete its recipe is next due, in seconds, doubled per refusal.
const REFUSAL_BACKOFF: u64 = 3_600;

/// The longest a refused recipe waits for its next round, in seconds.
const REFUSAL_BACKOFF_CAP: u64 = 86_400;

/// How many of one request's fence or ledger writes may lose to another before it leaves the round
/// to a later request.
const ROUND_ATTEMPTS: usize = 8;

/// The `Retry-After` of a ref update refused while a round's census or delete runs, in seconds.
pub const ROUND_RETRY_AFTER_SECONDS: u32 = 30;

/// The phase of a round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Down,
    Announced,
    Census,
    Deleting,
}

/// The fence item's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fence {
    /// [`FENCE_VERSION`].
    version: u64,

    /// The latest round's number; 0 before the first.
    round: u64,

    phase: Phase,

    /// When the round was announced, in seconds since the Unix epoch.
    started: u64,

    /// The round's recipes; empty while down.
    recipes: Vec<String>,

    /// Until when one request holds the round's delete, in seconds since the Unix epoch.
    lease_until: Option<u64>,
}

impl Fence {
    /// The fence no item stands for.
    fn none() -> Fence {
        Fence { version: FENCE_VERSION, round: 0, phase: Phase::Down, started: 0, recipes: Vec::new(), lease_until: None }
    }

    /// The same round's fence, lowered.
    fn lowered(&self) -> Fence {
        Fence { phase: Phase::Down, started: 0, recipes: Vec::new(), lease_until: None, ..self.clone() }
    }
}

/// Where one recipe's deletion stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Owed,
    Late,
    RecipeDeleting,
}

/// One recipe's ledger entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    state: State,

    /// When the redaction applied, in seconds since the Unix epoch.
    applied_at: u64,

    /// When its next round, or its recipe's deletion, is due, in seconds since the Unix epoch.
    next_round_after: u64,

    /// The last round that settled it; `None` before the first.
    settled_round: Option<u64>,

    /// How many deletes S3 has refused it in a row.
    refusals: u32,
}

/// The ledger item's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    /// [`LEDGER_VERSION`].
    version: u64,

    recipes: BTreeMap<String, Journal>,
}

/// One object a census has still to read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Node {
    Parcel(String),
    Tree(String),
    Recipe(String),
}

impl Node {
    fn hash(&self) -> &str {
        match self {
            Node::Parcel(hash) | Node::Tree(hash) | Node::Recipe(hash) => hash,
        }
    }
}

/// A census part way, saved between invocations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Census {
    version: u64,
    frontier: Vec<Node>,
    visited: BTreeSet<String>,
    named: BTreeSet<String>,
}

/// A finished census, frozen: the round chunks nothing else names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frozen {
    version: u64,
    unnamed: BTreeSet<String>,
}

/// How a content route treats a chunk an applied recipe names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Withheld {
    /// Not served yet: whether something else names it is not decided (`503`).
    Unavailable,

    /// Served as absent: the round's frozen census found nothing else naming it (`404`).
    Absent,
}

/// How far one call of [`Head::run_census`] got.
enum CensusRun {
    Frozen,
    Stopped,
    Failed(String),
}

/// The chunk lists of recipes read so far, by recipe hash. A recipe is content-addressed, so an
/// entry is never stale.
static RECIPE_CHUNKS: OnceLock<Mutex<HashMap<String, Arc<Vec<String>>>>> = OnceLock::new();

/// The work object a round's census is saved in.
fn census_name(round: u64) -> String {
    format!("chunks/census-{}", round)
}

/// The work object a round's finished census is frozen in.
fn frozen_name(round: u64) -> String {
    format!("chunks/census-{}-final", round)
}

/// `stored`, the item `what`, as a `T` of `version`.
///
/// # Returns
/// * `Err(String)` - It is of a version this build does not read, or does not parse: never read as
///                   no round, or nothing owed.
fn decode<T: for<'de> Deserialize<'de>>(what: &str, stored: &str, version: u64) -> Result<T, String> {
    let found = serde_json::from_str::<serde_json::Value>(stored)
        .ok()
        .and_then(|value| value.get("version").and_then(serde_json::Value::as_u64));

    if found != Some(version) {
        return Err(format!(
            "The {} item is of a version this build does not read ({}), so the deletion of redacted \
            chunks cannot go on. Update the deployment to a build that reads it.",
            what,
            found.map_or("none".to_string(), |found| found.to_string())
        ));
    }

    serde_json::from_str(stored).map_err(|error| format!("The {} item cannot be read: {}", what, error))
}

fn encode<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| format!("encoding a chunk deletion item failed: {}", error))
}

/// The fence item's bytes, `None` when there is none, and what they say.
fn read_fence<R: RefStore + ?Sized>(refs: &R) -> Result<(Option<String>, Fence), String> {
    match refs.get_item(ItemKey::ChunkFence)? {
        Some(stored) => {
            let fence = decode("chunk-fence", &stored, FENCE_VERSION)?;
            Ok((Some(stored), fence))
        }
        None => Ok((None, Fence::none())),
    }
}

/// The ledger item's bytes, `None` when there is none, and what they say.
fn read_ledger<R: RefStore + ?Sized>(refs: &R) -> Result<(Option<String>, Ledger), String> {
    match refs.get_item(ItemKey::ChunkLedger)? {
        Some(stored) => {
            let ledger = decode("chunk-ledger", &stored, LEDGER_VERSION)?;
            Ok((Some(stored), ledger))
        }
        None => Ok((None, Ledger { version: LEDGER_VERSION, recipes: BTreeMap::new() })),
    }
}

/// The ledger item's state holding one entry of each kind, and a fence item's in the middle of a
/// round, exactly as this build writes them: what the AWS format fixture freezes.
pub fn fixture_items(recipes: [&str; 3]) -> Result<(String, String), String> {
    let entry = |state, settled_round| Journal { state, applied_at: 1_780_000_001, next_round_after: 1_780_000_961, settled_round, refusals: 1 };
    let ledger = Ledger {
        version: LEDGER_VERSION,
        recipes: BTreeMap::from([
            (recipes[0].to_string(), entry(State::Owed, None)),
            (recipes[1].to_string(), entry(State::Late, Some(1))),
            (recipes[2].to_string(), entry(State::RecipeDeleting, Some(2))),
        ]),
    };
    let fence = Fence {
        version: FENCE_VERSION,
        round: 3,
        phase: Phase::Deleting,
        started: 1_780_000_100,
        recipes: vec![recipes[0].to_string()],
        lease_until: Some(1_780_000_200),
    };

    Ok((encode(&ledger)?, encode(&fence)?))
}

/// Whether the ledger and fence items, if any, read back.
pub fn items_read<R: RefStore + ?Sized>(refs: &R) -> Result<(), String> {
    read_ledger(refs)?;
    read_fence(refs)?;
    Ok(())
}

/// The chunk list of the recipe `hash`; `None` when the store holds no such object.
fn recipe_chunks<O: ObjectStore + ?Sized>(objects: &O, hash: &str) -> Result<Option<Arc<Vec<String>>>, String> {
    let cache = RECIPE_CHUNKS.get_or_init(|| Mutex::new(HashMap::new()));

    if let Some(chunks) = cache.lock().map_err(|error| error.to_string())?.get(hash) {
        return Ok(Some(Arc::clone(chunks)));
    }

    let Some(bytes) = objects.get(hash)? else {
        return Ok(None);
    };

    let chunks: Arc<Vec<String>> =
        Arc::new(object_utils::parse_recipe_bytes(hash, &bytes)?.chunks.into_iter().map(|chunk| chunk.hash).collect());
    cache.lock().map_err(|error| error.to_string())?.insert(hash.to_string(), Arc::clone(&chunks));

    Ok(Some(chunks))
}

/// The union of the chunk lists of `recipes`, and the recipes whose object the store lacks.
fn round_chunks<O: ObjectStore + ?Sized>(objects: &O, recipes: &[String]) -> Result<(HashMap<String, Arc<Vec<String>>>, Vec<String>), String> {
    let mut lists = HashMap::new();
    let mut lacking = Vec::new();

    for recipe in recipes {
        match recipe_chunks(objects, recipe)? {
            Some(chunks) => {
                lists.insert(recipe.clone(), chunks);
            }
            None => lacking.push(recipe.clone()),
        }
    }

    Ok((lists, lacking))
}

/// The ledger entry and its write that journal the recipe `hash`, applied at `applied_at`, as owed
/// and due at once: what the write recording that its redaction applied, or that its content is a
/// recipe, carries in the same transaction. A recipe already journaled is left as it is.
///
/// # Returns
/// * `Ok((expected, state))` - The ledger's bytes as read, and as they are to be written.
pub(super) fn journal<R: RefStore + ?Sized>(refs: &R, hash: &str, applied_at: u64) -> Result<(Option<String>, String), String> {
    let (stored, mut ledger) = read_ledger(refs)?;

    ledger.recipes.entry(hash.to_string()).or_insert(Journal {
        state: State::Owed,
        applied_at,
        next_round_after: applied_at,
        settled_round: None,
        refusals: 0,
    });

    Ok((stored, encode(&ledger)?))
}

/// Whether the applied recipe `hash` may be deleted: its chunks are done, and the ledger has moved
/// it to `recipe_deleting` or taken it off. A promotion that writes it again then has it deleted,
/// as a blob's is.
pub(super) fn recipe_may_go<R: RefStore + ?Sized>(refs: &R, hash: &str) -> Result<bool, String> {
    let (_, ledger) = read_ledger(refs)?;

    Ok(ledger.recipes.get(hash).is_none_or(|journal| journal.state == State::RecipeDeleting))
}

/// Whether a promotion of `hash` must be refused now: a round is running, and `hash` is a chunk of
/// one of its recipes. What the staging verifier and a lift commit read before they promote.
///
/// # Returns
/// * `Err(String)` - The fence, or a round recipe, could not be read.
pub fn refuses_promotion<O: ObjectStore + ?Sized, R: RefStore + ?Sized>(objects: &O, refs: &R, hashes: &[String]) -> Result<Option<String>, String> {
    let (_, fence) = read_fence(refs)?;

    if fence.phase == Phase::Down {
        return Ok(None);
    }

    let (lists, _) = round_chunks(objects, &fence.recipes)?;
    let chunks: HashSet<&String> = lists.values().flat_map(|chunks| chunks.iter()).collect();

    Ok(hashes.iter().find(|hash| chunks.contains(hash)).cloned())
}

/// Which of `hashes` are chunks an applied recipe names that must not be served as usual:
///
/// - every chunk of a recipe no round has settled yet, and of a recipe in the running round, is
///   unavailable;
/// - once the round's census is frozen, a chunk of one of its recipes is absent when the census
///   found nothing else naming it, and served when it found something.
///
/// A recipe that a round has settled is served as usual between rounds: a later head may name its
/// chunks legitimately. Unavailable wins over absent.
pub fn withheld<O: ObjectStore + ?Sized, R: RefStore + ?Sized>(objects: &O, refs: &R, hashes: &[String]) -> Result<HashMap<String, Withheld>, String> {
    let (_, ledger) = read_ledger(refs)?;
    let mut answer = HashMap::new();

    if !ledger.recipes.values().any(|journal| journal.state != State::RecipeDeleting) {
        return Ok(answer);
    }

    let (_, fence) = read_fence(refs)?;
    let round: &[String] = if fence.phase == Phase::Down { &[] } else { &fence.recipes };
    let unnamed = match fence.phase {
        Phase::Deleting => Some(read_frozen(objects, fence.round)?),
        _ => None,
    };

    let unsettled = ledger.recipes.iter()
        .filter(|(_, journal)| journal.state == State::Owed && journal.settled_round.is_none())
        .map(|(recipe, _)| recipe);
    let withheld: BTreeSet<&String> = unsettled.chain(round.iter()).collect();
    let wanted: HashSet<&str> = hashes.iter().map(String::as_str).collect();

    for recipe in withheld {
        let Some(chunks) = recipe_chunks(objects, recipe)? else {
            continue;
        };
        let frozen = unnamed.as_ref().filter(|_| round.contains(recipe));

        for chunk in chunks.iter().filter(|chunk| wanted.contains(chunk.as_str())) {
            let verdict = match frozen {
                Some(unnamed) if unnamed.contains(chunk) => Withheld::Absent,
                Some(_) => continue,
                None => Withheld::Unavailable,
            };

            let entry = answer.entry(chunk.clone()).or_insert(verdict);
            if verdict == Withheld::Unavailable {
                *entry = Withheld::Unavailable;
            }
        }
    }

    Ok(answer)
}

/// The frozen census of `round`.
///
/// # Returns
/// * `Err(String)` - It is missing, of another version, or does not parse: a fence that says
///                   deleting is only ever written after it.
fn read_frozen<O: ObjectStore + ?Sized>(objects: &O, round: u64) -> Result<BTreeSet<String>, String> {
    let bytes = objects.work_get(&frozen_name(round))?
        .ok_or_else(|| format!("The frozen census of chunk round {} is missing.", round))?;
    let stored = String::from_utf8(bytes).map_err(|_| format!("The frozen census of chunk round {} is not text.", round))?;

    Ok(decode::<Frozen>("frozen census", &stored, CENSUS_VERSION)?.unnamed)
}

/// How long a recipe refused `refusals` times in a row waits for its next round, in seconds.
fn backoff(refusals: u32) -> u64 {
    REFUSAL_BACKOFF.saturating_mul(1 << refusals.saturating_sub(1).min(16)).min(REFUSAL_BACKOFF_CAP)
}

/// The `503` a ref update answers while a round's census or delete runs.
fn round_running() -> HeadError {
    HeadError::retry_after(
        "The chunks of a redacted file are being deleted in this warehouse, and no pallet head moves \
        until that is done. Try again shortly.",
        ROUND_RETRY_AFTER_SECONDS,
    )
}

/// The `503` a promotion of `hash` answers while a round runs over its recipe.
pub(super) fn promotion_refused(hash: &str) -> HeadError {
    HeadError::retry_after(
        format!(
            "Object {} is a chunk of a redacted file whose chunks are being deleted in this \
            warehouse, so it is not stored until that is done. Try again shortly.",
            hash
        ),
        ROUND_RETRY_AFTER_SECONDS,
    )
}

impl<O: ObjectStore, R: RefStore> Head<O, R> {
    /// Refuse a read of `hash` while it is a chunk an applied recipe names that is withheld
    /// ([`withheld`]): `503` while undecided, after advancing the round, and `404` once absent.
    pub(super) fn refuse_withheld_chunk(&self, hash: &str) -> HeadResult<()> {
        let withheld = withheld(&self.objects, &self.refs, &[hash.to_string()]).map_err(HeadError::internal)?;

        match withheld.get(hash) {
            None => Ok(()),
            Some(Withheld::Absent) => Err(HeadError::not_found(format!("No object {} exists.", hash))),
            Some(Withheld::Unavailable) => {
                self.advance_round_logged();

                Err(HeadError::retry_after(
                    format!(
                        "Object {} is a chunk of a redacted file, and whether anything else in this \
                        warehouse names it is not decided yet, so it is not served. Try again later.",
                        hash
                    ),
                    ROUND_RETRY_AFTER_SECONDS,
                ))
            }
        }
    }

    /// The same head, with an announced round's census waiting `seconds` instead of
    /// [`ROUND_WAIT`], so a test can run a whole round in one request.
    pub fn with_chunk_round_wait(self, seconds: u64) -> Head<O, R> {
        Head { chunk_round_wait: seconds, ..self }
    }

    /// The fence bytes a ref update's commit is conditioned on, read before its closure audit;
    /// `None` when there is no fence item. While a round's census or delete runs, the update spends
    /// its invocation advancing the round, and answers `503` unless that lowered the fence.
    pub(super) fn chunk_fence_for_update(&self) -> HeadResult<Option<String>> {
        let (stored, fence) = read_fence(&self.refs).map_err(HeadError::internal)?;

        if !matches!(fence.phase, Phase::Census | Phase::Deleting) {
            return Ok(stored);
        }

        self.advance_round_logged();

        let (stored, fence) = read_fence(&self.refs).map_err(HeadError::internal)?;
        match fence.phase {
            Phase::Census | Phase::Deleting => Err(round_running()),
            Phase::Down | Phase::Announced => Ok(stored),
        }
    }

    /// The `503` a ref update answers when the fence moved after its audit.
    pub(super) fn chunk_fence_moved(&self) -> HeadError {
        round_running()
    }

    /// [`advance_round`](Self::advance_round), as best effort: a failure is logged, never answered.
    pub(super) fn advance_round_logged(&self) {
        if let Err(error) = self.advance_round() {
            eprintln!("forklift-aws-lambda: advancing the redacted-chunk round failed: {}", error.message);
        }
    }

    /// Take the chunk deletion as far as this request's budget goes: finish every recipe object
    /// that is due, start a round when a recipe is due one, and run the running round's phase.
    pub(super) fn advance_round(&self) -> HeadResult<()> {
        let mut budget = self.scan_budget();

        for _ in 0..ROUND_ATTEMPTS {
            if !budget.timely() {
                return Ok(());
            }

            let (stored, fence) = read_fence(&self.refs).map_err(HeadError::internal)?;
            let now = epoch_seconds(SystemTime::now());

            match fence.phase {
                Phase::Down => {
                    let (ledger_stored, ledger) = read_ledger(&self.refs).map_err(HeadError::internal)?;

                    if self.finish_recipes(ledger_stored.as_deref(), &ledger, now, &mut budget)? {
                        continue;
                    }

                    let due: Vec<String> = ledger.recipes.iter()
                        .filter(|(_, journal)| journal.state != State::RecipeDeleting && journal.next_round_after <= now)
                        .map(|(recipe, _)| recipe.clone())
                        .collect();

                    if due.is_empty() {
                        return Ok(());
                    }

                    let next = Fence { round: fence.round + 1, phase: Phase::Announced, started: now, recipes: due, ..Fence::none() };
                    self.write_fence(&next, stored.as_deref(), &mut budget)?;
                }
                Phase::Announced => {
                    if now < fence.started.saturating_add(self.chunk_round_wait) {
                        return Ok(());
                    }

                    let next = Fence { phase: Phase::Census, ..fence };
                    self.write_fence(&next, stored.as_deref(), &mut budget)?;
                }
                Phase::Census => match self.run_census(&fence, &mut budget)? {
                    CensusRun::Frozen => {
                        let next = Fence { phase: Phase::Deleting, ..fence };
                        self.write_fence(&next, stored.as_deref(), &mut budget)?;
                    }
                    CensusRun::Stopped => return Ok(()),
                    CensusRun::Failed(why) => {
                        eprintln!(
                            "forklift-aws-lambda: chunk round {} stopped: its census could not be taken: {}",
                            fence.round, why
                        );

                        if self.back_off(&fence.recipes, now, &mut budget)? {
                            self.write_fence(&fence.lowered(), stored.as_deref(), &mut budget)?;
                        }
                    }
                },
                Phase::Deleting => {
                    if fence.lease_until.is_some_and(|until| until > now) {
                        return Ok(());
                    }

                    let until = budget.deadline.map_or(now + INVOCATION_CEILING, epoch_seconds) + SWEEP_LEASE_MARGIN;
                    let leased = Fence { lease_until: Some(until), ..fence };

                    if !self.write_fence(&leased, stored.as_deref(), &mut budget)? {
                        continue;
                    }
                    let leased_stored = encode(&leased).map_err(HeadError::internal)?;

                    // Out of time part way, the lease runs out and the next request after it
                    // deletes again: every delete is idempotent, and a settled recipe is not
                    // settled twice.
                    if !self.delete_round(&leased, now, &mut budget)? {
                        return Ok(());
                    }

                    if self.write_fence(&leased.lowered(), Some(&leased_stored), &mut budget)? {
                        for name in [census_name(leased.round), frozen_name(leased.round)] {
                            if let Err(error) = self.objects.work_delete(&name) {
                                eprintln!("forklift-aws-lambda: a chunk census work object was not deleted: {}", error);
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Delete every recipe object the ledger has due in `recipe_deleting`, record each deleted on
    /// its redaction item, and take it off the ledger. A refused delete backs off.
    ///
    /// # Returns
    /// * `Ok(bool)` - Whether it wrote anything: read the ledger again.
    fn finish_recipes(&self, stored: Option<&str>, ledger: &Ledger, now: u64, budget: &mut Budget) -> HeadResult<bool> {
        let Some((recipe, journal)) = ledger.recipes.iter()
            .find(|(_, journal)| journal.state == State::RecipeDeleting && journal.next_round_after <= now)
        else {
            return Ok(false);
        };

        let mut next = ledger.clone();

        let deleted = self.objects.delete_staged(recipe).and_then(|()| self.objects.delete_canonical(recipe));
        match deleted {
            Ok(()) => {
                // The redaction item first: an entry off the ledger with no record of the deletion
                // would leave the redaction `deletion_owed` with nothing left to do it.
                if !self.mark_recipe_deleted(recipe, budget)? {
                    return Ok(true);
                }

                next.recipes.remove(recipe);
            }
            Err(error) => {
                eprintln!("forklift-aws-lambda: redacted recipe {} was not deleted: {}", recipe, error);

                let refusals = journal.refusals.saturating_add(1);
                next.recipes.insert(recipe.clone(), Journal { refusals, next_round_after: now + backoff(refusals), ..journal.clone() });
            }
        }

        self.write_ledger(&next, stored, budget)?;
        Ok(true)
    }

    /// Back off each of `recipes` as a refused delete does, after a round that could not finish.
    ///
    /// # Returns
    /// * `Ok(bool)` - Whether the ledger was written.
    fn back_off(&self, recipes: &[String], now: u64, budget: &mut Budget) -> HeadResult<bool> {
        let (stored, mut ledger) = read_ledger(&self.refs).map_err(HeadError::internal)?;

        for recipe in recipes {
            if let Some(journal) = ledger.recipes.get_mut(recipe) {
                journal.refusals = journal.refusals.saturating_add(1);
                journal.next_round_after = now + backoff(journal.refusals);
            }
        }

        self.write_ledger(&ledger, stored.as_deref(), budget)
    }

    /// Take the census of the round `fence` holds as far as `budget` goes, saving it on the way
    /// out, and freeze it once finished.
    fn run_census(&self, fence: &Fence, budget: &mut Budget) -> HeadResult<CensusRun> {
        if self.objects.work_get(&frozen_name(fence.round)).map_err(HeadError::internal)?.is_some() {
            return Ok(CensusRun::Frozen);
        }

        let (lists, _) = round_chunks(&self.objects, &fence.recipes).map_err(HeadError::internal)?;
        let wanted: BTreeSet<String> = lists.values().flat_map(|chunks| chunks.iter().cloned()).collect();
        let applied = self.applied_set().map_err(HeadError::internal)?;

        let saved = self.objects.work_get(&census_name(fence.round)).map_err(HeadError::internal)?
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|stored| decode::<Census>("chunk census", &stored, CENSUS_VERSION).ok());

        let mut census = match saved {
            Some(census) => census,
            None => match self.census_roots() {
                Ok(frontier) => Census { version: CENSUS_VERSION, frontier, visited: BTreeSet::new(), named: BTreeSet::new() },
                Err(why) => return Ok(CensusRun::Failed(why)),
            },
        };

        let scratch = self.scratch().map_err(HeadError::internal)?;
        let walked = scratch.scoped(|| -> Result<bool, String> {
            while census.named.len() < wanted.len() {
                let Some(node) = census.frontier.pop() else {
                    break;
                };

                if census.visited.contains(node.hash()) {
                    continue;
                }

                if !budget.spend() {
                    census.frontier.push(node);
                    return Ok(false);
                }

                census.visited.insert(node.hash().to_string());

                if !self.mirror(node.hash())? {
                    // Absent, as core's live set treats it: what it alone names is named by
                    // nothing that can be read.
                    continue;
                }

                match node {
                    Node::Parcel(hash) => {
                        let parcel = object_utils::load_parcel(&hash)?;
                        census.frontier.push(Node::Tree(parcel.tree_hash));
                        census.frontier.extend(parcel.parents.into_iter().map(Node::Parcel));
                    }
                    Node::Tree(hash) => {
                        let tree = object_utils::load_tree_shared(&hash)?;
                        census.frontier.extend(tree.get_subtrees().map(|(_, subtree)| Node::Tree(subtree.hash.clone())));
                        census.frontier.extend(tree.get_files()
                            .filter(|(_, file)| file.item_type.is_chunked() && !applied.contains(&file.hash))
                            .map(|(_, file)| Node::Recipe(file.hash.clone())));
                    }
                    Node::Recipe(hash) => {
                        census.named.extend(object_utils::recipe_chunk_hashes(&hash)?.into_iter().filter(|chunk| wanted.contains(chunk)));
                    }
                }
            }

            Ok(true)
        });

        let done = match walked {
            Ok(done) => done,
            Err(why) => return Ok(CensusRun::Failed(why)),
        };

        if !done {
            let bytes = encode(&census).map_err(HeadError::internal)?;
            self.objects.work_put(&census_name(fence.round), bytes.as_bytes()).map_err(HeadError::internal)?;

            return Ok(CensusRun::Stopped);
        }

        let unnamed = wanted.difference(&census.named).cloned().collect();
        let frozen = encode(&Frozen { version: CENSUS_VERSION, unnamed }).map_err(HeadError::internal)?;
        // One already there was frozen by another request from the same census: the deleter reads
        // only that one.
        self.objects.work_create(&frozen_name(fence.round), frozen.as_bytes()).map_err(HeadError::internal)?;

        Ok(CensusRun::Frozen)
    }

    /// Where a census starts: every pallet's head, live or a tombstone, meta pallets' included,
    /// and the head every haul not yet merged proposes, which no ref may hold.
    fn census_roots(&self) -> Result<Vec<Node>, String> {
        let listing = self.refs.list_refs()?;
        let mut roots: Vec<Node> = listing.live.iter().chain(listing.tombstones.iter())
            .map(|(_, head)| Node::Parcel(head.clone()))
            .collect();

        if let Some(head) = self.refs.get_head(PalletNamespace::Meta, HAUL_PALLET_NAME)?.live() {
            let scratch = self.scratch()?;
            let proposed = scratch.scoped(|| {
                materialize_state(&self.objects, head, &mut Mirror::default())?;
                haul_utils::unmerged_haul_heads_at(head)
            })?;

            roots.extend(proposed.into_iter().map(Node::Parcel));
        }

        Ok(roots)
    }

    /// Mirror `hash` into the current scratch unless it is there already.
    ///
    /// # Returns
    /// * `Ok(bool)` - Whether the store holds it.
    fn mirror(&self, hash: &str) -> Result<bool, String> {
        if file_utils::does_object_exist(hash)? {
            return Ok(true);
        }

        match self.objects.get(hash)? {
            Some(bytes) => object_utils::store_object_bytes(hash, &bytes).map(|_| true),
            None => Ok(false),
        }
    }

    /// Delete every chunk the frozen census of the round `fence` holds found unnamed, its canonical
    /// object and every staged copy, then settle each of the round's recipes on the ledger.
    ///
    /// # Returns
    /// * `Ok(bool)` - Whether the round is settled; `false` when time ran out first.
    fn delete_round(&self, fence: &Fence, now: u64, budget: &mut Budget) -> HeadResult<bool> {
        let unnamed = read_frozen(&self.objects, fence.round).map_err(HeadError::internal)?;
        let (lists, lacking) = round_chunks(&self.objects, &fence.recipes).map_err(HeadError::internal)?;

        let mut refused: BTreeMap<String, String> = BTreeMap::new();
        for recipe in lacking {
            eprintln!(
                "forklift-aws-lambda: the object of redacted recipe {} in chunk round {} is missing, so its \
                chunks cannot be found",
                recipe, fence.round
            );
            refused.insert(recipe, "its object is missing".to_string());
        }

        for chunk in &unnamed {
            if !budget.timely() {
                return Ok(false);
            }

            if let Err(error) = self.objects.delete_staged(chunk).and_then(|()| self.objects.delete_canonical(chunk)) {
                eprintln!("forklift-aws-lambda: redacted chunk {} was not deleted: {}", chunk, error);

                for (recipe, chunks) in &lists {
                    if chunks.contains(chunk) {
                        refused.entry(recipe.clone()).or_insert(error.clone());
                    }
                }
            }
        }

        for _ in 0..ROUND_ATTEMPTS {
            let (stored, mut ledger) = read_ledger(&self.refs).map_err(HeadError::internal)?;

            for recipe in &fence.recipes {
                let Some(journal) = ledger.recipes.get_mut(recipe) else {
                    continue;
                };

                if journal.settled_round.is_some_and(|settled| settled >= fence.round) {
                    continue;
                }

                journal.settled_round = Some(fence.round);

                if refused.contains_key(recipe) {
                    journal.refusals = journal.refusals.saturating_add(1);
                    journal.next_round_after = now + backoff(journal.refusals);
                    continue;
                }

                journal.refusals = 0;
                match journal.state {
                    State::Owed => {
                        journal.state = State::Late;
                        journal.next_round_after = late_sweep_due(journal.applied_at);
                    }
                    State::Late | State::RecipeDeleting => {
                        journal.state = State::RecipeDeleting;
                        journal.next_round_after = now;
                    }
                }
            }

            if self.write_ledger(&ledger, stored.as_deref(), budget)? {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Write `fence` as the fence item, on condition that it still holds `expected`.
    fn write_fence(&self, fence: &Fence, expected: Option<&str>, budget: &mut Budget) -> HeadResult<bool> {
        let state = encode(fence).map_err(HeadError::internal)?;

        self.write_items(&[ItemWrite { key: ItemKey::ChunkFence, state: &state, expected }], budget)
    }

    /// Write `ledger` as the ledger item, on condition that it still holds `expected`.
    fn write_ledger(&self, ledger: &Ledger, expected: Option<&str>, budget: &mut Budget) -> HeadResult<bool> {
        let state = encode(ledger).map_err(HeadError::internal)?;

        self.write_items(&[ItemWrite { key: ItemKey::ChunkLedger, state: &state, expected }], budget)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_recipe_backs_off_from_an_hour_doubling_to_a_day() {
        assert_eq!(backoff(1), 3_600);
        assert_eq!(backoff(2), 7_200);
        assert_eq!(backoff(5), 57_600);
        assert_eq!(backoff(6), 86_400);
        assert_eq!(backoff(u32::MAX), 86_400);
    }

    #[test]
    fn an_item_of_another_version_is_refused_never_read_as_nothing_owed() {
        for stored in [r#"{"version":2,"recipes":{}}"#, r#"{"recipes":{}}"#, "not json"] {
            assert!(decode::<Ledger>("chunk-ledger", stored, LEDGER_VERSION).is_err(), "{} was read", stored);
        }

        let unknown = r#"{"version":1,"round":0,"phase":"down","started":0,"recipes":[],"lease_until":null,"x":1}"#;
        assert!(decode::<Fence>("chunk-fence", unknown, FENCE_VERSION).is_err());

        let current = r#"{"version":1,"round":3,"phase":"census","started":5,"recipes":["r"],"lease_until":null}"#;
        assert_eq!(decode::<Fence>("chunk-fence", current, FENCE_VERSION).unwrap().phase, Phase::Census);
    }

    #[test]
    fn the_round_waits_out_an_invocation_and_a_margin() {
        assert!(ROUND_WAIT >= 900 + 60);
    }
}
