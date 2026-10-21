//! Case aliases in the working directory: two warehouse names the file system resolves to one
//! file (`README` and `readme` on a case-insensitive volume, an NFC and an NFD spelling on a
//! normalization-insensitive one).
//!
//! Which names alias is a property of the volume being written, never of the names, so trees
//! are free to hold such pairs and nothing here consults a fixed folding table for a verdict.
//! The volume is the oracle: a path is judged by what the file system resolves it to (by
//! `lstat` identity), and a pair of names nothing on disk can judge yet is settled by a probe on
//! the same volume. A key (a full case fold of the canonical decomposition) only proposes
//! candidates; it never decides. Two new names a volume merges under a rule the key does not
//! cover are not paired up front: for files the exclusive create of the second write refuses
//! them, but a second directory is entered silently, so its entries merge into the first.
//!
//! Every working-directory materializer runs its up-front collision check through
//! [`AliasCheck`], so a case-only rename is a rename (not an "untracked" collision), and two
//! entries that would land on one file are refused naming both instead of one silently
//! overwriting the other.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use unicode_normalization::UnicodeNormalization;
use crate::enums::inventory_item_state::InventoryItemState;
use crate::util::{file_utils, inventory_utils, tree_utils};

/// A file's identity as the file system reports it without following a symlink: the same pair the
/// inventory records (see [`file_utils::get_device_and_inode`]).
#[cfg(unix)]
type Identity = (u64, u64);

/// A file's identity as the file system reports it without following a symlink. This keeps the
/// 128-bit id, unlike the inventory's 64-bit file index, because the volume is the oracle for
/// whether two names are one entry, and ReFS ids need all 128 bits to stay unique.
#[cfg(not(unix))]
type Identity = (u64, u128);

#[cfg(unix)]
fn identity(path: &Path) -> Option<Identity> {
    let metadata = std::fs::symlink_metadata(path).ok()?;

    file_utils::get_device_and_inode(path, &metadata).ok()
}

#[cfg(not(unix))]
fn identity(path: &Path) -> Option<Identity> {
    file_utils::get_full_file_id(path).ok()
}

/// Whether `first` and `second` resolve, without following a symlink, to one entry on disk. A path
/// that does not resolve is the same entry as nothing.
///
/// # Returns
/// * `Ok(bool)`    - Whether the two are one entry.
/// * `Err(String)` - If a path resolves but cannot be identified: never answered with a guess.
pub fn same_entry(first: &Path, second: &Path) -> Result<bool, String> {
    let Some(identity_of_first) = resolved_identity(first)? else {
        return Ok(false);
    };

    Ok(resolved_identity(second)? == Some(identity_of_first))
}

/// [`identity`], telling a path that does not resolve (`None`: nothing there, or a name the file
/// system cannot hold) from an entry that cannot be identified (an error).
fn resolved_identity(path: &Path) -> Result<Option<Identity>, String> {
    use std::io::ErrorKind;

    match std::fs::symlink_metadata(path) {
        Err(error) if matches!(error.kind(),
            ErrorKind::NotFound | ErrorKind::NotADirectory | ErrorKind::InvalidFilename) => Ok(None),
        Err(error) => Err(format!("Could not read {:?} to identify it: {}", path, error)),
        Ok(_) => identity(path)
            .map(Some)
            .ok_or_else(|| format!("Could not identify {:?} on its volume.", path)),
    }
}

/// The entry of `listing` (the names `parent` lists) that `name` in `parent` resolves to on disk:
/// case-fold candidates first, then (a normalization alias, say) every entry. `None` when `name`
/// does not resolve, or no listed entry can be identified as it; an entry that cannot be
/// identified is passed over, never taken for a match.
pub fn listed_alias<'a>(parent: &Path, name: &str, listing: &'a [String]) -> Option<&'a String> {
    let target = identity(&parent.join(name))?;
    let fold = fold_key(name);

    let (candidates, rest): (Vec<&String>, Vec<&String>) = listing.iter()
        .partition(|listed| fold_key(listed) == fold);

    candidates.into_iter().chain(rest).find(|listed| identity(&parent.join(listed)) == Some(target))
}

/// The candidate key two names must share before a probe is spent on them: a full case fold
/// (`ß` and `ss`, the Kelvin sign and `k`, `ﬁ` and `fi` all meet) in canonical decomposition (an
/// NFC and an NFD spelling meet). It proposes pairs; the volume decides them.
pub fn fold_key(name: &str) -> String {
    // An ASCII name folds to its ASCII lowercase, and has no decomposition: the common case,
    // spared the three passes below.
    if name.is_ascii() {
        return name.to_ascii_lowercase();
    }

    name.to_lowercase().to_uppercase().to_lowercase().nfd().collect()
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
    /// A component short of the last resolves, under its own name, to something that is not a
    /// directory (a file, or a symlink, which is never followed): that entry occupies the path.
    /// A symlink is judged as the entry it is, never as the directory it may point at, so a
    /// refusal names the link itself.
    NonDirectory {
        /// The key of the entry on disk.
        occupant: String,
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

/// One run's alias check. Directory listings are read once per directory and kept for the run,
/// and so is each aliased directory's verdict (whether it is tracked, and whether the run vacates
/// it), so a renamed directory of many files is walked once. One check therefore serves one
/// operation's set of removals.
#[derive(Default)]
pub struct AliasCheck {
    listings: HashMap<String, Option<BTreeSet<String>>>,
    directories: HashMap<String, (bool, bool)>,
    ledger: Option<inventory_utils::ShardLedger>,
}

impl AliasCheck {
    /// A check with nothing read yet.
    pub fn new() -> AliasCheck {
        AliasCheck::default()
    }

    /// Whether the inventory holds a live file entry at `key` (see [`tracked_file`]), against the
    /// registered set as this check first read it.
    pub fn tracked_file(&mut self, key: &str) -> Result<bool, String> {
        if self.ledger.is_none() {
            self.ledger = Some(inventory_utils::ShardLedger::read()?);
        }

        tracked_file(self.ledger.as_ref().unwrap(), key)
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
        let mut names = path.split('/').peekable();

        while let Some(name) = names.next() {
            let key = if parent.is_empty() { name.to_string() } else { format!("{}/{}", parent, name) };

            let is_dir = match std::fs::symlink_metadata(&key) {
                Ok(metadata) => metadata.is_dir(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Occupancy::Vacant),
                Err(e) => return Err(format!("Error while getting metadata for path \"{}\": {}", key, e)),
            };

            let listed = self.listing(&parent).is_some_and(|names| names.contains(name));

            if !listed {
                return Ok(match self.find_occupant(&parent, name) {
                    Some((occupant, occupant_is_dir)) => Occupancy::Alias { component: key, occupant, occupant_is_dir },
                    None => Occupancy::Unidentified { component: key },
                });
            }

            // A file (or a symlink) where the path needs a directory occupies the whole path —
            // a file-to-directory flip when the operation removes it. Stopping here keeps the
            // walk from reading past it, which fails differently on each platform.
            if !is_dir && names.peek().is_some() {
                return Ok(Occupancy::NonDirectory { occupant: key });
            }

            parent = key;
        }

        Ok(Occupancy::Listed)
    }

    /// The path `path` (a warehouse key) opens on this volume, spelled as the directories list it,
    /// or `None` when it opens nothing. Walks the components from the root with `lstat`, never
    /// following a symlink. A component its parent lists under exactly its own name is kept. One
    /// that resolves without being listed is replaced by the listed sibling it resolves to, and the
    /// walk goes on inside that sibling, so an aliased directory does not end it the way it ends
    /// [`occupancy`]. A component that does not resolve, the way [`resolved_identity`] judges it,
    /// makes the path open nothing: a walk continued into an occupant that is now a file meets
    /// "not a directory" there, which is an absence, not an error. So does a path that goes on
    /// beneath a component that is not a directory (a symlink, which the warehouse never enters,
    /// or a file), since `lstat` would otherwise judge the rest inside a link's target.
    ///
    /// A directory that can be entered but not listed cannot tell which spelling it holds, so its
    /// component is kept as typed and the walk goes on, as the current directory's respelling
    /// keeps it (see `warehouse_utils::respell_as_listed`): a path is never refused only because
    /// its spelling cannot be checked.
    ///
    /// # Returns
    /// * `Ok(Some(String))` - The listed path (`path` itself when every component is listed).
    /// * `Ok(None)`         - If nothing is at the path.
    /// * `Err(String)`      - If a component could not be read, or resolves to an entry no
    ///                        listed name can be identified as.
    ///
    /// [`occupancy`]: AliasCheck::occupancy
    pub fn resolve(&mut self, path: &str) -> Result<Option<String>, String> {
        use std::io::ErrorKind;

        let mut resolved = String::new();
        let mut names = path.split('/').filter(|name| !name.is_empty()).peekable();

        while let Some(name) = names.next() {
            let key = join(&resolved, name);

            let is_dir = match std::fs::symlink_metadata(&key) {
                Err(error) if matches!(error.kind(),
                    ErrorKind::NotFound | ErrorKind::NotADirectory | ErrorKind::InvalidFilename) => return Ok(None),
                Err(error) => return Err(format!("Error while getting metadata for path \"{}\": {}", key, error)),
                Ok(metadata) => metadata.is_dir(),
            };

            let listed = self.listing(&resolved).is_none_or(|listing| listing.contains(name));

            if listed {
                if !is_dir && names.peek().is_some() {
                    return Ok(None);
                }
                resolved = key;
                continue;
            }

            let Some((occupant, _)) = self.find_occupant(&resolved, name) else {
                return Err(format!(
                    "\"{}\" opens an entry that \"{}\" lists under no name Forklift can identify it as.",
                    key, if resolved.is_empty() { "." } else { &resolved }
                ));
            };

            if !std::fs::symlink_metadata(&occupant).is_ok_and(|metadata| metadata.is_dir()) && names.peek().is_some() {
                return Ok(None);
            }
            resolved = occupant;
        }

        Ok(Some(resolved))
    }

    /// Whether two distinct warehouse paths are one entry on this volume. They must share a
    /// [`fold_key`] component by component, and then either both [`resolve`] to the same listed
    /// path, or neither resolves and a probe confirms that the names where the two first differ
    /// land on one file. One resolving and the other not is never a pair, and neither are two
    /// paths that are both listed under their own names (hard links on a case-sensitive volume).
    ///
    /// # Returns
    /// * `Ok(bool)`    - Whether the two are one entry.
    /// * `Err(String)` - If the disk could not be read, or a needed probe could not be made.
    ///
    /// [`resolve`]: AliasCheck::resolve
    pub fn one_entry(&mut self, first: &str, second: &str) -> Result<bool, String> {
        let first_names: Vec<&str> = first.split('/').collect();
        let second_names: Vec<&str> = second.split('/').collect();

        if first == second
            || first_names.len() != second_names.len()
            || first_names.iter().zip(&second_names).any(|(a, b)| fold_key(a) != fold_key(b)) {
            return Ok(false);
        }

        match (self.resolve(first)?, self.resolve(second)?) {
            (Some(first_listed), Some(second_listed)) => Ok(first_listed == second_listed),
            (None, None) => {
                let differ = first_names.iter().zip(&second_names).position(|(a, b)| a != b)
                    .expect("distinct paths of equal length differ in some component");

                probe_aliases(&first_names[..differ].join("/"), first_names[differ], second_names[differ])
            }
            _ => Ok(false),
        }
    }

    /// The first pair along `path` (a listed path, as [`resolve`] returns one): the shallowest of
    /// `path` and its ancestors that is one entry with a different node path, as
    /// `(that prefix, the node path)`. Only siblings of each prefix are compared. If a node path
    /// deeper down is one entry with a prefix, their ancestors where the two first differ are a
    /// pair too, under one parent spelled identically, and that pair is met first.
    ///
    /// # Returns
    /// * `Ok(Option<(String, String)>)` - The shallowest pair, if there is one.
    /// * `Err(String)`                  - If the disk could not be read.
    ///
    /// [`resolve`]: AliasCheck::resolve
    pub fn pair_along(&mut self, path: &str, nodes: &Nodes) -> Result<Option<(String, String)>, String> {
        let mut parent = String::new();

        for name in path.split('/').filter(|name| !name.is_empty()) {
            let prefix = join(&parent, name);
            let fold = fold_key(name);
            let siblings: Vec<String> = nodes.children(&parent)
                .filter(|(sibling, _)| *sibling != name && fold_key(sibling) == fold)
                .map(|(sibling, _)| join(&parent, sibling))
                .collect();

            for sibling in siblings {
                if self.one_entry(&prefix, &sibling)? {
                    return Ok(Some((prefix, sibling)));
                }
            }

            parent = prefix;
        }

        Ok(None)
    }

    /// The shallowest pair among `nodes`: two sibling node paths that are one entry on this
    /// volume, as [`one_entry`] judges them, from the parent nearest the root that holds one.
    /// Comparing siblings is enough: if two node paths are one entry, their ancestors where they
    /// first differ are too, and those are siblings under one parent spelled identically.
    ///
    /// # Returns
    /// * `Ok(Option<(String, String)>)` - The shallowest pair, if there is one.
    /// * `Err(String)`                  - If the disk could not be read, or a needed probe could
    ///                                    not be made.
    ///
    /// [`one_entry`]: AliasCheck::one_entry
    pub fn first_pair(&mut self, nodes: &Nodes) -> Result<Option<(String, String)>, String> {
        let mut parents: Vec<&String> = nodes.children.keys().collect();
        parents.sort_by_key(|parent| if parent.is_empty() { 0 } else { parent.matches('/').count() + 1 });

        for parent in parents {
            let mut classes: BTreeMap<String, Vec<&str>> = BTreeMap::new();
            for (name, _) in nodes.children(parent) {
                classes.entry(fold_key(name)).or_default().push(name);
            }

            for names in classes.values().filter(|names| names.len() > 1) {
                for (index, first) in names.iter().enumerate() {
                    for second in &names[index + 1..] {
                        let (first, second) = (join(parent, first), join(parent, second));

                        if self.one_entry(&first, &second)? {
                            return Ok(Some((first, second)));
                        }
                    }
                }
            }
        }

        Ok(None)
    }

    /// The one way out of a pair `first` and `second` (sibling node paths that are one entry):
    /// see [`Remedy`] for the two forms and when each applies.
    ///
    /// # Returns
    /// * `Ok(Remedy)`  - The remedy.
    /// * `Err(String)` - If the disk could not be read.
    pub fn remedy(&mut self, first: &str, second: &str, nodes: &Nodes) -> Result<Remedy, String> {
        let (parent, name) = first.rsplit_once('/').unwrap_or(("", first));
        let fold = fold_key(name);
        let candidates: Vec<(&str, bool)> = nodes.children(parent)
            .filter(|(member, _)| fold_key(member) == fold)
            .collect();

        // Sharing a key only proposes a member: one the volume keeps apart is another file, and
        // its own work is not this pair's to stage.
        let mut members = Vec::new();
        for (member, is_dir) in candidates {
            if member == name || self.one_entry(first, &join(parent, member))? {
                members.push((member, is_dir));
            }
        }

        let listed = match self.resolve(first)? {
            Some(listed) => Some(listed),
            None => self.resolve(second)?,
        };
        let listed_is_dir = listed.as_ref()
            .is_some_and(|listed| std::fs::symlink_metadata(listed).is_ok_and(|metadata| metadata.is_dir()));

        let nearest = self.nearest_listed(parent);

        if nearest != parent || listed_is_dir || members.iter().any(|(_, is_dir)| *is_dir) {
            return Ok(Remedy::LoadDirectory(nearest));
        }

        let listing = self.listing(parent).cloned().unwrap_or_default();
        let removes = members.iter()
            .filter(|(member, _)| !listing.contains(*member))
            .map(|(member, _)| join(parent, member))
            .collect();
        let load = listed.filter(|listed| !nodes.files.contains(listed));

        Ok(Remedy::PairScoped { removes, load })
    }

    /// The deepest of `path` and its ancestors whose every component is listed under exactly its
    /// own name (`""` for the root).
    fn nearest_listed(&mut self, path: &str) -> String {
        let mut nearest = String::new();

        for name in path.split('/').filter(|name| !name.is_empty()) {
            if !self.listing(&nearest).is_some_and(|names| names.contains(name)) {
                break;
            }
            nearest = join(&nearest, name);
        }

        nearest
    }

    /// The listed sibling of `name` in `parent` that the two resolve to (see [`listed_alias`]).
    fn find_occupant(&mut self, parent: &str, name: &str) -> Option<(String, bool)> {
        let names: Vec<String> = self.listing(parent)?.iter().cloned().collect();
        let listed = listed_alias(Path::new(parent), name, &names)?;

        let listed_key = if parent.is_empty() { listed.clone() } else { format!("{}/{}", parent, listed) };
        let is_dir = std::fs::symlink_metadata(&listed_key).is_ok_and(|metadata| metadata.is_dir());

        Some((listed_key, is_dir))
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
            Occupancy::Alias { occupant, occupant_is_dir: false, .. } | Occupancy::NonDirectory { occupant } => {
                if removals.removes_file(&occupant) {
                    Verdict::Proceed
                } else if self.tracked_file(&occupant)? {
                    Verdict::Refuse(Collision::Alias { path: path.to_string(), occupant })
                } else {
                    Verdict::Refuse(Collision::Untracked(occupant))
                }
            }
            Occupancy::Alias { occupant, occupant_is_dir: true, .. } => {
                let (tracked, vacated) = self.directory(&occupant, removals)?;

                if vacated {
                    Verdict::Proceed
                } else if tracked {
                    Verdict::Refuse(Collision::Alias { path: path.to_string(), occupant })
                } else {
                    Verdict::Refuse(Collision::Untracked(occupant))
                }
            }
        })
    }

    /// Whether a directory is tracked, and whether `removals` vacate it, read once per check.
    fn directory(&mut self, key: &str, removals: &dyn Removals) -> Result<(bool, bool), String> {
        if let Some(&known) = self.directories.get(key) {
            return Ok(known);
        }

        let tracked = inventory_utils::is_tracked_directory(key)?;
        let known = (tracked, tracked && vacated(key, removals)?);
        self.directories.insert(key.to_string(), known);
        Ok(known)
    }

    /// Whether what `key` resolves to on disk is still there once the operation has run: the
    /// entry [`judge`] refuses a path for, or one listed under exactly `key` that the operation
    /// does not remove (or, for a directory, vacate). Nothing on disk, or an occupant the
    /// operation clears, stays not.
    ///
    /// [`judge`]: AliasCheck::judge
    fn stays(&mut self, key: &str, removals: &dyn Removals) -> Result<bool, String> {
        if std::fs::symlink_metadata(key).is_err() {
            return Ok(false);
        }

        Ok(match self.judge(key, removals)? {
            Verdict::Proceed => false,
            Verdict::Refuse(_) => true,
            // Never following a symlink: a link to a directory is a file the operation may remove.
            Verdict::Exists if std::fs::symlink_metadata(key).is_ok_and(|metadata| metadata.is_dir()) => {
                !self.directory(key, removals)?.1
            }
            Verdict::Exists => !removals.removes_file(key),
        })
    }

    /// Find the pairs among `created` (paths one operation creates) that would land on one file:
    /// distinct names under one parent that share a [`fold_key`] and that a probe on the target
    /// volume confirms. A pair one of whose names resolves to something that [`stays`] is left
    /// to [`judge`], which refuses the path for that occupant; a pair whose names resolve only to
    /// what the operation clears is probed like any other, since both names are then written.
    ///
    /// # Arguments
    /// * `created`  - The paths the operation creates.
    /// * `removals` - What the same operation removes before it writes.
    ///
    /// # Returns
    /// * `Ok(Vec<Collision>)` - One `Alias` per confirmed pair (empty when there are none).
    /// * `Err(String)`        - If a probe could not be made, or the disk or the inventory could
    ///                          not be read.
    ///
    /// [`judge`]: AliasCheck::judge
    /// [`stays`]: AliasCheck::stays
    pub fn created_pairs<'a>(&mut self,
                             created: impl IntoIterator<Item = &'a str>,
                             removals: &dyn Removals) -> Result<Vec<Collision>, String> {
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
                        let (first_key, second_key) = (join(parent, first), join(parent, second));

                        if self.stays(&first_key, removals)? || self.stays(&second_key, removals)? {
                            continue;
                        }

                        if probe_aliases(parent, first, second)? {
                            collisions.push(Collision::Alias { path: second_key, occupant: first_key });
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

/// The paths the inventory holds live, as the alias test compares them: every live file entry,
/// every directory above one, and every directory known to hold one without being read.
#[derive(Debug, Default)]
pub struct Nodes {
    files: BTreeSet<String>,
    /// Each parent's node names, and whether the name is a directory above a live file entry.
    children: BTreeMap<String, BTreeMap<String, bool>>,
}

impl Nodes {
    /// The nodes of these live file entry paths (warehouse keys).
    pub fn from_live_files(files: BTreeSet<String>) -> Nodes {
        Nodes::from_live(files, std::iter::empty())
    }

    /// The nodes of these live file entry paths, and of `directories`: directory keys that hold a
    /// live entry somewhere beneath them, whatever is beneath them is not listed.
    pub fn from_live<'a>(files: BTreeSet<String>, directories: impl IntoIterator<Item = &'a str>) -> Nodes {
        let mut nodes = Nodes { files: BTreeSet::new(), children: BTreeMap::new() };

        for file in &files {
            nodes.add(file, false);
        }
        for directory in directories {
            nodes.add(directory, true);
        }

        nodes.files = files;
        nodes
    }

    /// Add `path` and every directory above it. A parent already present has its own ancestors
    /// added, so each directory is walked up from once.
    fn add(&mut self, path: &str, is_dir: bool) {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        let known = self.children.contains_key(parent);

        *self.children.entry(parent.to_string()).or_default().entry(name.to_string()).or_insert(false) |= is_dir;

        if !known && !parent.is_empty() {
            self.add(parent, true);
        }
    }

    /// The node names directly under `parent`, each with whether it is a directory node.
    fn children<'a>(&'a self, parent: &str) -> impl Iterator<Item = (&'a str, bool)> + 'a {
        self.children.get(parent).into_iter().flatten().map(|(name, is_dir)| (name.as_str(), *is_dir))
    }
}

/// The one way out of a named load refused for its spelling.
#[derive(Debug, PartialEq, Eq)]
pub enum Remedy {
    /// No pair: load the listed spelling.
    Load(String),
    /// For a pair of files under a parent whose every component is listed under its own name,
    /// where no live spelling of the pair is a directory above a live entry and the entry on disk,
    /// if any, is not a directory: remove each live spelling that is not listed (one path per
    /// `forklift remove`), then load the listed spelling when it is not live yet. That stages
    /// only the rename (or, with nothing on disk, the removal) and leaves other work alone.
    PairScoped {
        /// The spellings to remove, one call each.
        removes: Vec<String>,
        /// The listed spelling to load afterwards, if it is not live yet.
        load: Option<String>,
    },
    /// Any other pair: load the nearest directory above the pair whose every component is
    /// listed under its own name (`""` for the root). That re-stages everything under it, so
    /// unrelated unstaged edits and untracked files are staged too.
    LoadDirectory(String),
}

impl Remedy {
    /// The remedy as instructions, for an error message. Every command is run from the working
    /// tree's root, and every path in it is one shell argument (see [`instructions_when`]).
    ///
    /// [`instructions_when`]: Remedy::instructions_when
    pub fn instructions(&self) -> String {
        self.instructions_when(cfg!(windows))
    }

    /// [`instructions`] as the platform `windows` says: a path no shell there reads as one
    /// literal argument becomes `-- <path>`, and the message then names, in order, the paths to
    /// put in their place.
    ///
    /// [`instructions`]: Remedy::instructions
    fn instructions_when(&self, windows: bool) -> String {
        let mut unquoted = Vec::new();
        let mut command = |verb: &str, path: &str| match tree_utils::remedy_path_when(path, windows) {
            Some(quoted) => format!("\"forklift {} {}\"", verb, quoted),
            None => {
                unquoted.push(format!("\"{}\"", path));
                format!("\"forklift {} -- <path>\"", verb)
            }
        };

        let instructions = match self {
            Remedy::Load(path) => format!("To load it, run {} from the working tree's root.", command("load", path)),
            Remedy::PairScoped { removes, load } => {
                let mut commands: Vec<String> = removes.iter().map(|path| command("remove", path)).collect();
                commands.extend(load.iter().map(|path| command("load", path)));

                format!("From the working tree's root, run {}.", commands.join(", then "))
            }
            Remedy::LoadDirectory(directory) => {
                let (target, scope) = if directory.is_empty() {
                    (".", "the working tree".to_string())
                } else {
                    (directory.as_str(), format!("\"{}\"", directory))
                };

                format!(
                    "From the working tree's root, run {}. That stages every change in {}, not only this \
                    one, unstaged edits and untracked files included: check \"forklift stocktake\" before \
                    stacking.",
                    command("load", target), scope
                )
            }
        };

        if unquoted.is_empty() {
            instructions
        } else {
            format!("{} In place of each <path>, put {}, in that order, quoted for your shell.",
                instructions, unquoted.join(", then "))
        }
    }
}

/// Whether the volume `parent` lands on resolves `first` and `second` to one file. Probed in a
/// fresh directory inside the nearest existing ancestor of `parent` in the working directory —
/// the volume (and, where sensitivity is per directory, the directory) the write lands on, which
/// the metadata folder need not share. A probe that cannot be made is an error, never a "no":
/// two directories that alias would otherwise merge silently.
fn probe_aliases(parent: &str, first: &str, second: &str) -> Result<bool, String> {
    // Walked from the root with `lstat`, stopping at the first component that is not a real
    // directory: a symlink (one the operation replaces, say) would put the probe in its target,
    // outside the working directory and possibly on another volume.
    let mut base = PathBuf::new();

    for name in parent.split('/').filter(|name| !name.is_empty()) {
        let next = base.join(name);
        if !std::fs::symlink_metadata(&next).is_ok_and(|metadata| metadata.is_dir()) {
            break;
        }
        base = next;
    }

    if base.as_os_str().is_empty() {
        base = PathBuf::from(".");
    }

    let mut last_error = None;
    let Some(probe_dir) = (0..16).find_map(|attempt| {
        let candidate = base.join(format!(".forklift-alias-probe-{}-{}", std::process::id(), attempt));
        std::fs::create_dir(&candidate).inspect_err(|e| last_error = Some(e.to_string())).ok().map(|()| candidate)
    }) else {
        return Err(format!(
            "Could not tell whether \"{}\" and \"{}\" name one file here: no probe directory could be \
            made in \"{}\" ({}).",
            join(parent, first), join(parent, second), base.display(), last_error.unwrap_or_default()
        ));
    };

    let aliases = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(probe_dir.join(first))
        .is_ok()
        && identity(&probe_dir.join(second)).is_some_and(|id| identity(&probe_dir.join(first)) == Some(id));

    let _ = std::fs::remove_dir_all(&probe_dir);

    Ok(aliases)
}

/// Whether the inventory holds a live file entry at `key`.
///
/// Read through `ledger`: a parent that is not registered holds nothing staged, whatever its
/// shard's address holds. The answer decides what a materialization may write over.
pub fn tracked_file(ledger: &inventory_utils::ShardLedger, key: &str) -> Result<bool, String> {
    let (parent, name) = key.rsplit_once('/').unwrap_or(("", key));

    let Some(inventory) = ledger.inventory(parent)? else {
        return Ok(false);
    };

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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn an_entry_that_cannot_be_read_is_an_error_not_a_different_entry() {
        let root = std::env::temp_dir().join(format!("forklift-same-entry-{}", std::process::id()));
        let sealed = root.join("sealed");
        std::fs::create_dir_all(&sealed).unwrap();
        std::fs::write(root.join("file"), "content\n").unwrap();

        assert!(same_entry(&root.join("file"), &root.join("file")).unwrap());
        assert!(!same_entry(&root.join("absent"), &root.join("file")).unwrap(), "nothing is no entry");
        assert!(!same_entry(&root.join("file").join("below"), &root.join("file")).unwrap(),
            "a path under a file resolves to nothing");

        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o000)).unwrap();
        let sealed_read = std::fs::symlink_metadata(sealed.join("inside"));
        let verdict = same_entry(&sealed.join("inside"), &root.join("file"));
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&root).unwrap();

        // Root reads through a sealed directory, so there is nothing to refuse.
        if !matches!(sealed_read, Err(ref error) if error.kind() == std::io::ErrorKind::PermissionDenied) {
            return;
        }
        assert!(verdict.is_err(), "an unreadable entry was answered with a guess: {:?}", verdict);
    }

    /// Every path a remedy names is one shell argument; where no quoting holds for every shell
    /// (Windows), the command takes `-- <path>` and the message names the paths in order.
    #[test]
    fn a_remedy_names_each_path_as_one_argument() {
        let pair = Remedy::PairScoped {
            removes: vec!["My Notes".to_string(), "-x".to_string()],
            load: Some("my notes".to_string()),
        };

        assert_eq!(pair.instructions_when(false),
            "From the working tree's root, run \"forklift remove 'My Notes'\", then \"forklift remove -- -x\", \
            then \"forklift load 'my notes'\".");
        assert_eq!(pair.instructions_when(true),
            "From the working tree's root, run \"forklift remove -- <path>\", then \"forklift remove -- -x\", \
            then \"forklift load -- <path>\". In place of each <path>, put \"My Notes\", then \"my notes\", in \
            that order, quoted for your shell.");
        assert_eq!(Remedy::Load("a b".to_string()).instructions_when(false),
            "To load it, run \"forklift load 'a b'\" from the working tree's root.");
        assert!(Remedy::LoadDirectory("a b".to_string()).instructions_when(false)
            .starts_with("From the working tree's root, run \"forklift load 'a b'\". That stages every change in \"a b\""));
        assert!(Remedy::LoadDirectory(String::new()).instructions_when(true)
            .starts_with("From the working tree's root, run \"forklift load .\". That stages every change in the working tree"));
    }
}
