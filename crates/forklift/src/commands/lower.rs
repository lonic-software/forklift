use std::collections::BTreeMap;
use serde::Serialize;
use forklift_core::error::{CoreError, RefusalCode};
use forklift_core::model::operator::Operator;
use forklift_core::util::meta_tree::{self, CellRead, Entry, KeepSide, MetaConflict, MetaSigner, PalletRules, ReplayError};
use forklift_core::util::progress_utils::Progress;
use forklift_core::util::remote_utils::RemoteClient;
use forklift_core::util::stocktake_utils::ChangeKind;
use forklift_core::util::{
    audit_utils, bay_utils, config_utils, file_utils, haul_utils, kv_utils, merge_utils, object_utils,
    office_utils, pallet_utils, remote_utils, shift_utils, stocktake_utils, trust_utils,
};
use crate::commands::office;
use crate::events;
use crate::output::{self, CommandOutput};

/// Handle the lower command (git's "pull"): fetch the remote's new parcels for the
/// current pallet and fast-forward to them — working directory and inventory included.
///
/// The remote's trust state is synchronized first: on first contact with a trusted
/// remote its anchor is adopted (a one-way door, exactly like "office enroll"), and the
/// office pallet fast-forwards so newly admitted operators can be verified locally.
///
/// The meta pallets sync next, each on its own: a meta pallet that diverged has its
/// local net change replayed onto the remote head and lifted, unless both sides changed the
/// same path differently — a conflict, which `keep` resolves path by path. A pallet only this
/// side moved is left for lift, unless `keep` names it: then it replays the same way, so
/// "theirs" can drop a local change the remote refuses to admit. A pallet that
/// conflicts or fails stops only itself; the working pallet still lowers, and the command
/// then fails with the most severe outcome.
///
/// A diverged working pallet is never merged implicitly: lower reports it and the operator
/// consolidates deliberately.
///
/// Naming a pallet other than the current one obtains or updates that pallet's ref alone (see
/// [`lower_named_pallet`]); this bay, its files and its inventory stay where they are.
///
/// # Arguments
/// * `pallet` - The pallet to lower; `None` (or the current pallet) lowers the current pallet.
/// * `expect` - The genesis the remote's trust anchor must have (an out-of-band pin), checked
///              against the handshake before anything is fetched.
/// * `keep`   - Conflict resolutions, each `<pallet>:<path>=ours|theirs`.
///
/// # Returns
/// * `Ok(())`      - If every pallet is now in sync with the remote (or ahead of it).
/// * `Err(String)` - If no remote is configured, its trust anchor is not the expected one or
///                   not the one held, the remote has no such pallet, the warehouse is dirty, the
///                   histories diverged, a meta pallet conflicted or failed to sync, or a transfer
///                   failed.
pub async fn handle_command(pallet: Option<String>, expect: Option<String>, keep: Vec<String>) -> Result<(), String> {
    let keeps = parse_keeps(&keep)?;

    if let Some(name) = &pallet {
        pallet_utils::validate_pallet_name(name)?;
    }

    let current = pallet_utils::read_position()?.pallet;
    let named = pallet.filter(|name| *name != current);

    let progress = events::progress();
    let client = RemoteClient::from_config()?;
    let info = client.fetch_info().await?;

    // A named pallet the remote lacks is refused before trust or a meta pallet is written: it is
    // where a missing pallet's refusal sends an operator, so it must fail cleanly and coded. The
    // remote's trust is judged first, read-only, so a pinned or changed anchor still refuses as
    // such rather than as another remote's pallet list.
    if let Some(name) = &named {
        if !info.pallets.contains_key(name) {
            let held = office_utils::read_trust_anchor()?;
            trust_utils::check_handshake_trust(client.url(), &info, held.as_ref().map(|anchor| anchor.genesis.as_str()),
                expect.as_deref())?;

            return Err(remote_lacks_pallet(&info, name, "forklift palletize --remote").into());
        }
    }

    // Checks the pin (and a stripped anchor) against the handshake before anything is fetched.
    let trust = remote_utils::adopt_remote_trust(&client, &info, expect.as_deref()).await?;

    // Announced at once, not with the report: the adopted anchor is already on disk, and a
    // later refusal (a dirty warehouse, a missing pallet) would otherwise leave it unsaid.
    if let Some(genesis) = &trust.adopted_genesis {
        output::warn_trust_adopted(&trust_utils::first_contact(client.url(), genesis));
    }

    // Meta pallets (the manifest, …) sync alongside trust — fast-forwarded, never
    // materialized; diverged ones are reconciled by their pallet's policy.
    let meta = sync_meta(&client, &info, &keeps).await;

    let mut report = LowerReport {
        adopted_genesis: trust.adopted_genesis,
        office_moved: trust.office_moved,
        meta_adopted: meta.adopted,
        meta_replayed: meta.replayed,
        meta_restamped: meta.restamped,
        meta_conflicts: meta.conflicts,
        meta_failed: meta.failed,
        pallet: named.clone().unwrap_or(current),
        outcome: None,
        head: String::new(),
        fetched_objects: 0,
        fetched_signatures: 0,
    };

    let lowered = match named {
        Some(_) => lower_named_pallet(&client, &info, &mut report, &progress).await,
        None => lower_working_pallet(&client, &info, &mut report, &progress).await,
    };

    // The most severe outcome decides the exit: the working pallet's own failure, then a meta
    // pallet that failed, then a meta conflict. The report shows what did go through.
    if let Err(why) = lowered {
        if !output::is_json() {
            report.render_meta();
        }

        return Err(with_meta_conflicts(with_meta_failures(why, &report.meta_failed), &report.meta_conflicts));
    }

    if !report.meta_failed.is_empty() || !report.meta_conflicts.is_empty() {
        if !output::is_json() {
            report.render_human();
        }

        if !report.meta_failed.is_empty() {
            return Err(meta_failure_error(&report.meta_failed));
        }

        return Err(meta_conflict_refusal(&report.meta_conflicts, &report.pallet, &report.head));
    }

    output::emit(&report);

    Ok(())
}

/// Lower the current pallet: fast-forward it to the remote head, recording the outcome in the
/// report.
async fn lower_working_pallet(client: &RemoteClient,
                              info: &forklift_core::model::remote::WarehouseInfo,
                              report: &mut LowerReport,
                              progress: &Progress) -> Result<(), String> {
    let pallet = report.pallet.clone();
    let remote_head = remote_pallet_head(info, &pallet, "forklift lift")?;

    report.head = remote_head.clone();

    // The fast-forward materializes from the tree this bay holds, and moves the pallet from there.
    // Checked before "up to date" too: a pallet another bay already lowered leaves this one's
    // files behind, which only catching up fixes.
    let position = pallet_utils::read_position()?;
    position.require_current("lowered")?;

    if position.base.as_deref() == Some(remote_head.as_str()) {
        report.outcome = Some(LowerOutcome::UpToDate);
        return Ok(());
    }

    let local_head = position.base.clone();

    // Refuse before fetching anything: a dirty warehouse would be overwritten by the
    // fast-forward, and the operator should resolve that first.
    let local_tree = match &local_head {
        Some(hash) => Some(object_utils::load_parcel(hash)?.tree_hash),
        None => None,
    };

    // Anchored before the cleanliness check, so a file edited after it is never trusted.
    let anchor = file_utils::take_stat_anchor()?;

    ensure_warehouse_is_clean(local_tree.as_deref()).await?;

    // Path-pruned in a sparse warehouse (out-of-scope content stays sealed), whole in a full one
    // — a full fetch scope makes this byte-identical to the unscoped fetch. The office and meta
    // pallets synced above keep routing through the unscoped fetch, since their audit reads full
    // content.
    let stats = remote_utils::fetch_history_scoped(client, remote_head, &pallet, progress).await?;
    report.fetched_objects = stats.fetched_objects;
    report.fetched_signatures = stats.fetched_signatures;

    if let Some(local) = &local_head {
        if merge_utils::is_ancestor(remote_head, local)? {
            report.outcome = Some(LowerOutcome::Ahead);
            return Ok(());
        }

        if !merge_utils::is_ancestor(local, remote_head)? {
            return Err(remote_utils::diverged_refusal(&pallet, remote_head).into());
        }
    }

    let remote_tree = object_utils::load_parcel(remote_head)?.tree_hash;

    shift_utils::materialize_tree(local_tree.as_deref(), &remote_tree, "Lowering", anchor, progress)?;

    // A compare-and-set against the head this lower started from; the working tree goes back to
    // where it was if the pallet did not move (another bay moved it first, or the write failed).
    shift_utils::write_head_or_roll_back(&pallet, local_head.as_deref(), remote_head, &remote_tree, local_tree.as_deref(), anchor)?;

    report.outcome = Some(LowerOutcome::Lowered);

    Ok(())
}

/// The remote's head of user pallet `pallet`, or the refusal that it has none. The refusal lists
/// only the remote's user pallets: a meta pallet is synced by every lower and never named.
fn remote_pallet_head<'a>(info: &'a forklift_core::model::remote::WarehouseInfo,
                          pallet: &str,
                          next_step: &str) -> Result<&'a String, String> {
    info.pallets.get(pallet).ok_or_else(|| remote_lacks_pallet(info, pallet, next_step).into())
}

/// The [`RefusalCode::PalletNotFound`] refusal for a remote that has no user pallet `pallet`,
/// naming the ones it has. `next_step` is the remedy at the site: `lift` publishes the current
/// pallet, `palletize --remote` lists what another name could be.
fn remote_lacks_pallet(info: &forklift_core::model::remote::WarehouseInfo, pallet: &str, next_step: &str) -> CoreError {
    let user: Vec<&str> = info.pallets.keys()
        .map(String::as_str)
        .filter(|name| !name.starts_with(pallet_utils::META_QUALIFIER))
        .collect();

    CoreError::refusal(
        RefusalCode::PalletNotFound,
        format!(
            "The remote has no pallet \"{}\" (it has: {}). \"forklift palletize --remote\" lists \
            them.",
            pallet,
            if user.is_empty() { "none".to_string() } else { user.join(", ") }
        ),
        next_step,
    )
}

/// Lower a pallet other than the current one: fetch the remote head's closure, then move the
/// pallet's ref to it if it is unborn here or behind. Nothing else moves — not this bay's position,
/// its files or its inventory — so neither a dirty warehouse nor a bay behind its own pallet
/// refuses it. A bay standing on the pallet is left behind it, and catches up with `shift`.
///
/// The ref is written only once the fetch completed: every fetch stops at local ref heads,
/// taking each for a closure already present, so a ref moved before its objects arrived would hide
/// them from every later fetch. The write is a compare-and-set against the head read before the
/// fetch, through [`pallet_utils::update_pallet_head`] alone: the bay-moving write lower's
/// current-pallet path uses would re-point this bay at the pallet.
async fn lower_named_pallet(client: &RemoteClient,
                            info: &forklift_core::model::remote::WarehouseInfo,
                            report: &mut LowerReport,
                            progress: &Progress) -> Result<(), String> {
    let pallet = report.pallet.clone();
    let remote_head = remote_pallet_head(info, &pallet, "forklift palletize --remote")?;

    report.head = remote_head.clone();

    let local_head = pallet_utils::get_pallet_head(&pallet)?;

    if local_head.as_deref() == Some(remote_head.as_str()) {
        report.outcome = Some(LowerOutcome::UpToDate);
        return Ok(());
    }

    let stats = remote_utils::fetch_history_scoped(client, remote_head, &pallet, progress).await?;
    report.fetched_objects = stats.fetched_objects;
    report.fetched_signatures = stats.fetched_signatures;

    if let Some(local) = &local_head {
        if merge_utils::is_ancestor(remote_head, local)? {
            report.outcome = Some(LowerOutcome::Ahead);
            return Ok(());
        }

        // Not the working pallet's refusal: its remedy palletizes and shifts this bay, which a
        // named lower promised to leave alone. The reconciliation happens on the pallet itself.
        if !merge_utils::is_ancestor(local, remote_head)? {
            return Err(CoreError::refusal(
                RefusalCode::Diverged,
                format!(
                    "The local pallet \"{p}\" and the remote have diverged; the remote parcels are \
                    fetched. Reconcile it on the pallet: shift onto it (\"forklift shift {p}\"), then \
                    \"forklift lower\" names the steps.",
                    p = pallet
                ),
                format!("forklift shift {}; then forklift lower", pallet),
            ).into());
        }
    }

    let moved = match local_head {
        Some(_) => LowerOutcome::Lowered,
        None => LowerOutcome::Created,
    };

    // A bay standing on the pallet while it is unborn has no base to catch up from: created
    // under it, the pallet would refuse that bay's stack as behind, and its catch-up would read
    // all of its files as changes to overwrite. Judged after the fetch, just before the write, so
    // a bay that moved there while it ran is seen.
    if local_head.is_none() {
        refuse_a_bay_on_unborn(&pallet)?;
    }

    let error = match pallet_utils::update_pallet_head(&pallet, local_head.as_deref(), remote_head, &pallet_utils::RefPreconditions::NONE) {
        Ok(()) => {
            report.outcome = Some(moved);
            return Ok(());
        }
        Err(error) => error,
    };

    match error {
        // Another writer already put the pallet where this lower would have.
        pallet_utils::RefUpdateError::Moved(found) if found.found.as_deref() == Some(remote_head.as_str()) => {
            report.outcome = Some(LowerOutcome::UpToDate);
            Ok(())
        }
        // Not the behind refusal: its remedy, a shift, would carry this bay onto the pallet.
        pallet_utils::RefUpdateError::Moved(found) => Err(format!(
            "{} Run \"forklift lower {}\" again.", found, pallet
        )),
        // A failed write can still have landed: decided by reading the ref back.
        pallet_utils::RefUpdateError::Failed(error) => match pallet_utils::get_pallet_head(&pallet) {
            Ok(head) if head.as_deref() == Some(remote_head.as_str()) => Err(format!(
                "Pallet \"{}\" now holds parcel {}, but its write reported an error: {}",
                pallet, remote_head, error
            )),
            Ok(_) => Err(format!("Pallet \"{}\" was not moved: {}", pallet, error)),
            Err(read) => Err(format!(
                "Writing pallet \"{}\" failed ({}), and reading it back failed too ({}); it may or may \
                not hold parcel {}.",
                pallet, error, read, remote_head
            )),
        },
        refused => Err(refused.to_string()),
    }
}

/// Refuse when a bay stands on `pallet`, which has no ref here. A bay whose position cannot be
/// read might stand on it, so it refuses too, naming that bay.
fn refuse_a_bay_on_unborn(pallet: &str) -> Result<(), String> {
    let bays = std::iter::once((None, forklift_core::globals::forklift_root()))
        .chain(bay_utils::list_bays()?.into_iter().map(|name| {
            let dir = bay_utils::bay_state_dir(&name);
            (Some(name), dir)
        }));

    for (name, dir) in bays {
        let which = match &name {
            Some(name) => format!("Bay \"{}\"", name),
            None => "The warehouse's own working tree".to_string(),
        };

        let position = pallet_utils::read_position_in(&dir).map_err(|error| format!(
            "Cannot tell whether {} stands on pallet \"{}\": {}", which.to_lowercase(), pallet, error
        ))?;

        if position.pallet == pallet {
            return Err(format!(
                "{} stands on pallet \"{}\", which has nothing stacked here; creating it at the \
                remote's head would leave it nothing to catch up from. Run \"forklift lower\" there \
                instead.",
                which, pallet
            ));
        }
    }

    Ok(())
}

/// What a lower did to the pallet it lowered.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LowerOutcome {
    /// The local pallet was already at the remote head.
    UpToDate,

    /// The local pallet is ahead of the remote (lift to publish).
    Ahead,

    /// The local pallet fast-forwarded to the remote head.
    Lowered,

    /// The pallet did not exist here, and now points at the remote head. Only a named lower, of a
    /// pallet other than the current one, creates one.
    Created,
}

/// A meta pallet that failed to sync, and why.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize, Clone)]
pub(crate) struct MetaPalletFailure {
    /// The pallet's wire ref, e.g. `@kv`.
    pub(crate) pallet: String,

    pub(crate) error: String,
}

impl MetaPalletFailure {
    /// A failure carrying a refusal's human message, and its next step when the message does not
    /// already say it: the frame never reaches the `--json` report or the joined error, which
    /// carries no code of its own.
    pub(crate) fn new(pallet: String, error: String) -> MetaPalletFailure {
        let error = match forklift_core::error::CoreError::from(error) {
            forklift_core::error::CoreError::Refusal { message, next_step, .. }
                if !next_step.is_empty() && !message.contains(&next_step) => format!("{} {}", message, next_step),
            other => other.to_string(),
        };

        MetaPalletFailure { pallet, error }
    }
}

/// One side's value at a conflicted path.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize, Clone)]
pub(crate) struct MetaConflictSide {
    /// The value, as the pallet shows it; absent when this side removed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,

    /// The key id the value is stamped with, when there is a readable value.
    #[serde(skip_serializing_if = "Option::is_none")]
    signer: Option<String>,
}

/// A path both sides changed differently since they last agreed.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize, Clone)]
pub(crate) struct MetaConflictEntry {
    /// The path as `--keep` spells it, e.g. `<subject>/<key>` on `@kv`.
    path: String,

    ours: MetaConflictSide,
    theirs: MetaConflictSide,
}

/// A meta pallet that conflicted, with every conflicted path. Nothing was lifted for it.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize, Clone)]
pub(crate) struct MetaPalletConflicts {
    /// The pallet's wire ref, e.g. `@kv`.
    pallet: String,

    conflicts: Vec<MetaConflictEntry>,
}

/// The result of a lower: the trust sync that ran first, then the pallet's outcome.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct LowerReport {
    /// The genesis of the remote's trust anchor, when it was adopted on this first contact.
    #[serde(skip_serializing_if = "Option::is_none")]
    adopted_genesis: Option<String>,

    /// Whether the office pallet moved with the remote.
    office_moved: bool,

    /// Meta pallets (e.g. `@manifest`) fast-forwarded or adopted from the remote.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    meta_adopted: Vec<String>,

    /// Meta pallets (e.g. `@kv`) whose local net change was replayed onto the remote head and
    /// lifted.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    meta_replayed: Vec<String>,

    /// Cells a replay re-signed under the current key, as `<pallet> <path>`: their stamp was
    /// one of the operator's own older keys.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    meta_restamped: Vec<String>,

    /// Meta pallets that conflicted. Never in a successful report: their paths are carried in
    /// the `meta_conflict` error's details.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    meta_conflicts: Vec<MetaPalletConflicts>,

    /// Meta pallets that failed to sync. Never in a successful report.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    meta_failed: Vec<MetaPalletFailure>,

    pallet: String,

    /// What happened to the pallet lowered (`pallet`); always present in a successful report.
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<LowerOutcome>,

    head: String,
    fetched_objects: usize,
    fetched_signatures: usize,
}

impl LowerReport {
    /// The trust and meta pallet lines: what synced before the working pallet.
    fn render_meta(&self) {
        if let Some(genesis) = &self.adopted_genesis {
            println!(
                "Adopted the remote's trust anchor (genesis {}); every parcel is signed from now on.",
                forklift_core::error::sanitize(genesis)
            );
        }

        if self.office_moved {
            println!("Office pallet updated from the remote.");
        }

        for pallet in &self.meta_adopted {
            println!("Updated the {} pallet from the remote.", pallet);
        }

        for pallet in &self.meta_replayed {
            println!("Replayed the local {} changes onto the remote's and lifted them.", pallet);
        }

        for cell in &self.meta_restamped {
            println!("Re-signed {} under your current key.", forklift_core::error::sanitize(cell));
        }

        for pallet in &self.meta_conflicts {
            println!("{} changed differently on both sides; nothing was lifted for it:", pallet.pallet);

            for conflict in &pallet.conflicts {
                println!("  {}", forklift_core::error::sanitize(&conflict.path));
                println!("    ours:   {}", describe_side(&conflict.ours));
                println!("    theirs: {}", describe_side(&conflict.theirs));
            }
        }

        for failure in &self.meta_failed {
            println!(
                "Could not sync {}: {}",
                failure.pallet, forklift_core::error::sanitize(&failure.error)
            );
        }
    }
}

fn describe_side(side: &MetaConflictSide) -> String {
    match (&side.value, &side.signer) {
        (Some(value), Some(signer)) => format!(
            "\"{}\" (key {})", forklift_core::error::sanitize(value), kv_utils::short(signer)
        ),
        (Some(value), None) => format!("\"{}\"", forklift_core::error::sanitize(value)),
        (None, _) => "(removed)".to_string(),
    }
}

impl CommandOutput for LowerReport {
    fn render_human(&self) {
        self.render_meta();

        match self.outcome {
            None => {}
            Some(LowerOutcome::UpToDate) => {
                println!("Already up to date: pallet \"{}\" is at the remote head.", self.pallet);
            }
            Some(LowerOutcome::Ahead) => {
                println!(
                    "The local pallet \"{}\" is ahead of the remote; \"lift\" from it to publish it.",
                    self.pallet
                );
            }
            Some(LowerOutcome::Lowered) => {
                println!(
                    "Lowered pallet \"{}\" to {} ({} object(s) and {} signature(s) fetched).",
                    self.pallet, self.head, self.fetched_objects, self.fetched_signatures
                );
            }
            Some(LowerOutcome::Created) => {
                println!(
                    "Created pallet \"{}\" at the remote's head {} ({} object(s) and {} signature(s) \
                    fetched); \"forklift shift {}\" moves this bay onto it.",
                    self.pallet, self.head, self.fetched_objects, self.fetched_signatures, self.pallet
                );
            }
        }
    }
}

/// Ensure there are no staged or unstaged changes (untracked files are allowed) — the
/// same rule shift and consolidate apply before they move the working directory.
///
/// # Arguments
/// * `current_tree_hash` - The tree hash of the current pallet's head (or `None`).
///
/// # Returns
/// * `Ok(())`      - If the warehouse is clean.
/// * `Err(String)` - If there are changes that lowering would overwrite.
async fn ensure_warehouse_is_clean(current_tree_hash: Option<&str>) -> Result<(), String> {
    let staged = stocktake_utils::collect_staged_changes(current_tree_hash).await?;
    let unstaged: Vec<_> = stocktake_utils::collect_unstaged_changes().await?
        .into_iter()
        .filter(|change| change.kind != ChangeKind::Untracked)
        .collect();

    if staged.is_empty() && unstaged.is_empty() {
        return Ok(());
    }

    Err(
        "There are local changes that lowering would overwrite. Stack them, restore \
        them, or park them first (see \"stocktake\" for the details).".to_string()
    )
}

/// Conflict resolutions, by bare pallet name, then cell path.
type Keeps = BTreeMap<String, BTreeMap<String, KeepSide>>;

/// Parse `--keep` values (`<pallet>:<path>=ours|theirs`) into cell paths, before anything is
/// fetched. Only a meta pallet takes resolutions.
fn parse_keeps(specs: &[String]) -> Result<Keeps, String> {
    let mut keeps = Keeps::new();

    for spec in specs {
        let malformed = || format!(
            "\"--keep {}\" is not of the form <pallet>:<path>=ours|theirs, as in \
            kv:<subject>/<key>=theirs.",
            spec
        );

        let (target, side) = spec.rsplit_once('=').ok_or_else(malformed)?;
        let (pallet, spelled) = target.split_once(':').ok_or_else(malformed)?;
        let pallet = pallet.strip_prefix(pallet_utils::META_QUALIFIER).unwrap_or(pallet);

        let side = match side {
            "ours" => KeepSide::Ours,
            "theirs" => KeepSide::Theirs,
            _ => return Err(malformed()),
        };

        let rules = meta_tree::meta_rules(pallet).ok_or(format!(
            "\"--keep {}\": @{} takes no resolutions; it is not reconciled path by path.",
            spec, pallet
        ))?;

        let path = rules.resolution_path(spelled).map_err(|why| format!("\"--keep {}\": {}", spec, why))?;

        if keeps.entry(pallet.to_string()).or_default().insert(path, side).is_some() {
            return Err(format!("\"--keep {}\" names a path another --keep already resolves.", spec));
        }
    }

    Ok(keeps)
}

/// What syncing the meta pallets did, pallet by pallet.
#[derive(Default)]
struct MetaSync {
    adopted: Vec<String>,
    replayed: Vec<String>,
    restamped: Vec<String>,
    conflicts: Vec<MetaPalletConflicts>,
    failed: Vec<MetaPalletFailure>,
}

/// How one diverged meta pallet was reconciled.
enum Reconciled {
    /// Replayed onto the remote head and lifted.
    Replayed,

    /// The remote head already held the local net change; the local head moved to it.
    Adopted,

    /// Both sides changed some paths differently. Nothing moved.
    Conflicts(MetaPalletConflicts),
}

/// Adopt the remote's non-office meta pallets and reconcile any that diverged, each on its own.
/// Each one — `@kv`, `@manifest`, `@tags`, `@haul` — replays its local net change onto the
/// remote head as one parcel and lifts it. A pallet with no rules that diverged fails with its
/// name.
async fn sync_meta(client: &RemoteClient,
                   info: &forklift_core::model::remote::WarehouseInfo,
                   keeps: &Keeps) -> MetaSync {
    let adopt = remote_utils::adopt_meta_pallets(client, info).await;

    let mut sync = MetaSync {
        adopted: adopt.adopted,
        failed: adopt.failed.into_iter()
            .map(|(pallet, error)| MetaPalletFailure::new(pallet, error))
            .collect(),
        ..MetaSync::default()
    };

    // A pallet only this side moved needs no reconciling, so lower leaves it for lift — unless a
    // resolution names it. Then it replays onto the remote head like a diverged one, which is
    // how "theirs" drops a local change the remote will not admit (a tag cut by an admin who
    // was demoted before lifting it).
    let named_ahead = adopt.ahead.iter().filter(|(name, _)| keeps.contains_key(name));
    let no_keeps = BTreeMap::new();

    for (name, remote_head) in adopt.diverged.iter().chain(named_ahead) {
        let wire = pallet_utils::PalletRef::meta(name).to_wire();
        let keep = keeps.get(name).unwrap_or(&no_keeps);

        let reconciled = match meta_tree::meta_rules(name) {
            Some(rules) => replay(client, info, name, rules, remote_head, keep, &mut sync.restamped).await,
            None => Err(format!("The @{} pallet diverged from the remote and has no rules to replay it.", name)),
        };

        match reconciled {
            Ok(Reconciled::Replayed) => sync.replayed.push(wire),
            Ok(Reconciled::Adopted) => sync.adopted.push(wire),
            Ok(Reconciled::Conflicts(conflicts)) => sync.conflicts.push(conflicts),
            Err(error) => sync.failed.push(MetaPalletFailure::new(wire, error)),
        }
    }

    // Any other pallet a resolution names has nothing to resolve; say so, unless it already
    // failed for a reason of its own.
    for pallet in keeps.keys() {
        let wire = pallet_utils::PalletRef::meta(pallet).to_wire();
        let replayed = adopt.diverged.iter().chain(&adopt.ahead).any(|(name, _)| name == pallet);

        if replayed || sync.failed.iter().any(|failure| failure.pallet == wire) {
            continue;
        }

        let error = match pallet_utils::get_meta_pallet_head(pallet) {
            Err(error) => error,
            Ok(Some(_)) if !info.pallets.contains_key(&wire) => format!(
                "{} is not on the remote yet, so there is no remote side for --keep to keep.",
                wire
            ),
            Ok(_) => format!(
                "{} holds no local change the remote lacks, so there is nothing for --keep to \
                resolve.",
                wire
            ),
        };

        sync.failed.push(MetaPalletFailure::new(wire, error));
    }

    sync
}

/// Replay a meta pallet's local net change onto the remote head and lift the result. The
/// remote's new parcels were verified when they were adopted. The local head moves only once
/// the remote has accepted the replayed parcel.
async fn replay(client: &RemoteClient,
                info: &forklift_core::model::remote::WarehouseInfo,
                name: &str,
                rules: &'static dyn PalletRules,
                remote_head: &str,
                keep: &BTreeMap<String, KeepSide>,
                restamped: &mut Vec<String>) -> Result<Reconciled, String> {
    let wire = pallet_utils::PalletRef::meta(name).to_wire();
    let local = pallet_utils::get_meta_pallet_head(name)?
        .ok_or(format!("{} has no local head to replay.", wire))?;

    let base = meta_tree::linear_common_ancestor(&local, remote_head)?;

    let plan = match meta_tree::plan_replay(name, rules, base.as_deref(), &local, remote_head, keep) {
        Ok(plan) => plan,
        Err(ReplayError::Conflicts(conflicts)) => {
            return Ok(Reconciled::Conflicts(describe_conflicts(&wire, rules, &conflicts)?));
        }
        Err(ReplayError::Refused(why)) => return Err(why),
    };

    // Every local change is on the remote already or dropped by a resolution. Nothing is signed,
    // so an operator the office no longer lets sign can still drop what the remote refuses.
    if plan.is_empty() {
        pallet_utils::update_meta_pallet_head(name, Some(&local), remote_head, &pallet_utils::RefPreconditions::NONE)?;
        return Ok(Reconciled::Adopted);
    }

    // A replay of @haul publishes its merge records only once the remote's target holds their
    // parcels, as a lift does. Judged after the empty plan, so dropping the records still works.
    let local_cells = meta_tree::cells_at(name, &local, rules)?;

    if name == haul_utils::HAUL_PALLET_NAME {
        let remote_target_head = |target: &str| info.pallets.get(target).cloned();
        let held = haul_utils::held_merges(plan.changes(), local_cells.as_deref(), &remote_target_head)?;

        if !held.is_empty() {
            let mut targets: Vec<String> = held.iter().map(|merge| format!("\"forklift lower {}\"", merge.target)).collect();
            targets.sort();
            targets.dedup();

            return Err(format!(
                "{}. Once the remote's target holds the parcel (an operator who may write it lifts it \
                there) and this warehouse holds that head ({} fetches it), run \"forklift lower\" \
                again; or drop the record with {}.",
                haul_utils::describe_held(&held), targets.join(" and "), haul_utils::drop_held(&held)
            ));
        }
    }

    let actor = config_utils::get_operator()?;
    let (office, signing_key_id) = verified_signing_office(&actor)?;

    let description = format!("Replayed the local {} onto the remote's.", wire);
    let signer = MetaSigner::local(&signing_key_id);

    let replayed = meta_tree::replay_meta_pallet(name, rules, &office, &plan, &actor, &description, &signer);

    let parcel = match replayed {
        Ok(Some(parcel)) => parcel,
        Ok(None) => {
            pallet_utils::update_meta_pallet_head(name, Some(&local), remote_head, &pallet_utils::RefPreconditions::NONE)?;
            return Ok(Reconciled::Adopted);
        }
        Err(ReplayError::Conflicts(conflicts)) => {
            return Ok(Reconciled::Conflicts(describe_conflicts(&wire, rules, &conflicts)?));
        }
        Err(ReplayError::Refused(why)) => return Err(why),
    };

    // The cells the replay re-signed: in the replayed parcel under the current key, stamped
    // with another key on the local side. Reported only once the replay is lifted.
    let mut resigned = Vec::new();

    let changes = meta_tree::net_diff(
        meta_tree::cells_at(name, remote_head, rules)?.as_deref(),
        meta_tree::cells_at(name, &parcel, rules)?.as_deref(),
    )?;

    for change in changes {
        let Some(after) = &change.after else { continue };

        if let Some(CellRead::Readable(local_cell)) = meta_tree::read_cell_in(local_cells.as_deref(), &change.path, rules)? {
            if local_cell.signer != signing_key_id
                && matches!(meta_tree::read_cell_blob(after, rules)?, CellRead::Readable(ref cell) if cell.signer == signing_key_id) {
                resigned.push(format!("{} {}", wire, rules.display_path(&change.path)));
            }
        }
    }

    remote_utils::lift_pallet(client, &wire, &parcel, Some(remote_head), info.chunking, &Progress::NONE).await?;
    // Conditioned on the office the replay was signed and checked under, as every meta write is.
    // The remote already holds the replay, so a refusal says so rather than "nothing was written".
    match pallet_utils::update_meta_pallet_head(name, Some(&local), &parcel, &office.preconditions()) {
        Ok(()) => {}
        Err(pallet_utils::RefUpdateError::Failed(why)) => return Err(why),
        Err(refused) => {
            let why = match refused {
                pallet_utils::RefUpdateError::Moved(_) => "another bay or process moved it here meanwhile",
                _ => "the office or the trust anchor it was signed under changed here meanwhile",
            };

            return Err(format!(
                "{} was lifted to the remote, but its local head was not moved: {}. Run \"forklift \
                lower\" again to catch up.",
                wire, why
            ));
        }
    }
    restamped.extend(resigned);

    Ok(Reconciled::Replayed)
}

/// The office, verified against the anchor, and the operator's signing key in it. A converted
/// pallet's writes, and the key that signs them, are judged against the verified office, never the
/// unverified local read.
pub(crate) fn verified_signing_office(actor: &Operator) -> Result<(audit_utils::VerifiedOffice, String), String> {
    let office = audit_utils::verified_local_office()?;
    let (_, signing_key_id) = office::signing_key_in(&office, actor)?;

    Ok((office, signing_key_id))
}

fn describe_conflicts(wire: &str,
                      rules: &dyn PalletRules,
                      conflicts: &[MetaConflict]) -> Result<MetaPalletConflicts, String> {
    let side = |entry: &Entry| -> Result<MetaConflictSide, String> {
        Ok(match entry {
            Entry::Blob(blob) => match meta_tree::read_cell_blob(blob, rules)? {
                CellRead::Readable(envelope) => MetaConflictSide {
                    value: Some(rules.display_value(&envelope)),
                    signer: Some(envelope.signer),
                },
                CellRead::Unknown { signer, schema } => MetaConflictSide {
                    value: Some(format!("(a {} value this forklift cannot read)", schema)),
                    signer: Some(signer),
                },
                CellRead::Malformed(why) => MetaConflictSide {
                    value: Some(format!("(malformed: {})", why)),
                    signer: None,
                },
            },
            Entry::Tree(_) => MetaConflictSide { value: Some("(several values)".to_string()), signer: None },
            Entry::Absent => MetaConflictSide { value: None, signer: None },
        })
    };

    let conflicts = conflicts.iter()
        .map(|conflict| Ok(MetaConflictEntry {
            path: rules.display_path(&conflict.path),
            ours: side(&conflict.ours)?,
            theirs: side(&conflict.theirs)?,
        }))
        .collect::<Result<Vec<_>, String>>()?;

    Ok(MetaPalletConflicts { pallet: wire.to_string(), conflicts })
}

/// The `meta_conflict` refusal for the pallets that conflicted, naming the working pallet that
/// did lower and its head.
fn meta_conflict_refusal(conflicts: &[MetaPalletConflicts], pallet: &str, head: &str) -> String {
    let count: usize = conflicts.iter().map(|pallet| pallet.conflicts.len()).sum();
    let pallets: Vec<&str> = conflicts.iter().map(|pallet| pallet.pallet.as_str()).collect();
    let example = conflicts.first()
        .and_then(|pallet| pallet.conflicts.first().map(|conflict| (pallet, conflict)))
        .map(|(pallet, conflict)| format!(
            "{}:{}", pallet.pallet.trim_start_matches(pallet_utils::META_QUALIFIER), conflict.path
        ))
        .unwrap_or_default();

    CoreError::refusal_with_details(
        RefusalCode::MetaConflict,
        format!(
            "{} path(s) of {} changed differently here and on the remote; nothing was lifted for {}.",
            count,
            pallets.join(", "),
            if pallets.len() == 1 { "it" } else { "them" }
        ),
        format!(
            "Rerun lower with --keep <pallet>:<path>=ours|theirs for each named path, e.g. \
            --keep {}=theirs.",
            example
        ),
        serde_json::json!({ "pallets": conflicts, "lowered": { "pallet": pallet, "head": head } }),
    ).into()
}

/// The error for meta pallets that failed to sync, naming each.
pub(crate) fn meta_failure_error(failed: &[MetaPalletFailure]) -> String {
    let parts: Vec<String> = failed.iter()
        .map(|failure| format!("{}: {}", failure.pallet, failure.error))
        .collect();

    format!("Could not sync {} meta pallet(s). {}", failed.len(), parts.join(" "))
}

/// A working pallet's error, with any meta pallet failures appended.
pub(crate) fn with_meta_failures(error: String, failed: &[MetaPalletFailure]) -> String {
    if failed.is_empty() {
        return error;
    }

    forklift_core::error::append_to_message(error, &format!("Also: {}", meta_failure_error(failed)))
}

/// A working pallet's error, with any meta pallet conflicts named, so they are not first heard
/// of on the lower after the working pallet is fixed.
fn with_meta_conflicts(error: String, conflicts: &[MetaPalletConflicts]) -> String {
    if conflicts.is_empty() {
        return error;
    }

    let pallets: Vec<&str> = conflicts.iter().map(|pallet| pallet.pallet.as_str()).collect();

    forklift_core::error::append_to_message(error, &format!(
        "Also: {} conflicted with the remote; nothing was lifted for {}, and lowering again lists \
        the conflicting paths.",
        pallets.join(", "),
        if pallets.len() == 1 { "it" } else { "them" }
    ))
}

/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<LowerReport>(),
    ]
}
