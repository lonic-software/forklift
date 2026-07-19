//! The `forklift heal` recovery verb's own machinery (DESIGN.html §3.1.1): the deeper analysis
//! [`heal_utils::heal_if_tainted`](crate::util::heal_utils::heal_if_tainted) refuses into when its
//! best-effort restage pass cannot resolve every recorded path on its own.
//!
//! Entry-heal is deliberately conservative: any recorded path that comes back
//! [`Vanished`](heal_utils::RestageOutcome), [`Unreadable`](heal_utils::RestageOutcome),
//! [`HashMismatch`](heal_utils::RestageOutcome), or fails to restage operationally leaves the
//! whole taint standing, because entry-heal has no way to tell "this vanished object was never
//! referenced by anything durable" (safe — the retry that follows just restages it fresh) apart
//! from "this vanished object is a durable ref's only copy" (real loss) without doing the one
//! thing entry-heal must never do: a repository-scale walk on every command's hot path. This
//! module is that walk, run only here, on demand, by the one command whose whole job is to pay
//! for it.
//!
//! ## The closure walk
//!
//! For every recorded path this attempt cannot restage, [`run`] classifies it (a loose object, a
//! pack data/index file, or an inventory shard — the three shapes the taint schema can ever
//! record) and, for the object-shaped ones, checks whether the object it names is genuinely absent
//! (present elsewhere — another pack, or loose — is not a loss) and, if so, whether *any* durable
//! ref source still reaches it: pallet heads (both namespaces), every bay's parked parcels, tag
//! subjects, every bay's in-progress consolidation (its `their_head`), the trust anchor's adopted
//! head (a re-genesis pin, §8.7), and every bay's staged inventory shards **enumerated from the
//! files on disk**, never the registration ledger (`inventory_utils::write_metadata_to_file` is a
//! non-atomic, unsynced `std::fs::write` that is blind to a published-but-unregistered shard — the
//! phase-B wart; a walk that trusted it could silently miss a real dangling reference). The taint
//! is over the *shared* object store, so every bay-local source is read across every bay
//! (`bay_utils::all_bay_state_dirs`), never just the active one — see
//! [`collect_walk_roots`]'s own doc comment for why. An object no live ref reaches is safe to drop
//! from the taint — the same "absent + unreachable" shape a plain crash already leaves unhealed
//! today, which the ordinary retry-and-restage path handles without this module's help. An object
//! a live ref *does* still reach is reported, machine-coded, with the remedies that actually exist
//! for it.
//!
//! The walk is **read-only by construction**: every object read goes through [`object_utils`]'s
//! plain loaders (`load_parcel`/`load_tree`/`recipe_chunk_hashes`), which never call
//! [`file_utils::does_object_exist`] (the taint-gated presence check — calling it here would trip
//! the very gate this recovery is trying to lift, on its first call, under the tainted root this
//! walk necessarily runs under) and never touch [`crate::util::graph_utils`]'s persisting entry
//! points (`node`/`ensure` durably self-heal a commit-graph shard via `write_file_atomically` —
//! exactly the kind of barrier-on-a-possibly-failing-device this recovery path must never risk).
//! Presence of one already-known-suspect hash is checked with [`file_utils::raw_object_present`],
//! the one sanctioned gate-free presence check — see its own doc comment for why bypassing the
//! gate is safe specifically here. [`tests::closure_walk_never_touches_a_barrier_or_a_dir_sync`]
//! is the actual enforcer, not this paragraph.
//!
//! ## Partial clears
//!
//! A recovery attempt commonly resolves *some* recorded paths (restaged, or proven
//! vanished-and-unreferenced) while others remain genuinely dangling. The taint afterwards must
//! record exactly the unresolved remainder — [`taint_utils::replace_taint_with_remainder`] is the
//! crash-safe primitive that makes that true without ever leaving a window where the remainder is
//! unrecorded on disk.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use crate::enums::dir_entry_type::DirEntryType;
use crate::error::{CoreError, RefusalCode};
use crate::globals::{forklift_root, FOLDER_NAME_INVENTORY_ROOT};
use crate::util::{
    bay_utils, file_utils, heal_utils, object_utils, office_utils, pack_utils,
    pallet_utils, remote_utils, tag_utils, taint_utils,
};

/// How many dangling references a refusal message names individually before summarizing the
/// rest as "(and N more)" — mirrors `audit_utils::MAX_NAMED_MISSING_CHUNKS`'s reasoning: a pack
/// whose every object turns out dangling could otherwise produce a message with thousands of
/// lines, and a handful is enough for an operator (or an agent) to act on.
const MAX_NAMED_DANGLING: usize = 16;

/// The heavyweight exits named in a torn-taint or every-remedy-exhausted refusal: the same three
/// classes the design settles on (§3.2) — refetch, reproduce, or accept the loss — stated once so
/// the wording never drifts between the two refusal sites that need it.
const HEAVYWEIGHT_EXITS: &str = "re-fetch this warehouse's objects from a configured remote \
    (\"forklift lower\", or \"forklift franchise\" for a fresh clone), reproduce them by \
    re-running whatever operation created them if their content still exists in your working \
    tree (\"forklift load\" then \"forklift stack\"), or accept the loss — Forklift has no \
    in-tool way to drop a single dangling reference yet, so an object neither remedy reaches \
    stays lost until one of those two applies.";

/// What running `forklift heal` accomplished, on a full clear (see [`run`]'s `Ok` case). An
/// unresolved remainder is reported as an [`Err(CoreError)`](CoreError) instead — see [`run`].
pub struct HealOutcome {
    /// Whether anything was actually tainted at all — an untainted warehouse still reports
    /// success, with every list empty, so the command has something honest to say either way.
    pub was_tainted: bool,
    /// Recorded paths that were present, verified, and freshly rewritten.
    pub restaged: Vec<String>,
    /// Recorded paths (loose-object or pack-derived hashes, or a vanished shard's own path)
    /// resolved without a rewrite: proven absent *and* unreferenced by the closure walk, or (for
    /// a shard) a staging concern that carries no object-trust risk at all.
    pub resolved: Vec<String>,
    /// Advisory notes that never block clearing — currently, the "re-run the load" remedy note
    /// for each vanished inventory shard.
    pub notes: Vec<String>,
}

impl HealOutcome {
    fn nothing() -> HealOutcome {
        HealOutcome { was_tainted: false, restaged: Vec::new(), resolved: Vec::new(), notes: Vec::new() }
    }
}

/// Run the recovery verb once: read the standing taint (if any), attempt the same restage entry-
/// heal runs, and — for whatever it could not resolve — run the deeper, closure-walk-backed
/// analysis this module exists for. See the module doc comment for the full per-verdict behavior.
///
/// # Returns
/// * `Ok(HealOutcome)` - Nothing was tainted, or the taint is now **fully** cleared (every
///                       recorded path reached restaged, or vanished-and-unreferenced/a resolved
///                       shard note). The in-memory gate is cleared too.
/// * `Err(CoreError)`  - A [`RefusalCode::DurabilityTaint`] refusal: the taint is torn (unknown
///                       scope, never auto-healable), or at least one reference remains genuinely
///                       dangling after the walk. The taint is rewritten to record exactly the
///                       unresolved remainder (never the original full set, never nothing) and the
///                       gate is left standing.
pub fn run() -> Result<HealOutcome, CoreError> {
    let root = forklift_root();
    let state = taint_utils::read_taints(&root).map_err(|e| read_failure_refusal(&root, &e))?;

    if state.torn {
        return Err(torn_refusal(&root));
    }
    if state.recorded.is_empty() {
        return Ok(HealOutcome::nothing());
    }

    let attempt = heal_utils::attempt_restage_all(&root, &state.recorded);

    if attempt.all_clean() {
        heal_utils::finish_clean_heal(&root, &attempt.restaged, &state.files)
            .map_err(|e| sync_failure_refusal(&root, &e))?;
        return Ok(HealOutcome {
            was_tainted: true,
            restaged: display_paths(attempt.restaged.iter()),
            resolved: Vec::new(),
            notes: Vec::new(),
        });
    }

    // Lock in whatever DID restage cleanly this attempt, independent of how the rest resolves —
    // a successfully-restaged path's durability must never wait on the deeper analysis below.
    if !attempt.restaged.is_empty() {
        let parents: BTreeSet<PathBuf> = attempt.restaged.iter()
            .filter_map(|relative| root.join(relative).parent().map(Path::to_path_buf))
            .collect();
        heal_utils::sync_restaged_parents(&root, &parents).map_err(|e| sync_failure_refusal(&root, &e))?;
    }

    resolve_the_rest(&root, &state.recorded, &attempt, &state.files)
}

/// The deeper analysis: classify every path [`heal_utils::attempt_restage_all`] could not
/// restage, run the closure walk over whatever turned out to be genuinely missing, and rewrite
/// the taint to record exactly what remains dangling. Split out of [`run`] only for readability —
/// still called exactly once per invocation.
///
/// `taint_files` is [`run`]'s own `state.files` — the snapshot [`taint_utils::read_taints`]
/// returned before this whole analysis (including the closure walk, which can run for minutes)
/// began. It is threaded straight through to [`taint_utils::replace_taint_with_remainder`] so the
/// eventual rewrite deletes exactly the files that predate this call, never whatever the taint
/// directory holds by the time the walk finally finishes — see that function's doc comment.
fn resolve_the_rest(
    root: &Path,
    recorded: &BTreeSet<PathBuf>,
    attempt: &heal_utils::RestageAttempt,
    taint_files: &[PathBuf],
) -> Result<HealOutcome, CoreError> {
    let mut remainder: BTreeSet<PathBuf> = BTreeSet::new();
    let mut dangling_lines: Vec<String> = Vec::new();

    for (relative, error) in &attempt.unreadable {
        remainder.insert(relative.clone());
        dangling_lines.push(format!("unreadable: \"{}\" ({})", relative.to_string_lossy(), error));
    }
    for relative in &attempt.hash_mismatch {
        remainder.insert(relative.clone());
        dangling_lines.push(format!(
            "corrupt (content does not match its own hash): \"{}\"", relative.to_string_lossy()
        ));
    }
    for (relative, error) in &attempt.restage_failed {
        remainder.insert(relative.clone());
        dangling_lines.push(format!("could not be restaged: \"{}\" ({})", relative.to_string_lossy(), error));
    }

    let mut shard_vanished: Vec<PathBuf> = Vec::new();
    let mut loose_candidates: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut pack_stems: BTreeSet<PathBuf> = BTreeSet::new();

    for relative in &attempt.vanished {
        match classify_vanished(relative) {
            VanishedClass::Loose(hash) => { loose_candidates.insert(hash, relative.clone()); }
            VanishedClass::Shard => shard_vanished.push(relative.clone()),
            VanishedClass::PackData(stem) | VanishedClass::PackIndex(stem) => { pack_stems.insert(stem); }
            VanishedClass::Unrecognized => {
                remainder.insert(relative.clone());
                dangling_lines.push(format!(
                    "of a shape the recovery walk does not recognize and cannot auto-classify: \"{}\"",
                    relative.to_string_lossy()
                ));
            }
        }
    }

    // Resolve every vanished pack's stem: enumerate its surviving index, or escalate.
    let mut pack_candidates: BTreeMap<String, PathBuf> = BTreeMap::new();
    for stem in &pack_stems {
        let index_relative = stem.with_extension(pack_utils::PACK_INDEX_EXTENSION);
        let data_relative = stem.with_extension(pack_utils::PACK_DATA_EXTENSION);
        let index_absolute = root.join(&index_relative);

        let enumerated = if index_absolute.exists() {
            pack_utils::hashes_in_index_file(&index_absolute).ok()
        } else {
            None
        };

        match enumerated {
            Some(hashes) => {
                for hash in hashes {
                    pack_candidates.entry(hash).or_insert_with(|| data_relative.clone());
                }
            }
            None => {
                remainder.insert(data_relative.clone());
                if recorded.contains(&index_relative) {
                    remainder.insert(index_relative.clone());
                }
                dangling_lines.push(format!(
                    "pack \"{}\" is non-enumerable (its index is missing or unreadable), so its \
                    contents cannot be walked and it cannot be auto-healed",
                    data_relative.to_string_lossy()
                ));
            }
        }
    }

    // Raw-presence filter: a candidate present elsewhere (another pack, or loose) was never
    // actually lost — only what remains absent everywhere needs the closure walk at all.
    let mut truly_missing: BTreeSet<String> = BTreeSet::new();
    let mut resolved: Vec<String> = Vec::new();

    for hash in loose_candidates.keys().chain(pack_candidates.keys()) {
        match file_utils::raw_object_present(hash) {
            Ok(true) => resolved.push(hash.clone()),
            Ok(false) => { truly_missing.insert(hash.clone()); }
            Err(e) => return Err(walk_failure_refusal(root, &e)),
        }
    }

    let referenced = closure_references_any(&truly_missing).map_err(|e| walk_failure_refusal(root, &e))?;

    let remote_configured = remote_utils::RemoteClient::from_config().is_ok();

    for hash in &truly_missing {
        if referenced.contains(hash) {
            if let Some(path) = loose_candidates.get(hash) {
                remainder.insert(path.clone());
            }
            if let Some(pack_path) = pack_candidates.get(hash) {
                remainder.insert(pack_path.clone());
                let index_relative = pack_path.with_extension(pack_utils::PACK_INDEX_EXTENSION);
                if recorded.contains(&index_relative) {
                    remainder.insert(index_relative);
                }
            }
            dangling_lines.push(format!(
                "vanished and still referenced: \"{}\" ({})", hash, remedy_text(remote_configured)
            ));
        } else {
            resolved.push(hash.clone());
        }
    }

    let mut notes: Vec<String> = Vec::new();
    for relative in &shard_vanished {
        resolved.push(relative.to_string_lossy().into_owned());
        notes.push(format!(
            "the staged inventory shard \"{}\" is gone; if that staging state mattered, re-run \
            the load that produced it — a vanished shard is staging state, not an object \
            reference, so it never blocks clearing this taint",
            relative.to_string_lossy()
        ));
    }

    taint_utils::replace_taint_with_remainder(root, &remainder, taint_files)
        .map_err(|e| sync_failure_refusal(root, &e))?;

    if remainder.is_empty() {
        taint_utils::clear_gate(root);
        Ok(HealOutcome {
            was_tainted: true,
            restaged: display_paths(attempt.restaged.iter()),
            resolved,
            notes,
        })
    } else {
        Err(dangling_refusal(root, &dangling_lines))
    }
}

/// The three shapes a taint's recorded final path can ever take — see
/// [`file_utils::hash_from_object_path`]'s doc comment — plus an escape hatch for a shape none of
/// them recognize (never reached by any current write path, but a refusal beats a silent drop).
enum VanishedClass {
    /// A loose object; the hash its path encodes.
    Loose(String),
    /// A staged inventory shard file — not an object reference at all.
    Shard,
    /// A pack **data** file; its stem (the shared path prefix its `.idx` sibling shares).
    PackData(PathBuf),
    /// A pack **index** file; its stem.
    PackIndex(PathBuf),
    /// A shape none of the above recognizes.
    Unrecognized,
}

fn classify_vanished(relative: &Path) -> VanishedClass {
    if let Some(hash) = file_utils::hash_from_object_path(relative) {
        return VanishedClass::Loose(hash);
    }

    if relative.file_name().and_then(|name| name.to_str()) == Some(file_utils::FILE_NAME_INVENTORY_DATA) {
        return VanishedClass::Shard;
    }

    match relative.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if ext == pack_utils::PACK_DATA_EXTENSION => VanishedClass::PackData(relative.with_extension("")),
        Some(ext) if ext == pack_utils::PACK_INDEX_EXTENSION => VanishedClass::PackIndex(relative.with_extension("")),
        _ => VanishedClass::Unrecognized,
    }
}

/// Every durable ref source's roots for the closure walk (§3.2: "every durable ref source", never
/// the registration ledger) — see the module doc comment.
struct WalkRoots {
    /// Parcel hashes to walk ancestry-and-tree-closure from: every pallet head (both namespaces),
    /// every bay's parked parcels, every tag's subject parcel, every bay's in-progress
    /// consolidation `their_head`, and the trust anchor's adopted head (if any).
    parcels: Vec<String>,
    /// Object hashes a staged inventory shard (of any bay) references directly (never through a
    /// parcel), with the entry's type (so a chunked file's recipe is still descended into for its
    /// chunks).
    shard_referenced: Vec<(String, DirEntryType)>,
}

/// Collect every durable ref source's roots — see [`WalkRoots`].
///
/// **Warehouse-scale, not bay-scale.** The taint this walk exists to resolve is over the
/// *shared* object store (`forklift_root()`, invariant across every bay) — objects and pallet
/// refs are shared, but a bay's parked parcels, staged inventory shards and in-progress
/// consolidation are bay-*local* (`.forklift/bays/<name>/…`; the main tree keeps them directly
/// under `.forklift/`). Answering "is this shared object still referenced by *anything*
/// durable" from only the active bay's local state would under-count references and could clear
/// the taint on (and later let `gc` delete) an object a *different* bay still needs — silent
/// data loss. So every bay-local source below is read across every bay
/// (`bay_utils::all_bay_state_dirs`), never just the active one.
///
/// **Invariant with [`gc_utils::collect_live_set`](crate::util::gc_utils::collect_live_set):**
/// this walk's roots must stay a *superset* of gc's live-set roots (every pallet head, every
/// bay's parked parcels, every bay's in-progress consolidation `their_head`, and the shared
/// trust-anchor `adopts`) — plus every tag's subject and every bay's staged inventory shards,
/// sources gc deliberately does not root (tags are not a gc root today; an unstacked staged
/// shard is a pre-existing, accepted gc design choice, not a bug this walk needs to match). If
/// gc ever treats an object as live, this walk must never call the same object safe to drop —
/// otherwise `forklift heal` could clear a taint over an object `forklift gc` would refuse to
/// delete. The parked-parcels/consolidation portion of that shared root list is not duplicated
/// here — both this function and `collect_live_set` call
/// [`bay_utils::collect_bay_scoped_parcel_roots`] for it, so the two can never drift apart on
/// that portion by construction; **a future edit adding a new bay-local ref source should still
/// re-check both callers of the shared helper, and re-check the other function's root list for
/// anything not routed through it (tags, shards, the trust anchor).**
///
/// The undo journal is deliberately excluded from **both** walks (parity, not an oversight): an
/// entry there records history for `undo`/`redo`, never a live reference a future write would
/// resurrect from it.
///
/// Staged inventory shards are enumerated by walking the shard **files on disk**, never the
/// registration ledger (`inventory_utils::write_metadata_to_file`'s plain, unsynced
/// `std::fs::write`, which is blind to a shard published but never registered — the phase-B
/// wart §3.2 calls out by name; a walk that trusted the ledger could silently miss a real
/// dangling reference).
fn collect_walk_roots() -> Result<WalkRoots, String> {
    let mut parcels: Vec<String> = Vec::new();
    let mut shard_referenced: Vec<(String, DirEntryType)> = Vec::new();

    for (_, head) in pallet_utils::all_pallet_refs()? {
        parcels.push(head);
    }

    for tag in tag_utils::read_tags()? {
        parcels.push(tag.tag.subject);
    }

    // Bay-local sources, read across every bay — see this function's doc comment. Parked
    // parcels and in-progress-consolidation `their_head` are the portion shared with gc's live
    // set — see `bay_utils::collect_bay_scoped_parcel_roots`'s doc comment for the shared-helper
    // rationale and the fail-closed-on-an-unreadable-bay-source contract (by design, not a bug).
    // Staged inventory shards are recovery-only (gc deliberately does not root them) and stay a
    // separate per-bay loop here.
    parcels.extend(bay_utils::collect_bay_scoped_parcel_roots()?);

    for dir in bay_utils::all_bay_state_dirs()? {
        walk_shard_files(&dir.join(FOLDER_NAME_INVENTORY_ROOT), &mut shard_referenced)?;
    }

    // The trust anchor is shared (warehouse-global): read once, not per bay.
    if let Some(anchor) = office_utils::read_trust_anchor()? {
        if let Some(adopts) = anchor.adopts {
            parcels.push(adopts);
        }
    }

    Ok(WalkRoots { parcels, shard_referenced })
}

fn walk_shard_files(folder: &Path, hashes: &mut Vec<(String, DirEntryType)>) -> Result<(), String> {
    if !folder.exists() {
        return Ok(());
    }

    for entry_result in file_utils::read_directory(&folder.to_path_buf())? {
        let entry = entry_result.map_err(|e| format!("Error while reading directory entry: {}", e))?;
        let path = entry.path();

        if path.is_dir() {
            walk_shard_files(&path, hashes)?;
            continue;
        }

        if path.file_name().and_then(|name| name.to_str()) != Some(file_utils::FILE_NAME_INVENTORY_DATA) {
            continue;
        }

        let bytes = std::fs::read(&path)
            .map_err(|e| format!("Error while reading inventory shard \"{}\": {}", path.to_string_lossy(), e))?;
        let inventory = crate::parser::inventory::inventory_parser::parse_inventory(&bytes)
            .map_err(|e| format!("Error while parsing inventory shard \"{}\": {}", path.to_string_lossy(), e))?;

        for (_, item) in inventory.get_items() {
            hashes.push((item.hash.clone(), item.item_type));
        }
    }

    Ok(())
}

/// Walk every durable ref source's closure looking for `targets`, returning the subset actually
/// found referenced. Read-only: see the module doc comment and
/// [`tests::closure_walk_never_touches_a_barrier_or_a_dir_sync`].
pub(crate) fn closure_references_any(targets: &BTreeSet<String>) -> Result<BTreeSet<String>, String> {
    if targets.is_empty() {
        return Ok(BTreeSet::new());
    }

    let roots = collect_walk_roots()?;
    walk_closure_for(targets, &roots)
}

fn walk_closure_for(targets: &BTreeSet<String>, roots: &WalkRoots) -> Result<BTreeSet<String>, String> {
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    let mut visited_trees: HashSet<String> = HashSet::new();

    for (hash, item_type) in &roots.shard_referenced {
        check_leaf(hash, *item_type, targets, &mut referenced)?;
    }

    let mut visited_parcels: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = roots.parcels.iter().cloned().collect();

    while let Some(hash) = queue.pop_front() {
        if targets.contains(&hash) {
            referenced.insert(hash);
            continue;
        }
        if !visited_parcels.insert(hash.clone()) {
            continue;
        }

        let parcel = object_utils::load_parcel(&hash)?;
        walk_tree(&parcel.tree_hash, targets, &mut referenced, &mut visited_trees)?;

        for parent in parcel.parents {
            queue.push_back(parent);
        }
    }

    Ok(referenced)
}

fn walk_tree(
    tree_hash: &str,
    targets: &BTreeSet<String>,
    referenced: &mut BTreeSet<String>,
    visited_trees: &mut HashSet<String>,
) -> Result<(), String> {
    if targets.contains(tree_hash) {
        referenced.insert(tree_hash.to_string());
        return Ok(());
    }
    if !visited_trees.insert(tree_hash.to_string()) {
        return Ok(());
    }

    let tree = object_utils::load_tree(tree_hash)?;

    for (_, file) in tree.get_files() {
        check_leaf(&file.hash, file.item_type, targets, referenced)?;
    }

    for (_, subtree) in tree.get_subtrees() {
        walk_tree(&subtree.hash, targets, referenced, visited_trees)?;
    }

    Ok(())
}

/// Check one leaf entry (a tree's file entry, or a staged shard's item): if its own hash is a
/// target, record it; otherwise, for a chunked file, descend into its recipe's chunk list looking
/// for a target chunk hash (a chunk is reachable only *through* its recipe, never directly).
fn check_leaf(
    hash: &str,
    item_type: DirEntryType,
    targets: &BTreeSet<String>,
    referenced: &mut BTreeSet<String>,
) -> Result<(), String> {
    if targets.contains(hash) {
        referenced.insert(hash.to_string());
        return Ok(()); // Absent — nothing to descend into.
    }

    if item_type.is_chunked() {
        for chunk in object_utils::recipe_chunk_hashes(hash)? {
            if targets.contains(&chunk) {
                referenced.insert(chunk);
            }
        }
    }

    Ok(())
}

/// The remedies that actually exist for a vanished-and-referenced object — see
/// [`HEAVYWEIGHT_EXITS`]'s doc comment for why "abandon" is never named here: no ref class this
/// walk covers (a pallet head, a parked parcel, a tag subject) has an in-tool command to drop it.
fn remedy_text(remote_configured: bool) -> &'static str {
    if remote_configured {
        "re-fetch it from the configured remote (\"forklift lower\"), or reproduce it by \
        re-running the operation that created it if its content still exists in your working \
        tree (\"forklift load\" then \"forklift stack\") — there is no in-tool way to abandon a \
        single dangling reference yet"
    } else {
        "no remote is configured for this warehouse; reproduce it by re-running the operation \
        that created it if its content still exists in your working tree (\"forklift load\" \
        then \"forklift stack\") — there is no in-tool way to abandon a single dangling \
        reference yet"
    }
}

fn display_paths<'a>(paths: impl Iterator<Item = &'a PathBuf>) -> Vec<String> {
    paths.map(|p| p.to_string_lossy().into_owned()).collect()
}

fn torn_refusal(root: &Path) -> CoreError {
    let message = format!(
        "{} under \"{}\": its record is itself incomplete (a crash interrupted the write that \
        would have named every affected path), so the full scope of what needs restaging is \
        unknown — a torn taint can never be auto-healed. {}",
        taint_utils::GATE_TAINT_MARKER, root.to_string_lossy(), HEAVYWEIGHT_EXITS
    );
    CoreError::refusal(RefusalCode::DurabilityTaint, message, HEAVYWEIGHT_EXITS.to_string())
}

fn dangling_refusal(root: &Path, lines: &[String]) -> CoreError {
    let named: Vec<&String> = lines.iter().take(MAX_NAMED_DANGLING).collect();
    let overflow = lines.len().saturating_sub(named.len());

    let mut message = format!(
        "{} under \"{}\": {} reference(s) remain dangling after recovery: {}",
        taint_utils::GATE_TAINT_MARKER, root.to_string_lossy(), lines.len(),
        named.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("; "),
    );
    if overflow > 0 {
        message.push_str(&format!(" (and {} more)", overflow));
    }

    let next_step = format!(
        "Every dangling reference needs a heavyweight resolution: {} Re-run \"forklift heal\" \
        once you have resolved what you can; it reports exactly what is left.",
        HEAVYWEIGHT_EXITS
    );

    CoreError::refusal(RefusalCode::DurabilityTaint, message, next_step)
}

fn read_failure_refusal(root: &Path, error: &str) -> CoreError {
    CoreError::refusal(
        RefusalCode::DurabilityTaint,
        format!(
            "{} under \"{}\", but its record could not be read ({}); treating this warehouse as \
            unhealed rather than risk trusting unproven state.",
            taint_utils::GATE_TAINT_MARKER, root.to_string_lossy(), error
        ),
        "Check the taint directory's permissions and disk health, then run \"forklift heal\" again.",
    )
}

fn sync_failure_refusal(root: &Path, error: &str) -> CoreError {
    CoreError::refusal(
        RefusalCode::DurabilityTaint,
        format!(
            "{} under \"{}\": recovery made progress, but making it durable (or recording the \
            remainder) failed ({}). The taint is left standing so nothing is trusted \
            prematurely.",
            taint_utils::GATE_TAINT_MARKER, root.to_string_lossy(), error
        ),
        "Check disk health and run \"forklift heal\" again.",
    )
}

fn walk_failure_refusal(root: &Path, error: &str) -> CoreError {
    CoreError::refusal(
        RefusalCode::DurabilityTaint,
        format!(
            "{} under \"{}\": the recovery walk over this warehouse's durable references could \
            not complete ({}), so whether the remaining recorded paths are safe to drop could \
            not be determined.",
            taint_utils::GATE_TAINT_MARKER, root.to_string_lossy(), error
        ),
        "Check that every pallet head, parked parcel, tag and staged inventory shard is itself \
        readable, then run \"forklift heal\" again.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::file_utils;

    #[test]
    fn closure_walk_never_touches_a_barrier_or_a_dir_sync() {
        // Pins the read-only-by-construction claim in this module's doc comment: the walk must
        // never persist anything, however many (or few) targets it is asked about, and however
        // many ref roots exist. Mutation: route the walk through a persisting `graph_utils` entry
        // point (or any write path) → red.
        //
        // Enters a real scope with a real ref source (a pallet head over a real, stored tree) so
        // the walk actually runs against something — with no scope entered at all (the previous
        // shape of this test), the walk read nothing and the assertion below was vacuous: it
        // would still pass with the walk routed through a barrier, as long as that code path was
        // never reached because there was nothing to walk.
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::globals::StorageRootScope;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-walk-readonly-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        let mut tree_object = LooseObjectBuilder::build_tree(&tree);
        tree_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("base".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        pallet_utils::set_pallet_head("main", &parcel_object.hash).unwrap();

        // Thread-local sync recorders, NOT the process-wide `barrier_count()`/`dir_sync_count()`:
        // those are global atomics a test on another thread bumps mid-walk, which made this test
        // flaky under parallel `cargo test`. A durability barrier always ends by fsyncing its
        // touched directories, and an immediate / graph-self-heal write fsyncs via `sync_dir`, so
        // "no directory sync attempted on THIS thread during the walk" is equivalent to "no barrier
        // ran" — with zero cross-test pollution. Armed here, after the setup writes above, so only
        // the walk's own (non-)syncs are recorded; both guards RAII-reset on drop.
        let _sync_dir_guard = file_utils::SyncDirFaultGuard::recording();
        let _barrier_dir_guard = file_utils::DirSyncFaultGuard::recording();

        let targets: BTreeSet<String> = ["a".repeat(64), "b".repeat(64)].into_iter().collect();
        let referenced = closure_references_any(&targets)
            .expect("the walk must succeed against a real, readable ref source");
        assert!(referenced.is_empty(), "neither target hash is actually referenced");

        assert!(file_utils::sync_dir_attempts().is_empty(),
            "the closure walk must never fsync a directory (immediate / graph-self-heal path): {:?}",
            file_utils::sync_dir_attempts());
        assert!(file_utils::dir_sync_attempts().is_empty(),
            "the closure walk must never run a durability barrier (caught via its trailing dir sync): {:?}",
            file_utils::dir_sync_attempts());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// (i) A staged shard planted in bay "b" (never the active bay — no bay context is entered;
    /// every read here is path-based) references a target hash the closure walk must still find.
    /// Pins `collect_walk_roots`'s per-bay staged-shard enumeration. Red without it: the
    /// pre-fix walk only ever read the active bay's inventory (`bay_root()`, which with no
    /// active bay is `forklift_root()` itself — never bay "b"'s), so the shard planted here was
    /// invisible and `referenced` would come back empty.
    #[test]
    fn walk_finds_a_staged_shard_in_a_non_active_bay() {
        use crate::builder::inventory::InventoryBuilder;
        use crate::enums::inventory_item_state::InventoryItemState;
        use crate::globals::StorageRootScope;
        use crate::model::inventory::{Inventory, InventoryItem};

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-bay-shard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let target_hash = "d".repeat(64);

        let mut inventory = Inventory::new();
        inventory.add_item(InventoryItem {
            metadata_change_timestamp: 0,
            content_change_timestamp: 0,
            device: 0,
            inode: 0,
            item_type: DirEntryType::Normal,
            user_id: 0,
            group_id: 0,
            file_size: 0,
            hash: target_hash.clone(),
            file_name_length: "file.txt".len() as u64,
            state: InventoryItemState::Normal,
            name: "file.txt".to_string(),
        });
        let bytes = InventoryBuilder::build(&inventory);

        // Bay "b"'s staged inventory, planted directly by path — mirrors the real on-disk shape
        // (`<bay-state-dir>/inventory/inv_/data`) without ever entering a bay context.
        let shard_path = bay_utils::bay_state_dir("b")
            .join(FOLDER_NAME_INVENTORY_ROOT)
            .join(file_utils::PREFIX_INVENTORY_FOLDER)
            .join(file_utils::FILE_NAME_INVENTORY_DATA);
        std::fs::create_dir_all(shard_path.parent().unwrap()).unwrap();
        std::fs::write(&shard_path, bytes).unwrap();

        let targets: BTreeSet<String> = [target_hash.clone()].into_iter().collect();
        let referenced = closure_references_any(&targets).unwrap();

        assert!(referenced.contains(&target_hash),
            "a shard staged in a non-active bay must still be found by the closure walk");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// (ii) A parked hash planted in bay "b"'s `parked` file must still be found. Pins
    /// `collect_walk_roots`'s per-bay `read_parked_in` enumeration. Red without it: the pre-fix
    /// walk called `park_utils::read_parked()` once, scoped to the active bay only, so bay "b"'s
    /// parked hash was never a root.
    #[test]
    fn walk_finds_a_parked_hash_in_a_non_active_bay() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-bay-parked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let target_hash = "e".repeat(64);

        let bay_b_dir = bay_utils::bay_state_dir("b");
        std::fs::create_dir_all(&bay_b_dir).unwrap();
        std::fs::write(bay_b_dir.join("parked"), format!("{}\n", target_hash)).unwrap();

        let targets: BTreeSet<String> = [target_hash.clone()].into_iter().collect();
        let referenced = closure_references_any(&targets).unwrap();

        assert!(referenced.contains(&target_hash),
            "a parked hash in a non-active bay must still be found by the closure walk");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Regression lock for the fail-closed contract on
    /// `bay_utils::collect_bay_scoped_parcel_roots` (see its doc comment): a malformed `parked`
    /// file in a *non-active* bay must make the closure walk (and so `run`, `forklift heal`) fail
    /// outright, never silently skip the unreadable bay and proceed as if it had no references —
    /// that would re-open the exact under-counting bug the bay-scope fix closed. Red if
    /// `read_parked_in`'s `?` in `collect_bay_scoped_parcel_roots`
    /// (crates/forklift-core/src/util/bay_utils.rs) were ever changed to skip-and-continue on an
    /// `Err` instead of propagating it.
    #[test]
    fn walk_fails_closed_on_an_unreadable_bay_parked_file() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-bay-unreadable-parked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        // Bay "b"'s `parked` file is malformed (not 64 hex chars) — `read_parked_in` errors on it.
        let bay_b_dir = bay_utils::bay_state_dir("b");
        std::fs::create_dir_all(&bay_b_dir).unwrap();
        std::fs::write(bay_b_dir.join("parked"), b"not-a-valid-hash\n").unwrap();

        let targets: BTreeSet<String> = ["a".repeat(64)].into_iter().collect();
        let result = closure_references_any(&targets);

        assert!(result.is_err(),
            "an unreadable/malformed bay ref source must fail the closure walk, not be skipped");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// (iii) A consolidation `their_head` planted in bay "b"'s `consolidation` file must still be
    /// found. Pins `collect_walk_roots`'s addition of the consolidation source (per-bay). Red
    /// without it: pre-fix, `collect_walk_roots` never read consolidation state at all — this
    /// source did not exist in the walk yet.
    #[test]
    fn walk_finds_a_consolidation_their_head_in_a_non_active_bay() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-bay-consolidation-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let target_hash = "f".repeat(64);

        let bay_b_dir = bay_utils::bay_state_dir("b");
        std::fs::create_dir_all(&bay_b_dir).unwrap();
        std::fs::write(bay_b_dir.join("consolidation"), format!("{}\ntheir-pallet\n", target_hash)).unwrap();

        let targets: BTreeSet<String> = [target_hash.clone()].into_iter().collect();
        let referenced = closure_references_any(&targets).unwrap();

        assert!(referenced.contains(&target_hash),
            "a consolidation their_head in a non-active bay must still be found by the closure walk");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// (iv) A trust-anchor `adopts` hash (shared, warehouse-global — no bay involved) must still
    /// be found. Pins `collect_walk_roots`'s addition of the trust-anchor source. Red without it:
    /// pre-fix, `collect_walk_roots` never read the trust anchor at all.
    #[test]
    fn walk_finds_a_trust_anchor_adopts_hash() {
        use crate::globals::StorageRootScope;
        use crate::util::office_utils::{self, TrustAnchor};

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-trust-adopts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let target_hash = "1".repeat(64);

        office_utils::write_trust_anchor(&TrustAnchor {
            genesis: "0".repeat(64),
            enabled_at: 0,
            boundary: Vec::new(),
            prior_genesis: None,
            adopts: Some(target_hash.clone()),
        }).unwrap();

        let targets: BTreeSet<String> = [target_hash.clone()].into_iter().collect();
        let referenced = closure_references_any(&targets).unwrap();

        assert!(referenced.contains(&target_hash),
            "a trust-anchor adopts hash must be found by the closure walk (a re-genesis GC root)");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn classify_vanished_recognizes_every_shape_the_taint_schema_can_record() {
        assert!(matches!(
            classify_vanished(Path::new("objects/ab/cdef1234567890")),
            VanishedClass::Loose(hash) if hash == "abcdef1234567890"
        ));
        assert!(matches!(
            classify_vanished(Path::new("inventory/inv_/inv_src/data")),
            VanishedClass::Shard
        ));
        assert!(matches!(
            classify_vanished(Path::new("objects/pack/abc123.pack")),
            VanishedClass::PackData(stem) if stem == Path::new("objects/pack/abc123")
        ));
        assert!(matches!(
            classify_vanished(Path::new("objects/pack/abc123.idx")),
            VanishedClass::PackIndex(stem) if stem == Path::new("objects/pack/abc123")
        ));
        assert!(matches!(
            classify_vanished(Path::new("something/unexpected.txt")),
            VanishedClass::Unrecognized
        ));
    }

    #[test]
    fn shard_referenced_hash_is_found_even_when_never_registered_in_the_ledger() {
        // The phase-B "published but unregistered shard" shape (§3.2): a shard file exists on
        // disk referencing an object, but the (non-atomic, unsynced) registration ledger
        // (`inventory_utils::write_metadata_to_file`) was never updated to mention it. The
        // closure walk must still find the reference — it enumerates shard files on disk, never
        // the ledger. Mutation: enumerate shards from the ledger instead of disk → red (the
        // ledger here is deliberately left absent).
        use crate::builder::inventory::InventoryBuilder;
        use crate::enums::inventory_item_state::InventoryItemState;
        use crate::globals::StorageRootScope;
        use crate::model::inventory::{Inventory, InventoryItem};

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-shard-only-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let target_hash = "c".repeat(64);

        let mut inventory = Inventory::new();
        inventory.add_item(InventoryItem {
            metadata_change_timestamp: 0,
            content_change_timestamp: 0,
            device: 0,
            inode: 0,
            item_type: DirEntryType::Normal,
            user_id: 0,
            group_id: 0,
            file_size: 0,
            hash: target_hash.clone(),
            file_name_length: "file.txt".len() as u64,
            state: InventoryItemState::Normal,
            name: "file.txt".to_string(),
        });

        let bytes = InventoryBuilder::build(&inventory);
        let shard_path = file_utils::get_inventory_data_path_for_key("src");
        std::fs::create_dir_all(shard_path.parent().unwrap()).unwrap();
        std::fs::write(&shard_path, bytes).unwrap();

        // The registration ledger was never written at all — proving there is nothing there
        // for a ledger-based enumeration to have read.
        let (_, metadata) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        assert!(metadata.is_none(), "the registration ledger must stay untouched by this test");

        let targets: BTreeSet<String> = [target_hash.clone()].into_iter().collect();
        let referenced = closure_references_any(&targets).unwrap();

        assert!(referenced.contains(&target_hash),
            "a shard-only reference (absent from the ledger) must still be found by the walk");
    }
}
