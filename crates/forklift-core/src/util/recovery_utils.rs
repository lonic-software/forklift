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
//! Presence is checked with [`file_utils::raw_object_present`], the one sanctioned gate-free
//! presence check — see its own doc comment for why bypassing the gate is safe specifically here.
//! [`tests::closure_walk_never_touches_a_barrier_or_a_dir_sync`] is the actual enforcer, not this
//! paragraph.
//!
//! ## Presence-tolerant descent (invariant I3)
//!
//! In a sparse/narrowed or shallow warehouse, an object this walk would otherwise need to descend
//! into can be legitimately absent (sealed by a signed hash, never fetched) rather than lost. Every
//! *descent* point in [`walk_closure_for`]/`walk_tree`/`check_leaf` — a parent parcel, a subtree,
//! and a chunked leaf's recipe (the objects this walk would otherwise load and recurse through) —
//! therefore checks [`file_utils::raw_object_present`] **after** that node's own targets-check (a
//! vanished *target* must still be reported as referenced — see
//! [`tests::a_vanished_pallet_head_that_is_itself_the_target_is_still_reported_referenced`]),
//! **unconditionally in both modes below**, and on absence skips descending — it does **not**
//! error. This is sound to *clear* on, not merely tolerate: `collect_walk_roots`'s own doc comment
//! establishes that this walk's root set is a superset of
//! [`crate::util::gc_utils::collect_live_set`]'s, so with identical tolerance any target this walk
//! calls unreferenced is unreferenced under gc's smaller root set too — gc is already entitled to
//! collect it, so a heal that instead kept it tainted forever would brick every command over an
//! object the store's own collector calls garbage. A **present**-but-unloadable object still fails
//! loud (the ordinary `?` after the presence check returns `true`) — tolerance is for absence only,
//! never for corruption.
//!
//! A **terminal** leaf — a plain (blob) file entry, or one chunk hash out of a *present* recipe's
//! chunk list — has no further descent to skip: the walk never loads a blob's or a chunk's bytes
//! either way. Its presence check exists purely to *feed the sink* (see below), so it is gated on
//! the sink actually being present (`Option::is_some`) and skipped — not merely no-opped, the
//! syscall itself is skipped — when there is nothing to feed.
//!
//! The sink is `Option<&mut dyn FnMut(&str)>`: `None` for the common, targeted walk
//! ([`closure_references_any`]) — descent-guard absences are simply skipped, and the (gated)
//! terminal-leaf presence check never runs at all, so the targeted walk pays no per-leaf/per-chunk
//! stat it has no use for. `Some(collector)` for the targetless enumerator
//! ([`enumerate_absent_reachable`]) — every absence found, at every level including terminal
//! leaves and chunks, is recorded. Both share [`walk_closure_for`]'s exact descent (one code path,
//! two modes, so they can never drift apart); [`enumerate_absent_reachable`] additionally passes an
//! **empty target set**, so every node's targets-check is vacuous and every node's descent-guard is
//! actually reached.
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
    /// resolved without a rewrite: proven absent *and* unreferenced by the closure walk, present
    /// in a pack despite a vanished loose dentry (I4, `heal_utils::RestageOutcome::RecoveredPacked`),
    /// or (for a shard) a staging concern that carries no object-trust risk at all.
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
            // I4: a pack-recovered path was never rewritten — it belongs in `resolved`, exactly
            // like a vanished-and-unreferenced hash, not in `restaged`.
            resolved: display_paths(attempt.recovered_packed.iter()),
            notes: Vec::new(),
        });
    }

    // Lock in whatever DID restage cleanly this attempt, independent of how the rest resolves —
    // a successfully-restaged path's durability must never wait on the deeper analysis below.
    if !attempt.restaged.is_empty() {
        let parents: BTreeSet<PathBuf> = attempt.restaged.iter()
            .filter_map(|relative| root.join(relative).parent().map(Path::to_path_buf))
            .collect();
        heal_utils::sync_restaged_parents(&parents).map_err(|e| sync_failure_refusal(&root, &e))?;
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
    // I4: a pack-recovered path was never rewritten and never entered `attempt.vanished` in the
    // first place (`restage_object` resolved it directly) — it belongs in `resolved` alongside
    // the vanished-and-unreferenced hashes this loop finds below, not in `remainder`.
    let mut resolved: Vec<String> = display_paths(attempt.recovered_packed.iter());

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
/// that portion by construction. That helper takes the bay dirs as a parameter rather than
/// enumerating them itself specifically so a caller that also needs those dirs for something
/// else — this function, for staged inventory shards — can enumerate them once
/// ([`bay_utils::all_bay_state_dirs`]) and feed the same `Vec` to both the helper and its own
/// extra loop, instead of `bay_utils::list_bays` running twice per heal. **A future edit adding a
/// new bay-local ref source should still re-check both callers of the shared helper, and re-check
/// the other function's root list for anything not routed through it (tags, shards, the trust
/// anchor).**
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

    // Bay-local sources, read across every bay — see this function's doc comment. One
    // `all_bay_state_dirs` enumeration, fed to both the shared parked/consolidation helper and
    // this function's own staged-shard loop, so `list_bays` runs exactly once per heal. Parked
    // parcels and in-progress-consolidation `their_head` are the portion shared with gc's live
    // set — see `bay_utils::collect_bay_scoped_parcel_roots`'s doc comment for the shared-helper
    // rationale and the fail-closed-on-an-unreadable-bay-source contract (by design, not a bug).
    // Staged inventory shards are recovery-only (gc deliberately does not root them) and stay a
    // separate per-bay loop here.
    let bay_dirs = bay_utils::all_bay_state_dirs()?;
    parcels.extend(bay_utils::collect_bay_scoped_parcel_roots(&bay_dirs)?);

    for dir in &bay_dirs {
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
///
/// Presence-tolerant (I3) — see the module doc comment. Passes `None` for the sink: this call only
/// ever cares whether a target is referenced, never which hashes the walk found absent along the
/// way, and the `None` also gates off the terminal-leaf/chunk presence stats this call has no use
/// for (the descent guards — parcel, subtree, chunked-leaf's recipe — still run regardless).
pub(crate) fn closure_references_any(targets: &BTreeSet<String>) -> Result<BTreeSet<String>, String> {
    if targets.is_empty() {
        return Ok(BTreeSet::new());
    }

    let roots = collect_walk_roots()?;
    walk_closure_for(targets, &roots, None)
}

/// The targetless sibling of [`closure_references_any`]: enumerate every hash the walk finds
/// raw-absent (a root ref itself, a parent parcel, a subtree boundary, an absent recipe, a raw-
/// absent chunk under a present recipe, or a plain leaf), without ever descending past one. Built
/// for a later slice (the torn-taint rescan, which needs a *targetless* enumerator — a torn taint
/// has no `targets` set to drive [`closure_references_any`] with) and not wired into any command
/// yet; implemented and tested now so this collecting path is exercised rather than dead code.
///
/// Shares [`walk_closure_for`]'s exact descent, with an **empty** target set — every node then
/// falls through past its (vacuous) targets-check to its presence guard — and `Some` sink, which
/// both collects every absence found and (per the module doc comment) turns on the terminal-
/// leaf/chunk presence checks that [`closure_references_any`] skips entirely. Same descent, so this
/// can never tolerate (or fail to tolerate) anything [`closure_references_any`] doesn't.
///
/// # Returns
/// * `Ok(BTreeSet<String>)` - Every raw-absent hash the walk reached, recorded once each.
/// * `Err(String)`          - A ref source could not be read, or a *present* object could not be
///                            loaded (corrupt/unreadable) — the walk still fails loud on those.
// Not called from production code yet (see the doc comment above) — only from this module's own
// tests, which is exactly what this slice needs to prove the collecting path works. Silences the
// otherwise-genuine `dead_code` lint on a non-test build; remove this attribute in the slice that
// wires this into the torn-taint rescan.
#[allow(dead_code)]
pub(crate) fn enumerate_absent_reachable() -> Result<BTreeSet<String>, String> {
    let roots = collect_walk_roots()?;
    let empty_targets: BTreeSet<String> = BTreeSet::new();
    let mut absent: BTreeSet<String> = BTreeSet::new();
    {
        let mut collecting_sink = |hash: &str| { absent.insert(hash.to_string()); };
        walk_closure_for(&empty_targets, &roots, Some(&mut collecting_sink))?;
    }
    Ok(absent)
}

/// Re-borrow an `Option<&mut dyn FnMut(&str)>` for one call, without moving the original out of
/// its owning local variable — needed because `Option<&mut dyn FnMut(&str)>` is used repeatedly
/// across loop iterations and nested calls in [`walk_closure_for`]/`walk_tree`/`check_leaf`, and
/// the generic `Option::as_deref_mut` does not shrink a `&mut &mut dyn Trait`'s lifetime down to
/// just the one call the way this concrete, non-generic match does (a known reborrowing gap for
/// generic `DerefMut` blanket impls over nested mutable references).
fn reborrow_sink<'a>(sink: &'a mut Option<&mut dyn FnMut(&str)>) -> Option<&'a mut dyn FnMut(&str)> {
    match sink {
        Some(s) => Some(&mut **s),
        None => None,
    }
}

/// The shared descent core both [`closure_references_any`] and [`enumerate_absent_reachable`] call
/// (I3, module doc comment). `sink`:
/// * `None` — the targeted walk: a descent-guard absence (parcel/subtree/recipe) is simply skipped,
///   and the terminal-leaf/chunk presence checks are skipped entirely (not merely no-opped) since
///   nothing would consume their result.
/// * `Some(collector)` — the enumerating walk: every absence found, at every level, is recorded via
///   `collector`.
///
/// `targets` empty (as [`enumerate_absent_reachable`] passes) means every node's targets-check is
/// vacuous, so the walk falls through to (and records at) every presence guard it meets.
fn walk_closure_for(
    targets: &BTreeSet<String>,
    roots: &WalkRoots,
    mut sink: Option<&mut dyn FnMut(&str)>,
) -> Result<BTreeSet<String>, String> {
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    let mut visited_trees: HashSet<String> = HashSet::new();

    for (hash, item_type) in &roots.shard_referenced {
        check_leaf(hash, *item_type, targets, &mut referenced, reborrow_sink(&mut sink))?;
    }

    let mut visited_parcels: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = roots.parcels.iter().cloned().collect();

    while let Some(hash) = queue.pop_front() {
        // Targets-check FIRST: a vanished hash that is itself a root ref (a pallet head, a parked
        // parcel, a tag subject, …) is still a genuine reference to it — the presence guard below
        // must never pre-empt this, or a root reference to a raw-absent target would be silently
        // dropped instead of reported. See
        // [`tests::a_vanished_pallet_head_that_is_itself_the_target_is_still_reported_referenced`].
        if targets.contains(&hash) {
            referenced.insert(hash);
            continue;
        }
        if !visited_parcels.insert(hash.clone()) {
            continue;
        }
        // Presence-tolerant (I3): a parent parcel reached by ancestry can be legitimately absent
        // (sparse/shallow/narrowed) — record it via `sink` (if any) and stop; do not enqueue its
        // parents, since an absent parcel's own parent list cannot be read. Unconditional in both
        // modes — this is a descent guard, not a sink-feeding-only stat.
        if !file_utils::raw_object_present(&hash)? {
            if let Some(s) = reborrow_sink(&mut sink) { s(&hash); }
            continue;
        }

        let parcel = object_utils::load_parcel(&hash)?;
        walk_tree(&parcel.tree_hash, targets, &mut referenced, &mut visited_trees, reborrow_sink(&mut sink))?;

        for parent in parcel.parents {
            queue.push_back(parent);
        }
    }

    Ok(referenced)
}

/// Iterative, work-queue-driven — a parcel's spine tree can nest arbitrarily deep, and the
/// obvious recursive shape (one stack frame per level) risks a stack overflow on a
/// pathologically deep tree; see [`tests::walk_tree_survives_a_pathologically_deep_tree`]. Mirrors
/// `gc_utils::collect_live_set`'s own `tree_queue: VecDeque` loop — same shape, same semantics,
/// just applied here instead of there. `check_leaf`'s own descent (recipe → chunks) is one level
/// deep, never further, so it stays recursion-free without needing this treatment.
fn walk_tree(
    tree_hash: &str,
    targets: &BTreeSet<String>,
    referenced: &mut BTreeSet<String>,
    visited_trees: &mut HashSet<String>,
    mut sink: Option<&mut dyn FnMut(&str)>,
) -> Result<(), String> {
    let mut tree_queue: VecDeque<String> = VecDeque::new();
    tree_queue.push_back(tree_hash.to_string());

    while let Some(tree_hash) = tree_queue.pop_front() {
        // Targets-check FIRST — same reasoning as `walk_closure_for`'s parcel queue: a vanished
        // hash that is itself a hunted target is a reference, and must be reported as one even
        // though it is raw-absent.
        if targets.contains(&tree_hash) {
            referenced.insert(tree_hash);
            continue;
        }
        if !visited_trees.insert(tree_hash.clone()) {
            continue;
        }
        // Presence-tolerant (I3): a sealed subtree boundary in a sparse/shallow warehouse is
        // legitimately absent — record it (if a sink is listening) and stop; nothing beneath it is
        // locally knowable (the store invariant a warehouse never holds a child without its parent
        // tree, mirrored from `gc_utils::collect_live_set`'s own tree descent). Unconditional in
        // both modes — a descent guard, not a sink-feeding-only stat.
        // Presence-tolerant (I3): a sealed subtree boundary in a sparse/shallow warehouse is
        // legitimately absent — record it (if a sink is listening) and stop; nothing beneath it is
        // locally knowable (the store invariant a warehouse never holds a child without its parent
        // tree, mirrored from `gc_utils::collect_live_set`'s own tree descent). Unconditional in
        // both modes — a descent guard, not a sink-feeding-only stat.
        if !file_utils::raw_object_present(&tree_hash)? {
            if let Some(s) = reborrow_sink(&mut sink) { s(&tree_hash); }
            continue;
        }

        let tree = object_utils::load_tree(&tree_hash)?;

        for (_, file) in tree.get_files() {
            check_leaf(&file.hash, file.item_type, targets, referenced, reborrow_sink(&mut sink))?;
        }

        for (_, subtree) in tree.get_subtrees() {
            tree_queue.push_back(subtree.hash.clone());
        }
    }

    Ok(())
}

/// Check one leaf entry (a tree's file entry, or a staged shard's item): if its own hash is a
/// target, record it; otherwise, for a chunked file, descend into its recipe's chunk list looking
/// for a target chunk hash (a chunk is reachable only *through* its recipe, never directly).
///
/// Presence-tolerant (I3): a **chunked** leaf's recipe can itself be raw-absent (sparse/shallow) —
/// its chunks are then locally absent too (mirrors `gc_utils::mark_recipe_chunks_live`), so an
/// absent recipe is recorded (if a sink is listening) and its chunk list is never fetched. This
/// recipe-presence guard is a real descent guard (skips the `recipe_chunk_hashes` load) and runs
/// **unconditionally in both modes**, exactly like the parcel/subtree guards.
///
/// The two *terminal* checks below it are different in kind: a **present** recipe's individual
/// chunk hashes, and a **plain** (blob) leaf's own hash, are never loaded by this walk either way
/// (only ever compared against `targets`) — there is no descent for a presence check to gate. So
/// each is gated on `sink.is_some()` and skipped entirely (not merely no-opped — the
/// `raw_object_present` syscall itself is skipped) for the targeted walk
/// ([`closure_references_any`], `sink: None`), and only actually runs for the enumerator
/// ([`enumerate_absent_reachable`], `sink: Some(_)`) — see the module doc comment.
fn check_leaf(
    hash: &str,
    item_type: DirEntryType,
    targets: &BTreeSet<String>,
    referenced: &mut BTreeSet<String>,
    mut sink: Option<&mut dyn FnMut(&str)>,
) -> Result<(), String> {
    if targets.contains(hash) {
        referenced.insert(hash.to_string());
        return Ok(());
    }

    if item_type.is_chunked() {
        if !file_utils::raw_object_present(hash)? {
            if let Some(s) = reborrow_sink(&mut sink) { s(hash); }
            return Ok(());
        }
        for chunk in object_utils::recipe_chunk_hashes(hash)? {
            // Targets-check first (same reasoning as every other node): a target chunk is
            // recorded as referenced regardless of mode.
            if targets.contains(&chunk) {
                referenced.insert(chunk.clone());
            }
            // Enumerating-mode only (gated on the sink, not merely no-opped): the targeted walk
            // never needs a chunk's raw presence, only whether it is a target (handled above).
            if let Some(s) = reborrow_sink(&mut sink) {
                if !file_utils::raw_object_present(&chunk)? {
                    s(&chunk);
                }
            }
        }
    } else if let Some(s) = reborrow_sink(&mut sink) {
        // Enumerating-mode only, same reasoning: a plain leaf's bytes are never loaded by this
        // walk either way.
        if !file_utils::raw_object_present(hash)? {
            s(hash);
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

        // The enumerating mode (`enumerate_absent_reachable`, the §8.3 hook) shares the exact same
        // descent — pin that it is equally read-only, not just the no-op-sink mode above. Same
        // recording guards, still armed; any sync attempt from either call would show up here.
        let absent = enumerate_absent_reachable()
            .expect("the enumerating walk must succeed against a real, readable ref source");
        assert!(absent.is_empty(), "nothing in this fixture is raw-absent");

        assert!(file_utils::sync_dir_attempts().is_empty(),
            "the enumerating walk must never fsync a directory either: {:?}",
            file_utils::sync_dir_attempts());
        assert!(file_utils::dir_sync_attempts().is_empty(),
            "the enumerating walk must never run a durability barrier either: {:?}",
            file_utils::dir_sync_attempts());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Shared fixture for the I3 (presence-tolerant descent) tests: a pallet head "main" whose
    /// parcel's spine tree has exactly one subtree child pointing at a hash that is never actually
    /// stored anywhere (`absent_subtree_hash`) — the present-parent / absent-child boundary shape
    /// §8.1 exists to tolerate (a sealed hash committed to a signed tree, never fetched into this
    /// warehouse). Must be called after a `StorageRootScope` is entered. Returns the absent
    /// subtree's hash.
    fn plant_present_parent_with_absent_subtree_boundary() -> String {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;

        let absent_subtree_hash = "a".repeat(64);

        let mut root_tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        root_tree.add_child(TreeItem::new(
            "sub".to_string(), absent_subtree_hash.clone(), DirEntryType::Tree,
        ));
        let mut tree_object = LooseObjectBuilder::build_tree(&root_tree);
        tree_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("boundary".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        pallet_utils::set_pallet_head("main", &parcel_object.hash).unwrap();

        absent_subtree_hash
    }

    /// (I3, test 1) A target only theoretically reachable *through* an absent subtree boundary
    /// must be reported unreferenced, and the walk must complete without erroring on that boundary
    /// — the tolerant-descent behavior this slice adds. `target_hash` can never actually be linked
    /// from the absent subtree (there are no bytes to link from), which is exactly the point: the
    /// walk cannot know, and must not assume, anything about what an absent object might have
    /// pointed to — it just stops at the boundary. Mutation: revert `walk_tree`'s presence guard
    /// (recovery_utils.rs, before `object_utils::load_tree`) back to a bare `?` on
    /// `load_tree(&tree_hash)` → `load_tree` errors on the absent subtree (object does not exist)
    /// → `closure_references_any` returns `Err` → `.expect(...)` panics → red.
    #[test]
    fn tolerant_walk_clears_a_target_only_reachable_through_an_absent_subtree_boundary() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-i3-boundary-clears-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        plant_present_parent_with_absent_subtree_boundary();

        let target_hash = "b".repeat(64);
        let targets: BTreeSet<String> = [target_hash.clone()].into_iter().collect();

        let referenced = closure_references_any(&targets)
            .expect("a sealed/absent subtree boundary must be skipped, not fail the whole walk");
        assert!(!referenced.contains(&target_hash),
            "T is not actually reachable — its only theoretical path runs through the absent subtree");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// (I3, test 2 — gc-consistency) Pins the superset invariant `collect_walk_roots`'s doc
    /// comment establishes: on the exact same boundary fixture as the test above, `gc_utils`'s
    /// independently-implemented live-set walk must also treat the unreachable target as excluded.
    /// If heal's tolerance (this module) ever diverges from gc's own presence tolerance
    /// (`gc_utils::collect_live_set`), this reddens — heal must never clear (or keep tainted) an
    /// object gc disagrees about.
    #[test]
    fn gc_consistency_pins_the_same_absent_subtree_boundary() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-i3-gc-consistency-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        plant_present_parent_with_absent_subtree_boundary();

        let target_hash = "b".repeat(64);
        let targets: BTreeSet<String> = [target_hash.clone()].into_iter().collect();
        let referenced = closure_references_any(&targets).unwrap();
        assert!(!referenced.contains(&target_hash));

        let live = crate::util::gc_utils::collect_live_set().unwrap();
        assert!(!live.contains(&target_hash),
            "gc's own live set must also exclude a target only reachable through an absent \
            subtree boundary — heal's root set is a superset of gc's, so anything heal's \
            tolerant walk calls unreferenced must be unreferenced under gc's smaller root set too");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// (I3, test 3 — targets-before-presence) A vanished object that is itself a hunted target
    /// (here, a pallet head whose parcel was never actually stored) must still be reported
    /// referenced: a root reference to a raw-absent object is a genuine reference, and the
    /// presence guard must never pre-empt the targets-check that catches it. Mutation: swap the
    /// order in `walk_closure_for`'s parcel-queue loop so the `raw_object_present` guard runs
    /// *before* `targets.contains(&hash)` → the vanished target is skipped via `sink` and `continue`
    /// before ever being checked against `targets` → `referenced` does not contain it → red.
    #[test]
    fn a_vanished_pallet_head_that_is_itself_the_target_is_still_reported_referenced() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-i3-targets-before-presence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let vanished_head_hash = "e".repeat(64);
        pallet_utils::set_pallet_head("main", &vanished_head_hash).unwrap();

        let targets: BTreeSet<String> = [vanished_head_hash.clone()].into_iter().collect();
        let referenced = closure_references_any(&targets).unwrap();

        assert!(referenced.contains(&vanished_head_hash),
            "a vanished pallet head that is itself a hunted target must still be reported referenced");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// (I3, test 4 — present-but-corrupt still fails loud) A pallet head naming a hash that IS
    /// present on disk (`raw_object_present` → true) but whose stored bytes do not actually hash to
    /// it — present-but-unloadable, the opposite case from raw-absent. The presence guard must
    /// never turn this into a silent skip: once presence says "yes," the walk still runs the
    /// ordinary `?` load, and a corrupt object still fails the whole walk loudly, exactly as it did
    /// before this slice (and exactly as `gc_utils` still fails loud on a present-but-corrupt
    /// object after its own presence check).
    #[test]
    fn a_present_but_corrupt_pallet_head_fails_the_walk_loudly() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-i3-corrupt-fails-loud-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let corrupt_hash = "f".repeat(64);
        let mismatched_bytes = zstd::encode_all(
            b"these bytes do not correspond to corrupt_hash".as_slice(), 0,
        ).unwrap();
        let (folder, file_name) = file_utils::get_path_for_object(&corrupt_hash).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(PathBuf::from(&folder).join(&file_name), &mismatched_bytes).unwrap();

        pallet_utils::set_pallet_head("main", &corrupt_hash).unwrap();

        // Unrelated, non-empty (an empty `targets` set short-circuits before the walk ever runs).
        let targets: BTreeSet<String> = ["1".repeat(64)].into_iter().collect();
        let result = closure_references_any(&targets);

        assert!(result.is_err(),
            "a present-but-corrupt object must fail the walk loudly, never be silently skipped");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// (I3, test 9 — enumerating mode) On the same present-parent/absent-subtree boundary fixture
    /// as tests 1–2, `enumerate_absent_reachable` (the targetless, collecting-sink sibling) must
    /// record exactly the boundary subtree's own hash, and nothing beneath it — it must never
    /// descend past an absent node no matter which mode drives the shared walk core. Mutation:
    /// remove the `continue` after the presence-guard `sink` call in `walk_tree` (i.e. keep
    /// recording but still fall through to `load_tree`) → either an `Err` (load fails on the
    /// absent hash) or, if the mutation instead skipped the guard, spurious hashes from
    /// "descending" into content that was never there → this test reddens either way.
    #[test]
    fn enumerate_absent_reachable_records_the_boundary_and_nothing_beneath_it() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-i3-enumerate-boundary-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let absent_subtree_hash = plant_present_parent_with_absent_subtree_boundary();

        let absent = enumerate_absent_reachable()
            .expect("the enumerator must not error on a sealed/absent subtree boundary");

        assert!(absent.contains(&absent_subtree_hash),
            "the absent subtree boundary itself must be recorded by the collecting sink");
        assert_eq!(absent.len(), 1,
            "nothing beneath the absent boundary may be recorded — it must never be descended \
            into: {:?}", absent);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Shared fixture for the chunked-leaf tests below: a pallet head "main" whose parcel's spine
    /// tree has one `NormalChunked` file entry naming a recipe that IS stored (present), with one
    /// chunk in its list. The chunk object itself is never stored — its hash is only ever compared
    /// (as a target) or presence-checked (in enumerating mode), never loaded, so this is a valid,
    /// realistic fixture either way. Must be called after a `StorageRootScope` is entered. Returns
    /// the chunk's hash.
    fn plant_present_recipe_with_one_chunk() -> String {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::model::parcel::Parcel;
        use crate::model::recipe::{Recipe, RecipeChunk};
        use crate::model::tree_item::TreeItem;

        let chunk_hash = "9".repeat(64);
        let chunk_size = 10u64;

        let recipe = Recipe {
            // Never verified by this walk (or by gc) — any 64-hex value is fine here.
            content_hash: "0".repeat(64),
            total_size: chunk_size,
            chunks: vec![RecipeChunk { hash: chunk_hash.clone(), size: chunk_size }],
        };
        let mut recipe_object = LooseObjectBuilder::build_recipe(&recipe);
        recipe_object.store().unwrap();

        let mut root_tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        root_tree.add_child(TreeItem::new(
            "big.bin".to_string(), recipe_object.hash.clone(), DirEntryType::NormalChunked,
        ));
        let mut tree_object = LooseObjectBuilder::build_tree(&root_tree);
        tree_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("chunked".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        pallet_utils::set_pallet_head("main", &parcel_object.hash).unwrap();

        chunk_hash
    }

    /// (I3 refinement — per-chunk enumeration) `enumerate_absent_reachable` must record a raw-
    /// absent chunk hash out of a *present* recipe's chunk list, not just an absent recipe itself.
    /// Mutation: remove the per-chunk `raw_object_present`+sink check inside `check_leaf`'s chunked
    /// branch (the `if let Some(s) = reborrow_sink(&mut sink) { ... }` block after the recipe is
    /// confirmed present) → the chunk is compared against `targets` (empty, so never matches) and
    /// otherwise ignored → `absent` does not contain it → red. This is exactly the data-loss shape
    /// the coordinator flagged: a future §8.3 torn rescan would then lose this chunk from the
    /// remainder.
    #[test]
    fn enumerate_absent_reachable_records_an_absent_chunk_under_a_present_recipe() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-i3-enumerate-absent-chunk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let chunk_hash = plant_present_recipe_with_one_chunk();

        let absent = enumerate_absent_reachable()
            .expect("the enumerator must not error on a present recipe with an absent chunk");

        assert!(absent.contains(&chunk_hash),
            "a raw-absent chunk under a present recipe must be recorded by the collecting sink: {:?}",
            absent);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// (I3 refinement — gating non-regression) `closure_references_any` must still find a target
    /// chunk hash under a present recipe after the leaf/chunk presence checks were gated off for
    /// the targeted walk (`sink: None`) — the gating must only skip the *presence* stat, never the
    /// *targets* comparison the walk exists to answer. Mutation: gate the targets-check itself
    /// (instead of just the presence check) on `sink.is_some()` → the target chunk is never
    /// compared and `referenced` comes back empty → red.
    #[test]
    fn closure_references_any_still_finds_a_target_chunk_under_a_present_recipe() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-i3-target-chunk-noregress-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let chunk_hash = plant_present_recipe_with_one_chunk();

        let targets: BTreeSet<String> = [chunk_hash.clone()].into_iter().collect();
        let referenced = closure_references_any(&targets).unwrap();

        assert!(referenced.contains(&chunk_hash),
            "a target chunk hash under a present recipe must still be found by the targeted walk");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A pathologically deep parcel spine tree (a chain of `DEPTH` nested subtrees, one child
    /// each) must not crash the closure walk. Before `walk_tree` was made iterative (an explicit
    /// `VecDeque` work queue, mirroring `gc_utils::collect_live_set`'s own `tree_queue`), it
    /// recursed once per subtree level — confirmed to overflow the default test-thread stack at
    /// this depth: with `walk_tree`'s body temporarily reverted to its old recursive shape (one
    /// `walk_tree` call inside the `for (_, subtree) in tree.get_subtrees()` loop instead of a
    /// queue push) and this same test run in isolation, the process aborted outright — "thread
    /// '...' has overflowed its stack", `SIGABRT`, `cargo test` reporting "process didn't exit
    /// successfully" — not a normal `Err` a `#[should_panic]` could catch, an abnormal process
    /// crash, which is the actual falsifier a stack-overflow bug produces (empirically confirmed
    /// the recursive shape already dies somewhere between 1,000 and 2,000 levels on the machine
    /// this was verified on). Re-applying the iterative fix makes the same test pass cleanly.
    /// `DEPTH` here is ~25x that empirically-observed threshold — comfortably past it on any
    /// plausible stack size — while the iterative version still finishes in low single-digit
    /// seconds.
    ///
    /// Tree objects are written directly to their final on-disk path (`write_tree_object_fast`)
    /// rather than through `LooseObject::store` (which fsyncs, and renames through a barrier,
    /// per object): at `DEPTH` writes that per-object durability cost dominates real wall time
    /// for no benefit this test needs — it is exercising `walk_tree`'s traversal, not any
    /// write path's own durability, and toggling `FORKLIFT_FSYNC` would be a process-wide,
    /// cross-test change this shared unit-test binary cannot risk. Mirrors the same fast,
    /// direct-write shape `heal_utils::tests::write_loose_object` already uses for the same
    /// reason.
    #[test]
    fn walk_tree_survives_a_pathologically_deep_tree() {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::globals::StorageRootScope;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;

        fn write_tree_object_fast(tree: &TreeItem) -> String {
            let mut object = LooseObjectBuilder::build_tree(tree);
            let compressed = object.compress().unwrap();
            let (folder, file_name) = file_utils::get_path_for_object(&object.hash).unwrap();
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(PathBuf::from(&folder).join(&file_name), &compressed).unwrap();
            object.hash.clone()
        }

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-deep-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        const DEPTH: usize = 50_000;
        let target_hash = "9".repeat(64);

        // Innermost tree: a single file entry naming the (absent) target hash — never actually
        // stored as an object, since `check_leaf` only needs to check membership in `targets`
        // against an entry's own hash, never read the entry's bytes.
        let mut innermost = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        innermost.add_child(TreeItem::new(
            "file.txt".to_string(), target_hash.clone(), DirEntryType::Normal,
        ));
        let mut current_hash = write_tree_object_fast(&innermost);

        // Wrap it in `DEPTH` further levels, each with exactly one subtree child pointing at the
        // previous level — the deep, narrow spine shape that blows a recursive walk's stack.
        for i in 0..DEPTH {
            let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
            tree.add_child(TreeItem::new(
                format!("child-{}", i), current_hash.clone(), DirEntryType::Tree,
            ));
            current_hash = write_tree_object_fast(&tree);
        }

        let parcel = Parcel {
            tree_hash: current_hash,
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("deep".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        pallet_utils::set_pallet_head("main", &parcel_object.hash).unwrap();

        let targets: BTreeSet<String> = [target_hash.clone()].into_iter().collect();
        let referenced = closure_references_any(&targets)
            .expect("the closure walk must complete without crashing on a deeply nested tree");

        assert!(referenced.contains(&target_hash),
            "the deeply nested leaf's target hash must still be found by the closure walk");

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
