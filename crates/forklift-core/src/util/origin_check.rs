//! The check a move of a partial warehouse's origin makes against the new server before it
//! rewrites anything: that the new server holds every object this warehouse lacks and leans on its
//! origin for.
//!
//! What this warehouse leans on its origin for is its absent content: every tree, file and chunk
//! that something present here references and the store does not hold. A cache records it (the
//! sealed-object record), a sparse warehouse left it out of scope; either way the walk from the
//! roots a sweep keeps meets it ([`sealed_utils::walk`]), and an absence the record should hold
//! and does not is checked like any other, since the move needs only that the new server holds it.
//! An object absent because a redaction this warehouse applied names it is not leaned on: only
//! history names it, and this warehouse holds it absent whichever server it leans on. So the new
//! server is never asked for one, not even read from it while looking for the rest.
//!
//! What the new server holds is the closure of the heads it serves: its pallets and its tombstones.
//! The check fetches the parcels and signatures of that history it lacks
//! ([`remote_utils::fetch_parcels_only`]), no content, and walks the trees those parcels name,
//! here where they are present and from the new server, read into memory and never stored, where
//! they are not. Every absent object the walk meets is held there, and the walk stops as soon as
//! it has met them all. It takes the new server's closure to be whole but for the content its host
//! withholds (the handshake's `withheld` list), which serving a partial warehouse would break.
//!
//! A file whose content the new server withholds is not held there, though its history names it,
//! and neither is a chunk only such a file's recipe names: the server answers neither. An absent
//! object of that kind is no reason to refuse by itself. The check returns it as content the new
//! origin will not supply ([`Checked::withheld`]), and the move judges it against the previous
//! origin ([`judge_withheld`]): refused while that origin answers and does not withhold it,
//! reported otherwise.
//!
//! The fetched-heads ledger and the held-heads record are not checked. Both name heads a remote
//! may no longer hold, and both readers ask the remote rather than trust an entry: `scope fetch` drops a
//! ledger head the origin cannot serve, and `scope prune` asks the origin about each held head. A
//! ledger head may also be local work an `undo` left, which no server holds.

use std::collections::{BTreeMap, HashMap, HashSet};
use crate::error::{CoreError, RefusalCode};
use crate::model::remote::WarehouseInfo;
use crate::model::tree_item::TreeItem;
use crate::util::progress_utils::Progress;
use crate::util::remote_utils::{self, RemoteClient};
use crate::util::sealed_utils::{self, StoreAccessHold, Unrecorded};
use crate::util::{audit_utils, object_utils};
use crate::util::applied_redactions::AppliedSet;

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
    /// The content the new server withholds that this warehouse lacks and leans on its origin
    /// for, by hash, each with the host's statement: an absent withheld file, or a withheld recipe
    /// held here whose chunks are not. Not counted in `absent_objects`.
    pub withheld: BTreeMap<String, String>,
}

/// Why a move goes ahead though the new origin withholds content this warehouse leans on its
/// origin for ([`judge_withheld`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreviousOrigin {
    /// The previous origin withholds that content too.
    Withholds,
    /// No previous origin other than the new server is recorded, or its record could not be read.
    /// A move finishing one that stopped while rewriting may find the record already naming the
    /// new server, and then cannot tell which origin came before.
    Unrecorded,
    /// The previous origin did not answer its handshake, with the error.
    Unreachable(String),
}

impl PreviousOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            PreviousOrigin::Withholds => "withholds",
            PreviousOrigin::Unrecorded => "unrecorded",
            PreviousOrigin::Unreachable(_) => "unreachable",
        }
    }
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
/// * `Ok(Checked)`    - It holds them all but the content its host withholds, which
///   [`Checked::withheld`] lists and the caller must judge ([`judge_withheld`]).
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

    let tolerated = sealed_utils::read_tolerated()?;
    let walked = sealed_utils::walk(&[], &tolerated, Unrecorded::Boundary)?
        .map_err(|damage| damage.describe("which a walk that passes absences never reports"))?;
    let mut missing: HashSet<String> = walked.absent;
    let leaned_on = missing.len();
    checked.absent_objects = leaned_on;

    if missing.is_empty() {
        return Ok(checked);
    }

    let withheld: HashMap<&str, &str> = info.withheld.iter().flatten()
        .map(|item| (item.hash.as_str(), item.statement.as_str()))
        .collect();
    // Withheld content this warehouse lacks, looked for beneath the absent trees it leans on: an
    // absent file, or a recipe held here some of whose chunks are not. Looking costs a walk of
    // that whole region at worst, read from the new server, paid only on a move to a server whose
    // host withholds content absent here.
    let mut sought = HashSet::new();
    for hash in withheld.keys() {
        if lacks_withheld(hash)? {
            sought.insert(hash.to_string());
        }
    }

    let (parcels, gaps) = audit_utils::collect_reachable_noting_gaps(&heads, sealed_utils::is_present)?;
    if let Some(gap) = gaps.first() {
        return Err(CoreError::from(format!(
            "Parcel {} of the new server's history is missing here after it was fetched; nothing was changed.",
            gap
        )));
    }

    let leaned_roots = missing.clone();
    let mut walk = ServerWalk {
        applied: &tolerated.applied, withheld: &withheld, leaned_roots: &leaned_roots, sought,
        found: HashSet::new(), leaned_recipes: HashSet::new(), withheld_chunks: HashMap::new(),
        seen: HashSet::new(), seen_leaned: HashSet::new(), local: Vec::new(), trees: Vec::new(), recipes: Vec::new(),
    };
    for parcel in &parcels {
        walk.local.push((object_utils::load_parcel(parcel)?.tree_hash, false));
    }

    // Trees held here first, then trees read from the server, then recipes read from it: a chunk
    // only an absent recipe names is the rarest thing to look for, and the dearest. What is read
    // from the server is read a wave at a time, concurrently. Once nothing else is missing, the
    // walk goes on only beneath the absent trees this warehouse leans on, and only while withheld
    // content absent here is still sought.
    loop {
        if missing.is_empty() {
            if walk.sought.is_empty() {
                break;
            }
            walk.local.retain(|(_, leaned)| *leaned);
            walk.trees.retain(|(_, leaned)| *leaned);
            walk.recipes.clear();
        }

        if let Some((hash, leaned)) = walk.local.pop() {
            walk.meet_tree(&hash, leaned, &mut missing)?;
        } else if !walk.trees.is_empty() {
            let mut wave: HashMap<String, bool> = HashMap::new();
            for (hash, leaned) in std::mem::take(&mut walk.trees) {
                *wave.entry(hash).or_insert(false) |= leaned;
            }
            let hashes: Vec<String> = wave.keys().cloned().collect();
            for (hash, bytes) in remote_utils::read_objects_verified(client, &hashes).await? {
                checked.read_objects += 1;
                let leaned = wave.get(&hash).copied().unwrap_or(false);
                walk.descend(&object_utils::parse_tree_bytes(&hash, &bytes)?, leaned, &mut missing)?;
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
            break;
        }
    }

    for hash in &walk.found {
        checked.withheld.insert(hash.clone(), withheld[hash.as_str()].to_string());
    }
    let (settled, asked) = leaned_asks(&walk.leaned_recipes, &withheld)?;
    let answered = if asked.is_empty() {
        HashSet::new()
    } else {
        let chunks: Vec<String> = asked.keys().cloned().collect();
        answered_recipes(&asked, &client.missing_objects(&chunks).await?)
    };
    for recipe in settled.iter().chain(&answered) {
        checked.withheld.insert(recipe.clone(), withheld[recipe.as_str()].to_string());
    }

    // What is left the new server does not hold: content it withholds, a chunk only such a
    // recipe names, or anything else, which refuses the move.
    let left = missing.len();
    missing.retain(|hash| {
        let owner = if withheld.contains_key(hash.as_str()) { Some(hash) } else { walk.withheld_chunks.get(hash) };
        match owner {
            Some(owner) => {
                checked.withheld.insert(owner.clone(), withheld[owner.as_str()].to_string());
                false
            }
            None => true,
        }
    });

    if !missing.is_empty() {
        return Err(incomplete_refusal(client.url(), &missing));
    }

    checked.absent_objects = leaned_on - left;
    Ok(checked)
}

/// Sorts the withheld recipes held here beneath a tree leaned on. A chunk of such a recipe is
/// held on the new server while another recipe names it, and that server lists as missing a
/// chunk only withheld recipes name, so the chunks absent here are asked about, each mapped to
/// the recipes naming it. A recipe naming an absent chunk the host withholds itself is settled
/// at once, since the server never lists such a chunk missing; its chunks are not asked.
fn leaned_asks(
    recipes: &HashSet<String>,
    withheld: &HashMap<&str, &str>,
) -> Result<(Vec<String>, HashMap<String, Vec<String>>), CoreError> {
    let (mut settled, mut asked) = (Vec::new(), HashMap::<String, Vec<String>>::new());
    for recipe in recipes {
        let mut absent = Vec::new();
        for chunk in object_utils::recipe_chunk_hashes(recipe)? {
            if !sealed_utils::is_present(&chunk)? {
                absent.push(chunk);
            }
        }
        if absent.iter().any(|chunk| withheld.contains_key(chunk.as_str())) {
            settled.push(recipe.clone());
            continue;
        }
        for chunk in absent {
            asked.entry(chunk).or_default().push(recipe.clone());
        }
    }
    Ok((settled, asked))
}

/// The recipes naming a chunk the new server answered as missing.
fn answered_recipes(asked: &HashMap<String, Vec<String>>, missing: &[String]) -> HashSet<String> {
    missing.iter().flat_map(|chunk| asked.get(chunk).into_iter().flatten().cloned()).collect()
}

/// Whether this warehouse may lack the withheld content `hash`, so the walk seeks it: it does not
/// hold it, or holds it as a recipe some of whose chunks it does not hold.
fn lacks_withheld(hash: &str) -> Result<bool, String> {
    if !sealed_utils::is_present(hash)? {
        return Ok(true);
    }

    // Read whole to learn its type: a blob is at most the whole-object ceiling.
    match crate::util::redaction_utils::stored_type(hash)? {
        None => Ok(true),
        Some(crate::enums::object_type::ObjectType::Recipe) => {
            for chunk in object_utils::recipe_chunk_hashes(hash)? {
                if !sealed_utils::is_present(&chunk)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Some(_) => Ok(false),
    }
}

/// Judge the content the new server withholds that this warehouse leans on its origin for
/// ([`Checked::withheld`]) against the previous origin, which a move reads only for this.
///
/// The move is refused while the previous origin is recorded, answers its handshake and does not
/// withhold some of that content: it does not say it withholds it, so the warehouse may still have
/// a server that supplies it. Whether it holds the content is not asked. Otherwise it
/// goes ahead, and the caller reports the content as what the new origin will not supply. A move
/// deletes nothing, so the bytes stay wherever they were, and refusing over an origin that no
/// longer answers would hold the warehouse on it for good. Any failure to read the handshake,
/// a refusal of the token included, is read as not answering.
///
/// # Arguments
/// * `withheld` - What the check found withheld ([`Checked::withheld`]); with none, nothing is
///                dialed.
/// * `previous` - The previous origin as recorded; none when the record was missing or could not
///                be read.
/// * `target`   - The new server's URL.
/// * `token`    - The bearer token to dial the previous origin with.
///
/// # Returns
/// * `Ok(reason)`     - The move may go ahead, and why; `None` when nothing is withheld.
/// * `Err(CoreError)` - An `origin_move_incomplete` refusal naming the content the previous origin
///   does not withhold.
pub async fn judge_withheld(withheld: &BTreeMap<String, String>,
                            previous: Option<&str>,
                            target: &str,
                            token: Option<String>) -> Result<Option<PreviousOrigin>, CoreError> {
    if withheld.is_empty() {
        return Ok(None);
    }

    // A record naming the new server is one a stopped move already rewrote: the origin before it
    // is not known.
    let Some(previous) = previous.filter(|previous| !crate::util::origin_utils::same_remote(previous, target)) else {
        return Ok(Some(PreviousOrigin::Unrecorded));
    };

    let info = match RemoteClient::new(previous, token) {
        Ok(client) => client.fetch_info().await,
        Err(why) => Err(why),
    };
    let info = match info {
        Ok(info) => info,
        Err(why) => return Ok(Some(PreviousOrigin::Unreachable(why))),
    };

    let theirs: HashSet<&str> = info.withheld.iter().flatten().map(|item| item.hash.as_str()).collect();
    let served: Vec<&String> = withheld.keys().filter(|hash| !theirs.contains(hash.as_str())).collect();
    if served.is_empty() {
        return Ok(Some(PreviousOrigin::Withholds));
    }

    Err(withheld_refusal(target, previous, &served, withheld))
}

/// The first [`NAMED_MISSING`] of `hashes`, sorted, as a refusal's message names them, with how
/// many more there are.
fn named(hashes: &[&String]) -> String {
    let shown: Vec<&str> = hashes.iter().take(NAMED_MISSING).map(|hash| hash.as_str()).collect();
    match hashes.len().saturating_sub(NAMED_MISSING) {
        0 => shown.join(", "),
        rest => format!("{} and {} more", shown.join(", "), rest),
    }
}

/// The `origin_move_incomplete` refusal for a new server at `url` that withholds `served`, which
/// the previous origin at `previous` does not withhold.
fn withheld_refusal(url: &str, previous: &str, served: &[&String], statements: &BTreeMap<String, String>) -> CoreError {
    CoreError::refusal_with_details(
        RefusalCode::OriginMoveIncomplete,
        format!(
            "\"{}\" withholds {} object(s) this warehouse lacks and relies on its origin for ({}), which \
            the current origin \"{}\" does not withhold, so it cannot be the origin; the origin was left \
            where it was.",
            url, served.len(), named(served), previous
        ),
        format!(
            "Keep \"{}\" as the origin while it does not withhold that content, or read that content here \
            first, so this warehouse no longer relies on its origin for it, then run \"forklift remote move \
            {}\" again.",
            previous, url
        ),
        serde_json::json!({
            "missing": Vec::<String>::new(),
            "missing_count": 0,
            "withheld": served.iter().take(LISTED_MISSING)
                .map(|hash| serde_json::json!({ "hash": hash, "statement": statements[hash.as_str()] }))
                .collect::<Vec<_>>(),
            "withheld_count": served.len(),
        }),
    )
}

/// The new server's trees and recipes still to walk. A tree is *leaned on* when this warehouse
/// leans on its origin for it: an absent object the walk from its roots met ([`check_new_origin`]'s
/// `missing` as it starts), or anything beneath one.
struct ServerWalk<'a> {
    /// The redactions this warehouse applied: an absent object one names is never read from the
    /// server.
    applied: &'a AppliedSet,
    /// The content the new server withholds, with its host's statement: never held there, and
    /// never read from it.
    withheld: &'a HashMap<&'a str, &'a str>,
    /// The absent objects this warehouse leans on its origin for, as the walk from its roots met
    /// them.
    leaned_roots: &'a HashSet<String>,
    /// Withheld content absent here not yet met beneath a tree this warehouse leans on. A withheld
    /// recipe held here whose chunks are absent only beneath such a tree is not sought, so the walk
    /// may stop before it meets it.
    sought: HashSet<String>,
    /// Withheld content absent here met beneath a tree this warehouse leans on.
    found: HashSet<String>,
    /// Withheld recipes held here met beneath a tree this warehouse leans on: leaned on when the
    /// new server lacks one of their chunks.
    leaned_recipes: HashSet<String>,
    /// Each chunk a withheld recipe held here names, to the recipe: not held there unless another
    /// recipe the walk meets names it too.
    withheld_chunks: HashMap<String, String>,
    seen: HashSet<String>,
    /// The trees descended as leaned on: one first met otherwise is met again as leaned on.
    seen_leaned: HashSet<String>,
    /// Trees to judge here, each with whether it is leaned on: walked where present, else queued
    /// to read from the server.
    local: Vec<(String, bool)>,
    /// Absent trees to read from the server, each with whether it is leaned on.
    trees: Vec<(String, bool)>,
    /// Absent recipes to read from the server.
    recipes: Vec<String>,
}

impl ServerWalk<'_> {
    /// Meet the tree `hash` of the new server's history, beneath a tree leaned on when
    /// `parent_leaned`: one present here is descended, and an absent one is found and queued to
    /// read, since something present may name an absent object beneath it too.
    fn meet_tree(&mut self, hash: &str, parent_leaned: bool, missing: &mut HashSet<String>) -> Result<(), String> {
        let leaned = parent_leaned || self.leaned_roots.contains(hash);
        let first = self.seen.insert(hash.to_string());
        let first_leaned = leaned && self.seen_leaned.insert(hash.to_string());
        if !first && !first_leaned {
            return Ok(());
        }
        missing.remove(hash);

        if sealed_utils::is_present(hash)? {
            self.descend(&object_utils::load_tree(hash)?, leaned, missing)
        } else {
            // No redaction names a tree: only file content is redacted.
            self.trees.push((hash.to_string(), leaned));
            Ok(())
        }
    }

    /// Find what the tree `tree` names, and queue its subtrees and the recipes it names. Withheld
    /// content absent here that a tree leaned on (`leaned`) names is content this warehouse leans
    /// on its origin for and the new server does not hold.
    fn descend(&mut self, tree: &TreeItem, leaned: bool, missing: &mut HashSet<String>) -> Result<(), String> {
        for (_, file) in tree.get_files() {
            if self.withheld.contains_key(file.hash.as_str()) {
                // Neither the file nor, for a recipe, its chunks are held there by this name.
                let present = sealed_utils::is_present(&file.hash)?;
                if present && file.item_type.is_chunked() && self.seen.insert(file.hash.clone()) {
                    for chunk in object_utils::recipe_chunk_hashes(&file.hash)? {
                        self.withheld_chunks.entry(chunk).or_insert_with(|| file.hash.clone());
                    }
                }
                // Beneath a tree leaned on: an absent file is leaned on, and a recipe held here is
                // if the new server lacks one of its chunks, which it is asked once the walk ends.
                if leaned && !present && self.found.insert(file.hash.clone()) {
                    self.sought.remove(&file.hash);
                } else if leaned && present && file.item_type.is_chunked() && self.leaned_recipes.insert(file.hash.clone()) {
                    self.sought.remove(&file.hash);
                }
                continue;
            }

            missing.remove(&file.hash);
            if !file.item_type.is_chunked() || !self.seen.insert(file.hash.clone()) {
                continue;
            }

            if sealed_utils::is_present(&file.hash)? {
                for chunk in object_utils::recipe_chunk_hashes(&file.hash)? {
                    missing.remove(&chunk);
                }
            } else if !self.applied.contains(&file.hash) {
                self.recipes.push(file.hash.clone());
            }
        }

        for (_, subtree) in tree.get_subtrees() {
            self.local.push((subtree.hash.clone(), leaned));
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

    CoreError::refusal_with_details(
        RefusalCode::OriginMoveIncomplete,
        format!(
            "\"{}\" does not hold {} object(s) this warehouse lacks and relies on its origin for ({}), \
            so it cannot be the origin; the origin was left where it was.",
            url, named.len(), self::named(&named)
        ),
        next_step,
        serde_json::json!({
            "missing": named.iter().take(LISTED_MISSING).collect::<Vec<_>>(),
            "missing_count": named.len(),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enums::dir_entry_type::DirEntryType;
    use crate::globals::{self, StorageRootScope};

    /// An absent recipe a redaction this warehouse applied names is never queued to read from the
    /// new server; an absent recipe none names is, and a present one is not.
    ///
    /// Revert: queue every absent recipe → the applied one is queued → red.
    #[test]
    fn an_absent_applied_recipe_is_never_read_from_the_new_server() {
        let root = std::env::temp_dir().join(format!("forklift-origin-check-applied-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let _scope = StorageRootScope::enter(&root);

        let (redacted, absent) = ("1".repeat(64), "2".repeat(64));
        let applied = crate::util::applied_redactions::apply_for_test(&[&redacted]);
        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        tree.add_child(TreeItem::new("leak.bin".to_string(), redacted.clone(), DirEntryType::NormalChunked));
        tree.add_child(TreeItem::new("big.bin".to_string(), absent.clone(), DirEntryType::NormalChunked));

        let (withheld, roots) = (HashMap::new(), HashSet::new());
        let mut walk = ServerWalk {
            applied: &applied, withheld: &withheld, leaned_roots: &roots, sought: HashSet::new(),
            found: HashSet::new(), leaned_recipes: HashSet::new(), withheld_chunks: HashMap::new(), seen: HashSet::new(), seen_leaned: HashSet::new(),
            local: Vec::new(), trees: Vec::new(), recipes: Vec::new(),
        };
        let mut missing: HashSet<String> = HashSet::new();
        walk.descend(&tree, false, &mut missing).unwrap();

        assert_eq!(walk.recipes, vec![absent]);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A withheld file is never found held by the new server. A withheld recipe held here leaves
    /// the chunks only it names missing, each owned by the recipe, while a chunk another recipe
    /// names is found; a withheld file absent here is found as leaned on only beneath a tree this
    /// warehouse leans on.
    ///
    /// Revert: drop the chunk map → the lone chunk has no owner → red. Revert: find a withheld
    /// absent file beneath any tree → the walk not leaned on finds it → red. Revert: lack only an
    /// absent withheld object → the recipe missing a chunk is not found → red. Revert: map each
    /// chunk to its first recipe only, or drop the settled arm → red.
    #[test]
    fn withheld_content_is_never_found_on_the_new_server() {
        use crate::util::gc_utils::tests::store_recipe;

        let root = std::env::temp_dir().join(format!("forklift-origin-check-withheld-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let _scope = StorageRootScope::enter(&root);

        let (lone, shared, film) = ("3".repeat(64), "4".repeat(64), "5".repeat(64));
        let withheld_recipe = store_recipe(&[(lone.clone(), 1), (shared.clone(), 1)]);
        let other_recipe = store_recipe(&[(shared.clone(), 1)]);
        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        tree.add_child(TreeItem::new("taken.bin".to_string(), withheld_recipe.clone(), DirEntryType::NormalChunked));
        tree.add_child(TreeItem::new("other.bin".to_string(), other_recipe, DirEntryType::NormalChunked));
        tree.add_child(TreeItem::new("film.mkv".to_string(), film.clone(), DirEntryType::Normal));

        let applied = crate::util::applied_redactions::apply_for_test(&[]);
        let withheld: HashMap<&str, &str> = [(withheld_recipe.as_str(), "court order"), (film.as_str(), "court order")].into();
        let roots = HashSet::new();
        let walk_of = || ServerWalk {
            applied: &applied, withheld: &withheld, leaned_roots: &roots, sought: HashSet::from([film.clone()]),
            found: HashSet::new(), leaned_recipes: HashSet::new(), withheld_chunks: HashMap::new(), seen: HashSet::new(), seen_leaned: HashSet::new(),
            local: Vec::new(), trees: Vec::new(), recipes: Vec::new(),
        };

        let mut walk = walk_of();
        let mut missing: HashSet<String> = [lone.clone(), shared.clone(), withheld_recipe.clone(), film.clone()].into();
        walk.descend(&tree, false, &mut missing).unwrap();
        assert_eq!(missing, [lone.clone(), withheld_recipe.clone(), film.clone()].into());
        assert_eq!(walk.withheld_chunks.get(&lone), Some(&withheld_recipe));
        assert!(walk.found.is_empty(), "nothing is leaned on beneath a tree this warehouse does not lean on");

        // Beneath a tree leaned on the absent film is found, and the withheld recipe held here is
        // kept to ask the new server about its absent chunks.
        let mut walk = walk_of();
        walk.sought.insert(withheld_recipe.clone());
        walk.descend(&tree, true, &mut HashSet::new()).unwrap();
        assert_eq!(walk.found, HashSet::from([film.clone()]));
        assert_eq!(walk.leaned_recipes, HashSet::from([withheld_recipe.clone()]));
        assert!(walk.sought.is_empty());

        // The recipe held here is asked about by its absent chunks, and counts once the new
        // server lists that chunk missing; a recipe naming a chunk the host withholds itself is
        // settled without asking.
        let one = HashSet::from([withheld_recipe.clone()]);
        let (settled, asked) = leaned_asks(&one, &withheld).unwrap();
        assert!(settled.is_empty());
        assert_eq!(asked, HashMap::from([(lone.clone(), vec![withheld_recipe.clone()]), (shared.clone(), vec![withheld_recipe.clone()])]));
        assert_eq!(answered_recipes(&asked, &[lone.clone()]), one);
        assert!(answered_recipes(&asked, &[]).is_empty(), "a chunk the server holds leaves the recipe out");
        let twin = store_recipe(&[(lone.clone(), 1)]);
        let mut withheld = withheld.clone();
        withheld.insert(twin.as_str(), "court order");
        let (_, asked) = leaned_asks(&HashSet::from([withheld_recipe.clone(), twin.clone()]), &withheld).unwrap();
        assert_eq!(answered_recipes(&asked, &[lone.clone()]), HashSet::from([withheld_recipe.clone(), twin.clone()]));
        let mut withholding_chunk = withheld.clone();
        withholding_chunk.insert(lone.as_str(), "court order");
        let (settled, asked) = leaned_asks(&one, &withholding_chunk).unwrap();
        assert_eq!((settled, asked.is_empty()), (vec![withheld_recipe.clone()], true));

        // A withheld recipe held here with every chunk is not lacked.
        let whole = store_recipe(&[(crate::util::gc_utils::tests::store_chunk(b"c"), 1)]);
        assert!(!lacks_withheld(&whole).unwrap());
        assert!(lacks_withheld(&withheld_recipe).unwrap() && lacks_withheld(&film).unwrap());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A previous origin the record names as the new server is not one the move can ask: it reads
    /// as unrecorded, never as withholding, and nothing is dialed.
    ///
    /// Revert: read a record naming the new server as withholding → red.
    #[test]
    fn a_record_naming_the_new_server_reads_as_unrecorded() {
        let withheld = BTreeMap::from([("6".repeat(64), "court order".to_string())]);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let judged = runtime.block_on(judge_withheld(&withheld, Some("http://127.0.0.1:9/"), "http://127.0.0.1:9", None));
        assert_eq!(judged.unwrap(), Some(PreviousOrigin::Unrecorded));
        assert_eq!(runtime.block_on(judge_withheld(&BTreeMap::new(), None, "http://x", None)).unwrap(), None);
    }
}
