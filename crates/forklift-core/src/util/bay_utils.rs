//! Bays: parallel working directories bound to one warehouse (§7.5).
//!
//! A bay is an additional working directory that **shares** the warehouse's object store,
//! refs (pallets/meta), trust and configuration, while keeping its **own** working
//! directory, inventory, current pallet and lock. N agents work one machine without
//! cloning N object stores or fighting one lock — git's worktrees, designed in rather than
//! bolted on.
//!
//! A bay's working directory holds its own `.forklift` folder, so discovery stops there exactly as
//! it does at a warehouse. What tells the two apart is a `redirect` file inside it, naming the
//! warehouse back ([`classify_forklift`]); every site that decides "this is a warehouse" asks that
//! one helper. The folder is also where the bay's own probes live (the clock and executable-bit
//! probes measure the bay's volume, which need not be the warehouse's). The bay's local state
//! lives under the warehouse at `.forklift/bays/<name>/`.

use std::path::{Path, PathBuf};
use crate::globals::{bay_root, forklift_root, FOLDER_NAME_BAYS_ROOT, FOLDER_NAME_FORKLIFT_ROOT};
use crate::util::{cherry_pick_utils, file_utils, merge_utils, pallet_utils, park_utils, record_utils};
use crate::util::record_utils::RecordError;

/// The file inside a bay's `.forklift` folder that names its warehouse, and so makes that folder a
/// bay's rather than a warehouse's.
pub const FILE_NAME_BAY_REDIRECT: &str = "redirect";

/// The [`record_utils`] kind and version of a bay's redirect file: the warehouse root, then the
/// bay's name. Its header is how reading one refuses a stray file named `.forklift`
/// instead of misreading it as a bay.
const BAY_REDIRECT_RECORD: &str = "bay";
const BAY_REDIRECT_VERSION: u32 = 1;

/// The file inside a bay's local state recording its working directory (so the main tree
/// can list where each bay lives), a [`record_utils`] file of kind [`BAY_PATH_RECORD`] holding
/// that one path.
const FILE_NAME_BAY_PATH: &str = "path";
const BAY_PATH_RECORD: &str = "bay-path";
const BAY_PATH_VERSION: u32 = 1;

/// A filesystem path as the text a bay record stores. A path that is not UTF-8 is refused rather
/// than written lossily, which would record a different path.
fn path_text<'a>(path: &'a Path, what: &str) -> Result<&'a str, String> {
    path.to_str().ok_or_else(|| format!(
        "The {} \"{}\" is not valid UTF-8, so a bay cannot record it.", what, path.to_string_lossy()
    ))
}

/// Check that a bay's records can hold both of its paths — the same check [`write_bay_redirect`]
/// and [`write_bay_path`] make — so that `bay add` can refuse before it creates anything.
pub fn check_bay_paths_recordable(warehouse_root: &Path, bay_dir: &Path) -> Result<(), String> {
    path_text(warehouse_root, "warehouse root")?;
    path_text(bay_dir, "bay directory")?;

    Ok(())
}

/// A parsed bay `.forklift` redirect: the warehouse it belongs to, and the bay's name.
pub struct BayRedirect {
    /// The warehouse root (the folder containing the shared `.forklift`).
    pub warehouse_root: PathBuf,

    /// The bay's name.
    pub name: String,
}

/// What a directory's `.forklift` entry is (see [`classify_forklift`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkliftKind {
    /// A folder without a redirect: a warehouse's own forklift root.
    Warehouse,

    /// A folder holding a [`FILE_NAME_BAY_REDIRECT`] regular file: a bay's working metadata.
    Bay,

    /// A regular file: a bay redirect in the format older versions wrote, which is refused rather
    /// than read (see [`old_format_bay_error`]).
    OldFormatBay,

    /// No entry at all.
    Absent,
}

/// Classify the `.forklift` entry at `forklift_path` — the one helper behind every decision that a
/// directory is a warehouse, so that a bay is never served, prepared or entered as one. A symlinked
/// `.forklift` (or `redirect`) is followed, as discovery follows it.
///
/// # Returns
/// * `Ok(ForkliftKind)` - What the entry is.
/// * `Err(String)`      - If it is neither a file nor a folder, holds a `redirect` that is not a
///                        regular file, or could not be read.
pub fn classify_forklift(forklift_path: &Path) -> Result<ForkliftKind, String> {
    use std::io::ErrorKind;

    let absent = |e: &std::io::Error| matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory);

    let metadata = match std::fs::metadata(forklift_path) {
        Ok(metadata) => metadata,
        Err(e) if absent(&e) => return Ok(ForkliftKind::Absent),
        Err(e) => return Err(format!("Error while checking \"{}\": {}", forklift_path.to_string_lossy(), e)),
    };

    if metadata.is_file() {
        return Ok(ForkliftKind::OldFormatBay);
    }

    if !metadata.is_dir() {
        return Err(format!(
            "\"{}\" is neither a folder nor a file, so it is neither a warehouse nor a bay.",
            forklift_path.to_string_lossy()
        ));
    }

    let redirect = forklift_path.join(FILE_NAME_BAY_REDIRECT);

    match std::fs::metadata(&redirect) {
        Ok(metadata) if metadata.is_file() => Ok(ForkliftKind::Bay),
        Ok(_) => Err(format!(
            "\"{}\" is not a regular file, so \"{}\" is neither a warehouse nor a valid bay.",
            redirect.to_string_lossy(), forklift_path.to_string_lossy()
        )),
        Err(e) if absent(&e) => Ok(ForkliftKind::Warehouse),
        Err(e) => Err(format!("Error while checking \"{}\": {}", redirect.to_string_lossy(), e)),
    }
}

/// The refusal for a bay whose `.forklift` is a redirect file in the older format, with the
/// conversion that makes it current. The remedy moves the file into a fresh `.forklift.new` folder
/// rather than beside it, so it never touches a name the bay's own tree may hold.
///
/// # Arguments
/// * `root` - The directory holding the old-format `.forklift` file.
pub fn old_format_bay_error(root: &Path) -> String {
    let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);
    let staging = root.join(FOLDER_NAME_FORKLIFT_NEW);

    format!(
        "\"{}\" is a bay redirect file written by an older version of Forklift; a bay now keeps it \
        in a folder. Convert it in place: create the folder \"{}\", move the file \"{}\" to \"{}\", \
        then rename \"{}\" to \"{}\".",
        forklift.to_string_lossy(), staging.to_string_lossy(), forklift.to_string_lossy(),
        staging.join(FILE_NAME_BAY_REDIRECT).to_string_lossy(), staging.to_string_lossy(),
        forklift.to_string_lossy()
    )
}

/// The folder [`old_format_bay_error`]'s remedy stages the conversion in.
const FOLDER_NAME_FORKLIFT_NEW: &str = ".forklift.new";

/// Write a bay's `.forklift` folder, holding its redirect, into its working directory.
///
/// The folder is built complete under a fresh temporary sibling name and renamed into place, so a
/// crash never leaves an empty `.forklift` folder behind, which discovery would take for a
/// warehouse. The rename target is checked to be absent first: a rename over an existing entry
/// would replace a file on Windows.
///
/// # Arguments
/// * `bay_dir`        - The bay's working directory.
/// * `warehouse_root` - The warehouse root the bay shares.
/// * `name`           - The bay's name.
pub fn write_bay_redirect(bay_dir: &Path, warehouse_root: &Path, name: &str) -> Result<(), String> {
    let warehouse_root = path_text(warehouse_root, "warehouse root")?;
    let content = record_utils::to_bytes(BAY_REDIRECT_RECORD, BAY_REDIRECT_VERSION, [warehouse_root, name]);

    let forklift = bay_dir.join(FOLDER_NAME_FORKLIFT_ROOT);
    let staging = file_utils::temp_path_for(&forklift)?;

    std::fs::create_dir(&staging)
        .map_err(|e| format!("Error while creating the bay folder \"{}\": {}", staging.to_string_lossy(), e))?;

    let placed = file_utils::write_file_atomically(&staging.join(FILE_NAME_BAY_REDIRECT), &content)
        .and_then(|()| match std::fs::symlink_metadata(&forklift) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(format!("\"{}\" already exists, so the bay cannot be created there.", forklift.to_string_lossy())),
            Err(e) => Err(format!("Error while checking \"{}\": {}", forklift.to_string_lossy(), e)),
        })
        .and_then(|()| std::fs::rename(&staging, &forklift).map_err(|e| format!(
            "Error while moving the bay folder into place at \"{}\": {}", forklift.to_string_lossy(), e
        )));

    if let Err(e) = placed {
        // A real folder this call created, so the recursive removal follows nothing it did not make.
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    file_utils::sync_dir(bay_dir)
}

/// Remove a bay's `.forklift` folder from its working directory, so the directory is no longer a
/// bay.
///
/// The entry is first renamed to a fresh, absent-checked sibling name: that one step ends the bay,
/// and nothing re-creates a `.forklift` folder on demand, so a bay command running meanwhile
/// cannot leave one behind that would read as a warehouse. The renamed entry is then deleted
/// without following links: a symlink or file as itself, a real folder recursively. A missing
/// working directory or `.forklift` is not a failure.
///
/// # Returns
/// * `Ok(())`                    - The directory is no longer a bay, and nothing is left behind.
/// * `Err(RedirectRemoval::Kept)` - The rename failed: the directory is still a bay.
/// * `Err(RedirectRemoval::Left)` - The directory is no longer a bay, but the renamed entry could
///                                  not be deleted; the message names it.
pub fn remove_bay_redirect(bay_dir: &Path) -> Result<(), RedirectRemoval> {
    use std::io::ErrorKind;

    let forklift = bay_dir.join(FOLDER_NAME_FORKLIFT_ROOT);
    let gone = |e: &std::io::Error| matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory);

    match std::fs::symlink_metadata(&forklift) {
        Ok(_) => (),
        Err(e) if gone(&e) => return Ok(()),
        Err(e) => return Err(RedirectRemoval::Kept(format!("Error while checking \"{}\": {}", forklift.to_string_lossy(), e))),
    }

    let retired = file_utils::temp_path_for(&forklift).map_err(RedirectRemoval::Kept)?;

    match std::fs::symlink_metadata(&retired) {
        Err(e) if e.kind() == ErrorKind::NotFound => (),
        Ok(_) => return Err(RedirectRemoval::Kept(format!(
            "\"{}\" already exists, so the bay folder \"{}\" cannot be moved aside to remove it.",
            retired.to_string_lossy(), forklift.to_string_lossy()
        ))),
        Err(e) => return Err(RedirectRemoval::Kept(format!("Error while checking \"{}\": {}", retired.to_string_lossy(), e))),
    }

    match std::fs::rename(&forklift, &retired) {
        Ok(()) => (),
        Err(e) if gone(&e) => return Ok(()),
        Err(e) => return Err(RedirectRemoval::Kept(format!(
            "Error while moving the bay folder \"{}\" aside to remove it: {}", forklift.to_string_lossy(), e
        ))),
    }

    let deleted = match std::fs::symlink_metadata(&retired) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(&retired),
        Ok(_) => std::fs::remove_file(&retired),
        Err(e) => Err(e),
    };

    deleted.map_err(|e| RedirectRemoval::Left(format!(
        "The bay folder was moved aside to \"{}\", but it could not be deleted: {}. Delete it by hand.",
        retired.to_string_lossy(), e
    )))
}

/// How [`remove_bay_redirect`] failed.
#[derive(Debug)]
pub enum RedirectRemoval {
    /// Nothing was moved: the directory is still a bay.
    Kept(String),

    /// The directory is no longer a bay, but an entry was left behind.
    Left(String),
}

/// Validate a bay name: a valid pallet name (a bay checks out the pallet of its name) that is one
/// folder, since bays are listed one level deep under the bays folder.
///
/// # Returns
/// * `Ok(())`      - If the name is a valid bay name.
/// * `Err(String)` - Why it is not.
pub fn validate_bay_name(name: &str) -> Result<(), String> {
    // Checked first, so the pallet grammar's lowercase hint never suggests a name that is still
    // not a bay name.
    if name.contains('/') {
        return Err(format!(
            "\"{}\" is not a valid bay name: a bay name is one folder, so it must not contain \"/\"", name
        ));
    }

    pallet_utils::validate_pallet_name(name)
}

/// The `forklift bay remove` command line that addresses the bay folder `name` exactly, quoted so
/// it can be pasted into a shell, or `None` when no command line can: `bay remove` matches one
/// listed folder, so never a name holding "/", and refuses a name ending in "." or a space (see
/// [`is_listed_bay`]); a name holding a quote or a control character has no safe spelling here.
pub fn remove_command(name: &str) -> Option<String> {
    if name.is_empty() || name.contains('/') || name.ends_with('.') || name.ends_with(' ') {
        return None;
    }

    let word = if name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
        name.to_string()
    } else if name.chars().any(|c| c == '\'' || c == '"' || c.is_control()) {
        return None;
    } else {
        format!("'{}'", name)
    };

    // A leading '-' would be read as an option, so the name follows "--".
    let separator = if name.starts_with('-') { "-- " } else { "" };
    Some(format!("forklift bay remove {}{}", separator, word))
}

/// Read and validate the redirect in a bay's `.forklift` folder.
///
/// # Arguments
/// * `forklift_path` - The path of the bay's `.forklift` folder.
///
/// # Returns
/// * `Ok(BayRedirect)` - The warehouse root and bay name.
/// * `Err(String)`     - If the folder holds no valid bay redirect.
pub fn read_bay_redirect(forklift_path: &Path) -> Result<BayRedirect, String> {
    let folder = forklift_path;
    let forklift_path = &folder.join(FILE_NAME_BAY_REDIRECT);
    let bytes = std::fs::read(forklift_path)
        .map_err(|e| format!("Error while reading the bay redirect \"{}\": {}", forklift_path.to_string_lossy(), e))?;

    let fields = record_utils::from_bytes(&bytes, BAY_REDIRECT_RECORD, BAY_REDIRECT_VERSION).map_err(|e| match e {
        RecordError::Unversioned => format!(
            "\"{}\" is not a valid forklift bay redirect (or an older version of Forklift wrote it).",
            forklift_path.to_string_lossy()
        ),
        _ => format!("The bay redirect \"{}\" {}", forklift_path.to_string_lossy(), e.describe()),
    })?;

    let [warehouse_root, name] = <[String; 2]>::try_from(fields)
        .map_err(|_| format!("The bay redirect \"{}\" is malformed.", forklift_path.to_string_lossy()))?;

    if warehouse_root.is_empty() {
        return Err("The bay redirect has no warehouse path.".to_string());
    }
    if name.is_empty() {
        return Err("The bay redirect has no bay name.".to_string());
    }
    validate_bay_name(&name).map_err(|e| {
        let way_out = match remove_command(&name) {
            Some(command) => format!(
                "Run \"{}\" in the warehouse at \"{}\" if it lists that bay, or delete the folder \
                \"{}\" to stop treating this directory as a bay.", command, warehouse_root, folder.to_string_lossy()
            ),
            None => format!(
                "Delete the folder \"{}\" to stop treating this directory as a bay.", folder.to_string_lossy()
            ),
        };
        format!("The bay redirect \"{}\" names an invalid bay: {}. {}", forklift_path.to_string_lossy(), e, way_out)
    })?;

    Ok(BayRedirect {
        warehouse_root: PathBuf::from(warehouse_root),
        name,
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
    let bay_dir = path_text(bay_dir, "bay directory")?;

    file_utils::write_file_atomically(
        &state.join(FILE_NAME_BAY_PATH), &record_utils::to_bytes(BAY_PATH_RECORD, BAY_PATH_VERSION, [bay_dir])
    )
}

/// Read a bay's recorded working directory, or `None` when the bay never recorded one (a bay
/// whose creation failed before its `path` file was written). A file that is present but cannot
/// be read is an error, never `None`.
pub fn read_bay_path(name: &str) -> Result<Option<PathBuf>, String> {
    let path = bay_state_dir(name).join(FILE_NAME_BAY_PATH);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Error while reading the bay path \"{}\": {}", path.to_string_lossy(), e)),
    };

    let fields = record_utils::from_bytes(&bytes, BAY_PATH_RECORD, BAY_PATH_VERSION)
        .map_err(|e| format!("The bay path file \"{}\" {}", path.to_string_lossy(), e.describe()))?;

    match <[String; 1]>::try_from(fields) {
        Ok([bay_dir]) if !bay_dir.is_empty() => Ok(Some(PathBuf::from(bay_dir))),
        _ => Err(format!("The bay path file \"{}\" is malformed.", path.to_string_lossy())),
    }
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
/// their walk's `degraded_sources` (which folds this bay-scoped source together with the
/// office-record source — see `recovery_utils::WalkRoots::degraded_sources`) for how that plays
/// out at each call site.
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
    /// Every listed dir's base, parked-parcel hashes and the parcels an in-progress consolidation
    /// or cherry-pick records, from every bay this call could actually read.
    pub roots: Vec<String>,
    /// One plain-language note per bay skipped this call because its state could not be read —
    /// only ever populated under [`BayReadPolicy::Tolerate`] (always empty under `FailClosed`,
    /// since that policy aborts instead of skipping).
    pub degraded: Vec<String>,
}

/// The bay-scoped parcel roots shared by **both**
/// [`recovery_utils::collect_walk_roots`](crate::util::recovery_utils::collect_walk_roots) and
/// [`gc_utils::collect_live_set`](crate::util::gc_utils::collect_live_set): across every entry of
/// `dirs`, that bay's base, its parked parcels ([`park_utils::read_parked_in`]) and the parcels an
/// in-progress consolidation or cherry-pick records — see [`read_one_bay_scoped_roots`].
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
/// deleting an object `heal` would have called live). Both additionally add every bay's staged
/// inventory shard leaves (FORK-156) and the shared trust-pin roots (`adopts`, the enrollment
/// `boundary` snapshot, and every key's revocation `distrust_boundary` — FORK-81), via
/// `recovery_utils::gc_root_sources`. Neither of those is part of this helper — only the portion
/// the two loops had in common.
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
/// * `Ok(BayScopeOutcome)` - Every readable dir's base, parked-parcel hashes and the parcels an
///                           in-progress consolidation or cherry-pick records, in `dirs` order, plus (under `Tolerate`)
///                           a note for every bay that had to be skipped.
/// * `Err(String)`         - Under `FailClosed`, some bay's `position`, `parked` or `consolidation` file
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

/// One bay's own contribution to [`collect_bay_scoped_parcel_roots`] — the single read (the
/// bay's base, parked parcels, then an in-progress consolidation's `their_head` and `our_head`,
/// then an in-progress cherry-pick's `source` and `our_head`) both [`BayReadPolicy`] variants
/// perform identically; only what happens to its `Err` differs, in the caller above. A recorded
/// `our_head` is the completing parcel's parent, and the pallet may have moved off it since, so
/// it is rooted in its own right. A pick's `source` is no parent of anything, yet the completing
/// `stack` reads its authors, and no ref need still reach it. So is the base: an undo in another bay on the same pallet can
/// put the ref back behind it, leaving the tree this bay holds reachable from no ref.
fn read_one_bay_scoped_roots(dir: &Path) -> Result<Vec<String>, String> {
    let mut roots: Vec<String> = pallet_utils::read_position_in(dir)?.base.into_iter().collect();

    roots.append(&mut park_utils::read_parked_in(dir)?);

    if let Some(consolidation) = merge_utils::read_consolidation_state_in(dir)? {
        roots.push(consolidation.their_head);
        roots.push(consolidation.our_head);
    }

    if let Some(pick) = cherry_pick_utils::read_state_in(dir)? {
        roots.push(pick.source);
        roots.push(pick.our_head);
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

/// Whether `name` is, byte for byte, a bay folder listed in `.forklift/bays/`. This is the way
/// to address a bay whose name the grammar refuses (so it can be removed), and so it matches the
/// raw entries rather than validating the name: `.`, `..` and a name holding a separator are
/// never listed entries, so they never resolve to the forklift root or to another folder. A name
/// ending in `.` or a space is refused too, since Windows resolves such a path to a different
/// folder. Entries that are not valid UTF-8 are compared as they are, never a reason to fail.
///
/// # Returns
/// * `Ok(true)`    - If a real folder (not a link) of exactly that name is listed.
/// * `Ok(false)`   - If none is.
/// * `Err(String)` - If the bays folder could not be read.
pub fn is_listed_bay(name: &str) -> Result<bool, String> {
    if name.is_empty() || name.ends_with('.') || name.ends_with(' ') {
        return Ok(false);
    }

    let folder = forklift_root().join(FOLDER_NAME_BAYS_ROOT);

    if !folder.is_dir() {
        return Ok(false);
    }

    for entry in file_utils::read_directory(&folder)? {
        let entry = entry.map_err(|e| format!("Error while reading a bay entry: {}", e))?;

        if entry.file_name() == std::ffi::OsStr::new(name) {
            return Ok(file_utils::get_symlink_metadata_for_path(&entry.path())?.is_dir());
        }
    }

    Ok(false)
}

/// Remove a bay's local state folder. The bay's pallet (a normal ref) is left untouched;
/// removing the working directory is the caller's choice. Only a listed bay folder is removed
/// (see [`is_listed_bay`]).
pub fn remove_bay_state(name: &str) -> Result<(), String> {
    if !is_listed_bay(name)? {
        return Err(format!("There is no bay named \"{}\".", name));
    }

    let state = bay_state_dir(name);

    std::fs::remove_dir_all(&state)
        .map_err(|e| format!("Error while removing the bay state \"{}\": {}", state.to_string_lossy(), e))
}

/// Read a bay's current pallet (from its bay-local position), for listing from the main tree.
///
/// # Returns
/// * `Ok(None)`         - If the bay has no position file yet.
/// * `Ok(Some(String))` - The pallet the bay stands on.
/// * `Err(String)`      - If the position is present but cannot be read or names an invalid pallet.
pub fn read_bay_current_pallet(name: &str) -> Result<Option<String>, String> {
    let dir = bay_state_dir(name);
    let path = dir.join(pallet_utils::FILE_NAME_POSITION);

    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Error while reading the bay position file \"{}\": {}", path.to_string_lossy(), e)),
        Ok(_) => {}
    }

    pallet_utils::read_position_in(&dir).map(|position| Some(position.pallet))
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
    /// shared portion — actually surfaces a **non-active** bay's parked parcel and the parcels
    /// its in-progress consolidation and cherry-pick record, in one pass. Neither caller has its own test
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
        crate::util::pallet_utils::create_position_if_not_exists().unwrap();

        let parked_hash = "2".repeat(64);
        let consolidation_hash = "3".repeat(64);
        let (merge_base, pick_base) = ("4".repeat(64), "5".repeat(64));

        let bay_b_dir = bay_state_dir("b");
        std::fs::create_dir_all(&bay_b_dir).unwrap();
        crate::util::pallet_utils::write_position_in(&bay_b_dir, &crate::util::pallet_utils::BayPosition::new("main", None)).unwrap();
        std::fs::write(bay_b_dir.join("parked"), format!("{}\n", parked_hash)).unwrap();
        let state = crate::util::merge_utils::ConsolidationState {
            their_head: consolidation_hash.clone(), their_pallet: "their-pallet".to_string(), our_head: merge_base.clone(), our_pallet: "main".to_string(),
        };
        std::fs::write(bay_b_dir.join("consolidation"), crate::util::merge_utils::consolidation_state_bytes(&state)).unwrap();

        let bay_c_dir = bay_state_dir("c");
        std::fs::create_dir_all(&bay_c_dir).unwrap();
        crate::util::pallet_utils::write_position_in(&bay_c_dir, &crate::util::pallet_utils::BayPosition::new("main", None)).unwrap();
        let pick_source = "6".repeat(64);
        let pick = cherry_pick_utils::CherryPickState {
            source: pick_source.clone(), description: None, our_head: pick_base.clone(), our_pallet: "main".to_string(),
        };
        std::fs::write(bay_c_dir.join("cherry-pick"), serde_json::to_string(&pick).unwrap()).unwrap();

        let dirs = all_bay_state_dirs().unwrap();
        let roots = collect_bay_scoped_parcel_roots(&dirs, BayReadPolicy::FailClosed).unwrap().roots;

        assert!(roots.contains(&parked_hash),
            "a non-active bay's parked parcel must be in the shared roots");
        assert!(roots.contains(&consolidation_hash),
            "a non-active bay's consolidation their_head must be in the shared roots");
        assert!(roots.contains(&merge_base),
            "a non-active bay's consolidation our_head must be in the shared roots");
        assert!(roots.contains(&pick_base),
            "a non-active bay's cherry-pick our_head must be in the shared roots");
        assert!(roots.contains(&pick_source),
            "a non-active bay's cherry-pick source must be in the shared roots");

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
        crate::util::pallet_utils::create_position_if_not_exists().unwrap();

        let bay_b_dir = bay_state_dir("b");
        std::fs::create_dir_all(&bay_b_dir).unwrap();
        crate::util::pallet_utils::write_position_in(&bay_b_dir, &crate::util::pallet_utils::BayPosition::new("main", None)).unwrap();
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

    #[test]
    fn a_redirect_round_trips_a_warehouse_root_holding_a_line_break() {
        let dir = std::env::temp_dir()
            .join(format!("forklift-bay-utils-redirect-line-break-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        for root in ["/srv/ware\nhouse", "/srv/ware\r", "/srv/a\\nb"] {
            let _ = std::fs::remove_dir_all(dir.join(FOLDER_NAME_FORKLIFT_ROOT));
            write_bay_redirect(&dir, Path::new(root), "review").unwrap();

            let redirect = read_bay_redirect(&dir.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();

            assert_eq!(redirect.warehouse_root, PathBuf::from(root));
            assert_eq!(redirect.name, "review");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_bay_name_is_one_folder_of_the_pallet_grammar() {
        assert!(validate_bay_name("b1").is_ok());
        assert!(validate_bay_name("review.2").is_ok());

        let error = validate_bay_name("feature/x").unwrap_err();
        assert!(error.contains("must not contain \"/\""), "{}", error);
        let error = validate_bay_name("Feature/X").unwrap_err();
        assert!(error.contains("not a valid bay name") && !error.contains("lowercase"),
            "no hint toward a name that is still refused: {}", error);
        assert!(validate_bay_name("Review").unwrap_err().contains("is not a valid pallet name"));
    }

    /// The way-out command line spells the exact folder name for a shell, or is withheld where
    /// `bay remove` would refuse the name or no safe spelling exists.
    #[test]
    fn the_remove_command_spells_the_folder_name_for_a_shell() {
        assert_eq!(remove_command("Main").as_deref(), Some("forklift bay remove Main"));
        assert_eq!(remove_command("My Bay").as_deref(), Some("forklift bay remove 'My Bay'"));
        assert_eq!(remove_command("a$b;c").as_deref(), Some("forklift bay remove 'a$b;c'"));
        assert_eq!(remove_command("-x").as_deref(), Some("forklift bay remove -- -x"));

        for name in ["", "b1.", "b1 ", "it's", "say\"hi\"", "tab\there", "feature/x"] {
            assert_eq!(remove_command(name), None, "{:?}", name);
        }
    }

    #[test]
    fn a_redirect_naming_an_invalid_bay_is_refused() {
        let dir = std::env::temp_dir()
            .join(format!("forklift-bay-utils-redirect-invalid-name-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let forklift_path = dir.join(FOLDER_NAME_FORKLIFT_ROOT);

        write_bay_redirect(&dir, Path::new("/srv/warehouse"), "review").unwrap();
        assert_eq!(read_bay_redirect(&forklift_path).unwrap().name, "review");

        for name in ["Review", "..", "review."] {
            std::fs::remove_dir_all(&forklift_path).unwrap();
            write_bay_redirect(&dir, Path::new("/srv/warehouse"), name).unwrap();
            let error = read_bay_redirect(&forklift_path).err().expect(name);
            assert!(error.contains("names an invalid bay") && error.contains(&format!("\"{}\" is not a valid pallet name", name)),
                "{}: {}", name, error);
            // Deleting only the redirect would leave an empty folder, which reads as a warehouse.
            assert!(error.contains(&format!("delete the folder \"{}\"", forklift_path.to_string_lossy()))
                || error.contains(&format!("Delete the folder \"{}\"", forklift_path.to_string_lossy())),
                "{}: {}", name, error);
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Each shape of `.forklift` is classified as what it is: a folder holding a `redirect` file is a
    /// bay, never a warehouse, and a `redirect` that is not a file is refused rather than guessed.
    #[test]
    fn a_forklift_entry_is_classified_by_its_shape() {
        let dir = std::env::temp_dir().join(format!("forklift-bay-utils-classify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let forklift = dir.join(FOLDER_NAME_FORKLIFT_ROOT);

        assert_eq!(classify_forklift(&forklift).unwrap(), ForkliftKind::Absent);
        assert_eq!(classify_forklift(&dir.join("missing").join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap(), ForkliftKind::Absent);

        std::fs::write(&forklift, "old").unwrap();
        assert_eq!(classify_forklift(&forklift).unwrap(), ForkliftKind::OldFormatBay);
        std::fs::remove_file(&forklift).unwrap();

        std::fs::create_dir(&forklift).unwrap();
        assert_eq!(classify_forklift(&forklift).unwrap(), ForkliftKind::Warehouse);

        std::fs::create_dir(forklift.join(FILE_NAME_BAY_REDIRECT)).unwrap();
        let error = classify_forklift(&forklift).unwrap_err();
        assert!(error.contains("is not a regular file"), "{}", error);
        std::fs::remove_dir(forklift.join(FILE_NAME_BAY_REDIRECT)).unwrap();
        std::fs::remove_dir(&forklift).unwrap();

        write_bay_redirect(&dir, Path::new("/srv/warehouse"), "review").unwrap();
        assert_eq!(classify_forklift(&forklift).unwrap(), ForkliftKind::Bay);
        assert!(std::fs::symlink_metadata(&forklift).unwrap().is_dir(), "a bay's .forklift is a folder");
        assert!(std::fs::symlink_metadata(forklift.join(FILE_NAME_BAY_REDIRECT)).unwrap().is_file());

        // The folder is placed by one rename: a .forklift already there is refused, never merged into.
        let error = write_bay_redirect(&dir, Path::new("/srv/other"), "review").unwrap_err();
        assert!(error.contains("already exists"), "{}", error);
        assert_eq!(read_bay_redirect(&forklift).unwrap().warehouse_root, PathBuf::from("/srv/warehouse"));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "the staging folder is cleaned up");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Removing a bay's `.forklift` never follows a link planted in its place: the link goes, and
    /// what it pointed at is untouched.
    #[cfg(unix)]
    #[test]
    fn removing_the_bay_folder_never_follows_a_link() {
        let dir = std::env::temp_dir().join(format!("forklift-bay-utils-remove-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bay = dir.join("bay");
        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir_all(&bay).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("precious"), "keep me").unwrap();
        std::os::unix::fs::symlink(&elsewhere, bay.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();

        remove_bay_redirect(&bay).unwrap();

        assert_eq!(std::fs::read_to_string(elsewhere.join("precious")).ok().as_deref(), Some("keep me"),
                   "what the link pointed at must be untouched");
        assert_eq!(std::fs::read_dir(&bay).unwrap().count(), 0, "the link and its renamed name are gone");

        // A bay whose working tree or .forklift is already gone is removed without complaint.
        remove_bay_redirect(&bay).unwrap();
        remove_bay_redirect(&dir.join("missing")).unwrap();

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A fresh warehouse root with the given bay folders (and one plain file) under `bays/`.
    fn bays_fixture(name: &str, bays: &[&str]) -> (PathBuf, StorageRootScope) {
        let dir = std::env::temp_dir().join(format!("forklift-bay-utils-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bays_root = dir.join(FOLDER_NAME_FORKLIFT_ROOT).join(FOLDER_NAME_BAYS_ROOT);
        std::fs::create_dir_all(&bays_root).unwrap();
        for bay in bays {
            std::fs::create_dir_all(bays_root.join(bay)).unwrap();
            std::fs::write(bays_root.join(bay).join("marker"), bay).unwrap();
        }
        std::fs::write(bays_root.join("plain-file"), "not a bay").unwrap();
        std::fs::write(dir.join(FOLDER_NAME_FORKLIFT_ROOT).join("keep"), "shared state").unwrap();
        let scope = StorageRootScope::enter(&dir);

        (dir, scope)
    }

    /// `bay remove` addresses a bay by its exact folder name: a refused name that is a real folder
    /// resolves, while `.`, `..`, a path, a spelling the folder does not have byte for byte, a
    /// trailing `.` or space, and a plain file never do.
    #[test]
    fn only_an_exactly_listed_bay_folder_resolves() {
        let (dir, _scope) = bays_fixture("listed", &["b1", "Main"]);

        assert!(is_listed_bay("b1").unwrap());
        assert!(is_listed_bay("Main").unwrap(), "a folder the grammar refuses must still resolve");

        for name in ["", ".", "..", "b1/..", "b1/../b1", "../bays/b1", "main", "B1", "b1.", "b1 ", "plain-file", "absent"] {
            assert!(!is_listed_bay(name).unwrap(), "must not resolve: {:?}", name);
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The live data-loss case: removing `..` once emptied the whole forklift root. Only a listed
    /// folder is ever removed, and removing a refused name removes exactly that folder.
    #[test]
    fn removing_bay_state_never_leaves_the_bays_folder() {
        let (dir, _scope) = bays_fixture("remove", &["b1", "Main"]);
        let root = dir.join(FOLDER_NAME_FORKLIFT_ROOT);

        for name in ["..", ".", "b1/..", "b1/../..", ""] {
            assert!(remove_bay_state(name).is_err(), "must refuse: {:?}", name);
            assert!(root.join("keep").is_file() && root.join("bays/b1/marker").is_file() && root.join("bays/Main/marker").is_file(),
                "{:?} removed something", name);
        }

        remove_bay_state("Main").unwrap();
        assert!(!root.join("bays/Main").exists());
        assert!(root.join("keep").is_file() && root.join("bays/b1/marker").is_file(), "only the named folder goes");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A sibling whose name is not UTF-8 neither aborts the match nor blocks removing another bay.
    /// Linux only: APFS refuses such a name outright and NTFS names are UTF-16.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_non_utf8_sibling_does_not_block_resolving_a_bay() {
        use std::os::unix::ffi::OsStrExt;

        let (dir, _scope) = bays_fixture("non-utf8", &["b1"]);
        let odd = std::ffi::OsStr::from_bytes(b"bad\xff");
        std::fs::create_dir_all(forklift_root().join(FOLDER_NAME_BAYS_ROOT).join(odd)).unwrap();
        assert!(list_bays().is_err(), "the fixture must hold an entry the plain listing cannot read");

        assert!(is_listed_bay("b1").unwrap());
        remove_bay_state("b1").unwrap();
        assert!(!bay_state_dir("b1").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Only a missing position reads as "no pallet"; a position naming a refused pallet is an
    /// error, never a silent absence.
    #[test]
    fn a_bay_pallet_is_absent_only_when_the_bay_has_no_position() {
        let (dir, _scope) = bays_fixture("current-pallet", &["fresh", "good", "bad"]);

        assert_eq!(read_bay_current_pallet("fresh").unwrap(), None);

        pallet_utils::write_position_in(&bay_state_dir("good"), &pallet_utils::BayPosition::new("good", None)).unwrap();
        assert_eq!(read_bay_current_pallet("good").unwrap(), Some("good".to_string()));

        std::fs::write(bay_state_dir("bad").join(pallet_utils::FILE_NAME_POSITION),
            record_utils::to_bytes("position", 1, ["Main", ""])).unwrap();
        let error = read_bay_current_pallet("bad").unwrap_err();
        assert!(error.contains("\"Main\" is not a valid pallet name"), "{}", error);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_redirect_without_its_format_header_is_refused() {
        let dir = std::env::temp_dir()
            .join(format!("forklift-bay-utils-redirect-headerless-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let forklift_path = dir.join(FOLDER_NAME_FORKLIFT_ROOT);
        std::fs::create_dir_all(&forklift_path).unwrap();
        std::fs::write(forklift_path.join(FILE_NAME_BAY_REDIRECT), "/srv/warehouse\nreview\n").unwrap();

        let error = read_bay_redirect(&forklift_path).err().expect("a headerless redirect must be refused");
        assert!(error.contains("is not a valid forklift bay"), "{}", error);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_bay_path_round_trips_a_directory_holding_a_line_break_and_refuses_a_headerless_file() {
        let dir = std::env::temp_dir()
            .join(format!("forklift-bay-utils-bay-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        for bay_dir in ["/srv/bay\nreview", "/srv/bay\r", "/srv/a\\nb"] {
            write_bay_path("review", Path::new(bay_dir)).unwrap();
            assert_eq!(read_bay_path("review").unwrap(), Some(PathBuf::from(bay_dir)));
        }

        std::fs::write(bay_state_dir("review").join(FILE_NAME_BAY_PATH), "/srv/bay\n").unwrap();
        let error = read_bay_path("review").unwrap_err();
        assert!(error.contains("has no format header"), "{}", error);

        std::fs::remove_dir_all(&dir).ok();
    }
}
