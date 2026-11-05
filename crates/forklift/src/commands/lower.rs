use std::collections::BTreeMap;
use serde::Serialize;
use forklift_core::error::{CoreError, RefusalCode};
use forklift_core::model::operator::Operator;
use forklift_core::util::meta_tree::{self, CellRead, Entry, KeepSide, MetaConflict, MetaSigner, MetaWriter, PalletRules, ReplayError, Stamp};
use forklift_core::util::progress_utils::Progress;
use forklift_core::util::remote_utils::{BatchLaneClosure, RemoteClient};
use forklift_core::util::{
    alias_utils, audit_utils, bay_utils, config_utils, file_utils, haul_utils, held_heads, kv_utils, merge_utils, object_utils,
    operation_utils, pallet_utils, remote_utils, repair_utils, scope_utils, shift_utils, take_utils, trust_utils,
};
use forklift_core::util::take_utils::{FetchAt, TakeTrust};
use forklift_core::util::operation_utils::{Operation, OperationKind};
use crate::commands::consolidate::{self, MergeStatus};
use crate::commands::{merge_abort, office, shift};
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
/// A diverged working pallet has the remote's head merged into it, as `consolidate` would merge a
/// pallet ([`merge_diverged`]): a clean merge is stacked, and a conflicting one is left in progress
/// and refused as `merge_conflicts`, naming how to complete or abandon it. Local changes to tracked
/// files are refused (`conflict`) before anything is merged, where a fast-forward carries them.
///
/// Naming a pallet other than the current one obtains or updates that pallet's ref alone (see
/// [`lower_named_pallet`]); this bay, its files and its inventory stay where they are.
///
/// # Arguments
/// * `pallet` - The pallet to lower; `None` (or the current pallet) lowers the current pallet.
/// * `expect` - The genesis the remote's trust anchor must have (an out-of-band pin), checked
///              against the handshake before anything is fetched.
/// * `keep`   - Conflict resolutions, each `<pallet>:<path>=ours|theirs`.
/// * `confirm` - Whether a "theirs" resolution may drop the local changes it names. Without it,
///              one is refused (`confirmation_required`) before the remote is contacted.
/// * `repair`  - Whether a remote head no client can check out is repaired: a parcel stacked on it
///              removes every name that opens the metadata folder (see [`repair_pallet`]).
///
/// # Returns
/// * `Ok(())`      - If every pallet is now in sync with the remote (or ahead of it).
/// * `Err(String)` - If no remote is configured, its trust anchor is not the expected one or
///                   not the one held, a "theirs" resolution lacks `confirm`, the remote has no
///                   such pallet, the warehouse is dirty, the
///                   histories diverged, a meta pallet conflicted or failed to sync, or a transfer
///                   failed.
pub async fn handle_command(pallet: Option<String>, expect: Option<String>, keep: Vec<String>, confirm: bool, repair: bool) -> Result<(), String> {
    let keeps = parse_keeps(&keep)?;

    // Rule 5: a "theirs" resolution destroys local changes nobody else made. One is accepted only
    // where a local change lies at or beneath its path, so an accepted one always drops something
    // (or, where both sides made the same change, nothing anyone would miss).
    if !confirm {
        theirs_unconfirmed(&keep)?;
    }

    if let Some(name) = &pallet {
        pallet_utils::validate_pallet_name(name)?;
    }

    let current = pallet_utils::read_position()?.pallet;
    let named = pallet.filter(|name| *name != current);

    let progress = events::progress();
    let client = RemoteClient::for_warehouse()?;

    // A repair reads the whole of the head's tree, which a sparse warehouse never holds.
    if repair && !scope_utils::read_fetch_scope()?.is_full() {
        return Err(CoreError::refusal(
            RefusalCode::SparseWorkspace,
            "This warehouse fetches only part of each tree, so it cannot build a repair, which reads the \
            whole of the head's; the next step builds it in a new directory.",
            format!("forklift clone {} <directory> --repair", client.url()),
        ).into());
    }

    let info = client.fetch_info().await?;

    // A named pallet the remote lacks is refused before trust or a meta pallet is written: it is
    // where a missing pallet's refusal sends an operator, so it must fail cleanly and coded. The
    // remote's trust is judged first, read-only, so a pinned, stripped or changed anchor still
    // refuses as such rather than as another remote's pallet list.
    if let Some(name) = &named {
        if !info.pallets.contains_key(name) {
            remote_utils::check_remote_trust(&client, &info, expect.as_deref())?;

            return Err(remote_lacks_pallet(&info, name, &missing_named_step(name, repair)?).into());
        }
    }

    // Checks the pin (and a stripped anchor) against the handshake before anything is fetched,
    // and verifies the remote's office against the anchor it will stand under.
    let mut trust = remote_utils::prepare_remote_trust(&client, &info, expect.as_deref()).await?;

    // First contact: the pallet this lower takes is verified against the anchor the remote serves
    // before that anchor is installed, so a refusal leaves this warehouse holding none. Where it is
    // taken it is judged against the installed anchor, which by then remembers this verdict. The
    // local pallets are judged too, so a later take skips only history the anchor vouches for.
    let mut unvouched = Vec::new();
    if let Some(first_contact) = trust.take_trust() {
        let target = named.as_deref().unwrap_or(&current);
        let mut verified = Vec::new();

        if let Some(remote_head) = info.pallets.get(target) {
            remote_utils::fetch_history_scoped(&client, remote_head, target, &progress).await?;
            take_utils::verify_take(Some(&client), &first_contact, remote_head, target, FetchAt::Warehouse).await?;
            verified.push(remote_head.as_str());
        }

        unvouched = trust.judge_local_heads(&client, &verified).await?;
    }

    let trust = trust.commit()?;

    // Announced at once, not with the report: the adopted anchor is already on disk, and a
    // later refusal (a dirty warehouse, a missing pallet) would otherwise leave it unsaid.
    if let Some(genesis) = &trust.adopted_genesis {
        output::warn_trust_adopted(&trust_utils::first_contact(client.url(), genesis));
        let (meta, user): (Vec<String>, Vec<String>) = unvouched.into_iter()
            .partition(|pallet| pallet.starts_with(pallet_utils::META_QUALIFIER));
        output::warn_pallets_unvouched(&user);
        output::warn_meta_unvouched(&meta);
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
        batch_lane: None,
        repair: None,
        merged: None,
    };

    let lowered = match (named.is_some(), repair) {
        (named, true) => repair_pallet(&client, &info, named, &mut report, &progress).await,
        (true, false) => lower_named_pallet(&client, &info, &mut report, &progress).await,
        (false, false) => lower_working_pallet(&client, &info, &mut report, &progress).await,
    };

    report.batch_lane = client.batch_lane_closure();

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
    // Lifting creates the pallet on the remote only when there is something stacked to lift.
    let remote_head = match info.pallets.get(&pallet) {
        Some(head) => head,
        None => {
            let next_step = match pallet_utils::get_pallet_head(&pallet)? {
                Some(_) => "forklift lift",
                None => "forklift pallet list --remote",
            };
            return Err(remote_lacks_pallet(info, &pallet, next_step).into());
        }
    };

    report.head = remote_head.clone();

    // The fast-forward materializes from the tree this bay holds, and moves the pallet from there.
    // Checked before "up to date" too: a pallet another bay already lowered leaves this one's
    // files behind, which only catching up fixes.
    let position = pallet_utils::read_position()?;
    position.require_current("lowered")?;

    if position.base.as_deref() == Some(remote_head.as_str()) {
        record_served_head(&pallet, remote_head);
        report.outcome = Some(LowerOutcome::UpToDate);
        return Ok(());
    }

    let local_head = position.base.clone();

    let local_tree = match &local_head {
        Some(hash) => Some(object_utils::load_parcel(hash)?.tree_hash),
        None => None,
    };

    // A merge in progress is completed on the head it started from.
    shift::refuse_mid_merge("lowering")?;

    // Path-pruned in a sparse warehouse (out-of-scope content stays sealed), whole in a full one
    // — a full fetch scope makes this byte-identical to the unscoped fetch — and in a cache only
    // the parcels, whose content it records; the head's own is fetched below, once the head is
    // known to be checked out here or by a refusal's next step. The office and meta pallets
    // synced above keep routing through the unscoped fetch, since their audit reads full content.
    let stats = remote_utils::fetch_history_scoped(client, remote_head, &pallet, progress).await?;
    report.fetched_objects = stats.fetched_objects;
    report.fetched_signatures = stats.fetched_signatures;

    if let Some(local) = &local_head {
        if merge_utils::is_ancestor(remote_head, local)? {
            record_served_head(&pallet, remote_head);
            report.outcome = Some(LowerOutcome::Ahead);
            return Ok(());
        }
    }

    if let Some(local) = &local_head {
        if !merge_utils::is_ancestor(local, remote_head)? {
            // The merge checks the remote head's tree out alongside this one, so a head no client
            // can check out is refused as that first, naming its repair.
            report.fetched_objects += remote_utils::fetch_head_content(client, remote_head, progress).await?.fetched_objects;
            refuse_unmaterializable(client, &pallet, local_tree.as_deref(), &object_utils::load_parcel(remote_head)?.tree_hash)?;

            return merge_diverged(client, &pallet, local, remote_head, "forklift lower", report, |_| Ok(remote_head.clone())).await;
        }
    }

    // Before the marker: a refused take leaves nothing standing. The write — the first one, and
    // heal's roll-forward of an interrupted one — carries the verdict's preconditions, so an office
    // or anchor change since rolls the take back rather than landing it.
    let preconditions = take_utils::verify_take(Some(client), &TakeTrust::local()?, remote_head, &pallet, FetchAt::Warehouse).await?;

    // The content this lower checks out, verified first like the history it belongs to.
    let content = remote_utils::fetch_head_content(client, remote_head, progress).await?;
    report.fetched_objects += content.fetched_objects;

    let remote_tree = object_utils::load_parcel(remote_head)?.tree_hash;

    // Refused before the marker, so a refusal leaves nothing standing (`materialize_tree` checks
    // again, finding the same). A name that opens the metadata folder is refused as the head's,
    // before the planning below trips over it.
    refuse_unmaterializable(client, &pallet, local_tree.as_deref(), &remote_tree)?;

    // Collected once the remote's tree is known, so an edit made during the fetch is judged too.
    // Anchored before that, so a file edited after it is never trusted.
    let anchor = file_utils::take_stat_anchor()?;
    let carried = shift_utils::collect_carried(local_tree.as_deref()).await?;
    let (ops, removed_dirs) = shift_utils::diff_trees(local_tree.as_deref(), &remote_tree)?;
    refuse_carried_overlaps(&carried, &ops, &removed_dirs)?;
    alias_utils::refuse_collisions("Lowering", &shift_utils::collect_collisions(&ops, &removed_dirs)?)?;
    shift_utils::ensure_writes_present(&ops)?;

    // Finished by `write_head_or_roll_back` once the bay is settled at one head or the other.
    let mut operation = Operation::new(
        OperationKind::Lower,
        position.clone(),
        local_tree.clone(),
        pallet_utils::BayPosition::new(&pallet, Some(remote_head.to_string())),
    );
    operation.target_tree = Some(remote_tree.clone());
    operation.expected = local_head.clone();
    operation.office = preconditions.office.clone();
    operation.anchor = preconditions.anchor.clone();
    operation.carried = carried.clone();
    operation_utils::raise(&operation)?;

    shift_utils::materialize_tree(local_tree.as_deref(), &remote_tree, "Lowering", anchor, progress, &carried)?;

    // A compare-and-set against the head this lower started from; the working tree goes back to
    // where it was if the pallet did not move (another bay moved it first, or the write failed).
    let written = shift_utils::write_head_or_roll_back(&pallet, local_head.as_deref(), remote_head, &preconditions, &remote_tree, local_tree.as_deref(), anchor, &carried);
    // A write that reported an error can still have landed; the ref read back decides.
    if written.is_ok() || pallet_utils::get_pallet_head(&pallet).ok().flatten().as_deref() == Some(remote_head.as_str()) {
        record_served_head(&pallet, remote_head);
    }
    written?;

    report.outcome = Some(LowerOutcome::Lowered);

    Ok(())
}

/// Refuse the remote head of `pallet`, whose tree is `remote_tree`, with `head_not_materializable`
/// when checking it out over `local_tree` in this bay's scope would write a name that opens the
/// metadata folder.
fn refuse_unmaterializable(client: &RemoteClient, pallet: &str, local_tree: Option<&str>, remote_tree: &str) -> Result<(), String> {
    let Some(path) = shift_utils::first_unmaterializable_path(local_tree, remote_tree, &scope_utils::current_scope()?)? else {
        return Ok(());
    };

    // A sparse warehouse cannot build the repair, so its refusal sends the repair to a new clone.
    let route = if scope_utils::read_fetch_scope()?.is_full() {
        remote_utils::RepairRoute::Lower
    } else {
        remote_utils::RepairRoute::NewClone(client.url())
    };

    Err(remote_utils::head_not_materializable_refusal(pallet, &path, route).into())
}

/// The next step for a named lower of `pallet`, which the remote does not list. A repair of a pallet
/// whose head here cannot be checked out either has nothing on the remote to repair, so deleting it
/// is the way out; anything else lists what another name could be.
fn missing_named_step(pallet: &str, repair: bool) -> Result<String, String> {
    if repair {
        if let Some(head) = pallet_utils::get_pallet_head(pallet)? {
            if repair_utils::first_repairable_path(&head)?.is_some() {
                return Ok(format!("forklift pallet delete {}", pallet));
            }
        }
    }

    Ok("forklift pallet list --remote".to_string())
}

/// Lower `report.pallet` with `--repair` (the current pallet unless `named`): when the remote's head
/// H holds a name no client can check out, stack a parcel R on it that removes every such name
/// ([`repair_utils::repair_head`]) and stand R wherever the lower would have stood H. A head with
/// nothing to repair lowers as usual.
///
/// It acts on the pallet's ref V, not on where this bay stands, since several bays can share it:
/// - V unborn, at H or behind it: the ref moves from V to R. For the current pallet, the bay, whose
///   base must be V or behind it, is moved onto R; a bay whose base is not is refused as behind.
/// - V holding parcels H lacks: for the current pallet, R is merged into V as a diverged lower merges
///   H ([`merge_diverged`]), from a bay standing at V; a named pallet is refused as diverged, naming
///   the shift onto it and this lower, since a named lower leaves the bay alone.
/// - V ahead of H: the lower runs as usual, finding it ahead.
async fn repair_pallet(client: &RemoteClient,
                       info: &forklift_core::model::remote::WarehouseInfo,
                       named: bool,
                       report: &mut LowerReport,
                       progress: &Progress) -> Result<(), String> {
    let pallet = report.pallet.clone();

    let Some(remote_head) = info.pallets.get(&pallet).cloned() else {
        return lower_working_pallet(client, info, report, progress).await;
    };

    report.head = remote_head.clone();

    let stats = remote_utils::fetch_history_scoped(client, &remote_head, &pallet, progress).await?;
    report.fetched_objects = stats.fetched_objects;
    report.fetched_signatures = stats.fetched_signatures;

    let ref_head = pallet_utils::get_pallet_head(&pallet)?;
    let behind_remote = match &ref_head {
        None => true,
        Some(head) => head == &remote_head || merge_utils::is_ancestor(head, &remote_head)?,
    };
    let ahead_of_remote = !behind_remote && merge_utils::is_ancestor(&remote_head, ref_head.as_deref().unwrap_or_default())?;

    // An ahead ref lowers as usual without the head's content; anything else reads its whole tree.
    let repairable = !ahead_of_remote && {
        report.fetched_objects += remote_utils::fetch_head_content(client, &remote_head, progress).await?.fetched_objects;
        repair_utils::first_repairable_path(&remote_head)?.is_some()
    };

    if !repairable {
        // The plain lower fetches again, finding present what was fetched here, so it counts both.
        let fetched = (report.fetched_objects, report.fetched_signatures);
        let lowered = match named {
            true => lower_named_pallet(client, info, report, progress).await,
            false => lower_working_pallet(client, info, report, progress).await,
        };
        report.fetched_objects += fetched.0;
        report.fetched_signatures += fetched.1;

        return lowered;
    }

    let position = pallet_utils::read_position()?;

    // The bay moves from its own base to the repair, so it must hold nothing V lacks: a bay behind V
    // is fine, a bay beside it is refused as behind, as a plain lower refuses it.
    if !named && behind_remote {
        if let (Some(base), Some(head)) = (&position.base, &ref_head) {
            if base != head && !merge_utils::is_ancestor(base, head)? {
                position.require_current("lowered")?;
            }
        }
    }

    if !behind_remote {
        if named {
            return Err(CoreError::refusal(
                RefusalCode::Diverged,
                format!(
                    "The local pallet \"{p}\" and the remote have diverged, and the remote's head holds a \
                    name no client can check out. Lowering it with --repair from the pallet itself merges \
                    the repair in.",
                    p = pallet
                ),
                format!("forklift shift {}; then forklift lower --repair", pallet),
            ).into());
        }

        // The merge is built on the tree this bay holds, and moves the pallet from there.
        position.require_current("lowered")?;
        shift::refuse_mid_merge("lowering")?;

        let ours = ref_head.clone().ok_or("A diverged pallet has a head.")?;
        return merge_diverged(client, &pallet, &ours, &remote_head, "forklift lower --repair", report, |report| {
            let repair = repair_utils::repair_head(&remote_head)?.ok_or("The remote's head has nothing to repair.")?;
            report.repair = Some(RepairReport { parcel: repair.parcel.clone(), removed: repair.removed });

            Ok(repair.parcel)
        }).await;
    }

    let preconditions = take_utils::verify_take(Some(client), &TakeTrust::local()?, &remote_head, &pallet, FetchAt::Warehouse).await?;

    if named && behind_remote && ref_head.is_none() {
        refuse_a_bay_on_unborn(&pallet)?;
    }

    // The head write carries the take's preconditions, which are the signing's too: an unsigned
    // repair was judged with no anchor held, a signed one under the office it was signed against.
    let repair = repair_utils::repair_head(&remote_head)?.ok_or("The remote's head has nothing to repair.")?;
    report.repair = Some(RepairReport { parcel: repair.parcel.clone(), removed: repair.removed.clone() });

    if named {
        return write_named_head(&pallet, ref_head.as_deref(), &repair.parcel, &remote_head, &preconditions, report, LowerOutcome::Repaired);
    }

    let local_tree = match &position.base {
        Some(hash) => Some(object_utils::load_parcel(hash)?.tree_hash),
        None => None,
    };
    let anchor = file_utils::take_stat_anchor()?;

    shift::refuse_mid_merge("lowering")?;
    let carried = shift_utils::collect_carried(local_tree.as_deref()).await?;

    refuse_unmaterializable(client, &pallet, local_tree.as_deref(), &repair.tree)?;
    let (ops, removed_dirs) = shift_utils::diff_trees(local_tree.as_deref(), &repair.tree)?;
    refuse_carried_overlaps(&carried, &ops, &removed_dirs)?;
    alias_utils::refuse_collisions("Lowering", &shift_utils::collect_collisions(&ops, &removed_dirs)?)?;
    shift_utils::ensure_writes_present(&ops)?;

    // The bay moves from its base, but the ref from V: the marker and the compare-and-set take V.
    let mut operation = Operation::new(
        OperationKind::Lower,
        position.clone(),
        local_tree.clone(),
        pallet_utils::BayPosition::new(&pallet, Some(repair.parcel.clone())),
    );
    operation.target_tree = Some(repair.tree.clone());
    operation.expected = ref_head.clone();
    operation.office = preconditions.office.clone();
    operation.anchor = preconditions.anchor.clone();
    operation.carried = carried.clone();
    operation_utils::raise(&operation)?;

    shift_utils::materialize_tree(local_tree.as_deref(), &repair.tree, "Lowering", anchor, progress, &carried)?;

    let written = shift_utils::write_head_or_roll_back(&pallet, ref_head.as_deref(), &repair.parcel, &preconditions, &repair.tree, local_tree.as_deref(), anchor, &carried);
    // The held-heads record keeps H, the head the remote served, which the repair descends from.
    if written.is_ok() || pallet_utils::get_pallet_head(&pallet).ok().flatten().as_deref() == Some(repair.parcel.as_str()) {
        record_served_head(&pallet, &remote_head);
    }
    written?;

    report.outcome = Some(LowerOutcome::Repaired);

    Ok(())
}

/// Merge into `pallet`, which this bay stands on at `ours`, the parcel `theirs` builds: the remote's
/// head `served`, or a repair stacked on it. The warehouse must hold no changes to tracked files,
/// since the merge writes the bay's files and inventory; `command` is the lower the refusal's next
/// step runs again once they are parked. A clean merge is stacked and reported `merged`, and the
/// pallet then descends from `served`, so a lift fast-forwards it. A conflicting merge is left in
/// progress and refused as `merge_conflicts`.
async fn merge_diverged(client: &RemoteClient,
                        pallet: &str,
                        ours: &str,
                        served: &str,
                        command: &str,
                        report: &mut LowerReport,
                        theirs: impl FnOnce(&mut LowerReport) -> Result<String, String>) -> Result<(), String> {
    let our_tree = object_utils::load_parcel(ours)?.tree_hash;

    // Anchored before the cleanliness check, so a file edited after it is never trusted.
    let anchor = file_utils::take_stat_anchor()?;
    consolidate::ensure_warehouse_is_clean(Some(&our_tree), "merging the remote's head in", command).await?;

    // The remote's head becomes a parent of the merge parcel, so it is verified before anything is
    // merged. The merge's own write is conditioned by `stack`, which judges it again.
    take_utils::verify_take(Some(client), &TakeTrust::local()?, served, pallet, FetchAt::Warehouse).await?;

    let theirs = theirs(report)?;

    match consolidate::merge_head_into_current(pallet, ours, &theirs, REMOTE_LABEL, Some(pallet), true, anchor).await? {
        MergeStatus::Merged(parcel) => {
            record_served_head(pallet, served);
            report.merged = Some(parcel);
            report.outcome = Some(LowerOutcome::Merged);

            Ok(())
        }
        MergeStatus::Conflicts(paths) => Err(merge_conflicts_refusal(pallet, &paths)),
    }
}

/// The label a merge of the remote's head records as its other side, in the consolidation state
/// and the merge parcel's description: this lower's and the lift's auto-merge alike.
pub(crate) const REMOTE_LABEL: &str = "remote";

/// How many conflicted paths the refusal's message names; `details.paths` carries them all.
const LISTED_PATHS: usize = 10;

/// `paths` for the refusal's message: the first [`LISTED_PATHS`], then how many more there are.
fn listed_paths(paths: &[String]) -> String {
    let listed = paths[..paths.len().min(LISTED_PATHS)].join(", ");
    match paths.len().saturating_sub(LISTED_PATHS) {
        0 => listed,
        more => format!("{}, and {} more", listed, more),
    }
}

/// The [`RefusalCode::MergeConflicts`] refusal for a merge of the remote's head into `pallet` that
/// conflicts on `paths`, now left in progress.
fn merge_conflicts_refusal(pallet: &str, paths: &[String]) -> String {
    CoreError::refusal_with_details(
        RefusalCode::MergeConflicts,
        format!(
            "The local pallet \"{}\" and the remote had diverged, and merging the remote's head in \
            conflicts on these paths, now holding conflict markers: {}. Resolve them, load and stack \
            them, then lift; or abandon the merge with \"{}\".",
            pallet,
            listed_paths(paths),
            merge_abort::ABORT_COMMAND
        ),
        "forklift load <path>; then forklift stack; then forklift lift",
        serde_json::json!({ "paths": paths }),
    ).into()
}

/// Add the remote head a lower was served to the held-heads record, once `pallet`'s ref or the
/// bay's base holds it or a head descending from it. Every outcome that leaves the ref there records, up to date and ahead
/// included, so the next lower renews an entry a failed write lost. The ref stands either way, so a
/// failure only warns.
fn record_served_head(pallet: &str, remote_head: &str) {
    if let Err(why) = held_heads::record_held_head(pallet, remote_head) {
        output::warn_held_head_unrecorded(&why);
    }
}

/// The remote's head of user pallet `pallet`, or the refusal that it has none. The refusal lists
/// only the remote's user pallets: a meta pallet is synced by every lower and never named.
fn remote_pallet_head<'a>(info: &'a forklift_core::model::remote::WarehouseInfo,
                          pallet: &str,
                          next_step: &str) -> Result<&'a String, String> {
    info.pallets.get(pallet).ok_or_else(|| remote_lacks_pallet(info, pallet, next_step).into())
}

/// The [`RefusalCode::PalletNotFound`] refusal for a remote that has no user pallet `pallet`,
/// naming the ones it has. `next_step` is the remedy at the site: `lift` publishes a current
/// pallet with something stacked, `pallet list --remote` lists what another name could be. A pallet
/// deleted on the remote is refused naming the deletion and its restore instead.
#[track_caller]
fn remote_lacks_pallet(info: &forklift_core::model::remote::WarehouseInfo, pallet: &str, next_step: &str) -> CoreError {
    // A deleted pallet's history is still there, under its tombstone: restoring it is the way on.
    if let Some(head) = info.tombstone(pallet) {
        return CoreError::refusal(
            RefusalCode::PalletNotFound,
            format!(
                "The remote's pallet \"{}\" was deleted at {}; the remote still holds its history, \
                and the next step brings it back there.",
                pallet, head
            ),
            format!("forklift pallet restore {} --remote", pallet),
        );
    }

    let user: Vec<&str> = info.pallets.keys()
        .map(String::as_str)
        .filter(|name| !name.starts_with(pallet_utils::META_QUALIFIER))
        .collect();

    // The listing is pointed at unless it is the step itself.
    let listing = if next_step.contains("pallet list --remote") { "" } else { " \"forklift pallet list --remote\" lists them." };

    CoreError::refusal(
        RefusalCode::PalletNotFound,
        format!(
            "The remote has no pallet \"{}\" (it has: {}).{}",
            pallet,
            if user.is_empty() { "none".to_string() } else { user.join(", ") },
            listing
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
    let remote_head = remote_pallet_head(info, &pallet, "forklift pallet list --remote")?;

    report.head = remote_head.clone();

    let local_head = pallet_utils::get_pallet_head(&pallet)?;

    if local_head.as_deref() == Some(remote_head.as_str()) {
        record_served_head(&pallet, remote_head);
        report.outcome = Some(LowerOutcome::UpToDate);
        return Ok(());
    }

    let stats = remote_utils::fetch_history_scoped(client, remote_head, &pallet, progress).await?;
    report.fetched_objects = stats.fetched_objects;
    report.fetched_signatures = stats.fetched_signatures;

    if let Some(local) = &local_head {
        if merge_utils::is_ancestor(remote_head, local)? {
            record_served_head(&pallet, remote_head);
            report.outcome = Some(LowerOutcome::Ahead);
            return Ok(());
        }

        // Not merged as the working pallet is: the merge writes this bay's files, which a named lower
        // promised to leave alone. The reconciliation happens on the pallet itself.
        if !merge_utils::is_ancestor(local, remote_head)? {
            return Err(CoreError::refusal(
                RefusalCode::Diverged,
                format!(
                    "The local pallet \"{p}\" and the remote have diverged; the remote parcels are \
                    fetched. Lowering it from the pallet itself merges the remote's head in.",
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

    // The write carries the verdict's preconditions, so an office or anchor change since fails it.
    let preconditions = take_utils::verify_take(Some(client), &TakeTrust::local()?, remote_head, &pallet, FetchAt::Warehouse).await?;

    // A bay standing on the pallet while it is unborn has no base to catch up from: created
    // under it, the pallet would refuse that bay's stack as behind, and its catch-up would read
    // all of its files as changes to overwrite. Judged after the fetch, just before the write, so
    // a bay that moved there while it ran is seen.
    if local_head.is_none() {
        refuse_a_bay_on_unborn(&pallet)?;
    }

    write_named_head(&pallet, local_head.as_deref(), remote_head, remote_head, &preconditions, report, moved)
}

/// Move the ref of `pallet`, which no bay here is lowering, from `expected` to `new` (the remote's
/// head `served`, or a repair of it), recording `moved` as the outcome and `served` as a held head
/// once the ref holds `new`. A write that reported an error can still have landed, so the ref read
/// back decides.
fn write_named_head(pallet: &str,
                    expected: Option<&str>,
                    new: &str,
                    served: &str,
                    preconditions: &pallet_utils::RefPreconditions,
                    report: &mut LowerReport,
                    moved: LowerOutcome) -> Result<(), String> {
    let error = match pallet_utils::update_pallet_head(pallet, expected, new, preconditions) {
        Ok(()) => {
            record_served_head(pallet, served);
            report.outcome = Some(moved);
            return Ok(());
        }
        Err(error) => error,
    };

    match error {
        // Another writer already put the pallet where this lower would have.
        pallet_utils::RefUpdateError::Moved(found) if found.found.as_deref() == Some(new) => {
            record_served_head(pallet, served);
            report.outcome = Some(LowerOutcome::UpToDate);
            Ok(())
        }
        // Not the behind refusal: its remedy, a shift, would carry this bay onto the pallet.
        pallet_utils::RefUpdateError::Moved(found) => Err(format!(
            "{} Run \"forklift lower {}\" again.", found, pallet
        )),
        pallet_utils::RefUpdateError::Failed(error) => match pallet_utils::get_pallet_head(pallet) {
            Ok(head) if head.as_deref() == Some(new) => {
                record_served_head(pallet, served);
                Err(format!("Pallet \"{}\" now holds parcel {}, but its write reported an error: {}", pallet, new, error))
            }
            Ok(_) => Err(format!("Pallet \"{}\" was not moved: {}", pallet, error)),
            Err(read) => Err(format!(
                "Writing pallet \"{}\" failed ({}), and reading it back failed too ({}); it may or may \
                not hold parcel {}.",
                pallet, error, read, new
            )),
        },
        refused => Err(refused.into()),
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

    /// The remote head held a name no client can check out, and the pallet now points at a repair
    /// parcel stacked on it (`repair`); `forklift lift` publishes it.
    Repaired,

    /// The local pallet and the remote had diverged, and the remote's head (or, with `repair`, a
    /// repair stacked on it) was merged in cleanly: the merge parcel (`merged`) is stacked, and
    /// `forklift lift` publishes it.
    Merged,
}

/// The repair parcel a `--repair` lower stacked.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct RepairReport {
    /// The repair parcel, whose only parent is the remote head.
    pub(crate) parcel: String,

    /// The paths it removed, in name order.
    pub(crate) removed: Vec<String>,
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
    /// A failure carrying a refusal's human message with its next step folded in: the frame never
    /// reaches the `--json` report or the joined error, which carries no code of its own.
    pub(crate) fn new(pallet: String, error: String) -> MetaPalletFailure {
        let error = forklift_core::error::CoreError::from(error).to_string();

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

    /// The key id the value is stamped with, when there is a readable value written with a key.
    #[serde(skip_serializing_if = "Option::is_none")]
    signer: Option<String>,

    /// The operator a readable value written without a key declares.
    #[serde(skip_serializing_if = "Option::is_none")]
    operator: Option<String>,
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
    /// one of the operator's own older keys, or the operator's own from before trust.
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

    /// Why the remote's batch download stopped being used, when it did. Absent when it stayed open.
    #[serde(skip_serializing_if = "Option::is_none")]
    batch_lane: Option<BatchLaneClosure>,

    /// The repair parcel the pallet now points at, when the outcome is `repaired`, or the one merged
    /// in, when it is `merged` from a diverged pallet's repair.
    #[serde(skip_serializing_if = "Option::is_none")]
    repair: Option<RepairReport>,

    /// The merge parcel stacked, when the outcome is `merged`.
    #[serde(skip_serializing_if = "Option::is_none")]
    merged: Option<String>,
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
    match (&side.value, &side.signer, &side.operator) {
        (Some(value), Some(signer), _) => format!(
            "\"{}\" (key {})", forklift_core::error::sanitize(value), kv_utils::short(signer)
        ),
        (Some(value), None, Some(operator)) => format!(
            "\"{}\" (unsigned, by {})", forklift_core::error::sanitize(value), forklift_core::error::sanitize(operator)
        ),
        (Some(value), None, None) => format!("\"{}\"", forklift_core::error::sanitize(value)),
        (None, _, _) => "(removed)".to_string(),
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
                    "The local pallet \"{}\" is ahead of the remote; \"forklift lift\" from it to publish it.",
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
            Some(LowerOutcome::Repaired) => {
                let repair = self.repair.as_ref().map(|repair| (repair.parcel.as_str(), repair.removed.join("\", \""))).unwrap_or_default();
                println!(
                    "Repaired the remote's head {} of pallet \"{}\": parcel {} removes \"{}\", which no \
                    client can check out. \"forklift lift\" publishes it.",
                    self.head, self.pallet, repair.0, forklift_core::error::sanitize(&repair.1)
                );
            }
            Some(LowerOutcome::Merged) => {
                let theirs = match &self.repair {
                    Some(repair) => format!("a repair ({}) of the remote's head {}", repair.parcel, self.head),
                    None => format!("the remote's head {}", self.head),
                };
                println!(
                    "Merged {} into pallet \"{}\", which had diverged from it: parcel {} ({} object(s) and {} \
                    signature(s) fetched). \"forklift lift\" publishes it.",
                    theirs, self.pallet, self.merged.as_deref().unwrap_or_default(), self.fetched_objects, self.fetched_signatures
                );
            }
        }

        output::print_batch_lane(self.batch_lane);
    }
}

/// Refuse a lower whose diff (`ops` and `removed_dirs`) reaches a path with a local change: it
/// cannot write or remove that path, or the directory around it, and keep the change. Changes
/// the diff leaves alone are carried across it. Untracked files are judged by the collision check.
fn refuse_carried_overlaps(carried: &[shift_utils::CarriedEntry], ops: &[shift_utils::FileOp], removed_dirs: &[String]) -> Result<(), String> {
    let overlaps = shift_utils::carried_overlaps(carried, ops, removed_dirs);

    if overlaps.is_empty() {
        return Ok(());
    }

    const SHOWN: usize = 10;
    let mut named = overlaps.iter().take(SHOWN).map(|path| format!("\"{}\"", path.escape_debug())).collect::<Vec<_>>().join(", ");
    if overlaps.len() > SHOWN {
        named.push_str(&format!(" and {} more", overlaps.len() - SHOWN));
    }

    Err(format!(
        "Lowering would change {}, which {} local changes. Stack them, restore them, or park them \
        first (see \"forklift stocktake\" for the details).",
        named,
        if overlaps.len() == 1 { "has" } else { "have" }
    ))
}

/// Conflict resolutions, by bare pallet name, then cell path.
type Keeps = BTreeMap<String, BTreeMap<String, KeepSide>>;

/// Parse `--keep` values (`@<pallet>:<path>=ours|theirs`) into cell paths, before anything is
/// fetched. Only a meta pallet takes resolutions, and it is spelled with its `@` as everywhere
/// else on the command line; a bare name is refused rather than read as the meta pallet.
fn parse_keeps(specs: &[String]) -> Result<Keeps, String> {
    let mut keeps = Keeps::new();

    for spec in specs {
        let malformed = || format!(
            "\"--keep {}\" is not of the form @<pallet>:<path>=ours|theirs, as in \
            @kv:<subject>/<key>=theirs.",
            spec
        );

        let (target, side) = spec.rsplit_once('=').ok_or_else(malformed)?;
        let (pallet, spelled) = target.split_once(':').ok_or_else(malformed)?;

        let side = match side {
            "ours" => KeepSide::Ours,
            "theirs" => KeepSide::Theirs,
            _ => return Err(malformed()),
        };

        // A bare name is never read as the meta pallet. The `@` spelling is suggested only when
        // it would be accepted; any other bare name is the malformed case.
        let Some(pallet) = pallet.strip_prefix(pallet_utils::META_QUALIFIER) else {
            if meta_tree::meta_rules(pallet).is_none_or(|rules| rules.resolution_path(spelled).is_err()) {
                return Err(malformed());
            }
            return Err(format!(
                "\"--keep {}\" names the pallet without its \"{}\": a resolution names a meta \
                pallet, as in --keep {}{}.",
                spec, pallet_utils::META_QUALIFIER, pallet_utils::META_QUALIFIER, spec
            ));
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
pub(crate) enum Reconciled {
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
    // A pallet the remote does not hold yet has no remote side, yet a resolution may name it:
    // "theirs" there drops local changes from the root parcel the replay stacks in place of the
    // local chain.
    let named_ahead = adopt.ahead.iter()
        .filter(|(name, _)| keeps.contains_key(name))
        .map(|(name, remote_head)| (name.clone(), Some(remote_head.as_str())));
    let named_unborn = keeps.keys()
        .filter(|name| !info.pallets.contains_key(&pallet_utils::PalletRef::meta(name.as_str()).to_wire()))
        .filter(|name| matches!(pallet_utils::get_meta_pallet_head(name), Ok(Some(_))))
        .map(|name| (name.clone(), None));
    let replays: Vec<(String, Option<&str>)> = adopt.diverged.iter()
        .map(|(name, remote_head)| (name.clone(), Some(remote_head.as_str())))
        .chain(named_ahead)
        .chain(named_unborn)
        .collect();
    let no_keeps = BTreeMap::new();

    for (name, remote_head) in &replays {
        let wire = pallet_utils::PalletRef::meta(name).to_wire();
        let keep = keeps.get(name).unwrap_or(&no_keeps);

        let reconciled = match meta_tree::meta_rules(name) {
            Some(rules) => {
                replay(client, info, name, rules, *remote_head, None, "lower", keep, &mut sync.restamped).await
            }
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
        let replayed = replays.iter().any(|(name, _)| name == pallet)
            || adopt.ahead.iter().any(|(name, _)| name == pallet);

        if replayed || sync.failed.iter().any(|failure| failure.pallet == wire) {
            continue;
        }

        let error = match pallet_utils::get_meta_pallet_head(pallet) {
            Err(error) => error,
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

/// Replay a meta pallet's local net change onto the remote head — or, when the remote holds no
/// head (`None`), as a root parcel — and lift the result. The remote's new parcels were
/// verified when they were adopted. The local head moves only once the remote has accepted the
/// replayed parcel. `landed` is `(pallet, head)` for a working pallet lifted just before, which
/// stands in for the handshake's head of it when a `@haul` merge record waits for it;
/// `command` is the one to run again once such a record can go.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn replay(client: &RemoteClient,
                           info: &forklift_core::model::remote::WarehouseInfo,
                           name: &str,
                           rules: &'static dyn PalletRules,
                           remote_head: Option<&str>,
                           landed: Option<(&str, &str)>,
                           command: &str,
                           keep: &BTreeMap<String, KeepSide>,
                           restamped: &mut Vec<String>) -> Result<Reconciled, String> {
    let wire = pallet_utils::PalletRef::meta(name).to_wire();
    let local = pallet_utils::get_meta_pallet_head(name)?
        .ok_or(format!("{} has no local head to replay.", wire))?;

    let base = match remote_head {
        Some(remote_head) => meta_tree::linear_common_ancestor(&local, remote_head)?,
        None => None,
    };

    // Planned under the office alone, so a resolution that drops every local change needs no key;
    // the office also recognises a local cell the remote already holds re-signed.
    let office = audit_utils::local_meta_office()?;
    let planned = meta_tree::plan_replay(
        name, rules, base.as_deref(), &local, remote_head, office.as_ref().map(|office| &office.state), keep,
    );

    let plan = match planned {
        Ok(plan) => plan,
        Err(ReplayError::Conflicts(conflicts)) => {
            return Ok(Reconciled::Conflicts(describe_conflicts(&wire, rules, &conflicts)?));
        }
        Err(ReplayError::Refused(why)) => return Err(why),
    };

    // Every local change is on the remote already or dropped by a resolution. Nothing is signed,
    // so an operator the office no longer lets sign can still drop what the remote refuses. With
    // no remote head there is nothing to move to: the replay stacks an empty root instead.
    if let (true, Some(remote_head)) = (plan.is_empty(), remote_head) {
        pallet_utils::update_meta_pallet_head(name, Some(&local), remote_head, &pallet_utils::RefPreconditions::NONE)?;
        return Ok(Reconciled::Adopted);
    }

    // A replay of @haul publishes its merge records only once the remote's target holds their
    // parcels, as a lift does. Judged after the empty plan, so dropping the records still works.
    let local_cells = meta_tree::cells_at(name, &local, rules)?;

    if name == haul_utils::HAUL_PALLET_NAME {
        let remote_target_head = |target: &str| match landed {
            Some((pallet, head)) if pallet == target => Some(head.to_string()),
            _ => info.pallets.get(target).cloned(),
        };
        let held = haul_utils::held_merges(plan.changes(), local_cells.as_deref(), &remote_target_head)?;

        if !held.is_empty() {
            let mut targets: Vec<String> = held.iter().map(|merge| format!("\"forklift lower {}\"", merge.target)).collect();
            targets.sort();
            targets.dedup();

            return Err(format!(
                "{}. Once the remote's target holds the parcel (an operator who may write it lifts it \
                there) and this warehouse holds that head ({} fetches it), run \"forklift {}\" \
                again; or drop the record with {}.",
                haul_utils::describe_held(&held), targets.join(" and "), command, haul_utils::drop_held(&held)
            ));
        }
    }

    let actor = config_utils::get_operator()?;
    let writer = match office {
        None => MetaWriter::Unsigned,
        Some(office) => {
            let (_, signing_key_id) = office::signing_key_in(&office, &actor)
                .map_err(|why| no_key_refusal(why, &wire, rules, &plan, remote_head.is_some()))?;

            MetaWriter::Signed { office, signer: MetaSigner::local(&signing_key_id) }
        }
    };

    let description = match remote_head {
        Some(_) => format!("Replayed the local {} onto the remote's.", wire),
        None => format!("Replayed the local {} as the remote's first parcel.", wire),
    };
    let replayed = meta_tree::replay_meta_pallet(name, rules, &writer, &plan, &actor, &description);

    let parcel = match replayed {
        Ok(Some(parcel)) => parcel,
        Ok(None) => {
            let remote_head = remote_head.ok_or(format!("The replay of {} onto no remote head stacked nothing.", wire))?;
            pallet_utils::update_meta_pallet_head_served(name, Some(&local), remote_head, remote_head, &pallet_utils::RefPreconditions::NONE)?;
            return Ok(Reconciled::Adopted);
        }
        Err(ReplayError::Conflicts(conflicts)) => {
            return Ok(Reconciled::Conflicts(describe_conflicts(&wire, rules, &conflicts)?));
        }
        Err(ReplayError::Refused(why)) => return Err(why),
    };

    // The cells the replay re-signed: in the replayed parcel under the current key, stamped
    // with another key on the local side. Reported only once the replay is lifted.
    let own = writer.stamp(&actor);
    let mut resigned = Vec::new();

    let remote_cells = match remote_head {
        Some(remote_head) => meta_tree::cells_at(name, remote_head, rules)?,
        None => None,
    };
    let changes = meta_tree::net_diff(
        remote_cells.as_deref(),
        meta_tree::cells_at(name, &parcel, rules)?.as_deref(),
    )?;

    for change in changes {
        let Some(after) = &change.after else { continue };

        if let Some(CellRead::Readable(local_cell)) = meta_tree::read_cell_in(local_cells.as_deref(), &change.path, rules)? {
            if local_cell.stamp != own
                && matches!(meta_tree::read_cell_blob(after, rules)?, CellRead::Readable(ref cell) if cell.stamp == own) {
                resigned.push(format!("{} {}", wire, rules.display_path(&change.path)));
            }
        }
    }

    remote_utils::lift_pallet(client, &wire, &parcel, remote_head, None, info, &Progress::NONE).await?;

    // The remote now holds the re-signed cells whatever happens to the local head, so a refusal
    // below names them too.
    let resigned_note = match resigned.is_empty() {
        true => String::new(),
        false => format!(" The remote holds {} re-signed under your current key.", resigned.join(", ")),
    };

    // Conditioned on the trust state the replay was written and checked under, as every meta
    // write is. The remote already holds the replay, so a refusal says so rather than "nothing was
    // written".
    const REMOTE_ONLY: &str = "Only the remote was written.";

    // The replay is judged against the remote head it was stacked onto, not the remote's history
    // the local head lacks.
    let written = match remote_head {
        Some(remote_head) => pallet_utils::update_meta_pallet_head_served(name, Some(&local), &parcel, remote_head, &writer.preconditions()),
        None => pallet_utils::update_meta_pallet_head(name, Some(&local), &parcel, &writer.preconditions()),
    };

    match written {
        Ok(()) => {}
        Err(refused) => {
            use pallet_utils::RefUpdateError::*;

            let why = match &refused {
                // A write can land and still fail, so the local head is read back before saying
                // where it is.
                Failed(why) => {
                    let context = replay_failure_context(&wire, &parcel, pallet_utils::get_meta_pallet_head(name));
                    let failure = forklift_core::error::prefix_message(&context, why.clone());

                    return Err(match resigned_note.is_empty() {
                        true => failure,
                        false => forklift_core::error::append_to_message(failure, resigned_note.trim_start()),
                    });
                }
                // Refused however often it runs; framed, so it keeps its code.
                Redacted { .. } => {
                    let refusal = forklift_core::error::prefix_message(
                        &format!("{} was lifted to the remote, but this warehouse refused to move its local head there", wire),
                        refused.reported_with(REMOTE_ONLY),
                    );

                    return Err(match resigned_note.is_empty() {
                        true => refusal,
                        false => forklift_core::error::append_to_message(refusal, resigned_note.trim_start()),
                    });
                }
                Moved(_) => "another bay or process moved it here meanwhile",
                OfficeMoved { .. } | AnchorMoved | PalletsMoved { .. } | WitnessMoved { .. } => {
                    "the office or the trust anchor it was signed under changed here meanwhile"
                }
                // A replay lifts a fresh parcel onto the remote head, so it never takes the
                // pallet back, and only a user pallet has a tombstone or a completeness check:
                // none of these is expected. Running lower again would clear none of them.
                Backward { .. } | Deleted { .. } | Incomplete { .. } => {
                    return Err(format!(
                        "{} was lifted to the remote, but this warehouse refused to move its local \
                        head there.{} {}",
                        wire, resigned_note, refused.reported_with(REMOTE_ONLY)
                    ));
                }
            };

            return Err(format!(
                "{} was lifted to the remote, but its local head was not moved: {}.{} Run \"forklift \
                lower\" again to catch up.",
                wire, why, resigned_note
            ));
        }
    }
    restamped.extend(resigned);

    Ok(Reconciled::Replayed)
}

/// A replay that needs a signing key the operator does not have: the way to get one, and — when
/// the remote has a head to keep — the resolutions that drop each local change instead.
fn no_key_refusal(why: String,
                  wire: &str,
                  rules: &dyn PalletRules,
                  plan: &meta_tree::ReplayPlan,
                  remote_has_head: bool) -> String {
    let mut next = format!(
        "Replaying the local {} needs a signing key: create one with \"forklift profile keygen\", \
        and have an admin run the \"forklift office admit\" it prints",
        wire
    );

    if remote_has_head {
        let keeps: Vec<String> = plan.changes().iter()
            .map(|change| format!("--keep {}:{}=theirs", wire, rules.display_path(&change.path)))
            .collect();
        next.push_str(&format!(
            "; or drop the local changes with \"forklift lower {} --confirm\"", keeps.join(" ")
        ));
    }

    forklift_core::error::append_to_message(why, &format!("{}.", next))
}

/// Who writes a meta pallet here, by the warehouse's trust state: under trust, the verified office
/// and the operator's signing key in it — judged against the verified office, never the
/// unverified local read, and refusing an operator with no usable key; in a warehouse that never
/// enrolled, the operator unsigned. An office whose anchor is gone refuses.
pub(crate) fn meta_writer(actor: &Operator) -> Result<MetaWriter<'static>, String> {
    let Some(office) = audit_utils::local_meta_office()? else {
        return Ok(MetaWriter::Unsigned);
    };

    let (_, signing_key_id) = office::signing_key_in(&office, actor)?;

    Ok(MetaWriter::Signed { office, signer: MetaSigner::local(&signing_key_id) })
}

/// [`meta_writer`] for a command only writers and admins may run: under trust, an operator the
/// office does not hold, or holds as a reader, is refused (`may` names what they may not do).
/// Without trust there are no roles, so any operator writes.
pub(crate) fn writing_meta_writer(actor: &Operator, may: &str) -> Result<MetaWriter<'static>, String> {
    let writer = meta_writer(actor)?;

    if let Some(office) = writer.office() {
        let user = office.find_user(&actor.identifier).ok_or(format!(
            "\"{}\" is not enrolled; ask an enrolled operator to admit you.", actor.identifier
        ))?;

        if user.role == forklift_core::util::office_utils::Role::Reader {
            return Err(format!("\"{}\" is a reader; only writers and admins may {}.", user.identifier, may));
        }
    }

    Ok(writer)
}

fn describe_conflicts(wire: &str,
                      rules: &dyn PalletRules,
                      conflicts: &[MetaConflict]) -> Result<MetaPalletConflicts, String> {
    let stamped = |value: String, stamp: Stamp| match stamp {
        Stamp::Signer(key_id) => MetaConflictSide { value: Some(value), signer: Some(key_id), operator: None },
        Stamp::Operator(identifier) => MetaConflictSide { value: Some(value), signer: None, operator: Some(identifier) },
    };
    let bare = |value: Option<String>| MetaConflictSide { value, signer: None, operator: None };

    let side = |entry: &Entry| -> Result<MetaConflictSide, String> {
        Ok(match entry {
            Entry::Blob(blob) => match meta_tree::read_cell_blob(blob, rules)? {
                CellRead::Readable(envelope) => stamped(rules.display_value(&envelope), envelope.stamp),
                CellRead::Unknown { stamp, schema } => {
                    stamped(format!("(a {} value this forklift cannot read)", schema), stamp)
                }
                CellRead::Malformed(why) => bare(Some(format!("(malformed: {})", why))),
            },
            Entry::Tree(_) => bare(Some("(several values)".to_string())),
            Entry::Absent => bare(None),
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

/// Where a replay lifted to the remote left the local head, after the local write failed:
/// `read_back` is the local head read after the failure, since a write can land and still fail.
fn replay_failure_context(wire: &str, parcel: &str, read_back: Result<Option<String>, String>) -> String {
    match read_back {
        Ok(Some(head)) if head == parcel => format!(
            "{} was lifted to the remote, and its local head moved there, but the move may not be durable", wire
        ),
        Ok(_) => format!("{} was lifted to the remote, but this warehouse could not move its local head there", wire),
        Err(_) => format!("{} was lifted to the remote, but whether its local head moved there is not known", wire),
    }
}

/// The `meta_conflict` refusal for the pallets that conflicted, naming the working pallet that
/// did lower and its head.
fn meta_conflict_refusal(conflicts: &[MetaPalletConflicts], pallet: &str, head: &str) -> String {
    let count: usize = conflicts.iter().map(|pallet| pallet.conflicts.len()).sum();
    let pallets: Vec<&str> = conflicts.iter().map(|pallet| pallet.pallet.as_str()).collect();
    let example = conflicts.first()
        .and_then(|pallet| pallet.conflicts.first().map(|conflict| (pallet, conflict)))
        .map(|(pallet, conflict)| format!("{}:{}", pallet.pallet, conflict.path))
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
            "Rerun lower with --keep @<pallet>:<path>=ours|theirs for each named path (\"theirs\" \
            also needs --confirm), e.g. --keep {}=theirs --confirm.",
            example
        ),
        serde_json::json!({ "pallets": conflicts, "lowered": { "pallet": pallet, "head": head } }),
    ).into()
}

/// Refuse (`confirmation_required`) when `specs`, already parsed, hold a "theirs" resolution,
/// listing each as it was spelled: run without --confirm, it would drop the local changes at or
/// beneath its path.
fn theirs_unconfirmed(specs: &[String]) -> Result<(), CoreError> {
    let theirs: Vec<&str> = specs.iter().filter_map(|spec| spec.strip_suffix("=theirs")).collect();

    if theirs.is_empty() {
        return Ok(());
    }

    crate::human!("Taking \"theirs\" would drop every local change at or beneath:");
    for path in &theirs {
        crate::human!("  {}", path);
    }

    Err(CoreError::refusal_with_details(
        RefusalCode::ConfirmationRequired,
        format!(
            "{} \"theirs\" resolution(s) would drop local changes nothing else holds; nothing was changed.",
            theirs.len()
        ),
        "Check that no local change there is still wanted, then run the same lower again with --confirm.",
        serde_json::json!({ "theirs": theirs }),
    ))
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

#[cfg(test)]
mod tests {
    use super::{listed_paths, parse_keeps, replay_failure_context, LISTED_PATHS};

    // Revert: swap the landed and unmoved wording, or compare the read-back with anything but the
    // replayed parcel → a failed write that landed reads as unmoved, or the reverse.
    #[test]
    fn a_failed_replay_says_where_the_read_back_found_the_local_head() {
        let landed = replay_failure_context("@notes", "replayed", Ok(Some("replayed".to_string())));
        assert_eq!(landed, "@notes was lifted to the remote, and its local head moved there, but the move may not be durable");

        for unmoved in [Ok(Some("local".to_string())), Ok(None)] {
            assert_eq!(
                replay_failure_context("@notes", "replayed", unmoved),
                "@notes was lifted to the remote, but this warehouse could not move its local head there"
            );
        }

        assert_eq!(
            replay_failure_context("@notes", "replayed", Err("unreadable".to_string())),
            "@notes was lifted to the remote, but whether its local head moved there is not known"
        );
    }

    #[test]
    fn a_conflict_message_names_the_first_paths_and_counts_the_rest() {
        let paths = |n: usize| (0..n).map(|i| format!("p{}", i)).collect::<Vec<_>>();
        assert_eq!(listed_paths(&paths(2)), "p0, p1");
        assert_eq!(listed_paths(&paths(LISTED_PATHS)), paths(LISTED_PATHS).join(", "));
        assert_eq!(
            listed_paths(&paths(LISTED_PATHS + 3)),
            format!("{}, and 3 more", paths(LISTED_PATHS).join(", "))
        );
    }

    fn refusal(spec: &str) -> String {
        parse_keeps(&[spec.to_string()]).expect_err("the keep must be refused")
    }

    #[test]
    fn a_bare_meta_pallet_name_is_refused_with_a_spelling_the_parser_accepts() {
        let subject = "a".repeat(64);
        let suggested = refusal(&format!("kv:{}/ci=ours", subject));
        let accepted = format!("@kv:{}/ci=ours", subject);
        assert!(suggested.contains(&format!("--keep {}", accepted)), "{}", suggested);
        assert!(parse_keeps(&[accepted]).is_ok());
    }

    #[test]
    fn no_at_spelling_is_suggested_where_the_parser_would_refuse_it_too() {
        for spec in ["main:foo=ours", "kv:alice/ci=mine", "kv:alice/ci=ours", ":x=ours"] {
            let message = refusal(spec);
            assert!(message.contains("is not of the form") && !message.contains("--keep @"),
                    "{}: {}", spec, message);
        }
    }
}

/// A lower's local changes outlive every way its head write can end other than landing: the
/// rollback a moved pallet forces, and heal's roll-forward or roll-back of an interrupted lower,
/// each rebuild staging from the marker's carried entries.
#[cfg(test)]
mod carry_tests {
    use forklift_core::util::operation_utils::{self, Operation, OperationKind, RecoveryOutcome};
    use forklift_core::util::pallet_utils::{self, BayPosition, RefPreconditions};
    use forklift_core::util::{file_utils, progress_utils::Progress, shift_utils, stocktake_utils};
    use crate::test_support::{self, Warehouse};

    /// The trees and heads of a bay on main at `start`, one parcel behind `remote`, which changes
    /// only `app.txt`.
    struct Behind { start_tree: String, start: String, remote_tree: String, remote: String }

    /// Check `start` out on main, then stage an edit and a removal and leave an edit unstaged,
    /// none of them to `app.txt`.
    fn a_bay_behind_with_local_changes() -> Behind {
        test_support::configure_operator();
        let files = |app: &'static [u8]| [("app.txt", app), ("bob.txt", b"bob v1\n" as &[u8]), ("staged.txt", b"staged v1\n"), ("gone.txt", b"gone\n")];
        let start_tree = test_support::store_files(&files(b"app v1\n"));
        let start = test_support::store_parcel(&start_tree, &[]);
        let remote_tree = test_support::store_files(&files(b"app v2\n"));
        let remote = test_support::store_parcel(&remote_tree, &[&start]);

        pallet_utils::update_pallet_head("main", None, &start, &RefPreconditions::NONE).unwrap();
        pallet_utils::write_position(&BayPosition::new("main", Some(start.clone()))).unwrap();
        test_support::check_out(&start_tree);

        std::fs::write("staged.txt", "staged mine\n").unwrap();
        std::fs::remove_file("gone.txt").unwrap();
        test_support::block_on(crate::commands::load::handle_command(".")).unwrap();
        std::fs::write("bob.txt", "bob mine\n").unwrap();
        // Old enough that a stat taken now proves whatever hash it is recorded with.
        std::fs::OpenOptions::new().write(true).open("bob.txt").unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60)).unwrap();

        Behind { start_tree, start, remote_tree, remote }
    }

    /// The bay's staged and unstaged changes against `tree`, as their JSON.
    fn changes(tree: &str) -> (serde_json::Value, serde_json::Value) {
        let staged = test_support::block_on(stocktake_utils::collect_staged_changes(Some(tree))).unwrap();
        let unstaged = test_support::block_on(stocktake_utils::collect_unstaged_changes()).unwrap();
        (serde_json::to_value(staged).unwrap(), serde_json::to_value(unstaged).unwrap())
    }

    /// Raise the lower's marker and write the remote's files, as a lower does before its head
    /// write; returns the anchor and the carried entries.
    fn lower_files(behind: &Behind) -> (file_utils::StatAnchor, Vec<shift_utils::CarriedEntry>) {
        let anchor = file_utils::take_stat_anchor().unwrap();
        let carried = test_support::block_on(shift_utils::collect_carried(Some(&behind.start_tree))).unwrap();
        assert_eq!(carried.len(), 3, "{:?}", carried);

        let mut operation = Operation::new(OperationKind::Lower, BayPosition::new("main", Some(behind.start.clone())),
                                           Some(behind.start_tree.clone()), BayPosition::new("main", Some(behind.remote.clone())));
        operation.target_tree = Some(behind.remote_tree.clone());
        operation.expected = Some(behind.start.clone());
        operation.carried = carried.clone();
        operation_utils::raise(&operation).unwrap();

        shift_utils::materialize_tree(Some(&behind.start_tree), &behind.remote_tree, "Lowering", anchor, &Progress::NONE, &carried).unwrap();
        (anchor, carried)
    }

    /// Another bay moves main past the lower's start before its head write.
    fn move_main_elsewhere(behind: &Behind) {
        let elsewhere = test_support::store_parcel(&test_support::store_tree("other.txt", b"other\n"), &[&behind.start]);
        pallet_utils::update_pallet_head("main", Some(&behind.start), &elsewhere, &RefPreconditions::NONE).unwrap();
    }

    // Revert: the rollback rebuilds without the carried entries → the staged changes are dropped.
    #[test]
    fn a_lower_rolled_back_by_a_moved_pallet_keeps_the_local_changes() {
        let warehouse = Warehouse::with_working_tree("lower-carry-rollback");
        let behind = a_bay_behind_with_local_changes();
        let before = changes(&behind.start_tree);

        let (anchor, carried) = lower_files(&behind);
        assert_eq!(changes(&behind.remote_tree), before, "the lower itself carries them");
        move_main_elsewhere(&behind);
        shift_utils::write_head_or_roll_back("main", Some(&behind.start), &behind.remote, &RefPreconditions::NONE,
                                             &behind.remote_tree, Some(&behind.start_tree), anchor, &carried)
            .expect_err("a pallet another bay moved must roll the lower back");

        assert!(test_support::file_holds(&warehouse.root, "app.txt", b"app v1\n"));
        assert_eq!(changes(&behind.start_tree), before);
    }

    // Reverts: heal's roll-forward, or its roll-back, rebuilds without the marker's carried
    // entries → it reports the carried files as not holding their content, and drops the staged
    // changes.
    #[test]
    fn heal_keeps_the_local_changes_of_an_interrupted_lower_rolled_either_way() {
        for (name, moved) in [("lower-carry-heal-forward", false), ("lower-carry-heal-back", true)] {
            let warehouse = Warehouse::with_working_tree(name);
            let behind = a_bay_behind_with_local_changes();
            let before = changes(&behind.start_tree);

            lower_files(&behind);
            operation_utils::forget_raised_for_tests();
            if moved {
                move_main_elsewhere(&behind);
            }
            let recovery = operation_utils::recover().unwrap().expect("the lower's marker stands");

            let (outcome, tree, app) = match moved {
                false => (RecoveryOutcome::RolledForward, &behind.remote_tree, b"app v2\n"),
                true => (RecoveryOutcome::RolledBack, &behind.start_tree, b"app v1\n"),
            };
            assert_eq!(recovery.outcome, outcome, "{}", name);
            assert!(recovery.unheld.is_empty(), "{}: {:?}", name, recovery.unheld);
            assert!(test_support::file_holds(&warehouse.root, "app.txt", app), "{}", name);
            assert_eq!(changes(tree), before, "{}", name);
            drop(warehouse);
        }
    }
}
