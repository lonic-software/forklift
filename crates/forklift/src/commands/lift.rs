use serde::Serialize;
use forklift_core::error::CoreError;
use forklift_core::enums::config_scope::ConfigScope;
use forklift_core::util::remote_utils::{LiftResult, RemoteClient};
use forklift_core::util::{
    config_utils, file_utils, merge_utils, object_utils, pallet_utils, remote_utils, scope_utils,
};
use crate::commands::consolidate::{self, MergeStatus};
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
/// # Returns
/// * `Ok(())`      - If the lift completed (or there was nothing to lift).
/// * `Err(String)` - If no remote is configured, the remote is ahead, or a transfer
///                   failed.
pub async fn handle_command() -> Result<(), String> {
    ensure_origin_remote()?;

    let mut auto_merged = 0usize;

    // Optimistic lift (§7.7): a diverged push whose merge is clean auto-lowers,
    // consolidates and retries, so a fleet stacking to one pallet stops serializing
    // through a human. True overlaps still stop with the diverged error.
    loop {
        let client = RemoteClient::from_config()?;
        let info = client.fetch_info().await?;

        let office_new_parcels = match remote_utils::push_local_trust(&client, &info).await? {
            Some(LiftResult::Lifted(stats)) => Some(stats.new_parcels),
            _ => None,
        };

        // The office carries the keys, so it lifts first (above); the manifest and any
        // other meta pallet follow, once the remote can verify their signatures.
        let meta_pallets: Vec<String> = remote_utils::lift_meta_pallets(&client, &info).await?
            .into_iter()
            .filter(|lift| matches!(lift.result, LiftResult::Lifted(_)))
            .map(|lift| lift.pallet)
            .collect();

        let pallet = pallet_utils::get_current_pallet_name()?;

        let Some(head) = pallet_utils::get_pallet_head(&pallet)? else {
            return Err(format!(
                "Pallet \"{}\" has nothing stacked yet; there is nothing to lift.",
                pallet
            ));
        };

        let remote_head = info.pallets.get(&pallet).cloned();

        // If the remote diverged and the merge is clean, auto-merge and retry rather than
        // stopping. Overlapping changes fall through to the diverged error below.
        if let Some(remote) = &remote_head {
            if remote != &head && auto_merged < MAX_AUTO_MERGES && try_auto_merge(&client, &pallet, &head, remote).await? {
                auto_merged += 1;
                continue;
            }
        }

        let mut report = LiftReport {
            office_new_parcels,
            meta_pallets,
            auto_merged,
            pallet: pallet.clone(),
            up_to_date: false,
            head: head.clone(),
            new_parcels: 0,
            uploaded_objects: 0,
            uploaded_signatures: 0,
        };

        match remote_utils::lift_pallet(&client, &pallet, &head, remote_head.as_deref(), info.chunking).await? {
            LiftResult::UpToDate => report.up_to_date = true,
            LiftResult::Lifted(stats) => {
                report.new_parcels = stats.new_parcels;
                report.uploaded_objects = stats.uploaded_objects;
                report.uploaded_signatures = stats.uploaded_signatures;
            }
        }

        output::emit("lift", &report);

        return Ok(());
    }
}

/// Refuse a lift from a sparse workspace to a remote other than the one it fetched against.
/// A sparse warehouse only ever proved its out-of-scope closure present on its origin; a
/// different remote may lack objects it never verified there, so the lift's closure check would
/// fail late and confusingly. Refusing up front, with the origin named, is the clearer failure.
/// A no-op for a full (non-sparse) warehouse, which holds the whole closure and can lift
/// anywhere, and for a sparse warehouse still pointed at its origin.
fn ensure_origin_remote() -> Result<(), CoreError> {
    if !scope_utils::is_warehouse_sparse()? {
        return Ok(());
    }

    let Some(origin) = config_utils::get_scoped_value(config_utils::KEY_REMOTE_ORIGIN, ConfigScope::Warehouse)? else {
        // A sparse warehouse with no recorded origin predates this guard; leave it to the remote's
        // closure check rather than invent an origin.
        return Ok(());
    };

    let Some((configured, _scope)) = config_utils::get_effective_value(config_utils::KEY_REMOTE_URL)? else {
        // No remote is configured at all — a different, unrelated problem. Leave it to
        // `RemoteClient::from_config` (called right after this guard) to report that plainly,
        // rather than treating "unset" as "configured to the empty string" and masking it
        // behind a confusing "lifting to \"\"" origin refusal.
        return Ok(());
    };

    if configured != origin {
        return Err(scope_utils::non_origin_lift_refusal(&origin, &configured));
    }

    Ok(())
}

/// If the remote head has genuinely diverged from ours and a clean auto-merge is possible,
/// perform it (fetch their work, consolidate, stack the merge parcel) and report `true` so
/// the caller retries the lift. Report `false` when the remote is not diverged (it is our
/// ancestor or descendant — the ordinary lift handles those), the warehouse is dirty, or
/// the merge would conflict — so the caller falls through to the ordinary lift, which
/// reports the diverged error for a true overlap.
async fn try_auto_merge(client: &RemoteClient,
                        pallet: &str,
                        head: &str,
                        remote_head: &str) -> Result<bool, String> {
    // Their work must be local before we can compare ancestry or merge it. Path-pruned in a
    // sparse workspace (a full fetch scope makes this the whole closure, as before), so an
    // optimistic auto-merge stays as sparse as the workspace it runs in.
    if !file_utils::does_object_exist(remote_head)? {
        let fetch_scope = scope_utils::read_fetch_scope()?;
        remote_utils::fetch_history_scoped(client, remote_head, &fetch_scope).await?;
    }

    let diverged = !merge_utils::is_ancestor(remote_head, head)?   // we do not already contain them
        && !merge_utils::is_ancestor(head, remote_head)?;          // they do not already contain us

    if !diverged {
        return Ok(false);
    }

    // Merging materializes into the working directory, so only a clean warehouse qualifies.
    let our_tree = object_utils::load_parcel(head)?.tree_hash;

    if !consolidate::is_warehouse_clean(&our_tree).await? {
        return Ok(false);
    }

    match consolidate::merge_head_into_current(pallet, head, remote_head, "remote", false).await? {
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

    /// How many times the lift auto-merged a diverged remote before it went through
    /// (optimistic lift, §7.7).
    #[serde(skip_serializing_if = "is_zero")]
    auto_merged: usize,

    pallet: String,

    /// Whether the remote already had the local head.
    up_to_date: bool,

    head: String,
    new_parcels: usize,
    uploaded_objects: usize,
    uploaded_signatures: usize,
}

/// Whether a count is zero (for skipping it in the JSON envelope).
fn is_zero(count: &usize) -> bool {
    *count == 0
}

impl CommandOutput for LiftReport {
    fn render_human(&self) {
        if let Some(new_parcels) = self.office_new_parcels {
            println!("Lifted the office pallet: {} new parcel(s).", new_parcels);
        }

        for pallet in &self.meta_pallets {
            println!("Lifted the {} pallet.", pallet);
        }

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
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("LiftReport", schemars::schema_for!(LiftReport)),
    ]
}
