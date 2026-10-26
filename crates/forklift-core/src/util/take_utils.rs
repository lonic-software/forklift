//! Verify what a sync takes from a remote, against the trust anchor this warehouse holds.
//!
//! A remote serves history; what makes it this warehouse's is the ref write that adopts it. Every
//! site that adopts a remote head as a user pallet's — `lower`, `franchise`, `lift`'s auto-merge,
//! `mirror` — and every site that adopts one by hash — `palletize` at a hash, `haul merge` — calls
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
//! writer-invariant test in `crates/forklift/tests/take_writers.rs` keeps that list.
//!
//! A partial client — a sparse one, or one that never fetched some pallet — can lack the boundary
//! ancestry a verdict needs. The verdict then names the boundary it could not resolve
//! ([`BoundaryGap`]); the take fetches that boundary under the warehouse fetch scope, as a lower
//! fetches a pallet, and judges again. A gap that persists refuses.

use std::collections::HashSet;
use crate::error::{CoreError, RefusalCode};
use crate::util::audit_utils::{self, BoundaryGap, VerifiedOffice};
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

    fn judged(&self) -> Option<(&TrustAnchor, &OfficeState)> {
        match self {
            TakeTrust::Untrusted => None,
            TakeTrust::Held { anchor, office } => Some((anchor, &office.state)),
            TakeTrust::FirstContact { anchor, office_state, .. } => Some((anchor, office_state)),
        }
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
///              ancestry; `None` for a head already local (`palletize` at a hash, `haul merge`),
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
    let Some((anchor, office_state)) = trust.judged() else {
        return Ok(trust.preconditions());
    };

    // A held take is incremental: every local user-pallet head was signed here or passed this
    // check, so its ancestry needs no second walk. A first-contact take holds nothing verified
    // yet — the local heads predate any anchor — so it walks the whole history, as `audit` does.
    let known = match trust {
        TakeTrust::Held { .. } => local_user_heads()?,
        _ => Vec::new(),
    };
    let known: Vec<&str> = known.iter().map(String::as_str).collect();

    let mut fetched: HashSet<BoundaryGapKind> = HashSet::new();
    let mut fetch_error: Option<String> = None;

    loop {
        let refusal = match audit_utils::verify_pallet_history(head, anchor, office_state, &known) {
            Ok(_) => return Ok(trust.preconditions()),
            Err(refusal) => refusal,
        };

        let refetch = match (&refusal.gap, client) {
            (Some(gap), Some(client)) if fetched.insert(BoundaryGapKind::of(gap)) => Some((gap, client)),
            _ => None,
        };

        let Some((gap, client)) = refetch else {
            return Err(history_unverified(pallet, head, trust, client.is_some(), &refusal.message, fetch_error.as_deref()).into());
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

/// Every local user-pallet head.
fn local_user_heads() -> Result<Vec<String>, String> {
    let mut heads = Vec::new();

    for name in pallet_utils::list_pallets()? {
        if let Some(head) = pallet_utils::get_pallet_head(&name)? {
            heads.push(head);
        }
    }

    Ok(heads)
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
