//! Heal's rebuild of a damaged inventory metadata file: the registered set of directory keys.
//!
//! A shard names only its entries, and its address is a one-way hash of its key, so the set cannot
//! be read back from the store. The rebuild proposes keys from everything that names a directory
//! this bay may have staged, and keeps each one whose shard address holds a shard data file:
//!
//! * the lines of the damaged file that still read whole;
//! * every directory of the working tree;
//! * every directory of the tree of the bay position's base (never the pallet's ref, which may
//!   still be damaged, or moved by another bay);
//! * while an operation marker stands, every directory of each tree it names.
//!
//! The rebuild is complete when every shard data file in the store is matched and every tree it
//! proposed from was read. Only then does plain `heal` write it; `heal --inventory` writes it
//! either way, and what it leaves unmatched is no longer registered.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{CoreError, RefusalCode};
use crate::globals;
use crate::model::tree_item::TreeItem;
use crate::util::operation_utils::Operation;
use crate::util::scope_utils::{self, PathScope, ScopeClass};
use crate::util::sealed_utils::{self, At, Held};
use crate::util::{file_utils, inventory_utils, object_utils, pallet_utils, record_utils};

/// Whether the inventory metadata reads as damaged (`inventory_damaged`): what makes heal rebuild
/// it. Any other failure to read it is not this step's to mend.
pub fn damaged() -> bool {
    match file_utils::retrieve_inventory_metadata_or_none() {
        Ok(_) => false,
        Err(error) => matches!(
            CoreError::from(error),
            CoreError::Refusal { code: RefusalCode::InventoryDamaged, .. }
        ),
    }
}

/// A tree the rebuild proposed directories from and could not read.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TreeUnread {
    /// The tree's hash.
    pub tree: String,

    /// What named it: `base` (the bay position's base parcel), or the standing operation's
    /// `source`, `target`, `their` or `parked` tree.
    pub from: String,

    /// Why it was not read.
    pub reason: String,
}

/// What the rebuild found, before anything is written.
pub struct Rebuild {
    /// The recovered keys: each candidate whose shard address holds a shard data file.
    keys: BTreeSet<String>,

    /// How many shard data files in the store no recovered key's address matches. Writing the
    /// rebuild leaves them unregistered, for the next sweep.
    pub unmatched: usize,

    /// Candidate trees that exist and could not be read: the directories only they name were not
    /// proposed.
    pub unread: Vec<TreeUnread>,

    /// Candidate trees whose read failed while this warehouse holds them by hash alone (sealed,
    /// withheld or recorded unavailable). They hold nothing back, but a read once the origin
    /// answers may propose directories they name.
    pub passed_over: Vec<TreeUnread>,
}

impl Rebuild {
    /// Whether the rebuild accounts for every shard on disk and read every tree it proposed from.
    pub fn complete(&self) -> bool {
        self.unmatched == 0 && self.unread.is_empty()
    }

    /// How many keys the rebuild registers, before the ancestor closure.
    pub fn recovered(&self) -> usize {
        self.keys.len()
    }

    /// Write the recovered keys as the registered set, keeping every shard on disk where it is.
    pub fn write(&self) -> Result<(), String> {
        inventory_utils::ShardLedger::rebuild(&self.keys).map(|_| ())
    }
}

/// Propose the registered set and check it against the shard store.
///
/// # Arguments
/// * `marker` - The standing operation marker, whose trees are candidates too.
///
/// # Returns
/// * `Ok(Rebuild)` - What a write would register, and what it would leave out.
/// * `Err(String)` - If the shard store could not be listed.
pub fn plan(marker: Option<&Operation>) -> Result<Rebuild, String> {
    let shards = file_utils::shard_data_addresses(false)?;

    let mut candidates: BTreeSet<String> = salvaged_keys();
    working_tree_keys(&working_tree_root(), "", &mut candidates);

    // Each candidate tree, or the parcel that names it when that parcel does not read: an unread
    // parcel is an unread tree, since it is the tree's only name here.
    let mut trees: Vec<Result<(String, &str), TreeUnread>> = Vec::new();
    let parcel_tree = |parcel: &str, from: &'static str| object_utils::load_parcel(parcel)
        .map(|parcel| (parcel.tree_hash, from))
        .map_err(|reason| TreeUnread { tree: parcel.to_string(), from: from.to_string(), reason: plain(reason) });

    // The position heal runs first; a position it left unread names no base.
    if let Ok(pallet_utils::BayPosition { base: Some(base), .. }) = pallet_utils::read_position() {
        trees.push(parcel_tree(&base, "base"));
    }
    if let Some(marker) = marker {
        for (tree, from) in [(&marker.source_tree, "source"), (&marker.target_tree, "target"), (&marker.their_tree, "their")] {
            if let Some(tree) = tree {
                trees.push(Ok((tree.clone(), from)));
            }
        }
        if let Some(parked) = &marker.parked {
            trees.push(parcel_tree(parked, "parked"));
        }
    }

    // A scope that cannot be read counts as full, which can only propose more, and hold more.
    let scope = scope_utils::current_scope().unwrap_or_else(|_| PathScope::full());
    let tolerated = sealed_utils::read_tolerated().ok();

    let mut unread = Vec::new();
    let mut passed_over = Vec::new();
    let mut walked: BTreeSet<String> = BTreeSet::new();
    for candidate in trees {
        let (tree, from) = match candidate {
            Ok(tree) => tree,
            Err(parcel) => {
                unread.push(parcel);
                continue;
            }
        };
        if !walked.insert(tree.clone()) {
            continue;
        }
        let mut failed = |hash: &str, reason: String| {
            let held = tolerated.as_ref().and_then(|tolerated| tolerated.held(hash, At::Tree));
            let entry = TreeUnread { tree: hash.to_string(), from: from.to_string(), reason };
            match held {
                Some(held) => passed_over.push(TreeUnread { reason: held_reason(held, &entry.reason), ..entry }),
                None => unread.push(entry),
            }
        };
        tree_keys(&tree, &scope, &mut candidates, &mut failed);
    }

    let keys: BTreeSet<String> = candidates.into_iter()
        .filter(|key| shards.contains(&file_utils::shard_address(key)))
        .collect();
    let matched: BTreeSet<(String, String)> = keys.iter().map(|key| file_utils::shard_address(key)).collect();

    Ok(Rebuild {
        unmatched: shards.difference(&matched).count(),
        keys,
        unread,
        passed_over,
    })
}

/// A read's error as the words a report shows, without the step a refusal of it names: the
/// rebuild's own refusal names the step that applies.
fn plain(error: String) -> String {
    match CoreError::from(error) {
        CoreError::Refusal { message, .. } => message,
        CoreError::Other(message) => message,
    }
}

/// The reason of a tree passed over, which this warehouse holds by hash alone.
fn held_reason(held: Held, reason: &str) -> String {
    let by = match held {
        Held::Sealed => "the sealed record holds it, and the origin did not give it",
        Held::Redacted => "an applied redaction names it",
        Held::Withheld => "this warehouse withholds it",
        Held::Unavailable => "it is recorded unavailable",
    };
    format!("{} ({})", by, reason.trim_end_matches('.'))
}

/// The keys the damaged metadata file still names on lines that read whole.
fn salvaged_keys() -> BTreeSet<String> {
    let Ok(bytes) = std::fs::read(file_utils::get_inventory_metadata_path()) else {
        return BTreeSet::new();
    };

    let (entries, _) = record_utils::salvage(record_utils::without_digest_line(&bytes), |line| Some(line.to_string()));
    entries.iter().map(|entry| inventory_utils::metadata_entry_to_key(entry).to_string()).collect()
}

/// The root of this checkout's working tree: the folder that holds its own `.forklift`.
fn working_tree_root() -> PathBuf {
    let forklift = globals::working_forklift();
    match forklift.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Add the key of every directory at or under `key` in the working tree, never through a link and
/// never into a `.forklift` folder. A folder that cannot be listed proposes nothing below it.
fn working_tree_keys(root: &Path, key: &str, keys: &mut BTreeSet<String>) {
    keys.insert(key.to_string());

    let Ok(entries) = std::fs::read_dir(root.join(key)) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name == globals::FOLDER_NAME_FORKLIFT_ROOT {
            continue;
        }
        let child = if key.is_empty() { name } else { format!("{}/{}", key, name) };
        working_tree_keys(root, &child, keys);
    }
}

/// Add the key of every directory of `tree` the bay's scope does not rule out, as the
/// tree-to-shard builder walks it. A tree that cannot be read is given to `failed` with the reason,
/// and proposes nothing below it.
fn tree_keys(root: &str, scope: &PathScope, keys: &mut BTreeSet<String>, failed: &mut impl FnMut(&str, String)) {
    let full = scope.is_full();
    let mut level: Vec<(String, String)> = vec![(String::new(), root.to_string())];

    while !level.is_empty() {
        let mut next = Vec::new();

        for (key, hash) in level {
            let tree: TreeItem = match object_utils::load_tree(&hash) {
                Ok(tree) => tree,
                Err(reason) => {
                    failed(&hash, plain(reason));
                    continue;
                }
            };
            keys.insert(key.clone());

            for (name, subtree) in tree.get_subtrees() {
                let child = if key.is_empty() { name.clone() } else { format!("{}/{}", key, name) };
                if !full && scope.classify(&child) == ScopeClass::OutOfScope {
                    continue;
                }
                next.push((child, subtree.hash.clone()));
            }
        }

        level = next;
    }
}
