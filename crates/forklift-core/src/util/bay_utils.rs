//! Bays: parallel working directories bound to one warehouse (§7.5).
//!
//! A bay is an additional working directory that **shares** the warehouse's object store,
//! refs (pallets/meta), trust and configuration, while keeping its **own** working
//! directory, inventory, current pallet and lock. N agents work one machine without
//! cloning N object stores or fighting one lock — git's worktrees, designed in rather than
//! bolted on.
//!
//! A bay's working directory holds a `.forklift` **file** (not a folder) — a redirect back
//! to the warehouse — so discovery recognizes a bay the same way it recognizes a warehouse
//! (both are `dir.join(".forklift")`, one a folder, one a file). The bay's local state
//! lives under the warehouse at `.forklift/bays/<name>/`.

use std::path::{Path, PathBuf};
use crate::globals::{bay_root, forklift_root, FOLDER_NAME_BAYS_ROOT, FOLDER_NAME_FORKLIFT_ROOT};
use crate::util::{file_utils, merge_utils, park_utils};

/// The first line of a bay's `.forklift` redirect file — how discovery tells a bay's
/// redirect from an accidental file named `.forklift`.
const BAY_REDIRECT_MAGIC: &str = "forklift-bay";

/// The file inside a bay's local state recording its working directory (so the main tree
/// can list where each bay lives).
const FILE_NAME_BAY_PATH: &str = "path";

/// A parsed bay `.forklift` redirect: the warehouse it belongs to, and the bay's name.
pub struct BayRedirect {
    /// The warehouse root (the folder containing the shared `.forklift`).
    pub warehouse_root: PathBuf,

    /// The bay's name.
    pub name: String,
}

/// Whether the `.forklift` at `path` is a bay redirect (a file) rather than a warehouse
/// (a folder).
pub fn is_bay_redirect(forklift_path: &Path) -> bool {
    forklift_path.is_file()
}

/// Write a bay's `.forklift` redirect file into its working directory.
///
/// # Arguments
/// * `bay_dir`        - The bay's working directory.
/// * `warehouse_root` - The warehouse root the bay shares.
/// * `name`           - The bay's name.
pub fn write_bay_redirect(bay_dir: &Path, warehouse_root: &Path, name: &str) -> Result<(), String> {
    let content = format!(
        "{}\n{}\n{}\n",
        BAY_REDIRECT_MAGIC, warehouse_root.to_string_lossy(), name
    );

    file_utils::write_file_atomically(&bay_dir.join(FOLDER_NAME_FORKLIFT_ROOT), content.as_bytes())
}

/// Read and validate a bay's `.forklift` redirect file.
///
/// # Arguments
/// * `forklift_path` - The path of the bay's `.forklift` file.
///
/// # Returns
/// * `Ok(BayRedirect)` - The warehouse root and bay name.
/// * `Err(String)`     - If the file is not a valid bay redirect.
pub fn read_bay_redirect(forklift_path: &Path) -> Result<BayRedirect, String> {
    let content = std::fs::read_to_string(forklift_path)
        .map_err(|e| format!("Error while reading the bay redirect \"{}\": {}", forklift_path.to_string_lossy(), e))?;

    let mut lines = content.lines();

    if lines.next() != Some(BAY_REDIRECT_MAGIC) {
        return Err(format!(
            "\"{}\" is not a valid forklift bay (its \".forklift\" file is not a bay redirect).",
            forklift_path.to_string_lossy()
        ));
    }

    let warehouse_root = lines.next()
        .filter(|line| !line.is_empty())
        .ok_or("The bay redirect has no warehouse path.".to_string())?;
    let name = lines.next()
        .filter(|line| !line.is_empty())
        .ok_or("The bay redirect has no bay name.".to_string())?;

    Ok(BayRedirect {
        warehouse_root: PathBuf::from(warehouse_root),
        name: name.to_string(),
    })
}

/// The local-state folder of the given bay (under the shared forklift root).
pub fn bay_state_dir(name: &str) -> PathBuf {
    forklift_root().join(FOLDER_NAME_BAYS_ROOT).join(name)
}

/// Whether a bay of the given name exists (its state folder is present).
pub fn does_bay_exist(name: &str) -> bool {
    bay_state_dir(name).is_dir()
}

/// Record a bay's working directory in its local state (so it can be listed).
pub fn write_bay_path(name: &str, bay_dir: &Path) -> Result<(), String> {
    let state = bay_state_dir(name);
    file_utils::create_folder_if_not_exists(&state)?;
    file_utils::write_file_atomically(&state.join(FILE_NAME_BAY_PATH), bay_dir.to_string_lossy().as_bytes())
}

/// Read a bay's recorded working directory.
pub fn read_bay_path(name: &str) -> Result<PathBuf, String> {
    let path = bay_state_dir(name).join(FILE_NAME_BAY_PATH);
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Error while reading the bay path \"{}\": {}", path.to_string_lossy(), e))?;

    Ok(PathBuf::from(content.trim_end_matches('\n')))
}

/// List the names of all bays (the subfolders of `.forklift/bays/`), sorted.
///
/// # Returns
/// * `Ok(Vec<String>)` - The bay names (empty when there are none).
/// * `Err(String)`     - If the bays folder could not be read.
pub fn list_bays() -> Result<Vec<String>, String> {
    let folder = forklift_root().join(FOLDER_NAME_BAYS_ROOT);

    if !folder.is_dir() {
        return Ok(Vec::new());
    }

    let mut names: Vec<String> = Vec::new();

    for entry in file_utils::read_directory(&folder)? {
        let entry = entry.map_err(|e| format!("Error while reading a bay entry: {}", e))?;

        if file_utils::get_symlink_metadata_for_path(&entry.path())?.is_dir() {
            names.push(file_utils::get_name_for_file_or_directory(&entry)?);
        }
    }

    names.sort();

    Ok(names)
}

/// Every bay-local state dir in this warehouse, resolvable from any scope: the main tree's own
/// state dir (`forklift_root()` — the main tree keeps its bay-local state directly under
/// `.forklift/`, not under `.forklift/bays/`) followed by every named bay's state dir
/// (`bay_state_dir`), sorted by name.
///
/// This is the union a warehouse-scale walk must enumerate whenever it needs every bay's
/// bay-local state (parked parcels, staged inventory, an in-progress consolidation) rather than
/// just the active bay's — reading only the active bay under-counts references to a shared
/// object and can silently drop (or delete) something a *different* bay still needs.
/// `recovery_utils::collect_walk_roots` and `gc_utils::collect_live_set` both loop over this.
///
/// # Returns
/// * `Ok(Vec<PathBuf>)` - The main tree's state dir, followed by every bay's.
/// * `Err(String)`      - If the bays folder could not be listed.
pub fn all_bay_state_dirs() -> Result<Vec<PathBuf>, String> {
    let mut dirs = vec![forklift_root()];

    for name in list_bays()? {
        dirs.push(bay_state_dir(&name));
    }

    Ok(dirs)
}

/// How [`collect_bay_scoped_parcel_roots`] treats a bay whose `parked`/`consolidation` state
/// cannot be read.
///
/// **The real property, stated plainly (a previous version of this comment got it backwards):**
/// skipping an unreadable bay can only *narrow* the root set this call returns — an object
/// referenced only through that one bay's parked parcels or in-progress consolidation now looks
/// unreferenced, because the reference that would have named it is simply missing. A narrower
/// root set makes any caller's "not referenced" verdict *less* trustworthy, never more — it is
/// the opposite of conservative.
///
/// [`gc_utils::collect_live_set`](crate::util::gc_utils::collect_live_set) feeds a sweep that
/// *deletes* objects, so an incompletely known live set risks real, permanent data loss —
/// [`FailClosed`](BayReadPolicy::FailClosed) is the only sound choice there, and stays the
/// unconditional policy at that call site.
///
/// [`recovery_utils::collect_walk_roots`] (`crate::util::recovery_utils::collect_walk_roots`)
/// feeds `forklift heal`, which never deletes an *object* on the strength of this result — but it
/// does delete something else once it decides a hash is safe to drop: the durable taint record
/// that is, for a genuinely lost object, the *only* remaining trace that it ever went missing.
/// [`Tolerate`](BayReadPolicy::Tolerate) lets heal keep running with that one bay's roots simply
/// missing, rather than bricking the very command a standing taint tells users to run to recover —
/// but that is a license to keep making progress (restage what is present, report the bay by
/// name), never a license to clear anything on the strength of the narrower result. **A caller
/// using `Tolerate` must independently refuse to treat any "not referenced" verdict from a run
/// with a non-empty [`BayScopeOutcome::degraded`] as proof of anything** — see
/// `recovery_utils::resolve_the_rest`'s and `recovery_utils::rescan_torn_taint`'s own handling of
/// their walk's `degraded_bays` for how that plays out at each call site.
pub enum BayReadPolicy {
    /// Abort the whole call on the first unreadable bay — see this enum's doc comment. Required
    /// wherever the result feeds a destructive sweep.
    FailClosed,
    /// Skip an unreadable bay (it contributes no roots) and record a plain-language note about it
    /// in [`BayScopeOutcome::degraded`] instead of aborting. Sound only for a caller that treats a
    /// non-empty [`BayScopeOutcome::degraded`] as "this run's negative answers are not proof" and
    /// refuses to clear or delete anything — object *or* durable record — on their strength.
    Tolerate,
}

/// [`collect_bay_scoped_parcel_roots`]'s result.
pub struct BayScopeOutcome {
    /// Every listed dir's parked-parcel hashes plus in-progress-consolidation `their_head`, from
    /// every bay this call could actually read.
    pub roots: Vec<String>,
    /// One plain-language note per bay skipped this call because its state could not be read —
    /// only ever populated under [`BayReadPolicy::Tolerate`] (always empty under `FailClosed`,
    /// since that policy aborts instead of skipping).
    pub degraded: Vec<String>,
}

/// The bay-scoped parcel roots shared by **both**
/// [`recovery_utils::collect_walk_roots`](crate::util::recovery_utils::collect_walk_roots) and
/// [`gc_utils::collect_live_set`](crate::util::gc_utils::collect_live_set): across every entry of
/// `dirs`, that bay's parked parcels ([`park_utils::read_parked_in`]) plus its in-progress
/// consolidation's `their_head` ([`merge_utils::read_consolidation_state_in`]), if any.
///
/// Takes the already-enumerated `dirs` — normally the caller's own [`all_bay_state_dirs`] call —
/// rather than calling [`all_bay_state_dirs`] itself: both callers need at least one *other*
/// per-bay pass of their own over the same dirs (recovery additionally walks staged inventory
/// shards; a future gc source could too), and a second internal enumeration here would mean
/// `list_bays` runs twice per call for no reason. Passing the dirs in keeps this to exactly one
/// enumeration per caller while still sharing the parked+consolidation logic itself.
///
/// This loop used to be hand-duplicated in both callers. Extracted here so the two can never
/// drift apart: recovery's walk roots must stay a *superset* of gc's live-set roots (see
/// `collect_walk_roots`'s own doc comment for the full invariant), and a future edit adding a
/// new bay-local ref source to only one copy of a duplicated loop would silently break that
/// superset relationship — a live object in one recovery/gc pair but not the other, and quiet
/// data loss (`heal` clearing a taint over an object `gc` still refuses to delete, or `gc`
/// deleting an object `heal` would have called live). Recovery additionally walks per-bay
/// staged inventory shards and adds every tag's subject (sources gc deliberately does not
/// root); both additionally add the shared trust-anchor `adopts`. Neither of those is part of
/// this helper — only the portion the two loops had in common.
///
/// **`policy` decides what an unreadable bay does — see [`BayReadPolicy`].** gc's call site
/// passes `FailClosed` unconditionally and must never be weakened: a bay ref source that cannot
/// be read cannot be proven *not* to reference some object, and skipping it to keep a *sweep*
/// going would silently under-count references — exactly the data-loss bug the bay-scope fix
/// (reading every bay instead of just the active one) closed in the first place. Heal's call
/// site passes `Tolerate` so it can keep running past that same unreadable bay instead of
/// refusing outright — but the root set that produces is *narrower*, never wider, so it is never
/// by itself license to clear anything; the caller must still treat every hash this narrower walk
/// could not prove referenced as unproven, not safe, whenever a bay was actually skipped — see
/// [`BayReadPolicy`]'s own doc comment for the full reasoning.
///
/// # Arguments
/// * `dirs`   - The bay-local state dirs to read, in order — normally the caller's own
///              [`all_bay_state_dirs`] result, passed straight through so the bays folder is
///              listed exactly once per caller even when the caller also needs `dirs` for
///              something else.
/// * `policy` - What to do with a bay whose state cannot be read — see [`BayReadPolicy`].
///
/// # Returns
/// * `Ok(BayScopeOutcome)` - Every readable dir's parked-parcel hashes plus in-progress-
///                           consolidation `their_head`, in `dirs` order, plus (under `Tolerate`)
///                           a note for every bay that had to be skipped.
/// * `Err(String)`         - Under `FailClosed`, some bay's `parked` or `consolidation` file
///                           could not be read or was malformed — see above.
pub fn collect_bay_scoped_parcel_roots(
    dirs: &[PathBuf],
    policy: BayReadPolicy,
) -> Result<BayScopeOutcome, String> {
    let mut roots: Vec<String> = Vec::new();
    let mut degraded: Vec<String> = Vec::new();

    for dir in dirs {
        match read_one_bay_scoped_roots(dir) {
            Ok(mut found) => roots.append(&mut found),
            Err(e) => match policy {
                BayReadPolicy::FailClosed => return Err(e),
                BayReadPolicy::Tolerate => degraded.push(degraded_bay_note(dir, &e)),
            },
        }
    }

    Ok(BayScopeOutcome { roots, degraded })
}

/// One bay's own contribution to [`collect_bay_scoped_parcel_roots`] — the single read (parked
/// parcels, then in-progress-consolidation `their_head`) both [`BayReadPolicy`] variants perform
/// identically; only what happens to its `Err` differs, in the caller above.
fn read_one_bay_scoped_roots(dir: &Path) -> Result<Vec<String>, String> {
    let mut roots = park_utils::read_parked_in(dir)?;

    if let Some(consolidation) = merge_utils::read_consolidation_state_in(dir)? {
        roots.push(consolidation.their_head);
    }

    Ok(roots)
}

/// Plain-language note for a bay [`collect_bay_scoped_parcel_roots`] skipped under
/// [`BayReadPolicy::Tolerate`] — names the bay and the in-tool cleanup route (`forklift bay
/// remove`) a user can actually run, never a Rust path or internal identifier.
fn degraded_bay_note(dir: &Path, error: &str) -> String {
    if dir == forklift_root() {
        format!(
            "the current tree's own saved state could not be read this run ({}); it was treated \
            as having no parked parcels rather than blocking this command. Fix the file and run \
            this command again.",
            error
        )
    } else {
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        format!(
            "bay \"{}\" could not be read this run ({}); it was treated as having no parked \
            parcels rather than blocking this command. If the bay is stale, remove it with \
            \"forklift bay remove {}\"; otherwise fix the file and run this command again.",
            name, error, name
        )
    }
}

/// Remove a bay's local state folder. The bay's pallet (a normal ref) is left untouched;
/// removing the working directory is the caller's choice.
pub fn remove_bay_state(name: &str) -> Result<(), String> {
    let state = bay_state_dir(name);

    std::fs::remove_dir_all(&state)
        .map_err(|e| format!("Error while removing the bay state \"{}\": {}", state.to_string_lossy(), e))
}

/// Read a bay's current pallet (its bay-local `pallet` file), for listing from the main
/// tree. `None` when the bay is unborn or unreadable.
pub fn read_bay_current_pallet(name: &str) -> Option<String> {
    std::fs::read_to_string(bay_state_dir(name).join("pallet"))
        .ok()
        .map(|content| content.trim_end_matches('\n').to_string())
        .filter(|pallet| !pallet.is_empty())
}

/// The current bay's local-state folder — a convenience over [`bay_root`].
pub fn current_bay_state() -> PathBuf {
    bay_root()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::StorageRootScope;

    /// Pins that `collect_bay_scoped_parcel_roots` — the helper both
    /// `recovery_utils::collect_walk_roots` and `gc_utils::collect_live_set` call for their
    /// shared portion — actually surfaces a **non-active** bay's parked parcel *and* its
    /// in-progress consolidation `their_head` in one pass. Neither caller has its own test
    /// exercising both sources through this exact function, so this is the one place that would
    /// go red if a future edit accidentally dropped either source from the shared helper (which
    /// would then silently starve both callers, not just one — the F3 extraction's whole point).
    #[test]
    fn collect_bay_scoped_parcel_roots_finds_both_sources_from_a_non_active_bay() {
        let dir = std::env::temp_dir()
            .join(format!("forklift-bay-utils-shared-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let parked_hash = "2".repeat(64);
        let consolidation_hash = "3".repeat(64);

        let bay_b_dir = bay_state_dir("b");
        std::fs::create_dir_all(&bay_b_dir).unwrap();
        std::fs::write(bay_b_dir.join("parked"), format!("{}\n", parked_hash)).unwrap();
        std::fs::write(bay_b_dir.join("consolidation"), format!("{}\ntheir-pallet\n", consolidation_hash)).unwrap();

        let dirs = all_bay_state_dirs().unwrap();
        let roots = collect_bay_scoped_parcel_roots(&dirs, BayReadPolicy::FailClosed).unwrap().roots;

        assert!(roots.contains(&parked_hash),
            "a non-active bay's parked parcel must be in the shared roots");
        assert!(roots.contains(&consolidation_hash),
            "a non-active bay's consolidation their_head must be in the shared roots");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The fail-closed/tolerant split this helper's `policy` parameter exists for (see
    /// [`BayReadPolicy`]'s own doc comment): the exact same corrupt bay must abort under
    /// `FailClosed` (gc's policy) and must be skipped-and-noted, never abort, under `Tolerate`
    /// (heal's policy) — with the note naming the bay and the `forklift bay remove` cleanup
    /// route. `gc_utils`/`recovery_utils` each pin their own call site's behavior end to end;
    /// this is the one place that pins the shared helper's two policies directly against each
    /// other on one fixture.
    #[test]
    fn collect_bay_scoped_parcel_roots_splits_on_policy_for_the_same_corrupt_bay() {
        let dir = std::env::temp_dir()
            .join(format!("forklift-bay-utils-policy-split-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let bay_b_dir = bay_state_dir("b");
        std::fs::create_dir_all(&bay_b_dir).unwrap();
        std::fs::write(bay_b_dir.join("parked"), b"not-a-valid-hash\n").unwrap();

        let dirs = all_bay_state_dirs().unwrap();

        let strict = collect_bay_scoped_parcel_roots(&dirs, BayReadPolicy::FailClosed);
        assert!(strict.is_err(), "FailClosed must still abort on the corrupt bay");

        let tolerant = collect_bay_scoped_parcel_roots(&dirs, BayReadPolicy::Tolerate)
            .expect("Tolerate must never abort on a corrupt bay");
        assert!(tolerant.roots.is_empty(), "the corrupt bay contributes no roots");
        assert_eq!(tolerant.degraded.len(), 1, "exactly the one corrupt bay must be reported degraded");
        assert!(tolerant.degraded[0].contains("\"b\""), "the note must name the bay: {}", tolerant.degraded[0]);
        assert!(tolerant.degraded[0].contains("forklift bay remove"),
            "the note must name the in-tool cleanup route: {}", tolerant.degraded[0]);

        std::fs::remove_dir_all(&dir).ok();
    }
}
