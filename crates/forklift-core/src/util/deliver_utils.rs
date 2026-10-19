//! Deliver a draft pallet's checkpoint trail as one clean signed parcel (§7.3).
//!
//! Agents checkpoint constantly; humans want one reviewed parcel. `deliver` squashes the
//! draft pallet's net change into a single signed parcel on the target — a plain
//! single-parent parcel carrying the draft head's tree, so the checkpoints stay *out* of
//! the target's history — and records the trail as a signed **delivery** manifest entry
//! referencing the kept checkpoints. The clean story stays clean; the "what the agent
//! tried, in what order" evidence stays complete and discoverable.
//!
//! Because the delivered tree equals the draft head's tree, the working directory does not
//! change: delivering only moves refs, never materializes.

use std::collections::HashSet;
use chrono::Utc;
use crate::builder::object::loose_object_builder::LooseObjectBuilder;
use crate::enums::parcel_action_type::ParcelActionType;
use crate::model::operator::Operator;
use crate::model::parcel::Parcel;
use crate::model::parcel_action::ParcelAction;
use crate::util::manifest_utils::{Delivery, ManifestEntry, ManifestKind};
use crate::util::meta_tree::MetaSigner;
use crate::util::office_utils::Role;
use crate::util::{audit_utils, config_utils, manifest_utils, merge_utils, object_utils,
                  office_utils, pallet_utils, sign_utils, stack_utils};

/// What a delivery produced.
pub struct DeliverOutcome {
    /// The clean squashed parcel now on the target pallet.
    pub delivered: String,

    /// The target pallet the parcel was delivered to (now the current pallet).
    pub target: String,

    /// The draft pallet the trail came from (kept, so the trail stays browsable).
    pub source: String,

    /// The trail tip — the draft head that was squashed.
    pub trail_head: String,

    /// How many checkpoint parcels the delivery squashed.
    pub checkpoints: usize,

    /// The manifest head that holds the delivery record.
    pub manifest_head: String,
}

/// Deliver the current (draft) pallet's trail onto `target` as one clean signed parcel,
/// and record the trail as a signed delivery manifest entry.
///
/// # Arguments
/// * `target`      - The pallet to deliver onto (a user pallet; may be unborn).
/// * `description` - The delivered parcel's message (`None` uses a default).
///
/// # Returns
/// * `Ok(DeliverOutcome)` - The delivered parcel and trail details.
/// * `Err(String)`        - If there is nothing to deliver, trust is not established, or
///                          an object could not be built, stored or signed.
pub fn deliver(target: &str, description: Option<String>) -> Result<DeliverOutcome, String> {
    pallet_utils::validate_pallet_name(target)?;

    let position = pallet_utils::read_position()?;
    let source = position.pallet.clone();

    if target == source {
        return Err("A pallet cannot be delivered to itself.".to_string());
    }

    let operator = config_utils::get_operator()?;

    // Delivery records the trail as signed post-metadata, so it needs an enrolled key —
    // there is no unsigned delivery (the evidence is the point).
    let signing_key_id = stack_utils::resolve_signing_key(&operator)?.ok_or(
        "Deliver records the checkpoint trail as signed post-metadata, so it needs an \
        enrolled key. Establish trust with \"office enroll\" first.".to_string()
    )?;

    // The delivery record is judged against the verified office. A reader may write no pallet,
    // so is refused before anything is written.
    let office = audit_utils::verified_local_office()?;

    if office.find_user(&operator.identifier).is_some_and(|user| matches!(user.role, Role::Reader)) {
        return Err(format!(
            "\"{}\" is a reader in this office, and a reader cannot deliver.", operator.identifier
        ));
    }

    // The delivered parcel carries the tree this bay holds, and the bay ends up on it. A bay behind
    // its draft pallet would deliver a tree without the draft's latest parcels, so it catches up
    // first.
    position.require_current("delivered")?;

    let draft_head = position.base.clone().ok_or(format!(
        "Pallet \"{}\" has nothing stacked yet; there is nothing to deliver.", source
    ))?;

    let draft_parcel = object_utils::load_parcel(&draft_head)?;

    // A hand-made ref could point at an office parcel; its tracked-metadata namespace must
    // never be delivered onto a working pallet.
    office_utils::ensure_not_metadata_tree(&draft_parcel.tree_hash)?;

    let target_head = pallet_utils::get_pallet_head(target)?;

    // Nothing to deliver when the target already has the draft's work: either the draft is
    // an ancestor of the target (normally merged), or the target head already carries the
    // draft's exact tree — which is what a *prior* delivery of this draft leaves, since a
    // delivery is a squash the draft is not an ancestor of.
    if let Some(head) = &target_head {
        let target_tree = object_utils::load_parcel(head)?.tree_hash;

        if head == &draft_head
            || target_tree == draft_parcel.tree_hash
            || merge_utils::is_ancestor(&draft_head, head)? {
            return Err(format!(
                "\"{}\" is already delivered to \"{}\"; there is nothing to deliver.",
                source, target
            ));
        }
    }

    // The trail: everything reachable from the draft head but not from the target head —
    // the checkpoints since the two diverged.
    let draft_reachable = audit_utils::collect_reachable(&[draft_head.clone()])?;
    let target_reachable = match &target_head {
        Some(head) => audit_utils::collect_reachable(&[head.clone()])?,
        None => HashSet::new(),
    };
    let trail: Vec<String> = draft_reachable.into_iter()
        .filter(|hash| !target_reachable.contains(hash))
        .collect();
    let checkpoints = trail.len();

    // The delivered parcel preserves the trail's authors (the convention for re-apply
    // operations) and records the deliverer as the stacker.
    let authors = collect_trail_authors(&trail, &operator)?;
    let timestamp = Utc::now();

    let mut actions: Vec<ParcelAction> = authors.into_iter()
        .map(|op| ParcelAction { operator: op, action: ParcelActionType::Author, description: None, timestamp })
        .collect();
    actions.push(ParcelAction {
        operator: operator.clone(),
        action: ParcelActionType::Stack,
        description: None,
        timestamp,
    });

    let parcel = Parcel {
        tree_hash: draft_parcel.tree_hash.clone(),
        parents: target_head.iter().cloned().collect(),
        actions,
        description: Some(description.clone().unwrap_or_else(|| format!("Delivered \"{}\".", source))),
    };

    let mut object = LooseObjectBuilder::build_parcel(&parcel);
    object.store()?;

    let signature = sign_utils::sign_parcel_hash(&signing_key_id, &object.hash)?;
    sign_utils::store_parcel_signature(&object.hash, &signature)?;

    // Record the delivery as signed post-metadata on the clean parcel, referencing the
    // kept trail so "what the agent tried" stays discoverable. It is recorded before the target
    // moves, so a refused record leaves the target where it was. The converse — the ref write
    // failing after the record stood — leaves a record about a parcel no pallet holds, and a
    // retry records a second squash. That is accepted: a stray record misleads no reader about
    // a pallet, whereas a moved target with no record would lose the trail's evidence.
    let entry = ManifestEntry {
        subject: object.hash.clone(),
        kind: ManifestKind::Delivery,
        recorded_at: None,
        body: description.unwrap_or_default(),
        provenance: None,
        delivery: Some(Delivery {
            source: source.clone(),
            trail_head: draft_head.clone(),
            checkpoints: checkpoints as i64,
        }),
    };

    let manifest_description = format!(
        "Delivered {} checkpoint(s) from \"{}\" as parcel {}.",
        checkpoints, source, object.hash
    );
    let signer = MetaSigner::local(&signing_key_id);
    let recorded = manifest_utils::record_entry(&entry, &office, &operator, &manifest_description, &signer)?;
    let manifest_head = match recorded {
        Some(head) => head,
        // The record already stood: the head that holds it.
        None => pallet_utils::get_meta_pallet_head(manifest_utils::MANIFEST_PALLET_NAME)?.unwrap_or_default(),
    };

    pallet_utils::update_pallet_head(target, target_head.as_deref(), &object.hash)?;

    // The delivered tree equals the draft's, so switching the current pallet needs no
    // materialization — the working directory already matches, and the bay stands on the
    // delivered parcel.
    pallet_utils::record_landed(target, &object.hash)?;

    Ok(DeliverOutcome {
        delivered: object.hash,
        target: target.to_string(),
        source,
        trail_head: draft_head,
        checkpoints,
        manifest_head,
    })
}

/// The distinct authors of the trail parcels, in first-seen order (by identifier). Falls
/// back to the deliverer when the trail records no authors.
fn collect_trail_authors(trail: &[String], deliverer: &Operator) -> Result<Vec<Operator>, String> {
    let mut authors: Vec<Operator> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // A stable order: oldest checkpoints first (the trail set is unordered, so sort).
    let mut ordered = trail.to_vec();
    ordered.sort();

    for hash in ordered {
        let parcel = object_utils::load_parcel(&hash)?;

        for action in parcel.actions {
            if matches!(action.action, ParcelActionType::Author)
                && seen.insert(action.operator.identifier.clone()) {
                authors.push(action.operator);
            }
        }
    }

    if authors.is_empty() {
        authors.push(deliverer.clone());
    }

    Ok(authors)
}
