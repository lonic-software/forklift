//! Repair a head no client can check out: a parcel stacked on it that removes every name opening the
//! warehouse's metadata folder.
//!
//! A served user pallet's ref only moves forward, so the way back from a head holding such a name
//! runs forward too. The repair parcel's only parent is the wedged head, so a lift of it, or of a
//! consolidation built on it, fast-forwards the remote, and the remote's gate accepts it: every
//! subtree that held a bad name differs from the head's and is checked, and every subtree the two
//! share held none.

use std::path::Path;
use chrono::Utc;
use crate::builder::object::loose_object_builder::LooseObjectBuilder;
use crate::enums::dir_entry_type::DirEntryType;
use crate::enums::parcel_action_type::ParcelActionType;
use crate::model::parcel::Parcel;
use crate::model::parcel_action::ParcelAction;
use crate::model::tree_item::TreeItem;
use crate::util::stack_utils;
use crate::util::{audit_utils, config_utils, object_utils, office_utils, sign_utils};

/// A repair parcel, stored (and signed when trust is established) but on no pallet yet.
pub struct Repair {
    /// The repair parcel, whose only parent is the head it repairs.
    pub parcel: String,

    /// Its root tree.
    pub tree: String,

    /// The paths it removed, in name order.
    pub removed: Vec<String>,
}

/// Build the repair of `head`: its tree minus every entry, at any depth, whose name opens the
/// metadata folder by the rule a checkout refuses with ([`office_utils::opens_metadata_folder`]),
/// with every directory those removals leave empty dropped too, stacked as a parcel on `head` and
/// signed as `stack` would sign it. The whole tree is read, so its content must be present. Its head
/// write carries the preconditions of the take that verified `head`, which a signing choice made
/// since cannot loosen: unsigned is chosen only with no anchor held, which the take's write
/// requires too, and signed only under the office it verified against.
///
/// # Returns
/// * `Ok(Some(Repair))` - The repair parcel.
/// * `Ok(None)`         - If the head holds no such name, so a checkout of it needs no repair.
/// * `Err(String)`      - If a tree could not be read, the volume could not say for a name, the
///   operator cannot sign where trust is established, or an object could not be stored or signed.
pub fn repair_head(head: &str) -> Result<Option<Repair>, String> {
    let tree_hash = object_utils::load_parcel(head)?.tree_hash;
    let mut removed = Vec::new();

    let tree = match strip(&tree_hash, "", &mut removed)? {
        Stripped::Unchanged => return Ok(None),
        Stripped::Changed(hash) => hash,
        Stripped::Emptied => store_tree(&TreeItem::new(String::new(), String::new(), DirEntryType::Tree))?,
    };
    removed.sort();

    let operator = config_utils::get_operator()?;
    let signing = stack_utils::resolve_signing_key(&operator)?;
    let timestamp = Utc::now();

    let parcel = Parcel {
        tree_hash: tree.clone(),
        parents: vec![head.to_string()],
        actions: [ParcelActionType::Author, ParcelActionType::Stack].into_iter()
            .map(|action| ParcelAction { operator: operator.clone(), action, description: None, timestamp })
            .collect(),
        description: Some(format!(
            "Repair: remove {}, which no client can check out.",
            removed.iter().map(|path| format!("\"{}\"", path)).collect::<Vec<_>>().join(", ")
        )),
    };

    let mut object = LooseObjectBuilder::build_parcel(&parcel);
    object.store()?;

    if let Some(key_id) = signing.key_id() {
        let signature = sign_utils::sign_parcel_hash(key_id, &object.hash)?;
        sign_utils::store_parcel_signature(&object.hash, &signature)?;
    }

    Ok(Some(Repair { parcel: object.hash, tree, removed }))
}

/// The first path in the whole tree of `head` whose name opens the metadata folder, by the rule
/// [`repair_head`] removes with: `Some` exactly when it has something to repair.
pub fn first_repairable_path(head: &str) -> Result<Option<String>, String> {
    let mut pending = vec![(String::new(), object_utils::load_parcel(head)?.tree_hash)];

    while let Some((prefix, hash)) = pending.pop() {
        let tree = object_utils::load_tree(&hash)?;

        for (name, _) in tree.get_files().chain(tree.get_subtrees()) {
            if office_utils::opens_metadata_folder(Path::new(&prefix), name)? {
                return Ok(Some(audit_utils::child_path(&prefix, name)));
            }
        }

        pending.extend(tree.get_subtrees().map(|(name, subtree)| (audit_utils::child_path(&prefix, name), subtree.hash.clone())));
    }

    Ok(None)
}

/// What [`strip`] made of a tree.
enum Stripped {
    /// Nothing in it opened the metadata folder.
    Unchanged,

    /// Something was removed; the stored tree that is left.
    Changed(String),

    /// Something was removed, and nothing is left.
    Emptied,
}

/// Strip the tree `hash`, at warehouse path `prefix`, of every entry whose name opens the metadata
/// folder, recording each removed path in `removed`. A subtree only emptied by the removals is
/// dropped; one that was empty already is kept.
fn strip(hash: &str, prefix: &str, removed: &mut Vec<String>) -> Result<Stripped, String> {
    let tree = object_utils::load_tree(hash)?;
    let mut kept = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
    let mut changed = false;

    for (name, file) in tree.get_files() {
        if office_utils::opens_metadata_folder(Path::new(prefix), name)? {
            removed.push(audit_utils::child_path(prefix, name));
            changed = true;
        } else {
            kept.add_child(file.clone());
        }
    }

    for (name, subtree) in tree.get_subtrees() {
        let path = audit_utils::child_path(prefix, name);

        if office_utils::opens_metadata_folder(Path::new(prefix), name)? {
            removed.push(path);
            changed = true;
            continue;
        }

        match strip(&subtree.hash, &path, removed)? {
            Stripped::Unchanged => kept.add_child(subtree.clone()),
            Stripped::Changed(hash) => {
                kept.add_child(TreeItem::new(name.clone(), hash, DirEntryType::Tree));
                changed = true;
            }
            Stripped::Emptied => changed = true,
        }
    }

    if !changed {
        return Ok(Stripped::Unchanged);
    }

    if kept.get_files().len() == 0 && kept.get_subtrees().len() == 0 {
        return Ok(Stripped::Emptied);
    }

    Ok(Stripped::Changed(store_tree(&kept)?))
}

/// Store `tree` as a loose object and return its hash.
fn store_tree(tree: &TreeItem) -> Result<String, String> {
    let mut object = LooseObjectBuilder::build_tree(tree);
    object.store()?;

    Ok(object.hash)
}
