use std::collections::BTreeMap;
use serde::Serialize;
use forklift_core::error::{CoreError, RefusalCode};
use forklift_core::model::operator::Operator;
use forklift_core::util::meta_tree::{self, CellRead, Entry, KeepSide, MetaConflict, MetaSigner, PalletRules, ReplayError};
use forklift_core::util::office_utils::OfficeState;
use forklift_core::util::progress_utils::Progress;
use forklift_core::util::remote_utils::RemoteClient;
use forklift_core::util::stocktake_utils::ChangeKind;
use forklift_core::util::{
    audit_utils, config_utils, haul_utils, kv_utils, merge_utils, object_utils,
    pallet_utils, remote_utils, scope_utils, shift_utils, stocktake_utils, trust_utils,
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
/// The meta pallets sync next, each on its own: a converted pallet that diverged has its
/// local net change replayed onto the remote head and lifted, unless both sides changed the
/// same path differently — a conflict, which `keep` resolves path by path. A pallet that
/// conflicts or fails stops only itself; the working pallet still lowers, and the command
/// then fails with the most severe outcome.
///
/// A diverged working pallet is never merged implicitly: lower reports it and the operator
/// consolidates deliberately.
///
/// # Arguments
/// * `expect` - The genesis the remote's trust anchor must have (an out-of-band pin), checked
///              against the handshake before anything is fetched.
/// * `keep`   - Conflict resolutions, each `<pallet>:<path>=ours|theirs`.
///
/// # Returns
/// * `Ok(())`      - If every pallet is now in sync with the remote (or ahead of it).
/// * `Err(String)` - If no remote is configured, its trust anchor is not the expected one or
///                   not the one held, the warehouse is dirty, the histories diverged, a meta
///                   pallet conflicted or failed to sync, or a transfer failed.
pub async fn handle_command(expect: Option<String>, keep: Vec<String>) -> Result<(), String> {
    let keeps = parse_keeps(&keep)?;

    let progress = events::progress();
    let client = RemoteClient::from_config()?;
    let info = client.fetch_info().await?;

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
        meta_merged: meta.merged,
        meta_replayed: meta.replayed,
        meta_restamped: meta.restamped,
        meta_conflicts: meta.conflicts,
        meta_failed: meta.failed,
        pallet: pallet_utils::get_current_pallet_name()?,
        outcome: None,
        head: String::new(),
        fetched_objects: 0,
        fetched_signatures: 0,
    };

    let lowered = lower_working_pallet(&client, &info, &mut report, &progress).await;

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

    let Some(remote_head) = info.pallets.get(&pallet) else {
        return Err(format!(
            "The remote has no pallet \"{}\" (it has: {}).",
            pallet,
            info.pallets.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    };

    report.head = remote_head.clone();

    let local_head = pallet_utils::get_pallet_head(&pallet)?;

    if local_head.as_deref() == Some(remote_head.as_str()) {
        report.outcome = Some(LowerOutcome::UpToDate);
        return Ok(());
    }

    // Refuse before fetching anything: a dirty warehouse would be overwritten by the
    // fast-forward, and the operator should resolve that first.
    let local_tree = match &local_head {
        Some(hash) => Some(object_utils::load_parcel(hash)?.tree_hash),
        None => None,
    };

    ensure_warehouse_is_clean(local_tree.as_deref()).await?;

    // Path-pruned in a sparse warehouse (out-of-scope content stays sealed), whole in a full one
    // — a full fetch scope makes this byte-identical to the unscoped fetch. The office and meta
    // pallets synced above keep routing through the unscoped fetch, since their audit reads full
    // content.
    let fetch_scope = scope_utils::read_fetch_scope()?;
    let stats = remote_utils::fetch_history_scoped(client, remote_head, &fetch_scope, progress).await?;
    report.fetched_objects = stats.fetched_objects;
    report.fetched_signatures = stats.fetched_signatures;

    if let Some(local) = &local_head {
        if merge_utils::is_ancestor(remote_head, local)? {
            report.outcome = Some(LowerOutcome::Ahead);
            return Ok(());
        }

        if !merge_utils::is_ancestor(local, remote_head)? {
            return Err(format!(
                "The local pallet \"{}\" and the remote have diverged. The remote \
                parcels are fetched; palletize the remote head into its own pallet \
                (\"palletize <name> {}\"), consolidate it, and lift the result.",
                pallet,
                &remote_head[..12.min(remote_head.len())]
            ));
        }
    }

    let remote_tree = object_utils::load_parcel(remote_head)?.tree_hash;

    shift_utils::materialize_tree(local_tree.as_deref(), &remote_tree, "Lowering", progress)?;
    pallet_utils::set_pallet_head(&pallet, remote_head)?;

    report.outcome = Some(LowerOutcome::Lowered);

    Ok(())
}

/// What a lower did to the current pallet.
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
}

/// A meta pallet that failed to sync, and why.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize, Clone)]
pub(crate) struct MetaPalletFailure {
    /// The pallet's wire ref, e.g. `@kv`.
    pub(crate) pallet: String,

    pub(crate) error: String,
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

    /// Meta pallets whose diverged history was merged with the remote's.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    meta_merged: Vec<String>,

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

    /// What happened to the current pallet; always present in a successful report.
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

        for pallet in &self.meta_merged {
            println!("Merged the remote {} into the local one; \"lift\" to publish it.", pallet);
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
                    "The local pallet \"{}\" is ahead of the remote; \"lift\" to publish it.",
                    self.pallet
                );
            }
            Some(LowerOutcome::Lowered) => {
                println!(
                    "Lowered pallet \"{}\" to {} ({} object(s) and {} signature(s) fetched).",
                    self.pallet, self.head, self.fetched_objects, self.fetched_signatures
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
/// fetched. Only a converted pallet takes resolutions.
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

        let rules = meta_tree::converted_rules(pallet).ok_or(format!(
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
    merged: Vec<String>,
    replayed: Vec<String>,
    restamped: Vec<String>,
    conflicts: Vec<MetaPalletConflicts>,
    failed: Vec<MetaPalletFailure>,
}

/// How one diverged meta pallet was reconciled.
enum Reconciled {
    /// Merged: the local head now holds both sides' changes, and lift publishes it.
    Merged,

    /// Replayed onto the remote head and lifted.
    Replayed,

    /// The remote head already held the local net change; the local head moved to it.
    Adopted,

    /// Both sides changed some paths differently. Nothing moved.
    Conflicts(MetaPalletConflicts),
}

/// Adopt the remote's non-office meta pallets and reconcile any that diverged, each on its own.
/// The manifest and the haul log merge with a conflict-free two-parent join (their entries are
/// independent signed events). A converted pallet replays its local net change onto the remote
/// head as one parcel and lifts it. Anything else that diverged fails with its pallet named.
async fn sync_meta(client: &RemoteClient,
                   info: &forklift_core::model::remote::WarehouseInfo,
                   keeps: &Keeps) -> MetaSync {
    let adopt = remote_utils::adopt_meta_pallets(client, info).await;

    let mut sync = MetaSync {
        adopted: adopt.adopted,
        failed: adopt.failed.into_iter()
            .map(|(pallet, error)| MetaPalletFailure { pallet, error })
            .collect(),
        ..MetaSync::default()
    };

    let no_keeps = BTreeMap::new();

    for (name, remote_head) in &adopt.diverged {
        let wire = pallet_utils::PalletRef::meta(name).to_wire();
        let keep = keeps.get(name).unwrap_or(&no_keeps);

        let reconciled = match meta_tree::converted_rules(name) {
            Some(rules) => replay(client, info, name, rules, remote_head, keep, &mut sync.restamped).await,
            None => merge_unconverted(name, remote_head),
        };

        match reconciled {
            Ok(Reconciled::Merged) => sync.merged.push(wire),
            Ok(Reconciled::Replayed) => sync.replayed.push(wire),
            Ok(Reconciled::Adopted) => sync.adopted.push(wire),
            Ok(Reconciled::Conflicts(conflicts)) => sync.conflicts.push(conflicts),
            Err(error) => sync.failed.push(MetaPalletFailure { pallet: wire, error }),
        }
    }

    // A resolution for a pallet that did not diverge has nothing to resolve; say so, unless
    // the pallet already failed for a reason of its own.
    for pallet in keeps.keys() {
        let wire = pallet_utils::PalletRef::meta(pallet).to_wire();

        if !adopt.diverged.iter().any(|(name, _)| name == pallet)
            && !sync.failed.iter().any(|failure| failure.pallet == wire) {
            sync.failed.push(MetaPalletFailure {
                pallet: wire.clone(),
                error: format!(
                    "{} has not diverged from the remote, so there is nothing for --keep to resolve.",
                    wire
                ),
            });
        }
    }

    sync
}

/// Merge a diverged pallet that is still on its own code.
fn merge_unconverted(name: &str, remote_head: &str) -> Result<Reconciled, String> {
    let actor = config_utils::get_operator()?;
    let (_state, signing_key_id, _actor_id) = office::require_signing_actor(&actor)?;

    if name != haul_utils::HAUL_PALLET_NAME {
        return Err(format!("The @{} pallet diverged from the remote and has no merge.", name));
    }

    haul_utils::merge_hauls(remote_head, &actor, "Merged the remote hauls.".to_string(), &signing_key_id)?;

    Ok(Reconciled::Merged)
}

/// Replay a converted pallet's local net change onto the remote head and lift the result. The
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

    let actor = config_utils::get_operator()?;
    let (office, signing_key_id) = verified_signing_office(&actor)?;
    let base = meta_tree::linear_common_ancestor(&local, remote_head)?;

    let description = format!("Replayed the local {} onto the remote's.", wire);
    let signer = MetaSigner::local(&signing_key_id);

    let replayed = meta_tree::replay_meta_pallet(
        name, rules, &office, base.as_deref(), &local, remote_head, keep, &actor, &description, &signer
    );

    let parcel = match replayed {
        Ok(Some(parcel)) => parcel,
        Ok(None) => {
            pallet_utils::set_meta_pallet_head(name, remote_head)?;
            return Ok(Reconciled::Adopted);
        }
        Err(ReplayError::Conflicts(conflicts)) => {
            return Ok(Reconciled::Conflicts(describe_conflicts(&wire, rules, &conflicts)?));
        }
        Err(ReplayError::Refused(why)) => return Err(why),
    };

    // The cells the replay re-signed: in the replayed parcel under the current key, stamped
    // with another key on the local side. Reported only once the replay is lifted.
    let local_cells = meta_tree::cells_at(name, &local, rules)?;
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
    pallet_utils::set_meta_pallet_head(name, &parcel)?;
    restamped.extend(resigned);

    Ok(Reconciled::Replayed)
}

/// The office, verified against the anchor, and the operator's signing key in it. A converted
/// pallet's writes are judged against the verified office, never the unverified local read.
pub(crate) fn verified_signing_office(actor: &Operator) -> Result<(OfficeState, String), String> {
    let (_unverified, signing_key_id, _actor_id) = office::require_signing_actor(actor)?;

    Ok((audit_utils::verified_local_office()?, signing_key_id))
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
