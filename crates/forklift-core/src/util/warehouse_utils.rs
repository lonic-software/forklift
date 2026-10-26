use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use crate::error::{CoreError, RefusalCode};
use crate::globals::{self, forklift_root, FOLDER_NAME_FORKLIFT_ROOT};
use crate::util::{alias_utils, bay_utils, config_utils, file_utils, franchise_utils, pallet_utils, record_utils};

/// The directory the user invoked forklift from, relative to the warehouse root.
/// Set once by [`enter_warehouse`]; empty when the user is at the root itself.
static CWD_RELATIVE_TO_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Find the root folder of the warehouse by walking up from the current directory
/// until a folder containing the forklift root folder is found.
///
/// # Returns
/// * `Ok(PathBuf)`  - The path of the warehouse root.
/// * `Err(String)`  - A framed `not_a_warehouse` refusal if the current directory is not inside a
///                    working tree, or the current directory could not be read.
pub fn find_warehouse_root() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir()
        .map_err(|e| format!("Error while getting the current directory: {}", e))?;

    let mut dir = cwd.as_path();

    loop {
        // A warehouse and a bay (§7.5) both mark the root of their working tree with a
        // `.forklift` folder; `enter_warehouse` tells them apart.
        if dir.join(FOLDER_NAME_FORKLIFT_ROOT).exists() {
            return Ok(dir.to_path_buf());
        }

        match dir.parent() {
            Some(parent) => dir = parent,
            None => return Err(String::from(CoreError::refusal(
                RefusalCode::NotAWarehouse,
                format!(
                    "Not a forklift warehouse (no \"{}\" found in this directory or any of its parents).",
                    FOLDER_NAME_FORKLIFT_ROOT
                ),
                "Run \"forklift prepare\" to create a warehouse here, or change into one.",
            ))),
        }
    }
}

/// Whether `dir` is the root of a working tree: it holds a `.forklift` entry (a warehouse's or a
/// bay's folder, or an old-format bay file), which [`find_warehouse_root`] stops at from anywhere inside it. Below
/// the warehouse's own root, such a directory is another working tree, never content of this one.
///
/// Stricter than discovery, which follows a symlink: an entry of any kind counts, so a dangling
/// link is not taken for an ordinary directory either.
///
/// # Arguments
/// * `dir` - The directory, relative to the current directory or absolute.
///
/// # Returns
/// * `Ok(bool)`    - Whether `dir` holds a `.forklift` entry (`false` when `dir` does not exist or
///                   is not a directory).
/// * `Err(String)` - If the entry could not be read.
pub fn is_working_tree_root(dir: &Path) -> Result<bool, String> {
    use std::io::ErrorKind;

    let marker = dir.join(FOLDER_NAME_FORKLIFT_ROOT);

    match std::fs::symlink_metadata(&marker) {
        Ok(_) => Ok(true),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => Ok(false),
        Err(e) => Err(format!("Error while checking \"{}\": {}", marker.to_string_lossy(), e)),
    }
}

/// Discover the warehouse root, remember where the process was invoked from (relative to
/// the root), and switch the working directory to the root, so that all warehouse paths
/// (e.g. the object store) resolve correctly regardless of where forklift was invoked.
///
/// The root found is opened through [`open_warehouse_root`]; inside a bay, the warehouse its
/// redirect names is opened that way instead, once, with no further redirect followed.
///
/// # Returns
/// * `Ok(())`      - If the warehouse was entered successfully.
/// * `Err(String)` - If no warehouse was found, the root is no warehouse (a framed `not_a_warehouse`
///                   refusal) or holds a franchise that has not finished (a framed
///                   `franchise_incomplete` refusal), or the directory switch failed.
pub fn enter_warehouse() -> Result<(), String> {
    let cwd = std::env::current_dir()
        .map_err(|e| format!("Error while getting the current directory: {}", e))?;
    let root = find_warehouse_root()?;
    let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);

    // Inside a bay, the `.forklift` folder redirects to the shared warehouse: shared storage
    // (objects, refs, trust, config) resolves there, while the working directory, the
    // inventory, the current pallet, the lock and the probes of this bay's volume stay here.
    match bay_utils::classify_forklift(&forklift)? {
        bay_utils::ForkliftKind::Bay => {
            // A redirect that cannot be read stays a plain failure: one this build cannot judge
            // has no cure to offer, and an invalid one names its own way out.
            let redirect = bay_utils::read_bay_redirect(&forklift)?;
            let shared = open_bay_warehouse(&root, &redirect).map_err(String::from)?;
            globals::set_bay_context(shared.join(FOLDER_NAME_FORKLIFT_ROOT), forklift, redirect.name);
        }
        _ => {
            open_warehouse_root(&root, Head::Client).map_err(String::from)?;
        }
    }

    let relative = cwd.strip_prefix(&root).unwrap_or(Path::new(""));
    let _ = CWD_RELATIVE_TO_ROOT.set(respell_as_listed(&root, relative)?);

    std::env::set_current_dir(&root)
        .map_err(|e| format!("Error while switching to the working tree root \"{}\": {}", root.to_string_lossy(), e))
}

/// The head a refusal's next step is written for. The command that prepares a warehouse differs
/// between them, and so does what fills a re-created one with its history again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Head {
    /// The `forklift` CLI.
    Client,

    /// `forklift-server`, which serves the warehouse it opens.
    Server,
}

impl Head {
    /// The command that prepares a warehouse at `root` (absolute), quoted for a next step.
    fn prepare_command(self, root: &Path) -> String {
        match self {
            Head::Client => format!("\"forklift prepare\" in \"{}\"", root.to_string_lossy()),
            Head::Server => format!("\"forklift-server prepare --root {}\"", root.to_string_lossy()),
        }
    }

    /// What gives a re-created warehouse back the history it held.
    fn refill_step(self) -> &'static str {
        match self {
            Head::Client => "import the history again (\"forklift import-git\") or franchise it again",
            Head::Server => "lift each pallet into it again (\"forklift lift\") from a warehouse that holds them",
        }
    }
}

/// Open the warehouse whose root is `root`, the one way every head decides that a directory is a
/// warehouse. Only the `.forklift` at `root` is judged ([`bay_utils::classify_forklift`]); a bay's
/// redirect is never followed, so a bay's folder is refused here like any other non-warehouse. A
/// franchise that has not finished is refused before the marker is asked for, since it may not
/// have written one yet. Next steps are written for `head`.
///
/// # Returns
/// * `Ok(PathBuf)`     - `root`: its `.forklift` holds a warehouse marker this build reads.
/// * `Err(CoreError)`  - A `not_a_warehouse` refusal naming the absolute root, a
///                       `franchise_incomplete` refusal, the older bay format's refusal, or the
///                       `.forklift` could not be judged.
pub fn open_warehouse_root(root: &Path, head: Head) -> Result<PathBuf, CoreError> {
    let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);

    match bay_utils::classify_forklift(&forklift)? {
        bay_utils::ForkliftKind::Warehouse => {
            franchise_utils::gate(root)?;
            Ok(root.to_path_buf())
        }
        bay_utils::ForkliftKind::Unprepared => {
            franchise_utils::gate(root)?;
            Err(unprepared_refusal(root, head)?)
        }
        // The CLI runs inside a bay as in its warehouse; the server serves no bay, in any format.
        bay_utils::ForkliftKind::OldFormatBay if head == Head::Client => Err(bay_utils::old_format_bay_refusal(root)),
        bay_utils::ForkliftKind::Bay | bay_utils::ForkliftKind::OldFormatBay => {
            let shown = shown_root(root);
            let next_step = match head {
                Head::Client => format!(
                    "Use the warehouse the bay belongs to, or run the command inside the bay \"{}\".",
                    shown.to_string_lossy()
                ),
                Head::Server => "Use the warehouse the bay belongs to instead.".to_string(),
            };
            Err(CoreError::refusal(
                RefusalCode::NotAWarehouse,
                format!("\"{}\" is a bay's working directory, not a warehouse.", shown.to_string_lossy()),
                next_step,
            ))
        }
        bay_utils::ForkliftKind::Absent => {
            let shown = shown_root(root);
            Err(CoreError::refusal(
                RefusalCode::NotAWarehouse,
                format!("\"{}\" holds no \"{}\" folder, so it is not a warehouse.", shown.to_string_lossy(), FOLDER_NAME_FORKLIFT_ROOT),
                format!("Run {} to create a warehouse there.", head.prepare_command(&shown)),
            ))
        }
    }
}

/// Open the warehouse a bay at `bay_root` redirects to: one [`open_warehouse_root`] of the
/// redirect's warehouse root, never a chain of redirects. A redirect to another bay (which `bay add`
/// run inside a bay once wrote) and a redirect to a warehouse that is gone get refusals saying how
/// to get a working bay back.
///
/// # Returns
/// * `Ok(PathBuf)`    - The warehouse root the redirect names.
/// * `Err(CoreError)` - As [`open_warehouse_root`], naming the warehouse the redirect names.
fn open_bay_warehouse(bay_root: &Path, redirect: &bay_utils::BayRedirect) -> Result<PathBuf, CoreError> {
    let target = &redirect.warehouse_root;
    let target_forklift = target.join(FOLDER_NAME_FORKLIFT_ROOT);
    let bay = shown_root(bay_root);

    match bay_utils::classify_forklift(&target_forklift)? {
        bay_utils::ForkliftKind::Bay => {
            // The bay was registered in the warehouse the redirect's bay belongs to.
            let warehouse = bay_utils::read_bay_redirect(&target_forklift)
                .map(|other| format!("\"{}\"", other.warehouse_root.to_string_lossy()))
                .unwrap_or_else(|_| format!("the warehouse the bay \"{}\" belongs to", target.to_string_lossy()));

            Err(CoreError::refusal(
                RefusalCode::NotAWarehouse,
                format!(
                    "The bay \"{}\" redirects to \"{}\", which is another bay, not a warehouse.",
                    bay.to_string_lossy(), target.to_string_lossy()
                ),
                format!(
                    "Run \"forklift bay remove {}\" in {}, delete \"{}\", then run \"forklift bay add\" there \
                    under a new name into an empty folder.",
                    redirect.name, warehouse, bay.join(FOLDER_NAME_FORKLIFT_ROOT).to_string_lossy()
                ),
            ))
        }
        bay_utils::ForkliftKind::Absent => Err(CoreError::refusal(
            RefusalCode::NotAWarehouse,
            format!(
                "The bay \"{}\" belongs to the warehouse \"{}\", which holds no \"{}\" folder.",
                bay.to_string_lossy(), target.to_string_lossy(), FOLDER_NAME_FORKLIFT_ROOT
            ),
            format!(
                "Restore the warehouse at \"{}\", or delete \"{}\".",
                target.to_string_lossy(), bay.join(FOLDER_NAME_FORKLIFT_ROOT).to_string_lossy()
            ),
        )),
        bay_utils::ForkliftKind::Unprepared => {
            franchise_utils::gate(target)?;
            // Prepare's own remedy would build a warehouse that does not know this bay.
            Err(CoreError::refusal(
                RefusalCode::NotAWarehouse,
                format!(
                    "The bay \"{}\" belongs to \"{}\", whose \"{}\" holds no warehouse marker.",
                    bay.to_string_lossy(), target.to_string_lossy(), FOLDER_NAME_FORKLIFT_ROOT
                ),
                format!(
                    "Re-create the warehouse at \"{}\" (move its \"{}\" aside, run \"forklift prepare\" there, \
                    then import the history again with \"forklift import-git\" or franchise it again), then move \
                    \"{}\" aside and add the bay again with \"forklift bay add\" in \"{}\".",
                    target.to_string_lossy(), FOLDER_NAME_FORKLIFT_ROOT,
                    bay.to_string_lossy(), target.to_string_lossy()
                ),
            ))
        }
        _ => open_warehouse_root(target, Head::Client),
    }
}

/// Refuse a directory `forklift prepare` (or `forklift-server prepare`) is asked to make a
/// warehouse of, when what stands there is not for prepare to take: an unfinished franchise, a
/// bay's folder, or a `.forklift` with something in it but no warehouse marker. A missing or empty
/// `.forklift` is prepared, and so is a warehouse, which prepare completes.
///
/// [`prepare_warehouse`] itself does not ask this: franchise, the server's create and the Lambda
/// scratch fill folders they made or claimed themselves.
///
/// # Returns
/// * `Ok(())`         - Prepare may run at `root`.
/// * `Err(CoreError)` - A `franchise_incomplete` or `not_a_warehouse` refusal, or the `.forklift`
///                      could not be judged.
pub fn check_preparable(root: &Path, head: Head) -> Result<(), CoreError> {
    franchise_utils::gate(root)?;

    let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);

    match bay_utils::classify_forklift(&forklift)? {
        bay_utils::ForkliftKind::Absent | bay_utils::ForkliftKind::Warehouse => Ok(()),
        bay_utils::ForkliftKind::Unprepared if is_empty_folder(&forklift)? => Ok(()),
        bay_utils::ForkliftKind::Unprepared => Err(unprepared_refusal(root, head)?),
        bay_utils::ForkliftKind::Bay | bay_utils::ForkliftKind::OldFormatBay => Err(CoreError::refusal(
            RefusalCode::NotAWarehouse,
            format!(
                "\"{}\" belongs to a bay, so a warehouse cannot be prepared there.",
                forklift.to_string_lossy()
            ),
            "Remove the bay from its warehouse first (\"forklift bay remove <name>\" in the warehouse).",
        )),
    }
}

/// The refusal for a `.forklift` at `root` with no warehouse marker (and no redirect). An empty one
/// may sit inside a warehouse, where the user most likely meant that one: then removing it comes
/// first, so the next step never builds a nested warehouse the outer one would refuse. One with
/// anything in it is from before warehouses were marked, or not a warehouse; prepare refuses it,
/// so it is moved aside and the warehouse made again.
fn unprepared_refusal(root: &Path, head: Head) -> Result<CoreError, CoreError> {
    let shown = shown_root(root);
    let forklift = shown.join(FOLDER_NAME_FORKLIFT_ROOT);

    if !is_empty_folder(&forklift)? {
        return Ok(CoreError::refusal(
            RefusalCode::NotAWarehouse,
            format!(
                "\"{}\" holds no warehouse marker: it was made before warehouses were marked, or is not a warehouse.",
                forklift.to_string_lossy()
            ),
            format!(
                "Move \"{}\" aside, run {}, then {}; or, if it was a bay's, restore its redirect.",
                forklift.to_string_lossy(), head.prepare_command(&shown), head.refill_step()
            ),
        ));
    }

    let message = format!("\"{}\" is empty, so it is not a warehouse.", forklift.to_string_lossy());
    // The working tree the user would be in without the empty folder, found as discovery finds one,
    // and named only when it is a warehouse: a bay or another unmarked folder is no place to send them.
    let outer = shown.ancestors().skip(1).find(|dir| dir.join(FOLDER_NAME_FORKLIFT_ROOT).exists())
        .filter(|dir| bay_utils::classify_forklift(&dir.join(FOLDER_NAME_FORKLIFT_ROOT)) == Ok(bay_utils::ForkliftKind::Warehouse));

    let next_step = match outer {
        Some(outer) => format!(
            "Remove the empty \"{}\" to use the warehouse at \"{}\", or run {}.",
            forklift.to_string_lossy(), outer.to_string_lossy(), head.prepare_command(&shown)
        ),
        None => format!("Run {}.", head.prepare_command(&shown)),
    };

    Ok(CoreError::refusal(RefusalCode::NotAWarehouse, message, next_step))
}

/// `root` as an absolute path, for the refusals that name it.
fn shown_root(root: &Path) -> PathBuf {
    std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf())
}

/// Whether the folder `path` (followed, as classification follows it) holds nothing.
fn is_empty_folder(path: &Path) -> Result<bool, CoreError> {
    std::fs::read_dir(path)
        .map(|mut entries| entries.next().is_none())
        .map_err(|e| CoreError::from(format!("Error while reading \"{}\": {}", path.to_string_lossy(), e)))
}

/// Respell each component of `relative`, a directory below `root`, as its parent lists it.
///
/// A volume can open a directory by a spelling its parent does not list (`DOCS` for `Docs` on a
/// case-insensitive one), and a process started there may see that spelling as its current
/// directory. Joined onto a user path, it would mint a key beside the tracked one. The current
/// directory names one entry, so respelling it chooses nothing on the user's behalf: a component
/// the parent lists exactly is kept, and any other is replaced by the listed entry that is the
/// same entry on disk. When no listed entry can be identified as it (none is, or the volume will
/// not identify the entry), the spelling is refused rather than used: the parent's listing has
/// already shown it is not the listed name, so keeping it would mint the alias key. A parent
/// that cannot be listed (traversable but unreadable) cannot be asked, so its component keeps the
/// spelling the volume opened.
///
/// # Arguments
/// * `root`     - The warehouse root.
/// * `relative` - The directory, relative to `root`.
///
/// # Returns
/// * `Ok(PathBuf)` - `relative`, spelled as listed.
/// * `Err(String)` - If a listed parent lists no entry for a component.
fn respell_as_listed(root: &Path, relative: &Path) -> Result<PathBuf, String> {
    let mut respelled = PathBuf::new();

    for component in relative.components() {
        let name = component.as_os_str();
        let parent = root.join(&respelled);

        let Ok(listing) = std::fs::read_dir(&parent)
            .and_then(|entries| entries.map(|entry| entry.map(|entry| entry.file_name())).collect::<Result<Vec<_>, _>>()) else {
            respelled.push(name);
            continue;
        };

        if listing.iter().any(|listed| listed == name) {
            respelled.push(name);
            continue;
        }

        // A name that is not valid UTF-8 is no warehouse key, so only UTF-8 entries can be it.
        let names: Vec<String> = listing.into_iter().filter_map(|listed| listed.into_string().ok()).collect();
        let listed = name.to_str().and_then(|name| alias_utils::listed_alias(&parent, name, &names));

        let Some(listed) = listed else {
            return Err(format!(
                "The current directory's component \"{}\" is not listed in \"{}\" under that name, and \
                no entry listed there could be identified as it, so Forklift cannot tell which directory it is.",
                name.to_string_lossy(), parent.to_string_lossy()
            ));
        };

        respelled.push(listed);
    }

    Ok(respelled)
}

/// Get the directory the user invoked forklift from, relative to the warehouse root.
/// Returns an empty path if [`enter_warehouse`] has not been called (e.g. in tests
/// that operate from the root directly).
pub fn cwd_relative_to_root() -> PathBuf {
    CWD_RELATIVE_TO_ROOT.get().cloned().unwrap_or_default()
}

/// The file in a warehouse's `.forklift` folder that says `prepare` finished building it: written
/// last, so a folder holding it holds every piece of a warehouse.
pub const FILE_NAME_WAREHOUSE_MARKER: &str = "warehouse";
/// The record kind and format version of the warehouse marker (see [`record_utils`]). Version 1
/// holds no entries.
const WAREHOUSE_MARKER_RECORD: &str = "warehouse";
const WAREHOUSE_MARKER_VERSION: u32 = 1;

/// The lock file in a staging folder of [`place_forklift`], held while the folder is in use.
const FILE_NAME_STAGING_LOCK: &str = "lock";

/// Whether the `.forklift` folder `forklift` holds a warehouse marker. A marker this build cannot
/// read is an error, never taken for one or for none: it may be a newer Forklift's.
///
/// # Returns
/// * `Ok(true)`    - It holds a marker this build reads.
/// * `Ok(false)`   - It holds none.
/// * `Err(String)` - The marker cannot be read, or is in a format this build does not know.
pub fn has_warehouse_marker(forklift: &Path) -> Result<bool, String> {
    let path = forklift.join(FILE_NAME_WAREHOUSE_MARKER);

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => return Ok(false),
        Err(e) => return Err(format!("Error while reading the warehouse marker \"{}\": {}", path.to_string_lossy(), e)),
    };

    let unknown = |reason: String| format!(
        "The warehouse marker \"{}\" {} Forklift cannot tell whether \"{}\" is a warehouse, so it does \
        not use it.", path.to_string_lossy(), reason, forklift.to_string_lossy()
    );

    match record_utils::from_bytes(&bytes, WAREHOUSE_MARKER_RECORD, WAREHOUSE_MARKER_VERSION) {
        Ok(entries) if entries.is_empty() => Ok(true),
        Ok(_) => Err(unknown("holds entries this version of Forklift does not know.".to_string())),
        Err(e) => Err(unknown(e.describe())),
    }
}

/// Write the warehouse marker in `forklift`, atomically and synced with its folder.
fn write_warehouse_marker(forklift: &Path) -> Result<(), String> {
    let bytes = record_utils::to_bytes(WAREHOUSE_MARKER_RECORD, WAREHOUSE_MARKER_VERSION, std::iter::empty::<&str>());

    file_utils::replace_file_atomically(&forklift.join(FILE_NAME_WAREHOUSE_MARKER), &bytes)
        .map_err(|e| format!("Error while marking \"{}\" as a warehouse: {}", forklift.to_string_lossy(), e))
}

/// Prepare a warehouse in the current folder (the storage root in scope). Every step is
/// idempotent: only the pieces that are missing get created, and the warehouse marker
/// ([`FILE_NAME_WAREHOUSE_MARKER`]) is written last.
///
/// Where no `.forklift` stands, or an empty one, the whole folder is built under a staging name and
/// renamed into place ([`place_forklift`]), so a prepare stopped at any point leaves no half-built
/// `.forklift`. Where a `.forklift` with anything in it stands (a warehouse, or a franchise's claim),
/// the missing pieces are added in place.
///
/// No `.forkliftignore` is written, as `git init` writes no `.gitignore`. The ignore file is
/// tracked content, and a template written here would be an untracked file that franchise and
/// lower then refuse to overwrite with a pallet's own copy.
///
/// # Returns
/// * `Ok(Vec<String>)` - One note per created piece; empty if the warehouse was already
///                       fully prepared.
/// * `Err(String)`     - If the folder is a bay's, its marker cannot be read, or a folder or file
///                       could not be created.
pub fn prepare_warehouse() -> Result<Vec<String>, String> {
    let forklift = forklift_root();

    refuse_bay(&forklift)?;

    // An empty folder holds nothing to keep; it is replaced by a built one rather than filled, so
    // the folder is never a warehouse with pieces missing. Removing it is non-recursive, so a
    // folder something was put in since is filled in place instead, and so is one that cannot be
    // removed (a mount point, or in a directory this may not write).
    if is_empty_real_folder(&forklift)? {
        let _ = std::fs::remove_dir(&forklift);
    }

    match std::fs::symlink_metadata(&forklift) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(format!("Error while checking \"{}\": {}", forklift.to_string_lossy(), e)),
        Ok(_) => return fill_in_place(&forklift),
    }

    let mut notes = Vec::new();

    let placement = place_forklift(&forklift, |staged| {
        let _scope = globals::StorageRootScope::enter(staged.parent().unwrap_or(staged));

        // A bay's context outranks a storage-root scope, so the pieces would land in its shared
        // folder rather than the staged one.
        if forklift_root() != staged {
            return Err(format!(
                "A warehouse cannot be prepared at \"{}\" while a bay is entered.", forklift.to_string_lossy()
            ));
        }

        notes = fill(staged, &forklift)?.0;
        write_warehouse_marker(staged)
    })?;

    match placement {
        Placement::Placed => {
            notes.insert(0, format!("Created folder \"{}\".", FOLDER_NAME_FORKLIFT_ROOT));
            Ok(notes)
        }
        // Something else was put in place first: another prepare's, or a bay's redirect folder.
        Placement::Taken => match bay_utils::classify_forklift(&forklift)? {
            // Gone again already: filling it in place would build it unstaged.
            bay_utils::ForkliftKind::Absent => Err(format!(
                "\"{}\" changed while a warehouse was being prepared there. Try again.", forklift.to_string_lossy()
            )),
            _ => fill_in_place(&forklift),
        },
    }
}

/// Refuse a `.forklift` that is a bay's: filling it with warehouse pieces would report success
/// while leaving a directory that is neither.
fn refuse_bay(forklift: &Path) -> Result<(), String> {
    match bay_utils::classify_forklift(forklift)? {
        bay_utils::ForkliftKind::Bay | bay_utils::ForkliftKind::OldFormatBay => Err(format!(
            "\"{}\" belongs to a bay, so a warehouse cannot be prepared there. Remove the bay from its \
            warehouse first.", forklift.to_string_lossy()
        )),
        // A folder with no marker is one its caller made or claimed: prepare's commands refuse
        // anything else first (`check_preparable`).
        bay_utils::ForkliftKind::Warehouse | bay_utils::ForkliftKind::Unprepared | bay_utils::ForkliftKind::Absent => Ok(()),
    }
}

/// Add the missing pieces to the existing `.forklift` folder `forklift`, then the marker if it has
/// none. The folders created are synced into their parents before the marker is written, so a
/// marker never survives a power loss that the pieces it vouches for did not.
fn fill_in_place(forklift: &Path) -> Result<Vec<String>, String> {
    // Checked again here, not only before: a bay's folder can be put in place since.
    refuse_bay(forklift)?;

    let (mut notes, created) = fill(forklift, forklift)?;

    // Each created folder and every folder between it and `forklift`, once each, deepest first.
    let mut folders: Vec<&Path> = created.iter()
        .flat_map(|folder| folder.ancestors().take_while(|dir| dir.starts_with(forklift)))
        .collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    folders.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));

    for dir in folders {
        file_utils::sync_dir(dir)?;
    }

    if !has_warehouse_marker(forklift)? {
        write_warehouse_marker(forklift)?;
        notes.push(format!("Marked \"{}\" as a warehouse's folder.", FOLDER_NAME_FORKLIFT_ROOT));
    }

    Ok(notes)
}

/// Create the missing pieces of a warehouse in `forklift_root()`, which is `forklift`, but for the
/// marker. Notes name each piece under `shown`, the path the folder has once in place.
///
/// # Returns
/// * `Ok((notes, folders))` - One note per created piece, and the folders created.
/// * `Err(String)`          - A folder or file could not be created.
fn fill(forklift: &Path, shown: &Path) -> Result<(Vec<String>, Vec<PathBuf>), String> {
    let mut notes: Vec<String> = Vec::new();
    let mut created: Vec<PathBuf> = Vec::new();

    let (from, to) = (forklift.to_string_lossy().into_owned(), shown.to_string_lossy().into_owned());
    let respell = |path: &str| match path.strip_prefix(&*from) {
        Some(rest) => format!("{}{}", to, rest),
        None => path.to_string(),
    };

    let folders = [
        PathBuf::from(file_utils::get_path_objects_root()),
        PathBuf::from(file_utils::get_path_inventory_root()),
        config_utils::get_warehouse_config_folder(),
    ];

    for folder in folders {
        if file_utils::create_folder_if_not_exists(&folder)? {
            notes.push(format!("Created folder \"{}\".", respell(&folder.to_string_lossy())));
            created.push(folder);
        }
    }

    // Created here rather than by the position, so it is synced with the rest.
    let pallets = pallet_utils::get_pallets_folder();

    if file_utils::create_folder_if_not_exists(&pallets)? {
        created.push(pallets);
    }

    if pallet_utils::create_position_if_not_exists()? {
        notes.push(format!(
            "Created the pallets folder and the bay position (on pallet \"{}\").",
            pallet_utils::DEFAULT_PALLET_NAME
        ));
    }

    if config_utils::create_warehouse_config_if_not_exists()? {
        notes.push("Created warehouse configuration file.".to_string());
    }

    Ok((notes, created))
}

/// Whether `path` is a folder itself, not a link to one, with nothing in it.
fn is_empty_real_folder(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::read_dir(path)
            .map(|mut entries| entries.next().is_none())
            .map_err(|e| format!("Error while reading \"{}\": {}", path.to_string_lossy(), e)),
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("Error while checking \"{}\": {}", path.to_string_lossy(), e)),
    }
}

/// How [`place_forklift`] ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Placement {
    /// The built folder is in place.
    Placed,

    /// Something stood at the target by the time of the rename, and was left alone.
    Taken,
}

#[cfg(test)]
thread_local! {
    /// When set, the next [`place_forklift`] on this thread stops just before its rename and leaves
    /// its staging folder as a killed process would. Cleared when it fires.
    static STOP_BEFORE_RENAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// When set, the next [`place_forklift`] on this thread runs it just before its rename, as
    /// another process would. Cleared when it fires.
    static BEFORE_RENAME: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

/// Put a new `.forklift` folder in place at `forklift`, which must be absent: create a staging
/// folder `<name>.tmp~<pid>-<n>` beside it, lock it, let `build` fill `<staging>/<name>` (created
/// empty), sync that tree, and rename it to `forklift` only if nothing stands there yet
/// ([`file_utils::rename_no_replace`]), then sync the parent. A process stopped at any point leaves
/// no `.forklift`, only the staging folder, which the built-in ignore rules skip and the next call
/// in that directory removes.
///
/// The staging folder holds a lock file the call holds a kernel lock on (`File::try_lock`) for as
/// long as the folder is in use. The sweep of leftovers removes the content of a staging folder
/// only while holding its lock, and builds happen only while holding it, so a sweep never removes
/// a build in progress. A staging folder without a lock file is removed only if empty: its lock
/// file can be gone while a build holds the lock on it (a sweep deletes the file after releasing
/// it), so its content is never judged dead.
///
/// # Arguments
/// * `forklift` - Where the folder goes.
/// * `build`    - Fills the staged folder, given its path.
///
/// # Returns
/// * `Ok(Placement)` - Whether the folder went in place; the staging folder is gone either way.
/// * `Err(String)`   - The staging folder could not be made, `build` failed, or the rename failed
///                     for another reason; nothing is in place.
pub(crate) fn place_forklift(forklift: &Path, build: impl FnOnce(&Path) -> Result<(), String>) -> Result<Placement, String> {
    let name = forklift.file_name()
        .ok_or(format!("Cannot create \"{}\": it has no name.", forklift.to_string_lossy()))?
        .to_string_lossy().into_owned();
    let parent = forklift.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));

    sweep_staging_leftovers(parent, &name);

    let staging = file_utils::create_first_new_folder(file_utils::pid_numbered(&file_utils::TEMP_COUNTER, |pid, n| {
        parent.join(format!("{}.tmp~{}-{}", name, pid, n))
    })).map_err(|e| format!("Error while creating a staging folder beside \"{}\": {}", forklift.to_string_lossy(), e))?;

    let lock_path = staging.join(FILE_NAME_STAGING_LOCK);
    let lock = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&lock_path)
        .map_err(|e| format!("Error while creating the lock file \"{}\": {}", lock_path.to_string_lossy(), e))
        .and_then(|file| match file.try_lock() {
            Ok(()) => Ok(file),
            // Only a sweep takes a staging folder's lock, and it is clearing this one.
            Err(std::fs::TryLockError::WouldBlock) => Err(format!(
                "\"{}\" was being cleared away as it was created. Try again.", staging.to_string_lossy()
            )),
            Err(std::fs::TryLockError::Error(e)) => Err(format!(
                "Error while locking \"{}\": {}", lock_path.to_string_lossy(), e
            )),
        });

    let lock = match lock {
        Ok(lock) => lock,
        Err(e) => {
            let _ = std::fs::remove_file(&lock_path);
            let _ = std::fs::remove_dir(&staging);
            return Err(e);
        }
    };

    let staged = staging.join(&name);

    let built = std::fs::create_dir(&staged)
        .map_err(|e| format!("Error while creating \"{}\": {}", staged.to_string_lossy(), e))
        .and_then(|()| build(&staged))
        .and_then(|()| file_utils::sync_tree(&staged));

    #[cfg(test)]
    if STOP_BEFORE_RENAME.with(|stop| stop.replace(false)) {
        return Err("stopped before the rename".to_string());
    }

    #[cfg(test)]
    if let Some(before) = BEFORE_RENAME.with(|before| before.borrow_mut().take()) {
        before();
    }

    let placed = built.and_then(|()| match file_utils::rename_no_replace(&staged, forklift) {
        Ok(()) => Ok(Placement::Placed),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(Placement::Taken),
        Err(e) => Err(format!("Error while moving \"{}\" into place: {}", forklift.to_string_lossy(), e)),
    });

    // Still holding the lock, so no sweep is in the folder. A real folder this call created, so the
    // removal follows nothing it did not make; gone already once placed.
    let _ = file_utils::remove_unfollowed(&staged);
    drop(lock);
    let _ = std::fs::remove_file(&lock_path);
    let _ = std::fs::remove_dir(&staging);

    let placement = placed?;

    if placement == Placement::Placed {
        file_utils::sync_dir(parent)?;
    }

    Ok(placement)
}

/// Remove the staging folders of [`place_forklift`] for `name` that a stopped process left in
/// `parent`. Best-effort: a leftover that stays is skipped by the built-in ignore rules, and is
/// tried again by the next call. A link is never followed, and a folder whose lock is held is in
/// use and kept.
fn sweep_staging_leftovers(parent: &Path, name: &str) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };

    for entry in entries.flatten() {
        if entry.file_name().to_str().and_then(file_utils::staged_temp_base) != Some(name) {
            continue;
        }

        let staging = entry.path();

        if !std::fs::symlink_metadata(&staging).is_ok_and(|metadata| metadata.is_dir()) {
            continue;
        }

        let lock_path = staging.join(FILE_NAME_STAGING_LOCK);

        // Opened, never created: a lock file this created could be one a build is about to open.
        match std::fs::OpenOptions::new().read(true).write(true).open(&lock_path) {
            Ok(lock) => {
                if lock.try_lock().is_err() {
                    continue;
                }

                if let Ok(names) = std::fs::read_dir(&staging) {
                    for name in names.flatten().map(|entry| entry.file_name()) {
                        if name != FILE_NAME_STAGING_LOCK {
                            let _ = file_utils::remove_unfollowed(&staging.join(name));
                        }
                    }
                }

                drop(lock);
                let _ = std::fs::remove_file(&lock_path);
                let _ = std::fs::remove_dir(&staging);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let _ = std::fs::remove_dir(&staging);
            }
            Err(_) => (),
        }
    }
}

/// Validate a warehouse id, the one rule both server heads apply: a single portable component
/// (see [`pallet_utils::portable_component_problem`]) of at most 100 characters that does not
/// start with `.`. The id names a folder directly under forklift-server's served base and keys
/// the AWS head's ref partition, so it is stricter than a pallet name.
///
/// # Arguments
/// * `id` - The warehouse id.
///
/// # Returns
/// * `Ok(())`      - If the id is valid.
/// * `Err(String)` - The refusal, shaped `"<id>" is not a valid warehouse id: <reason>.`.
pub fn validate_warehouse_id(id: &str) -> Result<(), String> {
    match warehouse_id_problem(id) {
        None => Ok(()),
        Some(reason) => {
            let reason = pallet_utils::with_lowercase_hint(reason, id, |lowered| warehouse_id_problem(lowered).is_none());
            Err(format!("\"{}\" is not a valid warehouse id: {}.", id, reason))
        }
    }
}

/// The reason `id` is not a valid warehouse id, if it is not (see [`validate_warehouse_id`]).
fn warehouse_id_problem(id: &str) -> Option<String> {
    if id.is_empty() {
        return Some("it is empty".to_string());
    }

    if id.len() > 100 {
        return Some("it is longer than 100 characters".to_string());
    }

    if id.starts_with('.') || id.starts_with('-') {
        return Some("it must not start with \".\" or \"-\"".to_string());
    }

    pallet_utils::portable_component_problem(id, "\".\", \"_\" and \"-\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one warehouse-id rule both heads apply: the portable component grammar plus the id's
    /// own limits (one component, at most 100 characters, no leading `.`).
    #[test]
    fn a_warehouse_id_follows_the_portable_grammar_and_its_own_limits() {
        for id in ["tenant-a", "tenant_a.1", "console", "x.con", "com10"] {
            assert!(validate_warehouse_id(id).is_ok(), "expected valid: {}", id);
        }
        assert!(validate_warehouse_id(&"a".repeat(100)).is_ok(), "exactly 100 is fine");

        let cases = [
            ("", "it is empty"),
            (&*"a".repeat(101), "longer than 100 characters"),
            (".hidden", "must not start with"),
            ("-flag", "must not start with"),
            ("tenant/a", "only lowercase ASCII letters"),
            ("tenant a", "only lowercase ASCII letters"),
            ("Acme", "only lowercase ASCII letters"),
            ("acme.", "must not end with \".\""),
            ("aux", "reserved device name"),
            ("nul.tar.gz", "reserved device name"),
        ];

        for (id, reason) in cases {
            let error = validate_warehouse_id(id).expect_err(id);
            assert!(error.starts_with(&format!("\"{}\" is not a valid warehouse id: ", id)) && error.ends_with('.')
                && error.contains(reason), "{:?}: expected the reason {:?}, got {}", id, reason, error);
        }

        assert_eq!(
            validate_warehouse_id("Acme").unwrap_err(),
            "\"Acme\" is not a valid warehouse id: only lowercase ASCII letters, digits, \".\", \"_\" and \"-\" are \
            allowed; the lowercase \"acme\" is valid."
        );
    }

    /// A component the parent lists exactly is kept; one it lists under another spelling of the
    /// same entry takes the listed spelling; one it lists under no spelling is refused.
    #[test]
    fn a_current_directory_is_respelled_as_its_parents_list_it() {
        let root = std::env::temp_dir().join(format!("forklift-warehouse-respell-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("docs/Sub")).unwrap();

        assert_eq!(respell_as_listed(&root, Path::new("docs/Sub")).unwrap(), Path::new("docs/Sub"));
        assert_eq!(respell_as_listed(&root, Path::new("")).unwrap(), Path::new(""));

        let folds = root.join("DOCS").exists();
        if folds {
            assert_eq!(respell_as_listed(&root, Path::new("DOCS/sub")).unwrap(), Path::new("docs/Sub"));
        }

        let error = respell_as_listed(&root, Path::new("docs/missing")).unwrap_err();
        assert!(error.contains("\"missing\" is not listed"), "{}", error);

        std::fs::remove_dir_all(&root).ok();
    }

    /// Below a parent that can be traversed but not listed, a component keeps the spelling it was
    /// opened by, and the components above it are still respelled.
    #[cfg(unix)]
    #[test]
    fn a_current_directory_below_an_unlistable_parent_keeps_its_spelling() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("forklift-warehouse-respell-locked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("locked/inner")).unwrap();
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o100)).unwrap();

        // Root lists through any mode, so there is no unlistable parent to test.
        let unlistable = std::fs::read_dir(root.join("locked")).is_err();
        assert!(unlistable || unsafe { libc::geteuid() } == 0, "the parent was listable");
        if unlistable {
            assert_eq!(respell_as_listed(&root, Path::new("locked/inner")).unwrap(), Path::new("locked/inner"));

            if root.join("LOCKED").exists() {
                assert_eq!(respell_as_listed(&root, Path::new("LOCKED/inner")).unwrap(), Path::new("locked/inner"));
            }
        }

        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    fn fresh_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forklift-warehouse-{}-{}", label, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn prepare_at(root: &Path) -> Result<Vec<String>, String> {
        let _scope = globals::StorageRootScope::enter(root);
        prepare_warehouse()
    }

    /// The staging folders of `place_forklift` in `dir`.
    fn staging_leftovers(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir).unwrap().flatten()
            .filter(|entry| entry.file_name().to_str().and_then(file_utils::staged_temp_base) == Some(FOLDER_NAME_FORKLIFT_ROOT))
            .map(|entry| entry.path())
            .collect()
    }

    #[cfg(unix)]
    fn inode(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(path).unwrap().ino()
    }

    /// A new warehouse is built whole under a staging name, marked, and renamed into place; nothing
    /// of the staging is left, and the notes name the pieces where they now stand.
    #[test]
    fn prepare_marks_a_new_warehouse_and_leaves_no_staging_folder() {
        let root = fresh_dir("marks");
        let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);

        let notes = prepare_at(&root).unwrap();

        assert_eq!(has_warehouse_marker(&forklift), Ok(true));
        assert_eq!(std::fs::read(forklift.join(FILE_NAME_WAREHOUSE_MARKER)).unwrap(), b"/forklift-warehouse 1\n");
        for piece in ["objects", "inventory", "config"] {
            assert!(forklift.join(piece).is_dir(), "{} must be in place", piece);
        }
        assert_eq!(staging_leftovers(&root), Vec::<PathBuf>::new());
        assert_eq!(notes[0], "Created folder \".forklift\".");
        assert!(notes.iter().any(|note| note == &format!("Created folder \"{}/objects\".", forklift.to_string_lossy())), "{:?}", notes);
        assert!(notes.iter().all(|note| !note.contains(".tmp~")), "a note names a staging path: {:?}", notes);

        // Prepared again: nothing to do.
        assert_eq!(prepare_at(&root).unwrap(), Vec::<String>::new());
        assert_eq!(bay_utils::classify_forklift(&forklift), Ok(bay_utils::ForkliftKind::Warehouse));

        std::fs::remove_dir_all(&root).ok();
    }

    /// An empty `.forklift` is replaced by a built one, never filled in place.
    #[cfg(unix)]
    #[test]
    fn an_empty_forklift_folder_is_replaced_by_a_built_one() {
        let root = fresh_dir("empty");
        let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);
        std::fs::create_dir(&forklift).unwrap();
        // Held open, so the volume cannot hand the replacement the same inode.
        let _held = std::fs::File::open(&forklift).unwrap();
        let before = inode(&forklift);

        let notes = prepare_at(&root).unwrap();

        assert_ne!(inode(&forklift), before, "the empty folder was filled in place");
        assert_eq!(has_warehouse_marker(&forklift), Ok(true));
        assert_eq!(notes[0], "Created folder \".forklift\".");
        assert_eq!(staging_leftovers(&root), Vec::<PathBuf>::new());

        std::fs::remove_dir_all(&root).ok();
    }

    /// A `.forklift` with anything in it is filled in place, marker last (a franchise's claim is
    /// one); until marked, it is no warehouse.
    #[test]
    fn an_existing_forklift_folder_is_filled_in_place_and_marked() {
        let root = fresh_dir("in-place");
        let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);
        std::fs::create_dir_all(forklift.join("objects")).unwrap();
        std::fs::write(forklift.join("keep"), "mine").unwrap();
        assert_eq!(bay_utils::classify_forklift(&forklift), Ok(bay_utils::ForkliftKind::Unprepared));

        let notes = prepare_at(&root).unwrap();

        assert_eq!(std::fs::read_to_string(forklift.join("keep")).unwrap(), "mine");
        assert_eq!(has_warehouse_marker(&forklift), Ok(true));
        assert_eq!(notes.last().unwrap(), "Marked \".forklift\" as a warehouse's folder.");
        assert!(!notes.iter().any(|note| note.contains("objects")), "objects existed: {:?}", notes);
        assert!(notes.iter().any(|note| note.contains("inventory")), "{:?}", notes);

        std::fs::remove_dir_all(&root).ok();
    }

    /// A prepare stopped before its rename leaves no `.forklift`, only a staging folder the built-in
    /// ignore rules skip; the next prepare there sweeps it. Top level, and nested in a warehouse.
    #[test]
    fn a_prepare_stopped_before_its_rename_leaves_no_forklift_and_the_next_one_sweeps_it() {
        let outer = fresh_dir("stopped");
        let rules = file_utils::ignore_rules_from(None).unwrap();

        for (root, key_prefix) in [(outer.clone(), ""), (outer.join("sub"), "sub/")] {
            std::fs::create_dir_all(&root).unwrap();
            let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);

            STOP_BEFORE_RENAME.with(|stop| stop.set(true));
            assert_eq!(prepare_at(&root).unwrap_err(), "stopped before the rename");

            assert!(std::fs::symlink_metadata(&forklift).is_err(), "no `.forklift` may stand");
            let leftovers = staging_leftovers(&root);
            assert_eq!(leftovers.len(), 1, "{:?}", leftovers);
            let staging = &leftovers[0];
            assert!(staging.join(FOLDER_NAME_FORKLIFT_ROOT).join(FILE_NAME_WAREHOUSE_MARKER).is_file(), "built whole");

            let name = staging.file_name().unwrap().to_str().unwrap();
            for key in [format!("{}{}", key_prefix, name), format!("{}{}/lock", key_prefix, name),
                        format!("{}{}/.forklift/config/warehouse.toml", key_prefix, name)] {
                let parent = key.rsplit_once('/').map(|(parent, _)| parent).unwrap_or("");
                assert!(rules.excludes(&key, false, rules.coverage_of(parent)), "{} must be ignored", key);
            }

            prepare_at(&root).unwrap();

            assert_eq!(has_warehouse_marker(&forklift), Ok(true));
            assert_eq!(staging_leftovers(&root), Vec::<PathBuf>::new(), "the leftover was swept");
        }

        std::fs::remove_dir_all(&outer).ok();
    }

    /// A bay's folder put at `.forklift` while a prepare builds its own is refused, never filled.
    #[test]
    fn a_bay_put_in_place_during_a_prepare_is_refused_not_filled() {
        let root = fresh_dir("bay-race");
        let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);

        let bay = forklift.clone();
        BEFORE_RENAME.with(|before| *before.borrow_mut() = Some(Box::new(move || {
            std::fs::create_dir(&bay).unwrap();
            std::fs::write(bay.join(bay_utils::FILE_NAME_BAY_REDIRECT), "elsewhere\n").unwrap();
        })));

        let error = prepare_at(&root).unwrap_err();
        assert!(error.contains("belongs to a bay"), "{}", error);

        let names: Vec<_> = std::fs::read_dir(&forklift).unwrap().flatten().map(|entry| entry.file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from(bay_utils::FILE_NAME_BAY_REDIRECT)], "the bay's folder was filled");
        assert!(staging_leftovers(&root).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    /// A staging folder whose lock is held is in use and kept; once released it is swept. One
    /// without a lock file is removed only when empty: its lock file can be gone while a build
    /// still holds the lock on it.
    #[test]
    fn a_sweep_keeps_a_locked_staging_folder_and_a_lockless_one_with_content() {
        let root = fresh_dir("sweep");
        let make = |name: &str, lock: bool, content: bool| {
            let staging = root.join(name);
            std::fs::create_dir(&staging).unwrap();
            if lock {
                std::fs::write(staging.join(FILE_NAME_STAGING_LOCK), "").unwrap();
            }
            if content {
                std::fs::create_dir_all(staging.join(FOLDER_NAME_FORKLIFT_ROOT).join("objects")).unwrap();
            }
            staging
        };

        let live = make(".forklift.tmp~1-1", true, true);
        let dead = make(".forklift.tmp~1-2", true, true);
        let lockless_empty = make(".forklift.tmp~1-3", false, false);
        let lockless_full = make(".forklift.tmp~1-4", false, true);
        let other = make(".forklift.tmp~x-5", true, true);

        let held = std::fs::OpenOptions::new().read(true).write(true).open(live.join(FILE_NAME_STAGING_LOCK)).unwrap();
        held.try_lock().unwrap();

        prepare_at(&root).unwrap();

        assert!(live.join(FOLDER_NAME_FORKLIFT_ROOT).join("objects").is_dir(), "a held lock is never swept");
        assert!(!dead.exists(), "a staging folder whose lock can be taken is dead");
        assert!(!lockless_empty.exists());
        assert!(lockless_full.join(FOLDER_NAME_FORKLIFT_ROOT).join("objects").is_dir());
        assert!(other.exists(), "not a staging name");

        drop(held);
        sweep_staging_leftovers(&root, FOLDER_NAME_FORKLIFT_ROOT);
        assert!(!live.exists(), "released, it is swept");

        std::fs::remove_dir_all(&root).ok();
    }

    /// A marker this build cannot read is an error everywhere a warehouse is judged, and prepare
    /// neither overwrites it nor fills around it.
    #[test]
    fn a_marker_this_build_cannot_read_is_refused() {
        let root = fresh_dir("marker");
        let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);
        std::fs::create_dir_all(&forklift).unwrap();

        for bad in ["/forklift-warehouse 2\n", "/forklift-warehouse 1\nentry\n", "/forklift-warehouse 1", "warehouse\n"] {
            std::fs::write(forklift.join(FILE_NAME_WAREHOUSE_MARKER), bad).unwrap();

            let error = has_warehouse_marker(&forklift).unwrap_err();
            assert!(error.starts_with("The warehouse marker \""), "{}", error);
            assert!(bay_utils::classify_forklift(&forklift).is_err(), "{:?} classified", bad);
            assert!(prepare_at(&root).is_err(), "{:?} prepared over", bad);
            assert_eq!(std::fs::read_to_string(forklift.join(FILE_NAME_WAREHOUSE_MARKER)).unwrap(), bad);
            assert!(!forklift.join("objects").exists(), "{:?} filled around", bad);
        }

        std::fs::write(forklift.join(FILE_NAME_WAREHOUSE_MARKER), "/forklift-warehouse 1\n").unwrap();
        assert_eq!(has_warehouse_marker(&forklift), Ok(true));
        assert_eq!(bay_utils::classify_forklift(&forklift), Ok(bay_utils::ForkliftKind::Warehouse));

        std::fs::remove_dir_all(&root).ok();
    }

    /// A franchise's claim puts its `.forklift` in place with the record inside; prepare then fills
    /// it in place and marks it, keeping the record.
    #[test]
    fn prepare_fills_a_franchise_claim_and_keeps_its_record() {
        let root = fresh_dir("claim");
        let target = root.join("t");
        let forklift = target.join(FOLDER_NAME_FORKLIFT_ROOT);
        let record = franchise_utils::FranchiseRecord {
            url: "http://example.com:9418".to_string(),
            token: None,
            request: franchise_utils::Request { pallet: None, only: Vec::new() },
            genesis: None,
            bundle: franchise_utils::BundleState::None,
            attempt: franchise_utils::Attempt::Running,
            start_census: 0,
            last_failure: None,
        };

        franchise_utils::claim(&target, &record).unwrap();
        assert_eq!(staging_leftovers(&target), Vec::<PathBuf>::new());
        assert_eq!(has_warehouse_marker(&forklift), Ok(false));

        prepare_at(&target).unwrap();

        assert_eq!(has_warehouse_marker(&forklift), Ok(true));
        assert!(forklift.join("objects").is_dir());
        assert_eq!(franchise_utils::read(&forklift), Ok(Some(record)));

        std::fs::remove_dir_all(&root).ok();
    }
}
