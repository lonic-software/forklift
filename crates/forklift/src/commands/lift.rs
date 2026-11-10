use serde::Serialize;
use forklift_core::util::progress_utils::Progress;
use forklift_core::util::remote_utils::{BatchLaneClosure, LiftResult, RemoteClient};
use forklift_core::util::{
    audit_utils, file_utils, haul_utils, merge_utils, meta_tree, object_utils, pallet_utils,
    remote_utils, take_utils,
};
use forklift_core::util::office_utils::OFFICE_PALLET_NAME;
use forklift_core::util::take_utils::{FetchAt, TakeTrust};
use crate::commands::consolidate::{self, MergeStatus};
use crate::commands::lower::{self, MetaPalletFailure};
use crate::commands::mirror::{self, LiftMirror};
use crate::events;
use crate::output::{self, CommandOutput};

/// The most times a single lift will auto-merge a diverged remote before giving up (a
/// safety bound on a pathological fleet where the pallet keeps moving under us).
const MAX_AUTO_MERGES: usize = 5;

/// Handle the lift command (git's "push"): upload the current pallet's new parcels to
/// the configured remote and move the remote's ref with a compare-and-swap.
///
/// When trust is established locally, the trust anchor and the office pallet are lifted
/// first — the key registry must be on the remote before the signed parcels arrive,
/// because the remote audits every ref update.
///
/// # Arguments
/// * `recreate` - Re-create a pallet the remote deleted, acknowledging its tombstone (refused
///                unless the local head descends from the tombstone's).
///
/// # Returns
/// * `Ok(())`      - If the lift completed (or there was nothing to lift).
/// * `Err(String)` - If no remote is configured, the remote is ahead, or a transfer
///                   failed.
pub async fn handle_command(recreate: bool) -> Result<(), String> {
    let progress = events::progress();
    let mut auto_merged = 0usize;
    // Each attempt dials a fresh client, so a lane an earlier attempt's fetch closed is carried here.
    let mut batch_lane: Option<BatchLaneClosure> = None;

    // Optimistic lift (§7.7): a diverged push whose merge is clean auto-lowers,
    // consolidates and retries, so a fleet stacking to one pallet stops serializing
    // through a human. True overlaps still stop with the diverged error.
    loop {
        // A partial warehouse lifts only to its origin, the one remote proved to hold what it
        // lacks; another could fail late at its closure check over objects never verified there.
        let client = RemoteClient::for_warehouse()?;
        let info = client.fetch_info().await?;

        // The upload lifts the pallet's ref; only the auto-merge below works from this bay's files.
        let position = pallet_utils::read_position()?;
        let pallet = position.pallet.clone();
        let local_head = pallet_utils::get_pallet_head(&pallet)?;

        // A lift onto a pallet the remote deleted is refused here, before the office or any meta
        // pallet uploads, unless it is acknowledged; the remote refuses an unacknowledged creation
        // of a deleted pallet all the same.
        let tombstone = match &local_head {
            Some(head) => remote_utils::acknowledged_tombstone(&info, &pallet, head, recreate)?,
            None => None,
        };

        let office_new_parcels = match remote_utils::push_local_trust(&client, &info).await? {
            Some(LiftResult::Lifted(stats)) => Some(stats.new_parcels),
            _ => None,
        };

        // The office carries the keys, so it lifts first (above); the manifest and any
        // other meta pallet follow, once the remote can verify their signatures. Each lifts
        // on its own: one that fails is reported after the rest, and the working pallet,
        // have gone through. @haul lifts last, after the working pallet's attempt, since a
        // merge it records may wait for that pallet's parcel.
        let mut meta = MetaLifts::default();

        for name in pallet_utils::list_meta_pallets()? {
            if name != OFFICE_PALLET_NAME && name != haul_utils::HAUL_PALLET_NAME {
                lift_meta(&client, &info, &name, None, &mut meta).await;
            }
        }

        let Some(head) = local_head else {
            lift_meta(&client, &info, haul_utils::HAUL_PALLET_NAME, None, &mut meta).await;
            return Err(with_meta_outcome(format!(
                "Pallet \"{}\" has nothing stacked yet; there is nothing to lift.",
                pallet
            ), &meta.resigned, &meta.failed));
        };

        let remote_head = info.pallets.get(&pallet).cloned();

        // If the remote diverged and the merge is clean, auto-merge and retry rather than
        // stopping. Overlapping changes fall through to the diverged error below.
        if let Some(remote) = &remote_head {
            if remote != &head && auto_merged < MAX_AUTO_MERGES {
                match try_auto_merge(&client, &pallet, position.base.as_deref(), &head, remote, &progress).await {
                    Ok(true) => {
                        auto_merged += 1;
                        batch_lane = batch_lane.or(client.batch_lane_closure());
                        continue;
                    }
                    Ok(false) => {}
                    Err(why) => {
                        lift_meta(&client, &info, haul_utils::HAUL_PALLET_NAME, None, &mut meta).await;
                        return Err(with_meta_outcome(why, &meta.resigned, &meta.failed));
                    }
                }
            }
        }

        let mut report = LiftReport {
            office_new_parcels,
            meta_pallets: Vec::new(),
            meta_resigned: Vec::new(),
            meta_failed: Vec::new(),
            auto_merged,
            pallet: pallet.clone(),
            up_to_date: false,
            head: head.clone(),
            new_parcels: 0,
            uploaded_objects: 0,
            uploaded_signatures: 0,
            withheld: Vec::new(),
            batch_lane: None,
            mirror: None,
        };

        let lifted = remote_utils::lift_pallet(
            &client, &pallet, &head, remote_head.as_deref(), tombstone.as_deref(), &info, &progress
        ).await;

        let landed = lifted.is_ok().then_some((pallet.as_str(), head.as_str()));
        lift_meta(&client, &info, haul_utils::HAUL_PALLET_NAME, landed, &mut meta).await;
        (report.meta_pallets, report.meta_resigned, report.meta_failed) = (meta.lifted, meta.resigned, meta.failed);
        report.batch_lane = batch_lane.or(client.batch_lane_closure());

        match lifted {
            Ok(LiftResult::UpToDate) => report.up_to_date = true,
            Ok(LiftResult::Lifted(stats)) => {
                if let Some(why) = &stats.unrecorded {
                    output::warn_held_head_unrecorded(why);
                }
                report.new_parcels = stats.new_parcels;
                report.uploaded_objects = stats.uploaded_objects;
                report.uploaded_signatures = stats.uploaded_signatures;
                report.withheld = lift_withheld(stats.withheld, &info);
            }
            Err(why) => {
                // The human lines name the re-signed cells; JSON has only the error to carry them.
                let resigned: &[String] = if output::is_json() {
                    &report.meta_resigned
                } else {
                    report.render_meta();
                    &[]
                };

                return Err(with_meta_outcome(why, resigned, &report.meta_failed));
            }
        }

        if !report.meta_failed.is_empty() {
            if !output::is_json() {
                report.render_human();
            }

            return Err(lower::meta_failure_error(&report.meta_failed));
        }

        // Only a lift that ends in its success result mirrors (§9.8), and its outcome never fails
        // the lift: a pallet left behind is caught up by the next lift or `forklift git mirror`.
        report.mirror = mirror::after_lift(&client, &pallet, &head).await;

        output::emit(&report);

        return Ok(());
    }
}

/// The withheld content a lift left out (`hashes`), each beside the statement the remote's
/// handshake (`info`) gives for it, if it lists it.
fn lift_withheld(hashes: Vec<String>, info: &forklift_core::model::remote::WarehouseInfo) -> Vec<LiftWithheld> {
    hashes.into_iter()
        .map(|hash| {
            let statement = info.withheld.iter().flatten()
                .find(|item| item.hash == hash)
                .map(|item| item.statement.clone());

            LiftWithheld { hash, statement }
        })
        .collect()
}

/// A lift's error, naming the cells a replay re-signed onto the remote before it — they landed,
/// so they are not first heard of on the next lift — and any meta pallet that failed.
fn with_meta_outcome(error: String, resigned: &[String], failed: &[MetaPalletFailure]) -> String {
    let error = if resigned.is_empty() {
        error
    } else {
        forklift_core::error::append_to_message(error, &format!(
            "Also: the remote now holds {} re-signed under your current key.",
            resigned.join(", ")
        ))
    };

    lower::with_meta_failures(error, failed)
}

/// What lifting the meta pallets did.
#[derive(Default)]
struct MetaLifts {
    lifted: Vec<String>,
    resigned: Vec<String>,
    failed: Vec<MetaPalletFailure>,
}

/// Lift one meta pallet other than the office, recording the outcome. `@haul` lifts after the
/// working pallet's attempt, `landed` when that lift went through.
///
/// Under trust, a pallet whose new parcels include unvouched ones — written before trust, outside
/// the anchor's boundary — would be refused as it is, so its local net change is replayed under
/// the current key instead and lifted ([`lower::replay`]): as a root parcel when the remote holds
/// no head, re-signing the operator's own cells.
async fn lift_meta(client: &RemoteClient,
                   info: &forklift_core::model::remote::WarehouseInfo,
                   name: &str,
                   landed: Option<(&str, &str)>,
                   meta: &mut MetaLifts) {
    let wire = pallet_utils::PalletRef::meta(name).to_wire();
    let remote_head = info.pallets.get(&wire).map(String::as_str);

    // A replay's refusals name their own ways out, which the drop hint would contradict: with no
    // remote head, even dropping every local change needs a key to sign the empty root.
    let mut replaying = false;

    let lifted = match needs_re_signing(name, remote_head) {
        Err(why) => Err(why),
        Ok(Some(rules)) => {
            replaying = true;
            let replayed = lower::replay(
                client, info, name, rules, remote_head, landed, "lift", &Default::default(), &mut meta.resigned,
            ).await;

            match replayed {
                Ok(lower::Reconciled::Replayed) => Ok(true),
                Ok(lower::Reconciled::Adopted) => Ok(false),
                // The remote head is an ancestor of the local one, so no path changed on both sides.
                Ok(lower::Reconciled::Conflicts(_)) => Err(format!(
                    "{} changed differently here and on the remote; run \"forklift lower\" first.", wire
                )),
                Err(why) => Err(why),
            }
        }
        Ok(None) if name == haul_utils::HAUL_PALLET_NAME => {
            match remote_utils::lift_haul_pallet(client, info, landed).await {
                None => return,
                Some(lift) => lift.result.map(|result| matches!(result, LiftResult::Lifted(_))),
            }
        }
        Ok(None) => match pallet_utils::get_meta_pallet_head(name) {
            Err(why) => Err(why),
            Ok(None) => return,
            // The handshake also says whether chunked files are taken, which never gates a meta pallet.
            Ok(Some(local_head)) => remote_utils::lift_pallet(client, &wire, &local_head, remote_head, None, info, &Progress::NONE)
                .await
                .map(|result| matches!(result, LiftResult::Lifted(_))),
        },
    };

    match lifted {
        Ok(true) => meta.lifted.push(wire),
        Ok(false) => {}
        Err(why) => {
            let error = if replaying { why } else { with_drop_hint(why, info, &wire) };
            meta.failed.push(MetaPalletFailure::new(wire, error));
        }
    }
}

/// The rules to replay a meta pallet's lift by, when it must be re-signed to lift: trust is held
/// here, the local head is ahead of the remote's (or the remote holds none), and a new parcel is
/// unvouched. `None` when it lifts as it is — including when it has diverged, which the lift
/// itself refuses with the way out.
fn needs_re_signing(name: &str, remote_head: Option<&str>) -> Result<Option<&'static dyn meta_tree::PalletRules>, String> {
    let (Some(local), Some(rules)) = (pallet_utils::get_meta_pallet_head(name)?, meta_tree::meta_rules(name)) else {
        return Ok(None);
    };

    let Some((anchor, _)) = audit_utils::local_meta_trust()? else { return Ok(None) };

    if let Some(remote_head) = remote_head {
        if !remote_utils::is_local_ahead(remote_head, &local)? {
            return Ok(None);
        }
    }

    match meta_tree::holds_unvouched_parcels(name, &anchor.boundary, remote_head, &local)? {
        true => Ok(Some(rules)),
        false => Ok(None),
    }
}

/// A meta pallet's lift failure, naming the way out when the remote refuses a local change in it
/// (a tag whose tagger was demoted before lifting it): lowering with "theirs" at its path drops
/// the change. Only a failure the local office would refuse too gets the hint — a lost race or a
/// transport error is not one, and a diverged pallet's failure already says to lower.
fn with_drop_hint(error: String, info: &forklift_core::model::remote::WarehouseInfo, wire: &str) -> String {
    let name = wire.strip_prefix(pallet_utils::META_QUALIFIER).unwrap_or(wire);

    if !remote_utils::lift_refused_by_content(name, info.pallets.get(wire).map(String::as_str)) {
        return error;
    }

    forklift_core::error::append_to_message(error, &format!(
        "If the remote refuses a local change, \"forklift lower --keep {}:<path>=theirs --confirm\" drops it.", wire
    ))
}

/// If the remote head has genuinely diverged from ours and a clean auto-merge is possible,
/// perform it (fetch their work, consolidate, stack the merge parcel) and report `true` so
/// the caller retries the lift. Report `false` when the remote is not diverged (it is our
/// ancestor or descendant — the ordinary lift handles those), the warehouse is dirty, or
/// the merge would conflict — so the caller falls through to the ordinary lift, which
/// reports the diverged error for a true overlap.
async fn try_auto_merge(client: &RemoteClient,
                        pallet: &str,
                        base: Option<&str>,
                        head: &str,
                        remote_head: &str,
                        progress: &Progress) -> Result<bool, String> {
    // Their work must be local before we can compare ancestry or merge it. Path-pruned in a
    // sparse workspace (a full fetch scope makes this the whole closure, as before), so an
    // optimistic auto-merge stays as sparse as the workspace it runs in. A cache fetches only
    // the parcels here, and their head's content below, once their history is verified.
    if !file_utils::does_object_exist(remote_head)? {
        remote_utils::fetch_history_scoped(client, remote_head, pallet, progress).await?;
    }

    let diverged = !merge_utils::is_ancestor(remote_head, head)?   // we do not already contain them
        && !merge_utils::is_ancestor(head, remote_head)?;          // they do not already contain us

    if !diverged {
        return Ok(false);
    }

    // The merge's own ref write would refuse a head naming redacted content too, but in the
    // merge's words, so lift's check comes first.
    remote_utils::refuse_redacted_head(pallet, head, Some(remote_head))?;

    // The merge materializes into this bay and moves the pallet from the tree it holds, so a bay
    // another bay or process left behind its pallet is told so instead of attempting it.
    if base != Some(head) {
        return Err(pallet_utils::behind_refusal(
            pallet, base, Some(head), "lifted: the remote diverged, and only a bay at its pallet's head can merge it in",
        ).into());
    }

    // Their work becomes a parent of the merge parcel, so it is verified before anything is
    // merged. The merge's own write is conditioned by `stack`, which judges it again.
    take_utils::verify_take(Some(client), &TakeTrust::local()?, remote_head, pallet, FetchAt::Warehouse).await?;

    // The merge reads their head's content, and so does the diverged refusal's next step when no
    // merge happens here. In a cache it may be absent even when the parcel was already here: a
    // fetch that did not check that head out recorded its root instead.
    remote_utils::fetch_head_content(client, remote_head, progress).await?;

    // Merging materializes into the working directory, so only a clean warehouse qualifies.
    let our_tree = object_utils::load_parcel(head)?.tree_hash;

    // Anchored before the cleanliness check, so a file edited after it is never trusted.
    let anchor = file_utils::take_stat_anchor()?;

    if !consolidate::is_warehouse_clean(&our_tree).await? {
        return Ok(false);
    }

    // Unlike `consolidate`, this path has no fast-forward branch to exempt — reaching here
    // already means a genuine, unresolved divergence (checked above), so every path past this
    // point is the same real merge, and the incomplete-load guard lives inside
    // `merge_head_into_current` itself.
    match consolidate::merge_head_into_current(pallet, head, remote_head, lower::REMOTE_LABEL, Some(pallet), false, anchor).await? {
        MergeStatus::Merged(_) => Ok(true),
        MergeStatus::Conflicts(_) => Ok(false),
    }
}

/// The result of a lift: the pallet's outcome, plus the office lift when trust
/// required its keys to reach the remote first.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct LiftReport {
    /// The parcels the office lift uploaded, when one happened.
    #[serde(skip_serializing_if = "Option::is_none")]
    office_new_parcels: Option<usize>,

    /// The meta pallets (e.g. `@manifest`) lifted with new parcels.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    meta_pallets: Vec<String>,

    /// The cells (`<pallet> <path>`) a meta pallet's lift re-signed under the current key: written
    /// here before trust was established, and replayed under it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    meta_resigned: Vec<String>,

    /// The meta pallets that failed to lift. Never in a successful report: a failure makes the
    /// command fail, after the rest have gone through.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    meta_failed: Vec<MetaPalletFailure>,

    /// How many times the lift auto-merged a diverged remote before it went through
    /// (optimistic lift).
    #[serde(skip_serializing_if = "is_zero")]
    auto_merged: usize,

    pallet: String,

    /// Whether the remote already had the local head.
    up_to_date: bool,

    head: String,
    new_parcels: usize,
    uploaded_objects: usize,
    uploaded_signatures: usize,

    /// The content the lifted history names that the remote's host withholds, so it was not
    /// uploaded: the remote takes history naming it without it. Absent when none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    withheld: Vec<LiftWithheld>,

    /// Why the remote's batch download stopped being used while this lift fetched (an auto-merge),
    /// when it did. Absent when it stayed open.
    #[serde(skip_serializing_if = "Option::is_none")]
    batch_lane: Option<BatchLaneClosure>,

    /// What the git mirror did with the lifted pallet and the tags on its history (`mirror.url`,
    /// `mirror.auto`). Absent when no mirror applies to it. A mirror failure is reported here and
    /// never fails the lift.
    #[serde(skip_serializing_if = "Option::is_none")]
    mirror: Option<LiftMirror>,
}

/// Content the remote withholds, as a lift reports it.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct LiftWithheld {
    /// The withheld object's hash.
    hash: String,

    /// Why the host withholds it, in the host's words. Absent when the handshake did not list it
    /// and only the upload negotiation answered it withheld.
    #[serde(skip_serializing_if = "Option::is_none")]
    statement: Option<String>,
}

/// Whether a count is zero (for skipping it in the JSON envelope).
fn is_zero(count: &usize) -> bool {
    *count == 0
}

impl LiftReport {
    /// The office and meta pallet lines: what went through before the working pallet.
    fn render_meta(&self) {
        if let Some(new_parcels) = self.office_new_parcels {
            println!("Lifted the office pallet: {} new parcel(s).", new_parcels);
        }

        for pallet in &self.meta_pallets {
            println!("Lifted the {} pallet.", pallet);
        }

        for cell in &self.meta_resigned {
            println!("Re-signed {} under your current key.", cell);
        }
    }
}

impl CommandOutput for LiftReport {
    fn render_human(&self) {
        self.render_meta();

        if self.auto_merged > 0 {
            println!(
                "The remote had moved; auto-merged it {} time(s) and retried (optimistic lift).",
                self.auto_merged
            );
        }

        if self.up_to_date {
            println!("Already up to date: the remote has pallet \"{}\" at {}.", self.pallet, self.head);
        } else {
            println!(
                "Lifted pallet \"{}\" to {}: {} new parcel(s), {} object(s) and {} \
                signature(s) uploaded.",
                self.pallet, self.head, self.new_parcels, self.uploaded_objects,
                self.uploaded_signatures
            );
        }

        if !self.withheld.is_empty() {
            println!(
                "The remote's host withholds {} object(s) this history names, so they were not uploaded; \
                the remote takes the history without them:",
                self.withheld.len()
            );

            for item in &self.withheld {
                match &item.statement {
                    Some(statement) => println!("  {} ({})", item.hash, statement),
                    None => println!("  {}", item.hash),
                }
            }
        }

        output::print_batch_lane(self.batch_lane);

        if let Some(mirror) = &self.mirror {
            mirror.render();
        }
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<LiftReport>(),
    ]
}
