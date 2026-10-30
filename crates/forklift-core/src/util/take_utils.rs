//! Verify what a sync takes from a remote, against the trust anchor this warehouse holds.
//!
//! A remote serves history; what makes it this warehouse's is the ref write that adopts it. Every
//! site that adopts a remote head as a user pallet's — `lower`, `franchise`, `lift`'s auto-merge,
//! `mirror` — and every site that adopts one by hash — `pallet create` at a hash, `haul merge` — calls
//! [`verify_take`] first, before its first durable record. The verdict is the offline `audit`'s
//! own ([`audit_utils::verify_pallet_history`]); what this adds is where it is judged from:
//!
//! * a held anchor ([`TakeTrust::Held`]): the anchor and office on disk. Nothing a remote serves
//!   gets past them, and the ref write is conditioned on both, so an office or anchor change
//!   between the verdict and the write fails the write;
//! * first contact ([`TakeTrust::FirstContact`]): the anchor the remote serves and its office,
//!   verified before anything is installed, so a refusal leaves no anchor behind;
//! * no anchor anywhere ([`TakeTrust::Untrusted`]): nothing to verify against; the take goes
//!   through as it always has, conditioned on no anchor having appeared meanwhile.
//!
//! The known-verified set that keeps a held take incremental is every local user-pallet head.
//! It is sound only while every writer of a user-pallet ref either signs the head it writes,
//! copies an existing local head, verifies the head here, or refuses while an anchor is held; the
//! writer-invariant test in `crates/forklift/tests/take_writers.rs` keeps that list. Heads that
//! were already here when an anchor arrived from a remote were written under none of those, so
//! the install judges them first ([`record_unvouched_heads`]) and the set leaves out the ones the
//! anchor does not vouch for, and everything built on them.
//!
//! A partial client — a sparse one, or one that never fetched some pallet — can lack the boundary
//! ancestry a verdict needs. The verdict then names the boundary it could not resolve
//! ([`BoundaryGap`]); the take fetches that boundary under the warehouse fetch scope, as a lower
//! fetches a pallet, and judges again. A gap that persists refuses.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use crate::error::{CoreError, RefusalCode};
use crate::globals;
use crate::util::audit_utils::{self, BoundaryGap, VerifiedOffice};
use crate::util::lock_utils::RefLock;
use crate::util::{file_utils, merge_utils, object_utils, record_utils};
use crate::util::office_utils::{OfficeState, TrustAnchor};
use crate::util::pallet_utils::{self, AnchorPrecondition, OfficePrecondition, PalletsPrecondition, RefPreconditions};
use crate::util::progress_utils::Progress;
use crate::util::remote_utils::{self, RemoteClient};
use crate::util::scope_utils::MaterializationScope;

/// What a take is verified against.
pub enum TakeTrust {
    /// No anchor is held here and none is being installed: nothing to verify against.
    Untrusted,

    /// The anchor this warehouse holds, and its office verified against it.
    Held {
        /// The held anchor.
        anchor: TrustAnchor,
        /// The local office, verified against `anchor`, with the preconditions of a write judged
        /// against it.
        office: VerifiedOffice,
    },

    /// First contact: the anchor the remote serves (already checked against any `--expect` pin)
    /// and the office it will install, verified against that anchor. Nothing is installed yet.
    FirstContact {
        /// The anchor the remote serves.
        anchor: TrustAnchor,
        /// The office head that will be installed.
        office_head: String,
        /// The office state at `office_head`, verified against `anchor`.
        office_state: OfficeState,
    },
}

impl TakeTrust {
    /// What this warehouse's own trust state verifies a take against: [`TakeTrust::Held`] when
    /// an anchor is held (its office verified against it), [`TakeTrust::Untrusted`] otherwise.
    pub fn local() -> Result<TakeTrust, String> {
        Ok(match audit_utils::verified_local_trust()? {
            Some((anchor, office)) => TakeTrust::Held { anchor, office },
            None => TakeTrust::Untrusted,
        })
    }

    /// The preconditions of the ref write that adopts a head judged against this trust: the
    /// local state the verdict depended on.
    fn preconditions(&self) -> RefPreconditions {
        match self {
            TakeTrust::Held { office, .. } => office.preconditions(),
            // Judged with nothing held: the write stands only while nothing is, so an anchor that
            // appeared meanwhile — and with it a boundary this take was never checked against —
            // fails it.
            TakeTrust::Untrusted | TakeTrust::FirstContact { .. } => RefPreconditions {
                office: OfficePrecondition::NotConsumed,
                anchor: AnchorPrecondition::Absent,
                pallets: PalletsPrecondition::NotConsumed,
            },
        }
    }
}

/// The preconditions of a ref write adopting a head verified at `first_contact`, once the trust it
/// was verified against is installed: the installed anchor and office, which must be the ones it
/// was verified against.
///
/// # Returns
/// * `Ok(RefPreconditions)` - The write's preconditions.
/// * `Err(String)`          - The installed trust is not what the take was verified against (it
///                            changed meanwhile), or it could not be read.
pub fn installed(first_contact: &TakeTrust) -> Result<RefPreconditions, String> {
    let TakeTrust::FirstContact { anchor, office_head, .. } = first_contact else {
        return Ok(first_contact.preconditions());
    };

    match TakeTrust::local()? {
        TakeTrust::Held { anchor: held, office } if held == *anchor
            && office.preconditions().office == OfficePrecondition::At(Some(office_head.clone())) => Ok(office.preconditions()),
        _ => Err(
            "The trust anchor or office of this warehouse changed while the remote's history was \
            verified against them, so nothing was taken. Run the command again.".to_string()
        ),
    }
}

/// Where a take fetches what a verdict lacks: the warehouse fetch scope on disk, or one the caller
/// names because it has not written it yet (a sparse `franchise`).
#[derive(Clone, Copy)]
pub enum FetchAt<'a> {
    /// The fetch scope on disk.
    Warehouse,

    /// This scope.
    Scope(&'a MaterializationScope),
}

/// Verify `head`, served by `client` for `pallet`, against `trust` before it is adopted.
///
/// A verdict this store cannot reach because it lacks boundary ancestry fetches that boundary
/// from `client` — under `at`, the way the take fetched `head` — and is judged again; each kind
/// of boundary (the anchor's, one revoked key's) is fetched at most once.
///
/// # Arguments
/// * `client` - The remote the head came from, which is also asked for missing boundary
///              ancestry; `None` for a head already local (`pallet create` at a hash, `haul merge`),
///              whose gap is refused as it stands.
/// * `trust`  - What the head is verified against.
/// * `head`   - The head about to be adopted; its closure is present.
/// * `pallet` - The pallet it is adopted as (named in the refusal, and in a sparse warehouse's
///              fetched-heads ledger for a boundary fetch).
/// * `at`     - The fetch scope of a boundary fetch.
///
/// # Returns
/// * `Ok(RefPreconditions)` - The head verifies; the adopting ref write must carry these, so it
///                            lands only while the anchor and office it was judged against stand.
/// * `Err(String)`          - A `history_unverified` refusal naming the parcel and why, or a
///                            failure reading what the verdict needs.
pub async fn verify_take(client: Option<&RemoteClient>,
                         trust: &TakeTrust,
                         head: &str,
                         pallet: &str,
                         at: FetchAt<'_>) -> Result<RefPreconditions, String> {
    let (anchor, office_head, office_state) = match trust {
        TakeTrust::Untrusted => return Ok(trust.preconditions()),
        TakeTrust::Held { anchor, office } => (anchor, office.head(), &office.state),
        TakeTrust::FirstContact { anchor, office_head, office_state } => (anchor, Some(office_head.as_str()), office_state),
    };

    // A held take is incremental: every local user-pallet head the anchor vouches for was signed
    // here or passed this check, so its ancestry needs no second walk. A first-contact take holds
    // nothing verified yet — the local heads predate any anchor — so it walks the whole history,
    // as `audit` does.
    let known = match trust {
        TakeTrust::Held { anchor, .. } => vouched_local_heads(anchor)?,
        _ => Vec::new(),
    };
    let known: Vec<&str> = known.iter().map(String::as_str).collect();

    match judge(client, anchor, office_head, office_state, &known, head, pallet, at).await? {
        Judgement::Vouched => Ok(trust.preconditions()),
        Judgement::Refused { why, fetch_error } => {
            Err(history_unverified(pallet, head, trust, client.is_some(), &why, fetch_error.as_deref()).into())
        }
    }
}

/// What [`judge`] found.
enum Judgement {
    /// The anchor vouches for the head.
    Vouched,

    /// It does not: why, and — when the refusal is a boundary the remote was asked for — why
    /// fetching it failed.
    Refused {
        why: String,
        fetch_error: Option<String>,
    },
}

/// Heads that verified in this process, keyed by the warehouse, the anchor, the office head and
/// the head: a verdict depends on nothing else, so a command that judges one head twice (a lift's
/// auto-merge, then the stack that records it) walks it once.
static VOUCHED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// Judge `head` against `anchor` and `office_state` (verified at `office_head`), with `known`
/// heads' ancestry taken as verified, fetching a missing boundary from `client` as
/// [`verify_take`] describes.
///
/// # Returns
/// * `Ok(Judgement)` - The verdict.
/// * `Err(String)`   - The walk could not read what it needed: no verdict was reached.
#[allow(clippy::too_many_arguments)]
async fn judge(client: Option<&RemoteClient>,
               anchor: &TrustAnchor,
               office_head: Option<&str>,
               office_state: &OfficeState,
               known: &[&str],
               head: &str,
               pallet: &str,
               at: FetchAt<'_>) -> Result<Judgement, String> {
    let key = office_head.map(|office_head| format!("{}\u{1}{}", audit_utils::office_chain_key(anchor, office_head), head));
    let vouched = VOUCHED.get_or_init(|| Mutex::new(HashSet::new()));

    if key.as_ref().is_some_and(|key| vouched.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).contains(key)) {
        return Ok(Judgement::Vouched);
    }

    let mut fetched: HashSet<BoundaryGapKind> = HashSet::new();
    let mut fetch_error: Option<String> = None;

    loop {
        let refusal = match audit_utils::verify_pallet_history(head, anchor, office_state, known) {
            Ok(_) => {
                if let Some(key) = key {
                    vouched.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(key);
                }

                return Ok(Judgement::Vouched);
            }
            Err(refusal) if refusal.read_failure => return Err(refusal.message),
            Err(refusal) => refusal,
        };

        let refetch = match (&refusal.gap, client) {
            (Some(gap), Some(client)) if fetched.insert(BoundaryGapKind::of(gap)) => Some((gap, client)),
            _ => None,
        };

        let Some((gap, client)) = refetch else {
            // A fetch failure explains only a gap that is still open; a verdict reached past it
            // is about the history, not the network.
            let fetch_error = match refusal.gap {
                Some(_) => fetch_error,
                None => None,
            };

            return Ok(Judgement::Refused { why: refusal.message, fetch_error });
        };

        let mut heads: Vec<String> = match gap {
            BoundaryGap::Trust { .. } => anchor.boundary.clone(),
            BoundaryGap::Distrust { key_id, .. } => office_state.find_key(key_id)
                .map(|key| key.distrust_boundary.clone())
                .unwrap_or_default(),
        };
        heads.push(match gap {
            BoundaryGap::Trust { missing } | BoundaryGap::Distrust { missing, .. } => missing.clone(),
        });

        // Each head on its own: the remote may no longer hold one, and that one failing must not
        // keep the others from arriving. A head it cannot serve stays missing, and the next
        // verdict says so.
        for boundary_head in &heads {
            let result = match at {
                FetchAt::Warehouse => remote_utils::fetch_history_scoped(client, boundary_head, pallet, &Progress::NONE).await,
                FetchAt::Scope(scope) => remote_utils::fetch_history_at(client, boundary_head, pallet, scope, &Progress::NONE).await,
            };

            if let Err(error) = result {
                fetch_error.get_or_insert(format!("Fetching boundary parcel {} from the remote failed: {}", boundary_head, error));
            }
        }
    }
}

/// Which boundary a gap is in, so each is fetched once.
#[derive(Hash, PartialEq, Eq)]
enum BoundaryGapKind {
    Trust,
    Distrust(String),
}

impl BoundaryGapKind {
    fn of(gap: &BoundaryGap) -> BoundaryGapKind {
        match gap {
            BoundaryGap::Trust { .. } => BoundaryGapKind::Trust,
            BoundaryGap::Distrust { key_id, .. } => BoundaryGapKind::Distrust(key_id.clone()),
        }
    }
}

/// Every local user pallet with a head, as (name, head).
fn local_user_heads() -> Result<Vec<(String, String)>, String> {
    let mut heads = Vec::new();

    for name in pallet_utils::list_pallets()? {
        if let Some(head) = pallet_utils::get_pallet_head(&name)? {
            heads.push((name, head));
        }
    }

    Ok(heads)
}

/// Every local user-pallet head whose ancestry a take under `anchor` may skip: all of them except
/// a head an anchor installed from a remote was recorded not to vouch for, and any head built on
/// one — a parcel signed on top of history the anchor does not vouch for is no better than it.
fn vouched_local_heads(anchor: &TrustAnchor) -> Result<Vec<String>, String> {
    // A parcel no longer in the store is in no local head's history, which is always present.
    let mut unvouched = read_unvouched(&anchor.genesis)?;
    unvouched.retain(|head| file_utils::does_object_exist(head).unwrap_or(true));
    let mut heads = Vec::new();

    for (_, head) in local_user_heads()? {
        let mut vouched = true;

        for unvouched in &unvouched {
            if *unvouched == head || merge_utils::is_ancestor(unvouched, &head)? {
                vouched = false;
                break;
            }
        }

        if vouched {
            heads.push(head);
        }
    }

    Ok(heads)
}

/// Judge every local user-pallet head against an anchor about to be installed from a remote, and
/// record the ones it does not vouch for, before the install.
///
/// A take judged against a held anchor skips the history of every local user-pallet head. A head
/// this warehouse held before the remote's anchor arrived was never checked against it: work
/// stacked here before the remote enrolled, unsigned and outside the anchor's boundary, is one.
/// Such a head stays where it is — `audit` reports it, and a lift of it is refused — but a take
/// must walk it, so it is recorded under the anchor's genesis and left out of every later take's
/// known set ([`vouched_local_heads`]). A head the remote can serve the boundary of is fetched and
/// judged on what it serves, as a take is.
///
/// # Arguments
/// * `client`       - The remote the anchor comes from.
/// * `anchor`       - The anchor about to be installed.
/// * `office_head`  - The office head it will be installed with.
/// * `office_state` - The office state at `office_head`, verified against `anchor`.
/// * `verified`     - Heads already verified against both (the pallet the command takes).
///
/// # Returns
/// * `Ok(LocalJudgement)` - Every head judged, and the pallets the anchor does not vouch for.
/// * `Err(String)`        - The record could not be read or written.
pub async fn record_unvouched_heads(client: &RemoteClient,
                                    anchor: &TrustAnchor,
                                    office_head: &str,
                                    office_state: &OfficeState,
                                    verified: &[&str]) -> Result<LocalJudgement, String> {
    let mut known: Vec<String> = verified.iter().map(|head| head.to_string()).collect();
    let mut judgement = LocalJudgement { judged: Vec::new(), unvouched: Vec::new() };
    let mut unvouched = Vec::new();

    for (pallet, head) in local_user_heads()? {
        let bound: Vec<&str> = known.iter().map(String::as_str).collect();

        // A head whose history cannot be read is not one the anchor was seen to vouch for: left
        // out of later known sets like a refused one, rather than keeping the anchor out.
        match judge(Some(client), anchor, Some(office_head), office_state, &bound, &head, &pallet, FetchAt::Warehouse).await {
            Ok(Judgement::Vouched) => known.push(head.clone()),
            Ok(Judgement::Refused { .. }) | Err(_) => {
                unvouched.push(head.clone());
                judgement.unvouched.push(pallet);
            }
        }

        judgement.judged.push(head);
    }

    if !unvouched.is_empty() {
        let lock = RefLock::acquire()?;
        let mut entries = read_unvouched_entries()?;

        // An entry whose parcel left the store can bound nothing any more.
        entries.retain(|(_, head)| file_utils::does_object_exist(head).unwrap_or(true));

        for head in unvouched {
            let entry = (anchor.genesis.clone(), head);
            if !entries.contains(&entry) {
                entries.push(entry);
            }
        }

        write_unvouched_entries(&lock, &entries)?;
    }

    Ok(judgement)
}

/// What [`record_unvouched_heads`] found.
pub struct LocalJudgement {
    /// Every head judged: the install must be conditioned on the user pallets still holding only
    /// these, or a head moved meanwhile goes unjudged.
    pub judged: Vec<String>,

    /// The pallets whose heads the anchor does not vouch for, for the command to name.
    pub unvouched: Vec<String>,
}

/// The record of heads an installed anchor does not vouch for, in the shared forklift root: a
/// [`record_utils`] file of one `<genesis> <head>` entry per line. A warehouse that never installed
/// an anchor over such a head has no file.
const FILE_NAME_UNVOUCHED: &str = "unvouched-heads";
const UNVOUCHED_RECORD: &str = "unvouched-heads";
const UNVOUCHED_VERSION: u32 = 1;

fn unvouched_path() -> PathBuf {
    globals::forklift_root().join(FILE_NAME_UNVOUCHED)
}

fn read_unvouched_entries() -> Result<Vec<(String, String)>, String> {
    let path = unvouched_path();
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("Error while reading \"{}\": {}", path.to_string_lossy(), e)),
    };

    record_utils::from_bytes(&bytes, UNVOUCHED_RECORD, UNVOUCHED_VERSION)
        .map_err(|e| format!("The file \"{}\" {}", path.to_string_lossy(), e.describe()))?
        .into_iter()
        .map(|line| match line.split_once(' ') {
            Some((genesis, head)) if object_utils::is_object_hash(genesis) && object_utils::is_object_hash(head) => {
                Ok((genesis.to_string(), head.to_string()))
            }
            _ => Err(format!(
                "The file \"{}\" holds an entry that is not a genesis and a parcel hash: \"{}\".",
                path.to_string_lossy(), line.escape_debug()
            )),
        })
        .collect()
}

fn write_unvouched_entries(_lock: &RefLock, entries: &[(String, String)]) -> Result<(), String> {
    let lines = entries.iter().map(|(genesis, head)| format!("{} {}", genesis, head));

    file_utils::write_file_atomically(&unvouched_path(), &record_utils::to_bytes(UNVOUCHED_RECORD, UNVOUCHED_VERSION, lines))
}

/// The heads recorded as not vouched for by the anchor of `genesis`.
fn read_unvouched(genesis: &str) -> Result<Vec<String>, String> {
    Ok(read_unvouched_entries()?.into_iter()
        .filter(|(recorded, _)| recorded == genesis)
        .map(|(_, head)| head)
        .collect())
}

/// The refusal of a take whose history did not verify.
fn history_unverified(pallet: &str,
                      head: &str,
                      trust: &TakeTrust,
                      served: bool,
                      why: &str,
                      fetch_error: Option<&str>) -> CoreError {
    let against = match trust {
        TakeTrust::FirstContact { .. } => "the trust anchor the remote serves",
        _ => "the trust anchor this warehouse holds",
    };

    let mut message = match served {
        true => format!(
            "The remote's history of pallet \"{}\" (head {}) does not verify against {}, so that \
            anchor does not vouch for what the remote serves: {} Nothing was taken.",
            pallet, head, against, why
        ),
        false => format!(
            "The history of parcel {} does not verify against {}, so that anchor does not vouch \
            for it: {} Pallet \"{}\" was not moved to it.",
            head, against, why, pallet
        ),
    };

    if let Some(fetch_error) = fetch_error {
        message = format!("{} {}", message, fetch_error);
    }

    CoreError::refusal(
        RefusalCode::HistoryUnverified,
        message,
        "Run \"forklift audit\" where this history came from, and take it only once that passes.",
    )
}
