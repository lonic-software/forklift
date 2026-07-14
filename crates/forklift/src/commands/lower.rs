use serde::Serialize;
use forklift_core::util::remote_utils::RemoteClient;
use forklift_core::util::stocktake_utils::ChangeKind;
use forklift_core::util::{
    config_utils, haul_utils, manifest_utils, merge_utils, object_utils, pallet_utils, remote_utils,
    scope_utils, shift_utils, stocktake_utils,
};
use crate::commands::office;
use crate::output::{self, CommandOutput};

/// Handle the lower command (git's "pull"): fetch the remote's new parcels for the
/// current pallet and fast-forward to them — working directory and inventory included.
///
/// The remote's trust state is synchronized first: on first contact with a trusted
/// remote its anchor is adopted (a one-way door, exactly like "office enroll"), and the
/// office pallet fast-forwards so newly admitted operators can be verified locally.
///
/// A diverged pallet is never merged implicitly: lower reports it and the operator
/// consolidates deliberately.
///
/// # Returns
/// * `Ok(())`      - If the pallet is now at the remote head (or was already).
/// * `Err(String)` - If no remote is configured, the warehouse is dirty, the histories
///                   diverged, or a transfer failed.
pub async fn handle_command() -> Result<(), String> {
    let client = RemoteClient::from_config()?;
    let info = client.fetch_info().await?;

    let trust = remote_utils::adopt_remote_trust(&client, &info).await?;

    // Meta pallets (the manifest, …) sync alongside trust — fast-forwarded, never
    // materialized. A diverged manifest merges cleanly (its records are independent).
    let (meta_adopted, meta_merged) = adopt_and_merge_meta(&client, &info).await?;

    let pallet = pallet_utils::get_current_pallet_name()?;

    let Some(remote_head) = info.pallets.get(&pallet) else {
        return Err(format!(
            "The remote has no pallet \"{}\" (it has: {}).",
            pallet,
            info.pallets.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    };

    let local_head = pallet_utils::get_pallet_head(&pallet)?;

    let mut report = LowerReport {
        adopted_anchor: trust.adopted_anchor,
        office_moved: trust.office_moved,
        meta_adopted,
        meta_merged,
        pallet: pallet.clone(),
        outcome: LowerOutcome::UpToDate,
        head: remote_head.clone(),
        fetched_objects: 0,
        fetched_signatures: 0,
    };

    if local_head.as_deref() == Some(remote_head.as_str()) {
        output::emit("lower", &report);
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
    let stats = remote_utils::fetch_history_scoped(&client, remote_head, &fetch_scope).await?;
    report.fetched_objects = stats.fetched_objects;
    report.fetched_signatures = stats.fetched_signatures;

    if let Some(local) = &local_head {
        if merge_utils::is_ancestor(remote_head, local)? {
            report.outcome = LowerOutcome::Ahead;
            output::emit("lower", &report);
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

    shift_utils::materialize_tree(local_tree.as_deref(), &remote_tree, "Lowering")?;
    pallet_utils::set_pallet_head(&pallet, remote_head)?;

    report.outcome = LowerOutcome::Lowered;
    output::emit("lower", &report);

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

/// The result of a lower: the trust sync that ran first, then the pallet's outcome.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct LowerReport {
    /// Whether the remote's trust anchor was adopted on first contact.
    adopted_anchor: bool,

    /// Whether the office pallet moved with the remote.
    office_moved: bool,

    /// Meta pallets (e.g. `@manifest`) fast-forwarded or adopted from the remote.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    meta_adopted: Vec<String>,

    /// Meta pallets whose diverged history was merged with the remote's.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    meta_merged: Vec<String>,

    pallet: String,
    outcome: LowerOutcome,
    head: String,
    fetched_objects: usize,
    fetched_signatures: usize,
}

impl CommandOutput for LowerReport {
    fn render_human(&self) {
        if self.adopted_anchor {
            println!("Adopted the remote's trust anchor; every parcel is signed from now on.");
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

        match self.outcome {
            LowerOutcome::UpToDate => {
                println!("Already up to date: pallet \"{}\" is at the remote head.", self.pallet);
            }
            LowerOutcome::Ahead => {
                println!(
                    "The local pallet \"{}\" is ahead of the remote; \"lift\" to publish it.",
                    self.pallet
                );
            }
            LowerOutcome::Lowered => {
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

/// Adopt the remote's non-office meta pallets and merge any that diverged. Returns the
/// wire refs of the pallets fast-forwarded/adopted and of those merged. The manifest and
/// the haul log both have a merge policy: their entries are independent signed events, so
/// divergence is a conflict-free two-parent join (the union of records). A diverged office
/// would need a real merge design and is refused here. The join parcel is signed by the
/// current operator, so lowering a diverged meta pallet requires an enrolled key.
async fn adopt_and_merge_meta(client: &RemoteClient,
                              info: &forklift_core::model::remote::WarehouseInfo)
                              -> Result<(Vec<String>, Vec<String>), String> {
    let result = remote_utils::adopt_meta_pallets(client, info).await?;
    let mut merged = Vec::new();

    for (name, remote_head) in &result.diverged {
        let wire = pallet_utils::PalletRef::meta(name).to_wire();

        let actor = config_utils::get_operator()?;
        let (_state, signing_key_id, _actor_id) = office::require_signing_actor(&actor)?;

        if name == manifest_utils::MANIFEST_PALLET_NAME {
            manifest_utils::merge_manifest(
                remote_head, &actor, "Merged the remote manifest.".to_string(), &signing_key_id
            )?;
        } else if name == haul_utils::HAUL_PALLET_NAME {
            haul_utils::merge_hauls(
                remote_head, &actor, "Merged the remote hauls.".to_string(), &signing_key_id
            )?;
        } else {
            return Err(format!(
                "The {} pallet diverged from the remote and has no automatic merge; \
                resolve it by hand.",
                wire
            ));
        }

        merged.push(wire);
    }

    Ok((result.adopted, merged))
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("LowerReport", schemars::schema_for!(LowerReport)),
    ]
}
