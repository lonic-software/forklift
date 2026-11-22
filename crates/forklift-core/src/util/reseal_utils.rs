//! Re-sealing: recording again, as held by the origin, absent objects a cache warehouse's sealed
//! record lost (`heal`).
//!
//! A cache warehouse holds by hash, in its sealed record, the objects it freed or never fetched,
//! and a read refills one from the origin on demand ([`cache_miss`](crate::util::cache_miss)). An
//! entry the record lost turns such an absence into a loss, although the origin still holds the
//! object. This records it again, but only on the premise every other writer of the record stands
//! on: the object is in the closure of heads the origin holds.
//!
//! Those heads are the ones `store evict` frees under: the origin's live pallet and tombstone
//! heads, and the earlier heads of the held-heads record it confirms it still reaches, from a
//! handshake whose trust is judged as a lower would judge it. Meta pallets are left out, as evict
//! keeps meta history in full. The walk is local: it starts from those heads present here, follows
//! present parcels and trees, never descends an absent or withheld object, and reads nothing from
//! the server and stores nothing. A lost hash is re-sealed only when that walk meets it as a tree,
//! a file or a chunk: objects are content-addressed, so a present tree reached from a held head
//! through present objects is in the origin's held closure, and so is every child it names. Store
//! presence on the server alone is never enough: an interrupted upload can leave a tree there
//! without its blobs.
//!
//! What the walk does not meet stays lost: a lost parcel (the walk never meets one as a child),
//! content only unlifted local work reaches, content the origin's host withholds, anything a local
//! meta or adopted head reaches (evict keeps those in full, so they were never sealed by it), and a
//! target found only beneath an absent tree.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use crate::util::lock_utils::StoreAccessLock;
use crate::util::remote_utils::{self, HandshakeError, RemoteClient};
use crate::util::{
    bay_utils, cache_miss, file_utils, gc_utils, held_heads, object_utils, office_utils, pallet_utils,
    recovery_utils, sealed_utils,
};

/// What [`reseal_held`] did.
#[derive(Debug, Default)]
pub struct Resealed {
    /// The lost objects recorded again as held by the origin.
    pub recorded: Vec<String>,

    /// What limited the re-seal, one sentence each: a pallet the origin left unanswered, a
    /// held-heads record that does not parse, or a walk or write that could not finish.
    pub notes: Vec<String>,
}

/// Why [`reseal_held`] re-sealed nothing because of the origin.
#[derive(Debug)]
pub enum ResealError {
    /// The origin sent no response at all: running heal again once it answers may re-seal.
    NoResponse(String),
    /// The origin, or this warehouse's view of it, refused: its own message and next step stand.
    Refused(String),
}

/// Record again as sealed each of `lost`, absent objects of a cache warehouse that its sealed
/// record does not hold, that a local walk of the origin's held history meets (see the module
/// doc). The caller holds store access throughout, so nothing it re-seals is evicted meanwhile.
///
/// # Arguments
/// * `lost`          - The lost absent hashes ([`recovery_utils::find_lost_objects`]).
/// * `_store_access` - The caller's store-access hold.
///
/// # Returns
/// * `Ok(Resealed)`      - How many were recorded, and what limited it; a walk or a write that
///                         could not finish records nothing and says why in a note.
/// * `Err(ResealError)`  - The origin sent no response, or the client, the handshake or the
///                         origin's trust refused. Nothing was recorded.
pub async fn reseal_held(lost: &BTreeSet<String>, _store_access: &StoreAccessLock) -> Result<Resealed, ResealError> {
    let mut resealed = Resealed::default();
    if lost.is_empty() {
        return Ok(resealed);
    }

    let refused = |why: String| ResealError::Refused(why);
    let client = RemoteClient::for_warehouse().map_err(|e| refused(e.into()))?;
    let info = client.fetch_info_answered().await.map_err(|e| match e {
        HandshakeError::NoResponse(why) => ResealError::NoResponse(why),
        HandshakeError::Answered(why) => ResealError::Refused(why),
    })?;
    remote_utils::check_remote_trust(&client, &info, None, office_utils::TrustReadMode::Checked).map_err(refused)?;

    // Meta history is fetched and kept in full, never sealed, so its heads seal nothing here.
    let held: BTreeMap<String, String> = info.held_pallet_heads().into_iter()
        .filter(|(pallet, _)| !pallet.starts_with(pallet_utils::META_QUALIFIER))
        .collect();
    let mut heads: Vec<String> = held.values().cloned().collect();

    // Fewer heads meet fewer targets: a recorded head the origin does not confirm only seals less.
    match held_heads::confirmed_heads(&client, &held).await {
        Ok(confirmation) => {
            heads.extend(confirmation.heads);
            for (pallet, why) in confirmation.unanswered {
                resealed.notes.push(format!(
                    "The origin did not confirm the earlier heads of pallet \"{}\", so only its current head was walked: {}",
                    pallet, why.trim_end_matches('.')
                ));
            }
        }
        Err(why) => resealed.notes.push(format!(
            "The record of earlier held heads could not be read, so only the origin's current heads were walked: {}",
            why.trim_end_matches('.')
        )),
    }

    let withheld: HashSet<String> = info.withheld.iter().flatten().map(|item| item.hash.clone()).collect();

    let met = match met_under(&heads, lost, &withheld) {
        Ok(met) => met,
        Err(why) => {
            resealed.notes.push(format!("Nothing was re-sealed: the walk of the origin's held history could not finish: {}", why.trim_end_matches('.')));
            return Ok(resealed);
        }
    };

    match sealed_utils::record_held_absences(&met) {
        Ok(recorded) => resealed.recorded = recorded,
        Err(why) => resealed.notes.push(format!("Nothing was re-sealed: the sealed record could not be written: {}", why.trim_end_matches('.'))),
    }

    Ok(resealed)
}

/// Those of `targets` a local walk from `heads` meets as a tree, a file or a chunk, but no
/// `withheld` one and none a local meta or adopted head reaches, sorted. The walk descends only
/// present trees and present recipes that are not withheld, and stops once every target is met.
fn met_under(heads: &[String], targets: &BTreeSet<String>, withheld: &HashSet<String>) -> Result<Vec<String>, String> {
    let _unfetched = cache_miss::Unfetched::enter();

    let mut met: BTreeSet<String> = BTreeSet::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = Vec::new();

    for parcel in gc_utils::collect_reachable_for_sweep(heads)? {
        stack.push(object_utils::load_parcel(&parcel)?.tree_hash);
    }

    let meet = |hash: &str, met: &mut BTreeSet<String>| {
        if targets.contains(hash) {
            met.insert(hash.to_string());
        }
    };

    while let Some(hash) = stack.pop() {
        if met.len() == targets.len() {
            break;
        }

        if !seen.insert(hash.clone()) {
            continue;
        }

        meet(&hash, &mut met);
        if withheld.contains(&hash) || !file_utils::is_present_for_sweep(&hash)? {
            continue;
        }

        let tree = object_utils::load_tree(&hash)?;

        for (_, file) in tree.get_files() {
            if !seen.insert(file.hash.clone()) {
                continue;
            }

            meet(&file.hash, &mut met);
            if file.item_type.is_chunked() && !withheld.contains(&file.hash) && file_utils::is_present_for_sweep(&file.hash)? {
                for chunk in object_utils::recipe_chunk_hashes(&file.hash)? {
                    meet(&chunk, &mut met);
                }
            }
        }

        for (_, subtree) in tree.get_subtrees() {
            stack.push(subtree.hash.clone());
        }
    }

    if met.is_empty() {
        return Ok(Vec::new());
    }

    let local_meta = meta_reach()?;

    Ok(met.into_iter().filter(|hash| !withheld.contains(hash) && !local_meta.contains(hash)).collect())
}

/// Every object the local meta pallet heads and the trust anchor's adopted pin reach, present or
/// not: evict keeps those closures in full, so none of it was sealed by a writer this stands in for.
fn meta_reach() -> Result<HashSet<String>, String> {
    let bay_dirs = bay_utils::all_bay_state_dirs()?;
    let sources = recovery_utils::gc_root_sources(
        &bay_dirs, bay_utils::BayReadPolicy::FailClosed, office_utils::TrustPinReadPolicy::FailClosed, recovery_utils::NotedIn::Other,
    )?;

    let mut heads = sources.meta_heads.clone();
    heads.extend(sources.adopted_heads());

    let mut reach: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = Vec::new();
    for parcel in gc_utils::collect_reachable_for_sweep(&heads)? {
        stack.push(object_utils::load_parcel(&parcel)?.tree_hash);
    }

    while let Some(hash) = stack.pop() {
        if !reach.insert(hash.clone()) || !file_utils::is_present_for_sweep(&hash)? {
            continue;
        }

        let tree = object_utils::load_tree(&hash)?;
        for (_, file) in tree.get_files() {
            if reach.insert(file.hash.clone()) && file.item_type.is_chunked() && file_utils::is_present_for_sweep(&file.hash)? {
                reach.extend(object_utils::recipe_chunk_hashes(&file.hash)?);
            }
        }
        for (_, subtree) in tree.get_subtrees() {
            stack.push(subtree.hash.clone());
        }
    }

    Ok(reach)
}
