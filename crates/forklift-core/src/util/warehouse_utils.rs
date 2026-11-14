use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use crate::error::{CoreError, RefusalCode};
use crate::globals::{self, forklift_root, FOLDER_NAME_FORKLIFT_ROOT};
use crate::util::{alias_utils, bay_utils, config_utils, file_utils, franchise_utils, lock_utils, origin_utils, pallet_utils, record_utils, scope_utils, upgrade_utils};
use crate::util::record_utils::RecordError;
use crate::util::scope_utils::PathScope;

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
    enclosing_working_tree_root()?.ok_or_else(|| String::from(CoreError::refusal(
        RefusalCode::NotAWarehouse,
        format!(
            "Not a forklift warehouse (no \"{}\" found in this directory or any of its parents).",
            FOLDER_NAME_FORKLIFT_ROOT
        ),
        "Run \"forklift prepare\" to create a warehouse here, or change into one.",
    )))
}

/// [`find_warehouse_root`]'s walk, with "none found" as `None` rather than a refusal.
///
/// # Returns
/// * `Ok(Some(PathBuf))` - The nearest directory at or above the current one holding a
///                         `.forklift` folder.
/// * `Ok(None)`          - If there is none.
/// * `Err(String)`       - If the current directory could not be read.
fn enclosing_working_tree_root() -> Result<Option<PathBuf>, String> {
    let cwd = std::env::current_dir()
        .map_err(|e| format!("Error while getting the current directory: {}", e))?;

    let mut dir = cwd.as_path();

    loop {
        // A warehouse and a bay (§7.5) both mark the root of their working tree with a
        // `.forklift` folder; `enter_warehouse` tells them apart.
        if dir.join(FOLDER_NAME_FORKLIFT_ROOT).exists() {
            return Ok(Some(dir.to_path_buf()));
        }

        match dir.parent() {
            Some(parent) => dir = parent,
            None => return Ok(None),
        }
    }
}

/// [`enter_warehouse`] when the current directory is inside a working tree, and nothing
/// otherwise: for a command that runs anywhere but must read a warehouse's configuration when
/// it runs inside one. A working tree that is found but cannot be entered fails, as it would
/// for any warehouse command; it is never treated as "not inside one".
///
/// # Returns
/// * `Ok(true)`    - The warehouse was entered.
/// * `Ok(false)`   - The current directory is inside no working tree.
/// * `Err(String)` - As [`enter_warehouse`].
pub fn enter_warehouse_if_inside() -> Result<bool, String> {
    if enclosing_working_tree_root()?.is_none() {
        return Ok(false);
    }

    enter_warehouse()?;

    Ok(true)
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
///                   refusal) or holds a clone that has not finished (a framed
///                   `clone_incomplete` refusal), or the directory switch failed.
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
            shared.lease.hold_until_exit();
            globals::set_bay_context(shared.root.join(FOLDER_NAME_FORKLIFT_ROOT), forklift, redirect.name);
        }
        _ => {
            open_warehouse_root(&root, Head::Client).map_err(String::from)?.lease.hold_until_exit();
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
            Head::Server => format!("\"forklift-server prepare\" with --root \"{}\"", root.to_string_lossy()),
        }
    }

    /// The command that upgrades the warehouse at `root` (absolute) in place, quoted for a next step.
    fn upgrade_command(self, root: &Path) -> String {
        match self {
            Head::Client => format!("\"forklift upgrade\" in \"{}\"", root.to_string_lossy()),
            Head::Server => format!("\"forklift-server upgrade\" with --root \"{}\"", root.to_string_lossy()),
        }
    }

    /// The way out for a `.forklift` at `root` (absolute) that has content but no marker: making
    /// the warehouse again, and for the CLI, restoring a bay's lost redirect.
    pub(crate) fn recreate_step(self, forklift: &Path, root: &Path) -> String {
        match self {
            Head::Client => format!(
                "Move \"{}\" aside, run {}, then import the history again (\"forklift git import \
                <git-repository>\") or clone it again; or, if it was a bay's, restore its redirect.",
                forklift.to_string_lossy(), self.prepare_command(root)
            ),
            // A bay is never served, so its redirect is no way out here; and the old folder is refused
            // by every head, so the new warehouse holds only what clients lift into it.
            Head::Server => format!(
                "Move \"{}\" aside and run {}; the new warehouse starts empty, and clients lift their \
                pallets into it again (\"forklift lift\").",
                forklift.to_string_lossy(), self.prepare_command(root)
            ),
        }
    }

    /// The way to the warehouse at `outer` (absolute) for a user whose root holds an empty
    /// `.forklift` inside it, or to prepare `root` instead.
    fn outer_step(self, forklift: &Path, outer: &Path, root: &Path) -> String {
        match self {
            Head::Client => format!(
                "Remove the empty \"{}\" to use the warehouse at \"{}\", or run {}.",
                forklift.to_string_lossy(), outer.to_string_lossy(), self.prepare_command(root)
            ),
            // The server opens only the root it is given, never one above it.
            Head::Server => format!(
                "Pass --root \"{}\" to use the warehouse there, or run {}.",
                outer.to_string_lossy(), self.prepare_command(root)
            ),
        }
    }
}

/// A warehouse [`open_warehouse_root`] opened: the caller keeps `lease` for as long as it works
/// there, since nothing else keeps an upgrade from changing the format under it.
#[derive(Debug)]
pub struct OpenedWarehouse {
    pub root: PathBuf,
    pub lease: lock_utils::FormatLease,
}

/// Take the format lease of the warehouse at `root`, then judge its format. The lease comes first,
/// so the format read cannot change while the caller relies on it. The one gate between a marked
/// warehouse and everything that reads it, for what classifies a warehouse by itself too: a
/// `--warehouses` server reaches its warehouses that way.
///
/// # Returns
/// * `Ok(FormatLease)` - The warehouse is in the one format this build reads, and stays so.
/// * `Err(CoreError)`  - A refusal of [`judge_format`] or of [`lock_utils::FormatLease::shared`], or
///                       the marker could not be read or is gone.
///
/// A warehouse it opens has its marker pair made whole again: a copy missing, damaged or behind is
/// rewritten from the marker the pair resolves to (see [`read_marker_pair`]).
pub fn hold_warehouse_format(root: &Path, head: Head) -> Result<lock_utils::FormatLease, CoreError> {
    let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);
    let lease = lock_utils::FormatLease::shared(&forklift)?;

    let pair = read_marker_pair(&forklift)?;
    let (marker, bytes) = pair.chosen.ok_or_else(|| format!(
        "The warehouse marker in \"{}\" was removed while the warehouse was being opened.", forklift.to_string_lossy()
    ))?;

    judge_format(&marker, &shown_root(root), head, &upgrade_utils::CHAIN)?;
    heal_marker_pair(&forklift, &bytes, &pair.stale);

    Ok(lease)
}

/// Whether a build upgrading with `chain` (this build's is [`upgrade_utils::CHAIN`], reading
/// [`WAREHOUSE_FORMAT`]) may use a warehouse with `marker`, whose absolute root is `root`. An older
/// format is sent to the upgrade only when the chain reaches it. Next steps are written for `head`.
///
/// # Returns
/// * `Ok(())`         - It may.
/// * `Err(CoreError)` - A `warehouse_upgrade_incomplete`, `warehouse_format_older` or
///                      `warehouse_format_newer` refusal.
fn judge_format(marker: &WarehouseMarker, root: &Path, head: Head, chain: &upgrade_utils::UpgradeChain) -> Result<(), CoreError> {
    let shown = root.to_string_lossy();

    // An upgrade this build would have to finish from a format its chain does not reach is made
    // again here, not sent to an upgrade that refuses it.
    if marker.upgrade.is_some_and(|upgrade| upgrade.target <= chain.reads) && marker.format < chain.floor() {
        return Err(CoreError::refusal(
            RefusalCode::WarehouseFormatOlder,
            format!(
                "The warehouse at \"{}\" is in format {}, older than any format this version of Forklift \
                upgrades (format {} and later), and an upgrade of it did not finish.",
                shown, marker.format, chain.floor()
            ),
            head.recreate_step(&root.join(FOLDER_NAME_FORKLIFT_ROOT), root),
        ));
    }

    if let Some(upgrade) = &marker.upgrade {
        return Err(CoreError::refusal(
            RefusalCode::WarehouseUpgradeIncomplete,
            format!(
                "An upgrade of the warehouse at \"{}\" from format {} to format {} did not finish.",
                shown, marker.format, upgrade.target
            ),
            format!("Run {} with a build that reads format {}; it resumes the upgrade.", head.upgrade_command(root), upgrade.target),
        ));
    }

    match marker.format.cmp(&chain.reads) {
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Less if marker.format < chain.floor() => Err(CoreError::refusal(
            RefusalCode::WarehouseFormatOlder,
            format!(
                "The warehouse at \"{}\" is in format {}, older than any format this version of Forklift \
                upgrades (format {} and later).",
                shown, marker.format, chain.floor()
            ),
            head.recreate_step(&root.join(FOLDER_NAME_FORKLIFT_ROOT), root),
        )),
        std::cmp::Ordering::Less => Err(CoreError::refusal(
            RefusalCode::WarehouseFormatOlder,
            format!(
                "The warehouse at \"{}\" is in format {}, older than the format {} this version of Forklift reads.",
                shown, marker.format, chain.reads
            ),
            format!("Run {}.", head.upgrade_command(root)),
        )),
        std::cmp::Ordering::Greater => Err(CoreError::refusal(
            RefusalCode::WarehouseFormatNewer,
            format!(
                "The warehouse at \"{}\" is in format {}, newer than the format {} this version of Forklift reads.",
                shown, marker.format, chain.reads
            ),
            "Update this Forklift binary to one that reads that format.",
        )),
    }
}

/// Open the warehouse whose root is `root`, the one way every head decides that a directory is a
/// warehouse. Only the `.forklift` at `root` is judged ([`bay_utils::classify_forklift`]); a bay's
/// redirect is never followed, so a bay's folder is refused here like any other non-warehouse. A
/// clone that has not finished is refused before the marker is asked for, since it may not
/// have written one yet. Next steps are written for `head`.
///
/// # Returns
/// * `Ok(OpenedWarehouse)` - `root`: its `.forklift` holds a warehouse in the one format this build
///                           reads, and the lease that keeps it there.
/// * `Err(CoreError)`      - A `not_a_warehouse` refusal naming the absolute root, a
///                           `clone_incomplete` refusal, the older bay format's refusal, a refusal
///                           of [`hold_warehouse_format`], or the `.forklift` could not be judged.
pub fn open_warehouse_root(root: &Path, head: Head) -> Result<OpenedWarehouse, CoreError> {
    let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);

    match bay_utils::classify_forklift(&forklift)? {
        bay_utils::ForkliftKind::Warehouse => {
            franchise_utils::gate(root)?;
            let lease = hold_warehouse_format(root, head)?;
            Ok(OpenedWarehouse { root: root.to_path_buf(), lease })
        }
        bay_utils::ForkliftKind::Unprepared => {
            franchise_utils::gate(root)?;
            Err(unprepared_refusal(root, head)?)
        }
        bay_utils::ForkliftKind::OldFormatBay => match head {
            Head::Client => Err(bay_utils::old_format_bay_refusal(root)),
            Head::Server => Err(forklift_file_refusal(root, head)),
        },
        bay_utils::ForkliftKind::Bay => {
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
/// * `Ok(OpenedWarehouse)` - The warehouse the redirect names.
/// * `Err(CoreError)`      - As [`open_warehouse_root`], naming the warehouse the redirect names.
fn open_bay_warehouse(bay_root: &Path, redirect: &bay_utils::BayRedirect) -> Result<OpenedWarehouse, CoreError> {
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
                    "The bay \"{}\" belongs to \"{}\", whose \"{}\" holds neither a warehouse marker nor its copy.",
                    bay.to_string_lossy(), target.to_string_lossy(), FOLDER_NAME_FORKLIFT_ROOT
                ),
                format!(
                    "Re-create the warehouse at \"{}\" (move its \"{}\" aside, run \"forklift prepare\" there, \
                    then import the history again with \"forklift git import <git-repository>\" or clone it again), then move \
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
/// warehouse of, when what stands there is not for prepare to take: an unfinished clone, a
/// bay's folder, or a `.forklift` with something in it but no warehouse marker. A missing or empty
/// `.forklift` is prepared, and so is a warehouse in this build's format, which prepare completes;
/// one in another format is refused, since completing it would add this format's pieces under
/// the other's number.
///
/// [`prepare_warehouse`] itself does not ask this: clone, the server's create and the Lambda
/// scratch fill folders they made or claimed themselves.
///
/// # Returns
/// * `Ok(())`         - Prepare may run at `root`.
/// * `Err(CoreError)` - A `clone_incomplete` or `not_a_warehouse` refusal, a refusal of
///                      [`hold_warehouse_format`], or the `.forklift` could not be judged.
pub fn check_preparable(root: &Path, head: Head) -> Result<(), CoreError> {
    franchise_utils::gate(root)?;

    let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);

    match bay_utils::classify_forklift(&forklift)? {
        bay_utils::ForkliftKind::Absent => Ok(()),
        bay_utils::ForkliftKind::Warehouse => hold_warehouse_format(root, head).map(|_| ()),
        bay_utils::ForkliftKind::Unprepared if is_empty_folder(&forklift)? => Ok(()),
        bay_utils::ForkliftKind::Unprepared => Err(unprepared_refusal(root, head)?),
        bay_utils::ForkliftKind::OldFormatBay if head == Head::Server => Err(forklift_file_refusal(root, head)),
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

/// The server's refusal for a `.forklift` at `root` that is a file. The server serves no bay, so
/// converting one is no way out; and a file there may be anything, so it is not named a bay.
fn forklift_file_refusal(root: &Path, head: Head) -> CoreError {
    let shown = shown_root(root);
    CoreError::refusal(
        RefusalCode::NotAWarehouse,
        format!(
            "\"{}\" is a file, not a warehouse's folder.",
            shown.join(FOLDER_NAME_FORKLIFT_ROOT).to_string_lossy()
        ),
        format!("Move it aside and run {}.", head.prepare_command(&shown)),
    )
}

/// The refusal for a `.forklift` at `root` with no warehouse marker (and no redirect). An empty one
/// may sit inside a warehouse, where the user most likely meant that one: then removing it comes
/// first, so the next step never builds a nested warehouse the outer one would refuse. One with
/// anything in it lost both files of its marker pair, or is not a warehouse; prepare refuses it,
/// so it is moved aside and the warehouse made again.
fn unprepared_refusal(root: &Path, head: Head) -> Result<CoreError, CoreError> {
    let shown = shown_root(root);
    let forklift = shown.join(FOLDER_NAME_FORKLIFT_ROOT);

    if !is_empty_folder(&forklift)? {
        return Ok(CoreError::refusal(
            RefusalCode::NotAWarehouse,
            format!(
                "\"{}\" holds neither a warehouse marker nor its copy, so it is not a warehouse Forklift can open.",
                forklift.to_string_lossy()
            ),
            head.recreate_step(&forklift, &shown),
        ));
    }

    let message = format!("\"{}\" is empty, so it is not a warehouse.", forklift.to_string_lossy());
    // The working tree the user would be in without the empty folder, found as discovery finds one,
    // and named only when it is a warehouse: a bay or another unmarked folder is no place to send them.
    let outer = shown.ancestors().skip(1).find(|dir| dir.join(FOLDER_NAME_FORKLIFT_ROOT).exists())
        .filter(|dir| bay_utils::classify_forklift(&dir.join(FOLDER_NAME_FORKLIFT_ROOT)) == Ok(bay_utils::ForkliftKind::Warehouse));

    let next_step = match outer {
        Some(outer) => head.outer_step(&forklift, outer, &shown),
        None => format!("Run {}.", head.prepare_command(&shown)),
    };

    Ok(CoreError::refusal(RefusalCode::NotAWarehouse, message, next_step))
}

/// `root` as an absolute path, for the refusals that name it.
pub(crate) fn shown_root(root: &Path) -> PathBuf {
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

/// The file in a warehouse's `.forklift` folder that says `prepare` finished building it, and what
/// the warehouse holds of the history it names ([`WarehouseMode`]): written last, so a folder
/// holding it holds every piece of a warehouse. Its twin [`FILE_NAME_WAREHOUSE_MARKER_COPY`] holds
/// the same bytes, so one lost or damaged file never makes an intact store unreachable.
pub const FILE_NAME_WAREHOUSE_MARKER: &str = "warehouse";
/// The second copy of the warehouse marker, written just before [`FILE_NAME_WAREHOUSE_MARKER`] by
/// the same writer, and read when the primary is missing or damaged.
pub const FILE_NAME_WAREHOUSE_MARKER_COPY: &str = "warehouse-copy";
/// The record kind and format version of the warehouse marker (see [`record_utils`]). Version 4 is
/// frozen: every future build must read it to tell a warehouse of another format from anything else.
/// It holds `format N`, then the mode's word, then an `upgrading M S` entry while an upgrade to format
/// `M` stands at step `S`, and last `digest H`, the blake3 hash of every byte before that line, so
/// damage reads as damage rather than as a format or a mode. Versions 1 to 3 carried no digest and
/// are refused.
const WAREHOUSE_MARKER_RECORD: &str = "warehouse";
const WAREHOUSE_MARKER_VERSION: u32 = 4;
/// The entry prefix of a marker's digest, its last entry.
const MARKER_DIGEST_PREFIX: &str = "digest ";

/// The one warehouse format this build reads (docs/format/WAREHOUSE_FORMAT.md).
pub const WAREHOUSE_FORMAT: u32 = 1;

/// What a warehouse holds of the history it names, fixed when it is prepared and recorded in its
/// marker. The marker is the one file whose absence makes a warehouse unopenable, so the mode
/// cannot be deleted into a claim the store does not back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarehouseMode {
    /// Everything its history reaches.
    Full,
    /// Only the paths of its fetch scope (`clone --only`).
    Sparse,
    /// Content fetched only when read (`clone --cache`).
    Cache,
}

impl WarehouseMode {
    /// The mode's entry in the marker.
    pub fn word(self) -> &'static str {
        match self {
            WarehouseMode::Full => "full",
            WarehouseMode::Sparse => "sparse",
            WarehouseMode::Cache => "cache",
        }
    }

    pub(crate) fn from_word(word: &str) -> Option<WarehouseMode> {
        [WarehouseMode::Full, WarehouseMode::Sparse, WarehouseMode::Cache].into_iter()
            .find(|mode| mode.word() == word)
    }
}

/// The mode [`prepare_warehouse`] gives a warehouse it marks, with what a partial one needs
/// written before its marker: its origin, and a sparse one's scopes.
#[derive(Clone, Copy, Debug)]
pub enum PrepareMode<'a> {
    Full,
    /// `scope` is the fetch scope, which is also the main tree's first scope. Never full.
    Sparse { scope: &'a PathScope, origin: &'a str },
    Cache { origin: &'a str },
}

impl PrepareMode<'_> {
    /// The mode recorded in the marker.
    pub fn mode(&self) -> WarehouseMode {
        match self {
            PrepareMode::Full => WarehouseMode::Full,
            PrepareMode::Sparse { .. } => WarehouseMode::Sparse,
            PrepareMode::Cache { .. } => WarehouseMode::Cache,
        }
    }
}

/// The lock file in a staging folder of [`place_forklift`], held while the folder is in use.
const FILE_NAME_STAGING_LOCK: &str = "lock";

/// What a warehouse marker records, read through its frozen framing, whatever the format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WarehouseMarker {
    /// The warehouse's format.
    pub(crate) format: u32,

    /// The mode's word. Only [`WAREHOUSE_FORMAT`]'s words are this build's to interpret.
    pub(crate) mode: String,

    /// The upgrade that was started and has not finished.
    pub(crate) upgrade: Option<MarkerUpgrade>,
}

/// An upgrade recorded in a warehouse marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MarkerUpgrade {
    /// The format it moves to.
    pub(crate) target: u32,

    /// The steps of it that have finished.
    pub(crate) step: u32,
}

/// A marker's number as its writer spells it: plain decimal digits, no sign and no leading zero.
fn parse_marker_number(text: &str) -> Option<u32> {
    let canonical = text.bytes().all(|byte| byte.is_ascii_digit()) && (text == "0" || !text.starts_with('0'));

    canonical.then(|| text.parse().ok()).flatten()
}

/// Parse a marker's entries, per the framing frozen at record version 3.
fn parse_marker_entries(entries: &[String]) -> Result<WarehouseMarker, String> {
    let (format, mode, upgrade) = match entries {
        [format, mode] => (format, mode, None),
        [format, mode, upgrade] => (format, mode, Some(upgrade)),
        _ => return Err(format!("holds {} entries where it should hold its format, its mode and at most one upgrade.", entries.len())),
    };

    let format = format.strip_prefix("format ").and_then(parse_marker_number)
        .ok_or_else(|| format!("names no format where it should (\"{}\").", format.escape_debug()))?;

    let upgrade = upgrade.map(|entry| {
        entry.strip_prefix("upgrading ").and_then(|rest| rest.split_once(' '))
            .and_then(|(target, step)| Some(MarkerUpgrade { target: parse_marker_number(target)?, step: parse_marker_number(step)? }))
            .ok_or_else(|| format!("holds a malformed upgrade (\"{}\").", entry.escape_debug()))
    }).transpose()?;

    Ok(WarehouseMarker { format, mode: mode.clone(), upgrade })
}

/// The warehouse marker in `forklift`, read through its frozen framing. It says nothing about
/// whether this build may use the warehouse: [`judge_format`] does.
///
/// # Returns
/// * `Ok(Some(marker))` - It holds a marker.
/// * `Ok(None)`         - It holds none.
/// * `Err(String)`      - The marker cannot be read, or its framing is not the frozen one.
pub(crate) fn read_marker(forklift: &Path) -> Result<Option<WarehouseMarker>, String> {
    read_marker_pair(forklift).map(|pair| pair.chosen.map(|(marker, _)| marker))
}

/// One file of the marker pair, as read.
#[derive(Debug)]
enum MarkerFile {
    Absent,
    /// A marker in the frozen framing whose digest matches, with the bytes it was read from.
    Valid(WarehouseMarker, Vec<u8>),
    /// A file that is no marker this build can use: the refusal saying why.
    Unusable(String),
}

/// The marker pair of a `.forklift` folder, resolved.
#[derive(Debug)]
struct MarkerPair {
    /// The marker the pair records, with its bytes: the primary's when it is valid, else the copy's.
    chosen: Option<(WarehouseMarker, Vec<u8>)>,
    /// The files of the pair that do not hold the chosen marker's bytes.
    stale: Vec<&'static str>,
}

/// Read both files of the marker pair in `forklift` and resolve them. A valid primary is the
/// marker; else a valid copy is. Each choice names a state a stopped [`write_marker`] leaves: a copy
/// alone was written after every piece it vouches for, and a primary one write behind its copy is
/// what a stop just before that write leaves. Only a pair with neither file holds no marker: a file
/// that is there and unusable, with no valid twin, is an error, never taken for none, so nothing
/// marks a warehouse again over damage.
fn read_marker_pair(forklift: &Path) -> Result<MarkerPair, String> {
    let names = [FILE_NAME_WAREHOUSE_MARKER, FILE_NAME_WAREHOUSE_MARKER_COPY];
    let [primary, copy] = names.map(|name| read_marker_file(forklift, name));
    let (primary, copy) = (primary?, copy?);

    let (marker, bytes, other, other_name) = match (primary, copy) {
        (MarkerFile::Valid(marker, bytes), copy) => (marker, bytes, copy, FILE_NAME_WAREHOUSE_MARKER_COPY),
        (primary, MarkerFile::Valid(marker, bytes)) => (marker, bytes, primary, FILE_NAME_WAREHOUSE_MARKER),
        (MarkerFile::Absent, MarkerFile::Absent) => return Ok(MarkerPair { chosen: None, stale: Vec::new() }),
        (MarkerFile::Unusable(refusal), MarkerFile::Unusable(_)) => return Err(format!(
            "{} Its copy \"{}\" cannot be used either, so nothing records what the warehouse holds.",
            refusal, forklift.join(FILE_NAME_WAREHOUSE_MARKER_COPY).to_string_lossy()
        )),
        (MarkerFile::Unusable(refusal), MarkerFile::Absent) | (MarkerFile::Absent, MarkerFile::Unusable(refusal)) => return Err(refusal),
    };

    let other_holds_it = matches!(&other, MarkerFile::Valid(_, other_bytes) if *other_bytes == bytes);
    let stale = if other_holds_it { Vec::new() } else { vec![other_name] };

    Ok(MarkerPair { chosen: Some((marker, bytes)), stale })
}

/// Read one file of the marker pair. Only a failure to read it at all is an error.
fn read_marker_file(forklift: &Path, name: &str) -> Result<MarkerFile, String> {
    let path = forklift.join(name);

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => return Ok(MarkerFile::Absent),
        Err(e) => return Err(format!("Error while reading the warehouse marker \"{}\": {}", path.to_string_lossy(), e)),
    };

    Ok(match decode_marker(&bytes, &path, forklift) {
        Ok(marker) => MarkerFile::Valid(marker, bytes),
        Err(refusal) => MarkerFile::Unusable(refusal),
    })
}

/// The marker `bytes` (read from `path` in `forklift`) record, through the framing frozen at
/// record version 4, or the refusal saying why they are no marker this build can use.
fn decode_marker(bytes: &[u8], path: &Path, forklift: &Path) -> Result<WarehouseMarker, String> {
    let unknown = |reason: &str| format!(
        "The warehouse marker \"{}\" {} Forklift cannot tell whether \"{}\" is a warehouse, so it does \
        not use it.", path.to_string_lossy(), reason, forklift.to_string_lossy()
    );
    let older = |what: &str| format!(
        "The warehouse marker \"{}\" was written by an older version of Forklift, before the marker {}, \
        so Forklift does not use \"{}\". No upgrade reaches back that far. Make the warehouse again with \
        this version: clone it again, or prepare a new one and lower its history into it.",
        path.to_string_lossy(), what, forklift.to_string_lossy()
    );

    let entries = match record_utils::from_bytes(bytes, WAREHOUSE_MARKER_RECORD, WAREHOUSE_MARKER_VERSION) {
        Ok(entries) => entries,
        Err(RecordError::OtherVersion(header)) if header == record_utils::header(WAREHOUSE_MARKER_RECORD, 3) =>
            return Err(older("carried a digest")),
        Err(RecordError::OtherVersion(header))
            if (1..3).any(|version| header == record_utils::header(WAREHOUSE_MARKER_RECORD, version)) =>
            return Err(older("recorded the warehouse's format")),
        Err(e) => return Err(unknown(&e.describe())),
    };

    let damaged = || unknown("is damaged: it does not match its digest.");
    let (digest, entries) = entries.split_last().ok_or_else(damaged)?;
    let hex = digest.strip_prefix(MARKER_DIGEST_PREFIX).ok_or_else(damaged)?;
    let body = bytes.strip_suffix(format!("{}{}\n", MARKER_DIGEST_PREFIX, hex).as_bytes()).ok_or_else(damaged)?;

    if blake3::hash(body).to_hex().as_str() != hex {
        return Err(damaged());
    }

    parse_marker_entries(entries).map_err(|reason| unknown(&reason))
}

/// Bring the files of the marker pair in `forklift` that do not hold `bytes`, the pair's chosen
/// marker, back to it, copy first as [`write_marker`] writes. Run under a shared format lease, where
/// the only other marker writer is [`fill_in_place`], which writes only where neither file is there.
/// A rewrite that fails leaves the chosen marker as it was, so it is let go and tried at the next
/// opening.
fn heal_marker_pair(forklift: &Path, bytes: &[u8], stale: &[&'static str]) {
    for name in [FILE_NAME_WAREHOUSE_MARKER_COPY, FILE_NAME_WAREHOUSE_MARKER] {
        if stale.contains(&name) {
            let _ = file_utils::replace_file_atomically(&forklift.join(name), bytes);
        }
    }
}

/// The mode a marker of this build's format records.
fn mode_of(marker: &WarehouseMarker, forklift: &Path) -> Result<WarehouseMode, String> {
    WarehouseMode::from_word(&marker.mode).ok_or_else(|| format!(
        "The warehouse marker in \"{}\" names a mode this version of Forklift does not know (\"{}\"), so \
        Forklift cannot tell what it holds.", forklift.to_string_lossy(), marker.mode.escape_debug()
    ))
}

/// Whether the `.forklift` folder `forklift` holds a warehouse marker, of any format: this decides
/// only that the folder is a warehouse. A marker that cannot be read is an error, never taken for
/// one or for none, and so is a mode the marker's own format says this build should know.
///
/// # Returns
/// * `Ok(true)`    - It holds a marker.
/// * `Ok(false)`   - It holds none.
/// * `Err(String)` - The marker cannot be read, or is not in the frozen framing.
pub fn has_warehouse_marker(forklift: &Path) -> Result<bool, String> {
    match read_marker(forklift)? {
        Some(marker) if marker.format == WAREHOUSE_FORMAT => mode_of(&marker, forklift).map(|_| true),
        Some(_) => Ok(true),
        None => Ok(false),
    }
}

/// The mode the warehouse marker in `forklift` records.
///
/// # Returns
/// * `Ok(Some(mode))` - It holds a marker of this build's format.
/// * `Ok(None)`       - It holds none.
/// * `Err(String)`    - The marker cannot be read, is in another format, whose words this build
///                      does not interpret, or does not name a mode this build knows.
pub fn read_warehouse_marker(forklift: &Path) -> Result<Option<WarehouseMode>, String> {
    let Some(marker) = read_marker(forklift)? else {
        return Ok(None);
    };

    if marker.format != WAREHOUSE_FORMAT {
        return Err(format!(
            "The warehouse marker in \"{}\" records format {}, and this version of Forklift reads format {}.",
            forklift.to_string_lossy(), marker.format, WAREHOUSE_FORMAT
        ));
    }

    mode_of(&marker, forklift).map(Some)
}

/// The mode of the warehouse in scope. A forklift root with no marker is an error, never a full
/// warehouse: what it holds is not recorded.
pub fn warehouse_mode() -> Result<WarehouseMode, String> {
    let forklift = forklift_root();

    read_warehouse_marker(&forklift)?.ok_or_else(|| format!(
        "\"{}\" holds no warehouse marker, so Forklift cannot tell what the warehouse holds.",
        forklift.to_string_lossy()
    ))
}

/// Write the warehouse marker recording `mode` in `forklift`, atomically and synced with its folder.
fn write_warehouse_marker(forklift: &Path, mode: WarehouseMode) -> Result<(), String> {
    write_marker(forklift, &WarehouseMarker { format: WAREHOUSE_FORMAT, mode: mode.word().to_string(), upgrade: None })
}

/// Write `marker` as the warehouse marker in `forklift`, in the framing frozen at record version 4:
/// to its copy and then to the primary, each atomically and synced with its folder. A write stopped
/// between the two leaves the copy one write ahead, which [`read_marker_pair`] resolves.
pub(crate) fn write_marker(forklift: &Path, marker: &WarehouseMarker) -> Result<(), String> {
    let bytes = encode_marker(marker);

    for name in [FILE_NAME_WAREHOUSE_MARKER_COPY, FILE_NAME_WAREHOUSE_MARKER] {
        file_utils::replace_file_atomically(&forklift.join(name), &bytes)
            .map_err(|e| format!("Error while writing the warehouse marker in \"{}\": {}", forklift.to_string_lossy(), e))?;
    }

    Ok(())
}

/// The bytes of `marker` in the framing frozen at record version 4: its entries, then the digest
/// of every byte before the digest's line.
pub(crate) fn encode_marker(marker: &WarehouseMarker) -> Vec<u8> {
    let format = format!("format {}", marker.format);
    let upgrade = marker.upgrade.map(|upgrade| format!("upgrading {} {}", upgrade.target, upgrade.step));
    let entries = [Some(format.as_str()), Some(marker.mode.as_str()), upgrade.as_deref()];

    let mut bytes = record_utils::to_bytes(WAREHOUSE_MARKER_RECORD, WAREHOUSE_MARKER_VERSION, entries.into_iter().flatten());
    let digest = blake3::hash(&bytes).to_hex();
    bytes.extend_from_slice(format!("{}{}\n", MARKER_DIGEST_PREFIX, digest).as_bytes());
    bytes
}

/// Marker bytes holding `entries` (each line ending in `\n`, as they follow the header), framed
/// and digested as [`write_marker`] frames them, whatever the entries say: for a test that lays down
/// a `.forklift`, or a marker of another format, by hand. Hand-typed bytes could not carry the digest.
pub fn framed_marker_bytes(entries: &str) -> Vec<u8> {
    let mut bytes = format!("{}\n{}", record_utils::header(WAREHOUSE_MARKER_RECORD, WAREHOUSE_MARKER_VERSION), entries).into_bytes();
    let digest = blake3::hash(&bytes).to_hex();
    bytes.extend_from_slice(format!("{}{}\n", MARKER_DIGEST_PREFIX, digest).as_bytes());
    bytes
}

/// The origin [`mark_for_test`] records for a partial mode.
#[cfg(test)]
pub(crate) const TEST_ORIGIN: &str = "http://origin.test";

/// Record `mode` in the marker of the warehouse in scope, replacing any marker there, for a test
/// fixture that builds a `.forklift` by hand or changes its mode midway. A partial mode records
/// [`TEST_ORIGIN`] first, as preparing one records its origin; writes no scope file.
#[cfg(test)]
pub(crate) fn mark_for_test(mode: WarehouseMode) {
    let forklift = forklift_root();
    std::fs::create_dir_all(forklift.join("config")).unwrap();
    if mode != WarehouseMode::Full {
        origin_utils::write_origin(&forklift, TEST_ORIGIN).unwrap();
    }
    write_warehouse_marker(&forklift, mode).unwrap();
}

/// Mark `forklift` as a warehouse in `mode`. A partial warehouse's origin, and a sparse one's fetch
/// scope and main-tree scope, are written first, so no marker says partial while any is missing.
/// The scopes are written here rather than through `scope_utils`' setters, which read the mode
/// this writes.
fn mark_warehouse(forklift: &Path, mode: PrepareMode) -> Result<(), String> {
    match mode {
        PrepareMode::Full => (),
        PrepareMode::Sparse { scope, origin } => {
            origin_utils::write_origin(forklift, origin)?;
            scope_utils::record_sparse_scopes(forklift, scope)?;
        }
        PrepareMode::Cache { origin } => origin_utils::write_origin(forklift, origin)?,
    }

    write_warehouse_marker(forklift, mode.mode())
}

/// Prepare a warehouse in `mode` in the current folder (the storage root in scope). Every step is
/// idempotent: only the pieces that are missing get created, and the warehouse marker
/// ([`FILE_NAME_WAREHOUSE_MARKER`]) is written last. This is the only writer of a warehouse's mode.
/// A marker already in place is kept as it is, and then no scope is written, so preparing an
/// existing warehouse never changes its mode or restores a scope file: a caller that needs a
/// particular mode reads it back.
///
/// Where no `.forklift` stands, or an empty one, the whole folder is built under a staging name and
/// renamed into place ([`place_forklift`]), so a prepare stopped at any point leaves no half-built
/// `.forklift`. Where a `.forklift` with anything in it stands (a warehouse, or a clone's claim),
/// the missing pieces are added in place.
///
/// No `.forkliftignore` is written, as `git init` writes no `.gitignore`. The ignore file is
/// tracked content, and a template written here would be an untracked file that clone and
/// lower then refuse to overwrite with a pallet's own copy.
///
/// # Returns
/// * `Ok(Vec<String>)` - One note per created piece; empty if the warehouse was already
///                       fully prepared.
/// * `Err(String)`     - If the folder is a bay's, its marker cannot be read, or a folder or file
///                       could not be created.
pub fn prepare_warehouse(mode: PrepareMode) -> Result<Vec<String>, String> {
    let forklift = forklift_root();

    refuse_bay(&forklift)?;

    // An empty folder holds nothing to keep; it is replaced by a built one rather than filled, so
    // the folder is never a warehouse with pieces missing. Removing it is non-recursive, so a
    // folder something was put in since is filled in place instead, and so is one that cannot be
    // removed (a mount point, or in a directory this may not write).
    if is_empty_real_folder(&forklift)? {
        let _ = file_utils::remove_dir(&forklift);
    }

    match std::fs::symlink_metadata(&forklift) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(format!("Error while checking \"{}\": {}", forklift.to_string_lossy(), e)),
        Ok(_) => return fill_in_place(&forklift, mode),
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
        mark_warehouse(staged, mode)
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
            _ => fill_in_place(&forklift, mode),
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
fn fill_in_place(forklift: &Path, mode: PrepareMode) -> Result<Vec<String>, String> {
    // Checked again here, not only before: a bay's folder can be put in place since.
    refuse_bay(forklift)?;

    // So can a warehouse of another format, whose number this build's pieces would then stand under.
    // Leased before the format is read, so no upgrade changes it between the read and the fill. The
    // sidecar is every format's, so taking it adds nothing another format would not read.
    let _lease = lock_utils::FormatLease::shared(forklift).map_err(String::from)?;
    match read_marker(forklift)? {
        Some(marker) if marker.upgrade.is_some() || marker.format != WAREHOUSE_FORMAT => return Err(format!(
            "\"{}\" holds a warehouse in format {}{}, so this version of Forklift, which prepares format {}, \
            cannot complete it.",
            forklift.to_string_lossy(), marker.format,
            marker.upgrade.as_ref().map_or(String::new(), |upgrade| format!(" with an upgrade to format {} unfinished", upgrade.target)),
            WAREHOUSE_FORMAT
        )),
        _ => (),
    }

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
        mark_warehouse(forklift, mode)?;
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

    // Not a note: it is no piece a user asked for, and an open makes it again when it is missing.
    lock_utils::create_format_lock(forklift)?;

    // Not a note either, for the same reason; a prepare run again writes it back.
    write_git_self_ignore(forklift)?;

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

/// The git ignore file in a `.forklift` folder ([`write_git_self_ignore`]).
pub const FILE_NAME_GIT_SELF_IGNORE: &str = ".gitignore";

/// What [`write_git_self_ignore`] writes: one rule that ignores every entry beside it, itself
/// included, so git lists nothing of the folder.
const GIT_SELF_IGNORE: &str = "# Written by Forklift, so that git never tracks this folder.\n*\n";

/// Write a git ignore file into the `.forklift` folder `forklift` unless one is there, so a git
/// working tree the folder stands in never stages it: `git add -A` would otherwise sweep up a
/// warehouse's configuration, its remote token included, or a bay's redirect. Git reads an ignore
/// file in any folder of its tree, so this needs no write to the repository's own `.git`. One found
/// there is kept as it is, whatever it holds: the user may have written it.
///
/// Written like a configuration file, not like a store object: a standing durability taint, which
/// a configuration write is often the first step out of, must not refuse it.
///
/// # Returns
/// * `Ok(())`      - The file is there.
/// * `Err(String)` - It could not be checked or written.
pub fn write_git_self_ignore(forklift: &Path) -> Result<(), String> {
    let path = forklift.join(FILE_NAME_GIT_SELF_IGNORE);

    match std::fs::symlink_metadata(&path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => file_utils::replace_file_atomically(&path, GIT_SELF_IGNORE.as_bytes())
            .map_err(|e| format!("Error while creating \"{}\": {}", path.to_string_lossy(), e)),
        Err(e) => Err(format!("Error while checking \"{}\": {}", path.to_string_lossy(), e)),
    }
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
            let _ = file_utils::remove_file(&lock_path);
            let _ = file_utils::remove_dir(&staging);
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
    let _ = file_utils::remove_file(&lock_path);
    let _ = file_utils::remove_dir(&staging);

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
                let _ = file_utils::remove_file(&lock_path);
                let _ = file_utils::remove_dir(&staging);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let _ = file_utils::remove_dir(&staging);
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
        let _ = file_utils::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("docs/Sub")).unwrap();

        assert_eq!(respell_as_listed(&root, Path::new("docs/Sub")).unwrap(), Path::new("docs/Sub"));
        assert_eq!(respell_as_listed(&root, Path::new("")).unwrap(), Path::new(""));

        let folds = root.join("DOCS").exists();
        if folds {
            assert_eq!(respell_as_listed(&root, Path::new("DOCS/sub")).unwrap(), Path::new("docs/Sub"));
        }

        let error = respell_as_listed(&root, Path::new("docs/missing")).unwrap_err();
        assert!(error.contains("\"missing\" is not listed"), "{}", error);

        file_utils::remove_dir_all(&root).ok();
    }

    /// Below a parent that can be traversed but not listed, a component keeps the spelling it was
    /// opened by, and the components above it are still respelled.
    #[cfg(unix)]
    #[test]
    fn a_current_directory_below_an_unlistable_parent_keeps_its_spelling() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("forklift-warehouse-respell-locked-{}", std::process::id()));
        let _ = file_utils::remove_dir_all(&root);
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
        file_utils::remove_dir_all(&root).ok();
    }

    fn fresh_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forklift-warehouse-{}-{}", label, std::process::id()));
        let _ = file_utils::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn list_names_for_test(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect()
    }

    fn prepare_at(root: &Path) -> Result<Vec<String>, String> {
        let _scope = globals::StorageRootScope::enter(root);
        prepare_warehouse(PrepareMode::Full)
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
        // Runs the staging tree's sync and the replaced files' syncs for real: the unit tests skip syncs otherwise.
        let _syncs = crate::util::file_utils::real_syncs();
        let root = fresh_dir("marks");
        let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);

        let notes = prepare_at(&root).unwrap();

        assert_eq!(has_warehouse_marker(&forklift), Ok(true));
        for name in [FILE_NAME_WAREHOUSE_MARKER, FILE_NAME_WAREHOUSE_MARKER_COPY] {
            assert_eq!(std::fs::read(forklift.join(name)).unwrap(), framed_marker_bytes("format 1\nfull\n"), "{}", name);
        }
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

        file_utils::remove_dir_all(&root).ok();
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

        file_utils::remove_dir_all(&root).ok();
    }

    /// A `.forklift` with anything in it is filled in place, marker last (a clone's claim is
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

        file_utils::remove_dir_all(&root).ok();
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
                assert!(rules.excludes(&key, false, false, &file_utils::RuleContext::above_root()), "{} must be ignored", key);
            }

            prepare_at(&root).unwrap();

            assert_eq!(has_warehouse_marker(&forklift), Ok(true));
            assert_eq!(staging_leftovers(&root), Vec::<PathBuf>::new(), "the leftover was swept");
        }

        file_utils::remove_dir_all(&outer).ok();
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
        file_utils::remove_dir_all(&root).ok();
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

        file_utils::remove_dir_all(&root).ok();
    }

    /// A marker this build cannot read is an error everywhere a warehouse is judged, and prepare
    /// neither overwrites it nor fills around it.
    #[test]
    fn a_marker_this_build_cannot_read_is_refused() {
        let root = fresh_dir("marker");
        let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);
        std::fs::create_dir_all(&forklift).unwrap();

        // No mode, two modes, a word this build does not know, no format, no digest, a newer version,
        // the markers from before the digest and before the format was recorded, and no record header.
        let bad: Vec<Vec<u8>> = vec![
            framed_marker_bytes("format 1\n"), framed_marker_bytes("format 1\nfull\nsparse\n"),
            framed_marker_bytes("format 1\nshallow\n"), framed_marker_bytes("full\n"),
            b"/forklift-warehouse 4\nformat 1\nfull\n".to_vec(), b"/forklift-warehouse 5\nformat 1\nfull\n".to_vec(),
            b"/forklift-warehouse 3\nformat 1\nfull\n".to_vec(), b"/forklift-warehouse 2\nfull\n".to_vec(),
            b"/forklift-warehouse 1\n".to_vec(), b"/forklift-warehouse 4".to_vec(), b"warehouse\n".to_vec(),
        ];
        for bad in bad {
            std::fs::write(forklift.join(FILE_NAME_WAREHOUSE_MARKER), &bad).unwrap();

            let error = has_warehouse_marker(&forklift).unwrap_err();
            assert!(error.starts_with("The warehouse marker "), "{}", error);
            assert!(bay_utils::classify_forklift(&forklift).is_err(), "{:?} classified", bad);
            assert!(prepare_at(&root).is_err(), "{:?} prepared over", bad);
            assert_eq!(std::fs::read(forklift.join(FILE_NAME_WAREHOUSE_MARKER)).unwrap(), bad);
            assert!(!forklift.join(FILE_NAME_WAREHOUSE_MARKER_COPY).exists(), "{:?} copied", bad);
            assert!(!forklift.join("objects").exists(), "{:?} filled around", bad);
        }

        for (old, what) in [("/forklift-warehouse 1\n", "recorded the warehouse's format"),
                            ("/forklift-warehouse 3\nformat 1\nfull\n", "carried a digest")] {
            std::fs::write(forklift.join(FILE_NAME_WAREHOUSE_MARKER), old).unwrap();
            let error = read_warehouse_marker(&forklift).unwrap_err();
            assert!(error.contains("older version of Forklift") && error.contains(what), "an old marker must say how it came to be: {}", error);
        }

        for mode in [WarehouseMode::Full, WarehouseMode::Sparse, WarehouseMode::Cache] {
            std::fs::write(forklift.join(FILE_NAME_WAREHOUSE_MARKER),
                framed_marker_bytes(&format!("format 1\n{}\n", mode.word()))).unwrap();
            assert_eq!(read_warehouse_marker(&forklift), Ok(Some(mode)));
            assert_eq!(bay_utils::classify_forklift(&forklift), Ok(bay_utils::ForkliftKind::Warehouse));
        }

        file_utils::remove_dir_all(&root).ok();
    }

    /// Each file fault on one file of the marker pair is read through its twin and healed back to
    /// it; the same fault beside an absent twin, or on both files, is an error, never no marker.
    ///
    /// Revert: drop the copy fallback → a faulted primary is an error → red; take an unusable file
    /// beside an absent one for none → the lone fault reads as no marker → red.
    #[test]
    fn a_fault_on_one_marker_file_is_read_through_its_twin() {
        let root = fresh_dir("marker-pair");
        let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);
        std::fs::create_dir_all(&forklift).unwrap();
        let marker = WarehouseMarker { format: WAREHOUSE_FORMAT, mode: "full".to_string(), upgrade: None };
        let good = encode_marker(&marker);
        let lay = |name: &str, bytes: Option<Vec<u8>>| match bytes {
            Some(bytes) => std::fs::write(forklift.join(name), bytes).unwrap(),
            None => file_utils::remove_file(forklift.join(name)).unwrap(),
        };
        type Fault = fn(&[u8]) -> Option<Vec<u8>>;
        let faults: [(&str, Fault); 5] = [
            ("delete", |_| None),
            ("flip", |bytes| { let mut bytes = bytes.to_vec(); let middle = bytes.len() / 2; bytes[middle] ^= 1; Some(bytes) }),
            ("truncate", |bytes| Some(bytes[..bytes.len() / 2].to_vec())),
            ("zero", |bytes| Some(vec![0; bytes.len()])),
            ("empty", |_| Some(Vec::new())),
        ];
        let (primary, copy) = (FILE_NAME_WAREHOUSE_MARKER, FILE_NAME_WAREHOUSE_MARKER_COPY);

        for (fault, damage) in faults {
            for (hit, twin) in [(primary, copy), (copy, primary)] {
                write_marker(&forklift, &marker).unwrap();
                lay(hit, damage(&good));
                let pair = read_marker_pair(&forklift).unwrap();
                assert_eq!(pair.chosen, Some((marker.clone(), good.clone())), "{} on {}", fault, hit);
                assert_eq!(pair.stale, vec![hit], "{} on {}", fault, hit);
                heal_marker_pair(&forklift, &good, &pair.stale);
                assert_eq!(std::fs::read(forklift.join(hit)).unwrap(), good, "{} on {} healed", fault, hit);

                lay(twin, None);
                lay(hit, damage(&good));
                match damage(&good) {
                    None => assert!(read_marker_pair(&forklift).unwrap().chosen.is_none(), "both deleted"),
                    Some(_) => {
                        assert!(read_marker_pair(&forklift).is_err(), "{} on {} beside an absent twin", fault, hit);
                        lay(twin, damage(&good));
                        let both = read_marker_pair(&forklift).unwrap_err();
                        assert!(both.contains("Its copy") && both.contains("cannot be used either"), "{} on both: {}", fault, both);
                    }
                }
            }
        }

        // Both valid and differing, as a stop between the copy's write and the primary's leaves them:
        // the primary is the marker and the copy is brought back to it, whichever is the later.
        let upgrading = WarehouseMarker { upgrade: Some(MarkerUpgrade { target: WAREHOUSE_FORMAT + 1, step: 1 }), ..marker.clone() };
        for (in_primary, in_copy) in [(&marker, &upgrading), (&upgrading, &marker)] {
            lay(primary, Some(encode_marker(in_primary)));
            lay(copy, Some(encode_marker(in_copy)));
            let pair = read_marker_pair(&forklift).unwrap();
            assert_eq!(pair.chosen.map(|(chosen, _)| chosen).as_ref(), Some(in_primary));
            assert_eq!(pair.stale, vec![copy]);
        }

        // A marker from before the digest stands in for no missing twin.
        lay(primary, None);
        lay(copy, Some(b"/forklift-warehouse 3\nformat 1\nfull\n".to_vec()));
        assert!(read_marker_pair(&forklift).unwrap_err().contains("carried a digest"));

        file_utils::remove_dir_all(&root).ok();
    }

    /// A clone's claim puts its `.forklift` in place with the record inside; prepare then fills
    /// it in place and marks it, keeping the record.
    #[test]
    fn prepare_fills_a_franchise_claim_and_keeps_its_record() {
        let root = fresh_dir("claim");
        let target = root.join("t");
        let forklift = target.join(FOLDER_NAME_FORKLIFT_ROOT);
        let record = franchise_utils::FranchiseRecord {
            url: "http://example.com:9418".to_string(),
            token: None,
            request: franchise_utils::Request::default(),
            genesis: None,
            bundle: franchise_utils::BundleState::None,
            attempt: franchise_utils::Attempt::Running,
            start_census: 0,
            format: WAREHOUSE_FORMAT,
            last_failure: None,
        };

        franchise_utils::claim(&target, &record).unwrap();
        assert_eq!(staging_leftovers(&target), Vec::<PathBuf>::new());
        assert_eq!(has_warehouse_marker(&forklift), Ok(false));

        prepare_at(&target).unwrap();

        assert_eq!(has_warehouse_marker(&forklift), Ok(true));
        assert!(forklift.join("objects").is_dir());
        assert_eq!(franchise_utils::read(&forklift), Ok(Some(record)));

        file_utils::remove_dir_all(&root).ok();
    }

    /// The scopes a mode's prepare writes before the marker: both scope files for a sparse
    /// warehouse, neither for a full one or a cache.
    fn scope_files(forklift: &Path) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        (std::fs::read(forklift.join("config").join("fetch-scope")).ok(), std::fs::read(forklift.join("scope")).ok())
    }

    fn origin_in(target: &Path) -> Option<String> {
        let _scope = globals::StorageRootScope::enter(target);
        origin_utils::read_origin().unwrap()
    }

    /// `prepare_warehouse` records the mode it is given; a partial one writes its origin with it,
    /// and a sparse one the fetch scope and the main tree's scope too, built under the staging name
    /// and filled in place alike. Over a marker already standing it keeps the mode and writes no
    /// scope or origin, so preparing a sparse warehouse again never restores a scope that was
    /// deleted.
    ///
    /// Revert: write the marker before the scopes, or skip them in place (`fill_in_place` marking
    /// with `Full`) → the claim reads full; skip the origin for a cache → its row reads `None`;
    /// let `fill_in_place` write the scopes over a standing marker → the deleted fetch scope comes
    /// back.
    #[test]
    fn prepare_records_the_mode_and_a_partial_one_writes_its_origin_and_scopes_first() {
        let scope = PathScope::from_prefixes(["src"]);
        let written = b"/forklift-scope 4\nsrc\n".to_vec();

        let root = fresh_dir("modes");
        let origin = "http://origin.example";
        let sparse = PrepareMode::Sparse { scope: &scope, origin };
        for (name, mode) in [("full", PrepareMode::Full), ("sparse", sparse), ("cache", PrepareMode::Cache { origin })] {
            let target = root.join(name);
            std::fs::create_dir_all(&target).unwrap();
            let forklift = target.join(FOLDER_NAME_FORKLIFT_ROOT);
            {
                let _scope = globals::StorageRootScope::enter(&target);
                prepare_warehouse(mode).unwrap();
            }
            assert_eq!(read_warehouse_marker(&forklift), Ok(Some(mode.mode())), "{}", name);
            let expected = if name == "sparse" { (Some(written.clone()), Some(written.clone())) } else { (None, None) };
            assert_eq!(scope_files(&forklift), expected, "{}", name);
            assert_eq!(origin_in(&target).as_deref(), (name != "full").then_some(origin), "{}", name);
        }

        // Filled in place: a `.forklift` with something in it but no marker, as a clone's claim is.
        let target = root.join("claimed");
        let forklift = target.join(FOLDER_NAME_FORKLIFT_ROOT);
        std::fs::create_dir_all(&forklift).unwrap();
        std::fs::write(forklift.join("keep"), b"").unwrap();
        {
            let _scope = globals::StorageRootScope::enter(&target);
            prepare_warehouse(sparse).unwrap();
        }
        assert_eq!(read_warehouse_marker(&forklift), Ok(Some(WarehouseMode::Sparse)));
        assert_eq!(scope_files(&forklift), (Some(written.clone()), Some(written.clone())));
        assert_eq!(origin_in(&target).as_deref(), Some(origin));

        // Prepared again over its marker, with its fetch scope and origin gone: the mode stands and
        // nothing is restored, so the warehouse stays refused rather than quietly whole again.
        file_utils::remove_file(forklift.join("config").join("fetch-scope")).unwrap();
        file_utils::remove_file(forklift.join(origin_utils::FILE_NAME)).unwrap();
        {
            let _scope = globals::StorageRootScope::enter(&target);
            prepare_warehouse(PrepareMode::Full).unwrap();
            assert!(crate::util::scope_utils::read_fetch_scope().unwrap_err().contains("is missing"));
        }
        assert_eq!(read_warehouse_marker(&forklift), Ok(Some(WarehouseMode::Sparse)));
        assert_eq!(scope_files(&forklift), (None, Some(written)));
        assert_eq!(origin_in(&target), None);

        file_utils::remove_dir_all(&root).ok();
    }

    /// A warehouse put in place of another format between the check and the fill is not completed:
    /// this build's pieces would stand under a number that does not describe them.
    ///
    /// Revert: drop the backstop from `fill_in_place` → the missing folders are made → red.
    #[test]
    fn preparing_over_another_format_adds_nothing() {
        for (marker, expected) in [("format 2\nfull\n", "format 2"), ("format 1\nfull\nupgrading 2 0\n", "upgrade to format 2 unfinished")] {
            let root = fresh_dir("prepare-other-format");
            let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);
            std::fs::create_dir_all(&forklift).unwrap();
            std::fs::write(forklift.join(FILE_NAME_WAREHOUSE_MARKER), framed_marker_bytes(marker)).unwrap();

            let error = prepare_at(&root).unwrap_err();
            assert!(error.contains(expected), "{}", error);
            // The lease's sidecar is the only thing it may make: every format shares it.
            let mut left = list_names_for_test(&forklift);
            left.retain(|name| name != lock_utils::FILE_NAME_FORMAT_LOCK);
            assert_eq!(left, [FILE_NAME_WAREHOUSE_MARKER], "{:?}", marker);

            file_utils::remove_dir_all(&root).ok();
        }
    }

    /// An older format is sent to the upgrade only when the chain reaches it; one older than that is
    /// told to make the warehouse again, never pointed at an upgrade that would refuse it.
    ///
    /// Revert: name the upgrade for every older format → format 1 below the floor names it → red.
    #[test]
    fn an_older_format_is_sent_to_the_upgrade_only_when_the_chain_reaches_it() {
        let none: [upgrade_utils::UpgradeStep; 0] = [];
        let migrations = [upgrade_utils::Migration { steps: &none }];
        let chain = upgrade_utils::UpgradeChain { reads: 3, migrations: &migrations };
        let root = Path::new("/w");
        let step_for = |format| {
            let marker = WarehouseMarker { format, mode: "full".to_string(), upgrade: None };
            match judge_format(&marker, root, Head::Client, &chain) {
                Err(CoreError::Refusal { code: RefusalCode::WarehouseFormatOlder, next_step, .. }) => next_step,
                other => panic!("format {}: {:?}", format, other),
            }
        };

        assert!(step_for(2).contains("\"forklift upgrade\""), "{}", step_for(2));
        assert!(step_for(1).contains("\"forklift prepare\""), "{}", step_for(1));
        assert!(!step_for(1).contains("upgrade"), "{}", step_for(1));

        // The same holds for an upgrade of it that did not finish, when this build would finish it.
        let unfinished = WarehouseMarker { format: 1, mode: "full".to_string(), upgrade: Some(MarkerUpgrade { target: 2, step: 0 }) };
        match judge_format(&unfinished, root, Head::Client, &chain) {
            Err(CoreError::Refusal { code: RefusalCode::WarehouseFormatOlder, next_step, .. }) =>
                assert!(next_step.contains("\"forklift prepare\""), "{}", next_step),
            other => panic!("an unfinished upgrade below the floor: {:?}", other),
        }
    }

    /// Classifying decides only that a folder is a warehouse: any format is one, even with a mode
    /// this build has never heard of, while a mode the current format should know is an error.
    ///
    /// Revert: judge the mode at every format → a newer warehouse's mode is unreadable, not newer → red.
    #[test]
    fn a_marker_of_another_format_is_a_warehouse_whatever_its_mode() {
        let root = fresh_dir("marker-classify");
        let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);
        std::fs::create_dir_all(&forklift).unwrap();
        let marker = forklift.join(FILE_NAME_WAREHOUSE_MARKER);

        for accepted in ["format 2\nfull\n", "format 2\nsomething-new\n", "format 0\nsomething-old\n", "format 1\nfull\n"] {
            std::fs::write(&marker, framed_marker_bytes(accepted)).unwrap();
            assert_eq!(has_warehouse_marker(&forklift), Ok(true), "{:?}", accepted);
        }

        std::fs::write(&marker, framed_marker_bytes("format 1\nsomething-new\n")).unwrap();
        assert!(has_warehouse_marker(&forklift).is_err());

        file_utils::remove_dir_all(&root).ok();
    }
}
