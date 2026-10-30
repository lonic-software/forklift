//! The check a move of a partial warehouse's origin makes against the new server before it
//! rewrites anything: that the new server holds every object this warehouse lacks and leans on its
//! origin for.
//!
//! What this warehouse leans on its origin for is its absent content: every tree, file and chunk
//! that something present here references and the store does not hold. A cache records it (the
//! sealed-object record), a sparse warehouse left it out of scope; either way the walk from the
//! roots a sweep keeps meets it ([`sealed_utils::walk`]), and an absence the record should hold
//! and does not is checked like any other, since the move needs only that the new server holds it.
//!
//! What the new server holds is the closure of the heads it serves: its pallets and its tombstones.
//! The check fetches the parcels and signatures of that history it lacks
//! ([`remote_utils::fetch_parcels_only`]), no content, and walks the trees those parcels name,
//! here where they are present and from the new server, read into memory and never stored, where
//! they are not. Every absent object the walk meets is held there, and the walk stops as soon as
//! it has met them all. It takes the new server's closure to be whole, which serving a partial
//! warehouse would break.
//!
//! The fetched-heads ledger and the held-heads record are not checked. Both name heads a remote
//! may no longer hold, and both readers ask the remote rather than trust an entry: `scope fetch` drops a
//! ledger head the origin cannot serve, and `scope prune` asks the origin about each held head. A
//! ledger head may also be local work an `undo` left, which no server holds.

use std::collections::HashSet;
use crate::error::{CoreError, RefusalCode};
use crate::model::remote::WarehouseInfo;
use crate::model::tree_item::TreeItem;
use crate::util::progress_utils::Progress;
use crate::util::remote_utils::{self, RemoteClient};
use crate::util::sealed_utils::{self, StoreAccessHold, Unrecorded};
use crate::util::{audit_utils, object_utils};

/// The most missing objects a refusal's message names.
const NAMED_MISSING: usize = 10;

/// The most missing objects a refusal's details list.
const LISTED_MISSING: usize = 1000;

/// What [`check_new_origin`] found and fetched.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Checked {
    /// The parcels the walk of the new server's history descended into.
    pub walked_parcels: usize,
    /// The parcels and signatures it fetched, and stored.
    pub fetched_objects: usize,
    pub fetched_signatures: usize,
    /// The absent objects this warehouse leans on its origin for, each found on the new server.
    pub absent_objects: usize,
    /// The trees and recipes read from the new server into memory to find them.
    pub read_objects: usize,
}

/// Check that the server behind `client` holds every object this warehouse lacks and leans on its
/// origin for (see the module doc comment).
///
/// # Arguments
/// * `client`   - The new server's client: not the origin-checked one, which refuses while the move
///                stands.
/// * `info`     - Its handshake.
/// * `_hold`    - A store-access hold, so nothing is evicted while the absent objects are counted.
/// * `progress` - Where the fetch reports.
///
/// # Returns
/// * `Ok(Checked)`    - It holds them all.
/// * `Err(CoreError)` - An `origin_move_incomplete` refusal naming what it lacks; or a transfer, a
///   verification or a read failed.
pub async fn check_new_origin(client: &RemoteClient,
                              info: &WarehouseInfo,
                              _hold: &impl StoreAccessHold,
                              progress: &Progress) -> Result<Checked, CoreError> {
    let heads: Vec<String> = info.held_pallet_heads().into_values().collect();
    let mut checked = Checked::default();

    for head in &heads {
        let stats = remote_utils::fetch_parcels_only(client, head, progress).await?;
        checked.walked_parcels += stats.walked_parcels;
        checked.fetched_objects += stats.fetched_objects;
        checked.fetched_signatures += stats.fetched_signatures;
    }

    progress.status("Checking the new server's content");

    let sealed = sealed_utils::read_sealed()?;
    let walked = sealed_utils::walk(&[], &sealed, Unrecorded::Boundary)?
        .map_err(|damage| damage.describe("which a walk that passes absences never reports"))?;
    let mut missing: HashSet<String> = walked.absent;
    checked.absent_objects = missing.len();

    if missing.is_empty() {
        return Ok(checked);
    }

    let (parcels, gaps) = audit_utils::collect_reachable_noting_gaps(&heads, sealed_utils::is_present)?;
    if let Some(gap) = gaps.first() {
        return Err(CoreError::from(format!(
            "Parcel {} of the new server's history is missing here after it was fetched; nothing was changed.",
            gap
        )));
    }

    let mut walk = ServerWalk { seen: HashSet::new(), local: Vec::new(), trees: Vec::new(), recipes: Vec::new() };
    for parcel in &parcels {
        walk.local.push(object_utils::load_parcel(parcel)?.tree_hash);
    }

    // Trees held here first, then trees read from the server, then recipes read from it: a chunk
    // only an absent recipe names is the rarest thing to look for, and the dearest. What is read
    // from the server is read a wave at a time, concurrently.
    loop {
        if missing.is_empty() {
            return Ok(checked);
        }

        if let Some(hash) = walk.local.pop() {
            walk.meet_tree(&hash, &mut missing)?;
        } else if !walk.trees.is_empty() {
            let wave = std::mem::take(&mut walk.trees);
            for (hash, bytes) in remote_utils::read_objects_verified(client, &wave).await? {
                checked.read_objects += 1;
                walk.descend(&object_utils::parse_tree_bytes(&hash, &bytes)?, &mut missing)?;
            }
        } else if !walk.recipes.is_empty() {
            let wave = std::mem::take(&mut walk.recipes);
            for (hash, bytes) in remote_utils::read_objects_verified(client, &wave).await? {
                checked.read_objects += 1;
                for chunk in object_utils::parse_recipe_bytes(&hash, &bytes)?.chunks {
                    missing.remove(&chunk.hash);
                }
            }
        } else {
            return Err(incomplete_refusal(client.url(), &missing));
        }
    }
}

/// The new server's trees and recipes still to walk.
struct ServerWalk {
    seen: HashSet<String>,
    /// Trees to judge here: walked where present, else queued to read from the server.
    local: Vec<String>,
    /// Absent trees to read from the server.
    trees: Vec<String>,
    /// Absent recipes to read from the server.
    recipes: Vec<String>,
}

impl ServerWalk {
    /// Meet the tree `hash` of the new server's history: one present here is descended, and an
    /// absent one is found and queued to read, since something present may name an absent object
    /// beneath it too.
    fn meet_tree(&mut self, hash: &str, missing: &mut HashSet<String>) -> Result<(), String> {
        if !self.seen.insert(hash.to_string()) {
            return Ok(());
        }
        missing.remove(hash);

        if sealed_utils::is_present(hash)? {
            self.descend(&object_utils::load_tree(hash)?, missing)
        } else {
            self.trees.push(hash.to_string());
            Ok(())
        }
    }

    /// Find what the tree `tree` names, and queue its subtrees and the recipes it names.
    fn descend(&mut self, tree: &TreeItem, missing: &mut HashSet<String>) -> Result<(), String> {
        for (_, file) in tree.get_files() {
            missing.remove(&file.hash);
            if !file.item_type.is_chunked() || !self.seen.insert(file.hash.clone()) {
                continue;
            }

            if sealed_utils::is_present(&file.hash)? {
                for chunk in object_utils::recipe_chunk_hashes(&file.hash)? {
                    missing.remove(&chunk);
                }
            } else {
                self.recipes.push(file.hash.clone());
            }
        }

        for (_, subtree) in tree.get_subtrees() {
            self.local.push(subtree.hash.clone());
        }

        Ok(())
    }
}

/// The `origin_move_incomplete` refusal for a new server at `url` whose history does not reach
/// `missing`.
fn incomplete_refusal(url: &str, missing: &HashSet<String>) -> CoreError {
    let mut named: Vec<&String> = missing.iter().collect();
    named.sort();

    let next_step = format!(
        "Copy the old origin's warehouse to \"{}\" on the server's side, then run \"forklift remote move {}\" again.",
        url, url
    );
    let shown: Vec<&str> = named.iter().take(NAMED_MISSING).map(|hash| hash.as_str()).collect();
    let more = match named.len().saturating_sub(NAMED_MISSING) {
        0 => String::new(),
        rest => format!(" and {} more", rest),
    };

    CoreError::refusal_with_details(
        RefusalCode::OriginMoveIncomplete,
        format!(
            "\"{}\" does not hold {} object(s) this warehouse lacks and relies on its origin for ({}{}), \
            so it cannot be the origin; the origin was left where it was. {}",
            url, named.len(), shown.join(", "), more, next_step
        ),
        next_step,
        serde_json::json!({
            "missing": named.iter().take(LISTED_MISSING).collect::<Vec<_>>(),
            "missing_count": named.len(),
        }),
    )
}
