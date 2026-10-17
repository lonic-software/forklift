//! Case aliases in the working directory: two warehouse names the file system resolves to one
//! file (`README` and `readme` on a case-insensitive volume, an NFC and an NFD spelling on a
//! normalization-insensitive one).
//!
//! Which names alias is a property of the volume being written, never of the names, so trees
//! are free to hold such pairs and nothing here consults a fixed folding table for a verdict.
//! The volume is the oracle: a path is judged by what the file system resolves it to (by
//! `lstat` identity), and a pair of names nothing on disk can judge yet is settled by a probe on
//! the same volume. A case-fold key only proposes candidates; it never decides.
//!
//! Every working-directory materializer runs its up-front collision check through
//! [`AliasCheck`], so a case-only rename is a rename (not an "untracked" collision), and two
//! entries that would land on one file are refused naming both instead of one silently
//! overwriting the other.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use crate::enums::inventory_item_state::InventoryItemState;
use crate::parser::inventory::inventory_parser;
use crate::util::{file_utils, inventory_utils};

/// A file's identity as the file system reports it without following a symlink.
#[cfg(unix)]
type Identity = (u64, u64);

/// A file's identity as the file system reports it. Windows never materializes a symlink, so
/// the followed file id is the entry's own there.
#[cfg(not(unix))]
type Identity = file_id::FileId;

#[cfg(unix)]
fn identity(path: &Path) -> Option<Identity> {
    use std::os::unix::fs::MetadataExt;

    std::fs::symlink_metadata(path).ok().map(|metadata| (metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn identity(path: &Path) -> Option<Identity> {
    file_id::get_file_id(path).ok()
}

/// The candidate key two names must share before a probe is spent on them: a full case fold
/// (`ß` and `ss`, the Kelvin sign and `k`, `ﬁ` and `fi` all meet), with no normalization. It
/// proposes pairs; the volume decides them.
pub fn fold_key(name: &str) -> String {
    name.to_lowercase().to_uppercase().to_lowercase()
}

/// What sits at a path a materialization is about to create.
#[derive(Debug, PartialEq, Eq)]
pub enum Occupancy {
    /// Nothing: some component of the path does not resolve.
    Vacant,
    /// The path resolves, and every component is listed by its parent under exactly its own name.
    Listed,
    /// A component resolves, but its parent lists no entry of exactly that name: it aliases the
    /// listed entry `occupant` (its key on disk).
    Alias {
        /// The key of the aliased component, as the warehouse spells it.
        component: String,
        /// The key of the entry on disk the component resolves to.
        occupant: String,
        /// Whether the occupant is a directory.
        occupant_is_dir: bool,
    },
    /// A component resolves, its parent does not list it by name, and no listed entry has its
    /// identity: something occupies the path that cannot be named.
    Unidentified {
        /// The key of the component that resolved.
        component: String,
    },
}

/// What a planned materialization is about to remove, so an occupant it clears is not a
/// collision (a case-only rename).
pub trait Removals {
    /// Whether the operation removes the tracked file at `key` before it writes.
    fn removes_file(&self, key: &str) -> bool;

    /// Whether the operation removes the directory at `key` once it is empty.
    fn removes_directory(&self, key: &str) -> bool;
}

/// Whether the directory at `key` is certain to be gone once `removals` have run: the operation
/// removes it, and every entry on disk beneath it, recursively — each file, and each directory
/// (empty ones included). Anything else — untracked or ignored content, an empty directory
/// nothing removes, a tracked file that stays — keeps the directory, so a case-only rename's
/// new spelling would be written into the old one.
fn vacated(key: &str, removals: &dyn Removals) -> Result<bool, String> {
    if !removals.removes_directory(key) {
        return Ok(false);
    }

    let entries = std::fs::read_dir(key)
        .map_err(|e| format!("Error while reading directory \"{}\": {}", key, e))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("Error while reading directory \"{}\": {}", key, e))?;
        let Ok(name) = entry.file_name().into_string() else { return Ok(false) };
        let child = format!("{}/{}", key, name);
        let is_dir = entry.file_type().is_ok_and(|file_type| file_type.is_dir());

        let gone = if is_dir { vacated(&child, removals)? } else { removals.removes_file(&child) };

        if !gone {
            return Ok(false);
        }
    }

    Ok(true)
}

/// Why a path a materialization would create is refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Collision {
    /// Untracked content occupies the path, under the given on-disk key.
    Untracked(String),
    /// The path names the same file as a tracked entry that stays (or as untracked content
    /// inside a directory being replaced).
    Alias {
        /// The path being created.
        path: String,
        /// The entry on disk it resolves to.
        occupant: String,
    },
}

/// The verdict on one path a materialization creates.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing is in the way, or the only occupant is something the operation removes first.
    Proceed,
    /// The path exists under its own name: the caller's own untracked-collision rules apply.
    Exists,
    /// The path is refused.
    Refuse(Collision),
}

/// One run's alias check. Directory listings are read once per directory and kept for the run.
#[derive(Default)]
pub struct AliasCheck {
    listings: HashMap<String, Option<BTreeSet<String>>>,
}

impl AliasCheck {
    /// A check with nothing read yet.
    pub fn new() -> AliasCheck {
        AliasCheck::default()
    }

    /// The names a directory lists (`None` when it cannot be read as a directory).
    fn listing(&mut self, dir_key: &str) -> Option<&BTreeSet<String>> {
        self.listings
            .entry(dir_key.to_string())
            .or_insert_with(|| {
                let fs_path = if dir_key.is_empty() { PathBuf::from(".") } else { PathBuf::from(dir_key) };
                let entries = std::fs::read_dir(fs_path).ok()?;

                Some(entries
                    .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                    .collect())
            })
            .as_ref()
    }

    /// What occupies `path` (a warehouse key, relative to the working directory). Walks the
    /// components from the root with `lstat`, never following a symlink.
    ///
    /// # Arguments
    /// * `path` - The warehouse path a materialization is about to create.
    ///
    /// # Returns
    /// * `Ok(Occupancy)` - What is at the path.
    /// * `Err(String)`   - If a component's metadata could not be read.
    pub fn occupancy(&mut self, path: &str) -> Result<Occupancy, String> {
        let mut parent = String::new();

        for name in path.split('/') {
            let key = if parent.is_empty() { name.to_string() } else { format!("{}/{}", parent, name) };

            match std::fs::symlink_metadata(&key) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Occupancy::Vacant),
                // A component that is a file where a directory is expected: what occupies the
                // path is that file, which the caller's rules handle under its own name.
                Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => return Ok(Occupancy::Listed),
                Err(e) => return Err(format!("Error while getting metadata for path \"{}\": {}", key, e)),
            }

            let listed = self.listing(&parent).is_some_and(|names| names.contains(name));

            if !listed {
                return Ok(match self.find_occupant(&parent, name, &key) {
                    Some((occupant, occupant_is_dir)) => Occupancy::Alias { component: key, occupant, occupant_is_dir },
                    None => Occupancy::Unidentified { component: key },
                });
            }

            parent = key;
        }

        Ok(Occupancy::Listed)
    }

    /// The listed sibling of `name` in `parent` that `key` resolves to: case-fold candidates
    /// first, then (a normalization alias, say) every entry.
    fn find_occupant(&mut self, parent: &str, name: &str, key: &str) -> Option<(String, bool)> {
        let target = identity(Path::new(key))?;
        let names: Vec<String> = self.listing(parent)?.iter().cloned().collect();
        let fold = fold_key(name);

        let (candidates, rest): (Vec<&String>, Vec<&String>) = names.iter()
            .partition(|listed| fold_key(listed) == fold);

        candidates.into_iter().chain(rest).find_map(|listed| {
            let listed_key = if parent.is_empty() { listed.clone() } else { format!("{}/{}", parent, listed) };

            (identity(Path::new(&listed_key)) == Some(target)).then(|| {
                let is_dir = std::fs::symlink_metadata(&listed_key).is_ok_and(|metadata| metadata.is_dir());
                (listed_key, is_dir)
            })
        })
    }

    /// Judge one path a materialization creates.
    ///
    /// # Arguments
    /// * `path`     - The warehouse path being created.
    /// * `removals` - What the same operation removes before it writes.
    ///
    /// # Returns
    /// * `Ok(Verdict)` - Whether the path may be written.
    /// * `Err(String)` - If the disk or the inventory could not be read.
    pub fn judge(&mut self, path: &str, removals: &dyn Removals) -> Result<Verdict, String> {
        Ok(match self.occupancy(path)? {
            Occupancy::Vacant => Verdict::Proceed,
            Occupancy::Listed => Verdict::Exists,
            Occupancy::Unidentified { component } => Verdict::Refuse(Collision::Untracked(component)),
            Occupancy::Alias { occupant, occupant_is_dir: false, .. } => {
                if removals.removes_file(&occupant) {
                    Verdict::Proceed
                } else if tracked_file(&occupant)? {
                    Verdict::Refuse(Collision::Alias { path: path.to_string(), occupant })
                } else {
                    Verdict::Refuse(Collision::Untracked(occupant))
                }
            }
            Occupancy::Alias { occupant, occupant_is_dir: true, .. } => {
                let tracked = inventory_utils::is_tracked_directory(&occupant)?;

                if tracked && vacated(&occupant, removals)? {
                    Verdict::Proceed
                } else if tracked {
                    Verdict::Refuse(Collision::Alias { path: path.to_string(), occupant })
                } else {
                    Verdict::Refuse(Collision::Untracked(occupant))
                }
            }
        })
    }

    /// Find the pairs among `created` (paths one operation creates) that would land on one file:
    /// distinct names under one parent that share a [`fold_key`] and that a probe on the target
    /// volume confirms. Pairs where either name already resolves are left to [`judge`], which
    /// sees them on disk.
    ///
    /// # Arguments
    /// * `created` - The paths the operation creates.
    ///
    /// # Returns
    /// * `Ok(Vec<Collision>)` - One `Alias` per confirmed pair (empty when there are none).
    /// * `Err(String)`        - Never today; reserved for a probe that cannot be cleaned up.
    ///
    /// [`judge`]: AliasCheck::judge
    pub fn created_pairs<'a>(&mut self, created: impl IntoIterator<Item = &'a str>) -> Result<Vec<Collision>, String> {
        let mut by_parent: BTreeMap<&str, BTreeMap<String, BTreeSet<&str>>> = BTreeMap::new();

        for path in created {
            let mut start = 0;

            for (index, _) in path.match_indices('/').chain(std::iter::once((path.len(), ""))) {
                let parent = if start == 0 { "" } else { &path[..start - 1] };
                let name = &path[start..index];

                by_parent.entry(parent).or_default().entry(fold_key(name)).or_default().insert(name);
                start = index + 1;
            }
        }

        let mut collisions = Vec::new();

        for (parent, groups) in &by_parent {
            for names in groups.values().filter(|names| names.len() > 1) {
                let names: Vec<&str> = names.iter().copied().collect();

                for (index, first) in names.iter().enumerate() {
                    for second in &names[index + 1..] {
                        if probe_aliases(parent, first, second) {
                            collisions.push(Collision::Alias {
                                path: join(parent, second),
                                occupant: join(parent, first),
                            });
                        }
                    }
                }
            }
        }

        Ok(collisions)
    }
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() { name.to_string() } else { format!("{}/{}", parent, name) }
}

/// Whether the volume `parent` lands on resolves `first` and `second` to one file. Probed in a
/// fresh directory inside the nearest existing ancestor of `parent` in the working directory —
/// the volume (and, where sensitivity is per directory, the directory) the write lands on, which
/// the metadata folder need not share. A probe that cannot be made answers `false`: the names
/// then go on to the write, whose exclusive create is the backstop.
fn probe_aliases(parent: &str, first: &str, second: &str) -> bool {
    let mut base = PathBuf::from(if parent.is_empty() { "." } else { parent });

    while !base.is_dir() {
        if !base.pop() || base.as_os_str().is_empty() {
            base = PathBuf::from(".");
            break;
        }
    }

    let Some(probe_dir) = (0..16).find_map(|attempt| {
        let candidate = base.join(format!(".forklift-alias-probe-{}-{}", std::process::id(), attempt));
        std::fs::create_dir(&candidate).ok().map(|()| candidate)
    }) else {
        return false;
    };

    let aliases = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(probe_dir.join(first))
        .is_ok()
        && identity(&probe_dir.join(second)).is_some_and(|id| identity(&probe_dir.join(first)) == Some(id));

    let _ = std::fs::remove_dir_all(&probe_dir);

    aliases
}

/// Whether the inventory holds a live file entry at `key`.
fn tracked_file(key: &str) -> Result<bool, String> {
    let (parent, name) = key.rsplit_once('/').unwrap_or(("", key));
    let (_, bytes) = file_utils::retrieve_inventory_or_none_by_key(parent)?;

    let Some(bytes) = bytes else {
        return Ok(false);
    };

    let inventory = inventory_parser::parse_inventory(&bytes)
        .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", parent, e))?;

    Ok(matches!(inventory.get_item_by_name(name), Some(item) if item.state != InventoryItemState::Deleted))
}

/// Render collisions for a refusal: untracked occupants under their on-disk names, aliases as
/// both entries.
///
/// # Returns
/// * `(Vec<String>, Vec<String>)` - The untracked keys, and one "`a` and `b`" line per alias.
pub fn split_collisions(collisions: &[Collision]) -> (Vec<String>, Vec<String>) {
    let mut untracked = Vec::new();
    let mut aliases = Vec::new();

    for collision in collisions {
        match collision {
            Collision::Untracked(key) => if !untracked.contains(key) { untracked.push(key.clone()) },
            Collision::Alias { path, occupant } => {
                let line = format!("{} and {}", occupant, path);
                if !aliases.contains(&line) { aliases.push(line) }
            }
        }
    }

    (untracked, aliases)
}

/// The refusal for a materialization with collisions, or `Ok(())` when there are none.
///
/// # Arguments
/// * `operation`  - What is running, for the message ("Shift", "Consolidating side").
/// * `collisions` - The collisions found.
pub fn refuse_collisions(operation: &str, collisions: &[Collision]) -> Result<(), String> {
    if collisions.is_empty() {
        return Ok(());
    }

    let (untracked, aliases) = split_collisions(collisions);
    let mut parts = Vec::new();

    if !aliases.is_empty() {
        parts.push(format!(
            "{} would write two entries this file system treats as one file:\n  {}\n\
            Rename one of them on a case-sensitive file system first.",
            operation,
            aliases.join("\n  ")
        ));
    }

    if !untracked.is_empty() {
        parts.push(format!(
            "{} would overwrite these untracked files:\n  {}\n\
            Move them out of the way (or load and stack them) first.",
            operation,
            untracked.join("\n  ")
        ));
    }

    Err(parts.join("\n"))
}
